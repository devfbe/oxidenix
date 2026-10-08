//! Clocks and sleeping (phase R5): clock_gettime, clock_getres,
//! gettimeofday, time, nanosleep, clock_nanosleep and sched_yield, over the
//! kernel's clock and deadline sleep. Setting the clock stays with the
//! kernel for now.

use crate::syscall;
use crate::usercopy;
use restricted::*;

const NSEC_PER_SEC: u64 = 1_000_000_000;

const EINVAL: i64 = 22;
const EINTR: i64 = 4;
const EOPNOTSUPP: i64 = 95;

const CLOCK_REALTIME: i64 = 0;
const CLOCK_MONOTONIC: i64 = 1;
const CLOCK_THREAD_CPUTIME_ID: i64 = 3;
const CLOCK_BOOTTIME: i64 = 7;
const CLOCK_REALTIME_ALARM: i64 = 8;
const CLOCK_BOOTTIME_ALARM: i64 = 9;
const CLOCK_TAI: i64 = 11;
const TIMER_ABSTIME: u64 = 1;

const SYS_SCHED_YIELD: u64 = 24;
const SYS_NANOSLEEP: u64 = 35;
const SYS_GETTIMEOFDAY: u64 = 96;
const SYS_TIME: u64 = 201;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_CLOCK_GETRES: u64 = 229;
const SYS_CLOCK_NANOSLEEP: u64 = 230;

/// The result of a time system call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3) = (s.rdi, s.rsi, s.rdx, s.r10);
    let result = match s.rax {
        SYS_CLOCK_GETTIME => clock_gettime(a0, a1),
        SYS_CLOCK_GETRES => clock_getres(a0, a1),
        SYS_GETTIMEOFDAY => gettimeofday(a0, a1),
        SYS_TIME => time(a0),
        SYS_NANOSLEEP => clock_nanosleep(CLOCK_MONOTONIC as u64, 0, a0, a1),
        SYS_CLOCK_NANOSLEEP => clock_nanosleep(a0, a1, a2, a3),
        SYS_SCHED_YIELD => Ok(syscall(SYS_YIELD, [0; 6])),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

fn clock(id: u64) -> Result<u64, i64> {
    let ns = syscall(SYS_CLOCK_READ, [id, 0, 0, 0, 0, 0]);
    if ns < 0 { Err(-ns) } else { Ok(ns as u64) }
}

fn timespec(ns: u64) -> [u64; 2] {
    [ns / NSEC_PER_SEC, ns % NSEC_PER_SEC]
}

/// A program timespec as nanoseconds; EINVAL if it is not normalized.
fn read_timespec(addr: u64) -> Result<u64, i64> {
    let [sec, nsec]: [i64; 2] = usercopy::read(addr)?;
    if sec < 0 || !(0..NSEC_PER_SEC as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    Ok((sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(nsec as u64))
}

fn clock_gettime(id: u64, ts: u64) -> Result<i64, i64> {
    usercopy::write(ts, &timespec(clock(id)?))?;
    Ok(0)
}

/// Every clock counts in nanoseconds.
fn clock_getres(id: u64, res: u64) -> Result<i64, i64> {
    // A CPU clock of a thread or process that does not exist is invalid.
    clock(id)?;
    if res != 0 {
        usercopy::write(res, &timespec(1))?;
    }
    Ok(0)
}

fn gettimeofday(tv: u64, tz: u64) -> Result<i64, i64> {
    if tv != 0 {
        let ns = clock(CLOCK_REALTIME as u64)?;
        usercopy::write(tv, &[ns / NSEC_PER_SEC, ns % NSEC_PER_SEC / 1000])?;
    }
    if tz != 0 {
        // struct timezone: UTC, no daylight saving.
        usercopy::write(tz, &[0u32; 2])?;
    }
    Ok(0)
}

fn time(tloc: u64) -> Result<i64, i64> {
    let now = clock(CLOCK_REALTIME as u64)? / NSEC_PER_SEC;
    if tloc != 0 {
        usercopy::write(tloc, &now)?;
    }
    Ok(now as i64)
}

/// clock_nanosleep(clock, flags, request, remain), and nanosleep (on the
/// monotonic clock). An absolute sleep on the wall clock ends at the
/// monotonic time that corresponds to it when the sleep starts. A
/// relative sleep cut short by a signal stores the time left.
fn clock_nanosleep(id: u64, flags: u64, req: u64, rem: u64) -> Result<i64, i64> {
    let realtime = match id as i32 as i64 {
        CLOCK_REALTIME | CLOCK_REALTIME_ALARM | CLOCK_TAI => true,
        CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => false,
        CLOCK_THREAD_CPUTIME_ID => return Err(EINVAL),
        _ => {
            // Valid clocks without sleeps (CPU time, coarse, raw).
            clock(id)?;
            return Err(EOPNOTSUPP);
        }
    };
    let t = read_timespec(req)?;
    let now = clock(CLOCK_MONOTONIC as u64)?;
    let deadline = match (flags & TIMER_ABSTIME != 0, realtime) {
        (false, _) => now.saturating_add(t),
        (true, false) => t,
        (true, true) => now.saturating_add(t.saturating_sub(clock(CLOCK_REALTIME as u64)?)),
    };
    let slept = syscall(SYS_SLEEP_UNTIL, [deadline, 0, 0, 0, 0, 0]);
    if slept == -EINTR && flags & TIMER_ABSTIME == 0 && rem != 0 {
        let left = deadline.saturating_sub(clock(CLOCK_MONOTONIC as u64)?);
        let _ = usercopy::write(rem, &timespec(left));
    }
    if slept < 0 { Err(-slept) } else { Ok(0) }
}
