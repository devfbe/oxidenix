//! Interval timers (phase R8): alarm, setitimer and getitimer of `ITIMER_REAL`, run by the
//! instance's timer thread (`ROLE_TIMER`, a service thread of the pager's process), which
//! sleeps until the earliest deadline of the instance's processes and sends SIGALRM.
//!
//! A periodic timer is not reloaded when it expires but parked until its signal is taken
//! (delivered, or returned by sigtimedwait), as Linux does for POSIX timers: a pending
//! SIGALRM suppresses further expiries, so however short the interval, the timer costs at
//! most one expiry per signal the process handles. Periods missed meanwhile are skipped.
//! A parked timer whose signal was dropped as ignored runs on once someone can take it (a
//! handler is installed, or sigtimedwait waits for it). `ITIMER_VIRTUAL` and `ITIMER_PROF`
//! are not offered (EINVAL). Timers are not inherited by fork and survive exec.

use crate::process::{self, Pid, Table, PROCS};
use crate::signal::{self, SigInfo};
use crate::syscall;
use crate::usercopy;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const EINVAL: i64 = 22;
const NSEC_PER_SEC: u64 = 1_000_000_000;
const ITIMER_REAL: u64 = 0;

const SYS_GETITIMER: u64 = 36;
const SYS_ALARM: u64 = 37;
const SYS_SETITIMER: u64 = 38;

/// A process's `ITIMER_REAL`.
#[derive(Clone, Copy, Default)]
pub struct ITimer {
    /// When it expires next (monotonic nanoseconds; 0: off), and its interval.
    pub at: u64,
    pub every: u64,
    /// It expired and its SIGALRM pends: the next period starts when that is taken.
    pub parked: bool,
}

/// Advances when a timer changed: the timer thread looks again.
static CHANGED: AtomicU32 = AtomicU32::new(0);

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64
}

/// A timer was set or ended: the timer thread computes its next deadline again.
pub fn changed() {
    process::wake_word(&CHANGED);
}

/// The first period of a timer due at `at` that ends after `now`.
fn next_period(at: u64, every: u64, now: u64) -> u64 {
    if at > now || every == 0 {
        return at;
    }
    let periods = (now - at) / every + 1;
    at.saturating_add(periods.saturating_mul(every))
}

/// SIGALRM of process `pid` was taken: a parked timer runs on with its next period.
pub fn taken(t: &mut Table, pid: Pid) {
    let Some(p) = t.procs.get_mut(&pid) else { return };
    if p.itimer.parked {
        p.itimer.parked = false;
        p.itimer.at = next_period(p.itimer.at, p.itimer.every, now());
        changed();
    }
}

/// Someone can take process `pid`'s SIGALRM again (a handler, sigtimedwait): a parked timer
/// whose signal was dropped runs on.
pub fn handled(t: &mut Table, pid: Pid) {
    let pending = t.procs.get(&pid).is_some_and(|p| p.sig.shared.set & signal::bit(signal::SIGALRM) != 0);
    if !pending {
        taken(t, pid);
    }
}

/// The timer thread: sends SIGALRM for every timer that expired, then sleeps until the next
/// deadline or a change.
pub fn thread() -> ! {
    loop {
        let seen = CHANGED.load(Ordering::Acquire);
        let next = {
            let mut t = PROCS.lock();
            let now = now();
            let due: alloc::vec::Vec<Pid> =
                t.procs.values().filter(|p| p.zombie.is_none() && p.itimer.at != 0 && !p.itimer.parked && p.itimer.at <= now).map(|p| p.pid).collect();
            for pid in due {
                let p = t.procs.get_mut(&pid).expect("listed");
                if p.itimer.every == 0 {
                    p.itimer.at = 0;
                } else {
                    p.itimer.parked = true;
                }
                signal::post_process(&mut t, pid, SigInfo::kernel(signal::SIGALRM), false);
            }
            t.procs.values().filter(|p| p.zombie.is_none() && p.itimer.at != 0 && !p.itimer.parked).map(|p| p.itimer.at).min().unwrap_or(0)
        };
        // (A dying service thread's wait ends at once; its next call ends it.)
        process::wait_word(&CHANGED, seen, next, false);
    }
}

/// The result of a timer call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let result = match s.rax {
        SYS_ALARM => {
            let (old, _) = set((s.rdi as u32 as u64).saturating_mul(NSEC_PER_SEC), 0);
            Ok(old.div_ceil(NSEC_PER_SEC) as i64)
        }
        SYS_SETITIMER => setitimer(s.rdi, s.rsi, s.rdx),
        SYS_GETITIMER => getitimer(s.rdi, s.rsi),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// The calling process's timer: (time left, interval) in nanoseconds.
fn left(it: &ITimer) -> (u64, u64) {
    if it.at == 0 {
        return (0, 0);
    }
    let now = now();
    // A parked timer reports the period it would be in had it run on.
    let at = if it.parked { next_period(it.at, it.every, now) } else { it.at };
    // An armed timer never reports 0 left, which would mean "off".
    (at.saturating_sub(now).max(1000), it.every)
}

/// Sets the calling process's timer to `value` (0: off) and `interval` nanoseconds; returns
/// the old setting.
fn set(value: u64, interval: u64) -> (u64, u64) {
    let pid = crate::local::pid();
    let old = {
        let mut t = PROCS.lock();
        let Some(p) = t.procs.get_mut(&pid) else { return (0, 0) };
        let old = left(&p.itimer);
        p.itimer = if value == 0 { ITimer::default() } else { ITimer { at: now().saturating_add(value), every: interval, parked: false } };
        old
    };
    changed();
    old
}

/// struct itimerval: (interval, value) as two timevals, in nanoseconds.
fn read_itimerval(addr: u64) -> Result<(u64, u64), i64> {
    let [isec, iusec, vsec, vusec]: [i64; 4] = usercopy::read(addr)?;
    if !(0..1_000_000).contains(&iusec) || !(0..1_000_000).contains(&vusec) || isec < 0 || vsec < 0 {
        return Err(EINVAL);
    }
    let ns = |sec: i64, usec: i64| (sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(usec as u64 * 1000);
    Ok((ns(isec, iusec), ns(vsec, vusec)))
}

fn write_itimerval(addr: u64, (value, interval): (u64, u64)) -> Result<(), i64> {
    if addr == 0 {
        return Ok(());
    }
    let us = |ns: u64| ns.div_ceil(1000);
    let (v, i) = (us(value), us(interval));
    usercopy::write(addr, &[i / 1_000_000, i % 1_000_000, v / 1_000_000, v % 1_000_000])
}

fn setitimer(which: u64, new: u64, old: u64) -> Result<i64, i64> {
    if which != ITIMER_REAL {
        return Err(EINVAL);
    }
    let (interval, value) = if new == 0 { (0, 0) } else { read_itimerval(new)? };
    write_itimerval(old, set(value, interval))?;
    Ok(0)
}

fn getitimer(which: u64, cur: u64) -> Result<i64, i64> {
    if which != ITIMER_REAL {
        return Err(EINVAL);
    }
    let pid = crate::local::pid();
    let setting = PROCS.lock().procs.get(&pid).map_or((0, 0), |p| left(&p.itimer));
    write_itimerval(cur, setting)?;
    Ok(0)
}
