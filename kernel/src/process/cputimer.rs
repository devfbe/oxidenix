//! CPU-time alarms: the mechanism under the Linux server's timers on CPU clocks
//! (`ITIMER_VIRTUAL`, `ITIMER_PROF`, POSIX timers on `CLOCK_PROCESS_CPUTIME_ID`,
//! `CLOCK_THREAD_CPUTIME_ID` and the CPU clocks of other processes and threads, and
//! clock_nanosleep on those clocks; `restricted::SYS_CPU_ALARM`). The kernel knows no timer, signal or interval: the server
//! arms one deadline per clock of a process or thread (the earliest of its timers on it), and
//! the kernel tells it when the clock reaches it, once, by advancing and waking a word of
//! the server's (the timer thread sleeps on it), so the server never polls a CPU clock.
//!
//! A clock counts what the server's clocks read (`proc_info`, `thread_info`): user time, or
//! user and system time (all of it, the exactly measured run time). A thread's clock is
//! looked at on its way back to user mode after a timer tick that found it running
//! (`check`), as Linux's tick looks at the CPU timers of the task it interrupted (and runs
//! their expiry on the way back to user space, `CONFIG_POSIX_CPU_TIMERS_TASK_WORK`): an
//! alarm comes at most a tick after its deadline, wherever the time was spent; a thread
//! that does not run does not move its clocks. A process's clock is its threads', live and
//! ended, summed when a tick finds one of them running while an alarm of it is armed.

use super::task::Task;
use core::sync::atomic::{AtomicU64, Ordering};

/// The clocks an alarm watches: user time, and user and system time.
pub const USER: usize = 0;
pub const TOTAL: usize = 1;

/// A process's or thread's armed deadlines, one per clock (nanoseconds of that clock; 0:
/// none).
pub struct Alarms {
    deadline: [AtomicU64; 2],
}

impl Alarms {
    pub const fn new() -> Alarms {
        Alarms { deadline: [AtomicU64::new(0), AtomicU64::new(0)] }
    }

    fn armed(&self) -> bool {
        self.deadline.iter().any(|d| d.load(Ordering::Relaxed) != 0)
    }

    /// Arms `clock` for `deadline` (0: disarms), unless the clock, now `now`, is there
    /// already: then nothing is armed and the answer is true (the caller acts at once).
    pub fn arm(&self, clock: usize, deadline: u64, now: u64) -> bool {
        let slot = &self.deadline[clock];
        slot.store(deadline, Ordering::SeqCst);
        if deadline == 0 || now < deadline {
            return false;
        }
        // Past already: taken back (or a tick fired it meanwhile, which the caller's next
        // look sees as well).
        let _ = slot.compare_exchange(deadline, 0, Ordering::SeqCst, Ordering::Relaxed);
        true
    }

    /// Fires the alarms the clocks, now `user` and `total`, reached: each disarms (once,
    /// against a concurrent arm or check). Whether one did.
    fn fire(&self, user: u64, total: u64) -> bool {
        let mut fired = false;
        for (slot, now) in self.deadline.iter().zip([user, total]) {
            let d = slot.load(Ordering::Relaxed);
            if d != 0 && now >= d && slot.compare_exchange(d, 0, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                fired = true;
            }
        }
        fired
    }
}

/// The calling thread, on its way back to user mode (from an interrupt that found it in
/// user mode, or from a system call): if a tick counted it since it last looked, the
/// alarms of its own clocks and of its process's that their clocks reached fire, and its
/// instance's server hears of it. Only where the thread's own state may be used (the
/// instance is found through it).
pub fn check(task: &Task) {
    let ticks = task.utime.load(Ordering::Relaxed).wrapping_add(task.stime.load(Ordering::Relaxed));
    if task.alarm_ticks.load(Ordering::Relaxed) == ticks {
        return;
    }
    task.alarm_ticks.store(ticks, Ordering::Relaxed);
    let mut fired = false;
    if task.alarms.armed() {
        let (user, system) = task.cputime();
        fired |= task.alarms.fire(user, user + system);
    }
    let group = &task.group;
    if group.alarms.armed() {
        let (user, system) = group.info.lock().cputime();
        fired |= group.alarms.fire(user, user + system);
    }
    if fired {
        super::linux::cpu_alarm_fired();
    }
}
