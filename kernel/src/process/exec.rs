//! execve(2): replaces the program of the calling process.
//!
//! In a process with several threads the other threads end first (as on
//! Linux): the calling thread marks the process, sends them SIGKILL and
//! waits until they are gone; it then takes over the process id as its
//! thread id if it was not the main thread. A vfork parent continues once
//! the new program is in place.

use super::address_space::Mm;
use super::errno::*;
use super::sched::{current, prepare_to_wait, TABLE};
use super::signal::{self, GroupExit};
use super::syscall::Frame;
use super::task;
use super::{absolute, basename, clone, cmdline_of, load_inode, load_path, tlb, with_current, FdEntry, Pid};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

/// Channel a thread waits on for the other threads of its process to exit.
pub fn group_chan(tgid: Pid) -> usize {
    0x6_0000_0000 + tgid as usize
}

/// Ends every other thread of the calling process and makes the caller its
/// main thread. Fails if the process is already ending.
fn de_thread() -> Result<(), i64> {
    let me = current();
    let group = &me.group;
    let others: Vec<_> = {
        let info = group.info.lock();
        if info.threads.len() == 1 {
            return Ok(());
        }
        let mut g = group.sig.lock();
        if g.exit != GroupExit::None {
            return Err(EAGAIN);
        }
        g.exit = GroupExit::Exec(me.tid());
        info.threads.iter().filter(|t| !core::ptr::eq(&***t, me)).cloned().collect()
    };
    for t in &others {
        signal::kill_thread(t);
    }
    drop(others);
    let result = loop {
        let wait = prepare_to_wait(group_chan(group.tgid));
        if group.info.lock().threads.len() == 1 {
            break Ok(());
        }
        // Killed from outside: give up, the process ends anyway.
        if signal::killed() {
            break Err(EINTR);
        }
        wait.sleep();
    };
    if result.is_err() {
        return result;
    }
    group.sig.lock().exit = GroupExit::None;
    if me.tid() != group.tgid {
        // The old main thread is gone (it left the table when it exited).
        let mut table = TABLE.lock();
        let me_arc = table.tasks.remove(&me.tid()).expect("a live thread is listed");
        me.set_tid(group.tgid);
        table.tasks.insert(group.tgid, me_arc);
    }
    Ok(())
}

pub fn exec(frame: &mut Frame, path: &str, args: &[String], envs: &[String]) -> Result<(), i64> {
    // A Linux program's path was resolved by its server.
    let target = with_current(|p| p.linux.as_mut().and_then(|l| l.exec_target.take()));
    let (image, exe) = match target {
        Some((super::linux::ExecTarget::Inode(inode), abs)) => (load_inode(inode, args, envs)?, abs),
        // A file of the server's: the hold keeps it unwritten (the server's
        // ETXTBSY) while the program runs.
        Some((super::linux::ExecTarget::File(cache, hold), abs)) => {
            let hold: Option<super::address_space::Hold> = hold.map(|h| h as _);
            (super::loader::load(&cache, None, hold, args, envs)?, abs)
        }
        None => {
            let cwd = with_current(|p| p.cwd());
            (load_path(&cwd, path, args, envs)?, absolute(&cwd, path))
        }
    };
    let mut space = image.space;
    // The new program stays in the process's Linux server instance.
    if let Some(instance) = with_current(|p| p.mm.as_ref().and_then(|m| m.lock().instance().cloned())) {
        space.attach(instance, true).map_err(|_| ENOMEM)?;
    }
    let mm = Mm::new(space).ok_or(ENOMEM)?;
    // The point of no return: from here on the old program is gone.
    de_thread()?;
    let me = current();
    // A descriptor table still shared (CLONE_FILES without CLONE_THREAD)
    // becomes the process's own, as it is once the other threads are gone.
    let shared = with_current(|p| p.files.as_ref().filter(|f| alloc::sync::Arc::strong_count(f) > 1).cloned());
    let files = match shared {
        Some(f) => match f.duplicate() {
            Some(copy) => Some(copy),
            None => super::exit_group(signal::SIGKILL as i32),
        },
        None => None,
    };
    {
        let mut info = me.group.info.lock();
        info.name = basename(path).to_string();
        info.cmdline = cmdline_of(args);
        info.exe = exe;
        info.mem = Some(mm.stats.clone());
    }
    *me.comm.lock() = basename(path).chars().take(15).collect();
    me.group.sig.lock().reset_on_exec();
    // A new program gets no inherited hardware access.
    me.group.privileged.store(false, Ordering::Relaxed);
    let (closed, old_mm, old_files) = with_current(|p| {
        // A Linux thread runs this in a legacy call, in the program's view.
        let server = p.linux.as_ref().is_some_and(|l| l.normal_view());
        tlb::switch(p.mm.as_ref().map(|m| &*m.tlb), Some(&mm.tlb), server);
        let old_mm = p.mm.replace(mm);
        let old_files = match files {
            Some(f) => p.files.replace(f),
            None => None,
        };
        p.io_bitmap = None;
        p.server = None;
        p.clear_child_tid = 0;
        crate::smp::cpu().tables().set_io_bitmap(None);
        let closed: Vec<FdEntry> = p.files.as_ref().map(|f| f.take_cloexec()).unwrap_or_default();
        (closed, old_mm, old_files)
    });
    drop(closed);
    drop(old_files);
    // The old address space is freed here (unless a vfork parent shares
    // it); no CPU has it loaded for this task any more.
    drop(old_mm);
    // The channels the old program served lose their service: the new
    // program must not reach their grants.
    super::channel::service_exited(me.tgid());
    clone::release_vfork();
    FsBase::write(VirtAddr::new(0));
    let initial = task::FpuState::initial();
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) initial.0.as_ptr(), options(nostack)) };
    *frame = Frame::user_start(image.entry, image.sp);
    Ok(())
}
