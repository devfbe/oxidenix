//! O_PATH descriptors (open(2) with O_PATH; phase R6d, for any node of the namespace):
//! the descriptor names a node and opens nothing, no driver, no file. As Linux's: the
//! access mode and every flag but O_DIRECTORY, O_NOFOLLOW and O_CLOEXEC are ignored
//! (O_CREAT creates nothing), O_NOFOLLOW on a symlink names the symlink itself; fstat and
//! fstatfs describe the node (live), the descriptor serves as the directory of *at calls
//! and with AT_EMPTY_PATH as their target, fchdir takes a directory; reads, writes,
//! ioctls, mmap and the f* calls that change the node (fchmod, fchown, futimens) are
//! EBADF. Its description's status flags carry O_PATH: of the descriptor table's calls only
//! dup, close and fcntl's F_DUPFD, F_GETFD, F_SETFD and F_GETFL take it (F_GETFL shows
//! O_PATH), SCM_RIGHTS passes it, poll gives POLLNVAL, select and epoll EBADF.

use crate::files::{self, File, O_CLOEXEC};
use crate::namespace::Origin;
use crate::usercopy;
use alloc::sync::Arc;

const EBADF: i64 = 9;
pub const O_PATH: u32 = 0o10000000;
pub const O_DIRECTORY: u32 = 0o200000;
pub const O_NOFOLLOW: u32 = 0o400000;

pub struct PathOpen {
    pub origin: Origin,
}

/// An O_PATH descriptor for `origin` (`flags` as open(2) had them).
pub fn open(flags: u32, origin: Origin) -> Result<i64, i64> {
    let kept = flags & (O_PATH | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    files::install(files::new_id(), File::Path(Arc::new(PathOpen { origin })), kept | O_PATH)
}

/// The calls on one that reach the server.
pub fn call(nr: u64, p: &PathOpen, a1: u64) -> Result<i64, i64> {
    match nr {
        files::SYS_FSTAT => usercopy::to_program(a1, &p.origin.stat()?).map(|_| 0),
        files::SYS_FSTATFS => crate::paths::statfs_node(&p.origin.node, a1),
        _ => Err(EBADF),
    }
}
