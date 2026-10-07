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

use crate::syscall;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::{SYS_SERVER_FUTEX_WAIT, SYS_SERVER_FUTEX_WAKE};

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
    }
}
