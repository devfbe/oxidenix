//! Scheduling policy: sched_getscheduler and sched_getparam. The kernel's
//! scheduler has one policy for every thread, Linux's SCHED_OTHER with
//! static priority 0, so the answers are fixed; musl's
//! pthread_getschedparam asks both (V8 does at startup).
//!
//! Until the process model is the server's (R8) the thread ids are the
//! kernel's: the kernel says whether one exists (`SYS_THREAD_EXISTS`).

use crate::syscall;
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
    match syscall(SYS_THREAD_EXISTS, [tid as u64, 0, 0, 0, 0, 0]) {
        0 => Ok(()),
        _ => Err(ESRCH),
    }
}
