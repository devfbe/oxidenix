//! procfs: the system-wide part of Linux's /proc, and /sys, served from
//! user space. The kernel only offers its native system information
//! (`proc_query`, `procproto`); this server renders it in the formats Linux
//! programs parse (`tree`). It is the Linux personality of the system: a
//! non-Linux userland would not need it.
//!
//! Its clients are the Linux server instances, each over a channel of the
//! I/O rings speaking the file protocol (`fsring`, docs/design/io-rings.md
//! step 5): read-only and without holds, so `LOOKUP`, `STAT`, `READ`,
//! `READDIR`, `STATFS`, `FORGET` (and `RELEASE`, `FLUSH`, which have
//! nothing to do); everything that would change a file is EROFS. Names come
//! from, and results go to, the client's grants, copied with the copy that
//! survives a revoke (`oxrt::copy`): a client that revokes a grant under a
//! request gets EFAULT for it, procfs goes on. Each process's own files
//! (/proc/<pid>, `self`) are the Linux server's, which knows its processes.
//!
//! One thread, one event loop, as diskfs's: `ipc_receive` brings channel
//! offers and doorbells; every request is answered at once (nothing here
//! waits), a channel's only while its completion ring has room.
//!
//! procfs serves every instance, so what one can make it hold is bounded
//! and charged before it is taken (`procproto::admission`): channels per
//! instance (the kernel's word for the instance, in the offer), mapped
//! grants per channel, the memory of one answer. And it serves no
//! instance's processes: the kernel gives it the system-wide record only
//! (`proc_query`), each instance's /proc/<pid> is its own server's.

#![no_std]
#![no_main]

extern crate alloc;

mod render;
mod tree;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;
use fsring::errno::*;
use fsring::{Buf, Completion, Request, Stat, Usage};
use procproto::admission::{Channels, Grants, GRANT_PAGES_PER_CHANNEL, MAX_CHANNELS, MAX_RESULT, REFUSED_PER_CHANNEL};
use ring::channel::{Header, Layout, Offer};
use ring::{Consumer, Producer, Ring, Wait};

oxrt::entry!(main);

const N: usize = fsring::SLOTS as usize;
const PAGE: u64 = 4096;
const EROFS: i64 = 30;
const E2BIG: i64 = 7;
/// Requests taken from one channel per round, for fairness.
const TAKE_PER_ROUND: usize = 16;
/// Rounds without a request before the loop sleeps: a client reading
/// several files in a row finds procfs awake (no doorbell, no wakeup).
const SPIN_BUDGET: u32 = 2_000;
/// The only messages procfs takes are the kernel's channel offers.
const MAX_MESSAGE: usize = ring::channel::OFFER_BYTES;

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

/// A grant mapped here.
#[derive(Clone, Copy)]
struct Grant {
    addr: *mut u8,
    pages: u64,
    writable: bool,
}

struct Chan {
    id: u64,
    header: &'static Header,
    requests: Consumer<'static, N>,
    completions: Producer<'static, N>,
    grants: BTreeMap<u32, Grant>,
    /// What its mapped grants take of procfs (`admission`).
    budget: Grants,
    /// Grants larger than a channel's whole budget, refused until `FORGET`
    /// (at most `REFUSED_PER_CHANNEL`).
    refused: BTreeSet<u32>,
}

struct Service {
    /// Indexed by the slots `admission` gives out.
    chans: Vec<Option<Chan>>,
    slots: Channels,
    rr: usize,
}

impl Chan {
    /// The grant `id`, mapped now if it is new, within the channel's
    /// budget: the kernel maps it only if it fits the room left (E2BIG
    /// otherwise, nothing mapped), and a grant larger than the whole budget
    /// is remembered as refused until `FORGET`. ENOMEM for either.
    fn grant(&mut self, id: u32) -> Result<Grant, i64> {
        if let Some(g) = self.grants.get(&id) {
            return Ok(*g);
        }
        if self.refused.contains(&id) {
            return Err(ENOMEM);
        }
        let room = self.budget.room()?;
        let (addr, pages, writable) = match oxrt::grant_map_max(self.id, id, room) {
            Ok(mapped) => mapped,
            Err((e, pages)) if e == -E2BIG => {
                if pages > GRANT_PAGES_PER_CHANNEL && self.refused.len() < REFUSED_PER_CHANNEL {
                    self.refused.insert(id);
                }
                return Err(ENOMEM);
            }
            Err((e, _)) if e == -ENOENT => return Err(EBADF),
            Err((e, _)) => return Err(-e),
        };
        self.budget.charge(pages).expect("the kernel kept to the room");
        let g = Grant { addr, pages, writable };
        self.grants.insert(id, g);
        Ok(g)
    }

    fn forget(&mut self, id: u32) {
        self.refused.remove(&id);
        if let Some(g) = self.grants.remove(&id) {
            let _ = oxrt::munmap(g.addr, (g.pages * PAGE) as usize);
            self.budget.uncharge(g.pages);
        }
    }

    /// The bytes of `buf` (a name), copied out once.
    fn copy_in(&mut self, buf: &Buf) -> Result<Vec<u8>, i64> {
        let g = self.grant(buf.grant)?;
        if buf.end() > g.pages * PAGE {
            return Err(EINVAL);
        }
        let mut out = vec![0u8; buf.len as usize];
        if !unsafe { oxrt::copy::copy(out.as_mut_ptr(), g.addr.add(buf.offset as usize), out.len()) } {
            // Revoked under us: its mapping is a reservation now.
            self.forget(buf.grant);
            return Err(EFAULT);
        }
        Ok(out)
    }

    /// Copies `data` to the start of `buf` (at most its length).
    fn copy_out(&mut self, buf: &Buf, data: &[u8]) -> Result<(), i64> {
        let g = self.grant(buf.grant)?;
        if !g.writable {
            return Err(EACCES);
        }
        if buf.end() > g.pages * PAGE || data.len() > buf.len as usize {
            return Err(EINVAL);
        }
        if !unsafe { oxrt::copy::copy(g.addr.add(buf.offset as usize), data.as_ptr(), data.len()) } {
            self.forget(buf.grant);
            return Err(EFAULT);
        }
        Ok(())
    }

    /// Answers one request: (status, values). (Handles: every inode's
    /// generation is 0.)
    fn execute(&mut self, request: Request) -> Result<(i64, [u64; 4]), i64> {
        match request {
            Request::Stat { ino } => {
                let ino = node(ino)?;
                if !tree::exists(ino) {
                    return Err(ENOENT);
                }
                let (mode, links) = tree::mode(ino);
                let now = oxrt::now() as u32;
                let st = Stat { mode, links, size: 0, atime: now, mtime: now, ctime: now, generation: 0 };
                Ok((0, st.to_values()))
            }
            Request::Lookup { dir, name } => {
                let bytes = self.copy_in(&name)?;
                let name = fsring::check_name(&bytes)?;
                let ino = tree::lookup(node(dir)?, name)?;
                Ok((0, [ino as u64, tree::mode(ino).0 as u64, 0, 0]))
            }
            Request::Read { ino, offset, buf } => {
                let data = tree::contents(node(ino)?)?;
                let start = (offset.min(data.len() as u64)) as usize;
                let end = data.len().min(start + buf.len as usize);
                self.copy_out(&buf, &data[start..end])?;
                Ok(((end - start) as i64, [data.len() as u64, 0, 0, 0]))
            }
            Request::Readdir { dir, cursor, buf } => {
                let all = tree::entries(node(dir)?)?;
                // At most MAX_RESULT bytes of procfs's memory, whatever the
                // buffer (the listing goes on at the cursor).
                let mut out = vec![0u8; (buf.len as usize).min(MAX_RESULT)];
                let (mut used, mut next) = (0, cursor as usize);
                while let Some((name, ino, kind)) = all.get(next) {
                    match fsring::put_dirent(&mut out[used..], *ino, *kind, name.as_bytes()) {
                        Some(n) => used += n,
                        None if used == 0 => return Err(EINVAL),
                        None => break,
                    }
                    next += 1;
                }
                self.copy_out(&buf, &out[..used])?;
                let cursor = if next >= all.len() { 0 } else { next as u64 };
                Ok((used as i64, [cursor, 0, 0, 0]))
            }
            // Neither tree has symlinks (`self` is the Linux server's).
            Request::Readlink { .. } => Err(EINVAL),
            // Two roots, known by number (`procproto::PROC_ROOT`, `SYSFS_ROOT`).
            Request::Root => Err(EINVAL),
            Request::Statfs => {
                let usage = Usage { block_size: PAGE as u32, ..Usage::default() };
                Ok((0, usage.to_values()))
            }
            Request::Forget { grant } => {
                self.forget(grant);
                Ok((0, [0; 4]))
            }
            // No holds, no caches, nothing to make durable.
            Request::Release { .. } | Request::Flush => Ok((0, [0; 4])),
            Request::Write { .. }
            | Request::Create { .. }
            | Request::Unlink { .. }
            | Request::Rename { .. }
            | Request::Truncate { .. }
            | Request::SetPerm { .. }
            | Request::SetTimes { .. }
            | Request::Promise { .. } => Err(EROFS),
        }
    }

    /// Takes and answers up to `TAKE_PER_ROUND` requests (as many as the
    /// completion ring has room for); whether it did any.
    fn serve(&mut self) -> bool {
        let mut any = false;
        for _ in 0..TAKE_PER_ROUND {
            if self.completions.room() == 0 {
                break;
            }
            let Some(d) = self.requests.pop() else { break };
            let (status, values) = match Request::decode(&d) {
                Ok(r) => self.execute(r).unwrap_or_else(|e| (-e, [0; 4])),
                Err(e) => (-e, [0; 4]),
            };
            let pushed = self.completions.push(&Completion { tag: d.tag, op: d.op, status, values }.to_desc());
            debug_assert!(pushed, "room was checked");
            any = true;
        }
        if any {
            self.completions.ring_doorbell(&Futex);
        }
        any
    }
}

impl Service {
    /// An offer from the kernel: the status to answer with.
    fn offer(&mut self, message: &[u8]) -> i64 {
        let Some(offer) = Offer::decode(message) else { return -EINVAL };
        if offer.slots != fsring::SLOTS || offer.shared != 0 {
            return -EINVAL;
        }
        // Charged before anything is taken: the instance's share of the
        // channels (the kernel's word for which instance it is).
        let slot = match self.slots.admit(offer.instance) {
            Ok(slot) => slot,
            Err(e) => return -e,
        };
        let base = match oxrt::chan_attach(offer.channel) {
            Ok(base) => base,
            Err(e) => {
                self.slots.release(slot);
                return e;
            }
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
            budget: Grants::default(),
            refused: BTreeSet::new(),
        });
        0
    }

    /// One round over the channels: their requests answered, channels whose
    /// client went let go of. Whether anything happened.
    fn poll(&mut self) -> bool {
        let mut any = false;
        let count = self.chans.len();
        for k in 0..count {
            let c = (self.rr + k) % count;
            let Some(chan) = self.chans[c].as_mut() else { continue };
            if chan.header.state() != 0 {
                let chan = self.chans[c].take().expect("checked");
                for (_, g) in chan.grants.iter() {
                    let _ = oxrt::munmap(g.addr, (g.pages * PAGE) as usize);
                }
                let _ = oxrt::chan_detach(chan.id);
                self.slots.release(c);
                any = true;
                continue;
            }
            any |= chan.serve();
        }
        self.rr = (self.rr + 1) % count;
        any
    }

    /// Polls until nothing happened for `SPIN_BUDGET` rounds (at once if
    /// nothing did in the first: no spinning for an event that brought no
    /// request).
    fn run(&mut self) {
        if !self.poll() {
            return;
        }
        let mut idle = 0;
        while idle < SPIN_BUDGET {
            if self.poll() {
                idle = 0;
            } else {
                idle += 1;
                core::hint::spin_loop();
            }
        }
    }

    /// Announces the sleep in every ring and arms its doorbell: false if a
    /// request came meanwhile (then nothing sleeps). A ring with requests
    /// but no room for their completions sleeps too, until the client's
    /// doorbell (it rings after taking completions, `fsring` "Room").
    fn prepare_sleep(&mut self) -> bool {
        for chan in self.chans.iter_mut().flatten() {
            let tail = if chan.completions.room() == 0 {
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

    fn awake(&mut self) {
        for chan in self.chans.iter_mut().flatten() {
            chan.requests.awake();
        }
    }
}

fn main(_args: Vec<&'static str>) -> i32 {
    // Copies to and from grants fail instead of killing procfs when a
    // client revokes one meanwhile.
    if let Err(e) = oxrt::copy::register() {
        oxrt::println!("procfs: cannot register the copy fixup: {}", e);
        return 1;
    }
    if let Err(e) = oxrt::ipc_register_with(procproto::SERVICE, procproto::PROC_ROOT as u64, oxrt::IPC_CHANNELS) {
        oxrt::println!("procfs: cannot register: {}", e);
        return 1;
    }
    let mut service = Service { chans: (0..MAX_CHANNELS).map(|_| None).collect(), slots: Channels::default(), rr: 0 };
    let mut message = vec![0u8; MAX_MESSAGE];
    loop {
        let sleep = service.prepare_sleep();
        let event = oxrt::ipc_receive(&mut message, if sleep { None } else { Some(0) });
        service.awake();
        match event {
            Ok(oxrt::Event::Control(id, len)) => {
                let status = service.offer(&message[..len]);
                let _ = oxrt::ipc_reply(id, &status.to_le_bytes());
            }
            // No protocol besides the rings.
            Ok(oxrt::Event::Request(id, _)) => {
                let _ = oxrt::ipc_reply(id, &(-ENOSYS).to_le_bytes());
            }
            _ => {}
        }
        service.run();
    }
}

/// The inode a handle names: procfs's inodes all have generation 0.
fn node(n: fsring::Node) -> Result<u32, i64> {
    if n.generation != 0 {
        return Err(fsring::errno::ESTALE);
    }
    Ok(n.ino)
}
