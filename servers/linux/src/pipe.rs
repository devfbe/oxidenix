//! Pipes (phase R6a): a 64 KiB buffer shared by a read end and a write
//! end, each a file of the server with a placeholder in the kernel's
//! descriptor table.
//!
//! Waiting: every change of a pipe (data in or out, an end closed) bumps
//! its sequence word and wakes its waiters; a reader or writer that cannot
//! go on notes the word under the pipe's lock and sleeps while it still
//! holds that value (an interruptible server futex: a signal for the
//! program ends the wait with EINTR, and the call restarts as the kernel's
//! own pipes would). Readiness for poll, select and epoll is reported to
//! the kernel under the same lock, so reports never arrive out of order.
//!
//! As on Linux: a write without readers raises SIGPIPE for the writer
//! (`SYS_SIGNAL_THREAD`) and fails with EPIPE (unless some of it was
//! written), a read without writers returns 0, O_NONBLOCK gives EAGAIN.

use crate::files::{self, EFAULT};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const CAPACITY: usize = 64 * 1024;
const CHUNK: usize = 4096;
const PAGE: u64 = 4096;

/// Where a read's bytes go.
pub enum Dst<'a> {
    /// The program's buffers (base, length).
    Program(&'a [(u64, u64)]),
    /// The server's memory.
    Server(&'a mut [u8]),
}

/// Where a write's bytes come from.
pub enum Src<'a> {
    Program(&'a [(u64, u64)]),
    Server(&'a [u8]),
}

const EAGAIN: i64 = 11;
const EINTR: i64 = 4;
const EPIPE: i64 = 32;

const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;

struct Inner {
    buf: VecDeque<u8>,
    /// Whether each end is still open (both are until their placeholder's
    /// last descriptor goes).
    reader: bool,
    writer: bool,
    /// The readiness last reported for each end (read, write).
    reported: [i16; 2],
}

pub struct Shared {
    inner: Mutex<Inner>,
    /// Bumped on every change; waiters sleep on it.
    seq: AtomicU32,
    ids: [u64; 2],
}

pub struct PipeEnd {
    shared: Arc<Shared>,
    write: bool,
}

/// A new pipe: (read end, write end).
pub fn new() -> (Arc<PipeEnd>, Arc<PipeEnd>) {
    let shared = Arc::new(Shared {
        inner: Mutex::new(Inner { buf: VecDeque::new(), reader: true, writer: true, reported: [0, POLLOUT] }),
        seq: AtomicU32::new(0),
        ids: [files::new_id(), files::new_id()],
    });
    (Arc::new(PipeEnd { shared: shared.clone(), write: false }), Arc::new(PipeEnd { shared, write: true }))
}

impl Inner {
    /// (read end, write end) readiness.
    fn readiness(&self) -> [i16; 2] {
        let read = if !self.writer {
            POLLIN | POLLHUP
        } else if !self.buf.is_empty() {
            POLLIN
        } else {
            0
        };
        let write = if !self.reader {
            POLLERR
        } else if self.buf.len() < CAPACITY {
            POLLOUT
        } else {
            0
        };
        [read, write]
    }
}

impl Shared {
    /// After a change (lock held): wake the waiters, report what changed.
    /// New data is an event for the read end even if its readiness stays
    /// (an edge for EPOLLET), freed room one for the write end.
    fn changed(&self, inner: &mut Inner, data_in: bool, room_out: bool) {
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        let now = inner.readiness();
        let event = [data_in, room_out];
        for end in 0..2 {
            if now[end] != inner.reported[end] || event[end] {
                inner.reported[end] = now[end];
                files::ready(self.ids[end], now[end]);
            }
        }
    }

    /// Sleeps while the sequence word is `seen`; EINTR for a signal.
    fn wait(&self, seen: u32) -> Result<(), i64> {
        let word = &self.seq as *const AtomicU32 as u64;
        match syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, 0, FUTEX_INTERRUPTIBLE, 0, 0]) {
            r if r == -EINTR => Err(EINTR),
            _ => Ok(()),
        }
    }
}

impl PipeEnd {
    pub fn id(&self) -> u64 {
        self.shared.ids[self.write as usize]
    }

    pub fn readiness(&self) -> i16 {
        self.shared.inner.lock().readiness()[self.write as usize]
    }

    /// The end's placeholder is gone.
    pub fn close(&self) {
        let mut inner = self.shared.inner.lock();
        if self.write {
            inner.writer = false;
        } else {
            inner.reader = false;
        }
        self.shared.changed(&mut inner, false, false);
    }

    /// Reads into `dst`: what is there, waiting while the pipe is empty and
    /// has a writer.
    pub fn read(&self, mut dst: Dst, nonblock: bool) -> Result<i64, i64> {
        let want: u64 = match &dst {
            Dst::Program(vecs) => vecs.iter().map(|v| v.1).sum(),
            Dst::Server(buf) => buf.len() as u64,
        };
        if want == 0 {
            return Ok(0);
        }
        loop {
            let seen;
            {
                let mut inner = self.shared.inner.lock();
                if !inner.buf.is_empty() {
                    let done = copy_out(&mut inner, &mut dst)?;
                    self.shared.changed(&mut inner, false, done > 0);
                    return Ok(done as i64);
                }
                if !inner.writer {
                    return Ok(0);
                }
                seen = self.shared.seq.load(Ordering::Acquire);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.shared.wait(seen)?;
        }
    }

    /// Writes all of `src`, waiting for room (with O_NONBLOCK what fits).
    pub fn write(&self, src: Src, nonblock: bool) -> Result<i64, i64> {
        let mut written: u64 = 0;
        let mut chunk = [0u8; CHUNK];
        let pieces: alloc::vec::Vec<(u64, u64)> = match &src {
            Src::Program(vecs) => vecs.to_vec(),
            Src::Server(buf) => alloc::vec![(0, buf.len() as u64)],
        };
        for (base, len) in pieces {
            let mut at = 0;
            while at < len {
                // A chunk never crosses a page of the program's buffer, so a
                // hole there ends the write exactly where it begins.
                let n = match &src {
                    Src::Program(_) => {
                        let n = (len - at).min(CHUNK as u64).min(PAGE - (base + at) % PAGE) as usize;
                        if let Err(e) = usercopy::from_program(base + at, &mut chunk[..n]) {
                            return if written > 0 { Ok(written as i64) } else { Err(e) };
                        }
                        n
                    }
                    Src::Server(buf) => {
                        let n = (len - at).min(CHUNK as u64) as usize;
                        chunk[..n].copy_from_slice(&buf[at as usize..at as usize + n]);
                        n
                    }
                };
                let mut pushed = 0;
                while pushed < n {
                    let seen;
                    {
                        let mut inner = self.shared.inner.lock();
                        if !inner.reader {
                            drop(inner);
                            const SIGPIPE: u64 = 13;
                            syscall(SYS_SIGNAL_THREAD, [SIGPIPE, 0, 0, 0, 0, 0]);
                            return if written > 0 { Ok(written as i64) } else { Err(EPIPE) };
                        }
                        let room = CAPACITY.saturating_sub(inner.buf.len()).min(n - pushed);
                        if room > 0 {
                            if inner.buf.try_reserve(room).is_err() {
                                return if written > 0 { Ok(written as i64) } else { Err(EAGAIN) };
                            }
                            inner.buf.extend(&chunk[pushed..pushed + room]);
                            pushed += room;
                            written += room as u64;
                            self.shared.changed(&mut inner, true, false);
                            continue;
                        }
                        seen = self.shared.seq.load(Ordering::Acquire);
                    }
                    if nonblock {
                        return if written > 0 { Ok(written as i64) } else { Err(EAGAIN) };
                    }
                    if let Err(e) = self.shared.wait(seen) {
                        return if written > 0 { Ok(written as i64) } else { Err(e) };
                    }
                }
                at += n as u64;
            }
        }
        Ok(written as i64)
    }

    /// fstat: a FIFO, as the kernel's pipes were.
    pub fn fstat(&self, buf: u64) -> Result<i64, i64> {
        const S_IFIFO: u32 = 0o010000;
        if buf == 0 {
            return Err(EFAULT);
        }
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.id().to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&(S_IFIFO | 0o600).to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        usercopy::to_program(buf, &st).map(|_| 0).map_err(|_| EFAULT)
    }
}

/// Moves buffered bytes to `dst` (lock held); only what reached it leaves
/// the pipe. A chunk never crosses a page of the program's buffer, so a
/// hole there ends the read exactly where it begins.
fn copy_out(inner: &mut Inner, dst: &mut Dst) -> Result<usize, i64> {
    match dst {
        Dst::Server(buf) => {
            let n = buf.len().min(inner.buf.len());
            for (d, s) in buf[..n].iter_mut().zip(inner.buf.drain(..n)) {
                *d = s;
            }
            Ok(n)
        }
        Dst::Program(vecs) => {
            let mut done = 0;
            let mut chunk = [0u8; CHUNK];
            for &(base, len) in vecs.iter() {
                let mut at = 0;
                while at < len {
                    if inner.buf.is_empty() {
                        return Ok(done);
                    }
                    let n = (len - at).min(CHUNK as u64).min(PAGE - (base + at) % PAGE).min(inner.buf.len() as u64) as usize;
                    for (d, s) in chunk[..n].iter_mut().zip(inner.buf.iter()) {
                        *d = *s;
                    }
                    if let Err(e) = usercopy::to_program(base + at, &chunk[..n]) {
                        return if done > 0 { Ok(done) } else { Err(e) };
                    }
                    inner.buf.drain(..n);
                    at += n as u64;
                    done += n;
                }
            }
            Ok(done)
        }
    }
}
