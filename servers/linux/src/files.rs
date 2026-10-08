//! The server's files (phase R6): objects the server implements, each
//! named in the kernel's descriptor table by a placeholder (see
//! `restricted::SYS_KFD_INSTALL`). The table here maps the placeholder's id
//! to the object; an object goes when the kernel reports its placeholder's
//! last descriptor closed (`EVENT_CLOSED`, to the service thread).
//!
//! `handle` takes the system calls on descriptors and creates files (pipe,
//! pipe2): for a descriptor of one of the server's files it answers itself,
//! for one of the kernel's it returns None and the call passes through.

use crate::eventfd::EventFd;
use crate::pipe::{self, Dst, PipeEnd, Src};
use crate::sync::Mutex;
use crate::syscall;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use restricted::*;

pub const EBADF: i64 = 9;
pub const EINVAL: i64 = 22;
pub const ESPIPE: i64 = 29;
pub const ENOTTY: i64 = 25;
pub const EFAULT: i64 = 14;

pub const O_ACCMODE: u32 = 0o3;
pub const O_WRONLY: u32 = 0o1;
pub const O_RDWR: u32 = 0o2;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_CLOEXEC: u32 = 0o2000000;
const O_DIRECT: u32 = 0o40000;

/// What a placeholder names.
#[derive(Clone)]
pub enum File {
    Pipe(Arc<PipeEnd>),
    EventFd(Arc<EventFd>),
}

static FILES: Mutex<BTreeMap<u64, File>> = Mutex::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn new_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Registers `file` under `id` and gives it a descriptor (the lowest free
/// one). On failure the file is forgotten again.
pub fn install(id: u64, file: File, flags: u32, ready: i16) -> Result<i64, i64> {
    FILES.lock().insert(id, file);
    let fd = syscall(SYS_KFD_INSTALL, [id, flags as u64, ready as u16 as u64, 0, 0, 0]);
    if fd < 0 {
        FILES.lock().remove(&id);
        return Err(-fd);
    }
    Ok(fd)
}

/// Reports file `id`'s readiness for poll, select and epoll.
pub fn ready(id: u64, ready: i16) {
    syscall(SYS_KFD_READY, [id, ready as u16 as u64, 0, 0, 0, 0]);
}

/// The kernel closed the placeholder's last descriptor: the object goes.
pub fn closed(id: u64) {
    let gone = FILES.lock().remove(&id);
    if let Some(File::Pipe(end)) = gone {
        end.close();
    }
    // An eventfd simply goes.
}

/// Whether descriptor `fd` names one of the server's files.
pub fn is_server_file(fd: u64) -> bool {
    lookup(fd).is_some()
}

/// The server's file behind descriptor `fd` and its open flags, or None
/// for a file of the kernel's (or a bad descriptor: the kernel answers).
fn lookup(fd: u64) -> Option<(File, u32)> {
    let mut flags = 0u32;
    let id = syscall(SYS_KFD_LOOKUP, [fd, &mut flags as *mut u32 as u64, 0, 0, 0, 0]);
    if id <= 0 {
        return None;
    }
    FILES.lock().get(&(id as u64)).cloned().map(|f| (f, flags))
}

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_FSTAT: u64 = 5;
const SYS_LSEEK: u64 = 8;
const SYS_IOCTL: u64 = 16;
const SYS_SENDFILE: u64 = 40;
const SYS_PREAD64: u64 = 17;
const SYS_PWRITE64: u64 = 18;
const SYS_READV: u64 = 19;
const SYS_WRITEV: u64 = 20;
const SYS_PIPE: u64 = 22;
const SYS_FSYNC: u64 = 74;
const SYS_FDATASYNC: u64 = 75;
const SYS_FTRUNCATE: u64 = 77;
const SYS_PIPE2: u64 = 293;
const SYS_EVENTFD: u64 = 284;
const SYS_EVENTFD2: u64 = 290;
const SYS_PREADV: u64 = 295;
const SYS_PWRITEV: u64 = 296;

/// The result of a file system call in `s` the server handles, or None.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2) = (s.rdi, s.rsi, s.rdx);
    let result = match s.rax {
        SYS_PIPE => pipe2(a0, 0),
        SYS_EVENTFD => eventfd2(a0, 0),
        SYS_EVENTFD2 => eventfd2(a0, a1),
        SYS_SENDFILE => {
            let (out, input) = (lookup(a0), lookup(a1));
            if out.is_none() && input.is_none() {
                return None;
            }
            sendfile(a0, out, a1, input, a2, s.r10)
        }
        SYS_PIPE2 => pipe2(a0, a1),
        SYS_READ | SYS_WRITE | SYS_READV | SYS_WRITEV | SYS_FSTAT | SYS_LSEEK | SYS_IOCTL | SYS_PREAD64 | SYS_PWRITE64
        | SYS_PREADV | SYS_PWRITEV | SYS_FSYNC | SYS_FDATASYNC | SYS_FTRUNCATE => {
            let (file, flags) = lookup(a0)?;
            on_file(s.rax, file, flags, a1, a2)
        }
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

fn on_file(nr: u64, file: File, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    let end = match file {
        File::Pipe(end) => end,
        File::EventFd(e) => return on_eventfd(nr, &e, flags, a1, a2),
    };
    let readable = flags & O_ACCMODE != O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        SYS_READ if !readable => Err(EBADF),
        SYS_WRITE if !writable => Err(EBADF),
        SYS_READ => end.read(Dst::Program(&[(a1, a2)]), nonblock),
        SYS_WRITE => end.write(Src::Program(&[(a1, a2)]), nonblock),
        SYS_READV | SYS_WRITEV => {
            let vecs = iovecs(a1, a2)?;
            match nr {
                SYS_READV if !readable => Err(EBADF),
                SYS_WRITEV if !writable => Err(EBADF),
                SYS_READV => end.read(Dst::Program(&vecs), nonblock),
                _ => end.write(Src::Program(&vecs), nonblock),
            }
        }
        SYS_FSTAT => end.fstat(a1),
        SYS_LSEEK | SYS_PREAD64 | SYS_PWRITE64 | SYS_PREADV | SYS_PWRITEV => Err(ESPIPE),
        SYS_IOCTL => Err(ENOTTY),
        _ => Err(EINVAL),
    }
}

/// An iovec array of the program: (base, length) pairs.
fn iovecs(iov: u64, count: u64) -> Result<alloc::vec::Vec<(u64, u64)>, i64> {
    if count > 1024 {
        return Err(EINVAL);
    }
    let mut out = alloc::vec::Vec::new();
    for i in 0..count {
        let pair: [u64; 2] = crate::usercopy::read(iov + i * 16)?;
        out.push((pair[0], pair[1]));
    }
    Ok(out)
}

/// pipe2(fds, flags): a pipe; its read and write descriptors at `fds`.
fn pipe2(fds: u64, flags: u64) -> Result<i64, i64> {
    let flags = flags as u32;
    if flags & !(O_CLOEXEC | O_NONBLOCK | O_DIRECT) != 0 {
        return Err(EINVAL);
    }
    let (read_end, write_end) = pipe::new();
    let (rid, wid) = (read_end.id(), write_end.id());
    let common = flags & (O_CLOEXEC | O_NONBLOCK);
    let rfd = install(rid, File::Pipe(read_end.clone()), common, read_end.readiness())?;
    let wfd = match install(wid, File::Pipe(write_end.clone()), common | O_WRONLY, write_end.readiness()) {
        Ok(fd) => fd,
        Err(e) => {
            syscall(SYS_KFD_CLOSE, [rfd as u64, 0, 0, 0, 0, 0]);
            return Err(e);
        }
    };
    if let Err(e) = crate::usercopy::write(fds, &[rfd as i32, wfd as i32]) {
        syscall(SYS_KFD_CLOSE, [rfd as u64, 0, 0, 0, 0, 0]);
        syscall(SYS_KFD_CLOSE, [wfd as u64, 0, 0, 0, 0, 0]);
        return Err(e);
    }
    Ok(0)
}

/// sendfile(out, in, offset, count) with a server file at either end (the
/// kernel does it between its own files): chunks through the server's
/// memory, returning after a short read, as Linux and the kernel do.
fn sendfile(out_fd: u64, out: Option<(File, u32)>, in_fd: u64, input: Option<(File, u32)>, offset: u64, count: u64) -> Result<i64, i64> {
    if offset != 0 {
        return Err(EINVAL);
    }
    // An eventfd moves 8-byte values, not data.
    if matches!(out, Some((File::EventFd(_), _))) || matches!(input, Some((File::EventFd(_), _))) {
        return Err(EINVAL);
    }
    if let Some((_, flags)) = &out {
        if flags & O_ACCMODE == 0 {
            return Err(EBADF);
        }
    }
    if let Some((_, flags)) = &input {
        if flags & O_ACCMODE == O_WRONLY {
            return Err(EBADF);
        }
    }
    let mut buf = alloc::vec![0u8; 4096];
    let mut total = 0u64;
    while total < count {
        let want = (count - total).min(buf.len() as u64) as usize;
        let n = match &input {
            Some((File::Pipe(end), flags)) => end.read(Dst::Server(&mut buf[..want]), flags & O_NONBLOCK != 0),
            Some((File::EventFd(_), _)) => Err(EINVAL),
            None => match syscall(SYS_KFD_READ, [in_fd, buf.as_mut_ptr() as u64, want as u64, 0, 0, 0]) {
                r if r < 0 => Err(-r),
                r => Ok(r),
            },
        };
        let n = match n {
            Ok(n) => n as usize,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let wrote = match &out {
            Some((File::Pipe(end), flags)) => end.write(Src::Server(&buf[..n]), flags & O_NONBLOCK != 0),
            Some((File::EventFd(_), _)) => Err(EINVAL),
            None => match syscall(SYS_KFD_WRITE, [out_fd, buf.as_ptr() as u64, n as u64, 0, 0, 0]) {
                r if r < 0 => Err(-r),
                r => Ok(r),
            },
        };
        match wrote {
            Ok(_) => total += n as u64,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        }
        if n < want {
            break;
        }
    }
    Ok(total as i64)
}

fn on_eventfd(nr: u64, e: &EventFd, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        SYS_READ => e.read(a1, a2, nonblock),
        SYS_WRITE => e.write(a1, a2, nonblock),
        // One value per buffer, as reads and writes one after the other.
        SYS_READV | SYS_WRITEV => {
            let mut done = 0;
            for (base, len) in iovecs(a1, a2)? {
                let r = if nr == SYS_READV { e.read(base, len, nonblock) } else { e.write(base, len, nonblock) };
                match r {
                    Ok(n) => done += n,
                    Err(err) if done == 0 => return Err(err),
                    Err(_) => break,
                }
            }
            Ok(done)
        }
        SYS_FSTAT => e.fstat(a1),
        SYS_LSEEK | SYS_PREAD64 | SYS_PWRITE64 | SYS_PREADV | SYS_PWRITEV => Err(ESPIPE),
        SYS_IOCTL => Err(ENOTTY),
        _ => Err(EINVAL),
    }
}

/// eventfd2(initval, flags): a counter starting at `initval` (32 bits).
fn eventfd2(initval: u64, flags: u64) -> Result<i64, i64> {
    const EFD_SEMAPHORE: u64 = 1;
    if flags & !(EFD_SEMAPHORE | (O_NONBLOCK | O_CLOEXEC) as u64) != 0 {
        return Err(EINVAL);
    }
    let e = Arc::new(EventFd::new(initval as u32 as u64, flags & EFD_SEMAPHORE != 0));
    let open = O_RDWR | (flags as u32 & (O_NONBLOCK | O_CLOEXEC));
    install(e.id(), File::EventFd(e.clone()), open, e.ready())
}
