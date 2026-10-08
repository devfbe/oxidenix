//! The server's end of the file protocol (`fsring`, docs/design/io-rings.md)
//! to diskfs: one channel for the instance, shared by every thread of the
//! tree's processes and by the pager thread.
//!
//! **Slots.** Each request in flight has a slot (`fsring::SLOTS` of them,
//! as many as the rings hold): the request's tag names it (its index and a
//! count of the slot's uses), and the slot receives the completion. So at
//! most `SLOTS` requests are in flight: the submission ring always has
//! room, and so does the completion ring for every request diskfs takes.
//! A thread that holds no slot waits for one; one that holds some never
//! does (`run` completes one of its own first), so slots always come free.
//!
//! **Completions.** One waiting thread at a time takes completions from the
//! ring (it *reaps*: the `reaping` flag) and hands each to its slot, waking
//! the slot's owner; the others sleep on their slot's word. A reaper whose
//! own request completed stops reaping and wakes one sleeping owner whose
//! request is still in flight to take over. Its flag and the owners'
//! `sleeping` marks are written then read on opposite sides of SeqCst
//! fences, so either the reaper sees a sleeper or the sleeper sees that
//! no one reaps and reaps itself: no request waits without a reaper. The
//! reaper sleeps on the completion ring's doorbell; after taking
//! completions while requests wait in the submission ring it rings
//! diskfs's (fsring, "Room").
//!
//! **A hostile or dead diskfs.** A completion whose tag names no slot in
//! flight, or whose operation is not the slot's, is dropped. When diskfs
//! goes (the channel's `state`), every request in flight fails with EIO and
//! the client is dead: `datafs` makes a new one (diskfs is started again).
//! Callers check every status and value they use (`datafs`).
//!
//! **Scratch.** Names, directory entries, link targets and O_DIRECT reads
//! travel in a scratch buffer: a memory object mapped into the server's
//! region (`SYS_MO_MAP_SERVER`) and granted to diskfs once, handed out in
//! page runs (`scratch`).
//!
//! A thread that is being killed cannot sleep in the kernel: it polls until
//! its requests completed (a disk request is short), so no slot, grant or
//! pending page is ever abandoned.

use crate::sync::Mutex;
use crate::syscall;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use fsring::{Buf, Completion, SERVICE};
use restricted::*;
use ring::channel::{Header, Layout};
use ring::{Consumer, Desc, Producer, Ring, Wait};

const N: usize = fsring::SLOTS as usize;
pub const PAGE: u64 = 4096;
/// Pages of the scratch buffer.
pub const SCRATCH_PAGES: u64 = 64;

pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const EPIPE: i64 = 32;

/// A slot's states.
const FREE: u32 = 0;
const BUSY: u32 = 1;
const DONE: u32 = 2;

/// Sleeps while `word` holds `value` (a server futex: on a ring word the
/// channel's object, else the server's own memory). A thread being killed
/// cannot sleep: it yields instead, and its caller polls.
fn futex_wait(word: &AtomicU32, value: u32) {
    let r = syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, value as u64, 0, 0, 0, 0]);
    if r == -EINTR {
        syscall(SYS_YIELD, [0; 6]);
    }
}

fn futex_wake(word: &AtomicU32, n: u64) {
    syscall(SYS_SERVER_FUTEX_WAKE, [word as *const AtomicU32 as u64, n, 0, 0, 0, 0]);
}

struct Doorbell;

impl Wait for Doorbell {
    fn wait(&self, word: &AtomicU32, value: u32) {
        futex_wait(word, value);
    }

    fn wake(&self, word: &AtomicU32) {
        futex_wake(word, 1);
    }
}

/// A request in flight, or its completion waiting to be taken.
struct Slot {
    /// Advanced when the completion arrived or the owner should reap; the
    /// owner sleeps on it.
    word: AtomicU32,
    state: AtomicU32,
    /// The owner sleeps (or is about to).
    sleeping: AtomicBool,
    /// The tag and operation of the request in flight.
    tag: AtomicU64,
    op: AtomicU32,
    /// Uses so far: the tag's upper part.
    uses: AtomicU64,
    /// Written by the reaper before `state` becomes DONE.
    result: UnsafeCell<Completion>,
}

// `result` is written by the reaper only while the slot is BUSY and read
// by its owner only once it is DONE (Release/Acquire on `state`).
unsafe impl Sync for Slot {}

/// A request in flight: whoever submitted it must `wait` for it.
#[must_use]
pub struct Ticket(usize);

pub struct Client {
    handle: u64,
    header: &'static Header,
    requests: Mutex<Producer<'static, N>>,
    /// Used only by the thread that holds `reaping`.
    completions: UnsafeCell<Consumer<'static, N>>,
    reaping: AtomicBool,
    slots: Vec<Slot>,
    free: Mutex<Vec<usize>>,
    /// Advanced when a slot comes free (threads waiting for one sleep on
    /// it).
    freed: AtomicU32,
    dead: AtomicBool,
    /// Which connection this is (`datafs` counts them).
    pub generation: u64,
    scratch: ScratchArea,
}

// The consumer is touched only by the reaper (the `reaping` flag); the
// rest is atomics and locks.
unsafe impl Sync for Client {}
unsafe impl Send for Client {}

struct ScratchArea {
    object: u64,
    addr: u64,
    grant: u32,
    /// One bit per page in use.
    used: Mutex<u64>,
    freed: AtomicU32,
}

impl Client {
    /// A new channel to diskfs (started again if it died), connected, with
    /// its scratch buffer mapped and granted.
    pub fn connect(generation: u64) -> Result<Client, i64> {
        let mut addr = 0u64;
        let h = syscall(SYS_CHAN_CREATE, [N as u64, &mut addr as *mut u64 as u64, 0, 0, 0, 0]);
        if h < 0 {
            return Err(-h);
        }
        let handle = h as u64;
        let close = |e: i64| {
            syscall(SYS_HANDLE_CLOSE, [handle, 0, 0, 0, 0, 0]);
            e
        };
        let layout = Layout::new(N as u32).expect("a valid slot count");
        let base = addr as *const u8;
        // Mapped until the handle is closed (`drop`).
        let (sub, comp) = unsafe { (layout.ring::<N>(base, layout.submission), layout.ring::<N>(base, layout.completion)) };
        let r = syscall(SYS_CHAN_CONNECT, [handle, SERVICE.as_ptr() as u64, SERVICE.len() as u64, 0, 0, 0]);
        if r < 0 {
            return Err(close(-r));
        }
        let scratch = ScratchArea::new(handle).map_err(close)?;
        let slots = (0..N)
            .map(|_| Slot {
                word: AtomicU32::new(0),
                state: AtomicU32::new(FREE),
                sleeping: AtomicBool::new(false),
                tag: AtomicU64::new(0),
                op: AtomicU32::new(0),
                uses: AtomicU64::new(0),
                result: UnsafeCell::new(Completion::default()),
            })
            .collect();
        Ok(Client {
            handle,
            header: unsafe { Header::at(base) },
            requests: Mutex::new(Ring::new(sub).producer()),
            completions: UnsafeCell::new(Ring::new(comp).consumer()),
            reaping: AtomicBool::new(false),
            slots,
            free: Mutex::new((0..N).rev().collect()),
            freed: AtomicU32::new(0),
            dead: AtomicBool::new(false),
            generation,
            scratch,
        })
    }

    /// Whether diskfs is gone (or misbehaved): the client takes no new
    /// requests, a new one must be made.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire) || self.header.state() != 0
    }

    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// A free slot; with `block`, waits for one.
    fn acquire(&self, block: bool) -> Option<usize> {
        loop {
            let seen = self.freed.load(Ordering::Acquire);
            if let Some(i) = self.free.lock().pop() {
                return Some(i);
            }
            if !block {
                return None;
            }
            futex_wait(&self.freed, seen);
        }
    }

    fn release(&self, i: usize) {
        self.slots[i].state.store(FREE, Ordering::Release);
        self.free.lock().push(i);
        self.freed.fetch_add(1, Ordering::Release);
        futex_wake(&self.freed, 1);
    }

    /// Submits `d` (its tag is set here). Without `block`, None if no slot
    /// is free. EPIPE once the client is dead.
    pub fn submit(&self, mut d: Desc, block: bool) -> Result<Option<Ticket>, i64> {
        if self.is_dead() {
            return Err(EPIPE);
        }
        let Some(i) = self.acquire(block) else { return Ok(None) };
        let s = &self.slots[i];
        let uses = s.uses.fetch_add(1, Ordering::Relaxed) + 1;
        d.tag = uses << 8 | i as u64;
        s.tag.store(d.tag, Ordering::Relaxed);
        s.op.store(d.op as u32, Ordering::Relaxed);
        s.state.store(BUSY, Ordering::SeqCst);
        let pushed = {
            let mut requests = self.requests.lock();
            let pushed = requests.push(&d);
            requests.ring_doorbell(&Doorbell);
            pushed
        };
        if !pushed {
            // Never with as many slots as the ring has: diskfs did not take
            // what it completed. It is not to be trusted any more.
            self.dead.store(true, Ordering::Release);
            self.complete(i, Completion { tag: d.tag, op: d.op, status: -EIO, values: [0; 4] });
        }
        Ok(Some(Ticket(i)))
    }

    /// The completion of `t` (status -EIO if diskfs went first).
    pub fn wait(&self, t: Ticket) -> Completion {
        let i = t.0;
        let s = &self.slots[i];
        loop {
            let seen = s.word.load(Ordering::SeqCst);
            if s.state.load(Ordering::Acquire) == DONE {
                break;
            }
            if self.reaping.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                self.reap_until(i);
                self.stop_reaping();
                continue;
            }
            s.sleeping.store(true, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if s.state.load(Ordering::SeqCst) != DONE && self.reaping.load(Ordering::SeqCst) {
                futex_wait(&s.word, seen);
            }
            s.sleeping.store(false, Ordering::SeqCst);
        }
        let c = unsafe { *s.result.get() };
        self.release(i);
        c
    }

    /// Sends `d` and waits for its completion; EPIPE if the client is dead.
    pub fn call(&self, d: Desc) -> Result<Completion, i64> {
        let t = self.submit(d, true)?.expect("a blocking submit gets a slot");
        Ok(self.wait(t))
    }

    /// Takes completions (as the reaper) until slot `i`'s arrived.
    fn reap_until(&self, i: usize) {
        let completions = unsafe { &mut *self.completions.get() };
        let header = self.header;
        loop {
            let mut took = false;
            while let Some(d) = completions.pop() {
                took = true;
                self.deliver(&d);
            }
            if took {
                // Requests waiting for room may be taken now.
                let mut requests = self.requests.lock();
                if requests.room() < N {
                    requests.ring_doorbell(&Doorbell);
                }
            }
            if self.slots[i].state.load(Ordering::Acquire) == DONE {
                return;
            }
            match completions.pop_wait_while(&Doorbell, || header.state() == 0) {
                Some(d) => self.deliver(&d),
                None => {
                    self.fail_all();
                    return;
                }
            }
        }
    }

    /// Stops reaping; wakes an owner still waiting, to take over.
    fn stop_reaping(&self) {
        self.reaping.store(false, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if let Some(s) = self.slots.iter().find(|s| s.state.load(Ordering::SeqCst) == BUSY && s.sleeping.load(Ordering::SeqCst)) {
            s.word.fetch_add(1, Ordering::SeqCst);
            futex_wake(&s.word, 1);
        }
    }

    /// Hands a completion to its slot (a forged or stale one is dropped).
    fn deliver(&self, d: &Desc) {
        let c = Completion::from_desc(d);
        let i = (c.tag & 0xff) as usize;
        let Some(s) = self.slots.get(i) else { return };
        if s.state.load(Ordering::SeqCst) != BUSY || s.tag.load(Ordering::Relaxed) != c.tag || s.op.load(Ordering::Relaxed) != c.op as u32 {
            return;
        }
        self.complete(i, c);
    }

    fn complete(&self, i: usize, c: Completion) {
        let s = &self.slots[i];
        unsafe { *s.result.get() = c };
        s.state.store(DONE, Ordering::SeqCst);
        s.word.fetch_add(1, Ordering::SeqCst);
        if s.sleeping.load(Ordering::SeqCst) {
            futex_wake(&s.word, 1);
        }
    }

    /// diskfs is gone: every request in flight fails.
    fn fail_all(&self) {
        self.dead.store(true, Ordering::Release);
        for (i, s) in self.slots.iter().enumerate() {
            if s.state.load(Ordering::SeqCst) == BUSY {
                let (tag, op) = (s.tag.load(Ordering::Relaxed), s.op.load(Ordering::Relaxed) as u16);
                self.complete(i, Completion { tag, op, status: -EIO, values: [0; 4] });
            }
        }
    }

    /// `pages` pages of the scratch buffer (at most `SCRATCH_PAGES`), waiting
    /// until they are free.
    pub fn scratch(&self, pages: u64) -> Scratch<'_> {
        let pages = pages.clamp(1, SCRATCH_PAGES);
        let mask = if pages == 64 { u64::MAX } else { (1u64 << pages) - 1 };
        let area = &self.scratch;
        loop {
            let seen = area.freed.load(Ordering::Acquire);
            {
                let mut used = area.used.lock();
                if let Some(first) = (0..=SCRATCH_PAGES - pages).find(|&f| *used & (mask << f) == 0) {
                    *used |= mask << first;
                    return Scratch { client: self, first, pages };
                }
            }
            futex_wait(&area.freed, seen);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // The channel's end goes: every grant is revoked, diskfs lets go of
        // what this client held.
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
        syscall(SYS_MO_UNMAP_SERVER, [self.scratch.addr, 0, 0, 0, 0, 0]);
        syscall(SYS_HANDLE_CLOSE, [self.scratch.object, 0, 0, 0, 0, 0]);
    }
}

impl ScratchArea {
    fn new(channel: u64) -> Result<ScratchArea, i64> {
        let object = syscall(SYS_MO_CREATE, [SCRATCH_PAGES, 0, 0, 0, 0, 0]);
        if object < 0 {
            return Err(-object);
        }
        let object = object as u64;
        let addr = syscall(SYS_MO_MAP_SERVER, [object, SCRATCH_PAGES, 0, 0, 0, 0]);
        if addr < 0 {
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(-addr);
        }
        let grant = syscall(SYS_GRANT, [channel, object, 0, SCRATCH_PAGES, GRANT_WRITE, 0]);
        if grant <= 0 {
            syscall(SYS_MO_UNMAP_SERVER, [addr as u64, 0, 0, 0, 0, 0]);
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(if grant < 0 { -grant } else { EIO });
        }
        Ok(ScratchArea { object, addr: addr as u64, grant: grant as u32, used: Mutex::new(0), freed: AtomicU32::new(0) })
    }
}

/// Pages of the scratch buffer, free again when dropped. diskfs may write
/// them at any time while it has the grant: what is read back is copied
/// out once and checked.
pub struct Scratch<'a> {
    client: &'a Client,
    first: u64,
    pages: u64,
}

impl Scratch<'_> {
    pub fn len(&self) -> u64 {
        self.pages * PAGE
    }

    /// The range at `offset` (within these pages) as a buffer of the grant.
    pub fn buf(&self, offset: u64, len: u64) -> Buf {
        let at = (self.first * PAGE + offset.min(self.len())) as u32;
        Buf { grant: self.client.scratch.grant, offset: at, len: len.min(self.len() - offset.min(self.len())) as u32 }
    }

    fn base(&self) -> *mut u8 {
        (self.client.scratch.addr + self.first * PAGE) as *mut u8
    }

    /// Copies `data` in at `offset` (as much as fits).
    pub fn put(&self, offset: u64, data: &[u8]) {
        let n = (data.len() as u64).min(self.len().saturating_sub(offset)) as usize;
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.base().add(offset as usize), n) };
    }

    /// A copy of `len` bytes at `offset` (as many as there are).
    pub fn get(&self, offset: u64, len: u64) -> Vec<u8> {
        let n = len.min(self.len().saturating_sub(offset)) as usize;
        let mut out = alloc::vec![0u8; n];
        unsafe { core::ptr::copy_nonoverlapping(self.base().add(offset as usize), out.as_mut_ptr(), n) };
        out
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        let area = &self.client.scratch;
        let mask = if self.pages == 64 { u64::MAX } else { (1u64 << self.pages) - 1 };
        *area.used.lock() &= !(mask << self.first);
        area.freed.fetch_add(1, Ordering::Release);
        futex_wake(&area.freed, u32::MAX as u64);
    }
}

/// What `run` asks its source for next.
pub enum Next<T> {
    Request(Desc, T),
    /// Nothing until a request in flight completed (a budget is used up).
    Later,
    Done,
}

/// Runs a stream of requests with as many in flight as slots allow: `next`
/// gives the next request and its context, `done` gets each completion (a
/// failed submission comes as its status, -EPIPE) and may return a
/// follow-up request. The thread never waits for a slot while it has
/// requests in flight: it completes its oldest first.
pub fn run<T>(client: &Client, mut next: impl FnMut() -> Next<T>, mut done: impl FnMut(T, Completion) -> Option<(Desc, T)>) {
    let mut in_flight: VecDeque<(Ticket, T)> = VecDeque::new();
    let mut follow: VecDeque<(Desc, T)> = VecDeque::new();
    let mut exhausted = false;
    loop {
        let item = match follow.pop_front() {
            Some(item) => Some(item),
            None if !exhausted => match next() {
                Next::Request(d, ctx) => Some((d, ctx)),
                Next::Later if !in_flight.is_empty() => None,
                Next::Later | Next::Done => {
                    exhausted = true;
                    None
                }
            },
            None => None,
        };
        if let Some((d, ctx)) = item {
            match client.submit(d, in_flight.is_empty()) {
                Ok(Some(t)) => {
                    in_flight.push_back((t, ctx));
                    continue;
                }
                // No slot: one of ours completes first.
                Ok(None) => follow.push_front((d, ctx)),
                Err(e) => {
                    let failed = Completion { tag: 0, op: d.op, status: -e, values: [0; 4] };
                    follow.extend(done(ctx, failed));
                    continue;
                }
            }
        }
        let Some((t, ctx)) = in_flight.pop_front() else {
            if follow.is_empty() && exhausted {
                return;
            }
            continue;
        };
        let c = client.wait(t);
        follow.extend(done(ctx, c));
    }
}
