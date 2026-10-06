//! Kernel locking primitives that are correct on several CPUs.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};
use x86_64::instructions::interrupts;

/// A ticket spinlock that disables interrupts while held.
///
/// Tickets make it fair: CPUs get the lock in the order they asked for it,
/// so none can starve. Disabling interrupts makes it safe to take the same
/// lock in an interrupt handler: the handler can never spin on a lock that
/// the code it interrupted holds. The previous interrupt state is restored
/// when the guard is dropped, so guards nest.
pub struct IrqSpinLock<T: ?Sized> {
    next: AtomicU32,
    serving: AtomicU32,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Sync for IrqSpinLock<T> {}
unsafe impl<T: ?Sized + Send> Send for IrqSpinLock<T> {}

pub struct IrqSpinLockGuard<'a, T: ?Sized> {
    lock: &'a IrqSpinLock<T>,
    interrupts_were_enabled: bool,
}

/// Spins this long before declaring a deadlock (debug builds only); far
/// beyond any legitimate critical section, even under emulation.
#[cfg(debug_assertions)]
const DEADLOCK_SPINS: u64 = 1 << 34;

impl<T> IrqSpinLock<T> {
    pub const fn new(value: T) -> Self {
        IrqSpinLock { next: AtomicU32::new(0), serving: AtomicU32::new(0), data: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> IrqSpinLock<T> {
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        let interrupts_were_enabled = interrupts::are_enabled();
        interrupts::disable();
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        #[cfg(debug_assertions)]
        let mut spins = 0u64;
        while self.serving.load(Ordering::Acquire) != ticket {
            spin_loop();
            #[cfg(debug_assertions)]
            {
                spins += 1;
                if spins == DEADLOCK_SPINS {
                    panic!("deadlock: spinlock at {:p} not released", self);
                }
            }
        }
        IrqSpinLockGuard { lock: self, interrupts_were_enabled }
    }

    /// The lock if it is free right now.
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T>> {
        let interrupts_were_enabled = interrupts::are_enabled();
        interrupts::disable();
        let serving = self.serving.load(Ordering::Relaxed);
        let taken = self
            .next
            .compare_exchange(serving, serving.wrapping_add(1), Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if taken {
            return Some(IrqSpinLockGuard { lock: self, interrupts_were_enabled });
        }
        if interrupts_were_enabled {
            interrupts::enable();
        }
        None
    }

    /// Whether some CPU holds the lock (for assertions).
    pub fn is_locked(&self) -> bool {
        self.next.load(Ordering::Relaxed) != self.serving.load(Ordering::Relaxed)
    }
}

impl<T: ?Sized> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.serving.fetch_add(1, Ordering::Release);
        if self.interrupts_were_enabled {
            interrupts::enable();
        }
    }
}
