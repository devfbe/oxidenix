//! pidfds (pidfd_open, clone's and clone3's CLONE_PIDFD, pidfd_send_signal, pidfd_getfd,
//! waitid's P_PIDFD): a file of the server's that names a process of the instance, or with
//! `PIDFD_THREAD` (clone: CLONE_PIDFD with CLONE_THREAD) one thread (Linux 6.9), as Linux's
//! (an anonymous inode, "anon_inode:[pidfd]").
//!
//! A pid is reused once its process is reaped, so a pidfd names its process by more than
//! the number: it holds the process's words (`process::Words`, kept by an `Arc` also after
//! the process is reaped), and the pid names its process only while the table's process
//! of that number has those words. A thread pidfd holds the thread's watch too
//! (`process::Watch`): a thread that does not lead its process is named while the table's
//! thread of that id has that watch; the main thread while its process is (a zombie too).
//! The watch carries the state for pidfds (alive, exited, reaped) and the ids of its
//! pidfds, which hear of its end: a process pidfd is readable (POLLIN) once its process
//! exited and hung up (POLLHUP too) once it was reaped; a thread pidfd is readable once its
//! thread exited (also the main thread, its process living on) and hung up once the thread
//! is gone (another thread: at once; the main thread: with its process), as Linux's
//! pidfd_poll. Reading one is EINVAL, as on Linux.
//!
//! `pidfd_send_signal` signals the thread of a thread pidfd and the process of a process
//! pidfd, unless its flags say otherwise (`PIDFD_SIGNAL_THREAD`, `_THREAD_GROUP`,
//! `_PROCESS_GROUP`); `pidfd_getfd` takes the named thread's descriptor table (a process's
//! main thread's, EBADF once it exited, as Linux's task of the pid); `waitid(P_PIDFD)` waits
//! for the child of the pidfd's id (none for a thread that does not lead its process:
//! ECHILD, as on Linux).

use crate::fdtable;
use crate::files::{self, File, EBADF, EFAULT, EINVAL};
use crate::heap::{self, TryGrow};
use crate::process::{self, Pid, Table, Watch, Words, ALIVE, EXITED, PROCS};
use crate::signal::{self, SigInfo};
use crate::usercopy;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use restricted::*;

const ESRCH: i64 = 3;
const EPERM: i64 = 1;
const ESPIPE: i64 = 29;
const ENOTTY: i64 = 25;

const POLLIN: i16 = 0x1;
const POLLHUP: i16 = 0x10;
const POLLRDNORM: i16 = 0x40;

/// pidfd_open's flags: PIDFD_NONBLOCK (O_NONBLOCK) and PIDFD_THREAD (O_EXCL, which a thread
/// pidfd's status flags show, as Linux's f_flags).
const PIDFD_NONBLOCK: u64 = files::O_NONBLOCK as u64;
pub const PIDFD_THREAD: u64 = 0o200;
/// pidfd_send_signal's flags (Linux 6.9).
const PIDFD_SIGNAL_THREAD: u64 = 1;
const PIDFD_SIGNAL_THREAD_GROUP: u64 = 2;
const PIDFD_SIGNAL_PROCESS_GROUP: u64 = 4;

pub const SYS_PIDFD_SEND_SIGNAL: u64 = 424;
pub const SYS_PIDFD_OPEN: u64 = 434;
pub const SYS_PIDFD_GETFD: u64 = 438;

/// A pidfd's file.
pub struct PidFd {
    id: u64,
    /// The id it names: a process's (also of a thread pidfd of its main thread) or a
    /// thread's.
    pid: Pid,
    /// The process's words (of a thread pidfd: the thread's process's).
    words: Arc<Words>,
    /// A thread pidfd's watch (None: a process pidfd, which watches `words.watch`).
    thread: Option<Arc<Watch>>,
}

/// What a pidfd in `state` reports to poll.
pub fn readiness(state: u32) -> i16 {
    match state {
        ALIVE => 0,
        EXITED => POLLIN | POLLRDNORM,
        _ => POLLIN | POLLRDNORM | POLLHUP,
    }
}

/// The task a pidfd names in the table (Linux's `pid_task(pid, PIDTYPE_PID)`): its process
/// and whether the thread itself still runs (its record there, not exited).
struct Task {
    pid: Pid,
    zombie: bool,
    running: bool,
}

impl PidFd {
    fn watch(&self) -> &Watch {
        self.thread.as_deref().unwrap_or(&self.words.watch)
    }

    pub fn ready(&self) -> i16 {
        readiness(self.watch().state.load(Ordering::Acquire))
    }

    pub fn is_thread(&self) -> bool {
        self.thread.is_some()
    }

    pub fn stat(&self) -> [u8; 144] {
        // An anonymous inode (no file type), as on Linux.
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.id.to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&0o600u32.to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st
    }

    /// The task it names (None: gone, its id perhaps another's now). The main thread is
    /// there while its process is (as Linux's zombie leader, also when it exited first);
    /// another thread while its record has this pidfd's watch.
    fn task(&self, t: &Table) -> Option<Task> {
        let running = |tid: Pid| t.threads.get(&tid).is_some_and(|th| !th.exited);
        if let Some(p) = t.procs.get(&self.pid).filter(|p| Arc::ptr_eq(&p.words, &self.words)) {
            return Some(Task { pid: p.pid, zombie: p.zombie.is_some(), running: running(self.pid) });
        }
        let mine = self.thread.as_ref()?;
        let th = t.threads.get(&self.pid).filter(|th| th.watch.as_ref().is_some_and(|w| Arc::ptr_eq(w, mine)))?;
        Some(Task { pid: th.pid, zombie: false, running: !th.exited })
    }
}

impl Drop for PidFd {
    /// Its file closed: its process or thread no longer reports to it.
    fn drop(&mut self) {
        let mut ids = self.watch().pidfds.lock();
        if let Some(at) = ids.iter().position(|&id| id == self.id) {
            ids.swap_remove(at);
        }
    }
}

/// The calls on a pidfd's descriptor: nothing to read or write.
pub fn call(nr: u64, f: &PidFd, a1: u64) -> Result<i64, i64> {
    match nr {
        files::SYS_FSTAT => usercopy::to_program(a1, &f.stat()).map(|_| 0).map_err(|_| EFAULT),
        files::SYS_IOCTL => Err(ENOTTY),
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(ESPIPE),
        _ => Err(EINVAL),
    }
}

/// The result of a pidfd call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let result = match s.rax {
        SYS_PIDFD_OPEN => open(s.rdi as i32, s.rsi),
        SYS_PIDFD_SEND_SIGNAL => send_signal(s.rdi, s.rsi, s.rdx, s.r10),
        SYS_PIDFD_GETFD => getfd(s.rdi, s.rsi, s.rdx),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// A new pidfd of id `pid` (a process, whose words are `words`; with `thread`, the watch of
/// the thread of that id), its descriptor in the calling process's table (close-on-exec;
/// `nonblock`: O_NONBLOCK; a thread pidfd's status flags have PIDFD_THREAD).
pub fn install(pid: Pid, words: &Arc<Words>, thread: Option<Arc<Watch>>, nonblock: bool) -> Result<i64, i64> {
    let id = files::new_id();
    let thread_flag = if thread.is_some() { PIDFD_THREAD as u32 } else { 0 };
    let pidfd = heap::try_arc(PidFd { id, pid, words: words.clone(), thread })?;
    // Known to its process or thread before anyone can poll it (its end reports to it);
    // forgotten again with the file if no descriptor can be had.
    {
        let mut ids = pidfd.watch().pidfds.lock();
        ids.try_grow(1)?;
        ids.push(id);
    }
    let flags = files::O_RDWR | files::O_CLOEXEC | thread_flag | if nonblock { files::O_NONBLOCK } else { 0 };
    files::install(id, File::PidFd(pidfd), flags)
}

/// pidfd_open(pid, flags): a pidfd of process `pid` (a zombie too), EINVAL for a thread
/// that does not lead its process; with PIDFD_THREAD of thread `pid` (any thread, the main
/// one also after it exited). ESRCH for none.
fn open(pid: i32, flags: u64) -> Result<i64, i64> {
    if flags & !(PIDFD_NONBLOCK | PIDFD_THREAD) != 0 || pid <= 0 {
        return Err(EINVAL);
    }
    let pid = pid as Pid;
    let nonblock = flags & PIDFD_NONBLOCK != 0;
    if flags & PIDFD_THREAD != 0 {
        let fresh = heap::try_arc(Watch::default())?;
        let (words, watch) = process::thread_watch(pid, fresh)?;
        return install(pid, &words, Some(watch), nonblock);
    }
    let words = {
        let t = PROCS.lock();
        match t.procs.get(&pid) {
            Some(p) => p.words.clone(),
            None if t.threads.contains_key(&pid) => return Err(EINVAL),
            None => return Err(ESRCH),
        }
    };
    install(pid, &words, None, nonblock)
}

/// The pidfd behind descriptor `fd` (EBADF for none, and for another file).
pub fn of(fd: u64) -> Result<Arc<PidFd>, i64> {
    match &files::lookup(fd)?.file {
        File::PidFd(p) => Ok(p.clone()),
        _ => Err(EBADF),
    }
}

/// For waitid's P_PIDFD: the id of pidfd `fd` and its process's words (a child of that id
/// with those words is waited for), and whether the pidfd is nonblocking.
pub fn wait_target(fd: u64) -> Result<(Pid, Arc<Words>, bool), i64> {
    let f = files::lookup(fd)?;
    let File::PidFd(p) = &f.file else { return Err(EBADF) };
    Ok((p.pid, p.words.clone(), f.flags() & files::O_NONBLOCK != 0))
}

/// pidfd_send_signal(pidfd, sig, info, flags): `sig` to the pidfd's thread (a thread
/// pidfd) or process, or by `flags` to the thread of its id, that thread's process, or the
/// process group of its id, as kill(2) or, with `info`, as rt_sigqueueinfo(2). ESRCH once
/// the task is gone (a zombie, or a thread that exited but is not gone yet, takes the
/// signal and does nothing with it), as Linux's.
fn send_signal(fd: u64, sig: u64, uinfo: u64, flags: u64) -> Result<i64, i64> {
    let pidfd = of(fd)?;
    let scope = match flags {
        0 if pidfd.is_thread() => PIDFD_SIGNAL_THREAD,
        0 => PIDFD_SIGNAL_THREAD_GROUP,
        PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP | PIDFD_SIGNAL_PROCESS_GROUP => flags,
        _ => return Err(EINVAL),
    };
    if sig > signal::NSIG as u64 {
        return Err(EINVAL);
    }
    let me = crate::local::pid();
    let info = if uinfo != 0 {
        let raw: [u64; 16] = usercopy::read(uinfo)?;
        // The caller's siginfo must be of this signal, and may claim to be the kernel's or
        // kill's only towards the calling thread itself (Linux compares the pidfd's pid with
        // the caller's task pid), never towards a process group.
        if raw[0] as u32 as u64 != sig {
            return Err(EINVAL);
        }
        let code = raw[1] as u32 as i32;
        if (code >= 0 || code == signal::SI_TKILL) && (pidfd.pid != crate::local::tid() || scope == PIDFD_SIGNAL_PROCESS_GROUP) {
            return Err(EPERM);
        }
        SigInfo(raw)
    } else {
        SigInfo::user(sig as u32, signal::SI_USER, me)
    };
    let mut t = PROCS.lock();
    let task = pidfd.task(&t).ok_or(ESRCH)?;
    if sig == 0 {
        return Ok(0);
    }
    match scope {
        PIDFD_SIGNAL_THREAD => {
            if task.running {
                signal::post_thread(&mut t, task.pid, pidfd.pid, info);
            }
        }
        PIDFD_SIGNAL_PROCESS_GROUP => {
            if !signal::post_pgrp(&mut t, pidfd.pid, info) {
                return Err(ESRCH);
            }
        }
        _ => {
            if !task.zombie {
                signal::post_process(&mut t, task.pid, info, false);
            }
        }
    }
    Ok(0)
}

/// pidfd_getfd(pidfd, targetfd, flags): a new descriptor (close-on-exec) of the caller's
/// for the open file description behind descriptor `targetfd` of the pidfd's thread (a
/// process pidfd's: the main thread's). ESRCH once the task is gone, EBADF for a
/// descriptor it does not have (nor any, once it exited).
fn getfd(fd: u64, target: u64, flags: u64) -> Result<i64, i64> {
    let pidfd = of(fd)?;
    if flags != 0 {
        return Err(EINVAL);
    }
    let files = {
        let t = PROCS.lock();
        pidfd.task(&t).ok_or(ESRCH)?;
        t.threads.get(&pidfd.pid).and_then(|th| th.files.clone()).ok_or(EBADF)?
    };
    let description = files.get(target)?;
    drop(files);
    fdtable::current().install(description, true, 0)
}
