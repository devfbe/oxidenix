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
//!
//! A FIFO (`fifo`) is a pipe its node keeps while anyone has it open
//! (`fifo_pipe`: the node's slot holds it weakly, the ends strongly; with the
//! last end the buffer goes, as Linux's free_pipe_info). Its ends are opened as
//! Linux's fifo_open opens them (`open_fifo`): a reader waits for a writer and
//! a writer for a reader unless O_NONBLOCK (then a writer without readers is
//! ENXIO), O_RDWR never waits; the wait is for a partner's open (the pipe's
//! reader and writer counters move), not for one that stays, and a signal
//! ends it (EINTR, restarted as SA_RESTART says: Linux's ERESTARTSYS). A
//! reader opened O_NONBLOCK while no writer was there polls without
//! POLLHUP until a writer came (its `gate`, Linux's f_pipe).

use crate::files::{self, EFAULT};
use crate::heap::{self, NoMem, TryGrow};
use crate::sync::Mutex;
use crate::syscall;
use crate::unix::Sink;
use crate::usercopy;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
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
const ENXIO: i64 = 6;
const EINVAL: i64 = 22;
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
    /// How many ends that read and that write were ever opened (wrapping;
    /// Linux's r_counter and w_counter): what a FIFO's opener waits to move.
    r_counter: u32,
    w_counter: u32,
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
    /// `reopen` or `open_fifo` made gets them once installed
    /// (`PipeEnd::installed`).
    installed: bool,
    /// No POLLHUP while the writer counter is still this (a FIFO's reader
    /// opened O_NONBLOCK without writers: it has seen no writer yet).
    gate: Option<u32>,
}

pub struct Shared {
    inner: Mutex<Inner>,
    /// Reads, one at a time: held across the copy to the program, where
    /// `inner` is not (see `drain`). Never taken by a service thread.
    rlock: crate::sync::SleepLock,
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
    /// A FIFO's end: the node it was opened by (its status, its times and
    /// inotify events, `fifo::Named`).
    named: Option<crate::fifo::Named>,
}

/// A new pipe: (read end, write end); ENOMEM if it cannot be had (a
/// program makes as many as its descriptors allow).
pub fn new() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), NoMem> {
    let (rid, wid) = (files::new_id(), files::new_id());
    // (Nobody can change the pipe before both are installed: no report is lost.)
    let mut ends = heap::try_vec(2)?;
    ends.push(Reported { id: rid, reads: true, writes: false, ready: 0, installed: true, gate: None });
    ends.push(Reported { id: wid, reads: false, writes: true, ready: POLLOUT, installed: true, gate: None });
    let inner = Inner { buf: VecDeque::new(), readers: 1, writers: 1, r_counter: 1, w_counter: 1, ends };
    let shared = heap::try_arc(Shared { inner: Mutex::new(inner), rlock: crate::sync::SleepLock::new(()), seq: AtomicU32::new(0), ino: rid })?;
    // (A failure drops what was made: no end was counted yet, nothing to report.)
    let read_end = heap::try_arc(PipeEnd { shared: shared.clone(), id: rid, reads: true, writes: false, named: None })?;
    let write_end = heap::try_arc(PipeEnd { shared, id: wid, reads: false, writes: true, named: None })?;
    Ok((read_end, write_end))
}

/// A FIFO's pipe from its node's `slot`: the one there while any end of it is open, or
/// a new, empty one without ends put there (the caller holds the node's lock, so all its
/// openers get the same). ENOMEM if a new one cannot be had.
pub fn fifo_pipe(slot: &mut Weak<Shared>) -> Result<Arc<Shared>, NoMem> {
    if let Some(shared) = slot.upgrade() {
        return Ok(shared);
    }
    let inner = Inner { buf: VecDeque::new(), readers: 0, writers: 0, r_counter: 0, w_counter: 0, ends: Vec::new() };
    let shared = heap::try_arc(Shared { inner: Mutex::new(inner), rlock: crate::sync::SleepLock::new(()), seq: AtomicU32::new(0), ino: files::new_id() })?;
    *slot = Arc::downgrade(&shared);
    Ok(shared)
}

/// An end of the FIFO pipe `shared` for an open(2) with `flags` of its node (`named`),
/// as Linux's fifo_open: reading for O_RDONLY, writing for O_WRONLY, both for O_RDWR
/// (EINVAL for an access mode of 3); without O_NONBLOCK a reader waits until a writer
/// opens and a writer until a reader opens (EINTR for a signal meanwhile); with it a
/// writer without readers is ENXIO. The end is counted (a partner waiting for it goes
/// on) before it waits. The caller installs it under `id` (its description's, whose
/// watch it made), then calls `installed` (or `close` if installing failed), as for
/// `Pipe::reopen`. ENOMEM if the end cannot be had.
pub fn open_fifo(shared: Arc<Shared>, flags: u32, named: crate::fifo::Named, id: u64) -> Result<Arc<PipeEnd>, i64> {
    let (reads, writes) = match flags & files::O_ACCMODE {
        0 => (true, false),
        files::O_WRONLY => (false, true),
        files::O_RDWR => (true, true),
        _ => return Err(EINVAL),
    };
    let nonblock = flags & files::O_NONBLOCK != 0;
    // (Had before the pipe's lock, which the node's drop must not run under.)
    let end = heap::try_arc(PipeEnd { shared: shared.clone(), id, reads, writes, named: Some(named) })?;
    let mut inner = shared.inner.lock();
    if writes && !reads && nonblock && inner.readers == 0 {
        drop(inner);
        return Err(ENXIO);
    }
    // Room for its report first: nothing is counted if there is none.
    if inner.ends.try_grow(1).is_err() {
        drop(inner);
        return Err(heap::ENOMEM);
    }
    if reads {
        inner.readers += 1;
        inner.r_counter = inner.r_counter.wrapping_add(1);
    }
    if writes {
        inner.writers += 1;
        inner.w_counter = inner.w_counter.wrapping_add(1);
    }
    let gate = (reads && !writes && nonblock && inner.writers == 0).then_some(inner.w_counter);
    let ready = inner.readiness(reads, writes, gate);
    inner.ends.push(Reported { id, reads, writes, ready, installed: false, gate });
    // Partners waiting for this end go on; the other ends' readiness changes.
    shared.changed(&mut inner, false, false);
    if !nonblock && reads != writes {
        // The partner's count, and the counter of its opens to wait for.
        let partner = |i: &Inner| if reads { (i.writers, i.w_counter) } else { (i.readers, i.r_counter) };
        let (present, opened) = partner(&inner);
        if present == 0 {
            loop {
                let seen = shared.seq.load(Ordering::Acquire);
                drop(inner);
                if let Err(e) = shared.wait(seen) {
                    end.close();
                    return Err(e);
                }
                inner = shared.inner.lock();
                if partner(&inner).1 != opened {
                    break;
                }
            }
        }
    }
    drop(inner);
    Ok(end)
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
    /// lost). ENOMEM (nothing changed) if the end cannot be had.
    pub fn reopen(&self, flags: u32) -> Result<Arc<PipeEnd>, NoMem> {
        let mode = flags & files::O_ACCMODE;
        let (reads, writes) = (mode != files::O_WRONLY, mode != 0);
        let id = files::new_id();
        let shared = self.0.clone();
        let end = heap::try_arc(PipeEnd { shared: shared.clone(), id, reads, writes, named: None })?;
        {
            let mut inner = shared.inner.lock();
            // Room for its report first: nothing is counted if there is none.
            inner.ends.try_grow(1)?;
            inner.readers += reads as u32;
            inner.writers += writes as u32;
            inner.r_counter = inner.r_counter.wrapping_add(reads as u32);
            inner.w_counter = inner.w_counter.wrapping_add(writes as u32);
            let ready = inner.readiness(reads, writes, None);
            inner.ends.push(Reported { id, reads, writes, ready, installed: false, gate: None });
            // A pipe that had no reader or no writer has one now.
            shared.changed(&mut inner, false, false);
        }
        Ok(end)
    }

    /// Its `struct stat`: a FIFO of its own inode, as each end's.
    pub fn stat(&self) -> [u8; 144] {
        self.0.stat()
    }
}

impl Inner {
    /// The readiness of an end that reads and/or writes, with its `gate`
    /// (`Reported::gate`): an end gated while no writer came since its open
    /// reports no hangup until one comes (Linux's pipe_poll), only data.
    fn readiness(&self, reads: bool, writes: bool, gate: Option<u32>) -> i16 {
        let mut ready = 0;
        if reads {
            ready |= if self.writers == 0 && gate == Some(self.w_counter) {
                if self.buf.is_empty() { 0 } else { POLLIN }
            } else if self.writers == 0 {
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
            let (reads, writes, gate) = (inner.ends[i].reads, inner.ends[i].writes, inner.ends[i].gate);
            let now = inner.readiness(reads, writes, gate);
            let event = (data_in && reads) || (room_out && writes);
            let end = &mut inner.ends[i];
            if end.installed && (now != end.ready || event) {
                end.ready = now;
                files::ready(end.id, now);
            }
        }
    }

    /// Signal-driven I/O (lock held): the ends that read (`readers`) or that write get `io`
    /// (Linux's kill_fasync of the pipe's fasync_readers or fasync_writers).
    fn sigio(inner: &Inner, readers: bool, io: files::Io) {
        for e in inner.ends.iter().filter(|e| e.installed && if readers { e.reads } else { e.writes }) {
            files::sigio(e.id, io);
        }
    }

    /// A read or write call is done, or about to wait: as Linux's pipe_read and pipe_write
    /// at their end and before they sleep, a write signals the readers (POLL_IN) and a read
    /// the writers (POLL_OUT), whatever it moved.
    fn call_done(&self, write: bool) {
        if !crate::poll::async_in_use() {
            return;
        }
        let inner = self.inner.lock();
        Self::sigio(&inner, write, if write { files::Io::In } else { files::Io::Out });
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
        let inner = self.shared.inner.lock();
        inner.readiness(self.reads, self.writes, self.gate(&inner))
    }

    /// Its `Reported::gate` (found under the pipe's lock).
    fn gate(&self, inner: &Inner) -> Option<u32> {
        inner.ends.iter().find(|e| e.id == self.id).and_then(|e| e.gate)
    }

    /// A FIFO's end: the node it was opened by.
    pub fn named(&self) -> Option<&crate::fifo::Named> {
        self.named.as_ref()
    }

    /// The end's description is installed (`reopen`, `open_fifo`): it gets
    /// reports from now on, and its readiness now (as an event: data may
    /// have come before its watch existed).
    pub fn installed(&self) {
        let mut inner = self.shared.inner.lock();
        let now = inner.readiness(self.reads, self.writes, self.gate(&inner));
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
        // The last reader or the last writer went (Linux's pipe_release): the readers
        // learn of the end of file, the writers of EPIPE.
        if (inner.readers == 0) != (inner.writers == 0) {
            Shared::sigio(&inner, true, files::Io::In);
            Shared::sigio(&inner, false, files::Io::Out);
        }
    }

    /// Reads into `dst`: what is there, waiting while the pipe is empty and
    /// has a writer. A FIFO's node was accessed if anything was read.
    pub fn read(&self, dst: Dst, nonblock: bool) -> Result<i64, i64> {
        let r = self.read_data(dst, nonblock);
        if let (Ok(1..), Some(named)) = (&r, &self.named) {
            named.used(false);
        }
        r
    }

    fn read_data(&self, dst: Dst, nonblock: bool) -> Result<i64, i64> {
        let mut sink = match dst {
            Dst::Program(vecs) => Sink::program(vecs),
            Dst::Server(buf) => Sink::Server { buf, at: 0 },
        };
        if sink.room() == 0 {
            return Ok(0);
        }
        let r = self.read_waiting(&mut sink, nonblock);
        self.shared.call_done(false);
        r
    }

    fn read_waiting(&self, sink: &mut Sink, nonblock: bool) -> Result<i64, i64> {
        let mut reader = Some(self.shared.rlock.lock()?);
        loop {
            let seen;
            {
                let inner = self.shared.inner.lock();
                if !inner.buf.is_empty() {
                    drop(inner);
                    let r = self.drain(sink);
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
            self.shared.call_done(false);
            self.shared.wait(seen)?;
            reader = Some(self.shared.rlock.lock()?);
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

    /// Writes all of `src`, waiting for room (with O_NONBLOCK what fits). A
    /// FIFO's node was modified if anything was written.
    pub fn write(&self, src: Src, nonblock: bool) -> Result<i64, i64> {
        let r = self.write_data(src, nonblock);
        if let (Ok(1..), Some(named)) = (&r, &self.named) {
            named.used(true);
        }
        r
    }

    fn write_data(&self, src: Src, nonblock: bool) -> Result<i64, i64> {
        let empty = match &src {
            Src::Program(vecs) => vecs.iter().all(|&(_, len)| len == 0),
            Src::Server(buf) => buf.is_empty(),
        };
        if empty {
            return Ok(0);
        }
        let r = self.write_waiting(src, nonblock);
        self.shared.call_done(true);
        r
    }

    fn write_waiting(&self, src: Src, nonblock: bool) -> Result<i64, i64> {
        let mut written: u64 = 0;
        let mut chunk = [0u8; CHUNK];
        let whole;
        let pieces: &[(u64, u64)] = match &src {
            Src::Program(vecs) => vecs,
            Src::Server(buf) => {
                whole = [(0, buf.len() as u64)];
                &whole
            }
        };
        for &(base, len) in pieces {
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
                            // No memory for the data: ENOMEM, as Linux's
                            // pipe_write when it gets no page (unless some
                            // was written).
                            if inner.buf.try_grow(room).is_err() {
                                return if written > 0 { Ok(written as i64) } else { Err(heap::ENOMEM) };
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
                    self.shared.call_done(true);
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
        usercopy::to_program(buf, &self.stat()?).map(|_| 0).map_err(|_| EFAULT)
    }

    /// Its `struct stat`: a FIFO of its own inode, or a FIFO's end its node's (live).
    pub fn stat(&self) -> Result<[u8; 144], i64> {
        match &self.named {
            Some(named) => named.origin().stat(),
            None => Ok(self.shared.stat()),
        }
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
