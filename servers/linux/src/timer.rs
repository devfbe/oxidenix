//! Timers (phase R8, docs/design/linux-server.md "Timers"): the interval timers (alarm,
//! setitimer and getitimer of `ITIMER_REAL`, `ITIMER_VIRTUAL` and `ITIMER_PROF`) and the
//! POSIX timers (timer_create, timer_settime, timer_gettime, timer_getoverrun,
//! timer_delete) of the instance's processes, run by its timer thread (`ROLE_TIMER`, a
//! service thread of the pager's process), which also tells timerfds' pollers of their
//! expiries (`timerfd`).
//!
//! **Clocks.** A timer on the wall or the monotonic clock is a deadline the timer thread
//! sleeps until (an absolute one on the wall clock stays one of the wall clock: setting the
//! clock moves it, as POSIX says; a relative one is a span of the monotonic clock). A timer
//! on a CPU clock (`ITIMER_VIRTUAL`: the process's user time, `ITIMER_PROF` and
//! `CLOCK_PROCESS_CPUTIME_ID`: its user and system time, `CLOCK_THREAD_CPUTIME_ID`, the CPU
//! clocks of other processes and threads) has its deadline on that clock: the timer
//! thread arms the kernel's alarm of each process's and thread's clocks with the earliest
//! deadline of the timers on them (`SYS_CPU_ALARM`), and the kernel wakes the thread's
//! word (`CHANGED`) when a clock reaches it, at most a scheduler tick late (as Linux's
//! tick-driven CPU timers), so nothing polls a CPU clock.
//!
//! **Signals.** `ITIMER_REAL` and the POSIX timers are not reloaded when they expire but
//! wait until their signal is taken (delivered, or returned by sigtimedwait), as Linux's
//! POSIX timers do: a pending expiry suppresses further ones, so however short the
//! interval, a timer costs at most one expiry per signal its process handles; the periods
//! missed meanwhile are skipped, and a POSIX timer's signal tells how many (`si_overrun`,
//! `timer_getoverrun`). A timer whose signal was dropped as ignored runs on once someone
//! can take it (a handler is installed, or sigtimedwait waits for it). A POSIX timer's
//! signal is an instance of its own (also of a standard signal that pends already), tagged
//! with the timer's setting: a signal of a setting that was changed or deleted since is
//! dropped (as Linux since 6.13). Each POSIX timer counts against RLIMIT_SIGPENDING (its
//! preallocated signal, as on Linux: EAGAIN beyond). `ITIMER_VIRTUAL` and `ITIMER_PROF`
//! reload when they expire (SIGVTALRM, SIGPROF), as Linux's.
//!
//! **Inheritance.** A fork's child has no timers; an execve keeps the interval timers and
//! deletes the POSIX timers, as on Linux.

use crate::heap;
use crate::process::{self, Pid, Table, PROCS};
use crate::signal::{self, SigInfo, TimerPost};
use crate::syscall;
use crate::usercopy;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const EINVAL: i64 = 22;
const EAGAIN: i64 = 11;
const ESRCH: i64 = 3;
const EOPNOTSUPP: i64 = 95;
const NSEC_PER_SEC: u64 = 1_000_000_000;
/// The scheduler's tick: what a CPU interval timer about to expire reports left (Linux's
/// TICK_NSEC).
const TICK_NS: u64 = 10_000_000;

const ITIMER_REAL: u64 = 0;
const ITIMER_VIRTUAL: u64 = 1;
const ITIMER_PROF: u64 = 2;
const SIGVTALRM: u32 = 26;
const SIGPROF: u32 = 27;
const TIMER_ABSTIME: u64 = 1;
const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD_ID: i32 = 4;

const SYS_GETITIMER: u64 = 36;
const SYS_ALARM: u64 = 37;
const SYS_SETITIMER: u64 = 38;
const SYS_TIMER_CREATE: u64 = 222;
const SYS_TIMER_SETTIME: u64 = 223;
const SYS_TIMER_GETTIME: u64 = 224;
const SYS_TIMER_GETOVERRUN: u64 = 225;
const SYS_TIMER_DELETE: u64 = 226;

/// The kernel's CPU clocks of a process or thread (`SYS_CPU_ALARM`): user time, and user
/// and system time.
const USER: usize = 0;
const TOTAL: usize = 1;

/// A process's `ITIMER_REAL`.
#[derive(Clone, Copy, Default)]
pub struct ITimer {
    /// When it expires next (monotonic nanoseconds; 0: off), and its interval.
    pub at: u64,
    pub every: u64,
    /// It expired and its SIGALRM pends: the next period starts when that is taken.
    pub parked: bool,
}

/// `ITIMER_VIRTUAL` or `ITIMER_PROF`: when it expires next on its process's clock (0:
/// off), and its interval.
#[derive(Clone, Copy, Default)]
struct CpuITimer {
    at: u64,
    every: u64,
}

/// The clock a POSIX timer counts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Clock {
    /// CLOCK_MONOTONIC, CLOCK_BOOTTIME (nothing suspends) and their alarm clock.
    Mono,
    /// CLOCK_REALTIME, CLOCK_TAI and the real-time alarm clock.
    Wall,
    /// A CPU clock (`USER`, `TOTAL`) of a process or a thread.
    Process(Pid, usize),
    Thread(Pid, usize),
}

/// How a POSIX timer's expiry is told: not at all, or a signal to the process or one of
/// its threads.
#[derive(Clone, Copy)]
enum Notify {
    None,
    Signal { signo: u32, value: u64, thread: Option<Pid> },
}

/// A POSIX timer.
#[derive(Clone, Copy)]
struct PosixTimer {
    clock: Clock,
    notify: Notify,
    /// When it expires next (0: disarmed): on its clock's scale, but a relative timer of
    /// the wall clock on the monotonic clock's (`wall` false).
    at: u64,
    wall: bool,
    every: u64,
    /// Its signal pends (or was dropped as ignored, `ignored`): it waits until it is taken.
    parked: bool,
    ignored: bool,
    /// The overrun of its last expiry taken.
    overrun: i32,
    /// Its setting, in its signal's tag (`signal::timer_tag`).
    serial: u32,
}

/// A process's timers.
#[derive(Default)]
pub struct Timers {
    real: ITimer,
    virt: CpuITimer,
    prof: CpuITimer,
    posix: BTreeMap<i32, PosixTimer>,
}

/// The alarms of a process's or thread's CPU clocks: what the kernel holds, and what the
/// timer thread's last look wants (deadlines per clock; 0: none).
#[derive(Clone, Copy, Default)]
pub struct CpuAlarm {
    armed: [u64; 2],
    want: [u64; 2],
}

/// Advances when a timer changed or the kernel's CPU alarm fired: the timer thread looks
/// again.
static CHANGED: AtomicU32 = AtomicU32::new(0);
/// The next setting of a POSIX timer (its serial).
static SERIAL: AtomicU32 = AtomicU32::new(1);

fn mono() -> u64 {
    syscall(SYS_CLOCK_READ, [CLOCK_MONO, 0, 0, 0, 0, 0]).max(0) as u64
}

fn wall() -> u64 {
    syscall(SYS_CLOCK_READ, [CLOCK_WALL, 0, 0, 0, 0, 0]).max(0) as u64
}

/// A timer was set or ended, or the wall clock was set: the timer thread computes its next
/// deadline again.
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

/// Keeps the earlier of `next` and `d` (0: none) in `next`.
fn earliest(next: &mut u64, d: u64) {
    if d != 0 && (*next == 0 || d < *next) {
        *next = d;
    }
}

/// CPU clock `clock` (`USER`, `TOTAL`) of process `pid`, or None for a process that ended.
fn process_cpu(t: &Table, pid: Pid, clock: usize) -> Option<u64> {
    let p = t.procs.get(&pid).filter(|p| p.zombie.is_none())?;
    let mut info = ProcInfo::default();
    if syscall(SYS_PROC_INFO, [p.handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]) < 0 {
        return None;
    }
    Some(if clock == USER { info.user_ns } else { info.user_ns + info.system_ns })
}

/// CPU clock `clock` of thread `tid`, or None for a thread that ended.
fn thread_cpu(t: &Table, tid: Pid, clock: usize) -> Option<u64> {
    let th = t.threads.get(&tid).filter(|th| !th.exited)?;
    if th.key == 0 {
        // (Not started yet: it has run for nothing.)
        return Some(0);
    }
    let mut info = ThreadInfo::default();
    if syscall(SYS_THREAD_INFO, [th.key, &mut info as *mut ThreadInfo as u64, 0, 0, 0, 0]) < 0 {
        return None;
    }
    Some(if clock == USER { info.user_ns } else { info.user_ns + info.system_ns })
}

/// The time of timer `tm`'s clock, on the scale of its `at` (None: its process or thread
/// ended, and the clock with it).
fn now_of(t: &Table, tm: &PosixTimer) -> Option<u64> {
    match tm.clock {
        Clock::Wall if tm.wall => Some(wall()),
        Clock::Wall | Clock::Mono => Some(mono()),
        Clock::Process(pid, clock) => process_cpu(t, pid, clock),
        Clock::Thread(tid, clock) => thread_cpu(t, tid, clock),
    }
}

// ------------------------------------------------------------------ the timer thread

/// The timer thread: sends the signals of the timers that expired, reports the timerfds'
/// expiries, arms the kernel's CPU alarms, trims the server's heap when that is asked for
/// and due (`heap::trim_if_due`), then sleeps until the next deadline or a change.
pub fn thread() -> ! {
    loop {
        let seen = CHANGED.load(Ordering::Acquire);
        crate::heap::trim_if_due();
        let (mut next, again) = scan(&mut PROCS.lock());
        earliest(&mut next, crate::timerfd::due());
        earliest(&mut next, crate::heap::trim_due());
        if again {
            // A CPU clock was past the deadline it was to be armed for.
            continue;
        }
        // (A dying service thread's wait ends at once; its next call ends it.)
        process::wait_word(&CHANGED, seen, next, false);
    }
}

/// One look at every process's timers: the expired ones fire, the CPU alarms are armed.
/// Returns the next deadline (monotonic; 0: none) and whether to look again at once. (It
/// allocates nothing: the service thread holds nothing for the programs' timers.)
fn scan(t: &mut Table) -> (u64, bool) {
    let (now, now_wall) = (mono(), wall());
    let mut next = 0;
    for p in t.procs.values_mut() {
        p.cpu.want = [0; 2];
    }
    for th in t.threads.values_mut() {
        th.cpu.want = [0; 2];
    }
    let mut from: Pid = 0;
    while let Some(pid) = t.procs.range(from..).map(|(&pid, _)| pid).next() {
        from = pid + 1;
        if t.procs[&pid].zombie.is_some() {
            continue;
        }
        scan_real(t, pid, now, &mut next);
        scan_cpu_itimers(t, pid);
        let mut id_from = 0;
        while let Some(id) = t.procs[&pid].timers.posix.range(id_from..).map(|(&id, _)| id).next() {
            scan_posix(t, pid, id, now, now_wall, &mut next);
            let Some(n) = id.checked_add(1) else { break };
            id_from = n;
        }
    }
    (next, arm_alarms(t))
}

/// Process `pid`'s `ITIMER_REAL`: SIGALRM if it expired.
fn scan_real(t: &mut Table, pid: Pid, now: u64, next: &mut u64) {
    let p = t.procs.get_mut(&pid).expect("listed");
    let it = &mut p.timers.real;
    if it.at == 0 || it.parked {
        return;
    }
    if it.at <= now {
        if it.every == 0 {
            it.at = 0;
        } else {
            it.parked = true;
        }
        signal::post_process(t, pid, SigInfo::kernel(signal::SIGALRM), false);
        return;
    }
    earliest(next, it.at);
}

/// Process `pid`'s `ITIMER_VIRTUAL` and `ITIMER_PROF`: their signals if they expired, and
/// the alarms of its clocks they want.
fn scan_cpu_itimers(t: &mut Table, pid: Pid) {
    for (clock, sig) in [(USER, SIGVTALRM), (TOTAL, SIGPROF)] {
        let it = cpu_itimer(&mut t.procs.get_mut(&pid).expect("listed").timers, clock);
        if it.at == 0 {
            continue;
        }
        let Some(now) = process_cpu(t, pid, clock) else { return };
        let it = cpu_itimer(&mut t.procs.get_mut(&pid).expect("listed").timers, clock);
        let at = it.at;
        if at <= now {
            it.at = if it.every == 0 { 0 } else { next_period(at, it.every, now) };
            signal::post_process(t, pid, SigInfo::kernel(sig), false);
        }
        let p = t.procs.get_mut(&pid).expect("listed");
        let at = cpu_itimer(&mut p.timers, clock).at;
        earliest(&mut p.cpu.want[clock], at);
    }
}

fn cpu_itimer(timers: &mut Timers, clock: usize) -> &mut CpuITimer {
    if clock == USER { &mut timers.virt } else { &mut timers.prof }
}

/// POSIX timer `id` of process `pid`: its signal if it expired, and its next deadline (in
/// `next`, or as the want of the CPU clock it counts).
fn scan_posix(t: &mut Table, pid: Pid, id: i32, now: u64, now_wall: u64, next: &mut u64) {
    let tm = &t.procs[&pid].timers.posix[&id];
    if tm.at == 0 || tm.parked || matches!(tm.notify, Notify::None) {
        return;
    }
    let Some(clock_now) = now_of(t, tm) else { return };
    if tm.at <= clock_now {
        fire(t, pid, id, clock_now);
    }
    let tm = &t.procs[&pid].timers.posix[&id];
    if tm.at == 0 || tm.parked {
        return;
    }
    let at = tm.at;
    match tm.clock {
        Clock::Mono => earliest(next, at),
        Clock::Wall if !tm.wall => earliest(next, at),
        // The monotonic time the wall clock reaches it at, as it runs now.
        Clock::Wall => earliest(next, now.saturating_add(at.saturating_sub(now_wall)).max(1)),
        Clock::Process(q, clock) => {
            if let Some(p) = t.procs.get_mut(&q) {
                earliest(&mut p.cpu.want[clock], at);
            }
        }
        Clock::Thread(tid, clock) => {
            if let Some(th) = t.threads.get_mut(&tid) {
                earliest(&mut th.cpu.want[clock], at);
            }
        }
    }
}

/// POSIX timer `id` of process `pid` expired (its clock is at `now`): its signal goes, and
/// it waits until that is taken.
fn fire(t: &mut Table, pid: Pid, id: i32, now: u64) {
    let tm = &t.procs[&pid].timers.posix[&id];
    let Notify::Signal { signo, value, thread } = tm.notify else { return };
    let (tag, at, every) = (signal::timer_tag(id, tm.serial), tm.at, tm.every);
    let posted = signal::post_timer(t, pid, thread, SigInfo::timer(signo, id, value), tag);
    let Some(tm) = t.procs.get_mut(&pid).and_then(|p| p.timers.posix.get_mut(&id)) else { return };
    match posted {
        TimerPost::Queued => tm.parked = true,
        TimerPost::Ignored => {
            tm.parked = true;
            tm.ignored = true;
        }
        TimerPost::Lost => {}
    }
    if every == 0 {
        tm.at = 0;
    } else if !tm.parked {
        tm.at = next_period(at, every, now);
    }
}

/// Arms the kernel's CPU alarms whose wanted deadlines changed. Whether a clock was past
/// its deadline already (then the timer thread looks again at once).
fn arm_alarms(t: &mut Table) -> bool {
    let word = &CHANGED as *const AtomicU32 as u64;
    let mut again = false;
    let mut arm = |cpu: &mut CpuAlarm, target: u64, thread: bool| {
        for clock in [USER, TOTAL] {
            if cpu.want[clock] == cpu.armed[clock] {
                continue;
            }
            let flags = if thread { CPU_ALARM_THREAD } else { 0 } | if clock == USER { CPU_ALARM_USER } else { 0 };
            let r = syscall(SYS_CPU_ALARM, [target, flags, cpu.want[clock], word, 0, 0]);
            cpu.armed[clock] = if r == 0 { cpu.want[clock] } else { 0 };
            again |= r == 1;
        }
    };
    for p in t.procs.values_mut() {
        if p.zombie.is_none() {
            arm(&mut p.cpu, p.handle, false);
        }
    }
    for th in t.threads.values_mut() {
        if th.key != 0 && !th.exited {
            arm(&mut th.cpu, th.key, true);
        }
    }
    again
}

// ------------------------------------------------------------------ signals taken

/// SIGALRM of process `pid` was taken: a parked `ITIMER_REAL` runs on with its next
/// period.
pub fn taken(t: &mut Table, pid: Pid) {
    let Some(p) = t.procs.get_mut(&pid) else { return };
    let it = &mut p.timers.real;
    if it.parked {
        it.parked = false;
        it.at = next_period(it.at, it.every, mono());
        changed();
    }
}

/// The signal of a POSIX timer of process `pid` (tag `tag`) was taken: the timer runs on
/// with its next period. Returns the expiries it missed meanwhile (its overrun), or None
/// for a signal of a setting that is gone (dropped).
pub fn taken_posix(t: &mut Table, pid: Pid, tag: u64) -> Option<i32> {
    let (id, serial) = signal::timer_of(tag);
    let tm = t.procs.get(&pid)?.timers.posix.get(&id).filter(|tm| tm.serial == serial)?;
    let now = if tm.parked && tm.every != 0 { now_of(t, tm) } else { None };
    let tm = t.procs.get_mut(&pid)?.timers.posix.get_mut(&id)?;
    let mut overrun = 0;
    if tm.parked {
        tm.parked = false;
        if let Some(now) = now {
            let next = next_period(tm.at, tm.every, now);
            // The periods that ended since the one it was sent for.
            overrun = ((next - tm.at) / tm.every).saturating_sub(1).min(i32::MAX as u64) as i32;
            tm.at = next;
        }
        changed();
    }
    tm.overrun = overrun;
    Some(overrun)
}

/// Someone can take the signals of `mask` of process `pid` again (a handler, sigtimedwait):
/// its timers whose signal was dropped as ignored run on.
pub fn handled(t: &mut Table, pid: Pid, mask: u64) {
    if mask & signal::bit(signal::SIGALRM) != 0 {
        let pending = t.procs.get(&pid).is_some_and(|p| p.sig.shared.set & signal::bit(signal::SIGALRM) != 0);
        if !pending {
            taken(t, pid);
        }
    }
    let wanted = |_: &Table, _: i32, tm: &PosixTimer| tm.ignored && matches!(tm.notify, Notify::Signal { signo, .. } if mask & signal::bit(signo) != 0);
    resume(t, pid, wanted);
}

/// Signals of process `pid`'s timers were dropped while pending (a flush: SIGCONT, a
/// signal now ignored): the timers that waited for them run on.
pub fn unpark_lost(t: &mut Table, pid: Pid) {
    let lost = |t: &Table, id: i32, tm: &PosixTimer| !tm.ignored && !signal::timer_signal_queued(t, pid, signal::timer_tag(id, tm.serial));
    resume(t, pid, lost);
}

/// The parked timers of process `pid` that `select` picks run on with their next period.
fn resume(t: &mut Table, pid: Pid, select: impl Fn(&Table, i32, &PosixTimer) -> bool) {
    let mut id_from = 0;
    let mut any = false;
    while let Some(id) = t.procs.get(&pid).and_then(|p| p.timers.posix.range(id_from..).find(|&(&id, tm)| tm.parked && select(t, id, tm)).map(|(&id, _)| id)) {
        let tm = &t.procs[&pid].timers.posix[&id];
        let now = if tm.every != 0 { now_of(t, tm) } else { None };
        let tm = t.procs.get_mut(&pid).and_then(|p| p.timers.posix.get_mut(&id)).expect("found");
        tm.parked = false;
        tm.ignored = false;
        if let Some(now) = now {
            tm.at = next_period(tm.at, tm.every, now);
        }
        any = true;
        let Some(n) = id.checked_add(1) else { break };
        id_from = n;
    }
    if any {
        changed();
    }
}

// ------------------------------------------------------------------ ends

/// Deletes every POSIX timer of process `pid` (lock held): their pending signals go, and
/// what they were counted as.
fn delete_all(t: &mut Table, pid: Pid) {
    let Some(p) = t.procs.get_mut(&pid) else { return };
    let timers = core::mem::take(&mut p.timers.posix);
    for (&id, tm) in &timers {
        signal::remove_timer_signal(t, pid, signal::timer_tag(id, tm.serial));
    }
    t.rt_queued = t.rt_queued.saturating_sub(timers.len());
    changed();
}

/// Process `pid` exec'd (past its point of no return): its POSIX timers are deleted.
pub fn exec(t: &mut Table, pid: Pid) {
    delete_all(t, pid);
}

/// Process `pid` became a zombie: its timers go.
pub fn process_ended(t: &mut Table, pid: Pid) {
    delete_all(t, pid);
    if let Some(p) = t.procs.get_mut(&pid) {
        p.timers = Timers::default();
    }
}

// ------------------------------------------------------------------ system calls

/// The result of a timer call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let result = match s.rax {
        SYS_ALARM => {
            let (old, _) = set_real((s.rdi as u32 as u64).saturating_mul(NSEC_PER_SEC), 0);
            Ok(old.div_ceil(NSEC_PER_SEC) as i64)
        }
        SYS_SETITIMER => setitimer(s.rdi, s.rsi, s.rdx),
        SYS_GETITIMER => getitimer(s.rdi, s.rsi),
        SYS_TIMER_CREATE => timer_create(s.rdi, s.rsi, s.rdx),
        SYS_TIMER_SETTIME => timer_settime(s.rdi as i32, s.rsi, s.rdx, s.r10),
        SYS_TIMER_GETTIME => timer_gettime(s.rdi as i32, s.rsi),
        SYS_TIMER_GETOVERRUN => with_timer(s.rdi as i32, |_, tm| Ok(tm.overrun as i64)),
        SYS_TIMER_DELETE => timer_delete(s.rdi as i32),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// `ITIMER_REAL`'s setting: (time left, interval) in nanoseconds.
fn real_left(it: &ITimer) -> (u64, u64) {
    if it.at == 0 {
        return (0, 0);
    }
    let now = mono();
    // A parked timer reports the period it would be in had it run on.
    let at = if it.parked { next_period(it.at, it.every, now) } else { it.at };
    // An armed timer never reports 0 left, which would mean "off".
    (at.saturating_sub(now).max(1000), it.every)
}

/// Sets the calling process's `ITIMER_REAL` to `value` (0: off) and `interval`
/// nanoseconds; returns the old setting.
fn set_real(value: u64, interval: u64) -> (u64, u64) {
    let pid = crate::local::pid();
    let old = {
        let mut t = PROCS.lock();
        let Some(p) = t.procs.get_mut(&pid) else { return (0, 0) };
        let old = real_left(&p.timers.real);
        p.timers.real = if value == 0 { ITimer::default() } else { ITimer { at: mono().saturating_add(value), every: interval, parked: false } };
        old
    };
    changed();
    old
}

/// `ITIMER_VIRTUAL`'s or `ITIMER_PROF`'s setting: (time left, interval).
fn cpu_left(it: &CpuITimer, now: u64) -> (u64, u64) {
    if it.at == 0 {
        return (0, 0);
    }
    // About to expire: a tick, as Linux says.
    let left = if it.at <= now { TICK_NS } else { it.at - now };
    (left, it.every)
}

/// The calling process's CPU clock `clock`.
fn own_cpu(clock: usize) -> u64 {
    let mut info = ProcInfo::default();
    syscall(SYS_PROC_INFO, [0, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
    if clock == USER { info.user_ns } else { info.user_ns + info.system_ns }
}

/// Sets the calling process's `ITIMER_VIRTUAL` or `ITIMER_PROF` (`clock`) to `value` (0:
/// off) and `interval` nanoseconds of its clock; returns the old setting.
fn set_cpu(clock: usize, value: u64, interval: u64) -> (u64, u64) {
    let pid = crate::local::pid();
    let now = own_cpu(clock);
    let old = {
        let mut t = PROCS.lock();
        let Some(p) = t.procs.get_mut(&pid) else { return (0, 0) };
        let it = cpu_itimer(&mut p.timers, clock);
        let old = cpu_left(it, now);
        *it = if value == 0 { CpuITimer::default() } else { CpuITimer { at: now.saturating_add(value), every: interval } };
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
    if which > ITIMER_PROF {
        return Err(EINVAL);
    }
    let (interval, value) = if new == 0 { (0, 0) } else { read_itimerval(new)? };
    let was = match which {
        ITIMER_REAL => set_real(value, interval),
        ITIMER_VIRTUAL => set_cpu(USER, value, interval),
        _ => set_cpu(TOTAL, value, interval),
    };
    write_itimerval(old, was)?;
    Ok(0)
}

fn getitimer(which: u64, cur: u64) -> Result<i64, i64> {
    if which > ITIMER_PROF {
        return Err(EINVAL);
    }
    let pid = crate::local::pid();
    let setting = match which {
        ITIMER_REAL => PROCS.lock().procs.get(&pid).map_or((0, 0), |p| real_left(&p.timers.real)),
        _ => {
            let clock = if which == ITIMER_VIRTUAL { USER } else { TOTAL };
            let now = own_cpu(clock);
            PROCS.lock().procs.get_mut(&pid).map_or((0, 0), |p| cpu_left(cpu_itimer(&mut p.timers, clock), now))
        }
    };
    write_itimerval(cur, setting)?;
    Ok(0)
}

/// The clock of Linux's clock id `id` for a timer of the calling thread: the wall and
/// monotonic clocks and their aliases, the caller's own CPU clocks, or (Linux's encoding,
/// `!pid << 3`, bit 2 a thread, the low two bits what is counted) the CPU clock of a
/// process of the instance or of a thread of the caller's process. EOPNOTSUPP for a clock
/// without timers (the raw and coarse ones).
fn timer_clock(t: &Table, id: u64) -> Result<Clock, i64> {
    let (tid, pid) = process::me();
    Ok(match id as i32 {
        0 | 8 | 11 => Clock::Wall,
        1 | 7 | 9 => Clock::Mono,
        2 => Clock::Process(pid, TOTAL),
        3 => Clock::Thread(tid, TOTAL),
        4..=6 => return Err(EOPNOTSUPP),
        id if id < 0 => {
            let target = !(id >> 3) as u32;
            let clock = match id & 3 {
                // CPUCLOCK_VIRT; CPUCLOCK_PROF and CPUCLOCK_SCHED are both all of the time.
                1 => USER,
                3 => return Err(EINVAL),
                _ => TOTAL,
            };
            if id & 4 != 0 {
                let tid = if target == 0 { tid } else { target };
                if !t.threads.get(&tid).is_some_and(|th| th.pid == pid && !th.exited) {
                    return Err(EINVAL);
                }
                Clock::Thread(tid, clock)
            } else {
                let pid = if target == 0 { pid } else { target };
                if !t.procs.get(&pid).is_some_and(|p| p.zombie.is_none()) {
                    return Err(EINVAL);
                }
                Clock::Process(pid, clock)
            }
        }
        _ => return Err(EINVAL),
    })
}

/// timer_create(clock, sevp, timerid): a disarmed timer of the calling process; its id
/// (the lowest free) at `timerid`.
fn timer_create(clock: u64, sevp: u64, timerid: u64) -> Result<i64, i64> {
    // struct sigevent: sigev_value, sigev_signo, sigev_notify, then the thread id.
    let sev: Option<[u64; 8]> = if sevp != 0 { Some(usercopy::read(sevp)?) } else { None };
    let pid = crate::local::pid();
    let id = {
        let mut t = PROCS.lock();
        let clock = timer_clock(&t, clock)?;
        let notify = match sev {
            None => None,
            Some(w) => {
                let signo = w[1] as u32;
                let valid = (1..=signal::NSIG).contains(&signo);
                Some(match (w[1] >> 32) as i32 {
                    SIGEV_NONE => Notify::None,
                    SIGEV_SIGNAL if valid => Notify::Signal { signo, value: w[0], thread: None },
                    SIGEV_THREAD_ID if valid => {
                        let target = w[2] as u32;
                        if !t.threads.get(&target).is_some_and(|th| th.pid == pid && !th.exited) {
                            return Err(EINVAL);
                        }
                        Notify::Signal { signo, value: w[0], thread: Some(target) }
                    }
                    _ => return Err(EINVAL),
                })
            }
        };
        // Its preallocated signal counts against RLIMIT_SIGPENDING.
        let p = t.procs.get(&pid).ok_or(ESRCH)?;
        if t.rt_queued as u64 >= p.limits.soft(crate::ids::RLIMIT_SIGPENDING) {
            return Err(EAGAIN);
        }
        // The lowest free id.
        let mut id = 0i32;
        for &used in p.timers.posix.keys() {
            if used != id {
                break;
            }
            id = id.checked_add(1).ok_or(EAGAIN)?;
        }
        heap::preload_map::<i32, PosixTimer>()?;
        // Without a sigevent: SIGALRM with the timer's id as its value.
        let notify = notify.unwrap_or(Notify::Signal { signo: signal::SIGALRM, value: id as u64, thread: None });
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let tm = PosixTimer { clock, notify, at: 0, wall: false, every: 0, parked: false, ignored: false, overrun: 0, serial };
        t.procs.get_mut(&pid).expect("checked").timers.posix.insert(id, tm);
        t.rt_queued += 1;
        id
    };
    if let Err(e) = usercopy::write(timerid, &id) {
        let _ = timer_delete(id);
        return Err(e);
    }
    Ok(0)
}

/// Runs `f` on a copy of the calling process's timer `id` (EINVAL for none), with the lock
/// held; the copy is the timer afterwards.
fn with_timer<R>(id: i32, f: impl FnOnce(&mut Table, &mut PosixTimer) -> Result<R, i64>) -> Result<R, i64> {
    let pid = crate::local::pid();
    let mut t = PROCS.lock();
    let mut tm = *t.procs.get(&pid).and_then(|p| p.timers.posix.get(&id)).ok_or(EINVAL)?;
    let r = f(&mut t, &mut tm);
    if let Some(slot) = t.procs.get_mut(&pid).and_then(|p| p.timers.posix.get_mut(&id)) {
        *slot = tm;
    }
    r
}

/// Timer `tm`'s setting: (time left, interval) in nanoseconds.
fn posix_left(t: &Table, tm: &PosixTimer) -> (u64, u64) {
    if tm.at == 0 {
        return (0, tm.every);
    }
    let Some(now) = now_of(t, tm) else { return (0, tm.every) };
    // A timer waiting for its signal to be taken, or one nothing is told of, reports the
    // period it is in.
    let at = if tm.parked || matches!(tm.notify, Notify::None) { next_period(tm.at, tm.every, now) } else { tm.at };
    let left = at.saturating_sub(now);
    // An armed timer whose signal is about to go never reports 0 left (Linux: 1 ns).
    let left = if left == 0 && !matches!(tm.notify, Notify::None) { 1 } else { left };
    (left, tm.every)
}

/// struct itimerspec: (interval, value) as two timespecs, in nanoseconds.
pub fn read_itimerspec(addr: u64) -> Result<(u64, u64), i64> {
    let [isec, insec, vsec, vnsec]: [i64; 4] = usercopy::read(addr)?;
    let valid = |sec: i64, nsec: i64| sec >= 0 && (0..NSEC_PER_SEC as i64).contains(&nsec);
    if !valid(isec, insec) || !valid(vsec, vnsec) {
        return Err(EINVAL);
    }
    let ns = |sec: i64, nsec: i64| (sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(nsec as u64);
    Ok((ns(isec, insec), ns(vsec, vnsec)))
}

pub fn write_itimerspec(addr: u64, (value, interval): (u64, u64)) -> Result<(), i64> {
    if addr == 0 {
        return Ok(());
    }
    usercopy::write(addr, &[interval / NSEC_PER_SEC, interval % NSEC_PER_SEC, value / NSEC_PER_SEC, value % NSEC_PER_SEC])
}

/// timer_settime(id, flags, new, old): arms timer `id` for the time in `new` (relative, or
/// with TIMER_ABSTIME a time of its clock; 0: disarms) and its interval; the old setting
/// at `old`. A signal of the old setting still pending is dropped.
fn timer_settime(id: i32, flags: u64, new: u64, old: u64) -> Result<i64, i64> {
    let (interval, value) = read_itimerspec(new)?;
    let pid = crate::local::pid();
    let absolute = flags & TIMER_ABSTIME != 0;
    let was = with_timer(id, |t, tm| {
        let was = posix_left(t, tm);
        // Where it starts: an absolute time of its clock, or a span from now (the
        // monotonic clock's for the wall clock: setting the clock does not move it).
        let at = match tm.clock {
            _ if value == 0 => 0,
            _ if absolute => value,
            Clock::Wall | Clock::Mono => mono().saturating_add(value),
            Clock::Process(..) | Clock::Thread(..) => now_of(t, tm).ok_or(ESRCH)?.saturating_add(value),
        };
        signal::remove_timer_signal(t, pid, signal::timer_tag(id, tm.serial));
        *tm = PosixTimer {
            at,
            wall: at != 0 && absolute && tm.clock == Clock::Wall,
            every: if at == 0 { 0 } else { interval },
            parked: false,
            ignored: false,
            overrun: 0,
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            ..*tm
        };
        Ok(was)
    })?;
    changed();
    write_itimerspec(old, was)?;
    Ok(0)
}

fn timer_gettime(id: i32, cur: u64) -> Result<i64, i64> {
    let setting = with_timer(id, |t, tm| Ok(posix_left(t, tm)))?;
    write_itimerspec(cur, setting)?;
    Ok(0)
}

fn timer_delete(id: i32) -> Result<i64, i64> {
    let pid = crate::local::pid();
    {
        let mut t = PROCS.lock();
        let tm = t.procs.get_mut(&pid).and_then(|p| p.timers.posix.remove(&id)).ok_or(EINVAL)?;
        signal::remove_timer_signal(&mut t, pid, signal::timer_tag(id, tm.serial));
        t.rt_queued = t.rt_queued.saturating_sub(1);
    }
    changed();
    Ok(0)
}
