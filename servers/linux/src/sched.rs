//! Scheduling policy: sched_getscheduler and sched_getparam, getpriority
//! and setpriority. The kernel's scheduler has one policy for every
//! thread, Linux's SCHED_OTHER with static priority 0, so the first two
//! answer that (musl's pthread_getschedparam asks both, V8 does at
//! startup); within it a thread's nice value weighs its share of the CPU,
//! as on Linux (the kernel's round robin, weighted).
//!
//! Until the process model is the server's (R8) the thread ids are the
//! kernel's: the kernel says whether one exists (`SYS_THREAD_EXISTS`) and
//! keeps the nice values (`SYS_THREAD_NICE`).

use crate::syscall;
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
    let who = who as u32 as u64;
    let (scope, id) = match which {
        PRIO_PROCESS => (NICE_THREAD, who),
        PRIO_PGRP => (NICE_PGROUP, who),
        PRIO_USER if who == 0 => (NICE_ALL, 0),
        PRIO_USER => return Err(ESRCH),
        _ => return Err(EINVAL),
    };
    let (setting, nice) = match set {
        Some(n) => (1, n.clamp(-20, 19) as u64),
        None => (0, 0),
    };
    let lowest = syscall(SYS_THREAD_NICE, [scope, id, setting, nice, 0, 0]);
    if lowest < 0 {
        return Err(-lowest);
    }
    // (lowest is nice + 20.)
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
    match syscall(SYS_THREAD_EXISTS, [tid as u64, 0, 0, 0, 0, 0]) {
        0 => Ok(()),
        _ => Err(ESRCH),
    }
}
