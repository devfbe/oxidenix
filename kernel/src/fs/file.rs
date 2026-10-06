use super::{Device, Inode};
use crate::process::errno::*;
use crate::process::signal::interrupted;
use crate::process::{sleep_on, wakeup};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;

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
    buf: Mutex<VecDeque<u8>>,
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

pub enum Kind {
    Inode(Arc<Inode>),
    PipeRead(Arc<Pipe>),
    PipeWrite(Arc<Pipe>),
}

/// Open file description; several descriptors may share it (dup, fork).
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
}

impl OpenFile {
    pub fn new(kind: Kind, flags: u32, path: Option<String>) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            kind,
            offset: Mutex::new(0),
            flags: AtomicU32::new(flags & !O_CLOEXEC),
            path,
            dir_snapshot: Mutex::new(None),
        })
    }

    pub fn console() -> Arc<OpenFile> {
        let inode = super::resolve("/", "/dev/console", true).expect("/dev/console missing");
        OpenFile::new(Kind::Inode(inode), O_RDWR, Some("/dev/console".into()))
    }

    pub fn pipe() -> Result<(Arc<OpenFile>, Arc<OpenFile>), i64> {
        crate::fs::charge(PIPE_RESERVED).map_err(|_| ENFILE)?;
        let pipe = Arc::new(Pipe {
            buf: Mutex::new(VecDeque::new()),
            readers: AtomicUsize::new(1),
            writers: AtomicUsize::new(1),
        });
        Ok((
            OpenFile::new(Kind::PipeRead(pipe.clone()), 0, None),
            OpenFile::new(Kind::PipeWrite(pipe), O_WRONLY, None),
        ))
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

    fn nonblocking(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & O_NONBLOCK != 0
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, i64> {
        match &self.kind {
            Kind::Inode(inode) => self.read_inode(inode, buf),
            Kind::PipeRead(pipe) => self.read_pipe(pipe, buf),
            Kind::PipeWrite(_) => Err(EBADF),
        }
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, i64> {
        match &self.kind {
            Kind::Inode(inode) => self.write_inode(inode, buf),
            Kind::PipeWrite(pipe) => self.write_pipe(pipe, buf),
            Kind::PipeRead(_) => Err(EBADF),
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
                let n = inode.read_at(*off, buf)?;
                *off += n as u64;
                Ok(n)
            }
        }
    }

    fn write_inode(&self, inode: &Inode, buf: &[u8]) -> Result<usize, i64> {
        match inode.device() {
            Some(Device::Console) => Ok(crate::drivers::tty::write(buf)),
            Some(_) => Ok(buf.len()),
            None => {
                let mut off = self.offset.lock();
                if self.flags.load(Ordering::Relaxed) & O_APPEND != 0 {
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
            sleep_on(pipe.read_chan());
        }
    }

    fn write_pipe(&self, pipe: &Arc<Pipe>, buf: &[u8]) -> Result<usize, i64> {
        let mut written = 0;
        while written < buf.len() {
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
            sleep_on(pipe.write_chan());
        }
        Ok(written)
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
        };
        ready & (events | POLLERR | POLLHUP)
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
            Kind::Inode(_) => {}
        }
    }
}
