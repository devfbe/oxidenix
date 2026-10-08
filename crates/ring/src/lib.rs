//! Single-producer single-consumer rings of fixed-size descriptors in
//! shared memory: the data plane between the Linux server and the device
//! servers (docs/design/io-rings.md, ADR 0005).
//!
//! `RingMemory` is the layout both ends map; `Producer` and `Consumer` are
//! each end's view. Invariants and memory ordering (tested in
//! `tests/spsc.rs`):
//!
//! 1. Only the producer writes `tail` and the free slots; only the consumer
//!    writes `head`. Positions run freely modulo 2^32; `tail - head` is the
//!    fill level, at most `N`.
//! 2. Publication: the producer writes a slot, then stores `tail + 1` with
//!    Release; the consumer loads `tail` with Acquire before reading the
//!    slots below it, so it sees them and everything written before them
//!    (data in a granted buffer).
//! 3. Recycling: the consumer copies a slot out, then stores `head + 1`
//!    with Release; the producer loads `head` with Acquire before it reuses
//!    a slot, so the copy happened before the overwrite.
//! 4. Doorbell: only the consumer writes `sleeping`. Finding the ring
//!    empty, it stores `sleeping = 1`, fences (SeqCst), loads `tail` again
//!    and sleeps on `tail` only if it is unchanged; it stores
//!    `sleeping = 0` once it has an entry again. A producer stores `tail`,
//!    fences (SeqCst) and loads `sleeping`; if set, it wakes the consumer.
//!    The fences order each side's store before its load, so either the
//!    consumer sees the new tail or the producer sees `sleeping`: no wakeup
//!    is lost. (A producer that cleared `sleeping` itself could erase the
//!    flag of the consumer's next sleep, whose wakeup would then be lost.)
//! 5. The peer may be hostile: each end keeps its own position privately
//!    (it never reads it back from shared memory), indexes slots modulo
//!    `N`, and copies each descriptor out once (no double fetch). A wrong
//!    peer position yields wrong entries, never an access outside the ring.

#![no_std]

use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicU32, Ordering};

/// A request or a completion (64 bytes; the fields' meaning is the
/// protocol's).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Desc {
    pub op: u16,
    pub flags: u16,
    pub len: u32,
    /// The client's: a completion echoes its request's.
    pub tag: u64,
    pub object: u64,
    pub offset: u64,
    pub grant: u32,
    pub buf_off: u32,
    pub arg: [u64; 3],
}

const _: () = assert!(core::mem::size_of::<Desc>() == 64);

/// A word on a cache line of its own (the two ends write different ones).
#[repr(C, align(64))]
struct Line(AtomicU32);

/// The shared layout of one ring of `N` slots (`N` a power of two).
#[repr(C)]
pub struct RingMemory<const N: usize> {
    head: Line,
    tail: Line,
    sleeping: Line,
    slots: [UnsafeCell<Desc>; N],
}

// The slots are shared by the protocol above: a slot is written only by the
// producer while it is free and read only by the consumer while it is full.
unsafe impl<const N: usize> Sync for RingMemory<N> {}

impl<const N: usize> RingMemory<N> {
    const VALID: () = assert!(N.is_power_of_two() && N <= 1 << 16);

    pub fn new() -> Self {
        let () = Self::VALID;
        RingMemory {
            head: Line(AtomicU32::new(0)),
            tail: Line(AtomicU32::new(0)),
            sleeping: Line(AtomicU32::new(0)),
            slots: [const { UnsafeCell::new(Desc { op: 0, flags: 0, len: 0, tag: 0, object: 0, offset: 0, grant: 0, buf_off: 0, arg: [0; 3] }) }; N],
        }
    }

    /// Sets both positions (an empty ring), before either end uses it.
    pub fn set_positions(&self, at: u32) {
        self.head.0.store(at, Ordering::Relaxed);
        self.tail.0.store(at, Ordering::Relaxed);
    }

    /// The ring at `ptr` (in a mapping both ends share).
    ///
    /// # Safety
    /// `ptr` is valid and aligned for `RingMemory<N>` for `'a`. (Any bit
    /// pattern is a valid ring: a hostile peer cannot make it unsound.)
    pub unsafe fn from_ptr<'a>(ptr: *const Self) -> &'a Self {
        let () = Self::VALID;
        unsafe { &*ptr }
    }

    fn slot(&self, pos: u32) -> *mut Desc {
        self.slots[pos as usize & (N - 1)].get()
    }
}

impl<const N: usize> Default for RingMemory<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Sleeping on a word of the ring and waking who sleeps there (a futex).
pub trait Wait {
    /// Sleeps while `word` holds `value` (may return early).
    fn wait(&self, word: &AtomicU32, value: u32);
    fn wake(&self, word: &AtomicU32);
}

/// Either end's view of a ring.
pub struct Ring<'a, const N: usize> {
    mem: &'a RingMemory<N>,
}

impl<'a, const N: usize> Ring<'a, N> {
    pub fn new(mem: &'a RingMemory<N>) -> Self {
        Ring { mem }
    }

    /// The producing end (one per ring).
    pub fn producer(&self) -> Producer<'a, N> {
        let tail = self.mem.tail.0.load(Ordering::Relaxed);
        Producer { mem: self.mem, tail, head: self.mem.head.0.load(Ordering::Acquire) }
    }

    /// The consuming end (one per ring).
    pub fn consumer(&self) -> Consumer<'a, N> {
        let head = self.mem.head.0.load(Ordering::Relaxed);
        Consumer { mem: self.mem, head, tail: self.mem.tail.0.load(Ordering::Acquire) }
    }
}

pub struct Producer<'a, const N: usize> {
    mem: &'a RingMemory<N>,
    /// Ours: never read back from shared memory.
    tail: u32,
    /// The consumer's position as last seen.
    head: u32,
}

impl<const N: usize> Producer<'_, N> {
    /// Free slots now.
    pub fn room(&mut self) -> usize {
        self.head = self.mem.head.0.load(Ordering::Acquire);
        N - (self.tail.wrapping_sub(self.head) as usize).min(N)
    }

    /// Publishes `d`; false if the ring is full.
    pub fn push(&mut self, d: &Desc) -> bool {
        if self.tail.wrapping_sub(self.head) as usize >= N && self.room() == 0 {
            return false;
        }
        unsafe { self.mem.slot(self.tail).write_volatile(*d) };
        self.tail = self.tail.wrapping_add(1);
        self.mem.tail.0.store(self.tail, Ordering::Release);
        true
    }

    /// Wakes the consumer if it sleeps (after one or more pushes).
    pub fn ring_doorbell(&mut self, w: &impl Wait) {
        fence(Ordering::SeqCst);
        if self.mem.sleeping.0.load(Ordering::Relaxed) != 0 {
            w.wake(&self.mem.tail.0);
        }
    }
}

pub struct Consumer<'a, const N: usize> {
    mem: &'a RingMemory<N>,
    /// Ours: never read back from shared memory.
    head: u32,
    /// The producer's position as last seen.
    tail: u32,
}

impl<const N: usize> Consumer<'_, N> {
    /// The next entry, copied out, if there is one.
    pub fn pop(&mut self) -> Option<Desc> {
        if self.head == self.tail {
            self.tail = self.mem.tail.0.load(Ordering::Acquire);
            if self.head == self.tail {
                return None;
            }
        }
        let d = unsafe { self.mem.slot(self.head).read_volatile() };
        self.head = self.head.wrapping_add(1);
        self.mem.head.0.store(self.head, Ordering::Release);
        Some(d)
    }

    /// The next entry, sleeping until there is one.
    pub fn pop_wait(&mut self, w: &impl Wait) -> Desc {
        loop {
            if let Some(d) = self.pop_wait_while(w, || true) {
                return d;
            }
        }
    }

    /// The next entry, sleeping until there is one while `live()` holds;
    /// None once it does not (the peer is gone). `live` is checked each
    /// time the ring is found empty, before sleeping: whatever ends the
    /// peer must also end the sleep (a channel's teardown fails the futex
    /// waits on its memory, see `channel`), so that no end sleeps forever.
    pub fn pop_wait_while(&mut self, w: &impl Wait, live: impl Fn() -> bool) -> Option<Desc> {
        let mut flagged = false;
        let result = loop {
            if let Some(d) = self.pop() {
                break Some(d);
            }
            if !live() {
                break None;
            }
            self.mem.sleeping.0.store(1, Ordering::Relaxed);
            flagged = true;
            fence(Ordering::SeqCst);
            let tail = self.mem.tail.0.load(Ordering::Relaxed);
            if tail == self.head {
                w.wait(&self.mem.tail.0, tail);
            }
        };
        if flagged {
            self.mem.sleeping.0.store(0, Ordering::Relaxed);
        }
        result
    }

    /// For a consumer that sleeps elsewhere (an event loop waiting for
    /// several rings and other events at once): announces the sleep as
    /// invariant 4 says (`sleeping = 1`, fence, `tail` again) and returns
    /// the tail value to sleep on, or None if entries came meanwhile (then
    /// it has woken already, `sleeping` is cleared). The caller sleeps only
    /// while the tail word still holds that value (a futex compares it) and
    /// calls `awake` once it runs again.
    pub fn prepare_sleep(&mut self) -> Option<u32> {
        if self.head != self.tail || self.mem.tail.0.load(Ordering::Acquire) != self.head {
            return None;
        }
        self.mem.sleeping.0.store(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let tail = self.mem.tail.0.load(Ordering::Relaxed);
        if tail != self.head {
            self.awake();
            return None;
        }
        Some(tail)
    }

    /// Ends a sleep announced by `prepare_sleep`: producers stop ringing
    /// the doorbell (the consumer polls while it is awake).
    pub fn awake(&mut self) {
        self.mem.sleeping.0.store(0, Ordering::Relaxed);
    }

    /// Whether the ring holds an entry (without taking it).
    pub fn is_empty(&mut self) -> bool {
        if self.head != self.tail {
            return false;
        }
        self.tail = self.mem.tail.0.load(Ordering::Acquire);
        self.head == self.tail
    }
}

/// Offsets of the words in a `RingMemory` (for the kernel, which wakes the
/// sleepers of a channel whose peer died).
pub const HEAD_OFFSET: usize = 0;
pub const TAIL_OFFSET: usize = 64;
pub const SLEEPING_OFFSET: usize = 128;
/// Bytes of a `RingMemory` before its slots.
pub const RING_HEADER: usize = 192;

const _: () = {
    assert!(core::mem::offset_of!(RingMemory<2>, head) == HEAD_OFFSET);
    assert!(core::mem::offset_of!(RingMemory<2>, tail) == TAIL_OFFSET);
    assert!(core::mem::offset_of!(RingMemory<2>, sleeping) == SLEEPING_OFFSET);
    assert!(core::mem::offset_of!(RingMemory<2>, slots) == RING_HEADER);
    assert!(core::mem::size_of::<RingMemory<8>>() == RING_HEADER + 8 * 64);
};

pub mod channel;
pub mod selftest;
