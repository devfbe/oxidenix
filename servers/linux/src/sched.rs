//! Scheduling policy: sched_getscheduler and sched_getparam, getpriority
//! and setpriority. The kernel's scheduler has one policy for every
//! thread, Linux's SCHED_OTHER with static priority 0, so the first two
//! answer that (musl's pthread_getschedparam asks both, V8 does at
//! startup); within it a thread's nice value weighs its share of the CPU,
//! as on Linux (the kernel's fair scheduler).
//!
//! The thread ids are the instance's (`process`); the kernel keeps each
//! thread's nice value (`SYS_THREAD_NICE`, by key), and the server picks
//! the threads a call names: one, a process group's, or every one of the
//! instance (one user). Lowering a nice value is allowed: the one user is
//! root, with CAP_SYS_NICE, and RLIMIT_NICE has no limit.

use crate::process::{self, Pid};
use crate::syscall;
use alloc::vec::Vec;
use restricted::*;

const EINVAL: i64 = 22;
const ESRCH: i64 = 3;

const SYS_GETPRIORITY: u64 = 140;
const SYS_SETPRIORITY: u64 = 141;
const SYS_SCHED_GETPARAM: u64 = 143;
const SYS_SCHED_GETSCHEDULER: u64 = 145;
const SCHED_OTHER: i64 = 0;

/// The result of a scheduling-policy call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let result = match s.rax {
        SYS_SCHED_GETSCHEDULER => target(s.rdi).map(|_| SCHED_OTHER),
        SYS_SCHED_GETPARAM => getparam(s.rdi, s.rsi),
        SYS_GETPRIORITY => priority(s.rdi, s.rsi, None),
        SYS_SETPRIORITY => priority(s.rdi, s.rsi, Some(s.rdx as i32 as i64)).map(|_| 0),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// getpriority(which, who) and setpriority(which, who, nice): the nice
/// value of a process (a thread, as Linux takes the id), a process group
/// or a user's processes (one user, root: uid 0 is everyone, another uid
/// no one). getpriority's answer is the system call's, 20 - nice of the
/// most favored (libc turns it into the nice value); setpriority clamps
/// to -20..19 (root may raise and lower).
fn priority(which: u64, who: u64, set: Option<i64>) -> Result<i64, i64> {
    const PRIO_PROCESS: u64 = 0;
    const PRIO_PGRP: u64 = 1;
    const PRIO_USER: u64 = 2;
    let who = who as u32;
    let keys: Vec<u64> = match which {
        PRIO_PROCESS => alloc::vec![process::key_of(who as Pid).ok_or(ESRCH)?],
        PRIO_PGRP => {
            let pgid = if who == 0 { process::ids_of(0).map_or(0, |ids| ids.1) } else { who };
            process::keys_where(|p| p.pgid == pgid)
        }
        PRIO_USER if who == 0 => process::keys_where(|_| true),
        PRIO_USER => return Err(ESRCH),
        _ => return Err(EINVAL),
    };
    // (nice + 20 of the most favored thread, before a change.)
    let mut lowest: Option<i64> = None;
    for key in keys {
        let (setting, nice) = match set {
            Some(n) => (1, n.clamp(-20, 19) as u64),
            None => (0, 0),
        };
        let r = syscall(SYS_THREAD_NICE, [key, setting, nice, 0, 0, 0]);
        if r >= 0 {
            lowest = Some(lowest.map_or(r, |l| l.min(r)));
        }
    }
    let lowest = lowest.ok_or(ESRCH)?;
    Ok(40 - lowest)
}

/// sched_getparam(tid, param): the static priority, 0 under SCHED_OTHER.
fn getparam(tid: u64, param: u64) -> Result<i64, i64> {
    if param == 0 {
        return Err(EINVAL);
    }
    target(tid)?;
    crate::usercopy::write(param, &0i32)?;
    Ok(0)
}

/// Checks the thread a call names (a pid_t: the low 32 bits; 0 is the
/// caller): EINVAL for a negative id, ESRCH for one that does not exist.
fn target(tid: u64) -> Result<(), i64> {
    let tid = tid as i32;
    if tid < 0 {
        return Err(EINVAL);
    }
    if tid == 0 {
        return Ok(());
    }
    process::key_of(tid as Pid).map(|_| ()).ok_or(ESRCH)
}
