//! futex(2) (phase R9): Linux's operations over the kernel's futexes on
//! the program's memory (`restricted::SYS_FUTEX_WAIT`, `SYS_FUTEX_WAKE`,
//! `SYS_FUTEX_REQUEUE`: wait, wake and requeue by key, as the kernel keys
//! a word: by address space and address, or by memory object and offset
//! for a shared mapping unless the operation is private). The server
//! decodes the operations, reads the timeouts (relative on the monotonic
//! clock for FUTEX_WAIT, absolute for FUTEX_WAIT_BITSET, on the wall clock
//! with FUTEX_CLOCK_REALTIME) and restarts an interrupted wait as Linux
//! does: without a timeout the call restarts unless a handler without
//! SA_RESTART runs (ERESTARTSYS), with one it goes on to the same deadline
//! through restart_syscall unless a handler runs (ERESTART_RESTARTBLOCK).
//! FUTEX_WAKE_OP and the priority-inheritance operations are ENOSYS, as
//! before R9.

use crate::signal::{self, RestartBlock, ERESTARTSYS, ERESTART_RESTARTBLOCK};
use crate::syscall;
use crate::usercopy;
use restricted::*;

const EINVAL: i64 = 22;
const EINTR: i64 = 4;
const ENOSYS: i64 = 38;

pub const SYS_FUTEX: u64 = 202;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;
const FUTEX_BITSET_MATCH_ANY: u64 = u32::MAX as u64;
const NSEC_PER_SEC: u64 = 1_000_000_000;

/// The result of futex(2) in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    if s.rax != SYS_FUTEX {
        return None;
    }
    Some(futex(s.rdi, s.rsi, s.rdx, s.r10, s.r8, s.r9).unwrap_or_else(|e| -e))
}

fn check(r: i64) -> Result<i64, i64> {
    if r < 0 { Err(-r) } else { Ok(r) }
}

/// A count argument (an int; negative counts as none).
fn count(n: u64) -> u64 {
    (n as u32 as i32).max(0) as u64
}

/// futex(uaddr, op, val, timeout or val2, uaddr2, val3).
fn futex(uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> Result<i64, i64> {
    let private = if op & FUTEX_PRIVATE_FLAG != 0 { FUTEX_PRIVATE } else { 0 };
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    if realtime && !matches!(cmd, FUTEX_WAIT | FUTEX_WAIT_BITSET) {
        return Err(ENOSYS);
    }
    // Program memory only (the kernel checks again).
    if uaddr >= SHARED_BASE {
        return Err(usercopy::EFAULT);
    }
    match cmd {
        FUTEX_WAIT => wait(uaddr, val as u32, deadline(timeout, false, realtime)?, FUTEX_BITSET_MATCH_ANY, private),
        FUTEX_WAIT_BITSET => wait(uaddr, val as u32, deadline(timeout, true, realtime)?, val3 as u32 as u64, private),
        FUTEX_WAKE => check(syscall(SYS_FUTEX_WAKE, [uaddr, count(val), FUTEX_BITSET_MATCH_ANY, private, 0, 0])),
        FUTEX_WAKE_BITSET => check(syscall(SYS_FUTEX_WAKE, [uaddr, count(val), val3 as u32 as u64, private, 0, 0])),
        // For the requeue operations the timeout argument is a count.
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            // Negative counts are EINVAL here (Linux's futex_requeue).
            if (val as u32 as i32) < 0 || (timeout as u32 as i32) < 0 {
                return Err(EINVAL);
            }
            if uaddr2 >= SHARED_BASE {
                return Err(usercopy::EFAULT);
            }
            let flags = private | if cmd == FUTEX_CMP_REQUEUE { FUTEX_CMP } else { 0 };
            check(syscall(SYS_FUTEX_REQUEUE, [uaddr, count(val), count(timeout), uaddr2, val3 as u32 as u64, flags]))
        }
        _ => Err(ENOSYS),
    }
}

/// The monotonic deadline of a timeout at `ts` (a timespec; 0: none): an
/// interval, or with `absolute` a time of the monotonic clock or, with
/// `realtime`, of the wall clock (taken as the monotonic time it
/// corresponds to now).
fn deadline(ts: u64, absolute: bool, realtime: bool) -> Result<Option<u64>, i64> {
    if ts == 0 {
        return Ok(None);
    }
    let [sec, nsec]: [i64; 2] = usercopy::read(ts)?;
    if sec < 0 || !(0..NSEC_PER_SEC as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    let t = (sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(nsec as u64);
    let now = syscall(SYS_CLOCK_READ, [CLOCK_MONO, 0, 0, 0, 0, 0]).max(0) as u64;
    Ok(Some(if !absolute {
        now.saturating_add(t)
    } else if realtime {
        let wall = syscall(SYS_CLOCK_READ, [CLOCK_WALL, 0, 0, 0, 0, 0]).max(0) as u64;
        now.saturating_add(t.saturating_sub(wall))
    } else {
        t
    }))
}

/// Waits while the word at `uaddr` holds `val`, until a wake with a bit of
/// `bitset`, the deadline or a signal.
fn wait(uaddr: u64, val: u32, deadline: Option<u64>, bitset: u64, private: u64) -> Result<i64, i64> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    wait_until(uaddr, val, deadline, bitset, private)
}

fn wait_until(uaddr: u64, val: u32, deadline: Option<u64>, bitset: u64, private: u64) -> Result<i64, i64> {
    // A deadline of 0 means none to the kernel: one that passed is 1.
    let at = deadline.map_or(0, |d| d.max(1));
    match check(syscall(SYS_FUTEX_WAIT, [uaddr, val as u64, at, bitset, private, 0])) {
        Err(EINTR) => match deadline {
            None => Err(ERESTARTSYS),
            Some(deadline) => {
                signal::save_block(RestartBlock::Futex { uaddr, val, deadline, bitset, private });
                Err(ERESTART_RESTARTBLOCK)
            }
        },
        r => r,
    }
}

/// restart_syscall of a futex wait: on to the same deadline.
pub fn wait_rest(block: RestartBlock) -> Result<i64, i64> {
    let RestartBlock::Futex { uaddr, val, deadline, bitset, private } = block else { return Err(EINTR) };
    wait_until(uaddr, val, Some(deadline), bitset, private)
}
