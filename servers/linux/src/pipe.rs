//! Pipes (phase R6a): a 64 KiB buffer shared by a read end and a write
//! end, each a file of the server (an open file description of its own).
//! Opening /proc/<pid>/fd/N of one adds an end (`reopen`,
//! as Linux's fifo_open): a pipe has readers and writers, it reads end of
//! file once no writer is left and fails writes once no reader is.
//!
//! Waiting: every change of a pipe (data in or out, an end closed) bumps
//! its sequence word and wakes its waiters; a reader or writer that cannot
//! go on notes the word under the pipe's lock and sleeps while it still
//! holds that value (an interruptible server futex: a signal for the
//! program ends the wait with EINTR, and the call restarts as the kernel's
//! own pipes would). Readiness for poll, select and epoll is reported to
//! the kernel under the same lock, so reports never arrive out of order.
//! That lock is never held while program memory is copied (a fault there
//! may wait for the pager, which closes pipes): readers copy out of a
//! chunk taken under it, one reader at a time (`drain`).
//!
//! As on Linux: a write without readers raises SIGPIPE for the writer
//! (`signal::raise_thread`) and fails with EPIPE (unless some of it was
//! written), a read without writers returns 0, O_NONBLOCK gives EAGAIN.

use crate::files::{self, EFAULT};
use crate::sync::Mutex;
use crate::syscall;
use crate::unix::Sink;
use crate::usercopy;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
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
    /// Open ends that read and that write (each counted until its
    /// description's last reference goes; a pipe reopened through
    /// /proc/<pid>/fd gets more).
    readers: u32,
    writers: u32,
    /// Every open end: its description's id, its roles and the readiness last
    /// reported for it.
    ends: Vec<Reported>,
}

struct Reported {
    id: u64,
    reads: bool,
    writes: bool,
    ready: i16,
    /// Its description exists: its watch takes reports. An end
    /// `reopen` made gets them once installed (`PipeEnd::installed`).
    installed: bool,
}

pub struct Shared {
    inner: Mutex<Inner>,
    /// Reads, one at a time: held across the copy to the program, where
    /// `inner` is not (see `drain`). Never taken by a service thread.
    rlock: Mutex<()>,
    /// Bumped on every change; waiters sleep on it.
    seq: AtomicU32,
    /// Its inode number (fstat, /proc/<pid>/fd's "pipe:[ino]"): every end's.
    ino: u64,
}

/// An end: an open file description of the pipe that reads or writes (or
/// both: one reopened O_RDWR).
pub struct PipeEnd {
    shared: Arc<Shared>,
    id: u64,
    reads: bool,
    writes: bool,
}

/// A new pipe: (read end, write end).
pub fn new() -> (Arc<PipeEnd>, Arc<PipeEnd>) {
    let (rid, wid) = (files::new_id(), files::new_id());
    // (Nobody can change the pipe before both are installed: no report is lost.)
    let ends = alloc::vec![
        Reported { id: rid, reads: true, writes: false, ready: 0, installed: true },
        Reported { id: wid, reads: false, writes: true, ready: POLLOUT, installed: true },
    ];
    let shared = Arc::new(Shared {
        inner: Mutex::new(Inner { buf: VecDeque::new(), readers: 1, writers: 1, ends }),
        rlock: Mutex::new(()),
        seq: AtomicU32::new(0),
        ino: rid,
    });
    (Arc::new(PipeEnd { shared: shared.clone(), id: rid, reads: true, writes: false }), Arc::new(PipeEnd { shared, id: wid, reads: false, writes: true }))
}

/// The pipe itself, apart from its ends (what /proc/<pid>/fd/N of an end
/// leads to): it keeps the buffer, not an end, so it holds no reader or
/// writer open.
#[derive(Clone)]
pub struct Pipe(Arc<Shared>);

impl Pipe {
    /// Another end of the pipe, as open(2) of /proc/<pid>/fd/N makes one
    /// (Linux's fifo_open of a pipe: no waiting for a partner): reading for
    /// O_RDONLY, writing for O_WRONLY, both for O_RDWR. The caller installs
    /// it under its `id`, then calls `installed` (or `close` if installing
    /// failed): the end's watch takes readiness reports only once its
    /// description exists, so the end gets them from then on, and its
    /// readiness is reported again then (a change between the two is not
    /// lost).
    pub fn reopen(&self, flags: u32) -> Arc<PipeEnd> {
        let mode = flags & files::O_ACCMODE;
        let (reads, writes) = (mode != files::O_WRONLY, mode != 0);
        let id = files::new_id();
        let shared = self.0.clone();
        {
            let mut inner = shared.inner.lock();
            inner.readers += reads as u32;
            inner.writers += writes as u32;
            let ready = inner.readiness(reads, writes);
            inner.ends.push(Reported { id, reads, writes, ready, installed: false });
            // A pipe that had no reader or no writer has one now.
            shared.changed(&mut inner, false, false);
        }
        Arc::new(PipeEnd { shared, id, reads, writes })
    }

    /// Its `struct stat`: a FIFO of its own inode, as each end's.
    pub fn stat(&self) -> [u8; 144] {
        self.0.stat()
    }
}

impl Inner {
    /// The readiness of an end that reads and/or writes.
    fn readiness(&self, reads: bool, writes: bool) -> i16 {
        let mut ready = 0;
        if reads {
            ready |= if self.writers == 0 {
                POLLIN | POLLHUP
            } else if !self.buf.is_empty() {
                POLLIN
            } else {
                0
            };
        }
        if writes {
            ready |= if self.readers == 0 {
                POLLERR
            } else if self.buf.len() < CAPACITY {
                POLLOUT
            } else {
                0
            };
        }
        ready
    }
}

impl Shared {
    /// After a change (lock held): wake the waiters, report what changed.
    /// New data is an event for the ends that read even if their readiness
    /// stays (an edge for EPOLLET), freed room one for those that write.
    fn changed(&self, inner: &mut Inner, data_in: bool, room_out: bool) {
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        for i in 0..inner.ends.len() {
            let (reads, writes) = (inner.ends[i].reads, inner.ends[i].writes);
            let now = inner.readiness(reads, writes);
            let event = (data_in && reads) || (room_out && writes);
            let end = &mut inner.ends[i];
            if end.installed && (now != end.ready || event) {
                end.ready = now;
                files::ready(end.id, now);
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
        self.id
    }

    /// The pipe's inode number.
    pub fn ino(&self) -> u64 {
        self.shared.ino
    }

    pub fn readiness(&self) -> i16 {
        self.shared.inner.lock().readiness(self.reads, self.writes)
    }

    /// The end's description is installed (`reopen`): it gets reports from
    /// now on, and its readiness now (as an event: data may have come
    /// before its watch existed).
    pub fn installed(&self) {
        let mut inner = self.shared.inner.lock();
        let now = inner.readiness(self.reads, self.writes);
        if let Some(end) = inner.ends.iter_mut().find(|e| e.id == self.id) {
            end.installed = true;
            end.ready = now;
            files::ready(self.id, now);
        }
    }

    /// The end's description is gone.
    pub fn close(&self) {
        let mut inner = self.shared.inner.lock();
        inner.readers -= self.reads as u32;
        inner.writers -= self.writes as u32;
        inner.ends.retain(|e| e.id != self.id);
        self.shared.changed(&mut inner, false, false);
    }

    /// Reads into `dst`: what is there, waiting while the pipe is empty and
    /// has a writer.
    pub fn read(&self, dst: Dst, nonblock: bool) -> Result<i64, i64> {
        let mut sink = match dst {
            Dst::Program(vecs) => Sink::program(vecs),
            Dst::Server(buf) => Sink::Server { buf, at: 0 },
        };
        if sink.room() == 0 {
            return Ok(0);
        }
        let mut reader = Some(self.shared.rlock.lock());
        loop {
            let seen;
            {
                let inner = self.shared.inner.lock();
                if !inner.buf.is_empty() {
                    drop(inner);
                    let r = self.drain(&mut sink);
                    drop(reader);
                    return r;
                }
                if inner.writers == 0 {
                    return Ok(0);
                }
                seen = self.shared.seq.load(Ordering::Acquire);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            // Not holding the other readers off while it sleeps.
            drop(reader.take());
            self.shared.wait(seen)?;
            reader = Some(self.shared.rlock.lock());
        }
    }

    /// Moves buffered bytes to `sink` (the reader's lock held), a chunk at
    /// a time: copied out under the pipe's lock into the server's memory,
    /// to the program with no lock held (a fault may wait for the pager,
    /// which closes pipes), and only what reached it leaves the pipe.
    /// Readers are serialized, so the bytes stay at the front meanwhile.
    fn drain(&self, sink: &mut Sink) -> Result<i64, i64> {
        let mut chunk = [0u8; CHUNK];
        let mut done = 0;
        loop {
            let n = {
                let inner = self.shared.inner.lock();
                let n = inner.buf.len().min(CHUNK).min(sink.room());
                for (d, s) in chunk[..n].iter_mut().zip(inner.buf.iter()) {
                    *d = *s;
                }
                n
            };
            if n == 0 {
                break;
            }
            let k = match sink.put(&chunk[..n]) {
                Ok(k) => k,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            };
            {
                let mut inner = self.shared.inner.lock();
                inner.buf.drain(..k);
                self.shared.changed(&mut inner, false, k > 0);
            }
            done += k;
            if k < n {
                break;
            }
        }
        Ok(done as i64)
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
                        if inner.readers == 0 {
                            drop(inner);
                            crate::signal::raise_thread(crate::signal::SIGPIPE);
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
        if buf == 0 {
            return Err(EFAULT);
        }
        usercopy::to_program(buf, &self.stat()).map(|_| 0).map_err(|_| EFAULT)
    }

    /// Its `struct stat`: a FIFO of its own inode.
    pub fn stat(&self) -> [u8; 144] {
        self.shared.stat()
    }

    /// The pipe this is an end of.
    pub fn pipe(&self) -> Pipe {
        Pipe(self.shared.clone())
    }
}

impl Shared {
    fn stat(&self) -> [u8; 144] {
        const S_IFIFO: u32 = 0o010000;
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.ino.to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&(S_IFIFO | 0o600).to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st
    }
}
