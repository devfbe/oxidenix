//! The server's own words for each of its threads, in the thread's State
//! page (`restricted::SERVER_THREAD_OFFSET`, which the kernel never
//! touches): the thread's role, the descriptor table it last used (a
//! cache of `SYS_FILES_RECORD`'s answer), and what a restartable call
//! (poll) keeps for `restart_syscall`. The server has no thread-local
//! storage (no FS or GS base); a thread finds its page from its stack, as
//! `sync` finds its count of locks.
//!
//! Only the thread itself reads and writes its words, so they are plain
//! atomics with relaxed ordering. A thread area is reused by a later thread
//! once its thread is gone: `start` sets every word before anything reads
//! one.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use restricted::{SERVER_THREAD_OFFSET, THREADS_BASE, THREAD_AREA};

#[repr(C)]
pub struct Words {
    /// `restricted::ROLE_*`.
    role: AtomicU64,
    /// The `fdtable::FilesContext` this thread uses (its record's word),
    /// 0 until looked up; valid while the thread runs in that table.
    files: AtomicU64,
    /// What `restart_syscall` restarts (`RESTART_*`) and its arguments.
    restart: [AtomicU64; 4],
}

/// Nothing to restart (`restart_syscall` answers EINTR).
pub const RESTART_NONE: u64 = 0;
/// poll(fds, nfds) until the deadline: `[kind, fds, nfds, deadline]`.
pub const RESTART_POLL: u64 = 1;

/// The calling thread's words.
pub fn words() -> &'static Words {
    let sp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags)) };
    let n = (sp - THREADS_BASE) / THREAD_AREA;
    unsafe { &*((restricted::thread_state(n) + SERVER_THREAD_OFFSET) as *const Words) }
}

/// A new thread starts with `role`.
pub fn start(role: u64) {
    let w = words();
    w.role.store(role, Relaxed);
    w.files.store(0, Relaxed);
    for word in &w.restart {
        word.store(0, Relaxed);
    }
}

/// Whether the calling thread is one of the instance's service threads
/// (the pager, the worker, the net thread), which serve no program.
pub fn is_service() -> bool {
    matches!(words().role.load(Relaxed), restricted::ROLE_PAGER | restricted::ROLE_WORKER | restricted::ROLE_NET)
}

/// The descriptor table this thread used last (0: none yet).
pub fn files() -> u64 {
    words().files.load(Relaxed)
}

pub fn set_files(word: u64) {
    words().files.store(word, Relaxed);
}

/// What `restart_syscall` would restart.
pub fn restart() -> [u64; 4] {
    words().restart.each_ref().map(|w| w.load(Relaxed))
}

pub fn set_restart(state: [u64; 4]) {
    for (w, v) in words().restart.iter().zip(state) {
        w.store(v, Relaxed);
    }
}
