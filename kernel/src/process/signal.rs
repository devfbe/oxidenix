//! POSIX signals: per-process actions, mask and pending set; delivery on
//! every return to user space.

use super::address_space::USER_END;
use super::errno::*;
use super::syscall::Frame;
use super::{sched, uaccess, Pid, State};
use crate::interrupts::gdt;
use x86_64::instructions::interrupts;

pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGKILL: u32 = 9;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
const SIGTTIN: u32 = 21;
const SIGTTOU: u32 = 22;
const SIGURG: u32 = 23;
const SIGWINCH: u32 = 28;
const NSIG: u32 = 64;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SA_RESTORER: u64 = 0x0400_0000;
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
}

impl Default for Signals {
    fn default() -> Self {
        Signals {
            actions: [SigAction::default(); NSIG as usize],
            mask: 0,
            pending: 0,
        }
    }
}

fn bit(sig: u32) -> u64 {
    1 << (sig - 1)
}

/// SIGKILL and SIGSTOP can be neither caught, ignored nor blocked.
const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));

fn default_ignored(sig: u32) -> bool {
    // Stop signals are ignored too: there is no job suspension yet.
    matches!(sig, SIGCHLD | SIGCONT | SIGURG | SIGWINCH | SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
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

    /// Fork keeps actions and mask, but not pending signals.
    pub fn for_child(&self) -> Signals {
        Signals { pending: 0, ..self.clone() }
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

/// Whether a blocking syscall of the current process should return EINTR.
pub fn interrupted() -> bool {
    interrupts::without_interrupts(|| sched().cur().signals.deliverable() != 0)
}

/// Marks `sig` pending for `pid` and wakes it from an interruptible sleep.
fn post(pid: Pid, sig: u32) {
    let s = sched();
    let Some(p) = s.procs.get_mut(&pid) else { return };
    if pid == 0 || matches!(p.state, State::Zombie(_)) {
        return;
    }
    p.signals.pending |= bit(sig);
    let sleeping = matches!(p.state, State::Sleeping(_) | State::WaitChild);
    if sleeping && p.signals.deliverable() != 0 {
        s.make_ready(pid);
    }
}

pub fn send(pid: Pid, sig: u32) {
    interrupts::without_interrupts(|| post(pid, sig));
}

/// Sends `sig` to every process in group `pgid`; returns whether one existed.
/// Sends `sig` to every process in group `pgid`. Called from interrupt
/// context (Ctrl+C), so it must not allocate.
pub fn send_group(pgid: Pid, sig: u32) {
    interrupts::without_interrupts(|| {
        let mut pids: heapless::Vec<Pid, 256> = heapless::Vec::new();
        for p in sched().procs.values().filter(|p| p.pgid == pgid && p.pid != 0) {
            let _ = pids.push(p.pid);
        }
        for pid in pids {
            post(pid, sig);
        }
    })
}

pub fn kill(pid: i64, sig: u64) -> SysResult {
    if sig > NSIG as u64 {
        return Err(EINVAL);
    }
    let sig = sig as u32;
    interrupts::without_interrupts(|| {
        let s = sched();
        let me = s.cur();
        let (my_pid, my_pgid) = (me.pid, me.pgid);
        let targets: alloc::vec::Vec<Pid> = s
            .procs
            .values()
            .filter(|p| p.pid != 0 && !matches!(p.state, State::Zombie(_)))
            .filter(|p| match pid {
                p_ if p_ > 0 => p.pid == p_ as Pid,
                0 => p.pgid == my_pgid,
                -1 => p.pid != 1 && p.pid != my_pid,
                p_ => p.pgid == p_.unsigned_abs(),
            })
            .map(|p| p.pid)
            .collect();
        if targets.is_empty() {
            return Err(ESRCH);
        }
        if sig != 0 {
            for t in targets {
                post(t, sig);
            }
        }
        Ok(0)
    })
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
    let old = interrupts::without_interrupts(|| {
        let p = sched().cur();
        let old = p.signals.actions[sig as usize - 1];
        if let Some(mut new) = new {
            new.mask &= !UNBLOCKABLE;
            p.signals.actions[sig as usize - 1] = new;
            if new.handler == SIG_IGN || (new.handler == SIG_DFL && default_ignored(sig)) {
                p.signals.pending &= !bit(sig);
            }
        }
        old
    });
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
    let old = interrupts::without_interrupts(|| {
        let p = sched().cur();
        let old = p.signals.mask;
        if let Some(n) = new {
            p.signals.mask = match how {
                SIG_BLOCK => old | n,
                SIG_UNBLOCK => old & !n,
                SIG_SETMASK => n,
                _ => return Err(EINVAL),
            } & !UNBLOCKABLE;
        }
        Ok(old)
    })?;
    if oldset != 0 {
        uaccess::write(oldset, old)?;
    }
    Ok(0)
}

/// What a signal handler finds on its stack, from low to high addresses:
/// the return address (the restorer), then the saved state for sigreturn,
/// then a minimal siginfo.
#[repr(C)]
#[derive(Clone, Copy)]
struct SigFrame {
    restorer: u64,
    saved: Frame,
    saved_mask: u64,
    info: [u32; 32],
}

/// Delivers at most one pending signal before returning to user space:
/// terminates the process (default action) or redirects `frame` to the
/// handler.
pub fn deliver(frame: &mut Frame) {
    if !frame.from_user() {
        return;
    }
    let action = interrupts::without_interrupts(|| {
        let p = sched().cur();
        let set = p.signals.deliverable();
        if set == 0 {
            // Drop signals that are pending but ignored.
            let ignored = p.signals.pending & !p.signals.mask;
            p.signals.pending &= !ignored;
            return None;
        }
        let sig = set.trailing_zeros() + 1;
        p.signals.pending &= !bit(sig);
        let action = p.signals.actions[sig as usize - 1];
        if action.handler == SIG_DFL {
            return Some((sig, action, 0));
        }
        let old_mask = p.signals.mask;
        let mut block = action.mask;
        if action.flags & SA_NODEFER == 0 {
            block |= bit(sig);
        }
        p.signals.mask = (p.signals.mask | block) & !UNBLOCKABLE;
        if action.flags & SA_RESETHAND != 0 {
            p.signals.actions[sig as usize - 1] = SigAction::default();
        }
        Some((sig, action, old_mask))
    });
    let Some((sig, action, old_mask)) = action else { return };
    if action.handler == SIG_DFL {
        super::exit(sig as i32);
    }
    // Without a restorer the handler could never return; a handler outside
    // user space would make iretq fault in ring 0.
    if action.flags & SA_RESTORER == 0 || action.handler >= USER_END {
        super::exit(sig as i32);
    }

    let mut info = [0u32; 32];
    info[0] = sig;
    let sigframe = SigFrame { restorer: action.restorer, saved: *frame, saved_mask: old_mask, info };
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
    interrupts::without_interrupts(|| sched().cur().signals.mask = sf.saved_mask & !UNBLOCKABLE);
    Ok(frame.rax as i64)
}
