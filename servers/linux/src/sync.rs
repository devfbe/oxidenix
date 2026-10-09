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
//! **The rule.** A `Mutex` or `RwLock` is held for bounded work only: never
//! across a copy of program memory (a page fault may wait for the pager and
//! for diskfs), a wait for another party (diskfs, netd, procfs, a channel's
//! offer, a page, a pipe's or socket's peer), or a call that takes a lock
//! held across one. Their waits are like a kernel's spinlock or plain mutex:
//! not interruptible by the program's signals, nor ended by the thread's
//! death (`FUTEX_LOCK`); a dying thread still runs server code that takes
//! them (`process::exit_killed` lets go of its descriptor table and ends the
//! thread), and their holders let go in bounded time. A lock that is held
//! across such waits is a `SleepMutex` (`SleepLock`) or `SleepRwLock`
//! (Linux's mutex_lock_killable): its wait ends with EINTR when the waiting
//! thread dies and the caller unwinds; its holders are not priority-boosted.
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
use restricted::{FUTEX_LOCK, SERVER_LOCKS_OFFSET, SYS_SERVER_FUTEX_WAIT, SYS_SERVER_FUTEX_WAKE, THREADS_BASE, THREAD_AREA};

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

const EINTR: i64 = 4;

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
                // (Ends only when woken: bounded work holds it.)
                syscall(SYS_SERVER_FUTEX_WAIT, [addr, 2, 0, FUTEX_LOCK, 0, 0]);
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

/// A sleeping lock with data, held across waits for other parties and copies of program
/// memory (Linux's mutex_lock_killable): its wait ends with EINTR when the waiting thread
/// dies, and the caller unwinds (`?`). Not counted among the thread's held locks: a holder
/// is not priority-boosted (it may hold it for as long as a page or a service takes).
pub struct SleepMutex<T> {
    state: AtomicU32,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SleepMutex<T> {}
unsafe impl<T: Send> Send for SleepMutex<T> {}

impl<T> SleepMutex<T> {
    pub const fn new(data: T) -> Self {
        SleepMutex { state: AtomicU32::new(0), data: UnsafeCell::new(data) }
    }

    /// Takes the lock; EINTR if the thread dies while it waits.
    pub fn lock(&self) -> Result<SleepMutexGuard<'_, T>, i64> {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.state.swap(2, Ordering::Acquire) != 0 {
                let addr = &self.state as *const AtomicU32 as u64;
                if syscall(SYS_SERVER_FUTEX_WAIT, [addr, 2, 0, 0, 0, 0]) == -EINTR {
                    // (Others may still wait: the word stays 2, so the holder wakes one.)
                    return Err(EINTR);
                }
            }
        }
        Ok(SleepMutexGuard { mutex: self })
    }
}

pub struct SleepMutexGuard<'a, T> {
    mutex: &'a SleepMutex<T>,
}

impl<T> Deref for SleepMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for SleepMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for SleepMutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.mutex.state.swap(0, Ordering::Release) == 2 {
            let addr = &self.mutex.state as *const AtomicU32 as u64;
            syscall(SYS_SERVER_FUTEX_WAKE, [addr, 1, 0, 0, 0, 0]);
        }
    }
}

/// A `SleepMutex` without data (a socket's or a pipe's readers and writers, as Linux's
/// iolock and pipe mutex).
pub type SleepLock = SleepMutex<()>;

/// The state of a reader-writer lock: any number of readers or one writer, under a
/// `Mutex`; a waiter sleeps on `changed`, which every unlock that may let someone in
/// advances (read under the state lock before sleeping, so no wakeup is lost). Once a
/// writer waits, new readers wait too.
struct RwCore {
    state: Mutex<RwState>,
    changed: AtomicU32,
}

struct RwState {
    readers: u32,
    writer: bool,
    writers_waiting: u32,
}

impl RwCore {
    const fn new() -> Self {
        RwCore { state: Mutex::new(RwState { readers: 0, writer: false, writers_waiting: 0 }), changed: AtomicU32::new(0) }
    }

    /// Waits for a share; with `killable`, EINTR if the thread dies meanwhile.
    fn read(&self, killable: bool) -> Result<(), i64> {
        loop {
            let seen = {
                let mut st = self.state.lock();
                if !st.writer && st.writers_waiting == 0 {
                    st.readers += 1;
                    return Ok(());
                }
                self.changed.load(Ordering::Acquire)
            };
            self.sleep(seen, killable)?;
        }
    }

    /// Waits for the lock alone; with `killable`, EINTR if the thread dies meanwhile (its
    /// place among the waiting writers given up, so readers it held off go on).
    fn write(&self, killable: bool) -> Result<(), i64> {
        let mut waiting = false;
        loop {
            let seen = {
                let mut st = self.state.lock();
                if !st.writer && st.readers == 0 {
                    st.writer = true;
                    if waiting {
                        st.writers_waiting -= 1;
                    }
                    return Ok(());
                }
                if !waiting {
                    st.writers_waiting += 1;
                    waiting = true;
                }
                self.changed.load(Ordering::Acquire)
            };
            if let Err(e) = self.sleep(seen, killable) {
                self.state.lock().writers_waiting -= 1;
                self.advance();
                return Err(e);
            }
        }
    }

    fn sleep(&self, seen: u32, killable: bool) -> Result<(), i64> {
        let addr = &self.changed as *const AtomicU32 as u64;
        let flags = if killable { 0 } else { FUTEX_LOCK };
        if syscall(SYS_SERVER_FUTEX_WAIT, [addr, seen as u64, 0, flags, 0, 0]) == -EINTR && killable {
            return Err(EINTR);
        }
        Ok(())
    }

    fn advance(&self) {
        self.changed.fetch_add(1, Ordering::Release);
        let addr = &self.changed as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [addr, u32::MAX as u64, 0, 0, 0, 0]);
    }

    fn read_done(&self) {
        let wake = {
            let mut st = self.state.lock();
            st.readers -= 1;
            st.readers == 0 && st.writers_waiting > 0
        };
        if wake {
            self.advance();
        }
    }

    fn write_done(&self) {
        self.state.lock().writer = false;
        self.advance();
    }
}

/// A reader-writer lock without data for bounded work (as `Mutex`).
pub struct RwLock {
    core: RwCore,
}

impl RwLock {
    pub const fn new() -> Self {
        RwLock { core: RwCore::new() }
    }

    pub fn read(&self) -> ReadGuard<'_> {
        let _ = self.core.read(false);
        hold();
        ReadGuard { lock: self }
    }

    pub fn write(&self) -> WriteGuard<'_> {
        let _ = self.core.write(false);
        hold();
        WriteGuard { lock: self }
    }
}

pub struct ReadGuard<'a> {
    lock: &'a RwLock,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        self.lock.core.read_done();
        unhold();
    }
}

pub struct WriteGuard<'a> {
    lock: &'a RwLock,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        self.lock.core.write_done();
        unhold();
    }
}

/// A reader-writer lock without data held across waits for other parties (as
/// `SleepMutex`): its waits end with EINTR when the waiting thread dies.
pub struct SleepRwLock {
    core: RwCore,
}

impl SleepRwLock {
    pub const fn new() -> Self {
        SleepRwLock { core: RwCore::new() }
    }

    pub fn read(&self) -> Result<SleepReadGuard<'_>, i64> {
        self.core.read(true)?;
        Ok(SleepReadGuard { lock: self })
    }

    pub fn write(&self) -> Result<SleepWriteGuard<'_>, i64> {
        self.core.write(true)?;
        Ok(SleepWriteGuard { lock: self })
    }
}

pub struct SleepReadGuard<'a> {
    lock: &'a SleepRwLock,
}

impl Drop for SleepReadGuard<'_> {
    fn drop(&mut self) {
        self.lock.core.read_done();
    }
}

pub struct SleepWriteGuard<'a> {
    lock: &'a SleepRwLock,
}

impl Drop for SleepWriteGuard<'_> {
    fn drop(&mut self) {
        self.lock.core.write_done();
    }
}
