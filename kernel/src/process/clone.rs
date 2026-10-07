//! clone(2), fork and vfork: new threads and processes.
//!
//! What the new task shares with its creator follows the flags, as on
//! Linux: the address space (CLONE_VM), the descriptor table (CLONE_FILES),
//! the working directory (CLONE_FS), and, with CLONE_THREAD, the process
//! itself (pid, signal handlers, relations). fork is clone with none of
//! them; vfork shares the address space and suspends the parent until the
//! child execs or exits.

use super::address_space::Mm;
use super::errno::*;
use super::sched::{self, current, prepare_to_wait};
use super::signal::{self, GroupSignals};
use super::syscall::Frame;
use super::task::{FsInfo, Info, Process, ThreadGroup};
use super::{new_task, uaccess, with_current, Pid};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use x86_64::registers::model_specific::FsBase;

const CSIGNAL: u64 = 0xff;
const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_PTRACE: u64 = 0x2000;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x10_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
const CLONE_DETACHED: u64 = 0x40_0000;
const CLONE_UNTRACED: u64 = 0x80_0000;
const CLONE_CHILD_SETTID: u64 = 0x100_0000;
const CLONE_IO: u64 = 0x8000_0000;

/// Flags that are accepted; everything else (namespaces, pidfds, ...) is
/// EINVAL. SYSVSEM, PTRACE, UNTRACED, DETACHED and IO change nothing here.
const SUPPORTED: u64 = CSIGNAL
    | CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_PTRACE
    | CLONE_VFORK
    | CLONE_PARENT
    | CLONE_THREAD
    | CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_DETACHED
    | CLONE_UNTRACED
    | CLONE_CHILD_SETTID
    | CLONE_IO;

/// fork(): a new process with a copy-on-write copy of the caller's memory.
pub fn fork(frame: &Frame) -> Result<Pid, i64> {
    clone(frame, signal::SIGCHLD as u64, 0, 0, 0, 0)
}

/// vfork(): a new process that borrows the caller's memory; the caller
/// waits until it execs or exits.
pub fn vfork(frame: &Frame) -> Result<Pid, i64> {
    clone(frame, CLONE_VM | CLONE_VFORK | signal::SIGCHLD as u64, 0, 0, 0, 0)
}

/// clone(flags, stack, parent_tid, child_tid, tls).
pub fn clone(frame: &Frame, flags: u64, stack: u64, parent_tid: u64, child_tid: u64, tls: u64) -> Result<Pid, i64> {
    let _ = CLONE_PIDFD;
    if flags & !SUPPORTED != 0 {
        return Err(EINVAL);
    }
    // Linux's rules: threads share handlers, handlers need shared memory.
    // Shared handlers between different processes (an old LinuxThreads
    // feature) are not supported: handlers belong to the process here.
    if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0 {
        return Err(EINVAL);
    }
    if flags & CLONE_SIGHAND != 0 && (flags & CLONE_VM == 0 || flags & CLONE_THREAD == 0) {
        return Err(EINVAL);
    }
    let thread = flags & CLONE_THREAD != 0;
    if (thread && flags & CSIGNAL != 0) || flags & CSIGNAL > signal::NSIG as u64 {
        return Err(EINVAL);
    }
    if flags & CLONE_SETTLS != 0 && tls >= super::address_space::USER_END {
        return Err(EPERM);
    }
    // A non-canonical stack pointer would make iretq fault in ring 0.
    if stack >= super::address_space::USER_END {
        return Err(EINVAL);
    }

    let slot = sched::reserve_pid()?;
    let tid = slot.pid;
    let me = current();
    let parent = unsafe { me.own() };

    let mm = if flags & CLONE_VM != 0 {
        parent.mm()?
    } else {
        let mut space = parent.mm()?.lock().clone_user().map_err(|_| ENOMEM)?;
        if flags & CLONE_CHILD_SETTID != 0 {
            // Written into the child's copy only.
            space.write_user(child_tid, &(tid as u32).to_le_bytes()).map_err(|_| EFAULT)?;
        }
        Mm::new(space).ok_or(ENOMEM)?
    };
    let files = if flags & CLONE_FILES != 0 { parent.files()?.clone() } else { parent.files()?.duplicate().ok_or(ENOMEM)? };
    let fs = match (&parent.fs, flags & CLONE_FS != 0) {
        (Some(f), true) => f.clone(),
        (Some(f), false) => FsInfo::new(f.cwd()).ok_or(ENOMEM)?,
        (None, _) => FsInfo::new(alloc::string::String::from("/")).ok_or(ENOMEM)?,
    };
    let vfork_done = if flags & CLONE_VFORK != 0 { Some(Arc::try_new(AtomicBool::new(false)).map_err(|_| ENOMEM)?) } else { None };

    let group = if thread {
        me.group.clone()
    } else {
        let i = me.group.info.lock();
        // With CLONE_PARENT the caller's parent gets the caller's exit
        // signal, not one of the caller's choosing (as on Linux).
        let (ppid, exit_signal) =
            if flags & CLONE_PARENT != 0 { (i.ppid, i.exit_signal) } else { (me.tgid(), (flags & CSIGNAL) as u32) };
        // The parent-death signal is cleared for the child, as on Linux.
        let info = Info {
            dumpable: i.dumpable,
            no_new_privs: i.no_new_privs,
            cmdline: i.cmdline.clone(),
            exe: i.exe.clone(),
            mem: Some(mm.stats.clone()),
            nice: i.nice,
            exit_signal,
            ..Info::new(ppid, i.pgid, i.sid, i.name.clone())
        };
        drop(i);
        let sig: GroupSignals = me.group.sig.lock().for_child();
        ThreadGroup::new(tid, info, sig).ok_or(ENOMEM)?
    };

    let mut child_frame = *frame;
    child_frame.rax = 0;
    if stack != 0 {
        child_frame.rsp = stack;
    }
    // A thread of a Linux program starts in its own server thread, which
    // enters the program with the frame above.
    let instance = mm.lock().instance().cloned();
    let linux = match instance {
        Some(instance) => {
            let (thread, start) = super::linux::LinuxThread::new(instance, &child_frame)?;
            child_frame = start;
            Some(thread)
        }
        None => None,
    };
    let own = Process {
        mm: Some(mm),
        files: Some(files),
        fs: Some(fs),
        io_bitmap: None,
        server: None,
        clear_child_tid: if flags & CLONE_CHILD_CLEARTID != 0 { child_tid } else { 0 },
        vfork_done: vfork_done.clone(),
        linux,
    };
    let comm = me.comm.lock().clone();
    let child = new_task(tid, group, comm, own, child_frame)?;
    // The child resumes with the caller's FPU registers and TLS pointer
    // (or the new one).
    unsafe {
        let state = child.cpu_state();
        state.fs_base = if flags & CLONE_SETTLS != 0 { tls } else { FsBase::read().as_u64() };
        core::arch::asm!("fxsave64 [{}]", in(reg) state.fpu.0.as_mut_ptr(), options(nostack));
    }
    *child.sig.lock() = me.sig.lock().inherit();
    child.affinity.store(me.affinity.load(Ordering::Relaxed), Ordering::Relaxed);

    // Visible to the caller before the child can run (pthread_create
    // relies on it); for a shared address space also in the child's view.
    if flags & CLONE_PARENT_SETTID != 0 {
        uaccess::write(parent_tid, tid as u32)?;
    }
    if flags & CLONE_CHILD_SETTID != 0 && flags & CLONE_VM != 0 {
        uaccess::write(child_tid, tid as u32)?;
    }

    slot.insert(child.clone())?;
    sched::start(child);

    if let Some(done) = vfork_done {
        wait_vfork(&done);
    }
    Ok(tid)
}

/// Channel a vfork parent sleeps on.
fn vfork_chan(done: &Arc<AtomicBool>) -> usize {
    Arc::as_ptr(done) as usize
}

/// The vfork parent's wait. Only SIGKILL ends it early: the child runs on
/// the parent's stack, which the parent must not touch meanwhile.
fn wait_vfork(done: &Arc<AtomicBool>) {
    loop {
        let wait = prepare_to_wait(vfork_chan(done));
        if done.load(Ordering::Acquire) || signal::killed() {
            return;
        }
        wait.sleep();
    }
}

/// Lets a vfork parent continue (the child exec'd or is exiting).
pub fn release_vfork() {
    if let Some(done) = with_current(|p| p.vfork_done.take()) {
        done.store(true, Ordering::Release);
        sched::wakeup(vfork_chan(&done));
    }
}

/// set_tid_address(tidptr): where the thread's id is cleared (and a futex
/// woken) when it exits. Returns the thread id.
pub fn set_tid_address(ptr: u64) -> Result<Pid, i64> {
    with_current(|p| p.clear_child_tid = ptr);
    Ok(current().tid())
}
