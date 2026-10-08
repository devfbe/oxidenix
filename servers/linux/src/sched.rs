//! Scheduling policy: sched_getscheduler and sched_getparam. The kernel's
//! scheduler has one policy for every thread, Linux's SCHED_OTHER with
//! static priority 0, so the answers are fixed; musl's
//! pthread_getschedparam asks both (V8 does at startup).
//!
//! Until the process model is the server's (R8) the thread ids are the
//! kernel's: a thread is known to exist when its /proc directory does in
//! the kernel's tree (procfs resolves the id of any thread, as Linux's
//! /proc does, though it lists processes only).

use crate::namespace::{kernel_root, KInode};
use crate::syscall;
use alloc::format;
use restricted::*;

const EINVAL: i64 = 22;
const ESRCH: i64 = 3;

const SYS_SCHED_GETPARAM: u64 = 143;
const SYS_SCHED_GETSCHEDULER: u64 = 145;
const SCHED_OTHER: i64 = 0;

/// The result of a scheduling-policy call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let result = match s.rax {
        SYS_SCHED_GETSCHEDULER => target(s.rdi).map(|_| SCHED_OTHER),
        SYS_SCHED_GETPARAM => getparam(s.rdi, s.rsi),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
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
    let path = format!("proc/{tid}");
    let mut walk = Walk::default();
    let found = KInode::from_result(syscall(
        SYS_INODE_WALK,
        [kernel_root(), path.as_ptr() as u64, path.len() as u64, &mut walk as *mut Walk as u64, 0, 0],
    ));
    // The handle goes at once; only whether the walk got there counts.
    match found {
        Ok(_) if walk.consumed as usize == path.len() => Ok(()),
        _ => Err(ESRCH),
    }
}
