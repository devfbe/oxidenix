//! POSIX signals for processes with threads, following Linux.
//!
//! Handlers, the process's pending set (signals sent to the process) and
//! the interval timer belong to the thread group; each thread has its own
//! mask and pending set (signals sent to the thread: tkill, faults). A
//! process signal is taken by whichever thread does not block it. Default
//! actions act on the whole process: a fatal signal ends every thread (a
//! group exit), a stop signal stops every thread (a group stop) until
//! SIGCONT. Delivery happens on every return to user space.
//!
//! Lock order: group info → group signals → thread signals → wake_lock.

use super::address_space::USER_END;
use super::errno::*;
use super::syscall::Frame;
use super::sched::{self, current, try_wake};
use super::task::{State, Task, ThreadGroup};
use super::{uaccess, Pid};
use crate::interrupts::gdt;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGSEGV: u32 = 11;
pub const SIGALRM: u32 = 14;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
const SIGURG: u32 = 23;
const SIGWINCH: u32 = 28;
pub const NSIG: u32 = 64;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;

/// Kernel `struct sigaction` layout used by rt_sigaction.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SigAction {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

/// How a process is ending, if it is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GroupExit {
    None,
    /// exit_group or a fatal signal: every thread exits; the status of
    /// the process.
    Exiting(i32),
    /// The thread with this id execs: every other thread exits.
    Exec(Pid),
}

/// A process's signal state.
#[derive(Clone)]
pub struct GroupSignals {
    actions: [SigAction; NSIG as usize],
    /// Signals sent to the process.
    pending: u64,
    /// ITIMER_REAL: when the next SIGALRM is due (0: off) and the reload
    /// interval, in nanoseconds of the monotonic clock.
    alarm_at: u64,
    alarm_every: u64,
    /// The interval timer expired and its SIGALRM pends: the next period
    /// is armed when the signal is taken (see `alarm_expired`).
    alarm_parked: bool,
    /// The stop signal of a group stop in progress (0: none).
    stopping: u32,
    pub exit: GroupExit,
}

impl Default for GroupSignals {
    fn default() -> Self {
        GroupSignals {
            actions: [SigAction::default(); NSIG as usize],
            pending: 0,
            alarm_at: 0,
            alarm_every: 0,
            alarm_parked: false,
            stopping: 0,
            exit: GroupExit::None,
        }
    }
}

/// A thread's signal state.
#[derive(Clone, Default)]
pub struct ThreadSignals {
    pub mask: u64,
    /// Signals sent to this thread.
    pending: u64,
    /// Signals a sigtimedwait is waiting for (they are usually blocked, so
    /// they would not wake the thread otherwise).
    waiting_for: u64,
    /// A group stop asks this thread to stop.
    stop: bool,
    /// The mask to put back on the way to user space, after a call that
    /// waited with a temporary one (see `with_mask`).
    restore_mask: Option<u64>,
}

fn bit(sig: u32) -> u64 {
    1 << (sig - 1)
}

/// SIGKILL and SIGSTOP can be neither caught, ignored nor blocked.
const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));

fn default_ignored(sig: u32) -> bool {
    matches!(sig, SIGCHLD | SIGCONT | SIGURG | SIGWINCH)
}

fn is_stop(sig: u32) -> bool {
    matches!(sig, SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
}

const STOP_SIGNALS: u64 = (1 << (SIGSTOP - 1)) | (1 << (SIGTSTP - 1)) | (1 << (SIGTTIN - 1)) | (1 << (SIGTTOU - 1));

/// Wait status reported for a stopped (`sig << 8 | 0x7f`) or continued child.
pub const CONTINUED_STATUS: i32 = 0xffff;

pub fn stopped_status(sig: u32) -> i32 {
    ((sig as i32) << 8) | 0x7f
}

impl GroupSignals {
    /// Whether a group stop is under way (or done): what SIGCONT counts as stopped.
    pub fn stopping(&self) -> bool {
        self.stopping != 0
    }

    fn ignored(&self, sig: u32) -> bool {
        let handler = self.actions[sig as usize - 1].handler;
        handler == SIG_IGN || (handler == SIG_DFL && default_ignored(sig))
    }

    /// Pending signals a thread with `t` would act on.
    fn deliverable(&self, t: &ThreadSignals) -> u64 {
        let mut set = (t.pending | self.pending) & !(t.mask & !UNBLOCKABLE);
        for sig in 1..=NSIG {
            if set & bit(sig) != 0 && self.ignored(sig) {
                set &= !bit(sig);
            }
        }
        set
    }

    /// Fork keeps the actions, but not pending signals, timers or stops.
    pub fn for_child(&self) -> GroupSignals {
        GroupSignals { actions: self.actions, ..GroupSignals::default() }
    }

    /// Whether a group stop is in progress (new threads join it).
    pub fn joins_stop(&self) -> bool {
        self.stopping != 0
    }

    /// Exec resets caught signals to their default action.
    pub fn reset_on_exec(&mut self) {
        for a in self.actions.iter_mut() {
            if a.handler != SIG_IGN {
                *a = SigAction::default();
            }
        }
    }
}

impl ThreadSignals {
    pub fn set_stop(&mut self) {
        self.stop = true;
    }

    /// A new thread or process keeps the creator's mask, nothing else.
    pub fn inherit(&self) -> ThreadSignals {
        ThreadSignals { mask: self.mask, ..ThreadSignals::default() }
    }
}

/// Whether the calling process ignores `sig` (`restricted::SIGNAL_IGNORED`)
/// and whether the calling thread blocks it (`SIGNAL_BLOCKED`): for the Linux
/// server's terminals, whose background reads and writes depend on it
/// (SIGTTIN, SIGTTOU), until signals are the server's (R8).
pub fn state(sig: u32) -> u64 {
    if sig == 0 {
        // Whether a signal (or a stop) waits for the calling thread: a long call
        // returns what it did (Linux's signal_pending).
        return if interrupted() { restricted::SIGNAL_PENDING } else { 0 };
    }
    if sig > NSIG {
        return 0;
    }
    let me = current();
    let ignored = me.group.sig.lock().actions[sig as usize - 1].handler == SIG_IGN;
    let blocked = me.sig.lock().mask & bit(sig) != 0;
    (if ignored { restricted::SIGNAL_IGNORED } else { 0 }) | if blocked { restricted::SIGNAL_BLOCKED } else { 0 }
}

/// Sends `sig` (from a terminal: no permission checks) to every process of
/// group `pgid` in Linux server instance `instance`; whether there was one.
pub fn send_pgrp_in(instance: u64, pgid: Pid, sig: u32) -> bool {
    let mut targets: Vec<Arc<ThreadGroup>> = Vec::new();
    {
        let table = sched::TABLE.lock();
        for g in table.groups.values() {
            if g.tgid != 0
                && !g.privileged.load(Ordering::Relaxed)
                && g.instance.load(Ordering::Acquire) == instance
                && g.info.lock().pgid == pgid
            {
                targets.push(g.clone());
            }
        }
    }
    for g in &targets {
        post(g, None, sig);
    }
    !targets.is_empty()
}

/// Whether a blocking syscall of the current thread should return EINTR:
/// a signal to act on, or a group stop to join.
pub fn interrupted() -> bool {
    let me = current();
    let g = me.group.sig.lock();
    let t = me.sig.lock();
    t.stop || g.deliverable(&t) != 0
}

/// Whether SIGKILL is pending for the current thread (ends waits that
/// ignore other signals).
pub fn killed() -> bool {
    let me = current();
    let g = me.group.sig.lock();
    (me.sig.lock().pending | g.pending) & bit(SIGKILL) != 0
}

/// Whether the current thread is about to die: SIGKILL is pending or its
/// process is exiting (ends waits that nothing else may interrupt).
pub fn dying() -> bool {
    killed() || current().group.sig.lock().exit != GroupExit::None
}

/// Raises `sig` for a fault of the current thread (a CPU exception). Such
/// a signal can be neither blocked nor ignored: as on Linux, the action is
/// reset to the default (terminate) in that case. Returns whether the
/// process will die of it (no handler).
pub fn force(sig: u32) -> bool {
    let me = current();
    let mut g = me.group.sig.lock();
    let mut t = me.sig.lock();
    let blocked = t.mask & bit(sig) != 0;
    let action = &mut g.actions[sig as usize - 1];
    let default = if blocked || action.handler == SIG_IGN {
        *action = SigAction::default();
        true
    } else {
        action.handler == SIG_DFL
    };
    t.mask &= !bit(sig);
    t.pending |= bit(sig);
    default
}

/// Wakes a parent blocked in wait4 and sends it SIGCHLD.
fn notify_parent(ppid: Pid) {
    super::notify_parent(ppid);
    send(ppid, SIGCHLD);
}

/// Gets `t` to look at its signals: wakes it from an interruptible sleep,
/// or, if it runs on another CPU, interrupts it there (it checks on its
/// way back to user space).
pub fn kick(t: &Arc<Task>) {
    if try_wake(t, State::Sleeping) {
        return;
    }
    let cpu = t.last_cpu.load(Ordering::Relaxed);
    if t.on_cpu.load(Ordering::Acquire) && cpu != crate::smp::cpu().index {
        if let Some(c) = crate::smp::by_index(cpu) {
            crate::interrupts::apic::ipi::send_vector(c.apic_id(), crate::interrupts::apic::ipi::RESCHEDULE_VECTOR);
        }
    }
}

/// Queues `sig` for `group` (process-directed) or for one of its threads,
/// and wakes a thread that will act on it. SIGCONT resumes a stopped
/// process right away; SIGKILL wakes stopped threads to die. Never
/// allocates (it runs in interrupt context for Ctrl+C).
fn post(group: &Arc<ThreadGroup>, thread: Option<&Arc<Task>>, sig: u32) {
    if group.tgid == 0 || sig == 0 || sig > NSIG {
        return;
    }
    let mut continued_parent = None;
    {
        let mut info = group.info.lock();
        if info.threads.is_empty() {
            return;
        }
        let mut g = group.sig.lock();
        if sig == SIGCONT {
            let stopped = g.stopping != 0 || info.threads.iter().any(|t| t.state() == State::Stopped);
            g.pending &= !STOP_SIGNALS;
            g.stopping = 0;
            for t in info.threads.iter() {
                let mut ts = t.sig.lock();
                ts.pending &= !STOP_SIGNALS;
                ts.stop = false;
                drop(ts);
                try_wake(t, State::Stopped);
            }
            if stopped {
                info.report = Some(CONTINUED_STATUS);
                continued_parent = Some(info.ppid);
            }
        } else if is_stop(sig) {
            g.pending &= !bit(SIGCONT);
            for t in info.threads.iter() {
                t.sig.lock().pending &= !bit(SIGCONT);
            }
        }
        if sig == SIGKILL {
            for t in info.threads.iter() {
                try_wake(t, State::Stopped);
            }
        }
        match thread {
            Some(t) => t.sig.lock().pending |= bit(sig),
            None => g.pending |= bit(sig),
        }
        // A thread that takes it: one waiting for it in sigtimedwait (even
        // if it is ignored), else the target or the first thread that does
        // not block it.
        let ignored = g.ignored(sig);
        let takes = |t: &Arc<Task>| {
            let ts = t.sig.lock();
            ts.waiting_for & bit(sig) != 0 || (!ignored && (ts.mask & bit(sig) == 0 || bit(sig) & UNBLOCKABLE != 0))
        };
        match thread {
            Some(t) => {
                if takes(t) {
                    kick(t);
                }
            }
            None => {
                if let Some(t) = info.threads.iter().find(|t| takes(t)) {
                    kick(t);
                }
            }
        }
    }
    if let Some(ppid) = continued_parent {
        notify_parent(ppid);
    }
}

/// Hands the process's pending signals to another thread that can take
/// them, after a thread blocked them or left (the thread first kicked may
/// never deliver them).
pub fn retarget(group: &Arc<ThreadGroup>) {
    let info = group.info.lock();
    let g = group.sig.lock();
    if g.pending == 0 {
        return;
    }
    let me = current();
    let taker = info.threads.iter().filter(|t| !core::ptr::eq(&***t, me)).find(|t| {
        let ts = t.sig.lock();
        let open = g.pending & !(ts.mask & !UNBLOCKABLE);
        (1..=NSIG).any(|s| open & bit(s) != 0 && !g.ignored(s)) || g.pending & ts.waiting_for != 0
    });
    if let Some(t) = taker {
        kick(t);
    }
}

/// Sends SIGKILL to one thread (a group exit or exec ends the others).
pub fn kill_thread(t: &Arc<Task>) {
    post(&t.group.clone(), Some(t), SIGKILL);
}

/// The process with id `pid`, or the process of the thread with that id.
fn group_of(pid: Pid) -> Option<Arc<ThreadGroup>> {
    let table = sched::TABLE.lock();
    table.groups.get(&pid).cloned().or_else(|| table.tasks.get(&pid).map(|t| t.group.clone()))
}

/// Sends `sig` to process `pid` (from the kernel: no permission checks).
pub fn send(pid: Pid, sig: u32) {
    // The table lock must be released before posting: SIGCONT notifies
    // the parent, which sends again.
    if let Some(g) = group_of(pid) {
        post(&g, None, sig);
    }
}

/// Sends `sig` to the process `group` (from the kernel: no permission checks).
pub fn send_to(group: &Arc<ThreadGroup>, sig: u32) {
    post(group, None, sig);
}

/// kill(2). Privileged servers are protected like init on Linux: only the
/// kernel may signal them. A direct kill fails with EPERM, group and
/// broadcast kills skip them.
pub fn kill(pid: i64, sig: u64) -> SysResult {
    if sig > NSIG as u64 {
        return Err(EINVAL);
    }
    let sig = sig as u32;
    let me = current();
    let (my_pid, my_pgid) = (me.tgid(), me.group.info.lock().pgid);
    let mut targets: heapless::Vec<Arc<ThreadGroup>, { sched::MAX_PROCS }> = heapless::Vec::new();
    if pid > 0 {
        let g = group_of(pid as Pid).filter(|g| g.tgid != 0 && g.info.lock().exit_status.is_none()).ok_or(ESRCH)?;
        if my_pid != 0 && g.privileged.load(Ordering::Relaxed) {
            return Err(EPERM);
        }
        let _ = targets.push(g);
    } else {
        let table = sched::TABLE.lock();
        for g in table.groups.values() {
            if g.tgid == 0 || (my_pid != 0 && g.privileged.load(Ordering::Relaxed)) {
                continue;
            }
            let info = g.info.lock();
            if info.exit_status.is_some() {
                continue;
            }
            let selected = match pid {
                0 => info.pgid == my_pgid,
                -1 => g.tgid != 1 && g.tgid != my_pid,
                p => info.pgid == p.unsigned_abs(),
            };
            drop(info);
            if selected {
                let _ = targets.push(g.clone());
            }
        }
    }
    if targets.is_empty() {
        return Err(ESRCH);
    }
    if sig != 0 {
        for g in &targets {
            post(g, None, sig);
        }
    }
    Ok(0)
}

/// tgkill(tgid, tid, sig) and tkill (`tgid` None): a signal for one thread.
pub fn tgkill(tgid: Option<i64>, tid: i64, sig: u64) -> SysResult {
    if sig > NSIG as u64 || tid <= 0 || tgid.is_some_and(|g| g <= 0) {
        return Err(EINVAL);
    }
    let t = sched::TABLE.lock().tasks.get(&(tid as Pid)).cloned().ok_or(ESRCH)?;
    if tgid.is_some_and(|g| g as Pid != t.tgid()) {
        return Err(ESRCH);
    }
    if current().tgid() != 0 && t.group.privileged.load(Ordering::Relaxed) && t.tgid() != current().tgid() {
        return Err(EPERM);
    }
    if sig != 0 {
        post(&t.group.clone(), Some(&t), sig as u32);
    }
    Ok(0)
}

/// The interval timer of `group` armed as `seq` for `deadline` expired
/// (in the timer interrupt): SIGALRM. A periodic timer is not reloaded
/// here but parked until its signal is taken (delivered, or returned by
/// sigtimedwait), as Linux does for POSIX timers: a pending SIGALRM
/// suppresses further expiries, so however short the interval, the timer
/// interrupts at most once per signal the process handles and cannot keep
/// a CPU busy in its interrupt. Periods missed meanwhile are skipped.
pub fn alarm_expired(group: &Arc<ThreadGroup>, seq: u64, deadline: u64) {
    {
        let mut s = group.sig.lock();
        if group.alarm_seq.load(Ordering::Acquire) != seq {
            // Cancelled or set again meanwhile.
            return;
        }
        crate::timer::disarm_alarm(group);
        if s.alarm_every == 0 {
            s.alarm_at = 0;
        } else {
            s.alarm_at = deadline;
            s.alarm_parked = true;
        }
    }
    post(group, None, SIGALRM);
}

/// The first period of an interval timer due at `at` that ends after `now`.
fn next_period(at: u64, every: u64, now: u64) -> u64 {
    if at > now {
        return at;
    }
    let periods = (now - at) / every + 1;
    at.saturating_add(periods.saturating_mul(every))
}

/// SIGALRM was taken: a parked interval timer runs on with its next
/// period. The caller holds the group's signal lock (`s`).
fn alarm_taken(group: &Arc<ThreadGroup>, s: &mut GroupSignals) {
    if s.alarm_parked {
        s.alarm_parked = false;
        s.alarm_at = next_period(s.alarm_at, s.alarm_every, crate::time::now());
        crate::timer::arm_alarm(group, s.alarm_at);
    }
}

/// A parked interval timer whose SIGALRM was dropped as ignored runs on
/// once someone can take the signal again: a handler is installed, or a
/// thread waits for it in sigtimedwait.
fn alarm_unpark_dropped(group: &Arc<ThreadGroup>, s: &mut GroupSignals) {
    if s.alarm_parked && s.pending & bit(SIGALRM) == 0 {
        alarm_taken(group, s);
    }
}

/// setitimer(ITIMER_REAL, new, old) for the calling process. `new` and the
/// result are (value, interval) in microseconds; a value of 0 disarms.
pub fn set_alarm(value_us: u64, interval_us: u64) -> (u64, u64) {
    let group = current().group.clone();
    let mut s = group.sig.lock();
    let old = alarm_left(&s);
    s.alarm_parked = false;
    if value_us == 0 {
        s.alarm_at = 0;
        s.alarm_every = 0;
        crate::timer::disarm_alarm(&group);
    } else {
        s.alarm_at = crate::time::now().saturating_add(value_us.saturating_mul(1000));
        s.alarm_every = interval_us.saturating_mul(1000);
        crate::timer::arm_alarm(&group, s.alarm_at);
    }
    old
}

/// Turns off `group`'s interval timer (the process ended).
pub fn stop_alarm(group: &ThreadGroup) {
    let mut s = group.sig.lock();
    s.alarm_at = 0;
    s.alarm_every = 0;
    s.alarm_parked = false;
    crate::timer::disarm_alarm(group);
}

/// The current ITIMER_REAL setting: (remaining, interval) in microseconds.
pub fn get_alarm() -> (u64, u64) {
    alarm_left(&current().group.sig.lock())
}

fn alarm_left(s: &GroupSignals) -> (u64, u64) {
    if s.alarm_at == 0 {
        return (0, 0);
    }
    // A parked timer reports the period it would be in had it run on.
    let now = crate::time::now();
    let at = if s.alarm_parked { next_period(s.alarm_at, s.alarm_every, now) } else { s.alarm_at };
    // An armed timer never reports 0 left, which would mean "off".
    (at.saturating_sub(now).div_ceil(1000).max(1), s.alarm_every / 1000)
}

/// rt_sigtimedwait(set, info, timeout, size): takes the lowest pending
/// signal of `set` (normally blocked by the caller) without running its
/// handler; waits for one up to `timeout` (EAGAIN), or forever if null.
pub fn sigtimedwait(set: u64, info: u64, timeout: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let wanted: u64 = uaccess::read::<u64>(set)? & !UNBLOCKABLE;
    let deadline = if timeout != 0 { Some(crate::time::now().saturating_add(super::sys_time::read_timespec(timeout)?)) } else { None };
    let me = current();
    if wanted & bit(SIGALRM) != 0 {
        alarm_unpark_dropped(&me.group, &mut me.group.sig.lock());
    }
    let result = loop {
        let wait = sched::prepare_to_sleep();
        {
            let mut g = me.group.sig.lock();
            let mut t = me.sig.lock();
            let ready = (t.pending | g.pending) & wanted;
            if ready != 0 {
                let sig = ready.trailing_zeros() + 1;
                if t.pending & bit(sig) != 0 {
                    t.pending &= !bit(sig);
                } else {
                    g.pending &= !bit(sig);
                }
                if sig == SIGALRM {
                    alarm_taken(&me.group, &mut g);
                }
                break Ok(sig);
            }
            t.waiting_for = wanted;
        }
        if interrupted() {
            break Err(EINTR);
        }
        match deadline {
            Some(d) if crate::time::now() >= d => break Err(EAGAIN),
            Some(d) => wait.sleep_until(d),
            None => wait.sleep(),
        }
    };
    me.sig.lock().waiting_for = 0;
    let sig = result?;
    if info != 0 {
        let mut siginfo = [0u32; 32];
        siginfo[0] = sig;
        uaccess::write(info, siginfo)?;
    }
    Ok(sig as i64)
}

pub fn sigaction(sig: u64, act: u64, oldact: u64) -> SysResult {
    if sig == 0 || sig > NSIG as u64 {
        return Err(EINVAL);
    }
    let sig = sig as u32;
    let new: Option<SigAction> = if act != 0 { Some(uaccess::read(act)?) } else { None };
    if new.is_some() && bit(sig) & UNBLOCKABLE != 0 {
        return Err(EINVAL);
    }
    let old = {
        let group = &current().group;
        let info = group.info.lock();
        let mut g = group.sig.lock();
        let old = g.actions[sig as usize - 1];
        if let Some(mut new) = new {
            new.mask &= !UNBLOCKABLE;
            g.actions[sig as usize - 1] = new;
            // A signal that is now ignored is dropped wherever it pends.
            if g.ignored(sig) {
                g.pending &= !bit(sig);
                for t in info.threads.iter() {
                    t.sig.lock().pending &= !bit(sig);
                }
            } else if sig == SIGALRM {
                alarm_unpark_dropped(group, &mut g);
            }
        }
        old
    };
    if oldact != 0 {
        uaccess::write(oldact, old)?;
    }
    Ok(0)
}

pub fn sigprocmask(how: u64, set: u64, oldset: u64) -> SysResult {
    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIG_SETMASK: u64 = 2;
    let new: Option<u64> = if set != 0 { Some(uaccess::read(set)?) } else { None };
    let (old, blocked_more) = {
        let mut s = current().sig.lock();
        let old = s.mask;
        if let Some(n) = new {
            s.mask = match how {
                SIG_BLOCK => old | n,
                SIG_UNBLOCK => old & !n,
                SIG_SETMASK => n,
                _ => return Err(EINVAL),
            } & !UNBLOCKABLE;
        }
        (old, s.mask & !old != 0)
    };
    if blocked_more {
        retarget(&current().group);
    }
    if oldset != 0 {
        uaccess::write(oldset, old)?;
    }
    Ok(0)
}

/// Runs `wait` with `mask` (if any) as the thread's signal mask, for the
/// calls that wait with a temporary one (rt_sigsuspend, ppoll, pselect6,
/// epoll_pwait). When the wait is interrupted (EINTR), the mask stays
/// until the signal is delivered, so the handler runs with it and the
/// caller's own mask comes back with the handler's return; otherwise it
/// is put back at once, and a signal it held off is delivered only if the
/// caller's mask lets it through.
pub fn with_mask<T>(mask: Option<u64>, wait: impl FnOnce() -> Result<T, i64>) -> Result<T, i64> {
    let Some(mask) = mask else { return wait() };
    let me = current();
    let blocked_more = {
        let mut t = me.sig.lock();
        let current_mask = t.mask;
        let own = *t.restore_mask.get_or_insert(current_mask);
        t.mask = mask & !UNBLOCKABLE;
        t.mask & !own != 0
    };
    if blocked_more {
        retarget(&me.group);
    }
    let result = wait();
    if !matches!(result, Err(EINTR)) {
        let restored = {
            let mut t = me.sig.lock();
            let temporary = t.mask;
            t.restore_mask.take().map(|own| {
                t.mask = own;
                own & !temporary != 0
            })
        };
        if restored == Some(true) {
            retarget(&me.group);
        }
    }
    result
}

/// Puts the caller's own mask back if a temporary one (`with_mask`, kept
/// after EINTR) is still in place, without delivering what it let through
/// (Linux's restore_saved_sigmask).
pub fn restore_saved_mask() {
    let me = current();
    let restored = {
        let mut t = me.sig.lock();
        let temporary = t.mask;
        t.restore_mask.take().map(|own| {
            t.mask = own;
            own & !temporary != 0
        })
    };
    if restored == Some(true) {
        retarget(&me.group);
    }
}

/// A user signal mask argument: null is none, any size but 8 is EINVAL.
pub fn read_mask(ptr: u64, size: u64) -> Result<Option<u64>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    if size != 8 {
        return Err(EINVAL);
    }
    Ok(Some(uaccess::read(ptr)?))
}

/// rt_sigsuspend(mask, size): waits for a signal with `mask`.
pub fn sigsuspend(mask: u64, size: u64) -> SysResult {
    let mask = read_mask(mask, size)?.ok_or(EFAULT)?;
    with_mask(Some(mask), || loop {
        let wait = sched::prepare_to_sleep();
        if interrupted() {
            return Err(EINTR);
        }
        wait.sleep();
    })
}

/// rt_sigpending(set, size): the signals pending and blocked.
pub fn sigpending(set: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let me = current();
    let pending = {
        let g = me.group.sig.lock();
        let t = me.sig.lock();
        (t.pending | g.pending) & t.mask
    };
    uaccess::write(set, pending)?;
    Ok(0)
}

/// What a signal handler finds on its stack, from low to high addresses:
/// the return address (the restorer), then the saved state for sigreturn
/// (registers, mask and the FPU/SSE state, which an asynchronous handler
/// would otherwise clobber), then a minimal siginfo.
#[repr(C)]
#[derive(Clone, Copy)]
struct SigFrame {
    restorer: u64,
    saved: Frame,
    saved_mask: u64,
    fpu: [u8; 512],
    info: [u32; 32],
}

#[repr(C, align(16))]
struct FxArea([u8; 512]);

fn fxsave() -> [u8; 512] {
    let mut area = FxArea([0; 512]);
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) area.0.as_mut_ptr(), options(nostack)) };
    area.0
}

/// Restores an FPU image that came from user memory.
fn fxrstor(image: [u8; 512]) {
    let mut area = FxArea(image);
    // Reserved MXCSR bits would make fxrstor fault in ring 0.
    let mxcsr = u32::from_le_bytes(area.0[24..28].try_into().unwrap()) & 0xffbf;
    area.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) area.0.as_ptr(), options(nostack)) };
}

/// Whether a syscall interrupted with EINTR may be re-executed. Sleeps and
/// waits for events report the interruption instead, as on Linux. After
/// rt_sigreturn, rax holds the restored value of the *earlier* syscall, which
/// must not be mistaken for an EINTR of rt_sigreturn itself.
fn restartable(nr: u64) -> bool {
    // poll, rt_sigreturn, select, pause, nanosleep, rt_sigtimedwait,
    // restart_syscall, rt_sigsuspend, clock_nanosleep, the epoll waits,
    // pselect6, ppoll: EINTR, as on Linux (the Linux server's poll and select
    // restart themselves where Linux does, with the restart codes).
    !matches!(nr, 7 | 15 | 23 | 34 | 35 | 128 | 130 | 219 | 230 | 232 | 270 | 271 | 281 | 441)
}

/// How a call that returns one of the restart codes (only the Linux
/// server's calls do, `restricted::ERESTARTSYS` and the others) goes on
/// after signal delivery.
#[derive(Clone, Copy, PartialEq)]
enum Restart {
    /// EINTR the kernel's `restartable` call returned, or ERESTARTSYS:
    /// again unless a handler without SA_RESTART runs.
    Unless,
    /// ERESTARTNOINTR: always again.
    Always,
    /// ERESTARTNOHAND: again unless a handler runs.
    NoHandler,
    /// ERESTART_RESTARTBLOCK: as `NoHandler`, by restart_syscall.
    Block,
}

/// The restart a call's result in `rax` asks for (the server's codes, or
/// the kernel's EINTR for a restartable call), if any.
fn restart_of(rax: u64, nr: u64) -> Option<Restart> {
    match -(rax as i64) {
        restricted::ERESTARTSYS => Some(Restart::Unless),
        restricted::ERESTARTNOINTR => Some(Restart::Always),
        restricted::ERESTARTNOHAND => Some(Restart::NoHandler),
        restricted::ERESTART_RESTARTBLOCK => Some(Restart::Block),
        EINTR if restartable(nr) => Some(Restart::Unless),
        _ => None,
    }
}

/// Makes the interrupted syscall `nr` run again when the frame resumes
/// (or restart_syscall for `Restart::Block`).
fn restart(frame: &mut Frame, nr: u64, how: Restart) {
    rewind(frame, if how == Restart::Block { restricted::SYS_RESTART_SYSCALL } else { nr });
}

/// Makes the interrupted syscall `nr` run again when the frame resumes.
fn rewind(frame: &mut Frame, nr: u64) {
    frame.rax = nr;
    frame.rip -= 2; // length of the `syscall` instruction
}

/// Joins (or starts) the group stop for `sig`: stops the current thread
/// until SIGCONT (or SIGKILL). The last thread to stop reports the stop
/// to the parent. A SIGCONT that came in meanwhile cancels the stop: it is
/// checked under the group's signal lock, which `post` takes too.
fn group_stop(sig: u32) {
    let me = current();
    let report = {
        let info = me.group.info.lock();
        let mut g = me.group.sig.lock();
        if g.exit != GroupExit::None {
            return;
        }
        if g.stopping == 0 {
            if sig == 0 {
                return;
            }
            // Start the group stop: every other thread joins it.
            g.stopping = sig;
            for t in info.threads.iter().filter(|t| !core::ptr::eq(&***t, me)) {
                t.sig.lock().stop = true;
                kick(t);
            }
        }
        if (me.sig.lock().pending | g.pending) & (bit(SIGCONT) | bit(SIGKILL)) != 0 {
            return;
        }
        {
            let _w = me.wake_lock.lock();
            me.set_state(State::Stopped);
        }
        let all = info.threads.iter().all(|t| t.state() == State::Stopped);
        all.then(|| (info.ppid, stopped_status(g.stopping)))
    };
    if let Some((ppid, status)) = report {
        me.group.info.lock().report = Some(status);
        notify_parent(ppid);
    }
    sched::schedule();
}

/// The current thread dies of `sig`: with the whole process, unless the
/// process is already ending (then just this thread).
fn die(sig: u32) -> ! {
    super::exit_group(sig as i32)
}

/// Handles pending signals before returning to user space: stops the
/// process, terminates it, or redirects `frame` to a handler. `syscall` is
/// the number of the syscall that is returning, if any, so that calls
/// interrupted with EINTR (or a restart code of the Linux server's) can be
/// restarted. A restart code never reaches the program: without a signal
/// to act on the call simply runs again, with a handler it is EINTR unless
/// it restarts.
pub fn deliver(frame: &mut Frame, syscall: Option<u64>) {
    if !frame.from_user() {
        return;
    }
    let mut interrupted = syscall.and_then(|nr| restart_of(frame.rax, nr).map(|how| (nr, how)));
    loop {
        enum Next {
            Done,
            /// The caller's own mask is back after a temporary one: it may
            /// let other signals through, or need them handed to others.
            MaskRestored,
            Stop(u32),
            Die(u32),
            Handle(u32, SigAction, u64),
        }
        let next = {
            let me = current();
            let mut g = me.group.sig.lock();
            let mut t = me.sig.lock();
            let set = g.deliverable(&t);
            if t.stop && set & bit(SIGKILL) == 0 {
                t.stop = false;
                Next::Stop(0)
            } else if set == 0 && t.restore_mask.is_some() {
                t.mask = t.restore_mask.take().unwrap_or(t.mask);
                Next::MaskRestored
            } else if set == 0 {
                // Drop signals that are pending but ignored.
                let ignored = (t.pending | g.pending) & !t.mask;
                t.pending &= !ignored;
                g.pending &= !ignored;
                Next::Done
            } else {
                let sig = set.trailing_zeros() + 1;
                if t.pending & bit(sig) != 0 {
                    t.pending &= !bit(sig);
                } else {
                    g.pending &= !bit(sig);
                }
                if sig == SIGALRM {
                    alarm_taken(&me.group, &mut g);
                }
                let action = g.actions[sig as usize - 1];
                if action.handler == SIG_DFL && is_stop(sig) {
                    Next::Stop(sig)
                } else if action.handler == SIG_DFL {
                    Next::Die(sig)
                } else {
                    // After the handler, the mask from before a temporary one.
                    let old_mask = t.restore_mask.take().unwrap_or(t.mask);
                    let mut block = action.mask;
                    if action.flags & SA_NODEFER == 0 {
                        block |= bit(sig);
                    }
                    t.mask = (t.mask | block) & !UNBLOCKABLE;
                    if action.flags & SA_RESETHAND != 0 {
                        g.actions[sig as usize - 1] = SigAction::default();
                    }
                    Next::Handle(sig, action, old_mask)
                }
            }
        };
        match next {
            Next::Done => {
                // Nothing ran in user space, so the interrupted call can simply go on.
                if let Some((nr, how)) = interrupted {
                    restart(frame, nr, how);
                }
                return;
            }
            Next::MaskRestored => retarget(&current().group),
            Next::Stop(sig) => group_stop(sig),
            Next::Die(sig) => die(sig),
            Next::Handle(sig, action, old_mask) => {
                if let Some((nr, how)) = interrupted.take() {
                    let again = match how {
                        Restart::Always => true,
                        Restart::Unless => action.flags & SA_RESTART != 0,
                        Restart::NoHandler | Restart::Block => false,
                    };
                    if again {
                        restart(frame, nr, how);
                    } else {
                        frame.rax = (-EINTR) as u64;
                    }
                }
                return push_handler_frame(frame, sig, action, old_mask);
            }
        }
    }
}

fn push_handler_frame(frame: &mut Frame, sig: u32, action: SigAction, old_mask: u64) {
    // Without a restorer the handler could never return; a handler outside
    // user space would make iretq fault in ring 0.
    if action.flags & SA_RESTORER == 0 || action.handler >= USER_END {
        die(sig);
    }

    let mut info = [0u32; 32];
    info[0] = sig;
    let sigframe = SigFrame { restorer: action.restorer, saved: *frame, saved_mask: old_mask, fpu: fxsave(), info };
    // Skip the red zone; at handler entry rsp+8 must be 16-byte aligned.
    let size = core::mem::size_of::<SigFrame>() as u64;
    let sp = ((frame.rsp.wrapping_sub(128).wrapping_sub(size)) & !0xf).wrapping_sub(8);
    if uaccess::write(sp, sigframe).is_err() {
        die(SIGSEGV);
    }
    frame.rsp = sp;
    frame.rip = action.handler;
    frame.rdi = sig as u64;
    frame.rsi = sp + core::mem::offset_of!(SigFrame, info) as u64;
    frame.rdx = sp + core::mem::offset_of!(SigFrame, saved) as u64;
    frame.rax = 0;
    frame.rflags &= !(0x400 | 0x100); // clear DF and TF
}

/// rt_sigreturn: restores the state saved by `deliver`. The handler's `ret`
/// already popped the restorer address, so rsp points at `saved`.
pub fn sigreturn(frame: &mut Frame) -> SysResult {
    let base = frame.rsp.wrapping_sub(8);
    let sf: SigFrame = match uaccess::read(base) {
        Ok(sf) => sf,
        Err(_) => die(SIGSEGV),
    };
    let mut saved = sf.saved;
    // A non-canonical or kernel address would make iretq fault in ring 0.
    if saved.rip >= USER_END || saved.rsp >= USER_END {
        die(SIGSEGV);
    }
    // Never let user space choose privileged selectors or flags.
    const USER_FLAGS: u64 = 0xcd5; // CF PF AF ZF SF TF DF OF
    saved.cs = gdt::USER_CS as u64;
    saved.ss = gdt::USER_SS as u64;
    saved.rflags = (saved.rflags & USER_FLAGS) | 0x202;
    *frame = saved;
    fxrstor(sf.fpu);
    let blocked_more = {
        let mut t = current().sig.lock();
        let old = t.mask;
        t.mask = sf.saved_mask & !UNBLOCKABLE;
        t.mask & !old != 0
    };
    if blocked_more {
        retarget(&current().group);
    }
    Ok(frame.rax as i64)
}
