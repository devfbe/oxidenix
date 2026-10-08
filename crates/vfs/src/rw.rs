//! Where a read or write goes: the file position or an offset, and whether
//! a write appends. preadv2/pwritev2 add flags (`RWF_*`) to the offset
//! calls; pread64/pwrite64 and preadv/pwritev are the same with no flags.
//! As on Linux, a positional write to an O_APPEND descriptor appends
//! (musl's pwrite asks for `RWF_NOAPPEND` to get POSIX's behavior).

/// A high-priority request (polled I/O): a hint, nothing to do.
pub const RWF_HIPRI: u64 = 0x01;
/// The write is durable when the call returns (fdatasync's guarantee).
pub const RWF_DSYNC: u64 = 0x02;
/// The write is durable when the call returns (fsync's guarantee).
pub const RWF_SYNC: u64 = 0x04;
/// Fail with EAGAIN instead of waiting: not supported (EOPNOTSUPP, as
/// Linux answers for files without non-blocking I/O).
pub const RWF_NOWAIT: u64 = 0x08;
/// This write appends, as with O_APPEND.
pub const RWF_APPEND: u64 = 0x10;
/// This write does not append, even with O_APPEND.
pub const RWF_NOAPPEND: u64 = 0x20;

const SUPPORTED: u64 = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_APPEND | RWF_NOAPPEND;
const EINVAL: i64 = 22;
const EOPNOTSUPP: i64 = 95;

/// What a read or write does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// At this offset, leaving the file position; None: at the file
    /// position, which moves past the bytes transferred.
    pub at: Option<u64>,
    /// A write goes to the end of the file (an offset is then ignored; the
    /// file position moves only without one).
    pub append: bool,
    /// The written data must be durable before the call returns.
    pub sync: bool,
}

/// The plan for a read or a write (`write`) at `offset` (-1: the file
/// position) with `flags`, on a descriptor with or without O_APPEND.
pub fn plan(write: bool, offset: i64, flags: u64, o_append: bool) -> Result<Plan, i64> {
    if flags & !SUPPORTED != 0 {
        return Err(EOPNOTSUPP);
    }
    if flags & RWF_APPEND != 0 && flags & RWF_NOAPPEND != 0 {
        return Err(EINVAL);
    }
    let at = match offset {
        -1 => None,
        o if o < 0 => return Err(EINVAL),
        o => Some(o as u64),
    };
    let append = write && (flags & RWF_APPEND != 0 || (o_append && flags & RWF_NOAPPEND == 0));
    let sync = write && flags & (RWF_DSYNC | RWF_SYNC) != 0;
    Ok(Plan { at, append, sync })
}
