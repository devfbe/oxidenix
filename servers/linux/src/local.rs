//! The server's data for the thread it runs on (phases R6e, R8): a block in the thread's State
//! page (`restricted::SERVER_LOCAL_OFFSET`, which the kernel never touches), found from the
//! stack as the lock count is (`sync`; the server has no FS or GS base). It holds what the hot
//! paths need without the process lock: the thread's role, its tid and pid in the instance's
//! namespace, its key, and its working-directory record and descriptor table. Only the thread
//! itself writes it (`start` sets every word before anything reads one: a reused page holds a
//! dead thread's).

use crate::fdtable::FilesContext;
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
    /// `restricted::ROLE_*`.
    pub role: AtomicU32,
    /// The thread's key (`restricted::SYS_THREAD_CREATE`).
    pub key: AtomicU64,
    /// The thread's working-directory record: the reference its thread
    /// record (`process::Thread::fs`) holds, valid while the thread runs.
    pub fs: AtomicPtr<FsContext>,
    /// The thread's descriptor table: the reference its thread record
    /// (`process::Thread::files`) holds, valid while the thread runs (only
    /// the thread itself replaces it: execve, close_range's unshare, exit).
    pub files: AtomicPtr<FilesContext>,
    /// The status the thread's process ends with (`EXIT_PENDING`).
    pub exit_status: AtomicU32,
}

const _: () = assert!(SERVER_LOCAL_OFFSET as usize + core::mem::size_of::<Local>() <= 4096);

/// A call set a temporary signal mask (sigsuspend, ppoll, pselect, epoll_pwait): it is put
/// back before the program runs again, or by the signal frame of the handler it let in.
pub const RESTORE_MASK: u32 = 1;
/// The process must end (a failure past an execve's point of no return): the thread's loop
/// ends it once the call returned (`exit_pending`), on a stack that holds nothing more.
pub const EXIT_PENDING: u32 = 2;

/// Asks the thread's loop to end the process with `status` once the current call returned.
pub fn exit_pending(status: i32) {
    let l = get();
    l.exit_status.store(status as u32, Ordering::Relaxed);
    l.flags.fetch_or(EXIT_PENDING, Ordering::Relaxed);
}

/// The status a pending end asks for (`exit_pending`), if any.
pub fn pending_exit() -> Option<i32> {
    let l = get();
    (l.flags.load(Ordering::Relaxed) & EXIT_PENDING != 0).then(|| l.exit_status.load(Ordering::Relaxed) as i32)
}

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

/// A new thread starts with `role` and nothing else yet.
pub fn start(role: u64) {
    let l = get();
    l.role.store(role as u32, Ordering::Relaxed);
    l.tid.store(0, Ordering::Relaxed);
    l.pid.store(0, Ordering::Relaxed);
    l.flags.store(0, Ordering::Relaxed);
    l.key.store(0, Ordering::Relaxed);
    l.fs.store(core::ptr::null_mut(), Ordering::Relaxed);
    l.files.store(core::ptr::null_mut(), Ordering::Relaxed);
    l.exit_status.store(0, Ordering::Relaxed);
}

/// Whether the calling thread is one of the instance's service threads (the pager, the
/// worker, the net thread, the timer thread), which serve no program.
pub fn is_service() -> bool {
    use restricted::{ROLE_NET, ROLE_PAGER, ROLE_TIMER, ROLE_WORKER};
    matches!(get().role.load(Ordering::Relaxed) as u64, ROLE_PAGER | ROLE_WORKER | ROLE_NET | ROLE_TIMER)
}

/// Whether the calling thread is the instance's pager.
pub fn is_pager() -> bool {
    get().role.load(Ordering::Relaxed) as u64 == restricted::ROLE_PAGER
}

/// Sets up a program thread's block (at its start, and when an exec changes its tid).
pub fn set(tid: u32, pid: u32, key: u64, fs: *const FsContext, files: *const FilesContext) {
    let l = get();
    l.tid.store(tid, Ordering::Relaxed);
    l.pid.store(pid, Ordering::Relaxed);
    l.key.store(key, Ordering::Relaxed);
    l.flags.store(0, Ordering::Relaxed);
    l.fs.store(fs as *mut FsContext, Ordering::Release);
    l.files.store(files as *mut FilesContext, Ordering::Release);
}
