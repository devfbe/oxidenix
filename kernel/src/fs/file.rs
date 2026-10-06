use super::{Device, Inode, Node};
use crate::process::errno::*;
use crate::process::{sleep_on, wakeup};
use alloc::collections::VecDeque;
use alloc::string::String;
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

pub struct Pipe {
    buf: Mutex<VecDeque<u8>>,
    readers: AtomicUsize,
    writers: AtomicUsize,
}

impl Pipe {
    fn read_chan(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }

    fn write_chan(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize + 1
    }
}

pub enum Kind {
    Inode(Arc<Inode>),
    PipeRead(Arc<Pipe>),
    PipeWrite(Arc<Pipe>),
}

/// Geoeffnete Datei; mehrere Deskriptoren koennen sie teilen (dup, fork).
pub struct OpenFile {
    pub kind: Kind,
    pub offset: Mutex<u64>,
    pub flags: AtomicU32,
    /// Absoluter Pfad, falls ueber einen Pfad geoeffnet (fuer *at-Syscalls).
    pub path: Option<String>,
}

impl OpenFile {
    pub fn new(kind: Kind, flags: u32, path: Option<String>) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            kind,
            offset: Mutex::new(0),
            flags: AtomicU32::new(flags & !O_CLOEXEC),
            path,
        })
    }

    pub fn console() -> Arc<OpenFile> {
        let inode = super::resolve("/", "/dev/console", true).expect("/dev/console fehlt");
        OpenFile::new(Kind::Inode(inode), O_RDWR, Some("/dev/console".into()))
    }

    pub fn pipe() -> (Arc<OpenFile>, Arc<OpenFile>) {
        let pipe = Arc::new(Pipe {
            buf: Mutex::new(VecDeque::new()),
            readers: AtomicUsize::new(1),
            writers: AtomicUsize::new(1),
        });
        (
            OpenFile::new(Kind::PipeRead(pipe.clone()), 0, None),
            OpenFile::new(Kind::PipeWrite(pipe), O_WRONLY, None),
        )
    }

    pub fn inode(&self) -> Option<&Arc<Inode>> {
        match &self.kind {
            Kind::Inode(i) => Some(i),
            _ => None,
        }
    }

    pub fn is_console(&self) -> bool {
        self.inode()
            .is_some_and(|i| matches!(&*i.node.lock(), Node::Device(Device::Console)))
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
        match &*inode.node.lock() {
            Node::Dir(_) => Err(EISDIR),
            Node::Symlink(_) => Err(EINVAL),
            Node::Device(Device::Null) => Ok(0),
            Node::Device(Device::Zero) => {
                buf.fill(0);
                Ok(buf.len())
            }
            // Tastatureingabe fuer Prozesse gibt es noch nicht.
            Node::Device(Device::Console) => Ok(0),
            Node::File(data) => {
                let mut off = self.offset.lock();
                let bytes = data.bytes();
                let start = (*off as usize).min(bytes.len());
                let n = buf.len().min(bytes.len() - start);
                buf[..n].copy_from_slice(&bytes[start..start + n]);
                *off += n as u64;
                Ok(n)
            }
        }
    }

    fn write_inode(&self, inode: &Inode, buf: &[u8]) -> Result<usize, i64> {
        match &mut *inode.node.lock() {
            Node::Dir(_) => Err(EISDIR),
            Node::Symlink(_) => Err(EINVAL),
            Node::Device(Device::Console) => {
                crate::drivers::console::write_bytes(buf);
                Ok(buf.len())
            }
            Node::Device(_) => Ok(buf.len()),
            Node::File(data) => {
                let v = data.make_mut();
                let mut off = self.offset.lock();
                if self.flags.load(Ordering::Relaxed) & O_APPEND != 0 {
                    *off = v.len() as u64;
                }
                let start = *off as usize;
                if v.len() < start + buf.len() {
                    v.resize(start + buf.len(), 0);
                }
                v[start..start + buf.len()].copy_from_slice(buf);
                *off += buf.len() as u64;
                Ok(buf.len())
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
                    for (dst, src) in buf.iter_mut().zip(q.drain(..n)) {
                        *dst = src;
                    }
                    drop(q);
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
            sleep_on(pipe.read_chan());
        }
    }

    fn write_pipe(&self, pipe: &Arc<Pipe>, buf: &[u8]) -> Result<usize, i64> {
        let mut written = 0;
        while written < buf.len() {
            if pipe.readers.load(Ordering::Relaxed) == 0 {
                return if written > 0 { Ok(written) } else { Err(EPIPE) };
            }
            let pushed = {
                let mut q = pipe.buf.lock();
                let n = (PIPE_CAPACITY - q.len()).min(buf.len() - written);
                q.extend(&buf[written..written + n]);
                n
            };
            if pushed > 0 {
                written += pushed;
                wakeup(pipe.read_chan());
                continue;
            }
            if self.nonblocking() {
                return if written > 0 { Ok(written) } else { Err(EAGAIN) };
            }
            sleep_on(pipe.write_chan());
        }
        Ok(written)
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
