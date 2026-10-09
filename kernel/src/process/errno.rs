//! The kernel's error numbers, as its calls return them (negated): Linux's values, which the
//! Linux server passes on to its programs unchanged.

pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const ESRCH: i64 = 3;
pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const E2BIG: i64 = 7;
pub const ENOEXEC: i64 = 8;
pub const EBADF: i64 = 9;
pub const ECHILD: i64 = 10;
pub const EAGAIN: i64 = 11;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;
pub const EBUSY: i64 = 16;
pub const EEXIST: i64 = 17;
pub const EINVAL: i64 = 22;
pub const EMFILE: i64 = 24;
pub const EFBIG: i64 = 27;
pub const ENOSPC: i64 = 28;
pub const EPIPE: i64 = 32;
pub const ERANGE: i64 = 34;
pub const ENAMETOOLONG: i64 = 36;
pub const ENOSYS: i64 = 38;
pub const ENODATA: i64 = 61;
pub const EOPNOTSUPP: i64 = 95;
pub const EISCONN: i64 = 106;
pub const ENOTCONN: i64 = 107;
pub const ETIMEDOUT: i64 = 110;
pub const ECONNREFUSED: i64 = 111;

/// Syscall result: Ok(return value) or Err(positive errno).
pub type SysResult = Result<i64, i64>;
