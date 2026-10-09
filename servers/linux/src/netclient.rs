//! The instance's channel to netd (phase R7b, ADR 0007): the net thread
//! (`ROLE_NET`) that turns netd's changes into readiness reports. Until the
//! sockets come, the thread only waits.

use crate::syscall;
use core::sync::atomic::AtomicU32;
use restricted::*;

/// Nothing ever wakes it yet.
static IDLE: AtomicU32 = AtomicU32::new(0);

/// The net thread.
pub fn thread() -> ! {
    loop {
        syscall(SYS_SERVER_FUTEX_WAIT, [&IDLE as *const AtomicU32 as u64, 0, 0, 0, 0, 0]);
    }
}
