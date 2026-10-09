//! Open files: `OpenFile` (an open file description with offset and flags) over inodes, pipes,
//! eventfds and devices. A Linux program's descriptors are its Linux server's (phase R6e):
//! the open inodes of the kernel's tree it uses are open files the server holds by handle
//! (`restricted::SYS_KFILE_CALL`); the kernel's own descriptor tables serve the native servers.

use super::{Device, Inode};
use crate::process::errno::*;
use crate::process::signal::interrupted;
use crate::process::{sched::prepare_to_wait, wakeup};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::Arc;
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

pub enum Kind {
    Inode(Arc<Inode>),
    PipeRead(Arc<Pipe>),
    PipeWrite(Arc<Pipe>),
    EventFd(Arc<EventFd>),
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
    /// A regular file opened for writing holds the right to write it.
    _write_access: Option<super::WriteAccess>,
    /// Its number, the inode number fstat reports for a file without an
    /// inode (a pipe, an eventfd): opaque, never an address of the kernel's.
    pub number: u64,
}

/// The next `OpenFile::number`.
static NEXT_NUMBER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

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
            _write_access: write_access,
            number: NEXT_NUMBER.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
        })
    }

    /// An open inode; `write_access` for a regular file opened for
    /// writing (see `Inode::get_write_access`).
    pub fn inode_file(inode: Arc<Inode>, flags: u32, path: String, write_access: Option<super::WriteAccess>) -> Arc<OpenFile> {
        Self::with_access(Kind::Inode(inode), flags, Some(path), write_access)
    }

    /// The kernel's console output, the native servers' standard output and
    /// error (write only; not in /dev).
    pub fn console() -> Arc<OpenFile> {
        static CONSOLE: spin::Once<Arc<Inode>> = spin::Once::new();
        let inode = CONSOLE.call_once(|| Inode::new(super::Node::Device(Device::Console), 0o600).expect("file quota exhausted"));
        OpenFile::new(Kind::Inode(inode.clone()), O_WRONLY, Some("/dev/console".into()))
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
        }
    }

    fn read_inode(&self, inode: &Inode, buf: &mut [u8]) -> Result<usize, i64> {
        match inode.device() {
            // Output only (and never opened for reading).
            Some(Device::Console) => Err(EBADF),
            Some(Device::Server(..)) => Err(ENXIO),
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
            Some(Device::Console) => {
                crate::drivers::console::write_text(buf);
                Ok(buf.len())
            }
            Some(Device::Server(..)) => Err(ENXIO),
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

impl Drop for OpenFile {
    fn drop(&mut self) {
        match &self.kind {
            Kind::PipeRead(p) => {
                p.readers.fetch_sub(1, Ordering::Relaxed);
                wakeup(p.write_chan());
            }
            Kind::PipeWrite(p) => {
                p.writers.fetch_sub(1, Ordering::Relaxed);
                wakeup(p.read_chan());
            }
            Kind::Inode(_) | Kind::EventFd(_) => {}
        }
    }
}
