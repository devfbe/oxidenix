//! Open files: `OpenFile` (an open file description with offset and flags) over inodes, pipes,
//! eventfds, devices and files whose calls a server implements (`ServerFile`).

use super::{Device, Inode};
use crate::process::errno::*;
use crate::process::epoll::{self, Epoll};
use crate::process::poll::PollSource;
use crate::process::signal::interrupted;
use crate::process::{sched::prepare_to_wait, wakeup};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use crate::sync::Mutex;

pub const O_ACCMODE: u32 = 0o3;
pub const O_WRONLY: u32 = 0o1;
pub const O_RDWR: u32 = 0o2;
pub const O_CREAT: u32 = 0o100;
pub const O_EXCL: u32 = 0o200;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_DIRECTORY: u32 = 0o200000;
pub const O_NOFOLLOW: u32 = 0o400000;
pub const O_CLOEXEC: u32 = 0o2000000;

const PIPE_CAPACITY: usize = 64 * 1024;
/// Buffer space charged when a pipe is created. An empty pipe can therefore
/// always accept data; only growth beyond it depends on the quota, so a
/// full quota can slow pipes down but never deadlock a pipeline.
const PIPE_RESERVED: usize = 16 * 1024;

pub struct Pipe {
    buf: spin::Mutex<VecDeque<u8>>,
    readers: AtomicUsize,
    writers: AtomicUsize,
}

/// Quota charged for `len` buffered bytes beyond the reservation.
fn extra(len: usize) -> usize {
    len.saturating_sub(PIPE_RESERVED)
}

impl Pipe {
    fn read_chan(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }

    fn write_chan(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize + 1
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        crate::fs::release(PIPE_RESERVED + extra(self.buf.lock().len()));
    }
}

/// An eventfd: a counter that reads take and writes add to. Readers and
/// writers wait on one channel; every change wakes it.
pub struct EventFd {
    count: spin::Mutex<u64>,
    /// EFD_SEMAPHORE: a read takes 1, not everything.
    semaphore: bool,
}

/// The largest count; writing would overflow beyond it.
const EVENTFD_MAX: u64 = u64::MAX - 1;

impl EventFd {
    fn chan(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }
}

/// The owner of placeholder files (a Linux server instance): told when
/// the last descriptor of one goes.
pub trait ServerFiles: Send + Sync {
    fn closed(&self, id: u64);
    /// A descriptor or pin of a placeholder with references in flight
    /// went: sockets may have become reachable only from messages in
    /// flight (the server's collector looks).
    fn in_flight_reference_gone(&self);
}

/// A file the Linux server implements, as the kernel's descriptor table
/// holds it (a placeholder, see docs/design/linux-server.md, R6): the id of
/// the server's object and the readiness the server reports, for poll,
/// select and epoll. The server handles every other operation itself.
pub struct ServerFile {
    pub id: u64,
    owner: Weak<dyn ServerFiles>,
    ready: AtomicU32,
    /// Always ready (a regular file or a directory of the server's).
    always: bool,
}

impl ServerFile {
    pub fn new(id: u64, owner: Weak<dyn ServerFiles>, ready: i16, always: bool) -> Arc<ServerFile> {
        Arc::new(ServerFile { id, owner, ready: AtomicU32::new(ready as u16 as u32), always })
    }

    /// Whether `owner` (an instance of the Linux server) made this file.
    pub fn owned_by(&self, owner: *const ()) -> bool {
        self.owner.as_ptr() as *const () == owner
    }

    /// See `ServerFiles::in_flight_reference_gone`.
    fn in_flight_reference_gone(&self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.in_flight_reference_gone();
        }
    }

    /// The server's readiness report: wakes who polls the file.
    pub fn set_ready(&self, ready: i16) {
        self.ready.store(ready as u16 as u32, Ordering::Release);
        wakeup(self.chan());
    }

    fn ready(&self) -> i16 {
        self.ready.load(Ordering::Acquire) as u16 as i16
    }

    fn chan(&self) -> usize {
        self as *const ServerFile as usize
    }
}

impl Drop for ServerFile {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.closed(self.id);
        }
    }
}

pub enum Kind {
    Inode(Arc<Inode>),
    PipeRead(Arc<Pipe>),
    PipeWrite(Arc<Pipe>),
    EventFd(Arc<EventFd>),
    Epoll(Arc<Epoll>),
    /// A file of the Linux server (a placeholder).
    Server(Arc<ServerFile>),
}

/// Open file description; several descriptors may share it (dup, fork,
/// threads). Its locks sleep: a read holds the offset while the file
/// server answers.
pub struct OpenFile {
    pub kind: Kind,
    pub offset: Mutex<u64>,
    pub flags: AtomicU32,
    /// Absolute path if opened by path (for *at syscalls).
    pub path: Option<String>,
    /// Directory listing taken when reading starts at offset 0; getdents
    /// continues from it, so entries removed meanwhile (rm -r) never shift
    /// the position and skip others.
    pub dir_snapshot: Mutex<Option<Vec<(String, u64, u8)>>>,
    /// The epoll interests in this file, removed when it is closed.
    pub watchers: spin::Mutex<Vec<Weak<epoll::Item>>>,
    /// A regular file opened for writing holds the right to write it.
    _write_access: Option<super::WriteAccess>,
    /// The Linux server's handles on it that are descriptors in flight
    /// (`restricted::KFILE_INFLIGHT`).
    pub in_flight: AtomicUsize,
    /// References being let go of through `release` right now.
    releasing: AtomicUsize,
    /// Its number, the inode number fstat reports for a file without an
    /// inode (a socket, a pipe, an epoll instance): opaque, never an
    /// address of the kernel's.
    pub number: u64,
}

/// The next `OpenFile::number`.
static NEXT_NUMBER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Lets go of a descriptor's (or a pin's) reference to `file`. If it is
/// one of the Linux server's placeholders with descriptors in flight and,
/// with the reference gone, nothing but those keeps it (a candidate for the
/// server's collector, as Linux's unix_gc looks for files whose only
/// references are in flight), its owner is told: after the reference went,
/// so that the collector sees it gone. References other `release`s are
/// letting go of count as gone (`releasing`), so of concurrent ones at
/// least one tells; one told for nothing costs a collection.
pub fn release(file: Arc<OpenFile>) {
    if file.in_flight.load(Ordering::Acquire) == 0 || !matches!(file.kind, Kind::Server(_)) {
        return;
    }
    file.releasing.fetch_add(1, Ordering::AcqRel);
    let weak = Arc::downgrade(&file);
    drop(file);
    // Gone altogether: its owner hears of it as closed.
    let Some(file) = weak.upgrade() else { return };
    let others = Arc::strong_count(&file).saturating_sub(file.releasing.load(Ordering::Acquire));
    let candidate = others <= file.in_flight.load(Ordering::Acquire);
    file.releasing.fetch_sub(1, Ordering::AcqRel);
    if candidate {
        if let Kind::Server(s) = &file.kind {
            s.in_flight_reference_gone();
        }
    }
}

impl OpenFile {
    pub fn new(kind: Kind, flags: u32, path: Option<String>) -> Arc<OpenFile> {
        Self::with_access(kind, flags, path, None)
    }

    fn with_access(kind: Kind, flags: u32, path: Option<String>, write_access: Option<super::WriteAccess>) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            kind,
            offset: Mutex::new(0),
            flags: AtomicU32::new(flags & !O_CLOEXEC),
            path,
            dir_snapshot: Mutex::new(None),
            watchers: spin::Mutex::new(Vec::new()),
            _write_access: write_access,
            in_flight: AtomicUsize::new(0),
            releasing: AtomicUsize::new(0),
            number: NEXT_NUMBER.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
        })
    }

    /// An open inode; `write_access` for a regular file opened for
    /// writing (see `Inode::get_write_access`).
    pub fn inode_file(inode: Arc<Inode>, flags: u32, path: String, write_access: Option<super::WriteAccess>) -> Arc<OpenFile> {
        Self::with_access(Kind::Inode(inode), flags, Some(path), write_access)
    }

    pub fn console() -> Arc<OpenFile> {
        let inode = super::resolve("/", "/dev/console", true).expect("/dev/console missing");
        OpenFile::new(Kind::Inode(inode), O_RDWR, Some("/dev/console".into()))
    }

    pub fn pipe() -> Result<(Arc<OpenFile>, Arc<OpenFile>), i64> {
        crate::fs::charge(PIPE_RESERVED).map_err(|_| ENFILE)?;
        let pipe = Arc::new(Pipe {
            buf: spin::Mutex::new(VecDeque::new()),
            readers: AtomicUsize::new(1),
            writers: AtomicUsize::new(1),
        });
        Ok((
            OpenFile::new(Kind::PipeRead(pipe.clone()), 0, None),
            OpenFile::new(Kind::PipeWrite(pipe), O_WRONLY, None),
        ))
    }

    pub fn eventfd(initial: u64, semaphore: bool, flags: u32) -> Arc<OpenFile> {
        let counter = Arc::new(EventFd { count: spin::Mutex::new(initial), semaphore });
        OpenFile::new(Kind::EventFd(counter), O_RDWR | flags, None)
    }

    pub fn inode(&self) -> Option<&Arc<Inode>> {
        match &self.kind {
            Kind::Inode(i) => Some(i),
            _ => None,
        }
    }

    pub fn is_console(&self) -> bool {
        self.inode().is_some_and(|i| i.device() == Some(Device::Console))
    }

    pub fn readable(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & O_ACCMODE != O_WRONLY
    }

    pub fn writable(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & O_ACCMODE != 0
    }

    /// Positional read for pread64: no offset change, regular inodes only.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        if !self.readable() {
            return Err(EBADF);
        }
        match self.inode() {
            Some(inode) if inode.device().is_none() => self.inode_read(inode, off, buf),
            _ => Err(ESPIPE),
        }
    }

    /// Positional write for pwrite64: no offset change, regular inodes only.
    pub fn write_at(&self, off: u64, buf: &[u8]) -> Result<usize, i64> {
        if !self.writable() {
            return Err(EBADF);
        }
        match self.inode() {
            Some(inode) if inode.device().is_none() => inode.write_at(off, buf),
            _ => Err(ESPIPE),
        }
    }

    pub fn nonblocking(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & O_NONBLOCK != 0
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, i64> {
        if !self.readable() {
            return Err(EBADF);
        }
        match &self.kind {
            Kind::Inode(inode) => self.read_inode(inode, buf),
            Kind::PipeRead(pipe) => self.read_pipe(pipe, buf),
            Kind::PipeWrite(_) => Err(EBADF),
            Kind::EventFd(e) => self.read_eventfd(e, buf),
            // The server reads its files itself.
            Kind::Epoll(_) | Kind::Server(_) => Err(EINVAL),
        }
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, i64> {
        self.write_appending(buf, self.appends())
    }

    /// Whether writes go to the end (O_APPEND).
    pub fn appends(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & O_APPEND != 0
    }

    /// Positional write to the end of a regular file (pwrite on an O_APPEND
    /// descriptor, pwritev2's RWF_APPEND): no offset change.
    pub fn write_end(&self, buf: &[u8]) -> Result<usize, i64> {
        if !self.writable() {
            return Err(EBADF);
        }
        match self.inode() {
            Some(inode) if inode.device().is_none() => inode.write_at(inode.size(), buf),
            _ => Err(ESPIPE),
        }
    }

    /// write at the offset, which moves; to the end first if `append`
    /// (O_APPEND, or pwritev2's flags at the offset -1).
    pub fn write_appending(&self, buf: &[u8], append: bool) -> Result<usize, i64> {
        if !self.writable() {
            return Err(EBADF);
        }
        match &self.kind {
            Kind::Inode(inode) => self.write_inode(inode, buf, append),
            Kind::PipeWrite(pipe) => self.write_pipe(pipe, buf),
            Kind::PipeRead(_) => Err(EBADF),
            Kind::EventFd(e) => self.write_eventfd(e, buf),
            Kind::Epoll(_) | Kind::Server(_) => Err(EINVAL),
        }
    }

    fn read_inode(&self, inode: &Inode, buf: &mut [u8]) -> Result<usize, i64> {
        match inode.device() {
            // The TTY may sleep, so no inode lock may be held here.
            Some(Device::Console) => crate::drivers::tty::read(buf, self.nonblocking()),
            Some(Device::Null) => Ok(0),
            Some(Device::Zero) => {
                buf.fill(0);
                Ok(buf.len())
            }
            None => {
                let mut off = self.offset.lock();
                let n = self.inode_read(inode, *off, buf)?;
                *off += n as u64;
                Ok(n)
            }
        }
    }

    /// Reads file contents (the kernel's files are memory or generated:
    /// O_DIRECT reads them as any read does).
    fn inode_read(&self, inode: &Inode, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        inode.read_at(off, buf)
    }

    fn write_inode(&self, inode: &Inode, buf: &[u8], append: bool) -> Result<usize, i64> {
        match inode.device() {
            Some(Device::Console) => Ok(crate::drivers::tty::write(buf)),
            Some(_) => Ok(buf.len()),
            None => {
                let mut off = self.offset.lock();
                if append {
                    *off = inode.size();
                }
                let n = inode.write_at(*off, buf)?;
                *off += n as u64;
                Ok(n)
            }
        }
    }

    fn read_pipe(&self, pipe: &Arc<Pipe>, buf: &mut [u8]) -> Result<usize, i64> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let wait = prepare_to_wait(pipe.read_chan());
            {
                let mut q = pipe.buf.lock();
                if !q.is_empty() {
                    let n = buf.len().min(q.len());
                    let before = extra(q.len());
                    for (dst, src) in buf.iter_mut().zip(q.drain(..n)) {
                        *dst = src;
                    }
                    let after = extra(q.len());
                    drop(q);
                    crate::fs::release(before - after);
                    wakeup(pipe.write_chan());
                    return Ok(n);
                }
            }
            if pipe.writers.load(Ordering::Relaxed) == 0 {
                return Ok(0);
            }
            if self.nonblocking() {
                return Err(EAGAIN);
            }
            if interrupted() {
                return Err(EINTR);
            }
            wait.sleep();
        }
    }

    fn write_pipe(&self, pipe: &Arc<Pipe>, buf: &[u8]) -> Result<usize, i64> {
        let mut written = 0;
        while written < buf.len() {
            let wait = prepare_to_wait(pipe.write_chan());
            if pipe.readers.load(Ordering::Relaxed) == 0 {
                return if written > 0 { Ok(written) } else { Err(EPIPE) };
            }
            // Buffered bytes count against the filesystem quota; when it is
            // exhausted (or the heap is), the pipe behaves as if it were full.
            let pushed = {
                let mut q = pipe.buf.lock();
                let mut n = (PIPE_CAPACITY - q.len()).min(buf.len() - written);
                let mut charged = extra(q.len() + n) - extra(q.len());
                if charged > 0 && crate::fs::charge(charged).is_err() {
                    // Quota exhausted: only use what the reservation covers.
                    n = PIPE_RESERVED.saturating_sub(q.len()).min(n);
                    charged = 0;
                }
                if n > 0 && q.try_reserve(n).is_ok() {
                    q.extend(&buf[written..written + n]);
                    n
                } else {
                    crate::fs::release(charged);
                    0
                }
            };
            if pushed > 0 {
                written += pushed;
                wakeup(pipe.read_chan());
                continue;
            }
            if self.nonblocking() {
                return if written > 0 { Ok(written) } else { Err(EAGAIN) };
            }
            if interrupted() {
                return if written > 0 { Ok(written) } else { Err(EINTR) };
            }
            wait.sleep();
        }
        Ok(written)
    }
}

impl OpenFile {
    /// Takes the count (or 1 of it, as a semaphore) into an 8-byte buffer;
    /// waits while it is 0.
    fn read_eventfd(&self, e: &Arc<EventFd>, buf: &mut [u8]) -> Result<usize, i64> {
        if buf.len() < 8 {
            return Err(EINVAL);
        }
        loop {
            let wait = prepare_to_wait(e.chan());
            let taken = {
                let mut count = e.count.lock();
                let n = if e.semaphore { (*count).min(1) } else { *count };
                *count -= n;
                n
            };
            if taken > 0 {
                drop(wait);
                wakeup(e.chan());
                buf[..8].copy_from_slice(&taken.to_ne_bytes());
                return Ok(8);
            }
            if self.nonblocking() {
                return Err(EAGAIN);
            }
            if interrupted() {
                return Err(EINTR);
            }
            wait.sleep();
        }
    }

    /// Adds an 8-byte value; waits while the count would exceed its maximum.
    fn write_eventfd(&self, e: &Arc<EventFd>, buf: &[u8]) -> Result<usize, i64> {
        let value = u64::from_ne_bytes(buf.get(..8).ok_or(EINVAL)?.try_into().map_err(|_| EINVAL)?);
        if value == u64::MAX {
            return Err(EINVAL);
        }
        loop {
            let wait = prepare_to_wait(e.chan());
            let added = {
                let mut count = e.count.lock();
                let fits = EVENTFD_MAX - *count >= value;
                if fits {
                    *count += value;
                }
                fits
            };
            if added {
                drop(wait);
                wakeup(e.chan());
                return Ok(8);
            }
            if self.nonblocking() {
                return Err(EAGAIN);
            }
            if interrupted() {
                return Err(EINTR);
            }
            wait.sleep();
        }
    }
}

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;

impl OpenFile {
    /// Ready events among `events` (plus POLLERR/POLLHUP, which are always reported).
    pub fn poll(&self, events: i16) -> i16 {
        let ready = match &self.kind {
            Kind::Inode(_) if self.is_console() => {
                POLLOUT | if crate::drivers::tty::readable() { POLLIN } else { 0 }
            }
            Kind::Inode(_) => POLLIN | POLLOUT,
            Kind::PipeRead(p) => {
                let hup = p.writers.load(Ordering::Relaxed) == 0;
                (if hup || !p.buf.lock().is_empty() { POLLIN } else { 0 }) | if hup { POLLHUP } else { 0 }
            }
            Kind::PipeWrite(p) => {
                if p.readers.load(Ordering::Relaxed) == 0 {
                    POLLERR
                } else if p.buf.lock().len() < PIPE_CAPACITY {
                    POLLOUT
                } else {
                    0
                }
            }
            Kind::EventFd(e) => {
                let count = *e.count.lock();
                (if count > 0 { POLLIN } else { 0 }) | if count < EVENTFD_MAX { POLLOUT } else { 0 }
            }
            Kind::Epoll(e) => {
                if e.has_events() {
                    POLLIN
                } else {
                    0
                }
            }
            Kind::Server(s) => s.ready(),
        };
        ready & (events | POLLERR | POLLHUP)
    }

    /// Where this file announces changes of its readiness.
    pub fn poll_source(&self) -> PollSource {
        match &self.kind {
            Kind::Inode(_) if self.is_console() => PollSource::Chan(crate::drivers::tty::POLL_CHAN),
            // Regular files and devices are always ready.
            Kind::Inode(_) => PollSource::Always,
            Kind::PipeRead(p) => PollSource::Chan(p.read_chan()),
            Kind::PipeWrite(p) => PollSource::Chan(p.write_chan()),
            Kind::EventFd(e) => PollSource::Chan(e.chan()),
            Kind::Epoll(e) => PollSource::Epoll(e.clone()),
            Kind::Server(s) if s.always => PollSource::Always,
            Kind::Server(s) => PollSource::Chan(s.chan()),
        }
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        // The epoll instances watching this file forget it.
        let watchers = core::mem::take(&mut *self.watchers.lock());
        for item in watchers.iter().filter_map(Weak::upgrade) {
            if let Some(epoll) = item.owner() {
                epoll.file_closed(&item);
            }
        }
        match &self.kind {
            Kind::PipeRead(p) => {
                p.readers.fetch_sub(1, Ordering::Relaxed);
                wakeup(p.write_chan());
            }
            Kind::PipeWrite(p) => {
                p.writers.fetch_sub(1, Ordering::Relaxed);
                wakeup(p.read_chan());
            }
            // A server file tells its owner as its last reference goes.
            Kind::Inode(_) | Kind::EventFd(_) | Kind::Epoll(_) | Kind::Server(_) => {}
        }
    }
}
