//! Signals (phase R8, docs/design/linux-server.md "Processes and signals", ADR 0010):
//! actions, pending signals with their `siginfo`, masks, the alternate stack, posting
//! (kill, tkill, tgkill, sigqueue, the terminals, faults, children's ends), delivery with
//! Linux's x86-64 signal frames, `rt_sigreturn`, the restart of interrupted calls, group
//! stops and SIGCONT.
//!
//! The kernel knows no signal: posting one is a change of this state (under the process lock,
//! `process::PROCS`) and a kick of a thread that takes it (`SYS_THREAD_KICK`, Linux's
//! signal_wake_up). A kicked thread comes back to its server (`REASON_KICK`, or EINTR from a
//! wait) and delivers on its way back to the program (`deliver`): every signal it may take
//! gets a frame on the program's stack before the program runs, as Linux nests them. The
//! program's FPU registers are live in the CPU while the server runs on its thread (the
//! server uses none), so the server saves and restores them itself (`fxsave64`,
//! `fxrstor64`).

use crate::local;
use crate::process::{self, Pid, Proc, Report, Table, Thread, PROCS};
use crate::syscall;
use crate::usercopy;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

pub const SIGHUP: u32 = 1;
pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGSEGV: u32 = 11;
pub const SIGPIPE: u32 = 13;
pub const SIGALRM: u32 = 14;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
pub const SIGURG: u32 = 23;
pub const SIGWINCH: u32 = 28;
pub const SIGSYS: u32 = 31;
pub const SIGRTMIN: u32 = 32;
pub const NSIG: u32 = 64;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;
pub const SA_NOCLDSTOP: u64 = 1;
pub const SA_NOCLDWAIT: u64 = 2;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_ONSTACK: u64 = 0x0800_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;

pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_TKILL: i32 = -6;
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;

const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;
const SS_AUTODISARM: i32 = 1 << 31;
/// Linux's MINSIGSTKSZ on x86-64.
const MINSIGSTKSZ: u64 = 2048;

const EINTR: i64 = 4;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;

const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
pub const SYS_RT_SIGRETURN: u64 = 15;
const SYS_PAUSE: u64 = 34;
const SYS_KILL: u64 = 62;
const SYS_RT_SIGPENDING: u64 = 127;
const SYS_RT_SIGTIMEDWAIT: u64 = 128;
const SYS_RT_SIGQUEUEINFO: u64 = 129;
const SYS_RT_SIGSUSPEND: u64 = 130;
const SYS_SIGALTSTACK: u64 = 131;
const SYS_TKILL: u64 = 200;
pub const SYS_RESTART_SYSCALL: u64 = 219;
const SYS_TGKILL: u64 = 234;
const SYS_PSELECT6: u64 = 270;
const SYS_PPOLL: u64 = 271;
const SYS_EPOLL_PWAIT: u64 = 281;
const SYS_RT_TGSIGQUEUEINFO: u64 = 297;
const SYS_EPOLL_PWAIT2: u64 = 441;

pub fn bit(sig: u32) -> u64 {
    1 << (sig - 1)
}

/// SIGKILL and SIGSTOP can be neither caught, ignored nor blocked.
pub const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));
/// Delivered before the others (Linux's SYNCHRONOUS_MASK): faults.
const SYNCHRONOUS: u64 = (1 << (SIGSEGV - 1)) | (1 << (SIGBUS - 1)) | (1 << (SIGILL - 1)) | (1 << (SIGTRAP - 1)) | (1 << (SIGFPE - 1)) | (1 << (SIGSYS - 1));
const STOP_SIGNALS: u64 = (1 << (SIGSTOP - 1)) | (1 << (SIGTSTP - 1)) | (1 << (SIGTTIN - 1)) | (1 << (SIGTTOU - 1));
/// Most real-time signals queued with their data in the instance (RLIMIT_SIGPENDING).
const MAX_QUEUED: usize = 4096;

fn ignored_by_default(sig: u32) -> bool {
    matches!(sig, SIGCHLD | SIGCONT | SIGURG | SIGWINCH)
}

fn is_stop(sig: u32) -> bool {
    STOP_SIGNALS & bit(sig) != 0
}

/// Linux's `struct sigaction` as rt_sigaction takes it.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// Signal actions, shared by the processes made with `CLONE_SIGHAND`.
pub struct Hand {
    pub refs: u32,
    pub actions: [SigAction; 64],
}

/// A `siginfo_t` (128 bytes) as Linux lays it out on x86-64: signo, errno, code, then the
/// union from byte 16.
#[derive(Clone, Copy)]
pub struct SigInfo(pub [u64; 16]);

impl SigInfo {
    fn new(sig: u32, code: i32) -> SigInfo {
        let mut s = SigInfo([0; 16]);
        s.0[0] = sig as u64;
        s.0[1] = code as u32 as u64;
        s
    }

    pub fn signo(&self) -> u32 {
        self.0[0] as u32
    }

    /// A signal from the kernel's side of things (a terminal, a timer, a parent's death).
    pub fn kernel(sig: u32) -> SigInfo {
        SigInfo::new(sig, SI_KERNEL)
    }

    /// kill(2) (`SI_USER`) or tkill/tgkill (`SI_TKILL`) by process `pid` (uid 0).
    pub fn user(sig: u32, code: i32, pid: Pid) -> SigInfo {
        let mut s = SigInfo::new(sig, code);
        s.0[2] = pid as u64;
        s
    }

    /// A child's end, stop or continue.
    pub fn chld(sig: u32, code: i32, pid: Pid, status: i32, user_ns: u64, system_ns: u64) -> SigInfo {
        let mut s = SigInfo::new(sig, code);
        s.0[2] = pid as u64;
        s.0[3] = status as u32 as u64;
        // si_utime and si_stime in clock ticks (USER_HZ).
        s.0[4] = user_ns / 10_000_000;
        s.0[5] = system_ns / 10_000_000;
        s
    }

    /// A fault at `addr`.
    pub fn fault(sig: u32, code: i32, addr: u64) -> SigInfo {
        let mut s = SigInfo::new(sig, code);
        s.0[2] = addr;
        s
    }
}

/// Pending signals: their set and the `siginfo` of each instance (a standard signal pends
/// once; a real-time one queues every instance).
#[derive(Default)]
pub struct Queue {
    pub set: u64,
    list: VecDeque<SigInfo>,
}

impl Queue {
    /// Queues `info`; false if a standard signal was pending already (merged).
    fn push(&mut self, info: SigInfo, rt_queued: &mut usize, may_fail: bool) -> Result<bool, i64> {
        let sig = info.signo();
        if sig < SIGRTMIN && self.set & bit(sig) != 0 {
            return Ok(false);
        }
        if sig >= SIGRTMIN {
            if *rt_queued >= MAX_QUEUED || self.list.try_reserve(1).is_err() {
                // Out of queue space: sigqueue fails, kill pends without its data.
                if may_fail {
                    return Err(EAGAIN);
                }
                self.set |= bit(sig);
                return Ok(true);
            }
            *rt_queued += 1;
        } else if self.list.try_reserve(1).is_err() {
            self.set |= bit(sig);
            return Ok(true);
        }
        self.list.push_back(info);
        self.set |= bit(sig);
        Ok(true)
    }

    /// Takes the first pending instance of `sig` (one without data, if it was queued
    /// without: a zeroed siginfo as Linux's collect_signal gives).
    fn take(&mut self, sig: u32, rt_queued: &mut usize) -> SigInfo {
        let info = match self.list.iter().position(|i| i.signo() == sig) {
            Some(at) => {
                if sig >= SIGRTMIN {
                    *rt_queued -= 1;
                }
                self.list.remove(at).expect("found")
            }
            None => SigInfo::new(sig, SI_USER),
        };
        if !self.list.iter().any(|i| i.signo() == sig) {
            self.set &= !bit(sig);
        }
        info
    }

    /// Drops every pending signal of `mask`.
    fn flush(&mut self, mask: u64, rt_queued: &mut usize) {
        if self.set & mask == 0 {
            return;
        }
        let rt_gone = self.list.iter().filter(|i| mask & bit(i.signo()) != 0 && i.signo() >= SIGRTMIN).count();
        self.list.retain(|i| mask & bit(i.signo()) == 0);
        *rt_queued -= rt_gone.min(*rt_queued);
        self.set &= !mask;
    }
}

/// A group stop: the stop signal (0: none), how many threads have yet to stop, and whether
/// all have (reported to the parent).
#[derive(Default, Clone, Copy)]
pub struct Stop {
    pub sig: u32,
    pub pending: u32,
    pub stopped: bool,
}

/// A process's signal state.
pub struct ProcSignals {
    /// Its actions (`process::Table::hands`).
    pub hand: u64,
    /// Signals sent to the process.
    pub shared: Queue,
    pub stop: Stop,
}

impl ProcSignals {
    pub fn new(hand: u64) -> ProcSignals {
        ProcSignals { hand, shared: Queue::default(), stop: Stop::default() }
    }
}

/// The alternate signal stack (sigaltstack).
#[derive(Clone, Copy, Default)]
pub struct AltStack {
    pub sp: u64,
    pub size: u64,
    /// `SS_AUTODISARM` (the stack's other state is whether `size` is 0).
    pub autodisarm: bool,
}

/// What a relative sleep left to do when a signal interrupted it (restart_syscall).
#[derive(Clone, Copy)]
pub struct RestartBlock {
    /// The monotonic deadline, and where the time left goes (0: nowhere).
    pub deadline: u64,
    pub rem: u64,
}

/// A thread's signal state.
#[derive(Default)]
pub struct ThreadSignals {
    pub mask: u64,
    /// Signals sent to this thread (tkill, faults, SIGPIPE).
    pub pending: Queue,
    pub alt: AltStack,
    /// The mask to put back after a call that waited with a temporary one (sigsuspend,
    /// ppoll, pselect6, epoll_pwait): by the frame of the handler it let in, or before the
    /// program runs again.
    pub restore: Option<u64>,
    /// Signals a sigtimedwait waits for (posters kick it even if it blocks them).
    pub waiting_for: u64,
    /// The call that returned EINTR, to restart or not when the thread delivers.
    pub interrupted: Option<u64>,
    /// A relative sleep's rest (restart_syscall).
    pub block: Option<RestartBlock>,
    /// A group stop asks this thread to stop / it has.
    pub must_stop: bool,
    pub stopped: bool,
    /// The last exception (vector, error code, address) for the next frame's sigcontext.
    pub trap: (u64, u64, u64),
}

impl ThreadSignals {
    /// A new thread or process: the creator's mask and alternate stack (not with a shared
    /// address space without vfork: the stack would be shared), nothing else.
    pub fn for_child(&self, drop_alt: bool) -> ThreadSignals {
        ThreadSignals { mask: self.mask, alt: if drop_alt { AltStack::default() } else { self.alt }, ..ThreadSignals::default() }
    }
}

// ------------------------------------------------------------------ posting

/// Whether `sig` is ignored by process `p` (its action, or its default).
fn ignored(t: &Table, p: &Proc, sig: u32) -> bool {
    let a = t.hands[&p.sig.hand].actions[sig as usize - 1];
    a.handler == SIG_IGN || (a.handler == SIG_DFL && ignored_by_default(sig))
}

/// Whether thread `th` would take `sig` now.
fn wants(th: &Thread, sig: u32) -> bool {
    !th.exited && (th.sig.mask & bit(sig) == 0 || bit(sig) & UNBLOCKABLE != 0 || th.sig.waiting_for & bit(sig) != 0)
}

/// Whether the default action of `sig` ends the process.
fn kills_by_default(sig: u32) -> bool {
    !ignored_by_default(sig) && !is_stop(sig)
}

/// SIGCONT and the stop signals act when they are sent (Linux's prepare_signal): SIGCONT
/// ends a stop at once and drops pending stops; a stop signal drops a pending SIGCONT.
/// Whether the signal is to be queued at all (an ignored one is dropped unless the thread
/// it is for blocks it; the instance's init gets only signals it handles).
fn prepare(t: &mut Table, pid: Pid, thread: Option<Pid>, sig: u32, force: bool) -> bool {
    let Some(p) = t.procs.get(&pid) else { return false };
    if p.zombie.is_some() || p.exiting.is_some() {
        return false;
    }
    if sig == SIGCONT {
        let p = t.procs.get_mut(&pid).expect("checked");
        let mut rt = t.rt_queued;
        p.sig.shared.flush(STOP_SIGNALS, &mut rt);
        let threads = p.threads.clone();
        let stop = core::mem::take(&mut p.sig.stop);
        let words = p.words.clone();
        for tid in &threads {
            if let Some(th) = t.threads.get_mut(tid) {
                th.sig.pending.flush(STOP_SIGNALS, &mut rt);
                th.sig.must_stop = false;
            }
        }
        t.rt_queued = rt;
        if stop.sig != 0 {
            process::wake_word(&words.stop);
            if stop.stopped {
                notify_parent_stop(t, pid, Report::Continued);
            }
        }
    } else if is_stop(sig) {
        let mut rt = t.rt_queued;
        let p = t.procs.get_mut(&pid).expect("checked");
        p.sig.shared.flush(bit(SIGCONT), &mut rt);
        let threads = p.threads.clone();
        for tid in &threads {
            if let Some(th) = t.threads.get_mut(tid) {
                th.sig.pending.flush(bit(SIGCONT), &mut rt);
            }
        }
        t.rt_queued = rt;
    }
    let p = &t.procs[&pid];
    let action = t.hands[&p.sig.hand].actions[sig as usize - 1];
    // Linux's SIGNAL_UNKILLABLE: the namespace's init gets no signal it does not handle
    // from inside its namespace (a fault still kills it).
    if pid == 1 && action.handler == SIG_DFL && !force {
        return false;
    }
    let blocked_by = |tid: Pid| t.threads.get(&tid).is_some_and(|th| th.sig.mask & bit(sig) != 0 || th.sig.waiting_for & bit(sig) != 0);
    let target = thread.unwrap_or(pid);
    !(ignored(t, p, sig) && !blocked_by(target))
}

/// Sends `info` to process `pid` (Linux's group_send_sig_info); `force`: from the kernel's
/// side (a fault). Whether it exists.
pub fn post_process(t: &mut Table, pid: Pid, info: SigInfo, force: bool) -> bool {
    post(t, pid, None, info, force, false).is_ok()
}

/// Sends `info` to thread `tid` of process `pid`.
pub fn post_thread(t: &mut Table, pid: Pid, tid: Pid, info: SigInfo) -> bool {
    post(t, pid, Some(tid), info, false, false).is_ok()
}

fn post(t: &mut Table, pid: Pid, thread: Option<Pid>, info: SigInfo, force: bool, may_fail: bool) -> Result<(), i64> {
    if !t.procs.contains_key(&pid) {
        return Err(ESRCH);
    }
    let sig = info.signo();
    if sig == 0 || sig > NSIG || !prepare(t, pid, thread, sig, force) {
        return Ok(());
    }
    let mut rt = t.rt_queued;
    let queued = match thread {
        Some(tid) => match t.threads.get_mut(&tid) {
            Some(th) => th.sig.pending.push(info, &mut rt, may_fail),
            None => return Err(ESRCH),
        },
        None => t.procs.get_mut(&pid).expect("checked").sig.shared.push(info, &mut rt, may_fail),
    };
    t.rt_queued = rt;
    if queued? {
        complete(t, pid, thread, sig);
    }
    Ok(())
}

/// A signal was queued (Linux's complete_signal): a fatal one ends the process at once,
/// else a thread that takes it is kicked.
fn complete(t: &mut Table, pid: Pid, thread: Option<Pid>, sig: u32) {
    let p = &t.procs[&pid];
    let chosen = match thread {
        Some(tid) => t.threads.get(&tid).filter(|th| wants(th, sig)).map(|th| th.tid),
        // The main thread first, as Linux's does.
        None => p.threads.iter().filter_map(|tid| t.threads.get(tid)).find(|th| wants(th, sig)).map(|th| th.tid),
    };
    let Some(tid) = chosen else { return };
    let action = t.hands[&p.sig.hand].actions[sig as usize - 1];
    let waited_for = t.threads.get(&tid).is_some_and(|th| th.sig.waiting_for & bit(sig) != 0);
    if sig == SIGKILL || (action.handler == SIG_DFL && kills_by_default(sig) && !waited_for) {
        // A fatal signal kills every thread now, wherever it waits.
        t.group_exit(pid, sig as i32, None);
        return;
    }
    t.kick(tid);
}

/// Sends `info` to every process of group `pgid`; whether there was one.
pub fn post_pgrp(t: &mut Table, pgid: Pid, info: SigInfo) -> bool {
    let members: alloc::vec::Vec<Pid> = t.procs.values().filter(|p| p.pgid == pgid && p.zombie.is_none()).map(|p| p.pid).collect();
    for &pid in &members {
        post_process(t, pid, info, false);
    }
    !members.is_empty()
}

/// SIGHUP and SIGCONT to a process group that became orphaned with stopped members.
pub fn hup_and_continue(t: &mut Table, pgid: Pid) {
    post_pgrp(t, pgid, SigInfo::kernel(SIGHUP));
    post_pgrp(t, pgid, SigInfo::kernel(SIGCONT));
}

/// Raises a fault's signal for the calling thread (Linux's force_sig_info): one it blocks
/// or ignores is unblocked and reset to its default.
fn force(t: &mut Table, tid: Pid, info: SigInfo) {
    let sig = info.signo();
    let Some(pid) = t.threads.get(&tid).map(|th| th.pid) else { return };
    let blocked = t.threads[&tid].sig.mask & bit(sig) != 0;
    let a = &mut t.actions_mut(pid)[sig as usize - 1];
    if blocked || a.handler == SIG_IGN {
        *a = SigAction::default();
    }
    if let Some(th) = t.threads.get_mut(&tid) {
        th.sig.mask &= !bit(sig);
    }
    post(t, pid, Some(tid), info, true, false).ok();
}

/// Tells the parent of `pid` that it stopped or continued (Linux's
/// do_notify_parent_cldstop): a report for `wait`, a wakeup, and SIGCHLD unless the parent
/// set `SA_NOCLDSTOP` or ignores it.
fn notify_parent_stop(t: &mut Table, pid: Pid, report: Report) {
    let Some(p) = t.procs.get_mut(&pid) else { return };
    p.report = Some(report);
    let (ppid, handle) = (p.ppid, p.handle);
    let Some(parent) = t.procs.get(&ppid) else { return };
    let chld = t.hands[&parent.sig.hand].actions[SIGCHLD as usize - 1];
    let words = parent.words.clone();
    if chld.handler != SIG_IGN && chld.flags & SA_NOCLDSTOP == 0 {
        let mut info = ProcInfo::default();
        syscall(SYS_PROC_INFO, [handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
        let (code, value) = match report {
            Report::Stopped(sig) => (CLD_STOPPED, sig as i32),
            Report::Continued => (CLD_CONTINUED, SIGCONT as i32),
        };
        post_process(t, ppid, SigInfo::chld(SIGCHLD, code, pid, value, info.user_ns, info.system_ns), false);
    }
    process::wake_word(&words.child);
}

/// The thread `th` of process `pid` is gone (the process lives on): a group stop no longer
/// waits for it, and the process's pending signals go to another thread.
pub fn thread_left(t: &mut Table, pid: Pid, th: &Thread) {
    let Some(p) = t.procs.get_mut(&pid) else { return };
    if p.sig.stop.sig != 0 && !p.sig.stop.stopped && !th.sig.stopped && p.sig.stop.pending > 0 {
        p.sig.stop.pending -= 1;
        if p.sig.stop.pending == 0 {
            p.sig.stop.stopped = true;
            let sig = p.sig.stop.sig;
            notify_parent_stop(t, pid, Report::Stopped(sig));
        }
    }
    retarget(t, pid);
}

/// Kicks a thread of `pid` that can take one of its pending process signals.
fn retarget(t: &Table, pid: Pid) {
    let Some(p) = t.procs.get(&pid) else { return };
    let pending = p.sig.shared.set;
    if pending == 0 {
        return;
    }
    for tid in &p.threads {
        if let Some(th) = t.threads.get(tid) {
            let open = pending & (!th.sig.mask | UNBLOCKABLE | th.sig.waiting_for);
            if !th.exited && open != 0 {
                t.kick(*tid);
                return;
            }
        }
    }
}

// ------------------------------------------------------------------ for other modules

/// Whether a signal (or a stop) waits for the calling thread (Linux's signal_pending): a
/// long call returns what it did so far.
pub fn pending() -> bool {
    let (tid, pid) = process::me();
    let t = PROCS.lock();
    let (Some(th), Some(p)) = (t.threads.get(&tid), t.procs.get(&pid)) else { return false };
    p.exiting.is_some() || th.sig.must_stop || deliverable(th, p) != 0
}

/// Whether the calling process ignores `sig` (SIG_IGN) and whether the calling thread
/// blocks it: the terminals' SIGTTIN and SIGTTOU.
pub fn ignored_or_blocked(sig: u32) -> (bool, bool) {
    let (tid, pid) = process::me();
    let t = PROCS.lock();
    let ignored = t.procs.get(&pid).is_some_and(|p| t.hands[&p.sig.hand].actions[sig as usize - 1].handler == SIG_IGN);
    let blocked = t.threads.get(&tid).is_some_and(|th| th.sig.mask & bit(sig) != 0);
    (ignored, blocked)
}

/// Raises `sig` for the calling thread (SIGPIPE of a write without a reader); it is
/// delivered when the call returns, after its result.
pub fn raise_thread(sig: u32) {
    let (tid, pid) = process::me();
    let mut t = PROCS.lock();
    post_thread(&mut t, pid, tid, SigInfo::kernel(sig));
}

/// Sends `sig` from a terminal (SI_KERNEL) to process group `pgid`; whether there was one.
pub fn send_pgrp(pgid: Pid, sig: u32) -> bool {
    let mut t = PROCS.lock();
    post_pgrp(&mut t, pgid, SigInfo::kernel(sig))
}

/// Sends `sig` from a terminal to process `pid`; whether it exists.
pub fn send_process(pid: Pid, sig: u32) -> bool {
    let mut t = PROCS.lock();
    post_process(&mut t, pid, SigInfo::kernel(sig), false)
}

// ------------------------------------------------------------------ delivery

/// The signals thread `th` of `p` would act on now.
fn deliverable(th: &Thread, p: &Proc) -> u64 {
    (th.sig.pending.set | p.sig.shared.set) & !(th.sig.mask & !UNBLOCKABLE)
}

/// The next signal of `set`: faults first, then the lowest.
fn pick(set: u64) -> u32 {
    let sync = set & SYNCHRONOUS;
    (if sync != 0 { sync } else { set }).trailing_zeros() + 1
}

/// How an interrupted call restarts (Linux's restart codes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Restart {
    /// ERESTARTSYS: unless a handler without SA_RESTART runs.
    Sys,
    /// ERESTARTNOHAND: only if no handler runs.
    NoHand,
    /// ERESTART_RESTARTBLOCK: through restart_syscall, if no handler runs.
    Block,
    /// EINTR, always.
    Never,
}

fn restart_of(nr: u64) -> Restart {
    match nr {
        // poll, select, pause, rt_sigsuspend, pselect6, ppoll.
        7 | 23 | 34 | 130 | 270 | 271 => Restart::NoHand,
        // nanosleep, clock_nanosleep, restart_syscall: a relative sleep saved its rest; an
        // absolute one restarts as it is.
        35 | 230 | SYS_RESTART_SYSCALL => Restart::Block,
        // rt_sigreturn, rt_sigtimedwait, the epoll waits.
        SYS_RT_SIGRETURN | SYS_RT_SIGTIMEDWAIT | 232 | SYS_EPOLL_PWAIT | SYS_EPOLL_PWAIT2 => Restart::Never,
        _ => Restart::Sys,
    }
}

/// Makes the program run its system call `nr` again.
fn rewind(s: &mut State, nr: u64) {
    s.rax = nr;
    s.rip -= 2;
}

/// The interrupted call when no handler ran: it goes on.
fn restart_without_handler(s: &mut State, nr: u64, block: Option<RestartBlock>) {
    match restart_of(nr) {
        Restart::Sys | Restart::NoHand => rewind(s, nr),
        Restart::Block if block.is_some() => {
            // (`s.rip` is still after the call's `syscall`.)
            s.rax = SYS_RESTART_SYSCALL;
            s.rip -= 2;
        }
        Restart::Block => rewind(s, nr),
        Restart::Never => {}
    }
}

/// What the thread does next on its way back to its program.
enum Next {
    /// Nothing (more) to deliver.
    Done,
    /// Its process ends (with that status), or it was killed: it exits.
    Exit(i32),
    /// It stops until SIGCONT: it waits on the word (`seen`: its value before).
    Stop(alloc::sync::Arc<process::Words>, u32),
    /// A fatal signal: the process ends with it.
    Die(u32),
    /// A handler runs: (signal, action, siginfo, the mask its frame restores, the stack
    /// it runs on, the trap of a fault).
    Handle(u32, SigAction, SigInfo, u64, AltStack, (u64, u64, u64)),
}

/// Delivers the calling thread's signals before its program runs again (`s`: its
/// registers). `call`: the system call it just made and its result, if it returned to
/// here (an EINTR is restarted as Linux's restart codes say).
pub fn deliver(s: &mut State, call: Option<(u64, i64)>) {
    let (tid, pid) = process::me();
    if tid == 0 {
        return;
    }
    if let Some((nr, result)) = call {
        let mut t = PROCS.lock();
        let Some(th) = t.threads.get_mut(&tid) else { return };
        if result == -EINTR && nr != SYS_RT_SIGRETURN {
            th.sig.interrupted = Some(nr);
        } else if nr != SYS_RESTART_SYSCALL {
            th.sig.block = None;
        }
    }
    loop {
        let next = next_action(tid, pid);
        match next {
            Next::Done => {
                // An interrupted call no handler took goes on.
                finish(s);
                return;
            }
            Next::Exit(status) => {
                syscall(SYS_THREAD_EXIT, [status as u32 as u64, 0, 0, 0, 0, 0]);
            }
            Next::Die(sig) => process::die(sig as i32),
            Next::Stop(words, seen) => {
                // Until SIGCONT or a kill; a kick ends the wait too: the kernel's
                // restricted_enter then clears it and the thread comes back here (with the
                // restart of an interrupted call still to decide).
                if process::wait_word(&words.stop, seen, 0, true) == -process::EINTR {
                    return;
                }
            }
            Next::Handle(sig, action, info, old_mask, alt, trap) => {
                let restart = {
                    let mut t = PROCS.lock();
                    t.threads.get_mut(&tid).and_then(|th| {
                        th.sig.block = None;
                        th.sig.interrupted.take()
                    })
                };
                if let Some(nr) = restart {
                    // `s.rax` holds -EINTR.
                    if restart_of(nr) == Restart::Sys && action.flags & SA_RESTART != 0 {
                        rewind(s, nr);
                    }
                }
                if setup_frame(s, sig, &action, &info, old_mask, alt, trap).is_err() {
                    // The frame could not be written: SIGSEGV, which kills if it was that.
                    let mut t = PROCS.lock();
                    if sig == SIGSEGV {
                        t.actions_mut(pid)[SIGSEGV as usize - 1] = SigAction::default();
                    }
                    force(&mut t, tid, SigInfo::kernel(SIGSEGV));
                }
            }
        }
    }
    // (Each pass of the loop above returns or goes on.)
}

/// Decides the thread's next step under the lock (see `Next`).
fn next_action(tid: Pid, pid: Pid) -> Next {
    let mut guard = PROCS.lock();
    let t = &mut *guard;
    loop {
        let (Some(th), Some(p)) = (t.threads.get(&tid), t.procs.get(&pid)) else { return Next::Exit(SIGKILL as i32) };
        if p.exiting.is_some() || th.exited {
            return Next::Exit(p.exiting.unwrap_or(SIGKILL as i32));
        }
        // A group stop.
        if th.sig.must_stop {
            let p = t.procs.get_mut(&pid).expect("checked");
            p.sig.stop.pending = p.sig.stop.pending.saturating_sub(1);
            let complete = p.sig.stop.pending == 0 && !p.sig.stop.stopped;
            if complete {
                p.sig.stop.stopped = true;
            }
            let sig = p.sig.stop.sig;
            let th = t.threads.get_mut(&tid).expect("checked");
            th.sig.must_stop = false;
            th.sig.stopped = true;
            if complete {
                notify_parent_stop(t, pid, Report::Stopped(sig));
            }
            continue;
        }
        if th.sig.stopped {
            if p.sig.stop.sig != 0 && deliverable(th, p) & bit(SIGKILL) == 0 {
                let words = p.words.clone();
                let seen = words.stop.load(Ordering::Acquire);
                return Next::Stop(words, seen);
            }
            t.threads.get_mut(&tid).expect("checked").sig.stopped = false;
            continue;
        }
        let set = deliverable(th, p);
        if set == 0 {
            let th = t.threads.get_mut(&tid).expect("checked");
            // A temporary mask's wait ended without a handler: the caller's own mask.
            if let Some(restore) = th.sig.restore.take() {
                th.sig.mask = restore;
                local::get().flags.fetch_and(!local::RESTORE_MASK, Ordering::Relaxed);
                // The caller's own mask may let more in.
                continue;
            }
            local::get().flags.fetch_and(!local::RESTORE_MASK, Ordering::Relaxed);
            return Next::Done;
        }
        let sig = pick(set);
        let mut rt = t.rt_queued;
        let info = {
            let th = t.threads.get_mut(&tid).expect("checked");
            if th.sig.pending.set & bit(sig) != 0 {
                th.sig.pending.take(sig, &mut rt)
            } else {
                t.procs.get_mut(&pid).expect("checked").sig.shared.take(sig, &mut rt)
            }
        };
        t.rt_queued = rt;
        if sig == SIGALRM {
            crate::timer::taken(t, pid);
        }
        let action = t.actions(pid)[sig as usize - 1];
        if action.handler == SIG_IGN {
            continue;
        }
        if action.handler == SIG_DFL {
            if ignored_by_default(sig) {
                continue;
            }
            if is_stop(sig) {
                // A stop of an orphaned process group would never be continued (but
                // SIGSTOP, which is not the terminal's).
                let pgid = t.procs[&pid].pgid;
                if sig != SIGSTOP && t.pgrp_orphaned(pgid, None) {
                    continue;
                }
                start_stop(t, pid, tid, sig);
                continue;
            }
            return Next::Die(sig);
        }
        // A handler: its mask, and the frame's (the caller's own after a temporary one).
        let th = t.threads.get_mut(&tid).expect("checked");
        let old_mask = th.sig.restore.take().unwrap_or(th.sig.mask);
        local::get().flags.fetch_and(!local::RESTORE_MASK, Ordering::Relaxed);
        let mut block = action.mask;
        if action.flags & SA_NODEFER == 0 {
            block |= bit(sig);
        }
        th.sig.mask = (old_mask | block) & !UNBLOCKABLE;
        // (An SS_AUTODISARM stack is disarmed when a frame goes onto it: `setup_frame`.)
        let alt = th.sig.alt;
        let trap = core::mem::take(&mut th.sig.trap);
        if action.flags & SA_RESETHAND != 0 {
            t.actions_mut(pid)[sig as usize - 1] = SigAction::default();
        }
        return Next::Handle(sig, action, info, old_mask, alt, trap);
    }
}

/// Starts (or joins) the group stop for `sig`: every thread of the process stops.
fn start_stop(t: &mut Table, pid: Pid, me: Pid, sig: u32) {
    let p = t.procs.get_mut(&pid).expect("listed");
    if p.sig.stop.sig != 0 {
        // Under way: this thread joins it.
        if let Some(th) = t.threads.get_mut(&me) {
            if !th.sig.stopped {
                th.sig.must_stop = true;
            }
        }
        return;
    }
    let threads = p.threads.clone();
    let live: alloc::vec::Vec<Pid> = threads.into_iter().filter(|tid| t.threads.get(tid).is_some_and(|th| !th.exited)).collect();
    let p = t.procs.get_mut(&pid).expect("listed");
    p.sig.stop = Stop { sig, pending: live.len() as u32, stopped: false };
    for tid in live {
        if let Some(th) = t.threads.get_mut(&tid) {
            th.sig.must_stop = true;
        }
        if tid != me {
            t.kick(tid);
        }
    }
}

/// The restart of an interrupted call no handler took (Linux's restart codes without a
/// handler).
fn finish(s: &mut State) {
    let tid = local::tid();
    if tid == 0 {
        return;
    }
    let (nr, block) = {
        let mut t = PROCS.lock();
        let Some(th) = t.threads.get_mut(&tid) else { return };
        let Some(nr) = th.sig.interrupted.take() else { return };
        (nr, th.sig.block)
    };
    restart_without_handler(s, nr, block);
}

// ------------------------------------------------------------------ frames

/// x86-64's `struct sigcontext`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SigContext {
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rbx: u64,
    rdx: u64,
    rax: u64,
    rcx: u64,
    rsp: u64,
    rip: u64,
    eflags: u64,
    cs: u16,
    gs: u16,
    fs: u16,
    ss: u16,
    err: u64,
    trapno: u64,
    oldmask: u64,
    cr2: u64,
    fpstate: u64,
    reserved: [u64; 8],
}

/// `stack_t`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StackT {
    sp: u64,
    flags: i32,
    _pad: i32,
    size: u64,
}

/// The kernel's `struct ucontext` on x86-64.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UContext {
    flags: u64,
    link: u64,
    stack: StackT,
    mcontext: SigContext,
    sigmask: u64,
}

/// `struct rt_sigframe`: the return address (the restorer), the context, the siginfo.
#[repr(C)]
#[derive(Clone, Copy)]
struct RtSigFrame {
    pretcode: u64,
    uc: UContext,
    info: [u64; 16],
}

const UC_SIGCONTEXT_SS: u64 = 2;
const UC_STRICT_RESTORE_SS: u64 = 4;
/// Linux's user code and stack selectors on x86-64.
const USER_CS: u16 = 0x33;
const USER_SS: u16 = 0x2b;
const FPU_SIZE: u64 = 512;
/// rflags bits sigreturn takes from the frame (Linux's FIX_EFLAGS).
const FIX_EFLAGS: u64 = 0x40000 | 0x800 | 0x400 | 0x100 | 0x80 | 0x40 | 0x10 | 0x4 | 0x1 | 0x10000;

#[repr(C, align(64))]
struct FxArea([u8; 512]);

/// The program's FPU and SSE registers (live in the CPU while the server runs).
fn fxsave() -> FxArea {
    let mut area = FxArea([0; 512]);
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) area.0.as_mut_ptr(), options(nostack)) };
    // Bytes 464.. are free for software; Linux's fxsave frames carry no magic there.
    area.0[464..].fill(0);
    area
}

/// Loads an FPU image (from the program: MXCSR's reserved bits are cleared first, they
/// would fault).
fn fxrstor(mut area: FxArea) {
    let mask = match u32::from_le_bytes(area.0[28..32].try_into().expect("4 bytes")) {
        0 => 0xffbf,
        m => m,
    };
    let mxcsr = u32::from_le_bytes(area.0[24..28].try_into().expect("4 bytes")) & mask & 0xffff;
    area.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) area.0.as_ptr(), options(nostack)) };
}

/// The FPU state a program starts with (and a handler: Linux clears it for one).
fn fpu_init() {
    let mut area = FxArea([0; 512]);
    area.0[0..2].copy_from_slice(&0x037f_u16.to_le_bytes());
    area.0[24..28].copy_from_slice(&0x1f80_u32.to_le_bytes());
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) area.0.as_ptr(), options(nostack)) };
}

fn on_stack(alt: &AltStack, sp: u64) -> bool {
    !alt.autodisarm && alt.size != 0 && sp > alt.sp && sp - alt.sp <= alt.size
}

/// `sas_ss_flags` of the stack pointer `sp`.
fn alt_flags(alt: &AltStack, sp: u64) -> i32 {
    if alt.size == 0 {
        SS_DISABLE
    } else if on_stack(alt, sp) {
        SS_ONSTACK
    } else {
        0
    }
}

/// Builds the frame of a handler for `sig` on the program's stack and points its registers
/// at the handler (Linux's get_sigframe and setup_rt_frame).
fn setup_frame(s: &mut State, sig: u32, action: &SigAction, info: &SigInfo, old_mask: u64, alt: AltStack, trap: (u64, u64, u64)) -> Result<(), i64> {
    // Without a restorer the handler could never return (x86-64 requires SA_RESTORER); a
    // handler outside the program's memory could not run.
    if action.flags & SA_RESTORER == 0 || action.handler >= SHARED_BASE || action.restorer >= SHARED_BASE {
        return Err(EFAULT);
    }
    let mut sp = s.rsp.wrapping_sub(128);
    let mut entering = false;
    if action.flags & SA_ONSTACK != 0 && alt.size != 0 && !on_stack(&alt, s.rsp) {
        sp = alt.sp + alt.size;
        entering = true;
    }
    sp = sp.wrapping_sub(FPU_SIZE) & !63;
    let fpstate = sp;
    let size = core::mem::size_of::<RtSigFrame>() as u64;
    sp = (sp.wrapping_sub(size).wrapping_add(8) & !15).wrapping_sub(8);
    // A frame that runs off the alternate stack is a fault.
    if (entering || on_stack(&alt, s.rsp)) && !(sp > alt.sp && sp - alt.sp <= alt.size) {
        return Err(EFAULT);
    }
    let fpu = fxsave();
    usercopy::to_program(fpstate, &fpu.0)?;
    let mc = SigContext {
        r8: s.r8,
        r9: s.r9,
        r10: s.r10,
        r11: s.r11,
        r12: s.r12,
        r13: s.r13,
        r14: s.r14,
        r15: s.r15,
        rdi: s.rdi,
        rsi: s.rsi,
        rbp: s.rbp,
        rbx: s.rbx,
        rdx: s.rdx,
        rax: s.rax,
        rcx: s.rcx,
        rsp: s.rsp,
        rip: s.rip,
        eflags: s.rflags,
        cs: USER_CS,
        gs: 0,
        fs: 0,
        ss: USER_SS,
        err: trap.1,
        trapno: trap.0,
        oldmask: old_mask,
        cr2: trap.2,
        fpstate,
        reserved: [0; 8],
    };
    let stack = StackT { sp: alt.sp, flags: alt_flags(&alt, s.rsp) | if alt.autodisarm { SS_AUTODISARM } else { 0 }, _pad: 0, size: alt.size };
    let frame = RtSigFrame {
        pretcode: action.restorer,
        uc: UContext { flags: UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS, link: 0, stack, mcontext: mc, sigmask: old_mask },
        info: info.0,
    };
    usercopy::write(sp, &frame)?;
    if entering && alt.autodisarm {
        // SS_AUTODISARM: the stack is the handler's alone until it returns.
        let mut t = PROCS.lock();
        if let Some(th) = t.threads.get_mut(&local::tid()) {
            th.sig.alt = AltStack::default();
        }
    }
    s.rsp = sp;
    s.rip = action.handler;
    s.rdi = sig as u64;
    s.rsi = sp + core::mem::offset_of!(RtSigFrame, info) as u64;
    s.rdx = sp + core::mem::offset_of!(RtSigFrame, uc) as u64;
    s.rax = 0;
    // DF, TF and RF cleared, as Linux does.
    s.rflags &= !(0x400 | 0x100 | 0x10000);
    fpu_init();
    Ok(())
}

/// rt_sigreturn: back to the state the handler's frame saved. The handler's `ret` popped
/// the return address, so the frame starts 8 bytes below the stack pointer.
fn sigreturn(s: &mut State) -> i64 {
    let base = s.rsp.wrapping_sub(8);
    let frame: RtSigFrame = match read_frame(base) {
        Some(f) => f,
        None => return bad_frame(),
    };
    let mc = &frame.uc.mcontext;
    if mc.rip >= SHARED_BASE || mc.rsp >= SHARED_BASE {
        return bad_frame();
    }
    if mc.fpstate != 0 {
        let mut fpu = FxArea([0; 512]);
        if mc.fpstate >= SHARED_BASE || usercopy::from_program(mc.fpstate, &mut fpu.0).is_err() {
            return bad_frame();
        }
        fxrstor(fpu);
    } else {
        fpu_init();
    }
    *s = State {
        r8: mc.r8,
        r9: mc.r9,
        r10: mc.r10,
        r11: mc.r11,
        r12: mc.r12,
        r13: mc.r13,
        r14: mc.r14,
        r15: mc.r15,
        rdi: mc.rdi,
        rsi: mc.rsi,
        rbp: mc.rbp,
        rbx: mc.rbx,
        rdx: mc.rdx,
        rax: mc.rax,
        rcx: mc.rcx,
        rsp: mc.rsp,
        rip: mc.rip,
        rflags: (s.rflags & !FIX_EFLAGS) | (mc.eflags & FIX_EFLAGS),
        ..*s
    };
    {
        let mut t = PROCS.lock();
        let (tid, pid) = process::me();
        if let Some(th) = t.threads.get_mut(&tid) {
            let old = th.sig.mask;
            th.sig.mask = frame.uc.sigmask & !UNBLOCKABLE;
            // The handler's stack, as it was (errors are ignored, as Linux's
            // restore_altstack does).
            let st = frame.uc.stack;
            if !on_stack(&th.sig.alt, s.rsp) {
                if st.flags & SS_DISABLE != 0 {
                    th.sig.alt = AltStack::default();
                } else if st.size >= MINSIGSTKSZ && st.sp.checked_add(st.size).is_some_and(|e| e <= SHARED_BASE) {
                    th.sig.alt = AltStack { sp: st.sp, size: st.size, autodisarm: st.flags & SS_AUTODISARM != 0 };
                }
            }
            if th.sig.mask & !old != 0 {
                retarget(&t, pid);
            }
        }
    }
    s.rax as i64
}

fn read_frame(addr: u64) -> Option<RtSigFrame> {
    let mut bytes = [0u8; core::mem::size_of::<RtSigFrame>()];
    usercopy::from_program(addr, &mut bytes).ok()?;
    Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const RtSigFrame) })
}

/// A frame sigreturn cannot use: SIGSEGV for the thread. Returns the call's result (the
/// registers stay as they are).
fn bad_frame() -> i64 {
    let tid = local::tid();
    let mut t = PROCS.lock();
    force(&mut t, tid, SigInfo::kernel(SIGSEGV));
    0
}

// ------------------------------------------------------------------ exceptions

const SEGV_MAPERR: i32 = 1;
const SEGV_ACCERR: i32 = 2;
const BUS_ADRALN: i32 = 1;
const BUS_ADRERR: i32 = 2;
const FPE_INTDIV: i32 = 1;
const FPE_FLTDIV: i32 = 3;
const FPE_FLTOVF: i32 = 4;
const FPE_FLTUND: i32 = 5;
const FPE_FLTRES: i32 = 6;
const FPE_FLTINV: i32 = 7;
const ILL_ILLOPN: i32 = 2;
const TRAP_TRACE: i32 = 2;

/// The program raised an exception (`REASON_EXCEPTION`): its signal, as Linux's traps
/// give it, forced on the thread.
pub fn exception(s: &State) {
    let (vector, error, addr, kind) = (s.trap_vector, s.trap_error, s.trap_addr, s.trap_kind);
    let info = match vector {
        0 => SigInfo::fault(SIGFPE, FPE_INTDIV, s.rip),
        1 => SigInfo::fault(SIGTRAP, TRAP_TRACE, s.rip),
        3 => SigInfo::kernel(SIGTRAP),
        4 | 5 | 13 => SigInfo::kernel(SIGSEGV),
        6 => SigInfo::fault(SIGILL, ILL_ILLOPN, s.rip),
        10..=12 => SigInfo::kernel(SIGBUS),
        14 => match kind {
            FAULT_BUS => SigInfo::fault(SIGBUS, BUS_ADRERR, addr),
            FAULT_PROTECTION => SigInfo::fault(SIGSEGV, SEGV_ACCERR, addr),
            _ => SigInfo::fault(SIGSEGV, SEGV_MAPERR, addr),
        },
        16 | 19 => SigInfo::fault(SIGFPE, simd_code(), s.rip),
        17 => SigInfo::fault(SIGBUS, BUS_ADRALN, addr),
        7 => SigInfo::kernel(SIGFPE),
        _ => SigInfo::kernel(SIGSEGV),
    };
    let tid = local::tid();
    let mut t = PROCS.lock();
    if let Some(th) = t.threads.get_mut(&tid) {
        th.sig.trap = (vector, error, if vector == 14 { addr } else { 0 });
    }
    // The program dies of it unless it handles it: say why (as the kernel did).
    let pid = local::pid();
    let sig = info.signo();
    let action = t.actions(pid)[sig as usize - 1];
    let blocked = t.threads.get(&tid).is_some_and(|th| th.sig.mask & bit(sig) != 0);
    if action.handler == SIG_DFL || action.handler == SIG_IGN || blocked {
        let what = match (vector, sig) {
            (14, SIGBUS) => "bus error",
            (14, _) => "segmentation fault",
            (0, _) => "divide error",
            (6, _) => "invalid opcode",
            (13, _) => "general protection fault",
            _ => "exception",
        };
        let mut msg = alloc::string::String::new();
        let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("{} at {:#x} (rip {:#x}, error {:#x}, kind {}) in pid {}: process killed", what, addr, s.rip, error, kind, pid));
        syscall(SYS_SERVER_LOG, [msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0, 0]);
    }
    force(&mut t, tid, info);
}

/// The si_code of a floating-point exception, from MXCSR's flags not masked.
fn simd_code() -> i32 {
    let mut mxcsr: u32 = 0;
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut mxcsr as *mut u32, options(nostack)) };
    let raised = mxcsr & 0x3f & !((mxcsr >> 7) & 0x3f);
    if raised & 1 != 0 {
        FPE_FLTINV
    } else if raised & 4 != 0 {
        FPE_FLTDIV
    } else if raised & 8 != 0 {
        FPE_FLTOVF
    } else if raised & 16 != 0 {
        FPE_FLTUND
    } else if raised & 32 != 0 {
        FPE_FLTRES
    } else {
        0
    }
}

// ------------------------------------------------------------------ system calls

/// The result of a signal call in `s`, or None for other calls.
pub fn handle(s: &mut State) -> Option<i64> {
    let (a0, a1, a2, a3, a4, a5) = (s.rdi, s.rsi, s.rdx, s.r10, s.r8, s.r9);
    let result = match s.rax {
        SYS_RT_SIGACTION => sigaction(a0, a1, a2, a3),
        SYS_RT_SIGPROCMASK => sigprocmask(a0, a1, a2, a3),
        SYS_RT_SIGRETURN => return Some(sigreturn(s)),
        SYS_RT_SIGPENDING => sigpending(a0, a1),
        SYS_RT_SIGTIMEDWAIT => sigtimedwait(a0, a1, a2, a3),
        SYS_RT_SIGQUEUEINFO => sigqueue(a0 as i32, None, a1, a2),
        SYS_RT_TGSIGQUEUEINFO => sigqueue(a0 as i32, Some(a1 as i32), a2, a3),
        SYS_RT_SIGSUSPEND => sigsuspend(a0, a1),
        SYS_SIGALTSTACK => sigaltstack(a0, a1, s.rsp),
        SYS_PAUSE => pause(),
        SYS_KILL => kill(a0 as i32, a1),
        SYS_TKILL => tgkill(None, a0 as i32, a1),
        SYS_TGKILL => tgkill(Some(a0 as i32), a1 as i32, a2),
        SYS_RESTART_SYSCALL => restart_syscall(),
        // Until poll, select and epoll are the server's (R6e): the temporary mask is set
        // here, the call passes through without it.
        SYS_PPOLL => read_mask(a3, a4).and_then(|m| masked_with(s, 3, m)),
        SYS_PSELECT6 => pselect_mask(a5).and_then(|m| masked_with(s, 5, m)),
        SYS_EPOLL_PWAIT | SYS_EPOLL_PWAIT2 => read_mask(a4, a5).and_then(|m| masked_with(s, 4, m)),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// A signal mask argument: null is none; a size other than 8 is EINVAL.
fn read_mask(ptr: u64, size: u64) -> Result<Option<u64>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    if size != 8 {
        return Err(EINVAL);
    }
    Ok(Some(usercopy::read::<u64>(ptr)?))
}

fn sigaction(sig: u64, act: u64, oldact: u64, size: u64) -> Result<i64, i64> {
    if size != 8 || sig == 0 || sig > NSIG as u64 {
        return Err(EINVAL);
    }
    let sig = sig as u32;
    let new: Option<SigAction> = if act != 0 { Some(usercopy::read(act)?) } else { None };
    if new.is_some() && bit(sig) & UNBLOCKABLE != 0 {
        return Err(EINVAL);
    }
    let old = {
        let mut t = PROCS.lock();
        let pid = local::pid();
        let old = t.actions(pid)[sig as usize - 1];
        if let Some(mut new) = new {
            new.mask &= !UNBLOCKABLE;
            t.actions_mut(pid)[sig as usize - 1] = new;
            // A signal that is now ignored is dropped wherever it pends (in every process
            // sharing the actions, as Linux's do_sigaction).
            let now_ignored = new.handler == SIG_IGN || (new.handler == SIG_DFL && ignored_by_default(sig));
            if now_ignored {
                let hand = t.procs[&pid].sig.hand;
                let pids: alloc::vec::Vec<Pid> = t.procs.values().filter(|p| p.sig.hand == hand).map(|p| p.pid).collect();
                let mut rt = t.rt_queued;
                for p in pids {
                    let threads = t.procs[&p].threads.clone();
                    t.procs.get_mut(&p).expect("listed").sig.shared.flush(bit(sig), &mut rt);
                    for tid in threads {
                        if let Some(th) = t.threads.get_mut(&tid) {
                            th.sig.pending.flush(bit(sig), &mut rt);
                        }
                    }
                }
                t.rt_queued = rt;
            } else if sig == SIGALRM {
                crate::timer::handled(&mut t, pid);
            }
        }
        old
    };
    if oldact != 0 {
        usercopy::write(oldact, &old)?;
    }
    Ok(0)
}

fn sigprocmask(how: u64, set: u64, oldset: u64, size: u64) -> Result<i64, i64> {
    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIG_SETMASK: u64 = 2;
    if size != 8 {
        return Err(EINVAL);
    }
    let new = if set != 0 { Some(usercopy::read::<u64>(set)?) } else { None };
    let old = {
        let mut t = PROCS.lock();
        let (tid, pid) = process::me();
        let th = t.threads.get_mut(&tid).ok_or(ESRCH)?;
        let old = th.sig.mask;
        if let Some(n) = new {
            th.sig.mask = match how {
                SIG_BLOCK => old | n,
                SIG_UNBLOCK => old & !n,
                SIG_SETMASK => n,
                _ => return Err(EINVAL),
            } & !UNBLOCKABLE;
            let mask = th.sig.mask;
            if mask & !old != 0 {
                // What it blocks now another thread may take.
                retarget(&t, pid);
            }
            if old & !mask != 0 {
                // What it no longer blocks it may take now: it looks before it returns.
                let p = &t.procs[&pid];
                if deliverable(&t.threads[&tid], p) != 0 {
                    t.kick(tid);
                }
            }
        } else if how > SIG_SETMASK && set != 0 {
            return Err(EINVAL);
        }
        old
    };
    if oldset != 0 {
        usercopy::write(oldset, &old)?;
    }
    Ok(0)
}

fn sigpending(set: u64, size: u64) -> Result<i64, i64> {
    if size > 8 {
        return Err(EINVAL);
    }
    let pending = {
        let t = PROCS.lock();
        let (tid, pid) = process::me();
        let th = t.threads.get(&tid).ok_or(ESRCH)?;
        (th.sig.pending.set | t.procs[&pid].sig.shared.set) & th.sig.mask
    };
    let bytes = pending.to_le_bytes();
    usercopy::to_program(set, &bytes[..size as usize])?;
    Ok(0)
}

/// A word nothing ever wakes: interruptible waits on it end only by a kick (or a deadline).
static NEVER: AtomicU32 = AtomicU32::new(0);

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64
}

fn sigtimedwait(set: u64, info: u64, timeout: u64, size: u64) -> Result<i64, i64> {
    if size != 8 {
        return Err(EINVAL);
    }
    let wanted = usercopy::read::<u64>(set)? & !UNBLOCKABLE;
    let deadline = if timeout != 0 {
        let [sec, nsec]: [i64; 2] = usercopy::read(timeout)?;
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return Err(EINVAL);
        }
        Some(now().saturating_add((sec as u64).saturating_mul(1_000_000_000)).saturating_add(nsec as u64))
    } else {
        None
    };
    let (tid, pid) = process::me();
    if wanted & bit(SIGALRM) != 0 {
        crate::timer::handled(&mut PROCS.lock(), pid);
    }
    let found = loop {
        {
            let mut t = PROCS.lock();
            let th = t.threads.get(&tid).ok_or(ESRCH)?;
            let ready = (th.sig.pending.set | t.procs[&pid].sig.shared.set) & wanted;
            if ready != 0 {
                let sig = pick(ready);
                let mut rt = t.rt_queued;
                let info = {
                    let th = t.threads.get_mut(&tid).expect("checked");
                    th.sig.waiting_for = 0;
                    if th.sig.pending.set & bit(sig) != 0 {
                        th.sig.pending.take(sig, &mut rt)
                    } else {
                        t.procs.get_mut(&pid).expect("listed").sig.shared.take(sig, &mut rt)
                    }
                };
                t.rt_queued = rt;
                if sig == SIGALRM {
                    crate::timer::taken(&mut t, pid);
                }
                break Ok(info);
            }
            let p = &t.procs[&pid];
            let interrupted = deliverable(th, p) != 0 || p.exiting.is_some();
            let th = t.threads.get_mut(&tid).expect("checked");
            if interrupted {
                th.sig.waiting_for = 0;
                break Err(EINTR);
            }
            th.sig.waiting_for = wanted;
        }
        if deadline.is_some_and(|d| now() >= d) {
            break Err(EAGAIN);
        }
        let r = process::wait_word(&NEVER, 0, deadline.unwrap_or(0), true);
        if r == -EINTR {
            // A kick: the signal waited for (taken above), another one, or a spurious one.
            let mut t = PROCS.lock();
            let th = t.threads.get(&tid).ok_or(ESRCH)?;
            let ready = (th.sig.pending.set | t.procs[&pid].sig.shared.set) & wanted;
            if ready == 0 {
                t.threads.get_mut(&tid).expect("checked").sig.waiting_for = 0;
                break Err(EINTR);
            }
        }
    };
    let found = found?;
    if info != 0 {
        usercopy::write(info, &found.0)?;
    }
    Ok(found.signo() as i64)
}

/// Sets a temporary mask for the calling thread's wait: it stays until the thread goes back
/// to its program (then the frame of a handler it let in restores the old one, or it is put
/// back first).
fn set_temporary_mask(mask: u64) {
    let (tid, pid) = process::me();
    let mut t = PROCS.lock();
    let Some(th) = t.threads.get_mut(&tid) else { return };
    let own = *th.sig.restore.get_or_insert(th.sig.mask);
    th.sig.mask = mask & !UNBLOCKABLE;
    local::get().flags.fetch_or(local::RESTORE_MASK, Ordering::Relaxed);
    if th.sig.mask & !own != 0 {
        retarget(&t, pid);
    }
}

/// After a call that waited with a temporary mask did not end by EINTR: the caller's own
/// mask is back at once (a signal it held off is delivered only if that mask lets it).
fn drop_temporary_mask() {
    let (tid, pid) = process::me();
    let mut t = PROCS.lock();
    let Some(th) = t.threads.get_mut(&tid) else { return };
    if let Some(own) = th.sig.restore.take() {
        let temporary = th.sig.mask;
        th.sig.mask = own;
        if own & !temporary != 0 {
            retarget(&t, pid);
        }
    }
    local::get().flags.fetch_and(!local::RESTORE_MASK, Ordering::Relaxed);
}

/// Runs `wait` with `mask` (if any) as the calling thread's temporary signal mask, as
/// rt_sigsuspend, ppoll, pselect6 and epoll_pwait do: the interface the descriptor table's
/// waits use (R6e).
pub fn with_mask<T>(mask: Option<u64>, wait: impl FnOnce() -> Result<T, i64>) -> Result<T, i64> {
    let Some(mask) = mask else { return wait() };
    set_temporary_mask(mask);
    // The temporary mask may let a pending signal in: the wait must see it.
    let result = if pending() { Err(EINTR) } else { wait() };
    if !matches!(result, Err(EINTR)) {
        drop_temporary_mask();
    }
    result
}

/// pselect6's sixth argument: a pointer to { mask pointer, size }.
fn pselect_mask(arg: u64) -> Result<Option<u64>, i64> {
    if arg == 0 {
        return Ok(None);
    }
    let [ptr, size]: [u64; 2] = usercopy::read(arg)?;
    read_mask(ptr, size)
}

/// The argument register `index` (3: r10, 4: r8, 5: r9).
fn arg_mut(s: &mut State, index: usize) -> &mut u64 {
    match index {
        3 => &mut s.r10,
        4 => &mut s.r8,
        _ => &mut s.r9,
    }
}

/// A call passed through to the kernel (until R6e) with its mask argument (register
/// `index`) taken over here: the kernel sees none, the thread waits with the mask.
fn masked_with(s: &mut State, index: usize, mask: Option<u64>) -> Result<i64, i64> {
    let to_result = |r: i64| if r < 0 { Err(-r) } else { Ok(r) };
    let Some(mask) = mask else { return to_result(crate::pass_through_value(s)) };
    let saved = *arg_mut(s, index);
    let result = with_mask(Some(mask), || {
        *arg_mut(s, index) = 0;
        to_result(crate::pass_through_value(s))
    });
    *arg_mut(s, index) = saved;
    result
}

fn sigsuspend(mask: u64, size: u64) -> Result<i64, i64> {
    let mask = read_mask(mask, size)?.ok_or(EFAULT)?;
    with_mask(Some(mask), || loop {
        if process::wait_word(&NEVER, 0, 0, true) == -EINTR {
            return Err(EINTR);
        }
    })
}

fn pause() -> Result<i64, i64> {
    loop {
        if process::wait_word(&NEVER, 0, 0, true) == -EINTR {
            return Err(EINTR);
        }
    }
}

/// restart_syscall: a relative sleep goes on to its first deadline.
fn restart_syscall() -> Result<i64, i64> {
    let block = {
        let mut t = PROCS.lock();
        t.threads.get_mut(&local::tid()).and_then(|th| th.sig.block.take())
    };
    match block {
        Some(b) => crate::time::sleep_rest(b),
        None => Err(EINTR),
    }
}

/// Keeps the rest of an interrupted relative sleep for restart_syscall.
pub fn save_block(block: RestartBlock) {
    let mut t = PROCS.lock();
    if let Some(th) = t.threads.get_mut(&local::tid()) {
        th.sig.block = Some(block);
    }
}

fn sigaltstack(ss: u64, oss: u64, sp: u64) -> Result<i64, i64> {
    let new: Option<StackT> = if ss != 0 { Some(usercopy::read(ss)?) } else { None };
    let tid = local::tid();
    let old = {
        let mut t = PROCS.lock();
        let th = t.threads.get_mut(&tid).ok_or(ESRCH)?;
        let alt = th.sig.alt;
        let old = StackT { sp: alt.sp, flags: alt_flags(&alt, sp) | if alt.autodisarm { SS_AUTODISARM } else { 0 }, _pad: 0, size: alt.size };
        if let Some(n) = new {
            if on_stack(&alt, sp) {
                return Err(EPERM);
            }
            let mode = n.flags & !SS_AUTODISARM;
            match mode {
                SS_DISABLE => th.sig.alt = AltStack::default(),
                0 | SS_ONSTACK => {
                    if n.size < MINSIGSTKSZ {
                        return Err(ENOMEM);
                    }
                    if n.sp.checked_add(n.size).is_none_or(|e| e > SHARED_BASE) {
                        return Err(EFAULT);
                    }
                    th.sig.alt = AltStack { sp: n.sp, size: n.size, autodisarm: n.flags & SS_AUTODISARM != 0 };
                }
                _ => return Err(EINVAL),
            }
        }
        old
    };
    if oss != 0 {
        usercopy::write(oss, &old)?;
    }
    Ok(0)
}

/// kill(pid, sig): a process (a thread's id names its process), the caller's process group
/// (0), every process but pid 1 and the caller (-1), or a process group (-pgid).
fn kill(pid: i32, sig: u64) -> Result<i64, i64> {
    if sig > NSIG as u64 {
        return Err(EINVAL);
    }
    let sig = sig as u32;
    let me = local::pid();
    let mut t = PROCS.lock();
    let targets: alloc::vec::Vec<Pid> = match pid {
        p if p > 0 => {
            let p = p as Pid;
            let target = if t.procs.contains_key(&p) { p } else { t.threads.get(&p).map(|th| th.pid).ok_or(ESRCH)? };
            if t.procs[&target].zombie.is_some() && t.procs[&target].threads.is_empty() {
                // A zombie takes no signal (but exists).
                return Ok(0);
            }
            alloc::vec![target]
        }
        0 => {
            let pgid = t.procs[&me].pgid;
            t.procs.values().filter(|p| p.pgid == pgid && p.zombie.is_none()).map(|p| p.pid).collect()
        }
        -1 => t.procs.values().filter(|p| p.pid > 1 && p.pid != me && p.zombie.is_none()).map(|p| p.pid).collect(),
        i32::MIN => return Err(ESRCH),
        p => {
            let pgid = p.unsigned_abs();
            t.procs.values().filter(|q| q.pgid == pgid && q.zombie.is_none()).map(|q| q.pid).collect()
        }
    };
    if targets.is_empty() {
        return Err(ESRCH);
    }
    if sig != 0 {
        for target in targets {
            post_process(&mut t, target, SigInfo::user(sig, SI_USER, me), false);
        }
    }
    Ok(0)
}

/// tgkill(tgid, tid, sig) and tkill (`tgid` None): a signal for one thread.
fn tgkill(tgid: Option<i32>, tid: i32, sig: u64) -> Result<i64, i64> {
    if sig > NSIG as u64 || tid <= 0 || tgid.is_some_and(|g| g <= 0) {
        return Err(EINVAL);
    }
    let mut t = PROCS.lock();
    let th = t.threads.get(&(tid as Pid)).filter(|th| !th.exited).ok_or(ESRCH)?;
    let pid = th.pid;
    if tgid.is_some_and(|g| g as Pid != pid) {
        return Err(ESRCH);
    }
    if sig != 0 {
        post_thread(&mut t, pid, tid as Pid, SigInfo::user(sig as u32, SI_TKILL, local::pid()));
    }
    Ok(0)
}

/// rt_sigqueueinfo(pid, sig, info) and rt_tgsigqueueinfo(tgid, tid, sig, info): a signal
/// with the caller's data. Only SI_QUEUE-like codes (negative) may be sent to another
/// process (no one may pretend to be the kernel or kill).
fn sigqueue(pid: i32, tid: Option<i32>, sig: u64, uinfo: u64) -> Result<i64, i64> {
    if sig == 0 || sig > NSIG as u64 {
        return Err(EINVAL);
    }
    let mut raw: [u64; 16] = usercopy::read(uinfo)?;
    let code = raw[1] as u32 as i32;
    let me = local::pid();
    if (code >= 0 || code == SI_TKILL) && pid as Pid != me {
        return Err(EPERM);
    }
    raw[0] = sig;
    let info = SigInfo(raw);
    let mut t = PROCS.lock();
    match tid {
        None => {
            if pid <= 0 {
                return Err(ESRCH);
            }
            let p = pid as Pid;
            let target = if t.procs.contains_key(&p) { p } else { t.threads.get(&p).map(|th| th.pid).ok_or(ESRCH)? };
            post(&mut t, target, None, info, false, true).map(|_| 0)
        }
        Some(tid) => {
            let th = t.threads.get(&(tid as Pid)).filter(|th| !th.exited).ok_or(ESRCH)?;
            if th.pid != pid as Pid {
                return Err(ESRCH);
            }
            let owner = th.pid;
            post(&mut t, owner, Some(tid as Pid), info, false, true).map(|_| 0)
        }
    }
}
