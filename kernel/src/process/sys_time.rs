//! Clocks, sleeps and CPU-time accounting: clock_gettime and its
//! relatives, clock_nanosleep, getrusage and times.

use super::errno::*;
use super::sched::{current, TABLE};
use super::task::Task;
use super::{uaccess, Pid, TIMER_HZ};
use crate::time::{self, NSEC_PER_SEC};
use alloc::sync::Arc;

const CLOCK_REALTIME: i64 = 0;
const CLOCK_MONOTONIC: i64 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i64 = 2;
const CLOCK_THREAD_CPUTIME_ID: i64 = 3;
const CLOCK_MONOTONIC_RAW: i64 = 4;
const CLOCK_REALTIME_COARSE: i64 = 5;
const CLOCK_MONOTONIC_COARSE: i64 = 6;
const CLOCK_BOOTTIME: i64 = 7;
const CLOCK_REALTIME_ALARM: i64 = 8;
const CLOCK_BOOTTIME_ALARM: i64 = 9;
const CLOCK_TAI: i64 = 11;

/// What a clock id names. Negative ids are the CPU-time clocks of other
/// threads and processes, encoded as Linux does: the id is `!pid << 3`,
/// bit 2 selects a thread, the low two bits what is counted.
enum Clock {
    Monotonic,
    Realtime,
    Cpu { pid: Pid, thread: bool, what: CpuTime },
}

#[derive(Clone, Copy)]
enum CpuTime {
    /// User and system time (CPUCLOCK_PROF).
    Prof,
    /// User time only (CPUCLOCK_VIRT).
    Virt,
    /// Run time (CPUCLOCK_SCHED): the same sum, measured exactly.
    Sched,
}

fn clock(id: u64) -> Result<Clock, i64> {
    let id = id as i32 as i64;
    let me = || current().tgid();
    Ok(match id {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_REALTIME_ALARM | CLOCK_TAI => Clock::Realtime,
        // Nothing suspends, so boot time is monotonic time.
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => Clock::Monotonic,
        CLOCK_PROCESS_CPUTIME_ID => Clock::Cpu { pid: me(), thread: false, what: CpuTime::Sched },
        CLOCK_THREAD_CPUTIME_ID => Clock::Cpu { pid: current().tid(), thread: true, what: CpuTime::Sched },
        id if id < 0 => {
            let what = match id & 3 {
                0 => CpuTime::Prof,
                1 => CpuTime::Virt,
                2 => CpuTime::Sched,
                _ => return Err(EINVAL),
            };
            let thread = id & 4 != 0;
            let pid = !(id >> 3) as Pid;
            let pid = match pid {
                0 if thread => current().tid(),
                0 => me(),
                pid => pid,
            };
            Clock::Cpu { pid, thread, what }
        }
        _ => return Err(EINVAL),
    })
}

/// A thread's CPU time: only threads of the caller's own process.
fn thread_cputime(tid: Pid) -> Result<(u64, u64, u64), i64> {
    let me = current();
    let task: Arc<Task> = TABLE.lock().tasks.get(&tid).cloned().ok_or(EINVAL)?;
    if !Arc::ptr_eq(&task.group, &me.group) {
        return Err(EINVAL);
    }
    let (user, system) = task.cputime();
    Ok((user, system, task.runtime()))
}

fn process_cputime(pid: Pid) -> Result<(u64, u64), i64> {
    let group = TABLE.lock().groups.get(&pid).cloned().ok_or(EINVAL)?;
    let info = group.info.lock();
    Ok(info.cputime())
}

fn read_clock(id: u64) -> Result<u64, i64> {
    Ok(match clock(id)? {
        Clock::Monotonic => time::now(),
        Clock::Realtime => time::realtime(),
        Clock::Cpu { pid, thread: true, what } => {
            let (user, system, run) = thread_cputime(pid)?;
            match what {
                CpuTime::Prof => user + system,
                CpuTime::Virt => user,
                CpuTime::Sched => run,
            }
        }
        Clock::Cpu { pid, thread: false, what } => {
            let (user, system) = process_cputime(pid)?;
            match what {
                CpuTime::Virt => user,
                CpuTime::Prof | CpuTime::Sched => user + system,
            }
        }
    })
}

fn timespec(ns: u64) -> [u64; 2] {
    [ns / NSEC_PER_SEC, ns % NSEC_PER_SEC]
}

fn timeval(ns: u64) -> [u64; 2] {
    [ns / NSEC_PER_SEC, ns % NSEC_PER_SEC / 1000]
}

/// A user timespec as nanoseconds; EINVAL if it is not normalized.
pub fn read_timespec(ptr: u64) -> Result<u64, i64> {
    let [sec, nsec]: [i64; 2] = uaccess::read(ptr)?;
    if sec < 0 || !(0..NSEC_PER_SEC as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    Ok((sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(nsec as u64))
}

pub fn clock_gettime(id: u64, ts: u64) -> SysResult {
    uaccess::write(ts, timespec(read_clock(id)?))?;
    Ok(0)
}

/// Every clock counts in nanoseconds.
pub fn clock_getres(id: u64, res: u64) -> SysResult {
    // A CPU clock of a thread or process that does not exist is invalid.
    read_clock(id)?;
    if res != 0 {
        uaccess::write(res, timespec(1))?;
    }
    Ok(0)
}

/// Only the wall clock can be set.
pub fn clock_settime(id: u64, ts: u64) -> SysResult {
    if id as i32 as i64 != CLOCK_REALTIME {
        return Err(EINVAL);
    }
    time::set_realtime(read_timespec(ts)?);
    Ok(0)
}

/// clock_nanosleep(clock, flags, request, remain), and nanosleep (on the
/// monotonic clock). An absolute sleep on the wall clock ends at the
/// monotonic time that corresponds to it when the sleep starts. A
/// relative sleep cut short by a signal stores the time left.
pub fn clock_nanosleep(id: u64, flags: u64, req: u64, rem: u64) -> SysResult {
    const TIMER_ABSTIME: u64 = 1;
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
    let now = time::now();
    let deadline = match (flags & TIMER_ABSTIME != 0, realtime) {
        (false, _) => now.saturating_add(t),
        (true, false) => t,
        (true, true) => now.saturating_add(t.saturating_sub(time::realtime())),
    };
    super::sleep_until(deadline).map(|_| 0).inspect_err(|_| {
        if flags & TIMER_ABSTIME == 0 && rem != 0 {
            let _ = uaccess::write(rem, timespec(deadline.saturating_sub(time::now())));
        }
    })
}

pub fn gettimeofday(tv: u64, tz: u64) -> SysResult {
    if tv != 0 {
        uaccess::write(tv, timeval(time::realtime()))?;
    }
    if tz != 0 {
        // struct timezone: UTC, no daylight saving.
        uaccess::write(tz, [0u32; 2])?;
    }
    Ok(0)
}

pub fn settimeofday(tv: u64) -> SysResult {
    if tv != 0 {
        let [sec, usec]: [i64; 2] = uaccess::read(tv)?;
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return Err(EINVAL);
        }
        time::set_realtime((sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(usec as u64 * 1000));
    }
    Ok(0)
}

pub fn time(tloc: u64) -> SysResult {
    let now = time::realtime() / NSEC_PER_SEC;
    if tloc != 0 {
        uaccess::write(tloc, now)?;
    }
    Ok(now as i64)
}

const RUSAGE_SELF: i64 = 0;
const RUSAGE_CHILDREN: i64 = -1;
const RUSAGE_THREAD: i64 = 1;

/// getrusage(who, usage): CPU times and the peak of the resident set (of
/// the program running now); the other counters stay 0.
pub fn getrusage(who: u64, usage: u64) -> SysResult {
    let me = current();
    let ((user, system), pages) = match who as i32 as i64 {
        RUSAGE_SELF => {
            let info = me.group.info.lock();
            (info.cputime(), info.mem.as_ref().map_or(0, |m| m.peak_pages.load(core::sync::atomic::Ordering::Relaxed)))
        }
        RUSAGE_THREAD => (me.cputime(), 0),
        RUSAGE_CHILDREN => (me.group.info.lock().children_time, 0),
        _ => return Err(EINVAL),
    };
    let mut out = [0u64; 18];
    out[..2].copy_from_slice(&timeval(user));
    out[2..4].copy_from_slice(&timeval(system));
    // ru_maxrss, in KiB.
    out[4] = pages * 4;
    uaccess::write(usage, out)?;
    Ok(0)
}

/// times(buf): CPU times in clock ticks; returns the ticks since boot.
pub fn times(buf: u64) -> SysResult {
    let tick = NSEC_PER_SEC / TIMER_HZ;
    if buf != 0 {
        let info = current().group.info.lock();
        let ((user, system), children) = (info.cputime(), info.children_time);
        drop(info);
        uaccess::write(buf, [user / tick, system / tick, children.0 / tick, children.1 / tick])?;
    }
    Ok((time::now() / tick) as i64)
}
