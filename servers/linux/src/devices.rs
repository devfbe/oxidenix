//! Device nodes of the server's filesystems that are no terminals (phase R6d): a
//! character device node names its driver by its number wherever it is (ADR 0007), so
//! a node (1,3) or (1,5) on the server's tmpfs or /data is null or zero, served here with
//! the node's own status; and an `O_PATH` open of any device node (also the kernel's)
//! opens the node alone, without its driver, as Linux's: its status, nothing else
//! (reads, writes and ioctls are EBADF). The kernel's own /dev/null and /dev/zero stay the
//! kernel's.

use crate::files::{self, File, EINVAL, O_ACCMODE, O_CLOEXEC, O_NONBLOCK};
use crate::unix::Sink;
use crate::usercopy;
use alloc::sync::Arc;

const EBADF: i64 = 9;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Zero,
    /// `O_PATH`: the node only.
    Path,
}

/// An open file description of a device node served here.
pub struct DevOpen {
    pub kind: Kind,
    /// The node's status (fstat).
    pub stat: [u8; 144],
}

/// Opens a device node as `kind` (`O_PATH` keeps no access mode, as Linux's).
pub fn open(kind: Kind, flags: u32, stat: [u8; 144]) -> Result<i64, i64> {
    let kept = if kind == Kind::Path { flags & O_CLOEXEC } else { flags & (O_ACCMODE | O_NONBLOCK | O_CLOEXEC) };
    let id = files::new_id();
    let open = Arc::new(DevOpen { kind, stat });
    // Always ready (`files::install`: epoll refuses it with EPERM, as Linux's).
    files::install(id, File::Dev(open), kept, POLLIN | POLLOUT)
}

/// The calls on one.
pub fn call(nr: u64, d: &DevOpen, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    use crate::files::{SYS_FSTAT, SYS_IOCTL, SYS_PREAD64, SYS_PREADV, SYS_PWRITE64, SYS_PWRITEV, SYS_READ, SYS_READV, SYS_WRITE, SYS_WRITEV};
    if nr == SYS_FSTAT {
        return usercopy::to_program(a1, &d.stat).map(|_| 0);
    }
    if d.kind == Kind::Path {
        return Err(EBADF);
    }
    let readable = flags & O_ACCMODE != files::O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    match nr {
        SYS_READ | SYS_READV | SYS_PREAD64 | SYS_PREADV if !readable => Err(EBADF),
        SYS_WRITE | SYS_WRITEV | SYS_PWRITE64 | SYS_PWRITEV if !writable => Err(EBADF),
        SYS_READ | SYS_PREAD64 => read(d, Sink::program(&[(a1, a2)])),
        SYS_READV | SYS_PREADV => read(d, Sink::program(&files::iovecs(a1, a2)?)),
        // Everything written goes.
        SYS_WRITE | SYS_PWRITE64 => Ok(a2 as i64),
        SYS_WRITEV | SYS_PWRITEV => Ok(files::iovecs(a1, a2)?.iter().map(|v| v.1).sum::<u64>() as i64),
        // Linux's null and zero seek to 0.
        files::SYS_LSEEK => Ok(0),
        SYS_IOCTL => Err(files::ENOTTY),
        _ => Err(EINVAL),
    }
}

/// read(2): nothing from null, zeros from zero (up to the buffers).
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
        if done >= 1 << 20 {
            // A bounded piece per call, as Linux's read_zero (it reschedules).
            break;
        }
    }
    Ok(done as i64)
}
