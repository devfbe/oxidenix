//! Killing threads and processes, and what the kernel's waits ask about it
//! (`interrupted`, `dying`). The kernel has no signals (R9): a Linux
//! program's are its server's, which kicks and kills the program's threads
//! through the kernel (`linux::kick_task`, `linux::kill_task`); a native
//! server has none. What the kernel itself ends is a whole process: the
//! monitor's `kill`, memory running out, a native server's fault, a
//! failed Linux server. A Linux program's process the kernel kills has
//! every thread killed, and its server learns that the kernel did
//! (`ProcInfo::killed`); a native server's process is marked exiting and
//! each thread ends at its next return to user mode (`exit_if_dying`) or
//! once its wait ends.
//!
//! Wait statuses keep Linux's encoding (exit code << 8, or the number of
//! the signal a process died of: SIGKILL for a kill, SIGSEGV for a fault),
//! which the monitor prints and the server reports.

use super::sched::{current, try_wake, TABLE};
use super::task::{State, Task, ThreadGroup};
use super::syscall::Frame;
use super::Pid;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGSEGV: u32 = 11;

/// How a process is ending, if it is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GroupExit {
    None,
    /// Every thread exits; the wait status of the process.
    Exiting(i32),
}

/// A process's `GroupExit`: set once, read without a lock (every return to a Linux program
/// asks whether its process is ending).
pub struct ExitCell(core::sync::atomic::AtomicU64);

/// `ExitCell`'s word of `GroupExit::Exiting`: this bit and the status.
const EXITING: u64 = 1 << 32;

impl ExitCell {
    pub const fn new() -> Self {
        ExitCell(core::sync::atomic::AtomicU64::new(0))
    }

    pub fn get(&self) -> GroupExit {
        match self.0.load(Ordering::Acquire) {
            0 => GroupExit::None,
            word => GroupExit::Exiting(word as u32 as i32),
        }
    }

    pub fn is_exiting(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }

    /// The process ends with `status` unless it already does: whether this call made it end.
    pub fn begin(&self, status: i32) -> bool {
        self.0.compare_exchange(0, EXITING | status as u32 as u64, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
}

/// Whether a Linux program's process (its threads are its server's to
/// kick and kill).
fn linux_process(group: &ThreadGroup) -> bool {
    group.instance.load(Ordering::Acquire) != 0
}

/// Whether an interruptible wait of the current thread ends (EINTR): a
/// Linux program's thread was kicked (or dies); any other thread's process
/// is ending.
pub fn interrupted() -> bool {
    if super::linux::serves_program() {
        return super::linux::kicked();
    }
    dying()
}

/// Whether the current thread is about to die (ends every wait, also the
/// uninterruptible ones).
pub fn dying() -> bool {
    if super::linux::serves_program() {
        return super::linux::dying();
    }
    current().group.exit.is_exiting()
}

/// The kernel kills the calling thread's process (out of memory, a failed
/// Linux server): SIGKILL, recorded for the server of a Linux program's.
pub fn kernel_kill_current() -> ! {
    let group = current().group.clone();
    if linux_process(&group) {
        let _ = group.killed_by_kernel.compare_exchange(0, SIGKILL as u64, Ordering::AcqRel, Ordering::Acquire);
    }
    drop(group);
    super::exit_group(SIGKILL as i32)
}

/// Gets `t` to look at its state: wakes it from an interruptible sleep,
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

/// The kernel ends the process `group` with wait status `status` (no
/// permission checks: the kernel's own decision). Never allocates beyond
/// a list of its threads, and never sleeps.
pub fn kill_process(group: &Arc<ThreadGroup>, status: i32) {
    if group.tgid == 0 {
        return;
    }
    let threads: Vec<Arc<Task>> = {
        let info = group.info.lock();
        if info.threads.is_empty() {
            return;
        }
        if linux_process(group) {
            let _ = group.killed_by_kernel.compare_exchange(0, status as u64, Ordering::AcqRel, Ordering::Acquire);
        }
        group.exit.begin(status);
        info.threads.clone()
    };
    for t in &threads {
        if linux_process(group) {
            super::linux::kill_task(t);
        } else {
            kick(t);
        }
    }
}

/// Kills the process with id `pid` (or the process of the thread with
/// that id): the monitor's `kill`. ESRCH if there is none, or it ended.
pub fn kill_pid(pid: Pid) -> Result<(), i64> {
    let group = {
        let table = TABLE.lock();
        table.groups.get(&pid).cloned().or_else(|| table.tasks.get(&pid).map(|t| t.group.clone()))
    };
    let group = group.filter(|g| g.tgid != 0 && g.info.lock().exit_status.is_none()).ok_or(super::errno::ESRCH)?;
    kill_process(&group, SIGKILL as i32);
    Ok(())
}

/// On the way back to user mode of a thread that is no Linux program's:
/// it ends if its process does.
pub fn exit_if_dying(frame: &Frame) {
    if !frame.from_user() || super::linux::mode().is_some() {
        return;
    }
    if let GroupExit::Exiting(status) = current().group.exit.get() {
        super::exit_thread(status);
    }
}
