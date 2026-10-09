//! The instance's channel to netd (phase R7b, ADR 0008, the protocol
//! `netring`): requests (`ringclient`'s slots), the shared area with a
//! control block per socket, the buffer pool the sockets' rings live in,
//! and the net thread.
//!
//! **The channel** is made at the first socket (`net`), with
//! `netring::SHARED_PAGES` of shared area; when netd dies its sockets fail
//! and the next socket makes a new channel (the kernel starts netd again).
//!
//! **The pool.** A socket's rings (a receive and a send ring of `RING`
//! bytes each: an *area*) are part of a memory object of `CHUNK_AREAS`
//! areas, mapped into the server's region and granted to the channel once.
//! Chunks are made as sockets come; an empty chunk goes (`FORGET`, then
//! the revoke) at once if another has room, the last one after five
//! seconds as a spare (`put_rings`, `trim_pool`), so an instance without
//! sockets soon holds no pool memory.
//!
//! **The net thread** (`ROLE_NET`, a service thread of the pager's
//! process) takes the sockets netd marked (the client bitmap) and reports
//! their readiness (`inet::InetSock::netd_changed`), and closes the
//! sockets whose last descriptor the pager saw go (a close(2) closes its
//! socket itself, `Net::close_now`): `CLOSE` to netd, then their control
//! block and area are free. It sleeps on the shared area's doorbell, which
//! netd rings when it marked a socket and the thread announced its sleep,
//! and the server when it queued a close. When the channel dies (netd
//! went) it fails every socket of it and waits for the next channel.
//!
//! Locks: `Net::socks`, `Net::pool`, `Net::closing` and `Net::free` are
//! leaves, never held across a request to netd or a copy of program
//! memory. `Net::links` is held across its `LINKS` request (it guards the
//! page netd writes the answer into), never across a copy of program
//! memory, and nothing is taken under it.

use crate::inet::InetSock;
use crate::ringclient::{futex_wake, RingClient};
use crate::sync::Mutex;
use crate::syscall;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use netring::{Area, Buf, Completion, Ctl, Link, Request, SharedArea, Wait, MAX_SOCKETS, SERVICE, SHARED_PAGES};
use restricted::*;

const N: usize = netring::SLOTS as usize;
const PAGE: u64 = 4096;
/// Each ring of a socket.
pub const RING: u32 = 64 * 1024;
/// Areas of a pool chunk (2 MiB).
const CHUNK_AREAS: u64 = 16;
const AREA_BYTES: u64 = 2 * RING as u64;
const CHUNK_PAGES: u64 = CHUNK_AREAS * AREA_BYTES / PAGE;
/// How long the last empty chunk stays as a spare.
const SPARE_NS: u64 = 5_000_000_000;

pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const EAGAIN: i64 = 11;
pub const ENOBUFS: i64 = 105;
pub const ENETDOWN: i64 = 100;
const ETIMEDOUT: i64 = 110;

/// The net thread's sleep: a futex wait until a deadline (monotonic
/// nanoseconds, 0: none).
struct FutexUntil(u64);

impl Wait for FutexUntil {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let _ = syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, value as u64, self.0, 0, 0, 0]);
    }

    fn wake(&self, word: &AtomicU32) {
        futex_wake(word, i32::MAX as u64);
    }
}

/// A futex wait on a shared-area word, and a wake of every waiter.
pub struct Futex;

impl Wait for Futex {
    fn wait(&self, word: &AtomicU32, value: u32) {
        crate::ringclient::futex_wait(word, value);
    }

    fn wake(&self, word: &AtomicU32) {
        futex_wake(word, i32::MAX as u64);
    }
}

/// Sleeps on `word` while it holds `seen`, until `deadline` (0: none),
/// interruptibly: EINTR for a signal (or the thread's death), EAGAIN when
/// the time is up, EPIPE once the channel is gone.
pub fn sleep(word: &AtomicU32, seen: u32, deadline: u64) -> Result<(), i64> {
    match syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, seen as u64, deadline, FUTEX_INTERRUPTIBLE, 0, 0]) {
        r if r == -EINTR => Err(EINTR),
        r if r == -ETIMEDOUT => Err(EAGAIN),
        r if r < 0 && r != -EAGAIN => Err(-r),
        _ => Ok(()),
    }
}

/// A socket's rings in the server's memory.
#[derive(Clone, Copy, Debug)]
pub struct Rings {
    /// As netd names them.
    pub area: Area,
    /// Where the receive ring starts in the server's mapping (the send
    /// ring follows it).
    pub addr: u64,
    /// The pool's chunk and the area's slot in it.
    chunk: usize,
    slot: u32,
}

impl Rings {
    pub fn rx(&self) -> *mut u8 {
        self.addr as *mut u8
    }

    pub fn tx(&self) -> *mut u8 {
        (self.addr + self.area.size as u64) as *mut u8
    }

    pub fn size(&self) -> u32 {
        self.area.size
    }
}

/// A memory object of the pool, mapped and granted.
struct Chunk {
    object: u64,
    addr: u64,
    grant: u32,
    /// One bit per area in use.
    used: u64,
}

struct Pool {
    chunks: Vec<Option<Chunk>>,
    /// The last chunk, empty since then (monotonic nanoseconds).
    spare: Option<(usize, u64)>,
}

/// A socket the net thread closes.
struct Closing {
    index: u32,
    rings: Option<Rings>,
    abort: bool,
}

pub struct Net {
    ring: RingClient<N>,
    pub area: &'static SharedArea,
    /// Control blocks not in use.
    free: Mutex<Vec<u32>>,
    /// The sockets by control block, for the net thread.
    socks: Mutex<Vec<Weak<InetSock>>>,
    pool: Mutex<Pool>,
    closing: Mutex<Vec<Closing>>,
    /// A page granted for `LINKS`' result.
    links: Mutex<(u64, u64, u32)>,
}

/// The instance's current channel, and its generation (the net thread
/// waits on it for a new one).
static NET: Mutex<Option<Arc<Net>>> = Mutex::new(None);
static GENERATION: AtomicU32 = AtomicU32::new(0);
/// Closes queued and not yet done (the pager waits for them before the
/// instance goes).
static CLOSES: AtomicU32 = AtomicU32::new(0);

/// The channel to netd, made (or made again after netd died) now if need
/// be.
pub fn net() -> Result<Arc<Net>, i64> {
    let mut current = NET.lock();
    if let Some(n) = current.as_ref().filter(|n| !n.ring.is_dead()) {
        return Ok(n.clone());
    }
    let n = Arc::new(Net::connect()?);
    *current = Some(n.clone());
    drop(current);
    GENERATION.fetch_add(1, Ordering::SeqCst);
    futex_wake(&GENERATION, u32::MAX as u64);
    Ok(n)
}

/// The channel if there is one (without making one).
fn current() -> Option<Arc<Net>> {
    NET.lock().clone()
}

impl Net {
    fn connect() -> Result<Net, i64> {
        // No service (no network card, netd never started): the network
        // is down.
        let ring = RingClient::connect(SERVICE, SHARED_PAGES).map_err(|e| if e == 2 { ENETDOWN } else { e })?;
        let area = unsafe { SharedArea::at(ring.shared()) };
        let links = {
            let object = syscall(SYS_MO_CREATE, [1, 0, 0, 0, 0, 0]);
            if object < 0 {
                return Err(-object);
            }
            let addr = syscall(SYS_MO_MAP_SERVER, [object as u64, 1, 0, 0, 0, 0]);
            let grant = if addr < 0 { addr } else { syscall(SYS_GRANT, [ring.handle(), object as u64, 0, 1, GRANT_WRITE, 0]) };
            if grant <= 0 {
                if addr > 0 {
                    syscall(SYS_MO_UNMAP_SERVER, [addr as u64, 0, 0, 0, 0, 0]);
                }
                syscall(SYS_HANDLE_CLOSE, [object as u64, 0, 0, 0, 0, 0]);
                return Err(if grant < 0 { -grant } else { EIO });
            }
            (object as u64, addr as u64, grant as u32)
        };
        Ok(Net {
            ring,
            area,
            free: Mutex::new((0..MAX_SOCKETS as u32).rev().collect()),
            socks: Mutex::new((0..MAX_SOCKETS).map(|_| Weak::new()).collect()),
            pool: Mutex::new(Pool { chunks: Vec::new(), spare: None }),
            closing: Mutex::new(Vec::new()),
            links: Mutex::new(links),
        })
    }

    /// Whether netd went (or broke the protocol): every socket of this
    /// channel has failed.
    pub fn is_dead(&self) -> bool {
        self.ring.is_dead()
    }

    pub fn ctl(&self, index: u32) -> &Ctl {
        self.area.ctl(index as usize).expect("an index the client handed out")
    }

    /// Sends a request and waits for its completion (EIO if netd went).
    pub fn call(&self, r: Request) -> Result<Completion, i64> {
        self.ring.call(r.encode(0))
    }

    /// `call`, its status as the result: the value, or the errno.
    pub fn status(&self, r: Request) -> Result<Completion, i64> {
        let c = self.call(r)?;
        if c.status < 0 {
            return Err(-c.status);
        }
        Ok(c)
    }

    /// Tells netd that socket `index` has work for it (bytes to send,
    /// room it waits for): marks it and rings its doorbell.
    pub fn kick(&self, index: u32) {
        self.area.header.mark_service(index as usize);
        self.ring.doorbell();
    }

    /// A control block for a new socket (ENOBUFS if all are in use), its
    /// client half reset.
    pub fn take_index(&self) -> Result<u32, i64> {
        let i = self.free.lock().pop().ok_or(ENOBUFS)?;
        self.ctl(i).reset_client();
        Ok(i)
    }

    /// Gives back a control block whose socket netd never knew (or no
    /// longer knows).
    pub fn put_index(&self, i: u32) {
        self.socks.lock()[i as usize] = Weak::new();
        self.free.lock().push(i);
    }

    /// Registers socket `s` under its control block, for the net thread.
    pub fn register(&self, index: u32, s: &Arc<InetSock>) {
        self.socks.lock()[index as usize] = Arc::downgrade(s);
    }

    /// Rings for a socket from the pool (a new chunk if all are full).
    pub fn take_rings(&self) -> Result<Rings, i64> {
        let mut pool = self.pool.lock();
        for (k, chunk) in pool.chunks.iter_mut().enumerate() {
            let Some(chunk) = chunk else { continue };
            if chunk.used != (1 << CHUNK_AREAS) - 1 {
                let slot = (!chunk.used).trailing_zeros();
                chunk.used |= 1 << slot;
                return Ok(Self::rings_of(chunk, k, slot));
            }
        }
        let chunk = Chunk::new(self.ring.handle())?;
        let k = match pool.chunks.iter().position(Option::is_none) {
            Some(k) => k,
            None => {
                pool.chunks.push(None);
                pool.chunks.len() - 1
            }
        };
        let mut chunk = chunk;
        chunk.used = 1;
        let r = Self::rings_of(&chunk, k, 0);
        pool.chunks[k] = Some(chunk);
        Ok(r)
    }

    fn rings_of(chunk: &Chunk, k: usize, slot: u32) -> Rings {
        let offset = slot as u64 * AREA_BYTES;
        Rings { area: Area { grant: chunk.grant, offset: offset as u32, size: RING }, addr: chunk.addr + offset, chunk: k, slot }
    }

    /// Gives rings back (netd no longer uses them). A chunk that empties
    /// goes at once if another one has room; the last one stays as a spare
    /// for `SPARE_NS` (a program that closes and opens sockets in turn
    /// does not make and revoke a chunk each time) and then goes too
    /// (`trim_pool`, by the net thread), so an instance without sockets
    /// holds no pool memory.
    pub fn put_rings(&self, r: Rings) {
        let gone = {
            let mut pool = self.pool.lock();
            let Some(Some(chunk)) = pool.chunks.get_mut(r.chunk) else { return };
            chunk.used &= !(1 << r.slot);
            if chunk.used != 0 {
                return;
            }
            let room_elsewhere = pool.chunks.iter().enumerate().any(|(k, c)| k != r.chunk && c.as_ref().is_some_and(|c| c.used != (1 << CHUNK_AREAS) - 1));
            if room_elsewhere {
                pool.chunks[r.chunk].take()
            } else {
                pool.spare = Some((r.chunk, crate::ringclient::now()));
                None
            }
        };
        if let Some(chunk) = gone {
            self.free_chunk(r.chunk, chunk);
        }
    }

    /// Frees an empty chunk that left the pool: forgotten before it is
    /// revoked (no draining), and revoked only once netd let go of it (or
    /// is gone); else it goes back into the pool.
    fn free_chunk(&self, k: usize, chunk: Chunk) {
        if self.is_dead() || self.status(Request::Forget { grant: chunk.grant }).is_ok() {
            syscall(SYS_REVOKE, [self.ring.handle(), chunk.grant as u64, 0, 0, 0, 0]);
            drop(chunk);
        } else {
            let mut pool = self.pool.lock();
            match pool.chunks.get_mut(k) {
                Some(slot @ None) => *slot = Some(chunk),
                _ => pool.chunks.push(Some(chunk)),
            }
        }
    }

    /// The net thread: frees the spare chunk once it was empty for
    /// `SPARE_NS`; when to look again (monotonic nanoseconds, 0: no spare).
    fn trim_pool(&self) -> u64 {
        let gone = {
            let mut pool = self.pool.lock();
            let Some((k, since)) = pool.spare else { return 0 };
            let empty = pool.chunks.get(k).and_then(Option::as_ref).is_some_and(|c| c.used == 0);
            if !empty {
                pool.spare = None;
                return 0;
            }
            if crate::ringclient::now() < since + SPARE_NS {
                return since + SPARE_NS;
            }
            pool.spare = None;
            pool.chunks[k].take().map(|c| (k, c))
        };
        if let Some((k, chunk)) = gone {
            self.free_chunk(k, chunk);
        }
        0
    }

    /// netd's interfaces.
    pub fn links(&self) -> Result<Vec<Link>, i64> {
        let page = self.links.lock();
        let (_, addr, grant) = *page;
        let c = self.status(Request::Links { buf: Buf { grant, offset: 0, len: PAGE as u32 } })?;
        let n = (c.status as usize).min(PAGE as usize);
        // Copied out once, then decoded (netd may write it any time).
        let mut bytes = alloc::vec![0u8; n];
        unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, bytes.as_mut_ptr(), n) };
        drop(page);
        Ok(Link::decode_all(&bytes).collect())
    }

    /// Queues socket `index` (with its rings) for the net thread to close.
    /// On a dead channel nothing needs netd: done here (the net thread may
    /// have moved on to the next channel).
    pub fn queue_close(&self, index: u32, rings: Option<Rings>, abort: bool) {
        CLOSES.fetch_add(1, Ordering::SeqCst);
        self.closing.lock().push(Closing { index, rings, abort });
        if self.is_dead() {
            self.close_queued();
        } else {
            self.area.header.poke_client(&Futex);
        }
    }

    /// Closes socket `index` in netd now and frees its control block and
    /// rings.
    pub fn close_now(&self, index: u32, rings: Option<Rings>, abort: bool) {
        if !self.is_dead() {
            let _ = self.call(Request::Close { sock: index, abort });
        }
        if let Some(r) = rings {
            self.put_rings(r);
        }
        self.put_index(index);
    }

    /// The net thread: closes what was queued; true if there was any.
    fn close_queued(&self) -> bool {
        let queued = core::mem::take(&mut *self.closing.lock());
        let any = !queued.is_empty();
        for c in queued {
            self.close_now(c.index, c.rings, c.abort);
            CLOSES.fetch_sub(1, Ordering::SeqCst);
            futex_wake(&CLOSES, u32::MAX as u64);
        }
        any
    }

    /// The sockets of this channel that still live.
    fn live(&self) -> Vec<Arc<InetSock>> {
        self.socks.lock().iter().filter_map(Weak::upgrade).collect()
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        let (object, addr, _) = *self.links.lock();
        syscall(SYS_MO_UNMAP_SERVER, [addr, 0, 0, 0, 0, 0]);
        syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
        // The pool's chunks go with `pool` (after `ring`: the channel, and
        // with it every grant, went first).
    }
}

impl Chunk {
    fn new(channel: u64) -> Result<Chunk, i64> {
        let object = syscall(SYS_MO_CREATE, [CHUNK_PAGES, 0, 0, 0, 0, 0]);
        if object < 0 {
            return Err(if -object == 12 { ENOBUFS } else { -object });
        }
        let object = object as u64;
        let addr = syscall(SYS_MO_MAP_SERVER, [object, CHUNK_PAGES, 0, 0, 0, 0]);
        if addr < 0 {
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(-addr);
        }
        let grant = syscall(SYS_GRANT, [channel, object, 0, CHUNK_PAGES, GRANT_WRITE, 0]);
        if grant <= 0 {
            syscall(SYS_MO_UNMAP_SERVER, [addr as u64, 0, 0, 0, 0, 0]);
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(if grant < 0 { -grant } else { EIO });
        }
        Ok(Chunk { object, addr: addr as u64, grant: grant as u32, used: 0 })
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        syscall(SYS_MO_UNMAP_SERVER, [self.addr, 0, 0, 0, 0, 0]);
        syscall(SYS_HANDLE_CLOSE, [self.object, 0, 0, 0, 0, 0]);
    }
}

/// netd's interfaces (none without netd).
pub fn links() -> Vec<Link> {
    // A channel that died meanwhile (netd went, or gave an idle channel's
    // slot away) is made again, once.
    let ask = || -> Result<Vec<Link>, i64> {
        let n = net()?;
        match n.links() {
            Err(_) if n.is_dead() => net()?.links(),
            r => r,
        }
    };
    ask().unwrap_or_default()
}

/// Waits (at most a few seconds) until the closes queued so far are done:
/// before the instance goes, so that what its sockets still had to send
/// reaches netd (`CLOSE` hands it over).
pub fn settle() {
    let deadline = crate::ringclient::now() + 5_000_000_000;
    loop {
        let left = CLOSES.load(Ordering::SeqCst);
        if left == 0 || crate::ringclient::now() >= deadline {
            return;
        }
        syscall(SYS_SERVER_FUTEX_WAIT, [&CLOSES as *const AtomicU32 as u64, left as u64, deadline, 0, 0, 0]);
    }
}

/// The net thread.
pub fn thread() -> ! {
    loop {
        let generation = GENERATION.load(Ordering::SeqCst);
        let Some(net) = current() else {
            syscall(SYS_SERVER_FUTEX_WAIT, [&GENERATION as *const AtomicU32 as u64, generation as u64, 0, 0, 0, 0]);
            continue;
        };
        serve(&net, generation);
    }
}

/// Serves channel `net` until it dies or a new one replaced it.
fn serve(net: &Arc<Net>, generation: u32) {
    let header = &net.area.header;
    loop {
        if net.is_dead() {
            // netd went: every socket of the channel fails; closes need no
            // request any more.
            net.close_queued();
            for s in net.live() {
                s.netd_changed();
            }
            if GENERATION.load(Ordering::SeqCst) == generation {
                // Until the next socket makes a new channel.
                syscall(SYS_SERVER_FUTEX_WAIT, [&GENERATION as *const AtomicU32 as u64, generation as u64, 0, 0, 0, 0]);
            }
            // Closes queued meanwhile still need doing.
            net.close_queued();
            return;
        }
        if GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        let seen = header.client_seen();
        let mut any = false;
        header.take_client(|i| {
            any = true;
            let s = net.socks.lock().get(i).and_then(Weak::upgrade);
            if let Some(s) = s {
                s.netd_changed();
            }
        });
        any |= net.close_queued();
        if any {
            continue;
        }
        // (Until the spare chunk's time is up, if there is one.)
        let until = net.trim_pool();
        header.sleep_client(seen, &FutexUntil(until));
    }
}
