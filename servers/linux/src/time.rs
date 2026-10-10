//! Clocks and sleeping (phase R5): clock_gettime, clock_getres,
//! gettimeofday, time, nanosleep, clock_nanosleep and sched_yield, over the
//! kernel's clocks (`SYS_CLOCK_READ`: wall, monotonic, the caller's own CPU
//! time; Linux's clock ids map to them here) and deadline sleep; and
//! setting the wall clock (clock_settime, settimeofday: `SYS_CLOCK_SET`,
//! R9).

use crate::syscall;
use crate::usercopy;
use restricted::*;

const NSEC_PER_SEC: u64 = 1_000_000_000;

const EINVAL: i64 = 22;
const EINTR: i64 = 4;
const EFAULT: i64 = 14;
const EOPNOTSUPP: i64 = 95;

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
const TIMER_ABSTIME: u64 = 1;

const SYS_SCHED_YIELD: u64 = 24;
const SYS_NANOSLEEP: u64 = 35;
const SYS_GETTIMEOFDAY: u64 = 96;
const SYS_SETTIMEOFDAY: u64 = 164;
const SYS_TIME: u64 = 201;
const SYS_CLOCK_SETTIME: u64 = 227;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_CLOCK_GETRES: u64 = 229;
const SYS_CLOCK_NANOSLEEP: u64 = 230;

/// The wall-clock time now, for file timestamps.
pub fn realtime() -> vfs::stat::Time {
    let ns = syscall(SYS_CLOCK_READ, [CLOCK_WALL, 0, 0, 0, 0, 0]).max(0) as u64;
    vfs::stat::Time { sec: (ns / NSEC_PER_SEC) as i64, nsec: (ns % NSEC_PER_SEC) as u32 }
}

/// The result of a time system call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3) = (s.rdi, s.rsi, s.rdx, s.r10);
    let result = match s.rax {
        SYS_CLOCK_GETTIME => clock_gettime(a0, a1),
        SYS_CLOCK_GETRES => clock_getres(a0, a1),
        SYS_GETTIMEOFDAY => gettimeofday(a0, a1),
        SYS_SETTIMEOFDAY => settimeofday(a0, a1),
        SYS_CLOCK_SETTIME => clock_settime(a0, a1),
        SYS_TIME => time(a0),
        SYS_NANOSLEEP => clock_nanosleep(CLOCK_MONOTONIC as u64, 0, a0, a1),
        SYS_CLOCK_NANOSLEEP => clock_nanosleep(a0, a1, a2, a3),
        SYS_SCHED_YIELD => Ok(syscall(SYS_YIELD, [0; 6])),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// The clock with Linux's id `id`, in nanoseconds: the kernel's wall and monotonic clocks
/// (nothing suspends, so boot time is monotonic time; the coarse and raw clocks are the
/// precise ones; TAI is the wall clock), the caller's own CPU clocks, or the CPU clock of a
/// process or thread by its id in the instance (Linux's encoding: `!pid << 3`, bit 2 a
/// thread, the low two bits what is counted; pid 0 the caller). A thread's clock is only
/// its own process's threads' (Linux's lookup_task).
fn clock(id: u64) -> Result<u64, i64> {
    let signed = id as i32 as i64;
    let kernel = match signed {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_REALTIME_ALARM | CLOCK_TAI => CLOCK_WALL,
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => CLOCK_MONO,
        CLOCK_PROCESS_CPUTIME_ID => CLOCK_PROCESS_CPU,
        CLOCK_THREAD_CPUTIME_ID => CLOCK_THREAD_CPU,
        id if id < 0 => return cpu_clock(id),
        _ => return Err(EINVAL),
    };
    let ns = syscall(SYS_CLOCK_READ, [kernel, 0, 0, 0, 0, 0]);
    if ns < 0 { Err(-ns) } else { Ok(ns as u64) }
}

/// The CPU clock `id` (negative: see `clock`) of a process or thread.
fn cpu_clock(id: i64) -> Result<u64, i64> {
    let pid = !(id >> 3) as u32;
    let what = id & 3;
    if what == 3 {
        return Err(EINVAL);
    }
    let (user, system, run) = if id & 4 != 0 {
        // 0 is the caller to the kernel.
        let key = if pid == 0 {
            0
        } else {
            if crate::process::PROCS.lock().threads.get(&pid).is_none_or(|t| t.pid != crate::local::pid()) {
                return Err(EINVAL);
            }
            crate::process::key_of(pid).ok_or(EINVAL)?
        };
        let mut info = ThreadInfo::default();
        if syscall(SYS_THREAD_INFO, [key, &mut info as *mut ThreadInfo as u64, 0, 0, 0, 0]) < 0 {
            return Err(EINVAL);
        }
        (info.user_ns, info.system_ns, info.run_ns)
    } else {
        let handle = if pid == 0 { 0 } else { crate::process::handle_of(pid).ok_or(EINVAL)? };
        let mut info = ProcInfo::default();
        if syscall(SYS_PROC_INFO, [handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]) < 0 {
            return Err(EINVAL);
        }
        (info.user_ns, info.system_ns, info.user_ns + info.system_ns)
    };
    // CPUCLOCK_PROF, CPUCLOCK_VIRT, CPUCLOCK_SCHED.
    Ok(match what {
        0 => user + system,
        1 => user,
        _ => run,
    })
}

/// clock_settime(clock, ts): only the wall clock can be set (every other clock EINVAL).
fn clock_settime(id: u64, ts: u64) -> Result<i64, i64> {
    if id as i32 as i64 != CLOCK_REALTIME {
        return Err(EINVAL);
    }
    set_time(Some(read_timespec(ts)?), None)
}

/// The latest second the wall clock may be set to, exclusive: Linux's TIME_SETTOD_SEC_MAX
/// (KTIME_SEC_MAX less 30 years of uptime, so that the monotonic clock cannot overflow).
const SETTOD_SEC_MAX: u64 = 9_223_372_036 - 30 * 365 * 86_400;

/// The time zone settimeofday set (`struct timezone`: minutes west of Greenwich, the
/// type of daylight saving time), which gettimeofday reports; Linux's sys_tz. Only a
/// tree with the host grant sets it, the machine's own (today the only one there is).
static TIME_ZONE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Linux's do_sys_settimeofday64: sets the wall clock to `ns` and the time zone to `tz`,
/// whichever is given, in Linux's order: a time past `SETTOD_SEC_MAX` is EINVAL, then
/// without the host grant EPERM (even with neither given: the capability is checked
/// first), then a zone more than 15 hours from Greenwich EINVAL, and a time before the
/// monotonic clock's (as Linux's timekeeping: the wall clock never goes below boot). The
/// zone is only reported (the wall clock is UTC: no warp of a local-time RTC).
fn set_time(ns: Option<u64>, tz: Option<[i32; 2]>) -> Result<i64, i64> {
    if ns.is_some_and(|ns| ns / NSEC_PER_SEC >= SETTOD_SEC_MAX) {
        return Err(EINVAL);
    }
    let granted = syscall(SYS_HOST_GRANTED, [0; 6]);
    if granted < 0 {
        return Err(-granted);
    }
    if let Some([west, dst]) = tz {
        if !(-15 * 60..=15 * 60).contains(&west) {
            return Err(EINVAL);
        }
        TIME_ZONE.store((west as u32 as u64) | (dst as u32 as u64) << 32, core::sync::atomic::Ordering::Relaxed);
    }
    let Some(ns) = ns else { return Ok(0) };
    if ns < clock(CLOCK_MONOTONIC as u64)? {
        return Err(EINVAL);
    }
    let r = syscall(SYS_CLOCK_SET, [ns, 0, 0, 0, 0, 0]);
    if r < 0 { Err(-r) } else { Ok(0) }
}

/// settimeofday(tv, tz): the wall clock from a timeval, and the time zone (`set_time`);
/// both read first (EFAULT, EINVAL for a timeval that is not normalized).
fn settimeofday(tv: u64, tz: u64) -> Result<i64, i64> {
    let mut ns = None;
    if tv != 0 {
        let [sec, usec]: [i64; 2] = usercopy::read(tv)?;
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return Err(EINVAL);
        }
        ns = Some((sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(usec as u64 * 1000));
    }
    let zone = if tz != 0 { Some(usercopy::read::<[i32; 2]>(tz)?) } else { None };
    set_time(ns, zone)
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
        // struct timezone, as settimeofday set it (UTC, no daylight saving, at first).
        let zone = TIME_ZONE.load(core::sync::atomic::Ordering::Relaxed);
        usercopy::write(tz, &[zone as u32, (zone >> 32) as u32])?;
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
    if flags & TIMER_ABSTIME != 0 {
        // Interrupted, it restarts as it is (ERESTARTNOHAND).
        let slept = syscall(SYS_SLEEP_UNTIL, [deadline, 0, 0, 0, 0, 0]);
        return match slept {
            s if s == -EINTR => Err(crate::signal::ERESTARTNOHAND),
            s if s < 0 => Err(-s),
            _ => Ok(0),
        };
    }
    sleep_rest(crate::signal::RestartBlock::Sleep { deadline, rem })
}

/// A relative sleep to `block.deadline`: cut short by a signal it stores the time left at
/// `block.rem` and keeps the rest for restart_syscall (ERESTART_RESTARTBLOCK: it goes on
/// to the same deadline if no handler runs).
pub fn sleep_rest(block: crate::signal::RestartBlock) -> Result<i64, i64> {
    let crate::signal::RestartBlock::Sleep { deadline, rem } = block else { return Err(EINTR) };
    let slept = syscall(SYS_SLEEP_UNTIL, [deadline, 0, 0, 0, 0, 0]);
    if slept == -EINTR {
        if rem != 0 {
            let left = deadline.saturating_sub(clock(CLOCK_MONOTONIC as u64)?);
            // As Linux's nanosleep_copyout.
            usercopy::write(rem, &timespec(left)).map_err(|_| EFAULT)?;
        }
        crate::signal::save_block(block);
        return Err(crate::signal::ERESTART_RESTARTBLOCK);
    }
    if slept < 0 { Err(-slept) } else { Ok(0) }
}
