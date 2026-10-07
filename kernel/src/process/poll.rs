//! Waiting for any of several files at once (poll, select, epoll_wait).
//!
//! A file announces changes of its readiness on a wait queue (its
//! `PollSource`): a channel of the global wait queues for pipes, eventfds
//! and the TTY, an epoll instance's own queue for that instance. A
//! `Registration` puts a waker on that queue until it is dropped.
//!
//! A `PollTable` registers one waker on the source of every file it
//! checks, the first time it checks that file, and unregisters them all
//! when the call returns. A wakeup on any of them ends the wait, so poll
//! returns as soon as a pipe gets data or a key is typed, not at the next
//! re-check. Files whose readiness lives in a user-space server (sockets)
//! announce nothing in the kernel; while one is polled, readiness is also
//! re-checked every scheduler tick.
//!
//! The protocol cannot lose a wakeup: `rearm` forgets earlier wakeups
//! before the files are checked, and `wait` sleeps only if none came
//! since. A file that becomes ready after its check wakes the waiter
//! (it is on the file's queue by then), before or during the sleep.

use super::epoll::Epoll;
use super::errno::*;
use super::sched::{prepare_to_sleep, queue_of, PollWaiter, Waker};
use super::signal::interrupted;
use crate::fs::file::OpenFile;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Where a file announces that its readiness changed.
pub enum PollSource {
    /// Never: it is always ready (regular files, devices).
    Always,
    /// On a channel of the global wait queues.
    Chan(usize),
    /// On an epoll instance's own queue.
    Epoll(Arc<Epoll>),
    /// Never, though it changes (sockets): it has to be checked again.
    Recheck,
}

/// A waker on a file's wait queue; dropping it takes it off again.
pub struct Registration {
    on: Registered,
    waker: Arc<dyn Waker>,
}

enum Registered {
    Chan(usize),
    Epoll(Arc<Epoll>),
}

impl Registration {
    /// Registers `waker` on `source`; None for a source that announces
    /// nothing.
    pub fn new(source: &PollSource, waker: Arc<dyn Waker>) -> Result<Option<Registration>, i64> {
        let on = match source {
            PollSource::Always | PollSource::Recheck => return Ok(None),
            PollSource::Chan(chan) => {
                queue_of(*chan).add_waker(*chan, waker.clone())?;
                Registered::Chan(*chan)
            }
            PollSource::Epoll(ep) => {
                ep.queue().add_waker(0, waker.clone())?;
                Registered::Epoll(ep.clone())
            }
        };
        Ok(Some(Registration { on, waker }))
    }

    fn is_on(&self, source: &PollSource) -> bool {
        match (&self.on, source) {
            (Registered::Chan(a), PollSource::Chan(b)) => a == b,
            (Registered::Epoll(a), PollSource::Epoll(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        match &self.on {
            Registered::Chan(chan) => queue_of(*chan).remove_waker(*chan, &self.waker),
            Registered::Epoll(ep) => ep.queue().remove_waker(0, &self.waker),
        }
    }
}

pub struct PollTable {
    waiter: Arc<PollWaiter>,
    registrations: Vec<Registration>,
    /// A polled file announces nothing: re-check every tick.
    recheck: bool,
}

impl PollTable {
    pub fn new() -> PollTable {
        PollTable { waiter: Arc::new(PollWaiter::new()), registrations: Vec::new(), recheck: false }
    }

    /// Listens for changes of `file`'s readiness (once per source).
    pub fn watch(&mut self, file: &OpenFile) -> Result<(), i64> {
        self.watch_source(&file.poll_source())
    }

    pub fn watch_source(&mut self, source: &PollSource) -> Result<(), i64> {
        if matches!(source, PollSource::Recheck) {
            self.recheck = true;
        }
        if self.registrations.iter().any(|r| r.is_on(source)) {
            return Ok(());
        }
        self.registrations.try_reserve(1).map_err(|_| ENOMEM)?;
        if let Some(r) = Registration::new(source, self.waiter.clone())? {
            self.registrations.push(r);
        }
        Ok(())
    }

    /// Re-checks every tick even without a file that needs it.
    pub fn set_recheck(&mut self, recheck: bool) {
        self.recheck = recheck;
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
