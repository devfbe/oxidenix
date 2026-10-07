//! Waiting for any of several files at once (poll, select).
//!
//! A `PollTable` puts one waiter on the wait queue of every file it polls,
//! the first time it checks that file, and takes them all off when the
//! call returns. A wakeup on any of them ends the wait, so poll returns as
//! soon as a pipe gets data or a key is typed, not at the next re-check.
//! Files whose readiness lives in a user-space server (sockets) have no
//! wait queue in the kernel; while one is polled, readiness is also
//! re-checked every scheduler tick.
//!
//! The protocol cannot lose a wakeup: `rearm` forgets earlier wakeups
//! before the files are checked, and `wait` sleeps only if none came
//! since. A file that becomes ready after its check wakes the waiter
//! (it is on the file's queue by then), before or during the sleep.

use super::errno::*;
use super::sched::{add_poll_waiter, prepare_to_sleep, remove_poll_waiter, PollWaiter};
use super::signal::interrupted;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub struct PollTable {
    waiter: Arc<PollWaiter>,
    chans: Vec<usize>,
    /// A polled file has no wait queue: re-check every tick.
    recheck: bool,
}

impl PollTable {
    pub fn new() -> PollTable {
        PollTable { waiter: Arc::new(PollWaiter::new()), chans: Vec::new(), recheck: false }
    }

    /// Listens on wait channel `chan` (once, however often it is added).
    pub fn add(&mut self, chan: usize) -> Result<(), i64> {
        if self.chans.contains(&chan) {
            return Ok(());
        }
        self.chans.try_reserve(1).map_err(|_| ENOMEM)?;
        add_poll_waiter(chan, &self.waiter)?;
        self.chans.push(chan);
        Ok(())
    }

    /// A polled file can become ready without a wakeup.
    pub fn recheck(&mut self) {
        self.recheck = true;
    }

    /// Forgets earlier wakeups; call before checking the files.
    pub fn rearm(&self) {
        self.waiter.rearm();
    }

    /// Sleeps until a file woke the table (since `rearm`), `deadline`
    /// (nanoseconds since boot; None: none) or a signal (EINTR).
    pub fn wait(&self, deadline: Option<u64>) -> Result<(), i64> {
        let wait = prepare_to_sleep();
        if self.waiter.woken() {
            return Ok(());
        }
        if interrupted() {
            return Err(EINTR);
        }
        let deadline = if self.recheck {
            let tick = crate::time::now().saturating_add(crate::timer::TICK_NS);
            Some(deadline.map_or(tick, |d| d.min(tick)))
        } else {
            deadline
        };
        match deadline {
            Some(d) => wait.sleep_until(d),
            None => wait.sleep(),
        }
        Ok(())
    }
}

impl Drop for PollTable {
    fn drop(&mut self) {
        for &chan in &self.chans {
            remove_poll_waiter(chan, &self.waiter);
        }
    }
}
