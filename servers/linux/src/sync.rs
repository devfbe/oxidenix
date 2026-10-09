//! A mutex for the server's data, shared by every thread of the instance
//! (all the tree's processes run the server in the same shared region).
//!
//! The lock word follows Drepper's "Futexes Are Tricky" (mutex 2): 0 free,
//! 1 locked, 2 locked with (possible) waiters. Lock: compare-exchange 0 to
//! 1; if that fails, swap in 2 and sleep on the word while it is 2, until a
//! swap returns 0. Unlock: swap in 0; if it was 2, wake one waiter. The
//! kernel's server futex (`SYS_SERVER_FUTEX_WAIT`) compares the word under
//! its bucket lock before sleeping, so no wakeup is lost. Acquire on lock
//! and Release on unlock order the protected data.
//!
//! Waits are not interruptible by the program's signals (the server holds
//! these locks for short, bounded work, like a kernel's spinlocks), but a
//! dying thread stops waiting and never returns to the server.
//!
//! Priority: the server runs on its programs' threads, with their nice
//! values. A low-priority thread preempted while it holds a lock would
//! hold up every thread of the instance that needs it (priority
//! inversion). So each thread counts the locks it holds in a word of its
//! State page (`restricted::SERVER_LOCKS_OFFSET`, `held`), and the kernel's
//! scheduler runs a holder with the weight of nice -20: preempted in the
//! lock, it is back as soon as the most favored program would be (and a
//! program gains no more than that by its calls).
//!
//! The count changes by a plain load and store (`hold`, `unhold`), not by
//! a locked read-modify-write: only its own thread writes it. Locked
//! instructions cost tens of cycles each, twice per lock, and a path
//! lookup takes about fifty locks (most of them the heap's): counting
//! with them made `stat` of a tmpfs path about 15 % slower
//! (docs/benchmarks/README.md).

use crate::syscall;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{compiler_fence, AtomicU32, Ordering};
use restricted::{SERVER_LOCKS_OFFSET, SYS_SERVER_FUTEX_WAIT, SYS_SERVER_FUTEX_WAKE, SYS_YIELD, THREADS_BASE, THREAD_AREA};

/// The calling thread's count of the locks it holds: in its State page,
/// found from the stack, which is the thread's area's
/// (`restricted::thread_stack_top`).
fn held() -> &'static AtomicU32 {
    let sp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags)) };
    let n = (sp - THREADS_BASE) / THREAD_AREA;
    unsafe { &*((restricted::thread_state(n) + SERVER_LOCKS_OFFSET) as *const AtomicU32) }
}

/// The calling thread took a lock: one more in its count. The thread is
/// the count's only writer (the kernel zeroes it when it hands out the
/// slot, before the thread runs, and only reads it after), so a load and a
/// store make an exact count. The kernel reads it when it schedules: on
/// this CPU, an interrupt between the two sees the count as it was an
/// instruction earlier; another CPU sees either value. Both are a valid
/// moment of the thread's. The compiler fence keeps the store ahead of the
/// critical section (the lock's Acquire keeps it behind the lock).
#[inline(always)]
fn hold() {
    let h = held();
    h.store(h.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    compiler_fence(Ordering::SeqCst);
}

/// The calling thread let go of a lock (after the release, which the
/// compiler fence keeps ahead of the store): one fewer in its count.
#[inline(always)]
fn unhold() {
    compiler_fence(Ordering::SeqCst);
    let h = held();
    h.store(h.load(Ordering::Relaxed).wrapping_sub(1), Ordering::Relaxed);
}

pub struct Mutex<T> {
    state: AtomicU32,
    data: UnsafeCell<T>,
}

// The lock serializes access to `data`.
unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(data: T) -> Self {
        Mutex { state: AtomicU32::new(0), data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.state.swap(2, Ordering::Acquire) != 0 {
                let addr = &self.state as *const AtomicU32 as u64;
                syscall(SYS_SERVER_FUTEX_WAIT, [addr, 2, 0, 0, 0, 0]);
            }
        }
        hold();
        MutexGuard { mutex: self }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.mutex.state.swap(0, Ordering::Release) == 2 {
            let addr = &self.mutex.state as *const AtomicU32 as u64;
            syscall(SYS_SERVER_FUTEX_WAKE, [addr, 1, 0, 0, 0, 0]);
        }
        unhold();
    }
}

/// A reader-writer lock without data: any number of readers or one writer.
/// Its state is a `Mutex`'s (readers, writer, writers waiting); a waiter
/// sleeps on `changed`, which every unlock that may let someone in
/// advances (read under the state lock before sleeping, so no wakeup is
/// lost). Once a writer waits, new readers wait too.
pub struct RwLock {
    state: Mutex<RwState>,
    changed: AtomicU32,
}

struct RwState {
    readers: u32,
    writer: bool,
    writers_waiting: u32,
}

impl RwLock {
    pub const fn new() -> Self {
        RwLock { state: Mutex::new(RwState { readers: 0, writer: false, writers_waiting: 0 }), changed: AtomicU32::new(0) }
    }

    pub fn read(&self) -> ReadGuard<'_> {
        loop {
            let seen = {
                let mut st = self.state.lock();
                if !st.writer && st.writers_waiting == 0 {
                    st.readers += 1;
                    hold();
                    return ReadGuard { lock: self };
                }
                self.changed.load(Ordering::Acquire)
            };
            self.sleep(seen);
        }
    }

    pub fn write(&self) -> WriteGuard<'_> {
        let mut waiting = false;
        loop {
            let seen = {
                let mut st = self.state.lock();
                if !st.writer && st.readers == 0 {
                    st.writer = true;
                    if waiting {
                        st.writers_waiting -= 1;
                    }
                    hold();
                    return WriteGuard { lock: self };
                }
                if !waiting {
                    st.writers_waiting += 1;
                    waiting = true;
                }
                self.changed.load(Ordering::Acquire)
            };
            self.sleep(seen);
        }
    }

    fn sleep(&self, seen: u32) {
        let addr = &self.changed as *const AtomicU32 as u64;
        const EINTR: i64 = 4;
        // A dying thread cannot sleep: it yields while it waits.
        if syscall(SYS_SERVER_FUTEX_WAIT, [addr, seen as u64, 0, 0, 0, 0]) == -EINTR {
            syscall(SYS_YIELD, [0; 6]);
        }
    }

    fn advance(&self) {
        self.changed.fetch_add(1, Ordering::Release);
        let addr = &self.changed as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [addr, u32::MAX as u64, 0, 0, 0, 0]);
    }
}

pub struct ReadGuard<'a> {
    lock: &'a RwLock,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let wake = {
            let mut st = self.lock.state.lock();
            st.readers -= 1;
            st.readers == 0 && st.writers_waiting > 0
        };
        if wake {
            self.lock.advance();
        }
        unhold();
    }
}

pub struct WriteGuard<'a> {
    lock: &'a RwLock,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        self.lock.state.lock().writer = false;
        self.lock.advance();
        unhold();
    }
}
