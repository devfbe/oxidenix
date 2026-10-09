//! The server's end of a channel to a service (docs/design/io-rings.md):
//! request slots and the reaper, for any protocol whose completions are
//! `ring::Completion`s (`fsclient` to diskfs, `netclient` to netd). One
//! channel serves every thread of the instance's processes and its service
//! threads.
//!
//! **Slots.** Each request in flight has a slot (`N` of them, as many as
//! the rings hold): the request's tag names it (its index and a count of
//! the slot's uses), and the slot receives the completion. So at most `N`
//! requests are in flight: the submission ring always has room, and so
//! does the completion ring for every request the service takes. A thread
//! that holds no slot waits for one; one that holds some never does (`run`
//! completes one of its own first), so slots always come free.
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
//! completions while requests wait in the submission ring it rings the
//! service's (fsring, "Room").
//!
//! **A hostile or dead service.** A completion whose tag names no slot in
//! flight, or whose operation is not the slot's, is dropped. When the
//! service goes (the channel's `state`), or a request is not answered
//! within `REQUEST_TIMEOUT` (the services answer in milliseconds: one that
//! takes a minute hangs or withholds), every request in flight fails with
//! EIO and the client is dead: its owner makes a new one (the service is
//! started again if it died). So no wait for a service is unbounded.
//! Callers check every status and value they use.
//!
//! A thread that is being killed cannot sleep in the kernel: it polls until
//! its requests completed (requests are short), so no slot, grant or
//! pending page is ever abandoned.

use crate::sync::Mutex;
use crate::syscall;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use restricted::*;
use ring::channel::{Header, Layout};
use ring::{Completion, Consumer, Desc, Producer, Ring, RingMemory, Wait};

/// How long a request may stay unanswered before the client gives the
/// service up (generous: an emulated disk under load is slow, not this
/// slow).
const REQUEST_TIMEOUT: u64 = 60_000_000_000;

pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const EPIPE: i64 = 32;

/// A slot's states. A completion moves it from BUSY to DONE through
/// COMPLETING, which only one completer gets (the reaper, or a submitter
/// whose request could not be sent).
const FREE: u32 = 0;
const BUSY: u32 = 1;
const COMPLETING: u32 = 2;
const DONE: u32 = 3;

/// Sleeps while `word` holds `value` (a server futex: on a ring word the
/// channel's object, else the server's own memory). A thread being killed
/// cannot sleep: it yields instead, and its caller polls.
pub fn futex_wait(word: &AtomicU32, value: u32) {
    let r = syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, value as u64, 0, 0, 0, 0]);
    if r == -EINTR {
        syscall(SYS_YIELD, [0; 6]);
    }
}

pub fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64
}

pub fn futex_wake(word: &AtomicU32, n: u64) {
    syscall(SYS_SERVER_FUTEX_WAKE, [word as *const AtomicU32 as u64, n, 0, 0, 0, 0]);
}

/// The rings' doorbells: one sleeper on the word.
pub struct Doorbell;

impl Wait for Doorbell {
    fn wait(&self, word: &AtomicU32, value: u32) {
        futex_wait(word, value);
    }

    fn wake(&self, word: &AtomicU32) {
        futex_wake(word, 1);
    }
}

/// The completion ring's doorbell, slept on until `deadline` at most.
struct TimedDoorbell(u64);

impl Wait for TimedDoorbell {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let r = syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, value as u64, self.0, 0, 0, 0]);
        if r == -EINTR {
            syscall(SYS_YIELD, [0; 6]);
        }
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
    /// When the request was sent (its deadline is `REQUEST_TIMEOUT` later).
    since: AtomicU64,
    /// Written by the completer (COMPLETING) before `state` becomes DONE.
    result: UnsafeCell<Completion>,
}

// `result` is written only by whoever moved the slot to COMPLETING and read
// by its owner only once it is DONE (SeqCst on `state`).
unsafe impl Sync for Slot {}

/// A request in flight: whoever submitted it must `wait` for it.
#[must_use]
pub struct Ticket(usize);

pub struct RingClient<const N: usize> {
    handle: u64,
    header: &'static Header,
    /// The channel's mapping in the server's region, and its layout.
    base: u64,
    layout: Layout,
    requests: Mutex<Producer<'static, N>>,
    /// The submission ring itself, for doorbells without the lock.
    submission: &'static RingMemory<N>,
    /// Used only by the thread that holds `reaping`.
    completions: UnsafeCell<Consumer<'static, N>>,
    reaping: AtomicBool,
    slots: Vec<Slot>,
    free: Mutex<Vec<usize>>,
    /// Advanced when a slot comes free (threads waiting for one sleep on
    /// it).
    freed: AtomicU32,
    dead: AtomicBool,
}

// The consumer is touched only by the reaper (the `reaping` flag); the
// rest is atomics and locks.
unsafe impl<const N: usize> Sync for RingClient<N> {}
unsafe impl<const N: usize> Send for RingClient<N> {}

impl<const N: usize> RingClient<N> {
    /// A tag names its slot in its low 8 bits.
    const VALID: () = assert!(N <= 256);

    /// A new channel of `N` slots and `shared` pages of shared area,
    /// connected to `service` (started again if it died).
    pub fn connect(service: &str, shared: u32) -> Result<RingClient<N>, i64> {
        let () = Self::VALID;
        let layout = Layout::with_shared(N as u32, shared).ok_or(crate::files::EINVAL)?;
        let mut addr = 0u64;
        let h = syscall(SYS_CHAN_CREATE, [N as u64, &mut addr as *mut u64 as u64, shared as u64, 0, 0, 0]);
        if h < 0 {
            return Err(-h);
        }
        let handle = h as u64;
        let base = addr as *const u8;
        // Mapped until the handle is closed (`drop`).
        let (sub, comp) = unsafe { (layout.ring::<N>(base, layout.submission), layout.ring::<N>(base, layout.completion)) };
        let r = syscall(SYS_CHAN_CONNECT, [handle, service.as_ptr() as u64, service.len() as u64, 0, 0, 0]);
        if r < 0 {
            syscall(SYS_HANDLE_CLOSE, [handle, 0, 0, 0, 0, 0]);
            return Err(-r);
        }
        let slots = (0..N)
            .map(|_| Slot {
                word: AtomicU32::new(0),
                state: AtomicU32::new(FREE),
                sleeping: AtomicBool::new(false),
                tag: AtomicU64::new(0),
                op: AtomicU32::new(0),
                uses: AtomicU64::new(0),
                since: AtomicU64::new(0),
                result: UnsafeCell::new(Completion::default()),
            })
            .collect();
        Ok(RingClient {
            handle,
            header: unsafe { Header::at(base) },
            base: addr,
            layout,
            requests: Mutex::new(Ring::new(sub).producer()),
            submission: sub,
            completions: UnsafeCell::new(Ring::new(comp).consumer()),
            reaping: AtomicBool::new(false),
            slots,
            free: Mutex::new((0..N).rev().collect()),
            freed: AtomicU32::new(0),
            dead: AtomicBool::new(false),
        })
    }

    /// Whether the service is gone (or misbehaved): the client takes no
    /// new requests, a new one must be made.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire) || self.header.state() != 0
    }

    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// The channel's shared area (mapped while the client lives).
    pub fn shared(&self) -> *mut u8 {
        (self.base + self.layout.shared as u64) as *mut u8
    }

    /// Rings the service's doorbell without a request (work the protocol
    /// marked elsewhere: `RingMemory::ring_doorbell`).
    pub fn doorbell(&self) {
        self.submission.ring_doorbell(&Doorbell);
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
        if self.is_dead() {
            // Died while this thread waited for the slot.
            self.release(i);
            return Err(EPIPE);
        }
        let s = &self.slots[i];
        s.since.store(now(), Ordering::Relaxed);
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
            // Never with as many slots as the ring has: the service did not
            // take what it completed. It is not to be trusted any more.
            self.dead.store(true, Ordering::Release);
            self.complete(i, Completion { tag: d.tag, op: d.op, status: -EIO, values: [0; 4] });
        }
        Ok(Some(Ticket(i)))
    }

    /// The completion of `t` (status -EIO if the service went first).
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

    /// Takes completions (as the reaper) until slot `i`'s arrived, or its
    /// deadline passed (then the service is given up).
    fn reap_until(&self, i: usize) {
        let completions = unsafe { &mut *self.completions.get() };
        let header = self.header;
        let deadline = self.slots[i].since.load(Ordering::Relaxed).saturating_add(REQUEST_TIMEOUT);
        let live = || header.state() == 0 && !self.dead.load(Ordering::Acquire) && now() < deadline;
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
            if !live() {
                // (Checked here too: a flood of forged completions must not
                // keep the reaper from its deadline.)
                self.fail_all();
                return;
            }
            match completions.pop_wait_while(&TimedDoorbell(deadline), live) {
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
        if s.state.compare_exchange(BUSY, COMPLETING, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        unsafe { *s.result.get() = c };
        s.state.store(DONE, Ordering::SeqCst);
        s.word.fetch_add(1, Ordering::SeqCst);
        if s.sleeping.load(Ordering::SeqCst) {
            futex_wake(&s.word, 1);
        }
    }

    /// The service is gone, or hangs: every request in flight fails.
    fn fail_all(&self) {
        self.dead.store(true, Ordering::Release);
        for (i, s) in self.slots.iter().enumerate() {
            if s.state.load(Ordering::SeqCst) == BUSY {
                let (tag, op) = (s.tag.load(Ordering::Relaxed), s.op.load(Ordering::Relaxed) as u16);
                self.complete(i, Completion { tag, op, status: -EIO, values: [0; 4] });
            }
        }
    }
}

impl<const N: usize> Drop for RingClient<N> {
    fn drop(&mut self) {
        // The channel's end goes: every grant is revoked, the service lets
        // go of what this client held.
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
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
pub fn run<T, const N: usize>(client: &RingClient<N>, mut next: impl FnMut() -> Next<T>, mut done: impl FnMut(T, Completion) -> Option<(Desc, T)>) {
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
