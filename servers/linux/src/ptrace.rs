//! ptrace(2), process_vm_readv and process_vm_writev, and /proc/<pid>/mem (ADR 0019): the
//! tracer and tracee relations, the stops and their reports, the requests on a stopped
//! tracee's registers, and other processes' memory.
//!
//! A relation is per thread, as Linux's: a traced thread's record (`Tracee`, in its
//! `process::Thread`) names its tracer thread; the tracer's process's `wait` sees the stops
//! and ends of its tracees (`process::wait_child`); the tracer thread's end detaches them.
//! A tracee stops in its own server thread (`stop`), where its program's registers are the
//! `State` the server holds and its FPU registers are live in the CPU: the stop copies them,
//! with the FS base, into the record, reports itself and sleeps (killable only) on its
//! process's trace word; the tracer's requests read and change the record, a resume wakes
//! the word, and the tracee loads the record back before its program runs. So no kernel
//! call reads another thread's registers. The kernel gives the server what it alone has:
//! another process's memory (`SYS_VM_ACCESS`, with Linux's FOLL_FORCE for ptrace and
//! /proc/<pid>/mem) and a thread's hardware breakpoints (`SYS_THREAD_DEBUG`, which the
//! tracee's thread sets for itself from what the tracer poked).
//!
//! Where the stops come from: the system call path (`syscall_entry`, `syscall_exit`, in
//! `serve`), signal delivery (`signal::deliver`: signal-delivery-stops, group-stop traps,
//! `PTRACE_INTERRUPT` and `PTRACE_LISTEN`'s notifications), and the events of clone, exec
//! and exit (`event`, `exec_event`, `exit_event`). A traced thread's end becomes the zombie
//! its tracer reaps (`thread_gone`, `leader_ended`): its record moves onto its tracer
//! thread's list, so no exit allocates.

use crate::heap;
use crate::local;
use crate::process::{self, After, Pid, Table, Thread, PROCS};
use crate::signal::{self, SigInfo};
use crate::syscall;
use crate::usercopy;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use restricted::*;

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EIO: i64 = 5;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ENODEV: i64 = 19;
const ENOSYS: i64 = 38;

pub const SYS_PTRACE: u64 = 101;
const SYS_PROCESS_VM_READV: u64 = 310;
const SYS_PROCESS_VM_WRITEV: u64 = 311;

const PTRACE_TRACEME: u64 = 0;
const PTRACE_PEEKTEXT: u64 = 1;
const PTRACE_PEEKDATA: u64 = 2;
const PTRACE_PEEKUSR: u64 = 3;
const PTRACE_POKETEXT: u64 = 4;
const PTRACE_POKEDATA: u64 = 5;
const PTRACE_POKEUSR: u64 = 6;
const PTRACE_CONT: u64 = 7;
const PTRACE_KILL: u64 = 8;
const PTRACE_SINGLESTEP: u64 = 9;
const PTRACE_GETREGS: u64 = 12;
const PTRACE_SETREGS: u64 = 13;
const PTRACE_GETFPREGS: u64 = 14;
const PTRACE_SETFPREGS: u64 = 15;
const PTRACE_ATTACH: u64 = 16;
const PTRACE_DETACH: u64 = 17;
const PTRACE_SYSCALL: u64 = 24;
const PTRACE_ARCH_PRCTL: u64 = 30;
const PTRACE_SYSEMU: u64 = 31;
const PTRACE_SYSEMU_SINGLESTEP: u64 = 32;
const PTRACE_SETOPTIONS: u64 = 0x4200;
const PTRACE_GETEVENTMSG: u64 = 0x4201;
const PTRACE_GETSIGINFO: u64 = 0x4202;
const PTRACE_SETSIGINFO: u64 = 0x4203;
const PTRACE_GETREGSET: u64 = 0x4204;
const PTRACE_SETREGSET: u64 = 0x4205;
const PTRACE_SEIZE: u64 = 0x4206;
const PTRACE_INTERRUPT: u64 = 0x4207;
const PTRACE_LISTEN: u64 = 0x4208;
const PTRACE_PEEKSIGINFO: u64 = 0x4209;
const PTRACE_GETSIGMASK: u64 = 0x420a;
const PTRACE_SETSIGMASK: u64 = 0x420b;
const PTRACE_GET_SYSCALL_INFO: u64 = 0x420e;
const PTRACE_GET_RSEQ_CONFIGURATION: u64 = 0x420f;

pub const O_TRACESYSGOOD: u32 = 1;
pub const O_TRACEFORK: u32 = 2;
pub const O_TRACEVFORK: u32 = 4;
pub const O_TRACECLONE: u32 = 8;
pub const O_TRACEEXEC: u32 = 0x10;
pub const O_TRACEVFORKDONE: u32 = 0x20;
pub const O_TRACEEXIT: u32 = 0x40;
// (0x80, PTRACE_O_TRACESECCOMP, is accepted and never fires: there is no seccomp.)
pub const O_EXITKILL: u32 = 0x10_0000;
const O_SUSPEND_SECCOMP: u32 = 0x20_0000;
/// The options Linux knows (`PTRACE_O_MASK`).
const O_MASK: u64 = 0xff | O_EXITKILL as u64 | O_SUSPEND_SECCOMP as u64;

pub const EVENT_FORK: u32 = 1;
pub const EVENT_VFORK: u32 = 2;
pub const EVENT_CLONE: u32 = 3;
pub const EVENT_EXEC: u32 = 4;
pub const EVENT_VFORK_DONE: u32 = 5;
pub const EVENT_EXIT: u32 = 6;
pub const EVENT_STOP: u32 = 128;

/// `PTRACE_GETEVENTMSG` at a syscall stop (Linux 5.3's `PTRACE_EVENTMSG_SYSCALL_*`).
const EVENTMSG_SYSCALL_ENTRY: u64 = 1;
const EVENTMSG_SYSCALL_EXIT: u64 = 2;
/// `waitid`'s and `SIGCHLD`'s si_code of a ptrace stop.
pub const CLD_TRAPPED: i32 = 4;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const TRAP_BRKPT: i32 = 1;
const TRAP_TRACE: i32 = 2;
const TRAP_HWBKPT: i32 = 4;

const TF: u64 = 0x100;
/// The flags a tracer may set (Linux's FLAG_MASK: CF PF AF ZF SF TF DF OF RF AC).
const FLAG_MASK: u64 = 0x1 | 0x4 | 0x10 | 0x40 | 0x80 | 0x100 | 0x400 | 0x800 | 0x1_0000 | 0x4_0000;
/// Linux's user code and stack selectors on x86-64 (what GETREGS shows).
const USER_CS: u64 = 0x33;
const USER_SS: u64 = 0x2b;
/// `struct user`: where its debug registers are, and its size.
const USER_DEBUGREG: u64 = 848;
const USER_SIZE: u64 = 912;
const NT_PRSTATUS: u64 = 1;
const NT_PRFPREG: u64 = 2;
const NT_X86_XSTATE: u64 = 0x202;

/// The stop a tracee is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// A signal-delivery-stop: its signal is the tracer's to change or suppress.
    Signal,
    /// A group-stop or `PTRACE_EVENT_STOP` trap (Linux's jobctl trap, `CLD_STOPPED`).
    Group,
    SyscallEntry,
    SyscallExit,
    /// A `PTRACE_EVENT_*` stop, or the trap after a stepped-into handler's frame (0).
    Event(u32),
}

/// An `fxsave` image (the program's FPU and SSE registers).
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct Fpu(pub [u8; 512]);

/// A traced thread's record (Linux's `task->ptrace`, its jobctl trap bits, `exit_code`,
/// `last_siginfo`, `ptrace_message`, and the registers while it is stopped). Kept until the
/// thread is back from its stop once it was detached, and, once the thread ended, as the
/// zombie its tracer reaps.
pub struct Tracee {
    /// The tracer thread; 0 once detached (the record goes when the thread runs again).
    pub tracer: Pid,
    pub seized: bool,
    pub options: u32,
    /// How the tracer last resumed it: single steps, syscall stops, PTRACE_SYSEMU.
    step: bool,
    syscall: bool,
    sysemu: bool,
    /// TF is set because of single-stepping (Linux's TIF_FORCED_TF: hidden from GETREGS).
    forced_tf: bool,
    /// The stop it is in, if any.
    pub stop: Option<Stop>,
    /// What `wait` reports of the stop (0 once reported).
    pub code: u32,
    /// The tracer resumed it, with this signal (the stop ends).
    resumed: Option<u32>,
    /// `PTRACE_LISTEN`: stopped, but not reported and not resumed until notified.
    pub listening: bool,
    /// Linux's JOBCTL_TRAP_STOP (`PTRACE_INTERRUPT`, an attach to a stopped thread, a
    /// seized child's first stop) and JOBCTL_TRAP_NOTIFY (a group stop began or ended).
    pub trap_stop: bool,
    pub trap_notify: bool,
    /// `PTRACE_GETSIGINFO`'s siginfo (None: a stop without one, EINVAL).
    siginfo: Option<SigInfo>,
    /// `PTRACE_GETEVENTMSG`'s value.
    message: u64,
    /// The program's registers while stopped, orig_rax, FPU state and FS base.
    regs: State,
    orig_rax: u64,
    fpu: Fpu,
    fs_base: u64,
    /// The debug registers as the tracer poked them (DR0-DR3, DR6, DR7), and whether the
    /// thread's must be set again when it resumes.
    debug: [u64; 6],
    debug_changed: bool,
    /// Once its thread ended: the zombie's (tid, pid, wait status), on its tracer's list.
    pub zombie: Option<(Pid, Pid, i32)>,
    pub next: Option<Box<Tracee>>,
}

impl Tracee {
    fn new(tracer: Pid, seized: bool, options: u32) -> Tracee {
        Tracee {
            tracer,
            seized,
            options,
            step: false,
            syscall: false,
            sysemu: false,
            forced_tf: false,
            stop: None,
            code: 0,
            resumed: None,
            listening: false,
            trap_stop: false,
            trap_notify: false,
            siginfo: None,
            message: 0,
            regs: State::default(),
            orig_rax: u64::MAX,
            fpu: Fpu([0; 512]),
            fs_base: 0,
            debug: [0; 6],
            debug_changed: false,
            zombie: None,
            next: None,
        }
    }

    /// Stopped and waiting for its tracer (Linux's TASK_TRACED and not LISTENING).
    fn stopped(&self) -> bool {
        self.stop.is_some() && self.resumed.is_none() && !self.listening
    }

    /// The Local flags its thread runs with (`local::TRACE_*`).
    fn flags(&self) -> u32 {
        let mut f = 0;
        if self.syscall {
            f |= local::TRACE_SYSCALL;
        }
        if self.sysemu {
            f |= local::TRACE_SYSEMU;
        }
        if self.step {
            f |= local::TRACE_STEP;
        }
        f
    }
}

/// The thread's live record (a tracer's), if it is traced.
pub fn record(th: &Thread) -> Option<&Tracee> {
    th.trace.as_deref().filter(|r| r.tracer != 0)
}

fn record_mut(th: &mut Thread) -> Option<&mut Tracee> {
    th.trace.as_deref_mut().filter(|r| r.tracer != 0)
}

/// Whether thread `tid` is traced.
pub fn traced(t: &Table, tid: Pid) -> bool {
    t.threads.get(&tid).is_some_and(|th| record(th).is_some())
}

/// Whether a thread of process `pid` is traced.
pub fn process_traced(t: &Table, pid: Pid) -> bool {
    t.procs.get(&pid).is_some_and(|p| p.threads.iter().any(|tid| traced(t, *tid)))
}

/// Whether thread `tid` is traced by a tracer in another process than its real parent
/// (Linux's `ptrace_reparented`): its real parent then still hears of its group stops.
pub fn reparented(t: &Table, tid: Pid) -> bool {
    let Some(th) = t.threads.get(&tid) else { return false };
    let Some(r) = record(th) else { return false };
    let tracer_pid = t.threads.get(&r.tracer).map_or(0, |tr| tr.pid);
    t.procs.get(&th.pid).is_some_and(|p| p.ppid != tracer_pid)
}

/// Whether thread `tid` is traced by a seized tracer.
pub fn seized(t: &Table, tid: Pid) -> bool {
    t.threads.get(&tid).and_then(record).is_some_and(|r| r.seized)
}

/// Wakes the stopped tracees of process `pid` to look at their records.
fn wake(t: &Table, pid: Pid) {
    if let Some(p) = t.procs.get(&pid) {
        process::wake_word(&p.words.trace);
    }
}

/// Tells tracer thread `tracer` that its tracee `tid` stopped (`why`: `CLD_TRAPPED`, or
/// `CLD_STOPPED` for a group-stop trap) with `code`, or ended (`exited`: its wait status):
/// `SIGCHLD` unless the tracer's process ignores it (or set `SA_NOCLDSTOP`, for a stop), and
/// a wakeup of its process's waits (Linux's do_notify_parent_cldstop, do_notify_parent).
fn notify_tracer(t: &mut Table, tracer: Pid, tid: Pid, pid: Pid, why: i32, code: u32, exited: Option<i32>) {
    let Some(tracer_pid) = t.threads.get(&tracer).map(|th| th.pid) else { return };
    let Some(tp) = t.procs.get(&tracer_pid) else { return };
    let chld = t.actions(tracer_pid)[signal::SIGCHLD as usize - 1];
    let words = tp.words.clone();
    let send = chld.handler != signal::SIG_IGN && (exited.is_some() || chld.flags & signal::SA_NOCLDSTOP == 0);
    if send {
        let mut info = ProcInfo::default();
        if let Some(p) = t.procs.get(&pid) {
            syscall(SYS_PROC_INFO, [p.handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
        }
        let si = match exited {
            Some(status) if status & 0x7f == 0 => SigInfo::chld(signal::SIGCHLD, signal::CLD_EXITED, tid, (status >> 8) & 0xff, info.user_ns, info.system_ns),
            Some(status) => SigInfo::chld(signal::SIGCHLD, signal::CLD_KILLED, tid, status & 0x7f, info.user_ns, info.system_ns),
            None => SigInfo::chld(signal::SIGCHLD, why, tid, (code & 0x7f) as i32, info.user_ns, info.system_ns),
        };
        signal::post_process(t, tracer_pid, si, false);
    }
    process::wake_word(&words.child);
}

/// The siginfo of an event or syscall stop (Linux's ptrace_do_notify): `signo`, the stop's
/// code, the tracee's id.
fn notify_info(signo: u32, code: u32, tid: Pid) -> SigInfo {
    SigInfo::user(signo, code as i32, tid)
}

// ------------------------------------------------------------------ the tracee's side

/// What the tracer resumed a stopped thread with.
pub struct Resumed {
    /// The signal (0: none).
    pub sig: u32,
    /// The stop's siginfo as it is now (`PTRACE_SETSIGINFO` may have changed it).
    pub info: Option<SigInfo>,
    /// orig_rax as it is now (a syscall-entry stop's call, -1 to skip it).
    pub orig_rax: u64,
    /// The tracer (0 if it let go of the thread): a changed signal's sender.
    pub tracer: Pid,
}

/// Stops the calling thread for its tracer (Linux's ptrace_stop) until the tracer resumes
/// it: the stop `kind`, what `wait` reports (`code`), `why` for the tracer's SIGCHLD
/// (`CLD_TRAPPED`, or `signal::CLD_STOPPED` for a group-stop trap), the siginfo the tracer
/// may read (None: EINVAL for it) and `PTRACE_GETEVENTMSG`'s message. `s` is the program's
/// registers (and `orig_rax` the call being made, or -1), loaded back changed as the tracer
/// left them. None if it did not stop: it is not traced, or it was killed (then it ends on
/// its way back to its program).
pub fn stop(s: &mut State, orig_rax: u64, kind: Stop, code: u32, why: i32, info: Option<SigInfo>, message: u64) -> Option<Resumed> {
    let (tid, pid) = process::me();
    let fs_base = syscall(SYS_THREAD_FS, [0, 0, 0, 0, 0, 0]) as u64;
    let fpu = signal::fxsave();
    let words = {
        let mut t = PROCS.lock();
        let words = t.procs.get(&pid)?.words.clone();
        let th = t.threads.get_mut(&tid)?;
        if th.exited {
            return None;
        }
        let r = record_mut(th)?;
        r.regs = *s;
        r.orig_rax = orig_rax;
        r.fpu = Fpu(fpu.0);
        r.fs_base = fs_base;
        r.stop = Some(kind);
        r.code = code;
        r.siginfo = info;
        r.message = message;
        r.resumed = None;
        r.listening = false;
        // Any trap satisfies a pending PTRACE_INTERRUPT. (A notice is taken by the job-control
        // trap that reports it, when it is decided: `jobctl_trap`; one that came since then
        // waits for the next.)
        r.trap_stop = false;
        let tracer = r.tracer;
        notify_tracer(&mut t, tracer, tid, pid, why, code, None);
        words
    };
    let mut killed = false;
    loop {
        let seen = words.trace.load(Ordering::Acquire);
        {
            let t = PROCS.lock();
            match t.threads.get(&tid).and_then(|th| th.trace.as_deref()) {
                Some(r) if r.resumed.is_none() && r.tracer != 0 => {}
                _ => break,
            }
        }
        // Killable only (Linux's TASK_TRACED): a kick does not end the stop, a kill does.
        if process::wait_word(&words.trace, seen, 0, false) == -process::EINTR {
            killed = true;
            break;
        }
    }
    let (resumed, fpu, fs, debug, flags, gone) = {
        let mut t = PROCS.lock();
        let th = t.threads.get_mut(&tid)?;
        let r = th.trace.as_deref_mut()?;
        let sig = if killed { 0 } else { r.resumed.take().unwrap_or(0) };
        r.resumed = None;
        *s = r.regs;
        r.stop = None;
        r.listening = false;
        r.code = 0;
        r.message = 0;
        let resumed = Resumed { sig, info: r.siginfo.take(), orig_rax: r.orig_rax, tracer: r.tracer };
        let debug = core::mem::take(&mut r.debug_changed).then_some(r.debug);
        let detached = r.tracer == 0;
        let flags = if detached { 0 } else { r.flags() };
        let (fpu, fs) = (r.fpu, r.fs_base);
        // Detached during the stop: the record goes now (after the lock).
        let gone = if detached { th.trace.take() } else { None };
        (resumed, fpu, fs, debug, flags, gone)
    };
    drop(gone);
    signal::fxrstor(signal::FxArea(fpu.0));
    if fs != fs_base {
        syscall(SYS_THREAD_FS, [1, fs, 0, 0, 0, 0]);
    }
    if let Some(d) = debug {
        set_debug(&d);
    }
    set_flags(flags);
    Some(resumed)
}

/// The calling thread's Local trace flags (`local::TRACE_*`), which only it writes.
fn set_flags(flags: u32) {
    let l = local::get();
    let all = local::TRACE_SYSCALL | local::TRACE_SYSEMU | local::TRACE_STEP;
    let old = l.flags.load(Ordering::Relaxed);
    l.flags.store((old & !all) | flags, Ordering::Relaxed);
}

/// Gives the calling thread's program the hardware breakpoints its tracer poked (DR0-DR3
/// and DR7 of `debug`; checked when poked, as the kernel checks them).
fn set_debug(debug: &[u64; 6]) {
    let regs = [debug[0], debug[1], debug[2], debug[3], debug[5]];
    syscall(SYS_THREAD_DEBUG, [regs.as_ptr() as u64, 0, 0, 0, 0, 0]);
}

/// A record its tracer let go of while the thread ran (detached, or its tracer ended): the
/// thread forgets it, and its breakpoints with it (on its way back to its program, from
/// `signal::deliver`).
pub fn settle() {
    let tid = local::tid();
    let gone = {
        let mut t = PROCS.lock();
        let Some(th) = t.threads.get_mut(&tid) else { return };
        if !th.trace.as_ref().is_some_and(|r| r.tracer == 0 && r.stop.is_none()) {
            return;
        }
        th.trace.take()
    };
    if let Some(r) = gone {
        if r.debug[5] != 0 || r.debug_changed {
            set_debug(&[0; 6]);
        }
    }
    set_flags(0);
}

/// `serve`'s syscall-entry stop (the thread's `TRACE_SYSCALL` or `TRACE_SYSEMU` flag is
/// set): what to do with the call `s` makes.
pub enum Entry {
    /// Make call `nr` (the tracer may have changed it and the arguments in `s`).
    Run(u64),
    /// Skip it: its result is `s.rax` (-ENOSYS unless the tracer set another); orig_rax
    /// stays what the tracer left (-1, the call's own for PTRACE_SYSEMU).
    Skip(u64),
}

pub fn syscall_entry(s: &mut State) -> Entry {
    let nr = s.rax;
    let tid = local::tid();
    let (sysgood, sysemu) = {
        let t = PROCS.lock();
        match t.threads.get(&tid).and_then(record) {
            Some(r) => (r.options & O_TRACESYSGOOD != 0, r.sysemu),
            None => {
                drop(t);
                settle();
                return Entry::Run(nr);
            }
        }
    };
    let code = signal::SIGTRAP | if sysgood { 0x80 } else { 0 };
    // Linux shows -ENOSYS in rax and the call in orig_rax.
    s.rax = (-ENOSYS) as u64;
    match stop(s, nr, Stop::SyscallEntry, code, CLD_TRAPPED, Some(notify_info(signal::SIGTRAP, code, tid)), EVENTMSG_SYSCALL_ENTRY) {
        None => {
            s.rax = nr;
            // Killed during it: the call is not made (the thread ends).
            if killed() { Entry::Skip(nr) } else { Entry::Run(nr) }
        }
        Some(res) => {
            send_self(res.sig);
            if sysemu || res.orig_rax as i32 == -1 || killed() {
                Entry::Skip(res.orig_rax)
            } else {
                Entry::Run(res.orig_rax)
            }
        }
    }
}

/// `serve`'s report of a call's end (its `TRACE_SYSCALL` or `TRACE_STEP` flag is set):
/// the syscall-exit stop (`s.rax` the result, which the tracer may change), or, while
/// single-stepping, the step's SIGTRAP (`syscall` clears TF, so the step over it is
/// reported here: x86's user_single_step_report). Whether a signal waits now.
pub fn syscall_exit(s: &mut State, nr: u64) -> bool {
    let flags = local::get().flags.load(Ordering::Relaxed);
    if flags & local::TRACE_SYSEMU != 0 {
        return false;
    }
    let tid = local::tid();
    if flags & local::TRACE_STEP != 0 {
        let mut t = PROCS.lock();
        if t.threads.get(&tid).and_then(record).is_none() {
            return false;
        }
        signal::force_thread(&mut t, tid, SigInfo::fault(signal::SIGTRAP, TRAP_BRKPT, s.rip));
        return true;
    }
    let sysgood = {
        let t = PROCS.lock();
        match t.threads.get(&tid).and_then(record) {
            Some(r) => r.options & O_TRACESYSGOOD != 0,
            None => {
                drop(t);
                settle();
                return false;
            }
        }
    };
    let code = signal::SIGTRAP | if sysgood { 0x80 } else { 0 };
    // An interruptible wait's EINTR, as Linux's: the restart code it stands for.
    let raw = s.rax;
    s.rax = signal::shown_result(nr, raw as i64) as u64;
    let shown = s.rax;
    match stop(s, nr, Stop::SyscallExit, code, CLD_TRAPPED, Some(notify_info(signal::SIGTRAP, code, tid)), EVENTMSG_SYSCALL_EXIT) {
        None => {
            s.rax = raw;
            false
        }
        Some(res) => {
            if s.rax == shown {
                s.rax = raw;
            }
            send_self(res.sig)
        }
    }
}

/// Whether the calling thread was killed (it ends on its way back to its program).
fn killed() -> bool {
    let tid = local::tid();
    PROCS.lock().threads.get(&tid).is_none_or(|th| th.exited)
}

/// A signal the tracer resumed a syscall or event stop with: sent to the thread itself
/// (Linux's `send_sig(signr, current, 1)`, SI_KERNEL). Whether one was.
fn send_self(sig: u32) -> bool {
    if sig == 0 || sig > signal::NSIG {
        return false;
    }
    let (tid, pid) = process::me();
    let mut t = PROCS.lock();
    signal::post_thread(&mut t, pid, tid, SigInfo::kernel(sig));
    true
}

/// A `PTRACE_EVENT_*` stop inside a system call (`s` holds it: rax is the call's number
/// until it returns), if the calling thread is traced with the event's option: Linux's
/// ptrace_event(event, message). The call's number goes back into `s.rax`.
pub fn event(s: &mut State, event: u32, message: u64) {
    let tid = local::tid();
    let option = match event {
        EVENT_FORK => O_TRACEFORK,
        EVENT_VFORK => O_TRACEVFORK,
        EVENT_CLONE => O_TRACECLONE,
        EVENT_EXEC => O_TRACEEXEC,
        EVENT_VFORK_DONE => O_TRACEVFORKDONE,
        EVENT_EXIT => O_TRACEEXIT,
        _ => return,
    };
    if !PROCS.lock().threads.get(&tid).and_then(record).is_some_and(|r| r.options & option != 0) {
        return;
    }
    let nr = s.rax;
    s.rax = (-ENOSYS) as u64;
    event_stop(s, nr, event, message);
    s.rax = nr;
}

/// A `PTRACE_EVENT_*` stop with the registers in `s` and `orig_rax`.
fn event_stop(s: &mut State, orig_rax: u64, event: u32, message: u64) {
    let tid = local::tid();
    let code = signal::SIGTRAP | event << 8;
    stop(s, orig_rax, Stop::Event(event), code, CLD_TRAPPED, Some(notify_info(signal::SIGTRAP, code, tid)), message);
}

/// After an execve's success: `PTRACE_EVENT_EXEC` (the message: the thread's id before it,
/// which an exec by another thread than the main one changed), or for a tracee attached
/// without `PTRACE_SEIZE` and without the option, a SIGTRAP of its own (Linux's legacy
/// report).
pub fn exec_event(s: &mut State, old_tid: Pid) {
    let (tid, pid) = process::me();
    let legacy = {
        let mut t = PROCS.lock();
        // Its breakpoints went with the old program (the kernel's too).
        let Some(r) = t.threads.get_mut(&tid).and_then(record_mut) else { return };
        r.debug = [0; 6];
        r.debug_changed = false;
        if r.options & O_TRACEEXEC != 0 {
            false
        } else if !r.seized {
            signal::post_thread(&mut t, pid, tid, SigInfo::user(signal::SIGTRAP, signal::SI_USER, pid));
            true
        } else {
            return;
        }
    };
    if !legacy {
        event(s, EVENT_EXEC, old_tid as u64);
    }
}

/// `PTRACE_EVENT_EXIT` of the calling thread, which ends with wait status `status` (an
/// exit, an exit_group or a fatal signal; a thread killed by another's does not stop):
/// `in_call` if it is its system call.
pub fn exit_event(s: &mut State, status: i32, in_call: bool) {
    let tid = local::tid();
    if !PROCS.lock().threads.get(&tid).and_then(record).is_some_and(|r| r.options & O_TRACEEXIT != 0) {
        return;
    }
    if in_call {
        event(s, EVENT_EXIT, status as u32 as u64);
    } else {
        event_stop(s, u64::MAX, EVENT_EXIT, status as u32 as u64);
    }
}

/// After a handler's frame was set up for a thread single-stepped into it: Linux stops it
/// there (handle_signal's ptrace_notify(SIGTRAP)), single steps off until it is resumed again.
pub fn stepped_into_handler(s: &mut State) {
    let tid = local::tid();
    {
        let mut t = PROCS.lock();
        let Some(r) = t.threads.get_mut(&tid).and_then(record_mut) else { return };
        if !r.step {
            return;
        }
        r.step = false;
        r.forced_tf = false;
    }
    set_flags(local::get().flags.load(Ordering::Relaxed) & (local::TRACE_SYSCALL | local::TRACE_SYSEMU));
    let code = signal::SIGTRAP;
    stop(s, u64::MAX, Stop::Event(0), code, CLD_TRAPPED, Some(notify_info(signal::SIGTRAP, code, tid)), 0);
}

/// A debug exception of the calling thread's program (`dr6`: what DR6 said, `State::trap_error`):
/// the SIGTRAP's si_code (a single step, a hardware breakpoint, else int1), and the tracer's
/// view of DR6 (Linux's virtual_dr6).
pub fn debug_trap(dr6: u64) -> i32 {
    let tid = local::tid();
    if let Some(r) = PROCS.lock().threads.get_mut(&tid).and_then(record_mut) {
        r.debug[4] = dr6 & (0xf | DR6_STEP);
    }
    if dr6 & DR6_STEP != 0 {
        TRAP_TRACE
    } else if dr6 & 0xf != 0 {
        TRAP_HWBKPT
    } else {
        TRAP_BRKPT
    }
}

/// The trap of a seized tracee's or a group-stopped tracee's job control (Linux's
/// do_jobctl_trap): the stop signal while its process stops, else SIGTRAP; a seized tracee
/// reports it as `PTRACE_EVENT_STOP`. (code, siginfo) for `stop`. Decided under the lock
/// that takes the pending traps and notices it reports (a notice posted after it waits for
/// the next trap: Linux decides and enters the stop under one siglock).
pub fn jobctl_trap(t: &mut Table, tid: Pid, pid: Pid) -> (u32, Option<SigInfo>) {
    let group = t.procs.get(&pid).map_or(0, |p| p.sig.stop.sig);
    let seized = seized(t, tid);
    if let Some(r) = t.threads.get_mut(&tid).and_then(record_mut) {
        r.trap_stop = false;
        r.trap_notify = false;
    }
    if seized {
        let sig = if group != 0 { group } else { signal::SIGTRAP };
        let code = sig | EVENT_STOP << 8;
        (code, Some(notify_info(sig, code, tid)))
    } else {
        (if group != 0 { group } else { signal::SIGSTOP }, None)
    }
}

/// A group stop began or ended in process `pid` (Linux's ptrace_trap_notify for each seized
/// tracee): its seized tracees trap to tell their tracers; a listening one is woken for it.
pub fn group_notify(t: &mut Table, pid: Pid) {
    let Some(p) = t.procs.get(&pid) else { return };
    let mut woke = false;
    for i in 0..p.threads.len() {
        let tid = t.procs[&pid].threads[i];
        let Some(r) = t.threads.get_mut(&tid).and_then(record_mut).filter(|r| r.seized) else { continue };
        r.trap_notify = true;
        if r.listening {
            r.listening = false;
            r.resumed = Some(0);
            woke = true;
        }
        t.kick(tid);
    }
    if woke {
        wake(t, pid);
    }
}

// ------------------------------------------------------------------ clone, exit, exec

/// What a clone by a traced thread gives its child: its tracer, seized or not, the options,
/// the event the parent reports (0: none), and whether the parent's TF is a single step's.
pub struct Plan {
    tracer: Pid,
    seized: bool,
    options: u32,
    pub event: u32,
    pub forced_tf: bool,
}

/// The clone flags Linux reads for ptrace.
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PTRACE: u64 = 0x2000;
const CLONE_UNTRACED: u64 = 0x80_0000;

/// What a clone with `flags` and `exit_signal` by the calling thread `tid` does for its
/// tracer (Linux's copy_process and kernel_clone): None if it is not traced. The child is
/// traced too if the event's option is set or with `CLONE_PTRACE`, never with
/// `CLONE_UNTRACED`.
pub fn clone_plan(t: &Table, tid: Pid, flags: u64, exit_signal: u64) -> Option<Plan> {
    let r = t.threads.get(&tid).and_then(record)?;
    let event = if flags & CLONE_UNTRACED != 0 {
        0
    } else if flags & CLONE_VFORK != 0 {
        EVENT_VFORK
    } else if exit_signal != signal::SIGCHLD as u64 {
        EVENT_CLONE
    } else {
        EVENT_FORK
    };
    let option = match event {
        EVENT_VFORK => O_TRACEVFORK,
        EVENT_CLONE => O_TRACECLONE,
        EVENT_FORK => O_TRACEFORK,
        _ => 0,
    };
    let event = if r.options & option != 0 { event } else { 0 };
    let child = event != 0 || (flags & CLONE_PTRACE != 0 && flags & CLONE_UNTRACED == 0);
    Some(Plan { tracer: if child { r.tracer } else { 0 }, seized: r.seized, options: r.options, event, forced_tf: r.forced_tf })
}

impl Plan {
    /// The child's record, made before the clone's last lock (None: the child is not
    /// traced). ENOMEM without memory.
    pub fn child_record(&self) -> Result<Option<Box<Tracee>>, i64> {
        if self.tracer == 0 {
            return Ok(None);
        }
        let mut r = heap::try_box(Tracee::new(self.tracer, self.seized, self.options))?;
        // A seized child starts with a PTRACE_EVENT_STOP; another with a SIGSTOP (`adopt`).
        r.trap_stop = self.seized;
        Ok(Some(r))
    }
}

/// Gives the new thread `tid` (of process `pid`) its record, if its tracer still traces
/// the parent `parent` (under the clone's last lock); else the record is returned, to be
/// let go of after the lock.
pub fn adopt(t: &mut Table, parent: Pid, tid: Pid, record: Option<Box<Tracee>>) -> Option<Box<Tracee>> {
    let r = record?;
    let still = t.threads.get(&parent).and_then(self::record).is_some_and(|p| p.tracer == r.tracer) && t.threads.contains_key(&r.tracer);
    if !still {
        return Some(r);
    }
    let seized = r.seized;
    let th = t.threads.get_mut(&tid)?;
    th.trace = Some(r);
    if !seized {
        // (A pending SIGSTOP without data: its siginfo is SI_USER from pid 0, as Linux's.)
        th.sig.pending.set |= signal::bit(signal::SIGSTOP);
    }
    None
}

/// The thread `th` of process `pid` is gone (the service thread's `process::thread_ended`,
/// lock held; `last`: its process has no other thread): as a tracer, its tracees go free
/// (killed first with `PTRACE_O_EXITKILL`) and the zombies it did not reap with it; as a
/// tracee, it becomes its tracer's zombie (a thread but the main one, now; the main thread
/// once its process has ended: `leader_ended`).
pub fn thread_gone(t: &mut Table, th: &mut Thread, pid: Pid, after: &mut After) {
    let tid = th.tid;
    // As a tracer (Linux's exit_ptrace).
    let mut from = 0;
    loop {
        let next = t.threads.range(from..).find(|(_, x)| record(x).is_some_and(|r| r.tracer == tid)).map(|(k, _)| *k);
        let Some(tracee) = next else { break };
        from = tracee + 1;
        let exitkill = t.threads.get(&tracee).and_then(record).is_some_and(|r| r.options & O_EXITKILL != 0);
        if exitkill {
            let tracee_pid = t.threads[&tracee].pid;
            signal::post_thread_forced(t, tracee_pid, tracee, SigInfo::kernel(signal::SIGKILL));
        }
        // A stop it was in ends with its signal, if its tracer never saw it.
        let sig = t.threads.get(&tracee).and_then(record).map_or(0, |r| if r.stop == Some(Stop::Signal) && r.code != 0 { r.code } else { 0 });
        detach(t, tracee, sig);
    }
    // Its tracees' zombies it did not reap go; a main thread's real parent hears of its
    // process's end now.
    let mut list = th.dead.take();
    while let Some(mut z) = list {
        list = z.next.take();
        t.trace_zombies = t.trace_zombies.saturating_sub(1);
        if let Some((ztid, zpid, _)) = z.zombie {
            if ztid == zpid {
                release_zombie(t, zpid, after);
            }
        }
    }
    for p in t.procs.values_mut() {
        if p.leader_trace.as_ref().is_some_and(|r| r.tracer == tid) {
            p.leader_trace = None;
        }
    }
    // As a tracee.
    let Some(mut r) = th.trace.take().filter(|r| r.tracer != 0) else { return };
    if !t.threads.contains_key(&r.tracer) {
        return;
    }
    let status = th.exit_status.or_else(|| t.procs.get(&pid).and_then(|p| p.exiting)).unwrap_or(signal::SIGKILL as i32);
    r.stop = None;
    r.next = None;
    if tid == pid {
        // The main thread is reported once its whole process has ended.
        if let Some(p) = t.procs.get_mut(&pid) {
            p.leader_trace = Some(r);
        }
        return;
    }
    r.zombie = Some((tid, pid, status));
    let tracer = r.tracer;
    push_zombie(t, tracer, r);
    notify_tracer(t, tracer, tid, pid, 0, 0, Some(status));
}

/// Puts zombie `z` on tracer thread `tracer`'s list.
fn push_zombie(t: &mut Table, tracer: Pid, mut z: Box<Tracee>) {
    let Some(tr) = t.threads.get_mut(&tracer) else { return };
    z.next = tr.dead.take();
    tr.dead = Some(z);
    t.trace_zombies += 1;
}

/// Process `pid` ended with `status` (`process::process_end`, lock held): if its main thread
/// was traced from outside its real parent's process, the tracer gets the zombie first, and
/// its real parent hears of the end only once the tracer reaped it (`release_zombie`).
/// Whether the tracer holds it.
pub fn leader_ended(t: &mut Table, pid: Pid, status: i32) -> bool {
    let Some(p) = t.procs.get_mut(&pid) else { return false };
    let Some(mut r) = p.leader_trace.take() else { return false };
    let ppid = p.ppid;
    let tracer = r.tracer;
    let Some(tracer_pid) = t.threads.get(&tracer).map(|th| th.pid) else { return false };
    if tracer_pid == ppid {
        // Its tracer is its parent: the parent's own wait reaps it.
        return false;
    }
    r.zombie = Some((pid, pid, status));
    push_zombie(t, tracer, r);
    t.procs.get_mut(&pid).expect("checked").trace_zombie = true;
    notify_tracer(t, tracer, pid, pid, 0, 0, Some(status));
    true
}

/// The tracer reaped (or let go of) the traced main thread's zombie of process `pid`: its
/// real parent hears of the end now.
pub fn release_zombie(t: &mut Table, pid: Pid, after: &mut After) {
    let Some(p) = t.procs.get_mut(&pid) else { return };
    if !p.trace_zombie {
        return;
    }
    p.trace_zombie = false;
    process::zombie_released(t, pid, after);
}

/// Whether a tracer's zombie still holds thread id `tid` (it is not reused until reaped).
pub fn zombie_holds(t: &Table, tid: Pid) -> bool {
    t.trace_zombies != 0
        && t.threads.values().any(|th| {
            let mut z = th.dead.as_deref();
            while let Some(e) = z {
                if e.zombie.is_some_and(|(ztid, _, _)| ztid == tid) {
                    return true;
                }
                z = e.next.as_deref();
            }
            false
        })
}

/// What a tracer's `wait` found of a tracee (`process::wait_child`).
pub struct Report {
    pub tid: Pid,
    pub pid: Pid,
    /// A stop's code, or None for an end (`status`).
    pub stop: Option<u32>,
    pub status: i32,
}

/// The first stop or end of a tracee of the threads `mine` picks that `select` (on the
/// tracee's tid and pid) takes, for `wait` with `exited` (WEXITED) and, unless `keep`
/// (WNOWAIT), consumed (an end's zombie reaped: `after` gets what follows). Also whether
/// any tracee was selected at all.
pub fn wait_scan(
    t: &mut Table,
    mine: impl Fn(&Table, Pid) -> bool,
    select: impl Fn(&Table, Pid, Pid) -> bool,
    exited: bool,
    keep: bool,
    after: &mut After,
) -> (bool, Option<Report>) {
    let mut any = false;
    // Stops (reported whatever WSTOPPED says, as Linux's for tracees).
    let mut stop: Option<(Pid, Pid, u32)> = None;
    let view: &Table = t;
    for (tid, th) in view.threads.iter() {
        let Some(r) = record(th) else { continue };
        if !mine(view, r.tracer) || !select(view, *tid, th.pid) {
            continue;
        }
        any = true;
        if r.code != 0 && r.stopped() {
            stop = Some((*tid, th.pid, r.code));
            break;
        }
    }
    if let Some((tid, pid, code)) = stop {
        if !keep {
            if let Some(r) = t.threads.get_mut(&tid).and_then(record_mut) {
                r.code = 0;
            }
        }
        return (true, Some(Report { tid, pid, stop: Some(code), status: ((code as i32) << 8) | 0x7f }));
    }
    // Ends: the zombies on the lists of the tracer threads `mine` picks.
    let mut found: Option<(Pid, Pid, Pid, i32)> = None;
    let view: &Table = t;
    'tracers: for (tracer, th) in view.threads.iter() {
        if !mine(view, *tracer) {
            continue;
        }
        let mut z = th.dead.as_deref();
        while let Some(e) = z {
            if let Some((ztid, zpid, status)) = e.zombie {
                if select(view, ztid, zpid) {
                    any = true;
                    if exited {
                        found = Some((*tracer, ztid, zpid, status));
                        break 'tracers;
                    }
                }
            }
            z = e.next.as_deref();
        }
    }
    let Some((tracer, tid, pid, status)) = found else { return (any, None) };
    if !keep {
        unlink_zombie(t, tracer, tid);
        if tid == pid {
            release_zombie(t, pid, after);
        }
    }
    (true, Some(Report { tid, pid, stop: None, status }))
}

/// Takes the zombie of `tid` off tracer `tracer`'s list.
fn unlink_zombie(t: &mut Table, tracer: Pid, tid: Pid) {
    let Some(th) = t.threads.get_mut(&tracer) else { return };
    let mut at = &mut th.dead;
    loop {
        match at {
            None => return,
            Some(e) if e.zombie.is_some_and(|(ztid, _, _)| ztid == tid) => {
                let mut gone = at.take().expect("matched");
                *at = gone.next.take();
                t.trace_zombies = t.trace_zombies.saturating_sub(1);
                return;
            }
            Some(e) => at = &mut e.next,
        }
    }
}

/// An exec by thread `old` (not the main one) of process `pid` moved its record to the
/// process's id: its tracees follow it, and the old main thread's record goes silently
/// (Linux's de_thread releases the old leader unreported).
pub fn exec_moved(t: &mut Table, old: Pid, pid: Pid) {
    // (Its zombies stay its own: their list moved with its record.)
    for th in t.threads.values_mut() {
        if let Some(r) = th.trace.as_deref_mut() {
            if r.tracer == old {
                r.tracer = pid;
            }
        }
    }
    if let Some(p) = t.procs.get_mut(&pid) {
        p.leader_trace = None;
    }
}

/// Detaches tracee `tid` (lock held): it runs on with `sig` if it was stopped, its group
/// stop goes on if its process is stopped, its single step and breakpoints end. A thread
/// that runs drops the record at its next look (`settle`: it is kicked for it).
fn detach(t: &mut Table, tid: Pid, sig: u32) {
    let Some(th) = t.threads.get_mut(&tid) else { return };
    let pid = th.pid;
    let Some(r) = th.trace.as_deref_mut() else { return };
    r.tracer = 0;
    r.trap_stop = false;
    r.trap_notify = false;
    r.syscall = false;
    r.sysemu = false;
    r.step = false;
    if core::mem::take(&mut r.forced_tf) {
        r.regs.rflags &= !TF;
    }
    if r.debug[5] != 0 {
        r.debug = [0; 6];
        r.debug_changed = true;
    }
    let was_stopped = r.stop.is_some() && r.resumed.is_none();
    if was_stopped {
        r.resumed = Some(sig);
        r.listening = false;
    }
    // The group stop in force takes it back (Linux's __ptrace_unlink).
    let group = t.procs.get(&pid).is_some_and(|p| p.sig.stop.sig != 0);
    let th = t.threads.get_mut(&tid).expect("checked");
    if group && !th.sig.must_stop && !th.exited {
        th.sig.stopped = true;
    }
    if was_stopped {
        wake(t, pid);
    } else {
        t.kick(tid);
    }
}

// ------------------------------------------------------------------ the tracer's side

/// The result of a ptrace or process_vm call in `s`, or None for other calls.
pub fn handle(s: &mut State) -> Option<i64> {
    let result = match s.rax {
        SYS_PTRACE => ptrace(s.rdi, s.rsi as i32, s.rdx, s.r10),
        SYS_PROCESS_VM_READV => process_vm(s.rdi as i32, s.rsi, s.rdx, s.r10, s.r8, s.r9, false),
        SYS_PROCESS_VM_WRITEV => process_vm(s.rdi as i32, s.rsi, s.rdx, s.r10, s.r8, s.r9, true),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

fn ptrace(request: u64, pid: i32, addr: u64, data: u64) -> Result<i64, i64> {
    match request {
        PTRACE_TRACEME => return traceme(),
        PTRACE_ATTACH | PTRACE_SEIZE => return attach(pid, request == PTRACE_SEIZE, addr, data),
        _ => {}
    }
    if pid <= 0 {
        return Err(ESRCH);
    }
    let tid = pid as Pid;
    let me = local::tid();
    // Every other request is the tracer thread's, on a tracee that is stopped (but KILL
    // and INTERRUPT): Linux's ptrace_check_attach.
    let any_state = matches!(request, PTRACE_KILL | PTRACE_INTERRUPT);
    let (handle, tracee_pid) = {
        let t = PROCS.lock();
        let th = t.threads.get(&tid).ok_or(ESRCH)?;
        let r = record(th).filter(|r| r.tracer == me).ok_or(ESRCH)?;
        if !any_state && (!r.stopped() || th.exited) {
            return Err(ESRCH);
        }
        (t.procs.get(&th.pid).map_or(0, |p| p.handle), th.pid)
    };
    match request {
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            let mut word = [0u8; 8];
            if vm_access(handle, addr, &mut word, false, true) != Ok(8) {
                return Err(EIO);
            }
            usercopy::to_program(data, &word)?;
            Ok(0)
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            let mut word = data.to_le_bytes();
            if vm_access(handle, addr, &mut word, true, true) != Ok(8) {
                return Err(EIO);
            }
            Ok(0)
        }
        PTRACE_PEEKUSR => {
            let value = with_stopped(tid, |_, r| peek_user(r, addr))?;
            usercopy::write(data, &value)?;
            Ok(0)
        }
        PTRACE_POKEUSR => with_stopped(tid, |_, r| poke_user(r, addr, data)).map(|_| 0),
        PTRACE_GETREGS => {
            let regs = with_stopped(tid, |_, r| Ok(get_regs(r)))?;
            usercopy::write(data, &regs)?;
            Ok(0)
        }
        PTRACE_SETREGS => {
            let regs: UserRegs = usercopy::read(data)?;
            with_stopped(tid, |_, r| set_regs(r, &regs)).map(|_| 0)
        }
        PTRACE_GETFPREGS => {
            let fpu = with_stopped(tid, |_, r| Ok(r.fpu))?;
            usercopy::to_program(data, &fpu.0)?;
            Ok(0)
        }
        PTRACE_SETFPREGS => {
            let mut fpu = Fpu([0; 512]);
            usercopy::from_program(data, &mut fpu.0)?;
            with_stopped(tid, |_, r| set_fpu(r, &fpu)).map(|_| 0)
        }
        PTRACE_GETREGSET => get_regset(tid, addr, data),
        PTRACE_SETREGSET => set_regset(tid, addr, data),
        PTRACE_CONT | PTRACE_SYSCALL | PTRACE_SINGLESTEP | PTRACE_SYSEMU | PTRACE_SYSEMU_SINGLESTEP => resume(tid, request, data),
        PTRACE_DETACH => {
            if data > signal::NSIG as u64 {
                return Err(EIO);
            }
            let mut t = PROCS.lock();
            if !t.threads.get(&tid).and_then(record).is_some_and(|r| r.tracer == me && r.stopped()) {
                return Err(ESRCH);
            }
            detach(&mut t, tid, data as u32);
            Ok(0)
        }
        PTRACE_KILL => {
            let mut t = PROCS.lock();
            if t.threads.get(&tid).is_some_and(|th| !th.exited) {
                signal::post_thread(&mut t, tracee_pid, tid, SigInfo::kernel(signal::SIGKILL));
            }
            Ok(0)
        }
        PTRACE_INTERRUPT => {
            let mut t = PROCS.lock();
            let r = t.threads.get_mut(&tid).and_then(record_mut).ok_or(ESRCH)?;
            if !r.seized {
                return Err(EIO);
            }
            r.trap_stop = true;
            if r.listening {
                // It traps again at once.
                r.listening = false;
                r.resumed = Some(0);
                wake(&t, tracee_pid);
            } else {
                t.kick(tid);
            }
            Ok(0)
        }
        PTRACE_LISTEN => {
            let mut t = PROCS.lock();
            let r = t.threads.get_mut(&tid).and_then(record_mut).ok_or(ESRCH)?;
            let event_stop = r.siginfo.is_some_and(|i| (i.0[1] as u32 as i32) >> 8 == EVENT_STOP as i32);
            if !r.seized || !event_stop {
                return Err(EIO);
            }
            r.listening = true;
            r.code = 0;
            if r.trap_notify {
                // A notice waits already: it traps again at once.
                r.listening = false;
                r.resumed = Some(0);
                wake(&t, tracee_pid);
            }
            Ok(0)
        }
        PTRACE_SETOPTIONS => {
            if data & !O_MASK != 0 || data & O_SUSPEND_SECCOMP as u64 != 0 {
                return Err(EINVAL);
            }
            with_stopped(tid, |_, r| {
                r.options = data as u32;
                Ok(())
            })
            .map(|_| 0)
        }
        PTRACE_GETEVENTMSG => {
            let message = with_stopped(tid, |_, r| Ok(r.message))?;
            usercopy::write(data, &message)?;
            Ok(0)
        }
        PTRACE_GETSIGINFO => {
            let info = with_stopped(tid, |_, r| r.siginfo.ok_or(EINVAL))?;
            usercopy::write(data, &info.0)?;
            Ok(0)
        }
        PTRACE_SETSIGINFO => {
            let raw: [u64; 16] = usercopy::read(data)?;
            with_stopped(tid, |_, r| match r.siginfo.as_mut() {
                Some(i) => {
                    i.0 = raw;
                    Ok(())
                }
                None => Err(EINVAL),
            })
            .map(|_| 0)
        }
        PTRACE_PEEKSIGINFO => peek_siginfo(tid, tracee_pid, addr, data),
        PTRACE_GETSIGMASK => {
            if addr != 8 {
                return Err(EINVAL);
            }
            let mask = with_stopped(tid, |t, _| Ok(t.threads.get(&tid).map_or(0, |th| th.sig.restore.unwrap_or(th.sig.mask))))?;
            usercopy::write(data, &mask)?;
            Ok(0)
        }
        PTRACE_SETSIGMASK => {
            if addr != 8 {
                return Err(EINVAL);
            }
            let mask: u64 = usercopy::read(data)?;
            with_stopped(tid, |t, _| {
                if let Some(th) = t.threads.get_mut(&tid) {
                    th.sig.mask = mask & !signal::UNBLOCKABLE;
                    th.sig.restore = None;
                }
                Ok(())
            })
            .map(|_| 0)
        }
        PTRACE_GET_SYSCALL_INFO => syscall_info(tid, addr, data),
        PTRACE_GET_RSEQ_CONFIGURATION => {
            // No restartable sequences here: none registered.
            let conf = [0u8; 32];
            let n = (addr as usize).min(conf.len());
            usercopy::to_program(data, &conf[..n])?;
            Ok(conf.len() as i64)
        }
        PTRACE_ARCH_PRCTL => arch_prctl(tid, data, addr),
        _ => Err(EIO),
    }
}

/// Runs `f` on stopped tracee `tid`'s record (traced by the caller), under the lock.
fn with_stopped<R>(tid: Pid, f: impl FnOnce(&mut Table, &mut Tracee) -> Result<R, i64>) -> Result<R, i64> {
    let me = local::tid();
    let mut t = PROCS.lock();
    let th = t.threads.get_mut(&tid).ok_or(ESRCH)?;
    if th.exited || !record(th).is_some_and(|r| r.tracer == me && r.stopped()) {
        return Err(ESRCH);
    }
    // (The record is taken out for `f`, which may look at the table, and put back.)
    let mut r = th.trace.take().expect("checked");
    let result = f(&mut t, &mut r);
    if let Some(th) = t.threads.get_mut(&tid) {
        th.trace = Some(r);
    }
    result
}

/// PTRACE_TRACEME: the calling thread is traced by its parent (its main thread, or its
/// first live one).
fn traceme() -> Result<i64, i64> {
    let mut fresh = heap::try_box(Tracee::new(0, false, 0))?;
    let (tid, pid) = process::me();
    let mut t = PROCS.lock();
    if t.threads.get(&tid).and_then(record).is_some() {
        return Err(EPERM);
    }
    let ppid = t.procs.get(&pid).map_or(0, |p| p.ppid);
    let tracer = t.procs.get(&ppid).filter(|p| p.zombie.is_none() && p.exiting.is_none()).and_then(|p| p.threads.iter().copied().find(|x| t.threads.get(x).is_some_and(|th| !th.exited)));
    let Some(tracer) = tracer else { return Err(EPERM) };
    fresh.tracer = tracer;
    let th = t.threads.get_mut(&tid).ok_or(ESRCH)?;
    let gone = th.trace.replace(fresh);
    drop(t);
    drop(gone);
    Ok(0)
}

/// PTRACE_ATTACH (a SIGSTOP for the thread) and PTRACE_SEIZE (`options` at once, no stop).
fn attach(pid: i32, seize: bool, addr: u64, options: u64) -> Result<i64, i64> {
    if seize && (addr != 0 || options & !O_MASK != 0) {
        return Err(EIO);
    }
    // (No seccomp to suspend: Linux's answer without CONFIG_SECCOMP.)
    if seize && options & O_SUSPEND_SECCOMP as u64 != 0 {
        return Err(EINVAL);
    }
    if pid <= 0 {
        return Err(ESRCH);
    }
    let tid = pid as Pid;
    let mut fresh = heap::try_box(Tracee::new(0, seize, if seize { options as u32 } else { 0 }))?;
    let (me, my_pid) = process::me();
    let mut t = PROCS.lock();
    let th = t.threads.get(&tid).ok_or(ESRCH)?;
    let target_pid = th.pid;
    // Not its own thread group, not a traced or dying thread (Linux's ptrace_attach).
    if target_pid == my_pid || th.exited || record(th).is_some() {
        return Err(EPERM);
    }
    if t.procs.get(&target_pid).is_none_or(|p| p.zombie.is_some() || p.exiting.is_some()) {
        return Err(EPERM);
    }
    fresh.tracer = me;
    let group_stopped = th.sig.stopped;
    let th = t.threads.get_mut(&tid).expect("checked");
    // A record its last tracer let go of while the thread was stopped carries its
    // registers home: the new one takes over from it.
    if let Some(old) = th.trace.as_deref_mut() {
        old.tracer = me;
        old.seized = seize;
        old.options = fresh.options;
        old.trap_stop = false;
        old.trap_notify = false;
    } else {
        th.trace = Some(fresh);
    }
    if group_stopped {
        // Stopped already: it traps for its new tracer instead (Linux's JOBCTL_TRAP_STOP).
        if let Some(r) = th.trace.as_deref_mut() {
            r.trap_stop = true;
        }
        if let Some(p) = t.procs.get(&target_pid) {
            process::wake_word(&p.words.stop);
        }
    }
    if !seize {
        signal::post_thread_forced(&mut t, target_pid, tid, SigInfo::kernel(signal::SIGSTOP));
    }
    Ok(0)
}

/// CONT, SYSCALL, SINGLESTEP, SYSEMU, SYSEMU_SINGLESTEP: resumes the stopped tracee with
/// signal `data`.
fn resume(tid: Pid, request: u64, data: u64) -> Result<i64, i64> {
    if data > signal::NSIG as u64 {
        return Err(EIO);
    }
    let me = local::tid();
    let mut t = PROCS.lock();
    let th = t.threads.get_mut(&tid).ok_or(ESRCH)?;
    let pid = th.pid;
    let r = th.trace.as_deref_mut().filter(|r| r.tracer == me && r.stopped()).ok_or(ESRCH)?;
    r.syscall = request == PTRACE_SYSCALL;
    r.sysemu = matches!(request, PTRACE_SYSEMU | PTRACE_SYSEMU_SINGLESTEP);
    r.step = matches!(request, PTRACE_SINGLESTEP | PTRACE_SYSEMU_SINGLESTEP);
    if r.step {
        if r.regs.rflags & TF == 0 {
            r.regs.rflags |= TF;
            r.forced_tf = true;
        }
    } else if core::mem::take(&mut r.forced_tf) {
        r.regs.rflags &= !TF;
    }
    r.resumed = Some(data as u32);
    r.code = 0;
    wake(&t, pid);
    Ok(0)
}

// ------------------------------------------------------------------ registers

/// Linux's `struct user_regs_struct` on x86-64.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UserRegs {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbp: u64,
    rbx: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rax: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    orig_rax: u64,
    rip: u64,
    cs: u64,
    eflags: u64,
    rsp: u64,
    ss: u64,
    fs_base: u64,
    gs_base: u64,
    ds: u64,
    es: u64,
    fs: u64,
    gs: u64,
}

const USER_REGS: usize = core::mem::size_of::<UserRegs>() / 8;

impl UserRegs {
    fn words(&self) -> [u64; USER_REGS] {
        unsafe { core::mem::transmute(*self) }
    }

    fn from_words(w: [u64; USER_REGS]) -> UserRegs {
        unsafe { core::mem::transmute(w) }
    }
}

fn get_regs(r: &Tracee) -> UserRegs {
    let s = &r.regs;
    UserRegs {
        r15: s.r15,
        r14: s.r14,
        r13: s.r13,
        r12: s.r12,
        rbp: s.rbp,
        rbx: s.rbx,
        r11: s.r11,
        r10: s.r10,
        r9: s.r9,
        r8: s.r8,
        rax: s.rax,
        rcx: s.rcx,
        rdx: s.rdx,
        rsi: s.rsi,
        rdi: s.rdi,
        orig_rax: r.orig_rax,
        rip: s.rip,
        cs: USER_CS,
        eflags: if r.forced_tf { s.rflags & !TF } else { s.rflags },
        rsp: s.rsp,
        ss: USER_SS,
        fs_base: r.fs_base,
        gs_base: 0,
        ds: 0,
        es: 0,
        fs: 0,
        gs: 0,
    }
}

/// Linux's putreg for word `i` of `user_regs_struct` with `value`, into `r` (EIO for what
/// cannot be: another code or stack segment than the 64-bit user ones, a data segment or
/// GS base other than 0, an FS base above the program's memory).
fn put_reg(r: &mut Tracee, i: usize, value: u64) -> Result<(), i64> {
    let s = &mut r.regs;
    match i {
        0 => s.r15 = value,
        1 => s.r14 = value,
        2 => s.r13 = value,
        3 => s.r12 = value,
        4 => s.rbp = value,
        5 => s.rbx = value,
        6 => s.r11 = value,
        7 => s.r10 = value,
        8 => s.r9 = value,
        9 => s.r8 = value,
        10 => s.rax = value,
        11 => s.rcx = value,
        12 => s.rdx = value,
        13 => s.rsi = value,
        14 => s.rdi = value,
        15 => r.orig_rax = value,
        16 => s.rip = value,
        17 if value == USER_CS => {}
        18 => {
            let value = if r.forced_tf { value | TF } else { value };
            s.rflags = (s.rflags & !FLAG_MASK) | (value & FLAG_MASK);
        }
        19 => s.rsp = value,
        20 if value == USER_SS => {}
        21 if value < SHARED_BASE => r.fs_base = value,
        22..=26 if value == 0 => {}
        _ => return Err(EIO),
    }
    Ok(())
}

fn set_regs(r: &mut Tracee, regs: &UserRegs) -> Result<(), i64> {
    // All or nothing.
    let mut copy = Tracee { regs: r.regs, orig_rax: r.orig_rax, fs_base: r.fs_base, forced_tf: r.forced_tf, ..Tracee::new(0, false, 0) };
    for (i, w) in regs.words().into_iter().enumerate() {
        put_reg(&mut copy, i, w)?;
    }
    r.regs = copy.regs;
    r.orig_rax = copy.orig_rax;
    r.fs_base = copy.fs_base;
    Ok(())
}

/// A new FPU image: MXCSR must have no bit the CPU does not know (EINVAL, as Linux's).
fn set_fpu(r: &mut Tracee, fpu: &Fpu) -> Result<(), i64> {
    let mxcsr = u32::from_le_bytes(fpu.0[24..28].try_into().expect("4 bytes"));
    if mxcsr & !signal::mxcsr_mask() != 0 {
        return Err(EINVAL);
    }
    r.fpu = *fpu;
    Ok(())
}

/// PEEKUSER: a word of `struct user` (the registers, the debug registers; 0 elsewhere).
fn peek_user(r: &Tracee, addr: u64) -> Result<u64, i64> {
    if addr % 8 != 0 || addr >= USER_SIZE {
        return Err(EIO);
    }
    if (addr as usize) < USER_REGS * 8 {
        return Ok(get_regs(r).words()[addr as usize / 8]);
    }
    if addr >= USER_DEBUGREG {
        return Ok(match (addr - USER_DEBUGREG) / 8 {
            n @ 0..=3 => r.debug[n as usize],
            6 => r.debug[4],
            7 => r.debug[5],
            _ => 0,
        });
    }
    Ok(0)
}

/// POKEUSER: a register, or a debug register (DR0-DR3, DR6, DR7: EIO for DR4 and DR5 and
/// anywhere else).
fn poke_user(r: &mut Tracee, addr: u64, value: u64) -> Result<(), i64> {
    if addr % 8 != 0 || addr >= USER_SIZE {
        return Err(EIO);
    }
    if (addr as usize) < USER_REGS * 8 {
        let mut copy = Tracee { regs: r.regs, orig_rax: r.orig_rax, fs_base: r.fs_base, forced_tf: r.forced_tf, ..Tracee::new(0, false, 0) };
        put_reg(&mut copy, addr as usize / 8, value)?;
        r.regs = copy.regs;
        r.orig_rax = copy.orig_rax;
        r.fs_base = copy.fs_base;
        return Ok(());
    }
    if addr < USER_DEBUGREG {
        return Err(EIO);
    }
    let n = ((addr - USER_DEBUGREG) / 8) as usize;
    let mut debug = r.debug;
    match n {
        0..=3 => debug[n] = value,
        6 => {
            r.debug[4] = value;
            return Ok(());
        }
        7 => debug[5] = value,
        _ => return Err(EIO),
    }
    check_debug(&debug)?;
    r.debug = debug;
    r.debug_changed = true;
    Ok(())
}

/// The kernel's rules for the breakpoints DR7 enables (`restricted::SYS_THREAD_DEBUG`,
/// Linux's arch_build_bp_info): an instruction breakpoint of length 1, or a write or
/// read-write one of length 1, 2, 4 or 8, aligned, below 64 TiB.
fn check_debug(debug: &[u64; 6]) -> Result<(), i64> {
    let dr7 = debug[5];
    for slot in 0..4 {
        if (dr7 >> (2 * slot)) & 3 == 0 {
            continue;
        }
        let rw = (dr7 >> (16 + 4 * slot)) & 3;
        let len = match (dr7 >> (18 + 4 * slot)) & 3 {
            0 => 1,
            1 => 2,
            3 => 4,
            _ => 8,
        };
        let addr = debug[slot];
        if rw == 2 || (rw == 0 && len != 1) || addr % len != 0 || addr.saturating_add(len) > SHARED_BASE {
            return Err(EINVAL);
        }
    }
    Ok(())
}

/// GETREGSET: `NT_PRSTATUS` (the registers) or `NT_PRFPREG` (the FPU state) into the iovec
/// at `iov`, its length set to what was written (ENODEV for the XSAVE state: the CPU's is
/// not enabled, EINVAL for other sets).
fn get_regset(tid: Pid, kind: u64, iov: u64) -> Result<i64, i64> {
    let [base, len]: [u64; 2] = usercopy::read(iov)?;
    let (bytes, size, unit): ([u8; 512], usize, u64) = match kind {
        NT_PRSTATUS => {
            let regs = with_stopped(tid, |_, r| Ok(get_regs(r)))?;
            let mut b = [0u8; 512];
            for (i, w) in regs.words().into_iter().enumerate() {
                b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
            }
            (b, USER_REGS * 8, 8)
        }
        NT_PRFPREG => (with_stopped(tid, |_, r| Ok(r.fpu))?.0, 512, 4),
        NT_X86_XSTATE => return Err(ENODEV),
        _ => return Err(EINVAL),
    };
    if len % unit != 0 {
        return Err(EINVAL);
    }
    let n = (len as usize).min(size);
    usercopy::to_program(base, &bytes[..n])?;
    usercopy::write(iov + 8, &(n as u64))?;
    Ok(0)
}

/// SETREGSET: the first bytes of a register set from the iovec at `iov` (the rest stays).
fn set_regset(tid: Pid, kind: u64, iov: u64) -> Result<i64, i64> {
    let [base, len]: [u64; 2] = usercopy::read(iov)?;
    let (size, unit) = match kind {
        NT_PRSTATUS => (USER_REGS * 8, 8),
        NT_PRFPREG => (512, 4),
        NT_X86_XSTATE => return Err(ENODEV),
        _ => return Err(EINVAL),
    };
    if len % unit != 0 {
        return Err(EINVAL);
    }
    let n = (len as usize).min(size);
    let mut bytes = [0u8; 512];
    usercopy::from_program(base, &mut bytes[..n])?;
    with_stopped(tid, |_, r| {
        if kind == NT_PRSTATUS {
            let mut words = get_regs(r).words();
            for (i, w) in words.iter_mut().enumerate().take(n / 8) {
                *w = u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
            }
            set_regs(r, &UserRegs::from_words(words))
        } else {
            let mut fpu = r.fpu;
            fpu.0[..n].copy_from_slice(&bytes[..n]);
            set_fpu(r, &fpu)
        }
    })?;
    usercopy::write(iov + 8, &(n as u64))?;
    Ok(0)
}

/// PTRACE_ARCH_PRCTL: arch_prctl on the tracee (`code`; `arg` a value, or for the GET
/// codes where the tracer's memory takes it).
fn arch_prctl(tid: Pid, code: u64, arg: u64) -> Result<i64, i64> {
    const ARCH_SET_GS: u64 = 0x1001;
    const ARCH_SET_FS: u64 = 0x1002;
    const ARCH_GET_FS: u64 = 0x1003;
    const ARCH_GET_GS: u64 = 0x1004;
    match code {
        ARCH_SET_FS => {
            if arg >= SHARED_BASE {
                return Err(EPERM);
            }
            with_stopped(tid, |_, r| {
                r.fs_base = arg;
                Ok(0)
            })
        }
        ARCH_GET_FS => {
            let base = with_stopped(tid, |_, r| Ok(r.fs_base))?;
            usercopy::write(arg, &base)?;
            Ok(0)
        }
        ARCH_GET_GS => {
            with_stopped(tid, |_, _| Ok(()))?;
            usercopy::write(arg, &0u64)?;
            Ok(0)
        }
        // (The program has no GS base of its own here, as arch_prctl's own.)
        ARCH_SET_GS => Err(EINVAL),
        _ => Err(EINVAL),
    }
}

/// PTRACE_GET_SYSCALL_INFO: the stop's `struct ptrace_syscall_info` (up to `size` bytes at
/// `out`); returns its full size.
fn syscall_info(tid: Pid, size: u64, out: u64) -> Result<i64, i64> {
    const OP_NONE: u8 = 0;
    const OP_ENTRY: u8 = 1;
    const OP_EXIT: u8 = 2;
    let (stop, regs, orig) = with_stopped(tid, |_, r| Ok((r.stop, r.regs, r.orig_rax)))?;
    let mut info = [0u8; 88];
    info[4..8].copy_from_slice(&AUDIT_ARCH_X86_64.to_le_bytes());
    info[8..16].copy_from_slice(&regs.rip.to_le_bytes());
    info[16..24].copy_from_slice(&regs.rsp.to_le_bytes());
    let len = match stop {
        Some(Stop::SyscallEntry) => {
            info[0] = OP_ENTRY;
            info[24..32].copy_from_slice(&orig.to_le_bytes());
            for (i, a) in [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9].into_iter().enumerate() {
                info[32 + i * 8..40 + i * 8].copy_from_slice(&a.to_le_bytes());
            }
            80
        }
        Some(Stop::SyscallExit) => {
            info[0] = OP_EXIT;
            info[24..32].copy_from_slice(&regs.rax.to_le_bytes());
            info[32] = ((regs.rax as i64) < 0 && (regs.rax as i64) >= -4095) as u8;
            33
        }
        _ => {
            info[0] = OP_NONE;
            24
        }
    };
    let n = (size as usize).min(len);
    usercopy::to_program(out, &info[..n])?;
    Ok(len as i64)
}

/// PTRACE_PEEKSIGINFO: the tracee's queued signals (its own, or its process's with
/// `PTRACE_PEEKSIGINFO_SHARED`) from `off` on, at most `nr`, to `out`; how many.
fn peek_siginfo(tid: Pid, pid: Pid, args: u64, out: u64) -> Result<i64, i64> {
    const SHARED: u32 = 1;
    let raw: [u64; 2] = usercopy::read(args)?;
    let (off, flags, nr) = (raw[0], raw[1] as u32, (raw[1] >> 32) as i32);
    if flags & !SHARED != 0 || nr < 0 {
        return Err(EINVAL);
    }
    let mut taken: Vec<[u64; 16]> = heap::try_vec((nr as usize).min(64))?;
    with_stopped(tid, |t, _| {
        let queue = if flags & SHARED != 0 { t.procs.get(&pid).map(|p| &p.sig.shared) } else { t.threads.get(&tid).map(|th| &th.sig.pending) };
        if let Some(q) = queue {
            for info in q.infos().skip(off.min(usize::MAX as u64) as usize).take(taken.capacity()) {
                taken.push(info.0);
            }
        }
        Ok(())
    })?;
    for (i, info) in taken.iter().enumerate() {
        if let Err(e) = usercopy::write(out + i as u64 * 128, info) {
            return if i == 0 { Err(e) } else { Ok(i as i64) };
        }
    }
    Ok(taken.len() as i64)
}

// ------------------------------------------------------------------ memory

/// `SYS_VM_ACCESS` on process (or space) handle `handle` at `addr`, `buf.len()` bytes (in
/// pieces the kernel takes): how many it copied before the first that failed, or the
/// error if the first did.
pub fn vm_access(handle: u64, addr: u64, buf: &mut [u8], write: bool, force: bool) -> Result<usize, i64> {
    let flags = if write { VM_ACCESS_WRITE } else { 0 } | if force { VM_ACCESS_FORCE } else { 0 };
    let mut done = 0usize;
    while done < buf.len() {
        let rest = &mut buf[done..];
        let n = syscall(SYS_VM_ACCESS, [handle, addr.wrapping_add(done as u64), rest.as_mut_ptr() as u64, rest.len() as u64, flags, 0]);
        if n < 0 {
            return if done == 0 { Err(-n) } else { Ok(done) };
        }
        if n == 0 {
            break;
        }
        done += n as usize;
    }
    Ok(done)
}

/// A bounce buffer's size for the copies between processes.
const CHUNK: usize = 16 * 1024;

/// process_vm_readv and process_vm_writev(pid, local_iov, liovcnt, remote_iov, riovcnt,
/// flags): the remote ranges in order, as far as both sides allow (a partial count, or the
/// first error if nothing moved).
fn process_vm(pid: i32, lvec: u64, lcnt: u64, rvec: u64, rcnt: u64, flags: u64, write: bool) -> Result<i64, i64> {
    if flags != 0 {
        return Err(EINVAL);
    }
    let local = crate::files::iovecs(lvec, lcnt)?;
    let remote = crate::files::iovecs(rvec, rcnt)?;
    let handle = {
        let t = PROCS.lock();
        if pid <= 0 {
            return Err(ESRCH);
        }
        let id = pid as Pid;
        let owner = if t.procs.contains_key(&id) { id } else { t.threads.get(&id).ok_or(ESRCH)?.pid };
        let p = t.procs.get(&owner).ok_or(ESRCH)?;
        if p.zombie.is_some() {
            return Err(ESRCH);
        }
        p.handle
    };
    let total: u64 = remote.iter().map(|v| v.1).fold(0u64, |a, b| a.saturating_add(b)).min(local.iter().map(|v| v.1).fold(0u64, |a, b| a.saturating_add(b)));
    if total == 0 {
        return Ok(0);
    }
    let mut buf: Vec<u8> = heap::try_vec(CHUNK)?;
    buf.resize(CHUNK, 0);
    let mut moved = 0u64;
    // The local side's cursor: which iovec, how far into it.
    let (mut li, mut loff) = (0usize, 0u64);
    let mut error = None;
    'remote: for &(raddr, rlen) in &remote {
        let mut roff = 0u64;
        while roff < rlen {
            // Skip empty local iovecs.
            while li < local.len() && loff == local[li].1 {
                li += 1;
                loff = 0;
            }
            if li == local.len() {
                break 'remote;
            }
            let n = ((rlen - roff).min(local[li].1 - loff) as usize).min(CHUNK);
            let chunk = &mut buf[..n];
            let laddr = local[li].0.wrapping_add(loff);
            let got = if write {
                if let Err(e) = usercopy::from_program(laddr, chunk) {
                    error = Some(e);
                    break 'remote;
                }
                vm_access(handle, raddr.wrapping_add(roff), chunk, true, false)
            } else {
                vm_access(handle, raddr.wrapping_add(roff), chunk, false, false)
            };
            let got = match got {
                Ok(g) => g,
                Err(e) => {
                    error = Some(e);
                    break 'remote;
                }
            };
            if !write && got > 0 {
                if let Err(e) = usercopy::to_program(laddr, &chunk[..got]) {
                    error = Some(e);
                    break 'remote;
                }
            }
            moved += got as u64;
            roff += got as u64;
            loff += got as u64;
            if got < n {
                error = Some(EFAULT);
                break 'remote;
            }
        }
    }
    match error {
        Some(e) if moved == 0 => Err(e),
        _ => Ok(moved as i64),
    }
}

/// /proc/<pid>/mem's open: a handle on the address space process (or thread) `id` has now
/// (`SYS_VM_SPACE`), which the open file keeps; ESRCH for none (a zombie).
pub fn mem_open(id: Pid) -> Result<u64, i64> {
    let handle = {
        let t = PROCS.lock();
        let owner = if t.procs.contains_key(&id) { id } else { t.threads.get(&id).ok_or(ESRCH)?.pid };
        let p = t.procs.get(&owner).ok_or(ESRCH)?;
        if p.zombie.is_some() {
            return Err(ESRCH);
        }
        p.handle
    };
    let space = syscall(SYS_VM_SPACE, [handle, 0, 0, 0, 0, 0]);
    if space < 0 { Err(-space) } else { Ok(space as u64) }
}

/// /proc/<pid>/mem's read or write at `offset` (an address) through space handle `space`,
/// with the program's buffers `vecs` (as Linux's mem_rw: FOLL_FORCE; EIO if nothing could
/// be moved, 0 once the space is gone): how many bytes.
pub fn mem_rw(space: u64, offset: u64, vecs: &[(u64, u64)], write: bool) -> Result<i64, i64> {
    let mut buf: Vec<u8> = heap::try_vec(4096)?;
    buf.resize(4096, 0);
    let mut moved = 0u64;
    for &(base, len) in vecs {
        let mut off = 0u64;
        while off < len {
            let n = ((len - off) as usize).min(buf.len());
            let chunk = &mut buf[..n];
            let at = offset.wrapping_add(moved);
            let got = if write {
                if let Err(e) = usercopy::from_program(base + off, chunk) {
                    return if moved == 0 { Err(e) } else { Ok(moved as i64) };
                }
                vm_access(space, at, chunk, true, true)
            } else {
                vm_access(space, at, chunk, false, true)
            };
            let got = match got {
                Ok(g) => g,
                Err(_) if moved == 0 => return Err(EIO),
                Err(_) => return Ok(moved as i64),
            };
            if got == 0 {
                return Ok(moved as i64);
            }
            if !write {
                if let Err(e) = usercopy::to_program(base + off, &chunk[..got]) {
                    return if moved == 0 { Err(e) } else { Ok(moved as i64) };
                }
            }
            moved += got as u64;
            off += got as u64;
            if got < n {
                return Ok(moved as i64);
            }
        }
    }
    Ok(moved as i64)
}
