//! POSIX signals: per-process actions, mask and pending set; delivery on
//! every return to user space.

use super::address_space::USER_END;
use super::errno::*;
use super::syscall::Frame;
use super::sched::{self, current, try_wake};
use super::task::{State, Task};
use super::{uaccess, Pid, TIMER_HZ};
use crate::interrupts::gdt;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
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
const NSIG: u32 = 64;

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

#[derive(Clone)]
pub struct Signals {
    actions: [SigAction; NSIG as usize],
    pub mask: u64,
    pending: u64,
    /// ITIMER_REAL: next SIGALRM tick (0: off) and the reload interval.
    alarm_at: u64,
    alarm_every: u64,
    /// Signals a sigtimedwait is waiting for (they are usually blocked, so
    /// they would not wake the task otherwise).
    waiting_for: u64,
}

impl Default for Signals {
    fn default() -> Self {
        Signals {
            actions: [SigAction::default(); NSIG as usize],
            mask: 0,
            pending: 0,
            alarm_at: 0,
            alarm_every: 0,
            waiting_for: 0,
        }
    }
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

impl Signals {
    /// Pending, unblocked signals that would actually do something.
    fn deliverable(&self) -> u64 {
        let mut set = self.pending & !(self.mask & !UNBLOCKABLE);
        for sig in 1..=NSIG {
            if set & bit(sig) == 0 {
                continue;
            }
            let handler = self.actions[sig as usize - 1].handler;
            if handler == SIG_IGN || (handler == SIG_DFL && default_ignored(sig)) {
                set &= !bit(sig);
            }
        }
        set
    }

    /// Fork keeps actions and mask, but not pending signals or timers.
    pub fn for_child(&self) -> Signals {
        Signals { pending: 0, alarm_at: 0, alarm_every: 0, ..self.clone() }
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

/// Background processes reading from the terminal get SIGTTIN (and EINTR,
/// so the read restarts once they are continued in the foreground), or EIO
/// if they ignore or block it.
pub fn check_tty_read(foreground: Pid) -> Result<(), i64> {
    let me = current();
    let pgid = me.info.lock().pgid;
    if me.pid == 0 || foreground == 0 || pgid == foreground {
        return Ok(());
    }
    {
        let sig = me.sig.lock();
        let ignored = sig.actions[SIGTTIN as usize - 1].handler == SIG_IGN;
        if ignored || sig.mask & bit(SIGTTIN) != 0 {
            return Err(EIO);
        }
    }
    send_group(pgid, SIGTTIN);
    Err(EINTR)
}

/// Whether a blocking syscall of the current process should return EINTR.
pub fn interrupted() -> bool {
    current().sig.lock().deliverable() != 0
}

/// Raises `sig` for a fault of the current process (a CPU exception). Such
/// a signal can be neither blocked nor ignored: as on Linux, the action is
/// reset to the default (terminate) in that case. Returns whether the
/// process will die of it (no handler).
pub fn force(sig: u32) -> bool {
    let mut s = current().sig.lock();
    let blocked = s.mask & bit(sig) != 0;
    let action = &mut s.actions[sig as usize - 1];
    let default = if blocked || action.handler == SIG_IGN {
        *action = SigAction::default();
        true
    } else {
        action.handler == SIG_DFL
    };
    s.mask &= !bit(sig);
    s.pending |= bit(sig);
    default
}

/// Wakes a parent blocked in wait4 and sends it SIGCHLD.
fn notify_parent(ppid: Pid) {
    super::notify_parent(ppid);
    send(ppid, SIGCHLD);
}

/// Marks `sig` pending for `t` and wakes it from an interruptible sleep.
/// SIGCONT resumes a stopped process right away; SIGKILL wakes it to die.
fn post(t: &Arc<Task>, sig: u32) {
    if t.pid == 0 || t.state() == State::Zombie {
        return;
    }
    let mut continued_parent = None;
    let (wake, resume) = {
        let mut s = t.sig.lock();
        if sig == SIGCONT {
            s.pending &= !STOP_SIGNALS;
        } else if is_stop(sig) {
            s.pending &= !bit(SIGCONT);
        }
        s.pending |= bit(sig);
        let resume = sig == SIGCONT || sig == SIGKILL;
        (s.deliverable() != 0 || s.waiting_for & bit(sig) != 0, resume)
    };
    if resume && try_wake(t, State::Stopped) && sig == SIGCONT {
        let mut info = t.info.lock();
        info.report = Some(CONTINUED_STATUS);
        continued_parent = Some(info.ppid);
    }
    if wake {
        try_wake(t, State::Sleeping);
    }
    if let Some(ppid) = continued_parent {
        notify_parent(ppid);
    }
}

pub fn send(pid: Pid, sig: u32) {
    // The table lock must be released before posting: SIGCONT notifies
    // the parent, which sends again.
    let target = sched::TABLE.lock().tasks.get(&pid).cloned();
    if let Some(t) = target {
        post(&t, sig);
    }
}

/// Sends `sig` to every process in group `pgid`. Called from interrupt
/// context (Ctrl+C), so it must not allocate.
pub fn send_group(pgid: Pid, sig: u32) {
    let mut targets: heapless::Vec<Arc<Task>, { sched::MAX_PROCS }> = heapless::Vec::new();
    {
        let table = sched::TABLE.lock();
        // Servers never belong to a terminal's job; skip them regardless.
        for t in table.tasks.values() {
            if t.pid != 0 && !t.privileged.load(Ordering::Relaxed) && t.info.lock().pgid == pgid {
                let _ = targets.push(t.clone());
            }
        }
    }
    for t in &targets {
        post(t, sig);
    }
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
    let (my_pid, my_pgid) = (me.pid, me.info.lock().pgid);
    let mut targets: heapless::Vec<Arc<Task>, { sched::MAX_PROCS }> = heapless::Vec::new();
    {
        let table = sched::TABLE.lock();
        if pid > 0 && my_pid != 0 && table.tasks.get(&(pid as Pid)).is_some_and(|t| t.privileged.load(Ordering::Relaxed)) {
            return Err(EPERM);
        }
        for t in table.tasks.values() {
            if t.pid == 0 || t.state() == State::Zombie || (my_pid != 0 && t.privileged.load(Ordering::Relaxed)) {
                continue;
            }
            let selected = match pid {
                p if p > 0 => t.pid == p as Pid,
                0 => t.info.lock().pgid == my_pgid,
                -1 => t.pid != 1 && t.pid != my_pid,
                p => t.info.lock().pgid == p.unsigned_abs(),
            };
            if selected {
                let _ = targets.push(t.clone());
            }
        }
    }
    if targets.is_empty() {
        return Err(ESRCH);
    }
    if sig != 0 {
        for t in &targets {
            post(t, sig);
        }
    }
    Ok(0)
}

/// Sends SIGALRM to every process whose interval timer expired by `now`
/// and reloads it. Runs in the timer interrupt; never allocates.
pub fn expire_alarms(now: u64) {
    let mut due: heapless::Vec<Arc<Task>, { sched::MAX_PROCS }> = heapless::Vec::new();
    {
        let table = sched::TABLE.lock();
        for t in table.tasks.values() {
            let mut s = t.sig.lock();
            if s.alarm_at != 0 && s.alarm_at <= now {
                s.alarm_at = if s.alarm_every != 0 { now.saturating_add(s.alarm_every) } else { 0 };
                let _ = due.push(t.clone());
            }
        }
    }
    for t in &due {
        post(t, SIGALRM);
    }
}

/// setitimer(ITIMER_REAL, new, old) with timer-tick resolution. `new` and
/// `old` are (interval, value) in microseconds; a value of 0 disarms.
pub fn set_alarm(value_us: u64, interval_us: u64) -> (u64, u64) {
    let to_ticks = |us: u64| us.saturating_mul(TIMER_HZ).div_ceil(1_000_000);
    let to_us = |ticks: u64| ticks.saturating_mul(1_000_000) / TIMER_HZ;
    let now = sched::ticks();
    let mut s = current().sig.lock();
    let old = (to_us(s.alarm_at.saturating_sub(now)), to_us(s.alarm_every));
    s.alarm_at = if value_us == 0 { 0 } else { now.saturating_add(to_ticks(value_us).max(1)) };
    s.alarm_every = if value_us == 0 { 0 } else { to_ticks(interval_us) };
    old
}

/// The current ITIMER_REAL setting: (remaining, interval) in microseconds.
pub fn get_alarm() -> (u64, u64) {
    let now = sched::ticks();
    let s = current().sig.lock();
    let remaining = if s.alarm_at == 0 { 0 } else { s.alarm_at.saturating_sub(now).max(1) };
    let to_us = |ticks: u64| ticks.saturating_mul(1_000_000) / TIMER_HZ;
    (to_us(remaining), to_us(s.alarm_every))
}

/// rt_sigtimedwait(set, info, timeout, size): takes the lowest pending
/// signal of `set` (normally blocked by the caller) without running its
/// handler; waits for one up to `timeout` (EAGAIN), or forever if null.
pub fn sigtimedwait(set: u64, info: u64, timeout: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let wanted: u64 = uaccess::read::<u64>(set)? & !UNBLOCKABLE;
    let deadline = if timeout != 0 {
        let [sec, nsec]: [u64; 2] = uaccess::read(timeout)?;
        if nsec >= 1_000_000_000 || sec > i64::MAX as u64 {
            return Err(EINVAL);
        }
        let tick_ns = 1_000_000_000 / TIMER_HZ;
        Some(sched::ticks().saturating_add(sec.saturating_mul(TIMER_HZ)).saturating_add(nsec.div_ceil(tick_ns)))
    } else {
        None
    };
    let me = current();
    let result = loop {
        let wait = sched::prepare_to_sleep();
        {
            let mut s = me.sig.lock();
            let ready = s.pending & wanted;
            if ready != 0 {
                let sig = ready.trailing_zeros() + 1;
                s.pending &= !bit(sig);
                break Ok(sig);
            }
            s.waiting_for = wanted;
        }
        if interrupted() {
            break Err(EINTR);
        }
        match deadline {
            Some(d) if sched::ticks() >= d => break Err(EAGAIN),
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
        let mut s = current().sig.lock();
        let old = s.actions[sig as usize - 1];
        if let Some(mut new) = new {
            new.mask &= !UNBLOCKABLE;
            s.actions[sig as usize - 1] = new;
            if new.handler == SIG_IGN || (new.handler == SIG_DFL && default_ignored(sig)) {
                s.pending &= !bit(sig);
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
    let old = {
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
        old
    };
    if oldset != 0 {
        uaccess::write(oldset, old)?;
    }
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
    !matches!(nr, 7 | 15 | 23 | 34 | 35 | 270 | 271)
}

/// Makes the interrupted syscall `nr` run again when the frame resumes.
fn rewind(frame: &mut Frame, nr: u64) {
    frame.rax = nr;
    frame.rip -= 2; // length of the `syscall` instruction
}

/// Stops the current process until SIGCONT (or SIGKILL) arrives. A
/// SIGCONT that came in meanwhile cancels the stop: it is checked under the
/// signal lock, which `post` takes too.
fn stop(sig: u32) {
    let me = current();
    {
        let s = me.sig.lock();
        if s.pending & (bit(SIGCONT) | bit(SIGKILL)) != 0 {
            return;
        }
        let _w = me.wake_lock.lock();
        me.set_state(State::Stopped);
    }
    let ppid = {
        let mut info = me.info.lock();
        info.report = Some(stopped_status(sig));
        info.ppid
    };
    notify_parent(ppid);
    sched::schedule();
}

/// Handles pending signals before returning to user space: stops the
/// process, terminates it, or redirects `frame` to a handler. `syscall` is
/// the number of the syscall that is returning, if any, so that calls
/// interrupted with EINTR can be restarted.
pub fn deliver(frame: &mut Frame, syscall: Option<u64>) {
    if !frame.from_user() {
        return;
    }
    let mut interrupted = syscall.filter(|&nr| frame.rax == (-EINTR) as u64 && restartable(nr));
    loop {
        let action = (|| {
            let mut s = current().sig.lock();
            let set = s.deliverable();
            if set == 0 {
                // Drop signals that are pending but ignored.
                let ignored = s.pending & !s.mask;
                s.pending &= !ignored;
                return None;
            }
            let sig = set.trailing_zeros() + 1;
            s.pending &= !bit(sig);
            let action = s.actions[sig as usize - 1];
            if action.handler == SIG_DFL {
                return Some((sig, action, 0));
            }
            let old_mask = s.mask;
            let mut block = action.mask;
            if action.flags & SA_NODEFER == 0 {
                block |= bit(sig);
            }
            s.mask = (s.mask | block) & !UNBLOCKABLE;
            if action.flags & SA_RESETHAND != 0 {
                s.actions[sig as usize - 1] = SigAction::default();
            }
            Some((sig, action, old_mask))
        })();
        let Some((sig, action, old_mask)) = action else {
            // Nothing ran in user space, so the interrupted call can simply go on.
            if let Some(nr) = interrupted {
                rewind(frame, nr);
            }
            return;
        };
        if action.handler == SIG_DFL && is_stop(sig) {
            stop(sig);
            continue;
        }
        if action.handler == SIG_DFL {
            super::exit(sig as i32);
        }
        if let Some(nr) = interrupted.take() {
            if action.flags & SA_RESTART != 0 {
                rewind(frame, nr);
            }
        }
        return push_handler_frame(frame, sig, action, old_mask);
    }
}

fn push_handler_frame(frame: &mut Frame, sig: u32, action: SigAction, old_mask: u64) {
    // Without a restorer the handler could never return; a handler outside
    // user space would make iretq fault in ring 0.
    if action.flags & SA_RESTORER == 0 || action.handler >= USER_END {
        super::exit(sig as i32);
    }

    let mut info = [0u32; 32];
    info[0] = sig;
    let sigframe = SigFrame { restorer: action.restorer, saved: *frame, saved_mask: old_mask, fpu: fxsave(), info };
    // Skip the red zone; at handler entry rsp+8 must be 16-byte aligned.
    let size = core::mem::size_of::<SigFrame>() as u64;
    let sp = ((frame.rsp.wrapping_sub(128).wrapping_sub(size)) & !0xf).wrapping_sub(8);
    if uaccess::write(sp, sigframe).is_err() {
        super::exit(11);
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
        Err(_) => super::exit(11),
    };
    let mut saved = sf.saved;
    // A non-canonical or kernel address would make iretq fault in ring 0.
    if saved.rip >= USER_END || saved.rsp >= USER_END {
        super::exit(11);
    }
    // Never let user space choose privileged selectors or flags.
    const USER_FLAGS: u64 = 0xcd5; // CF PF AF ZF SF TF DF OF
    saved.cs = gdt::USER_CS as u64;
    saved.ss = gdt::USER_SS as u64;
    saved.rflags = (saved.rflags & USER_FLAGS) | 0x202;
    *frame = saved;
    fxrstor(sf.fpu);
    current().sig.lock().mask = sf.saved_mask & !UNBLOCKABLE;
    Ok(frame.rax as i64)
}
