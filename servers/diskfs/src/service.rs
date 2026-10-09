//! The ring service: diskfs's end of the data plane (docs/design/io-rings.md,
//! the file protocol `fsring`). It attaches the channels clients offer,
//! takes requests from their submission rings and completes them through
//! their completion rings.
//!
//! **Data.** `READ` and `WRITE` run concurrently, up to `MAX_OPS` at once:
//! ext2fs says where the bytes lie (`read_map`) or which blocks a write
//! goes to (`reserve`), and the device moves the data straight between the
//! disk and the client's granted pages by DMA, the requests of all
//! operations in flight together. Nothing copies the data, with these
//! exceptions: a hole in a read is zero-filled by the CPU; a write that
//! does not start or end on a sector boundary within an existing block
//! first reads that sector (or two) into scratch memory, and the device
//! then writes the sector from there and from the grant (a new block is
//! padded with zeros from the zero page instead); the sector bytes a read
//! does not want land in a sink page. A block whose current data is not
//! (only) on the disk (after a failed commit) makes the request go through
//! ext2fs's own read or write with a bounce copy (`EAGAIN`, rare). A write
//! links its new blocks once its data is on the device; it is durable
//! after a `FLUSH` (see `fsring`, "Durability").
//!
//! **Barriers.** Every other operation runs when no operation is in
//! flight: taking one stops taking requests (on
//! every channel) until the operations taken before it completed; then it
//! runs synchronously. Barriers wait in a queue, in the order they were
//! taken. A write waits ("stalls") while another write in flight covers
//! one of its blocks, while scratch memory is short, or while every
//! operation slot is taken; it was taken before every barrier waiting, so
//! it is retried whatever waits, and if it must go through ext2fs's own
//! write it goes in front of them. A `FORGET` of a grant no operation
//! uses (the client sends it once the requests on the grant completed)
//! runs at once, without stopping anything: it touches no file; so does a
//! `PROMISE` (ext2fs's promises, owned by the channel: they end with it).
//! A write in flight spends its promise when it links its blocks.
//!
//! **Holds.** diskfs keeps, per channel, a bit per inode the client holds
//! (`fsring`, "Holds"): an inode whose last link went is freed only when
//! no client holds it, so one client's `RELEASE` never frees an inode
//! another one still uses. A channel's holds go with it. The holds are
//! diskfs's memory: a restarted diskfs knows a client's again only as it
//! names them.
//!
//! **Hostile clients.** Each descriptor is copied out of the ring once and
//! validated (`fsring::Request::decode`); a grant must exist and hold the
//! buffer; names are copied out once, then checked. A client never makes
//! diskfs touch memory outside its grants, and a grant it revokes in the
//! middle of a request fails that request (`EFAULT`: the CPU copies run
//! behind the kernel's copy fixup, `oxrt::copy`; device addresses stay
//! valid until diskfs lets go of them, `FORGET`). Resources are bounded:
//! `MAX_CHANNELS` channels, `MAX_GRANTS` grants each, at most as many
//! requests taken from a channel as its completion ring has room for, and
//! `MAX_OPS` operations in flight.
//!
//! **Waiting.** While there is work, the service polls the rings and the
//! device; after `SPIN_BUDGET` polls without progress it sleeps (nothing
//! in flight: doorbells armed, `ipc_receive`) or yields (device busy; its
//! interrupts stay off, see `blk`), as NAPI does. A channel whose requests
//! wait for room in its completion ring is no work: diskfs sleeps on its
//! doorbell (`Consumer::prepare_blocked_sleep`), which the client rings
//! after taking completions (`fsring`, "Room").

use crate::blk::{Kind, SubmitError, VirtioBlk, SECTOR_SIZE};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;
use ext2fs::{Ext2, NewNode, Reservation};
use fsring::errno::*;
use fsring::{op, Buf, Completion, Kind as NodeKind, Request};
use ring::channel::{Header, Layout, Offer};
use ring::{Consumer, Desc, Producer, Ring, Wait};

type Fs = Ext2<VirtioBlk>;

const N: usize = fsring::SLOTS as usize;
const PAGE: u64 = 4096;
const SECTOR: u64 = SECTOR_SIZE as u64;

/// Channels attached at once (one per Linux server instance).
const MAX_CHANNELS: usize = 16;
/// Grants of a channel diskfs keeps what it learnt of (`FORGET` lets go).
const MAX_GRANTS: usize = 1024;
/// Reads and writes in flight, over all channels.
const MAX_OPS: usize = 32;
/// Requests taken from one channel per round, for fairness.
const TAKE_PER_ROUND: usize = 8;
/// Device addresses cached per channel (direct-mapped).
const DMA_CACHE: usize = 1024;
/// Polls of the rings and the device without progress before the service
/// sleeps or yields (the tuning knob of docs/design/io-rings.md, "Polling
/// and interrupts").
pub const SPIN_BUDGET: u32 = 2_000;
/// Bytes of scratch memory per write that needs sectors read first (a head
/// and a tail sector).
const BOUNCE: u64 = 2 * SECTOR;

/// Zeros for holes.
static ZERO: [u8; PAGE as usize] = [0; PAGE as usize];

/// The completion ring's doorbell: a futex wake on its tail.
struct Futex;

impl Wait for Futex {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let _ = oxrt::futex_wait(word, value, None);
    }

    fn wake(&self, word: &AtomicU32) {
        let _ = oxrt::futex_wake(word, 1);
    }
}

/// A grant as diskfs knows it: mapped here, its first page's device
/// address held (so its id cannot be reused before `FORGET`).
#[derive(Clone, Copy)]
struct Grant {
    addr: *mut u8,
    pages: u64,
    writable: bool,
}

impl Grant {
    /// EINVAL unless `buf` lies within the grant.
    fn check(&self, buf: &Buf) -> Result<(), i64> {
        if buf.end() > self.pages * PAGE {
            return Err(EINVAL);
        }
        Ok(())
    }

    fn copy_in(&self, buf: &Buf) -> Result<Vec<u8>, i64> {
        self.check(buf)?;
        let mut out = vec![0u8; buf.len as usize];
        let src = unsafe { self.addr.add(buf.offset as usize) };
        if !unsafe { oxrt::copy::copy(out.as_mut_ptr(), src, out.len()) } {
            return Err(EFAULT);
        }
        Ok(out)
    }

    fn copy_out(&self, offset: u64, data: &[u8]) -> Result<(), i64> {
        if !self.writable {
            return Err(EACCES);
        }
        if offset + data.len() as u64 > self.pages * PAGE {
            return Err(EINVAL);
        }
        let dst = unsafe { self.addr.add(offset as usize) };
        if !unsafe { oxrt::copy::copy(dst, data.as_ptr(), data.len()) } {
            return Err(EFAULT);
        }
        Ok(())
    }

    fn zero(&self, offset: u64, len: u64) -> Result<(), i64> {
        let mut done = 0;
        while done < len {
            let n = (len - done).min(PAGE);
            self.copy_out(offset + done, &ZERO[..n as usize])?;
            done += n;
        }
        Ok(())
    }
}

/// Device addresses of grant pages: (grant << 32 | page) -> address.
struct DmaCache {
    keys: Vec<u64>,
    addrs: Vec<u64>,
}

impl DmaCache {
    fn new() -> DmaCache {
        DmaCache { keys: vec![u64::MAX; DMA_CACHE], addrs: vec![0; DMA_CACHE] }
    }

    fn slot(key: u64) -> usize {
        (key.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 54) as usize % DMA_CACHE
    }

    fn get(&self, grant: u32, page: u64) -> Option<u64> {
        let key = (grant as u64) << 32 | page;
        let i = Self::slot(key);
        (self.keys[i] == key).then(|| self.addrs[i])
    }

    fn put(&mut self, grant: u32, page: u64, addr: u64) {
        let key = (grant as u64) << 32 | page;
        let i = Self::slot(key);
        self.keys[i] = key;
        self.addrs[i] = addr;
    }

    fn purge(&mut self, grant: u32) {
        for k in self.keys.iter_mut().filter(|k| **k != u64::MAX && (**k >> 32) as u32 == grant) {
            *k = u64::MAX;
        }
    }
}

struct Chan {
    id: u64,
    header: &'static Header,
    requests: Consumer<'static, N>,
    completions: Producer<'static, N>,
    grants: BTreeMap<u32, Grant>,
    dma: DmaCache,
    /// Requests taken and not yet completed into the ring.
    taken: usize,
    /// Completions the ring had no room for (the client moved its head
    /// wrongly): retried.
    unposted: VecDeque<Desc>,
    /// A write taken that waits for another one or for scratch memory.
    stalled: Option<Desc>,
    /// Completions pushed since the last doorbell.
    rang: bool,
    gone: bool,
    /// The inodes the client holds (see `Holds`).
    held: Bitmap,
}

impl Chan {
    /// Whether the client has requests (or completions to take) that wait
    /// only for it to make room in its completion ring: it rings the
    /// submission doorbell once it did (`fsring`, "Room").
    fn blocked(&mut self) -> bool {
        let room = self.completions.room();
        (!self.unposted.is_empty() && room == 0) || (self.taken + self.unposted.len() >= room && !self.requests.is_empty())
    }

    /// Whether a request can be taken now.
    fn ready(&mut self) -> bool {
        !self.gone && self.stalled.is_none() && self.taken + self.unposted.len() < self.completions.room() && !self.requests.is_empty()
    }
}

/// One bit per inode.
struct Bitmap(Vec<u64>);

impl Bitmap {
    fn new(inodes: u64) -> Bitmap {
        Bitmap(vec![0; (inodes / 64 + 1) as usize])
    }

    fn set(&mut self, ino: u32) {
        if let Some(w) = self.0.get_mut(ino as usize / 64) {
            *w |= 1 << (ino % 64);
        }
    }

    fn clear(&mut self, ino: u32) {
        if let Some(w) = self.0.get_mut(ino as usize / 64) {
            *w &= !(1 << (ino % 64));
        }
    }

    fn has(&self, ino: u32) -> bool {
        self.0.get(ino as usize / 64).is_some_and(|w| w & 1 << (ino % 64) != 0)
    }

    fn inodes(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().enumerate().flat_map(|(i, &w)| (0..64).filter(move |b| w & 1 << b != 0).map(move |b| (i * 64 + b) as u32))
    }
}

/// One device request of an operation: buffers (device address, length).
struct DevReq {
    kind: Kind,
    lba: u64,
    bufs: Vec<(u64, u32)>,
}

/// Where the bytes of a stretch of a device request come from or go to.
#[derive(Clone, Copy)]
enum Src {
    /// Read bytes nobody wants.
    Sink,
    /// Zeros a write pads a new block with.
    Zeros,
    /// Scratch memory at this device address.
    Scratch(u64),
    /// The operation's grant, from this offset.
    Grant(u64),
}

#[derive(Clone, Copy)]
struct Part {
    src: Src,
    len: u64,
}

enum Work {
    Read {
        bytes: u64,
        size: u64,
    },
    Write {
        ino: u32,
        blocks: (u64, u64),
        reservation: Reservation,
        end: u64,
        len: u64,
        /// The scratch slot of the sectors read first.
        bounce: Option<usize>,
    },
}

struct Op {
    chan: usize,
    tag: u64,
    op: u16,
    /// The grant it moves data from or to.
    grant: u32,
    /// The device requests of the current stage, how many were submitted,
    /// how many are still in flight; a write's data requests wait in
    /// `later` while the sectors it keeps are read.
    reqs: Vec<DevReq>,
    next: usize,
    outstanding: u32,
    later: Vec<DevReq>,
    error: Option<i64>,
    work: Work,
}

/// What starting a request came to.
enum Start {
    Running(Op),
    Done(i64, [u64; 4]),
    /// It must wait (a write on blocks in flight, no scratch memory).
    Stall,
    /// Through ext2fs's own read or write, as a barrier.
    Fallback,
}

pub struct Service {
    chans: Vec<Option<Chan>>,
    ops: Vec<Option<Op>>,
    /// Barriers taken (channel, request), each waiting for what was taken
    /// before it to complete, in order. While one waits, nothing new is
    /// taken; a stalled write taken before it that turns out to need the
    /// fallback goes in front (it was taken earlier than every one here).
    barriers: VecDeque<(usize, Desc)>,
    inodes: u64,
    /// The channel the next round starts with.
    rr: usize,
    /// Free scratch slots, and the scratch memory's device address.
    bounce: Vec<usize>,
    scratch: u64,
    sink: u64,
    zeros: u64,
    /// Bytes per device request (see `plan`), and per data buffer.
    piece: u64,
    unit: u64,
    max_segment: u64,
}

impl Service {
    /// The service for `disk` with a filesystem of `inodes` inodes and
    /// blocks of `block_size`; an error for a device or an image the ring
    /// path cannot serve.
    pub fn new(disk: &VirtioBlk, block_size: usize, inodes: u64) -> Result<Service, &'static str> {
        let (_, scratch) = disk.scratch();
        let slots = (crate::blk::SCRATCH_PAGES as u64 * PAGE / BOUNCE) as usize;
        if block_size as u64 > PAGE {
            return Err("blocks larger than a page");
        }
        // A data buffer is at most `unit` bytes: a power of two (so that it
        // divides a page) of whole sectors within the device's limit.
        let max_segment = disk.max_segment() as u64;
        let mut unit = PAGE;
        while unit > max_segment {
            unit /= 2;
        }
        if unit < SECTOR {
            return Err("the device takes less than a sector per segment");
        }
        // A piece of P bytes of a grant is at most P / unit + 1 buffers,
        // plus a pad before and after it (each under a block, under a page:
        // PAGE / unit buffers at most).
        let pads = 2 * (PAGE / unit);
        let units = (disk.max_segments() as u64).checked_sub(1 + pads).filter(|&n| n > 0).ok_or("the device takes too few segments per request")?;
        let piece = (units * unit).min(128 * 1024) / unit * unit;
        Ok(Service {
            chans: (0..MAX_CHANNELS).map(|_| None).collect(),
            ops: (0..MAX_OPS).map(|_| None).collect(),
            barriers: VecDeque::new(),
            inodes,
            rr: 0,
            bounce: (0..slots).rev().collect(),
            scratch,
            sink: disk.sink(),
            zeros: disk.zeros(),
            piece,
            unit,
            max_segment,
        })
    }

    // ------------------------------------------------------------ channels

    /// An offer from the kernel (a control request's payload): the status
    /// to answer with. The channel is attached unless it is refused here.
    pub fn offer(&mut self, message: &[u8]) -> i64 {
        let Some(offer) = Offer::decode(message) else { return -EINVAL };
        if offer.slots != fsring::SLOTS {
            return -EINVAL;
        }
        let Some(slot) = self.chans.iter().position(Option::is_none) else { return -ENOSPC };
        let base = match oxrt::chan_attach(offer.channel) {
            Ok(base) => base,
            Err(e) => return e,
        };
        let layout = Layout::new(fsring::SLOTS).expect("a valid slot count");
        // Mapped until chan_detach, which comes after the Chan is dropped.
        let (sub, comp) = unsafe { (layout.ring::<N>(base, layout.submission), layout.ring::<N>(base, layout.completion)) };
        self.chans[slot] = Some(Chan {
            id: offer.channel,
            header: unsafe { Header::at(base) },
            requests: Ring::new(sub).consumer(),
            completions: Ring::new(comp).producer(),
            grants: BTreeMap::new(),
            dma: DmaCache::new(),
            taken: 0,
            unposted: VecDeque::new(),
            stalled: None,
            rang: false,
            gone: false,
            held: Bitmap::new(self.inodes),
        });
        0
    }

    fn chan(&mut self, c: usize) -> &mut Chan {
        self.chans[c].as_mut().expect("a channel in use")
    }

    /// The grant `id` of channel `c`, learnt now if it is new.
    fn grant(&mut self, c: usize, id: u32) -> Result<Grant, i64> {
        let chan = self.chan(c);
        if let Some(g) = chan.grants.get(&id) {
            return Ok(*g);
        }
        if chan.grants.len() >= MAX_GRANTS {
            return Err(ENOSPC);
        }
        let (addr, pages, writable) = oxrt::grant_map(chan.id, id).map_err(|e| if e == -ENOENT { EBADF } else { -e })?;
        let g = Grant { addr, pages, writable };
        // Holding a device address keeps the id this grant's until FORGET.
        let mut first = [0u64; 1];
        if oxrt::grant_dma_pages(chan.id, id, 0, &mut first).is_err() {
            let _ = oxrt::munmap(addr, (pages * PAGE) as usize);
            return Err(EBADF);
        }
        chan.dma.put(id, 0, first[0]);
        chan.grants.insert(id, g);
        Ok(g)
    }

    /// The device address of page `page` of grant `id` (fetching up to the
    /// pages up to `last` at once when it is not cached).
    fn dma(&mut self, c: usize, id: u32, page: u64, last: u64) -> Result<u64, i64> {
        let chan = self.chan(c);
        if let Some(a) = chan.dma.get(id, page) {
            return Ok(a);
        }
        let mut addrs = [0u64; 64];
        let n = (last.saturating_sub(page) + 1).min(64) as usize;
        oxrt::grant_dma_pages(chan.id, id, page, &mut addrs[..n]).map_err(|e| if e == -ENOENT { EBADF } else { -e })?;
        for (i, &a) in addrs[..n].iter().enumerate() {
            chan.dma.put(id, page + i as u64, a);
        }
        Ok(addrs[0])
    }

    fn forget(&mut self, c: usize, id: u32) {
        let chan = self.chan(c);
        chan.dma.purge(id);
        if let Some(g) = chan.grants.remove(&id) {
            let _ = oxrt::munmap(g.addr, (g.pages * PAGE) as usize);
            let _ = oxrt::grant_dma_unmap(chan.id, id);
        }
    }

    /// Completes a request of channel `c` (dropped if its client is gone).
    fn complete(&mut self, c: usize, tag: u64, op: u16, status: i64, values: [u64; 4]) {
        let chan = self.chan(c);
        chan.taken -= 1;
        if chan.gone {
            return;
        }
        let d = Completion { tag, op, status, values }.to_desc();
        if chan.unposted.is_empty() && chan.completions.push(&d) {
            chan.rang = true;
        } else {
            chan.unposted.push_back(d);
        }
    }

    // ----------------------------------------------------------- the loop

    /// Whether anything is to be done without waiting for an event.
    /// A channel that waits for its client to make room in its completion
    /// ring is not: the client rings when it did (no spinning on behalf of
    /// a client that never takes its completions).
    pub fn busy(&mut self) -> bool {
        if self.ops.iter().any(Option::is_some) || !self.barriers.is_empty() {
            return true;
        }
        self.chans.iter_mut().flatten().any(|c| {
            c.stalled.is_some() || c.gone || c.header.state() != 0 || (!c.unposted.is_empty() && c.completions.room() > 0) || c.ready()
        })
    }

    /// Announces the sleep in every ring and arms its doorbell: false if
    /// work came meanwhile (then nothing sleeps). A ring with requests the
    /// service cannot take yet (no room for their completions) sleeps too,
    /// until the client's doorbell.
    pub fn prepare_sleep(&mut self) -> bool {
        for chan in self.chans.iter_mut().flatten() {
            let tail = if chan.blocked() {
                chan.requests.prepare_blocked_sleep()
            } else {
                match chan.requests.prepare_sleep() {
                    Some(tail) => tail,
                    None => return false,
                }
            };
            match oxrt::chan_watch(chan.id, tail) {
                Ok(true) => {}
                // Moved, or the client is gone: there is work.
                Ok(false) | Err(_) => return false,
            }
        }
        true
    }

    /// Ends a sleep: clients stop ringing while the service polls.
    pub fn awake(&mut self) {
        for chan in self.chans.iter_mut().flatten() {
            chan.requests.awake();
        }
    }

    /// Polls until nothing happened for `SPIN_BUDGET` rounds: then returns
    /// for the caller to look at IPC and sleep; if the device is busy, it
    /// yields first.
    pub fn run(&mut self, fs: &mut Fs) {
        let mut idle = 0;
        let mut worked = false;
        loop {
            if self.poll(fs) {
                idle = 0;
                worked = true;
                continue;
            }
            // Nothing for the rings (an IPC request woke the loop): no
            // spinning on behalf of a client that sent nothing.
            if !worked && fs.device().in_flight() == 0 {
                return;
            }
            idle += 1;
            if idle < SPIN_BUDGET {
                core::hint::spin_loop();
                continue;
            }
            if fs.device().in_flight() > 0 {
                oxrt::sched_yield();
            }
            return;
        }
    }

    /// One round: completions from the device, requests to it, requests
    /// from the rings, completions into them. Whether anything happened.
    fn poll(&mut self, fs: &mut Fs) -> bool {
        let mut progress = self.reap(fs);
        progress |= self.submit(fs);
        progress |= self.take(fs);
        progress |= self.submit(fs);
        progress |= self.post();
        progress
    }

    fn reap(&mut self, fs: &mut Fs) -> bool {
        let mut any = false;
        while let Some((token, ok)) = fs.device_mut().reap() {
            any = true;
            let Some(op) = self.ops.get_mut(token as usize).and_then(Option::as_mut) else { continue };
            op.outstanding -= 1;
            if !ok {
                op.error.get_or_insert(EIO);
            }
            self.advance(fs, token as usize);
        }
        any
    }

    /// Moves operation `i` on once its requests in flight are done: to its
    /// next stage, or to its completion.
    fn advance(&mut self, fs: &mut Fs, i: usize) {
        let op = self.ops[i].as_mut().expect("an operation in flight");
        if op.outstanding > 0 || (op.error.is_none() && op.next < op.reqs.len()) {
            return;
        }
        if op.error.is_none() && !op.later.is_empty() {
            op.reqs = core::mem::take(&mut op.later);
            op.next = 0;
            return;
        }
        let op = self.ops[i].take().expect("checked above");
        let (status, values) = match op.work {
            Work::Read { bytes, size } => match op.error {
                Some(e) => (-e, [0; 4]),
                None => (bytes as i64, [size, 0, 0, 0]),
            },
            Work::Write { reservation, end, len, bounce, .. } => {
                if let Some(slot) = bounce {
                    self.bounce.push(slot);
                }
                match op.error {
                    Some(e) => {
                        fs.unreserve(&reservation);
                        (-e, [0; 4])
                    }
                    None => match fs.link(&reservation, end) {
                        Ok(size) => (len as i64, [size, 0, 0, 0]),
                        Err(e) => (-e, [0; 4]),
                    },
                }
            }
        };
        self.complete(op.chan, op.tag, op.op, status, values);
    }

    fn submit(&mut self, fs: &mut Fs) -> bool {
        let mut submitted = false;
        let disk = fs.device_mut();
        'ops: for (i, slot) in self.ops.iter_mut().enumerate() {
            let Some(op) = slot else { continue };
            while op.error.is_none() && op.next < op.reqs.len() {
                let r = &op.reqs[op.next];
                match disk.submit(r.kind, r.lba, &r.bufs, i as u64) {
                    Ok(()) => {
                        op.next += 1;
                        op.outstanding += 1;
                        submitted = true;
                    }
                    Err(SubmitError::Busy) => break 'ops,
                    Err(SubmitError::Invalid) => op.error = Some(EIO),
                }
            }
        }
        if submitted {
            disk.kick();
        }
        // Operations that failed before anything was in flight end now.
        for i in 0..self.ops.len() {
            if self.ops[i].as_ref().is_some_and(|op| op.error.is_some() && op.outstanding == 0) {
                self.advance(fs, i);
            }
        }
        submitted
    }

    /// Takes requests from the rings (and runs a barrier once the
    /// operations before it are done).
    fn take(&mut self, fs: &mut Fs) -> bool {
        let mut any = false;
        let count = self.chans.len();
        for k in 0..count {
            let c = (self.rr + k) % count;
            let Some(chan) = self.chans[c].as_mut() else { continue };
            if !chan.gone && chan.header.state() != 0 {
                // The client is gone: its requests are dropped, its
                // operations in flight finish (into pinned pages).
                chan.gone = true;
                chan.stalled = None;
                self.barriers.retain(|&(bc, _)| bc != c);
                any = true;
            }
            let chan = self.chans[c].as_mut().expect("checked");
            if chan.gone {
                if !self.ops.iter().flatten().any(|op| op.chan == c) {
                    self.close(fs, c);
                    any = true;
                }
                continue;
            }
            // A stalled write was taken before anything now waiting: it is
            // retried whatever waits, but only with a slot for it.
            if chan.stalled.is_some() && self.ops.iter().any(Option::is_none) {
                let d = self.chan(c).stalled.take().expect("checked");
                any |= self.dispatch(fs, c, d, true);
            }
            for _ in 0..TAKE_PER_ROUND {
                if !self.barriers.is_empty() || !self.ops.iter().any(Option::is_none) || !self.chan(c).ready() {
                    break;
                }
                let chan = self.chan(c);
                let Some(d) = chan.requests.pop() else { break };
                chan.taken += 1;
                any = true;
                self.dispatch(fs, c, d, false);
            }
        }
        self.rr = (self.rr + 1) % count;
        // Barriers in order, each once everything taken before it is done.
        while self.ops.iter().all(Option::is_none) && self.chans.iter().flatten().all(|c| c.stalled.is_none()) {
            let Some((c, d)) = self.barriers.pop_front() else { break };
            self.run_barrier(fs, c, &d);
            any = true;
        }
        any
    }

    /// Starts request `d` of channel `c` (taken before if `retry`).
    fn dispatch(&mut self, fs: &mut Fs, c: usize, d: Desc, retry: bool) -> bool {
        let request = match Request::decode(&d) {
            Ok(r) => r,
            Err(e) => {
                self.complete(c, d.tag, d.op, -e, [0; 4]);
                return true;
            }
        };
        self.hold_named(c, &request);
        // A grant no operation uses is let go at once: no barrier (the
        // client sends FORGET after the requests on it completed).
        if let Request::Forget { grant } = request {
            let busy = self.ops.iter().flatten().any(|op| op.chan == c && op.grant == grant)
                || self.chan(c).stalled.is_some_and(|s| s.grant == grant)
                || self.barriers.iter().any(|&(bc, ref b)| bc == c && b.grant == grant);
            if !busy {
                self.forget(c, grant);
                self.complete(c, d.tag, d.op, 0, [0; 4]);
                return true;
            }
        }
        if let Request::Promise { .. } = request {
            // Touches no file: at once.
            let status = self.execute(fs, c, request).map_or_else(|e| -e, |(s, _)| s);
            self.complete(c, d.tag, d.op, status, [0; 4]);
            return true;
        }
        if !request.is_data() {
            // Taken fresh: nothing else waits (taking stops while one does).
            self.barriers.push_back((c, d));
            return true;
        }
        // Started only with a slot to run in (else it waits its turn).
        let Some(slot) = self.ops.iter().position(Option::is_none) else {
            self.chan(c).stalled = Some(d);
            return !retry;
        };
        let started = match request {
            Request::Read { ino, offset, buf } => self.start_read(fs, c, ino, offset, buf),
            Request::Write { ino, offset, buf } => self.start_write(fs, c, ino, offset, buf),
            _ => unreachable!("data requests only"),
        };
        match started {
            Ok(Start::Running(mut op)) => {
                op.tag = d.tag;
                op.op = d.op;
                self.ops[slot] = Some(op);
                self.advance(fs, slot);
            }
            Ok(Start::Done(status, values)) => self.complete(c, d.tag, d.op, status, values),
            Ok(Start::Stall) => {
                self.chan(c).stalled = Some(d);
                return !retry;
            }
            // Taken before every barrier waiting now (see `barriers`).
            Ok(Start::Fallback) if retry => self.barriers.push_front((c, d)),
            Ok(Start::Fallback) => self.barriers.push_back((c, d)),
            Err(e) => self.complete(c, d.tag, d.op, -e, [0; 4]),
        }
        true
    }

    // ------------------------------------------------------------- holds

    /// Records that channel `c` holds the inodes `request` names (see
    /// `Holds` in the module comment).
    fn hold_named(&mut self, c: usize, request: &Request) {
        let chan = self.chan(c);
        match *request {
            Request::Read { ino, .. } | Request::Write { ino, .. } | Request::Stat { ino } | Request::Truncate { ino, .. } => chan.held.set(ino),
            Request::Readlink { ino, .. } | Request::SetPerm { ino, .. } | Request::SetTimes { ino, .. } | Request::Promise { ino, .. } => chan.held.set(ino),
            Request::Lookup { dir, .. } | Request::Create { dir, .. } | Request::Unlink { dir, .. } | Request::Readdir { dir, .. } => chan.held.set(dir),
            Request::Rename { from, to, .. } => {
                chan.held.set(from);
                chan.held.set(to);
            }
            Request::Release { .. } | Request::Flush | Request::Statfs | Request::Forget { .. } => {}
        }
    }

    /// Frees `ino` if its last link is gone and no client holds it.
    fn try_free(&mut self, fs: &mut Fs, ino: u32) {
        if self.chans.iter().flatten().any(|c| c.held.has(ino)) || fs.check(ino).is_err() {
            return;
        }
        if fs.stat(ino).is_ok_and(|s| s.links == 0) {
            let _ = fs.release(ino);
        }
    }

    /// Channel `c` ends: diskfs lets go of it and its client's inodes.
    fn close(&mut self, fs: &mut Fs, c: usize) {
        let chan = self.chans[c].take().expect("a channel in use");
        // Vouches that no device uses its grants any more.
        let _ = oxrt::chan_detach(chan.id);
        fs.forget_promises(chan.id);
        for ino in chan.held.inodes() {
            self.try_free(fs, ino);
        }
    }

    fn post(&mut self) -> bool {
        let mut any = false;
        for chan in self.chans.iter_mut().flatten() {
            while let Some(d) = chan.unposted.front() {
                if !chan.completions.push(d) {
                    break;
                }
                chan.unposted.pop_front();
                chan.rang = true;
            }
            if chan.rang {
                chan.rang = false;
                chan.completions.ring_doorbell(&Futex);
                any = true;
            }
        }
        any
    }

    // ---------------------------------------------------- reads and writes

    fn op(&self, c: usize, grant: u32, work: Work, reqs: Vec<DevReq>, later: Vec<DevReq>) -> Start {
        Start::Running(Op { chan: c, tag: 0, op: 0, grant, reqs, next: 0, outstanding: 0, later, error: None, work })
    }

    fn start_read(&mut self, fs: &mut Fs, c: usize, ino: u32, offset: u64, buf: Buf) -> Result<Start, i64> {
        let g = self.grant(c, buf.grant)?;
        g.check(&buf)?;
        if !g.writable {
            return Err(EACCES);
        }
        let (size, extents) = match fs.read_map(ino, offset, buf.len as u64) {
            Err(ext2fs::errno::EAGAIN) => return Ok(Start::Fallback),
            r => r.map_err(|e| e)?,
        };
        let mut reqs = Vec::new();
        let mut at = buf.offset as u64;
        let mut bytes = 0;
        for e in extents {
            match e.disk {
                None => g.zero(at, e.len)?,
                Some(d) => {
                    let start = d / SECTOR * SECTOR;
                    let end = (d + e.len).div_ceil(SECTOR) * SECTOR;
                    let parts = [
                        Part { src: Src::Sink, len: d - start },
                        Part { src: Src::Grant(at), len: e.len },
                        Part { src: Src::Sink, len: end - d - e.len },
                    ];
                    self.plan(c, buf.grant, Kind::Read, start, &parts, &mut reqs)?;
                }
            }
            at += e.len;
            bytes += e.len;
        }
        Ok(self.op(c, buf.grant, Work::Read { bytes, size }, reqs, Vec::new()))
    }

    fn start_write(&mut self, fs: &mut Fs, c: usize, ino: u32, offset: u64, buf: Buf) -> Result<Start, i64> {
        let g = self.grant(c, buf.grant)?;
        g.check(&buf)?;
        let len = buf.len as u64;
        if len == 0 {
            fs.check(ino)?;
            return Ok(Start::Done(0, [fs.stat(ino)?.size, 0, 0, 0]));
        }
        let bs = fs.block_size() as u64;
        let end = offset + len;
        let blocks = (offset / bs, (end - 1) / bs);
        // Writes to the same blocks one after the other.
        let overlaps = self.ops.iter().flatten().any(|op| match &op.work {
            Work::Write { ino: other, blocks: (a, b), .. } => *other == ino && *a <= blocks.1 && blocks.0 <= *b,
            Work::Read { .. } => false,
        });
        let unaligned = offset % SECTOR != 0 || end % SECTOR != 0;
        if overlaps || (unaligned && self.bounce.is_empty()) {
            return Ok(Start::Stall);
        }
        let reservation = match fs.reserve(ino, offset, len) {
            Err(ext2fs::errno::EAGAIN) => return Ok(Start::Fallback),
            r => r?,
        };
        let bounce = if unaligned { self.bounce.pop() } else { None };
        let scratch = bounce.map(|slot| self.scratch + slot as u64 * BOUNCE);
        // The first and the last sector are one: one scratch sector serves
        // both pads (blocks are whole sectors, so a file's sector is a
        // disk sector).
        let one = offset / SECTOR == (end - 1) / SECTOR;
        let (mut reads, mut writes) = (Vec::new(), Vec::new());
        let planned = (|| {
            for run in &reservation.runs {
                let file = run.file_block * bs;
                let disk = run.block as u64 * bs;
                let span = run.count as u64 * bs;
                let (from, to) = (offset.max(file), end.min(file + span));
                let (d0, d1) = (disk + from - file, disk + to - file);
                let data = Part { src: Src::Grant(buf.offset as u64 + from - offset), len: to - from };
                let parts = if run.new {
                    // A new block is written whole: zeros around the data.
                    [Part { src: Src::Zeros, len: d0 - disk }, data, Part { src: Src::Zeros, len: disk + span - d1 }]
                } else {
                    // Whole sectors: the bytes around the data as they are
                    // on the disk, read first into scratch memory.
                    // Only the operation's first bytes can start inside a
                    // sector and its last end inside one.
                    let (s0, s1) = (d0 / SECTOR * SECTOR, d1.div_ceil(SECTOR) * SECTOR);
                    let head = scratch.unwrap_or(0);
                    let tail = if one { head } else { head + SECTOR };
                    if d0 > s0 {
                        reads.push(DevReq { kind: Kind::Read, lba: s0 / SECTOR, bufs: vec![(head, SECTOR as u32)] });
                    }
                    if s1 > d1 && !(one && d0 > s0) {
                        reads.push(DevReq { kind: Kind::Read, lba: s1 / SECTOR - 1, bufs: vec![(tail, SECTOR as u32)] });
                    }
                    [Part { src: Src::Scratch(head), len: d0 - s0 }, data, Part { src: Src::Scratch(tail + SECTOR - (s1 - d1)), len: s1 - d1 }]
                };
                let start = if run.new { disk } else { d0 / SECTOR * SECTOR };
                self.plan(c, buf.grant, Kind::Write, start, &parts, &mut writes)?;
            }
            Ok::<(), i64>(())
        })();
        if let Err(e) = planned {
            fs.unreserve(&reservation);
            if let Some(slot) = bounce {
                self.bounce.push(slot);
            }
            return Err(e);
        }
        // Scratch memory only if a sector had to be read.
        let bounce = match (bounce, reads.is_empty()) {
            (Some(slot), true) => {
                self.bounce.push(slot);
                None
            }
            (b, _) => b,
        };
        let work = Work::Write { ino, blocks, reservation, end, len, bounce };
        Ok(if reads.is_empty() { self.op(c, buf.grant, work, writes, Vec::new()) } else { self.op(c, buf.grant, work, reads, writes) })
    }

    /// Cuts the device range from byte `start` (sector-aligned), made of
    /// `parts`, into requests of at most `piece` bytes at sector boundaries,
    /// with the grant's pages at their device addresses.
    fn plan(&mut self, c: usize, grant: u32, kind: Kind, start: u64, parts: &[Part], out: &mut Vec<DevReq>) -> Result<(), i64> {
        let total: u64 = parts.iter().map(|p| p.len).sum();
        let last_page = parts
            .iter()
            .filter_map(|p| match p.src {
                Src::Grant(at) if p.len > 0 => Some((at + p.len - 1) / PAGE),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let mut pos = 0u64;
        let mut cur = DevReq { kind, lba: start / SECTOR, bufs: Vec::new() };
        for part in parts {
            let mut done = 0;
            while done < part.len {
                let room = self.piece - pos % self.piece;
                let mut take = (part.len - done).min(room);
                let addr = match part.src {
                    Src::Sink => {
                        take = take.min(self.unit);
                        self.sink
                    }
                    Src::Zeros => {
                        take = take.min(self.unit);
                        self.zeros
                    }
                    Src::Scratch(a) => {
                        take = take.min(self.unit);
                        a + done
                    }
                    Src::Grant(at) => {
                        let o = at + done;
                        // Within a page (and a unit, which divides it).
                        take = take.min(self.unit - o % self.unit);
                        self.dma(c, grant, o / PAGE, last_page)? + o % PAGE
                    }
                };
                match cur.bufs.last_mut() {
                    Some((a, l)) if *a + *l as u64 == addr && *l as u64 + take <= self.max_segment => *l += take as u32,
                    _ => cur.bufs.push((addr, take as u32)),
                }
                done += take;
                pos += take;
                if pos % self.piece == 0 || pos == total {
                    let next = DevReq { kind, lba: (start + pos) / SECTOR, bufs: Vec::new() };
                    out.push(core::mem::replace(&mut cur, next));
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ barriers

    fn run_barrier(&mut self, fs: &mut Fs, c: usize, d: &Desc) {
        let (status, values) = match Request::decode(d).and_then(|r| self.execute(fs, c, r)) {
            Ok((status, values)) => (status, values),
            Err(e) => (-e, [0; 4]),
        };
        // The inode a lookup or create returns, and the one whose last link
        // an unlink or rename took (the client releases it), are held.
        if status == 0 && matches!(d.op, op::LOOKUP | op::CREATE | op::UNLINK | op::RENAME) && values[0] != 0 {
            self.chan(c).held.set(values[0] as u32);
        }
        self.complete(c, d.tag, d.op, status, values);
    }

    /// A name from a grant: copied out once, then checked.
    fn name(&mut self, c: usize, buf: &Buf) -> Result<String, i64> {
        let g = self.grant(c, buf.grant)?;
        let bytes = g.copy_in(buf)?;
        Ok(String::from(fsring::check_name(&bytes)?))
    }

    /// A directory that can take new entries (not removed).
    fn live_dir(fs: &mut Fs, dir: u32) -> Result<(), i64> {
        fs.check(dir)?;
        if fs.stat(dir)?.links == 0 {
            return Err(ENOENT);
        }
        Ok(())
    }

    fn execute(&mut self, fs: &mut Fs, c: usize, request: Request) -> Result<(i64, [u64; 4]), i64> {
        let none = [0u64; 4];
        match request {
            Request::Flush => fs.sync().map(|_| (0, none)),
            Request::Stat { ino } => {
                fs.check(ino)?;
                let s = fs.stat(ino)?;
                let stat = fsring::Stat { mode: s.mode, links: s.links, size: s.size, atime: s.atime, mtime: s.mtime, ctime: s.ctime, generation: s.generation };
                Ok((0, stat.to_values()))
            }
            Request::Statfs => {
                let (bs, blocks, free, inodes, free_inodes) = fs.usage();
                let u = fsring::Usage {
                    block_size: bs as u32,
                    blocks: blocks as u32,
                    free_blocks: free as u32,
                    inodes: inodes as u32,
                    free_inodes: free_inodes as u32,
                    max_file_size: fs.max_file_size(),
                };
                Ok((0, u.to_values()))
            }
            Request::Lookup { dir, name } => {
                fs.check(dir)?;
                let name = self.name(c, &name)?;
                let ino = fs.lookup(dir, &name)?;
                let s = fs.stat(ino)?;
                Ok((0, [ino as u64, s.mode as u64, s.generation as u64, 0]))
            }
            Request::Create { dir, name, kind, perm } => {
                Self::live_dir(fs, dir)?;
                let name = self.name(c, &name)?;
                let node = match kind {
                    NodeKind::File => NewNode::File,
                    NodeKind::Dir => NewNode::Dir,
                    NodeKind::Symlink(target) => {
                        let g = self.grant(c, target.grant)?;
                        let bytes = g.copy_in(&target)?;
                        NewNode::Symlink(String::from(fsring::check_target(&bytes)?))
                    }
                };
                let ino = fs.create(dir, &name, &node, perm)?;
                let s = fs.stat(ino)?;
                Ok((0, [ino as u64, s.mode as u64, s.generation as u64, 0]))
            }
            Request::Unlink { dir, name, is_dir } => {
                fs.check(dir)?;
                let name = self.name(c, &name)?;
                let gone = fs.unlink(dir, &name, is_dir)?;
                Ok((0, [gone.first().copied().unwrap_or(0) as u64, 0, 0, 0]))
            }
            Request::Rename { from, name, to, new_name } => {
                fs.check(from)?;
                Self::live_dir(fs, to)?;
                let (old, new) = (self.name(c, &name)?, self.name(c, &new_name)?);
                let gone = fs.rename(from, &old, to, &new)?;
                Ok((0, [gone.first().copied().unwrap_or(0) as u64, 0, 0, 0]))
            }
            Request::Truncate { ino, len } => {
                fs.check(ino)?;
                fs.truncate(ino, len).map(|_| (0, none))
            }
            Request::Release { ino } => {
                // The client lets go of it: freed if its last link is gone
                // and no other client holds it.
                fs.check(ino)?;
                self.chan(c).held.clear(ino);
                self.try_free(fs, ino);
                Ok((0, none))
            }
            Request::SetPerm { ino, perm } => {
                fs.check(ino)?;
                fs.set_perm(ino, perm).map(|_| (0, none))
            }
            Request::SetTimes { ino, atime, mtime, ctime } => {
                fs.check(ino)?;
                fs.set_times(ino, atime, mtime, ctime).map(|_| (0, none))
            }
            Request::Readlink { ino, buf } => {
                fs.check(ino)?;
                let g = self.grant(c, buf.grant)?;
                g.check(&buf)?;
                let target = fs.readlink(ino)?;
                if target.len() > buf.len as usize {
                    return Err(ERANGE);
                }
                g.copy_out(buf.offset as u64, target.as_bytes())?;
                Ok((target.len() as i64, none))
            }
            Request::Readdir { dir, cursor, buf } => {
                fs.check(dir)?;
                let g = self.grant(c, buf.grant)?;
                g.check(&buf)?;
                if !g.writable {
                    return Err(EACCES);
                }
                let entries = fs.list(dir)?;
                let mut out = vec![0u8; buf.len as usize];
                let (mut used, mut next) = (0, cursor as usize);
                for (name, ino, kind) in entries.iter().skip(next) {
                    match fsring::put_dirent(&mut out[used..], *ino, *kind, name.as_bytes()) {
                        Some(n) => used += n,
                        None => break,
                    }
                    next += 1;
                }
                if used == 0 && next < entries.len() {
                    // Not even one entry fits.
                    return Err(EINVAL);
                }
                g.copy_out(buf.offset as u64, &out[..used])?;
                let next = if next >= entries.len() { 0 } else { next as u64 };
                Ok((used as i64, [next, 0, 0, 0]))
            }
            Request::Forget { grant } => {
                self.forget(c, grant);
                Ok((0, none))
            }
            // (Run at once by `dispatch`; never a barrier.)
            Request::Promise { ino, offset, len } => {
                let owner = self.chan(c).id;
                fs.check(ino)?;
                fs.promise(owner, ino, offset, len).map(|_| (0, none))
            }
            // Through ext2fs's own paths, with a bounce copy (see `Start::Fallback`).
            Request::Read { ino, offset, buf } => {
                fs.check(ino)?;
                let g = self.grant(c, buf.grant)?;
                g.check(&buf)?;
                let mut chunk = vec![0u8; 32 * 1024];
                let mut done = 0u64;
                while done < buf.len as u64 {
                    let n = (buf.len as u64 - done).min(chunk.len() as u64) as usize;
                    let got = fs.read(ino, offset + done, &mut chunk[..n])?;
                    g.copy_out(buf.offset as u64 + done, &chunk[..got])?;
                    done += got as u64;
                    if got < n {
                        break;
                    }
                }
                Ok((done as i64, [fs.stat(ino)?.size, 0, 0, 0]))
            }
            Request::Write { ino, offset, buf } => {
                fs.check(ino)?;
                let g = self.grant(c, buf.grant)?;
                g.check(&buf)?;
                let mut done = 0u64;
                while done < buf.len as u64 {
                    let n = (buf.len as u64 - done).min(32 * 1024) as u32;
                    let part = Buf { grant: buf.grant, offset: buf.offset + done as u32, len: n };
                    let data = g.copy_in(&part)?;
                    done += fs.write(ino, offset + done, &data)? as u64;
                }
                Ok((done as i64, [fs.stat(ino)?.size, 0, 0, 0]))
            }
        }
    }
}
