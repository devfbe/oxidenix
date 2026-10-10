//! Ending threads and processes, and the kernel's wait for its own.
//!
//! A thread that exits clears its CLONE_CHILD_CLEARTID word and wakes its
//! futex (how a Linux program's thread is joined), drops its address space
//! and leaves the process. Nothing of it stays behind: its kernel stack is
//! freed once the CPU switched away. The last thread to leave ends the
//! process. Every process the kernel keeps after its end is the kernel's
//! own (the monitor started it: a tree's first process, a native server,
//! a Linux server instance's service process): a zombie until the kernel
//! reaps it (`wait_for`, `reap_orphans`). A process the Linux server made
//! leaves the table with its last thread: its relations and its zombie are
//! the server's.

use super::errno::*;
use super::kill::GroupExit;
use super::sched::{current, prepare_to_wait, schedule, wakeup, TABLE};
use super::task::{State, Task, ThreadGroup};
use super::{futex, ipc, irq, tlb, uaccess, with_current, Pid};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use x86_64::instructions::interrupts;

/// Where the kernel waits for its processes to end.
fn ended_chan() -> usize {
    0x5_0000_0000
}

/// The process ends with `status` (unless it is already ending: then with
/// that one): every thread of it ends.
pub fn exit_group(status: i32) -> ! {
    let me = current();
    let others: Vec<Arc<Task>> = {
        let info = me.group.info.lock();
        let mut exit = me.group.exit.lock();
        match *exit {
            // Already ending: just this thread.
            GroupExit::Exiting(_) => Vec::new(),
            GroupExit::None => {
                *exit = GroupExit::Exiting(status);
                info.threads.iter().filter(|t| !core::ptr::eq(&***t, me)).cloned().collect()
            }
        }
    };
    for t in &others {
        if me.group.instance.load(Ordering::Acquire) != 0 {
            super::linux::kill_task(t);
        } else {
            super::kill::kick(t);
        }
    }
    drop(others);
    exit_thread(status)
}

/// The calling thread ends. If it is the last one, the process ends with
/// `status` (or the status of a group exit in progress).
pub fn exit_thread(status: i32) -> ! {
    let me = current();
    assert!(me.tgid() != 0, "kernel task must not exit");
    // Joining threads wait for the word to become 0. Best effort: the
    // memory may be gone already.
    let ctid = with_current(|p| core::mem::take(&mut p.clear_child_tid));
    if ctid != 0 && uaccess::write(ctid, 0u32).is_ok() {
        let _ = futex::wake_one(ctid);
    }
    // Shared state goes before interrupts are disabled.
    let server = with_current(|p| p.server.take());
    drop(server);
    interrupts::disable();
    let mm = unsafe { me.own() }.mm.take();
    tlb::switch(mm.as_ref().map(|m| &*m.tlb), None, false);
    drop(mm);
    unsafe { me.own() }.io_bitmap = None;
    // Its stale timers would keep its memory until they came up.
    crate::timer::forget(me);

    let group = me.group.clone();
    let last = {
        let mut table = TABLE.lock();
        table.tasks.remove(&me.tid());
        let mut info = group.info.lock();
        info.threads.retain(|t| !core::ptr::eq(&**t, me));
        if me.tid() == group.tgid {
            info.main_status = Some(status);
        }
        let (user, system) = me.cputime();
        info.dead_time.0 += user;
        info.dead_time.1 += system;
        let last = info.threads.is_empty();
        if last {
            // A process the Linux server made is its server's to reap
            // (the server's handle keeps what it reports); the kernel keeps
            // a zombie only of its own.
            if group.server_reaps.load(Ordering::Relaxed) {
                table.groups.remove(&group.tgid);
            } else {
                table.zombies += 1;
            }
        }
        last
    };
    if last {
        let main_status = group.info.lock().main_status;
        let status = match *group.exit.lock() {
            GroupExit::Exiting(s) => s,
            GroupExit::None => main_status.unwrap_or(status),
        };
        process_exit(&group, status);
    }
    // Nothing of this call's stack is ever dropped (schedule does not come
    // back): the reference to the process must go now, or every process
    // outlives its reaping.
    drop(group);
    {
        let _w = me.wake_lock.lock();
        me.set_state(State::Dead);
    }
    schedule();
    unreachable!("an exited task was scheduled again");
}

/// The last thread of `group` left: the process ended with `status`.
fn process_exit(group: &Arc<ThreadGroup>, status: i32) {
    let pid = group.tgid;
    // A server's death is recorded before its services are marked dead
    // (the restart policy measures its life).
    if group.privileged.load(Ordering::Relaxed) {
        super::server_exited(pid);
    }
    ipc::on_exit(pid);
    irq::on_exit(pid);
    // Channels it served lose their service (and their clients learn it).
    super::channel::service_exited(pid);
    {
        let mut info = group.info.lock();
        info.exit_status = Some(status);
        if let Some(mem) = info.mem.take() {
            info.peak_pages = info.peak_pages.max(mem.peak_pages.load(Ordering::Relaxed));
        }
    }
    if !group.server_reaps.load(Ordering::Relaxed) {
        wakeup(ended_chan());
    }
}

/// Reaps the kernel's process `pid` if it ended: its wait status.
fn reap(pid: Pid) -> Option<i32> {
    let mut table = TABLE.lock();
    let status = table.groups.get(&pid)?.info.lock().exit_status?;
    table.groups.remove(&pid);
    table.zombies -= 1;
    Some(status)
}

/// For the monitor: waits until the kernel's process `pid` ended and
/// reaps it; its wait status. ECHILD if there is no such process.
pub fn wait_for(pid: Pid) -> Result<i32, i64> {
    loop {
        let wait = prepare_to_wait(ended_chan());
        if let Some(status) = reap(pid) {
            return Ok(status);
        }
        let known = TABLE.lock().groups.get(&pid).is_some_and(|g| !g.server_reaps.load(Ordering::Relaxed));
        if !known {
            return Err(ECHILD);
        }
        wait.sleep();
    }
}

/// Reaps the kernel's processes that ended (servers, service processes,
/// what is left of a process tree).
pub fn reap_orphans() {
    let ended: Vec<Pid> = TABLE
        .lock()
        .groups
        .values()
        .filter(|g| g.tgid != 0 && !g.server_reaps.load(Ordering::Relaxed) && g.info.lock().exit_status.is_some())
        .map(|g| g.tgid)
        .collect();
    for pid in ended {
        reap(pid);
    }
}

pub enum WaitStatus {
    Exited(i32),
    Killed(i32),
}

pub fn decode_status(status: i32) -> WaitStatus {
    match status & 0x7f {
        0 => WaitStatus::Exited((status >> 8) & 0xff),
        sig => WaitStatus::Killed(sig),
    }
}
