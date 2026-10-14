//! pidfds (pidfd_open, clone's and clone3's CLONE_PIDFD, pidfd_send_signal, pidfd_getfd,
//! waitid's P_PIDFD): a file of the server's that names a process of the instance, as
//! Linux's (an anonymous inode, "anon_inode:[pidfd]").
//!
//! A pid is reused once its process is reaped, so a pidfd names its process by more than
//! the number: it holds the process's words (`process::Words`, kept by an `Arc` also after
//! the process is reaped), and the pid names its process only while the table's process
//! of that number has those words. The words carry the process's state for pidfds (alive,
//! exited: a zombie, reaped) and the ids of its pidfds, which hear of its end: a pidfd is
//! readable (POLLIN) once its process exited and hung up (POLLHUP too) once it was reaped.
//! Reading one is EINVAL, as on Linux.
//!
//! Thread pidfds (`PIDFD_THREAD`, `CLONE_PIDFD` with `CLONE_THREAD`, Linux 6.9) are not
//! offered (EINVAL, as before 6.9).

use crate::fdtable;
use crate::files::{self, File, EBADF, EFAULT, EINVAL};
use crate::heap::{self, TryGrow};
use crate::process::{self, Pid, Words, ALIVE, EXITED, PROCS};
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

/// pidfd_open's flags: PIDFD_NONBLOCK (O_NONBLOCK).
const PIDFD_NONBLOCK: u64 = files::O_NONBLOCK as u64;
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
    pid: Pid,
    words: Arc<Words>,
}

/// What a pidfd of a process in `state` reports to poll.
pub fn readiness(state: u32) -> i16 {
    match state {
        ALIVE => 0,
        EXITED => POLLIN | POLLRDNORM,
        _ => POLLIN | POLLRDNORM | POLLHUP,
    }
}

impl PidFd {
    pub fn ready(&self) -> i16 {
        readiness(self.words.state.load(Ordering::Acquire))
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

    /// Whether `t`'s process `pid` is this pidfd's (not reaped, its number not reused).
    fn names(&self, p: &process::Proc) -> bool {
        Arc::ptr_eq(&p.words, &self.words)
    }
}

impl Drop for PidFd {
    /// Its file closed: its process no longer reports to it.
    fn drop(&mut self) {
        let mut ids = self.words.pidfds.lock();
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

/// A new pidfd of process `pid` (whose words are `words`), its descriptor in the calling
/// process's table (close-on-exec; `nonblock`: O_NONBLOCK).
pub fn install(pid: Pid, words: &Arc<Words>, nonblock: bool) -> Result<i64, i64> {
    let id = files::new_id();
    let pidfd = heap::try_arc(PidFd { id, pid, words: words.clone() })?;
    // Known to its process before anyone can poll it (its end reports to it); forgotten
    // again with the file if no descriptor can be had.
    {
        let mut ids = words.pidfds.lock();
        ids.try_grow(1)?;
        ids.push(id);
    }
    let flags = files::O_RDWR | files::O_CLOEXEC | if nonblock { files::O_NONBLOCK } else { 0 };
    files::install(id, File::PidFd(pidfd), flags)
}

/// pidfd_open(pid, flags): a pidfd of process `pid` (a zombie too); EINVAL for a thread
/// that does not lead its process, ESRCH for none.
fn open(pid: i32, flags: u64) -> Result<i64, i64> {
    if flags & !PIDFD_NONBLOCK != 0 || pid <= 0 {
        return Err(EINVAL);
    }
    let pid = pid as Pid;
    let words = {
        let t = PROCS.lock();
        match t.procs.get(&pid) {
            Some(p) => p.words.clone(),
            None if t.threads.contains_key(&pid) => return Err(EINVAL),
            None => return Err(ESRCH),
        }
    };
    install(pid, &words, flags & PIDFD_NONBLOCK != 0)
}

/// The pidfd behind descriptor `fd` (EBADF for none, and for another file).
pub fn of(fd: u64) -> Result<Arc<PidFd>, i64> {
    match &files::lookup(fd)?.file {
        File::PidFd(p) => Ok(p.clone()),
        _ => Err(EBADF),
    }
}

/// For waitid's P_PIDFD: the process of pidfd `fd` (its pid and words) and whether the
/// pidfd is nonblocking.
pub fn wait_target(fd: u64) -> Result<(Pid, Arc<Words>, bool), i64> {
    let f = files::lookup(fd)?;
    let File::PidFd(p) = &f.file else { return Err(EBADF) };
    Ok((p.pid, p.words.clone(), f.flags() & files::O_NONBLOCK != 0))
}

/// pidfd_send_signal(pidfd, sig, info, flags): `sig` to the pidfd's process (or, by
/// `flags`, its main thread or its process group), as kill(2) or, with `info`, as
/// rt_sigqueueinfo(2). ESRCH once the process was reaped (a zombie takes the signal and
/// does nothing with it).
fn send_signal(fd: u64, sig: u64, uinfo: u64, flags: u64) -> Result<i64, i64> {
    let pidfd = of(fd)?;
    if flags & !(PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP | PIDFD_SIGNAL_PROCESS_GROUP) != 0 || flags.count_ones() > 1 {
        return Err(EINVAL);
    }
    if sig > signal::NSIG as u64 {
        return Err(EINVAL);
    }
    let me = crate::local::pid();
    let info = if uinfo != 0 {
        let raw: [u64; 16] = usercopy::read(uinfo)?;
        // The caller's siginfo must be of this signal, and may claim to be the kernel's or
        // kill's only towards itself.
        if raw[0] as u32 as u64 != sig {
            return Err(EINVAL);
        }
        let code = raw[1] as u32 as i32;
        if (code >= 0 || code == signal::SI_TKILL) && pidfd.pid != me {
            return Err(EPERM);
        }
        SigInfo(raw)
    } else {
        SigInfo::user(sig as u32, signal::SI_USER, me)
    };
    let mut t = PROCS.lock();
    let p = t.procs.get(&pidfd.pid).filter(|p| pidfd.names(p)).ok_or(ESRCH)?;
    let zombie = p.zombie.is_some();
    let pgid = pidfd.pid;
    if sig == 0 {
        return Ok(0);
    }
    if zombie {
        return Ok(0);
    }
    match flags {
        PIDFD_SIGNAL_THREAD => {
            // The thread of the pidfd's id: the process's main one, while it lives.
            if !t.threads.get(&pidfd.pid).is_some_and(|th| !th.exited) {
                return Err(ESRCH);
            }
            signal::post_thread(&mut t, pidfd.pid, pidfd.pid, info);
        }
        PIDFD_SIGNAL_PROCESS_GROUP => {
            if !signal::post_pgrp(&mut t, pgid, info) {
                return Err(ESRCH);
            }
        }
        _ => {
            signal::post_process(&mut t, pidfd.pid, info, false);
        }
    }
    Ok(0)
}

/// pidfd_getfd(pidfd, targetfd, flags): a new descriptor (close-on-exec) of the caller's
/// for the open file description behind descriptor `targetfd` of the pidfd's process.
/// ESRCH once the process was reaped, EBADF for a descriptor it does not have (nor any,
/// once it exited).
fn getfd(fd: u64, target: u64, flags: u64) -> Result<i64, i64> {
    let pidfd = of(fd)?;
    if flags != 0 {
        return Err(EINVAL);
    }
    let files = {
        let t = PROCS.lock();
        let p = t.procs.get(&pidfd.pid).filter(|p| pidfd.names(p)).ok_or(ESRCH)?;
        if p.zombie.is_some() {
            return Err(EBADF);
        }
        p.threads.iter().find_map(|tid| t.threads.get(tid).and_then(|th| th.files.clone())).ok_or(EBADF)?
    };
    let description = files.get(target)?;
    drop(files);
    fdtable::current().install(description, true, 0)
}
