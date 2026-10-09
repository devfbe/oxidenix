//! eventfd (phase R6b): a counter that reads take and writes add to, a
//! file of the server (an open file description of its table's, `files`).
//! Waiting and readiness reports work as for pipes (see `pipe`): a
//! sequence word bumped on every change, an interruptible futex, reports
//! under the counter's lock.

use crate::files::{self, EFAULT, EINVAL};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const EAGAIN: i64 = 11;
const EINTR: i64 = 4;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;

/// The largest count; a write that would go beyond it waits.
const MAX: u64 = u64::MAX - 1;

pub struct EventFd {
    count: Mutex<u64>,
    seq: AtomicU32,
    semaphore: bool,
    id: u64,
}

impl EventFd {
    pub fn new(initial: u64, semaphore: bool) -> EventFd {
        EventFd { count: Mutex::new(initial), seq: AtomicU32::new(0), semaphore, id: files::new_id() }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    fn readiness(count: u64) -> i16 {
        (if count > 0 { POLLIN } else { 0 }) | if count < MAX { POLLOUT } else { 0 }
    }

    pub fn ready(&self) -> i16 {
        Self::readiness(*self.count.lock())
    }

    /// After a change (lock held): wake the waiters and report, every time
    /// (each change is an event for EPOLLET).
    fn changed(&self, count: u64) {
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        files::ready(self.id, Self::readiness(count));
    }

    fn wait(&self, seen: u32) -> Result<(), i64> {
        let word = &self.seq as *const AtomicU32 as u64;
        match syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, 0, FUTEX_INTERRUPTIBLE, 0, 0]) {
            r if r == -EINTR => Err(EINTR),
            _ => Ok(()),
        }
    }

    /// Takes the count (1 of it, as a semaphore) into 8 bytes at `buf`;
    /// waits while it is 0.
    pub fn read(&self, buf: u64, len: u64, nonblock: bool) -> Result<i64, i64> {
        if len < 8 {
            return Err(EINVAL);
        }
        loop {
            let seen;
            {
                let mut count = self.count.lock();
                if *count > 0 {
                    let taken = if self.semaphore { 1 } else { *count };
                    usercopy::write(buf, &taken)?;
                    *count -= taken;
                    self.changed(*count);
                    return Ok(8);
                }
                seen = self.seq.load(Ordering::Acquire);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.wait(seen)?;
        }
    }

    /// Adds the 8-byte value at `buf`; waits while the count would exceed
    /// its maximum.
    pub fn write(&self, buf: u64, len: u64, nonblock: bool) -> Result<i64, i64> {
        if len < 8 {
            return Err(EINVAL);
        }
        let value: u64 = usercopy::read(buf)?;
        if value == u64::MAX {
            return Err(EINVAL);
        }
        loop {
            let seen;
            {
                let mut count = self.count.lock();
                if MAX - *count >= value {
                    *count += value;
                    self.changed(*count);
                    return Ok(8);
                }
                seen = self.seq.load(Ordering::Acquire);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.wait(seen)?;
        }
    }

    /// fstat: an anonymous inode (no file type), as on Linux.
    pub fn fstat(&self, buf: u64) -> Result<i64, i64> {
        usercopy::to_program(buf, &self.stat()).map(|_| 0).map_err(|_| EFAULT)
    }

    pub fn stat(&self) -> [u8; 144] {
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.id.to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&0o600u32.to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st
    }
}
