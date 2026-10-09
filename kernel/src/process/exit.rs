//! Ending threads and processes, and waiting for children.
//!
//! A thread that exits clears its CLONE_CHILD_CLEARTID word and wakes its
//! futex (how a thread is joined), drops its references to the address
//! space and other shared state, and leaves the process. Nothing of it
//! stays behind: its kernel stack is freed once the CPU switched away. The
//! last thread to leave ends the process: its children go to the kernel,
//! its parent is notified, and the process stays a zombie until reaped.

use super::errno::*;
use super::exec::group_chan;
use super::sched::{current, prepare_to_wait, schedule, wakeup, TABLE};
use super::signal::{self, GroupExit};
use super::task::{State, Task, ThreadGroup};
use super::{clone, futex, ipc, irq, tlb, uaccess, with_current, Pid};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use x86_64::instructions::interrupts;

/// Channel a parent sleeps on in wait4.
fn child_chan(parent: Pid) -> usize {
    0x5_0000_0000 + parent as usize
}

/// Wakes a parent blocked in wait4.
pub fn notify_parent(ppid: Pid) {
    wakeup(child_chan(ppid));
}

/// exit_group(2), and death by a signal: every thread of the process ends;
/// `status` becomes the process's wait status unless it is already ending.
pub fn exit_group(status: i32) -> ! {
    let me = current();
    let others: Vec<Arc<Task>> = {
        let info = me.group.info.lock();
        let mut g = me.group.sig.lock();
        match g.exit {
            // Already ending (or another thread execs): just this thread.
            GroupExit::Exiting(_) | GroupExit::Exec(_) => Vec::new(),
            GroupExit::None => {
                g.exit = GroupExit::Exiting(status);
                info.threads.iter().filter(|t| !core::ptr::eq(&***t, me)).cloned().collect()
            }
        }
    };
    for t in &others {
        signal::kill_thread(t);
    }
    drop(others);
    exit_thread(status)
}

/// exit(2) of one thread. If it is the last one, the process ends with
/// `status` (or the status of a group exit in progress).
pub fn exit_thread(status: i32) -> ! {
    let me = current();
    assert!(me.tgid() != 0, "kernel task must not exit");
    // A vfork parent may go on: this child no longer uses its memory.
    clone::release_vfork();
    // Joining threads wait for the word to become 0. Best effort: the
    // memory may be gone already.
    let ctid = with_current(|p| core::mem::take(&mut p.clear_child_tid));
    if ctid != 0 {
        if uaccess::write(ctid, 0u32).is_ok() {
            let _ = futex::wake_one(ctid);
        }
    }
    // Shared state goes before interrupts are disabled: closing the last
    // reference to a file may wake others or talk to a server.
    let (files, fs, server) = with_current(|p| (p.files.take(), p.fs.take(), p.server.take()));
    drop(files);
    drop(fs);
    drop(server);
    interrupts::disable();
    let mm = unsafe { me.own() }.mm.take();
    tlb::switch(mm.as_ref().map(|m| &*m.tlb), None, false);
    drop(mm);
    unsafe { me.own() }.io_bitmap = None;

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
            table.zombies += 1;
        }
        last
    };
    // An exec in another thread waits for this one to be gone.
    wakeup(group_chan(group.tgid));
    if last {
        let main_status = group.info.lock().main_status;
        let status = match group.sig.lock().exit {
            GroupExit::Exiting(s) => s,
            _ => main_status.unwrap_or(status),
        };
        process_exit(&group, status);
    } else {
        // Process signals this thread was meant to take go to another.
        signal::retarget(&group);
    }
    {
        let _w = me.wake_lock.lock();
        me.set_state(State::Dead);
    }
    schedule();
    unreachable!("an exited task was scheduled again");
}

/// The last thread of `group` left: the process becomes a zombie.
fn process_exit(group: &Arc<ThreadGroup>, status: i32) {
    let pid = group.tgid;
    ipc::on_exit(pid);
    irq::on_exit(pid);
    // Channels it served lose their service (and their clients learn it).
    super::channel::service_exited(pid);
    // A zombie gets no SIGALRM.
    super::signal::stop_alarm(group);
    // Orphans go to the kernel, which reaps them; those that asked for it
    // (PR_SET_PDEATHSIG) get a signal.
    let mut death_signals: Vec<(Pid, u32)> = Vec::new();
    {
        let table = TABLE.lock();
        for g in table.groups.values() {
            let mut info = g.info.lock();
            if info.ppid == pid && g.tgid != pid {
                info.ppid = 0;
                // The new parent hears of the end the ordinary way.
                info.exit_signal = signal::SIGCHLD;
                if info.pdeath_sig != 0 {
                    death_signals.push((g.tgid, info.pdeath_sig));
                }
            }
        }
    }
    for (pid, sig) in death_signals {
        signal::send(pid, sig);
    }
    let (ppid, exit_signal) = {
        let mut info = group.info.lock();
        info.exit_status = Some(status);
        if let Some(mem) = info.mem.take() {
            info.peak_pages = info.peak_pages.max(mem.peak_pages.load(Ordering::Relaxed));
        }
        (info.ppid, info.exit_signal)
    };
    notify_parent(ppid);
    // A protected (server) parent accepts nothing but SIGCHLD.
    let parent_protected = TABLE.lock().groups.get(&ppid).is_some_and(|g| g.privileged.load(Ordering::Relaxed));
    let exit_signal = if parent_protected && exit_signal != 0 { signal::SIGCHLD } else { exit_signal };
    if exit_signal != 0 {
        signal::send(ppid, exit_signal);
    }
}

const WNOHANG: u64 = 1;
const WUNTRACED: u64 = 2;
const WCONTINUED: u64 = 8;

/// What a child used, as wait4 reports it: CPU time in nanoseconds (user,
/// system) and the most pages resident, its own and those of the children
/// it reaped.
#[derive(Clone, Copy, Default)]
struct Usage {
    time: (u64, u64),
    peak_pages: u64,
}

/// Waits for a child selected like wait4's `pid` (> 0: that child, 0: same
/// process group, -1: any, < -1: group -pid). Returns (pid, wait status,
/// the child's usage), or None with WNOHANG if nothing is ready. Stops and
/// continues are reported with WUNTRACED and WCONTINUED.
fn wait_child(pid: i64, options: u64) -> Result<Option<(Pid, i32, Usage)>, i64> {
    let me = current();
    let my_pid = me.tgid();
    let my_pgid = me.group.info.lock().pgid;
    let wanted = |r: i32| {
        (r == signal::CONTINUED_STATUS && options & WCONTINUED != 0) || (r != signal::CONTINUED_STATUS && options & WUNTRACED != 0)
    };
    loop {
        let wait = prepare_to_wait(child_chan(my_pid));
        let mut any_child = false;
        let mut found: Option<(Pid, i32, bool)> = None;
        // What the child used (a reaped child's moves to the parent's
        // account).
        let mut usage = Usage::default();
        {
            let mut table = TABLE.lock();
            for g in table.groups.values() {
                if g.tgid == my_pid {
                    continue;
                }
                let mut info = g.info.lock();
                let selected = info.ppid == my_pid
                    && match pid {
                        p if p > 0 => g.tgid == p as Pid,
                        0 => info.pgid == my_pgid,
                        -1 => true,
                        p => info.pgid == p.unsigned_abs(),
                    };
                if !selected {
                    continue;
                }
                any_child = true;
                let (own, children) = (info.cputime(), info.children_time);
                let live_peak = info.mem.as_ref().map_or(0, |m| m.peak_pages.load(Ordering::Relaxed));
                usage = Usage {
                    time: (own.0 + children.0, own.1 + children.1),
                    peak_pages: info.peak_pages.max(live_peak).max(info.children_peak),
                };
                if let Some(status) = info.exit_status {
                    found = Some((g.tgid, status, true));
                    break;
                }
                if let Some(r) = info.report.filter(|&r| wanted(r)) {
                    info.report = None;
                    found = Some((g.tgid, r, false));
                    break;
                }
            }
            if let Some((child, _, true)) = found {
                table.groups.remove(&child);
                table.zombies -= 1;
                let mut info = me.group.info.lock();
                info.children_time.0 += usage.time.0;
                info.children_time.1 += usage.time.1;
                info.children_peak = info.children_peak.max(usage.peak_pages);
            }
        }
        if !any_child {
            return Err(ECHILD);
        }
        if let Some((child, status, _)) = found {
            return Ok(Some((child, status, usage)));
        }
        if options & WNOHANG != 0 {
            return Ok(None);
        }
        // Checked after the scan: a finished child wins over a signal.
        if signal::interrupted() {
            return Err(EINTR);
        }
        wait.sleep();
    }
}

/// wait4(pid, status, options, rusage): the child's wait status and, at
/// `rusage`, what it used (with its reaped children, as Linux reports a
/// reaped child; a stopped one's so far).
pub fn wait4(pid: i64, status_ptr: u64, options: u64, rusage: u64) -> SysResult {
    match wait_child(pid, options)? {
        Some((pid, status, usage)) => {
            if status_ptr != 0 {
                uaccess::write(status_ptr, status)?;
            }
            if rusage != 0 {
                super::sys_time::write_rusage(rusage, usage.time, usage.peak_pages)?;
            }
            Ok(pid as i64)
        }
        None => Ok(0),
    }
}

/// For the kernel shell: blocks until a specific child exits or stops.
pub fn wait_for(pid: Pid) -> Result<i32, i64> {
    wait_child(pid as i64, WUNTRACED).map(|r| r.expect("blocking wait always yields a result").1)
}

/// Reaps zombies whose parent (the kernel) no longer waits for them.
pub fn reap_orphans() {
    while let Ok(Some(_)) = wait_child(-1, WNOHANG) {}
}

pub enum WaitStatus {
    Exited(i32),
    Killed(i32),
    Stopped(i32),
}

pub fn decode_status(status: i32) -> WaitStatus {
    match status & 0x7f {
        0 => WaitStatus::Exited((status >> 8) & 0xff),
        0x7f => WaitStatus::Stopped((status >> 8) & 0xff),
        sig => WaitStatus::Killed(sig),
    }
}
