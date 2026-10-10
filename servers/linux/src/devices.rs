//! Null and zero (phase R6d; /dev's since R9): a character device node names its driver
//! by its number wherever it is (ADR 0007), so a node (1,3) or (1,5) in /dev, on the
//! server's tmpfs or /data is null or zero, served here. The open file description keeps
//! the node it was opened by (`Origin`): fstat is the node's, live, and the calls that
//! change the node (fchmod, fchown, futimens) change it.
//!
//! Reads and writes check their buffers as Linux's (`rw_verify_area`, `import_iovec`):
//! a count above `isize::MAX` is EINVAL, a range beyond the program's memory EFAULT, and
//! a call moves at most `MAX_RW_COUNT` bytes. A read of zero fills the whole count, and
//! returns what it filled when a signal comes.

use crate::files::{self, File, EINVAL, O_ACCMODE, O_CLOEXEC, O_NONBLOCK};
use crate::namespace::Origin;
use crate::unix::Sink;
use crate::usercopy;
use alloc::sync::Arc;
use alloc::vec::Vec;
use restricted::*;

const EBADF: i64 = 9;
const EFAULT: i64 = 14;
const EACCES: i64 = 13;
/// The most one read or write moves (Linux's MAX_RW_COUNT).
pub const MAX_RW_COUNT: u64 = 0x7fff_f000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Zero,
}

/// An open file description of null or zero.
pub struct DevOpen {
    pub kind: Kind,
    pub origin: Origin,
}

/// Opens the node `origin` as `kind`.
pub fn open(kind: Kind, flags: u32, origin: Origin) -> Result<i64, i64> {
    let kept = flags & (O_ACCMODE | O_NONBLOCK | O_CLOEXEC);
    // Always ready (`File::always_ready`: epoll refuses it with EPERM, as Linux's).
    files::install(files::new_id(), File::Dev(Arc::new(DevOpen { kind, origin })), kept)
}

/// Whether `[addr, addr + len)` lies in the program's memory (Linux's access_ok).
fn access_ok(addr: u64, len: u64) -> bool {
    addr.checked_add(len).is_some_and(|end| end <= SHARED_BASE)
}

/// A read's or write's buffer, checked and clamped as Linux's (`rw_verify_area`).
fn one_buffer(addr: u64, len: u64) -> Result<Vec<(u64, u64)>, i64> {
    if len > isize::MAX as u64 {
        return Err(EINVAL);
    }
    if !access_ok(addr, len) {
        return Err(EFAULT);
    }
    Ok(alloc::vec![(addr, len.min(MAX_RW_COUNT))])
}

/// An iovec array, checked and clamped as Linux's `import_iovec`: a length above
/// `isize::MAX` is EINVAL, a range beyond the program's memory EFAULT, and the total
/// stops at `MAX_RW_COUNT`.
pub fn checked_iovecs(iov: u64, count: u64) -> Result<Vec<(u64, u64)>, i64> {
    let mut vecs = files::iovecs(iov, count)?;
    let mut total = 0u64;
    for v in vecs.iter_mut() {
        if v.1 > isize::MAX as u64 {
            return Err(EINVAL);
        }
        if !access_ok(v.0, v.1) {
            return Err(EFAULT);
        }
        v.1 = v.1.min(MAX_RW_COUNT - total);
        total += v.1;
    }
    Ok(vecs)
}

/// The calls on one.
pub fn call(nr: u64, d: &DevOpen, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    use crate::files::{SYS_FSTAT, SYS_IOCTL, SYS_PREAD64, SYS_PREADV, SYS_PWRITE64, SYS_PWRITEV, SYS_READ, SYS_READV, SYS_WRITE, SYS_WRITEV};
    let readable = flags & O_ACCMODE != files::O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    match nr {
        SYS_FSTAT => usercopy::to_program(a1, &d.origin.stat()?).map(|_| 0),
        SYS_READ | SYS_READV | SYS_PREAD64 | SYS_PREADV if !readable => Err(EBADF),
        SYS_WRITE | SYS_WRITEV | SYS_PWRITE64 | SYS_PWRITEV if !writable => Err(EBADF),
        SYS_READ | SYS_PREAD64 => read(d, Sink::program(&one_buffer(a1, a2)?)),
        SYS_READV | SYS_PREADV => read(d, Sink::program(&checked_iovecs(a1, a2)?)),
        // Everything written goes (what the checks let through).
        SYS_WRITE | SYS_PWRITE64 => Ok(one_buffer(a1, a2)?[0].1 as i64),
        SYS_WRITEV | SYS_PWRITEV => Ok(checked_iovecs(a1, a2)?.iter().map(|v| v.1).sum::<u64>() as i64),
        // Linux's null and zero seek to 0.
        files::SYS_LSEEK => Ok(0),
        SYS_IOCTL => Err(files::ENOTTY),
        _ => Err(EINVAL),
    }
}

/// read(2): nothing from null; zeros from zero, the whole count, unless a signal comes
/// (then what was filled; checked every 64 KiB, as Linux's read_iter_zero checks after
/// every page).
pub fn read(d: &DevOpen, mut sink: Sink) -> Result<i64, i64> {
    if d.kind == Kind::Null {
        return Ok(0);
    }
    let zeros = [0u8; 4096];
    let mut done = 0usize;
    while sink.room() > 0 {
        let n = sink.room().min(zeros.len());
        match sink.put(&zeros[..n]) {
            Ok(k) => {
                done += k;
                if k < n {
                    break;
                }
            }
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
        if done % (64 * 1024) == 0 && sink.room() > 0 && files::signal_pending() {
            break;
        }
    }
    Ok(done as i64)
}

/// What mmap of one maps: zero's are anonymous memory (shared: a new object) once the
/// descriptor allows it, as Linux's: EACCES unless it is open for reading, or for a
/// shared writable mapping unless also for writing; a shared mapping of a descriptor not
/// open for writing stays read-only (mprotect cannot add PROT_WRITE). Null maps nothing.
pub fn map(d: &DevOpen, flags: u32, shared: bool, prot_write: bool) -> Result<files::Mapping, i64> {
    const ENODEV: i64 = 19;
    if d.kind == Kind::Null {
        return Err(ENODEV);
    }
    let readable = flags & O_ACCMODE != files::O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    if !readable || (shared && prot_write && !writable) {
        return Err(EACCES);
    }
    Ok(files::Mapping::Anonymous { read_only: shared && !writable })
}
