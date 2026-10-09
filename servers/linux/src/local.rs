//! The server's data for the thread it runs on (phase R8): a block in the thread's State page
//! (`restricted::SERVER_LOCAL_OFFSET`, which the kernel never touches), found from the stack
//! as the lock count is (`sync`). It holds what the hot paths need without the process lock:
//! the thread's tid and pid in the instance's namespace, its key, and its working-directory
//! record. Only the thread itself writes it (and its creator, before it runs).

use crate::records::FsContext;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use restricted::{thread_state, SERVER_LOCAL_OFFSET, THREADS_BASE, THREAD_AREA};

#[repr(C)]
pub struct Local {
    /// The thread's id and its process's (0 for a service thread).
    pub tid: AtomicU32,
    pub pid: AtomicU32,
    /// Flags (`RESTORE_MASK`).
    pub flags: AtomicU32,
    _pad: u32,
    /// The thread's key (`restricted::SYS_THREAD_CREATE`).
    pub key: AtomicU64,
    /// The thread's working-directory record: the reference its thread
    /// record (`process::Thread::fs`) holds, valid while the thread runs.
    pub fs: AtomicPtr<FsContext>,
}

/// A call set a temporary signal mask (sigsuspend, ppoll, pselect, epoll_pwait): it is put
/// back before the program runs again, or by the signal frame of the handler it let in.
pub const RESTORE_MASK: u32 = 1;

/// The thread area of the calling thread, from its stack (`restricted::thread_stack_top`).
pub fn slot() -> u64 {
    let sp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags)) };
    (sp - THREADS_BASE) / THREAD_AREA
}

/// The block of thread area `n`.
pub fn of_slot(n: u64) -> &'static Local {
    unsafe { &*((thread_state(n) + SERVER_LOCAL_OFFSET) as *const Local) }
}

/// The calling thread's block.
pub fn get() -> &'static Local {
    of_slot(slot())
}

/// The calling thread's id.
pub fn tid() -> u32 {
    get().tid.load(Ordering::Relaxed)
}

/// The calling thread's process id.
pub fn pid() -> u32 {
    get().pid.load(Ordering::Relaxed)
}

/// Sets up the calling thread's block (at its start, and when an exec changes its tid).
pub fn set(tid: u32, pid: u32, key: u64, fs: *const FsContext) {
    let l = get();
    l.tid.store(tid, Ordering::Relaxed);
    l.pid.store(pid, Ordering::Relaxed);
    l.key.store(key, Ordering::Relaxed);
    l.flags.store(0, Ordering::Relaxed);
    l.fs.store(fs as *mut FsContext, Ordering::Release);
}
