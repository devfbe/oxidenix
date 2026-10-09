//! The file protocol of the data plane: the requests the Linux server (the
//! client) sends diskfs (the service) through a channel's submission ring,
//! and their completions (docs/design/io-rings.md, "The file protocol").
//!
//! A request is a `ring::Desc`: `op`, the client's `tag` (echoed by the
//! completion), the inode in `object`, and a buffer, a range of a grant
//! (`grant`, `buf_off`, `len`). File data travels in granted pages only:
//! diskfs's device moves it between the disk and the grant by DMA. Names
//! and results that do not fit a completion travel in a grant too. Every
//! field an operation does not use must be 0, and `flags` is 0: anything
//! else completes with EINVAL (`Request::decode` is the one place that
//! says what is valid). A completion carries the tag, the operation, a
//! status (a value >= 0 or a negative errno) and four values.
//!
//! | op | request | completion |
//! |----|---------|------------|
//! | `READ` | `object` inode, `offset`, buffer (a writable grant) | bytes read (short at the end of the file), v0 = file size |
//! | `WRITE` | `object` inode, `offset`, buffer | bytes written (all of them), v0 = file size |
//! | `FLUSH` | - | 0 once every write completed before it is durable |
//! | `STAT` | `object` inode | 0, `Stat` in v0..v3 |
//! | `LOOKUP` | `object` directory, buffer = name | v0 = inode, v1 = its mode, v2 = its generation |
//! | `CREATE` | `object` directory, buffer = name, `arg` = [kind, permissions, target length]; a symlink's target follows the name in the grant | v0 = new inode, v1 = its mode, v2 = its generation |
//! | `UNLINK` | `object` directory, buffer = name, `arg[0]` = 1 for a directory | v0 = inode whose last link went (0: none) |
//! | `RENAME` | `object` old directory, buffer = old name, `arg` = [new directory, new name length]; the new name follows the old one | as `UNLINK` (an entry replaced) |
//! | `TRUNCATE` | `object` inode, `offset` = new size | 0 |
//! | `READDIR` | `object` directory, `offset` = cursor (0: from the start), buffer for entries | bytes of entries (`dirents`), v0 = next cursor (0: done) |
//! | `RELEASE` | `object` inode | 0: the client holds it no more (see "Holds") |
//! | `READLINK` | `object` symlink, buffer for the target | length of the target |
//! | `SETPERM` | `object` inode, `arg[0]` = permission bits | 0 |
//! | `SETTIMES` | `object` inode, `offset` = which (`TIME_ATIME`, `TIME_MTIME`, `TIME_CTIME`), `arg` = [atime, mtime, ctime] in seconds since 1970 (below 2^32; 0 for one not set) | 0 |
//! | `STATFS` | - | 0, `Usage` in v0..v3 (sizes, counts, the largest file) |
//! | `FORGET` | `grant` | 0 once no request on the grant is in flight and the service let go of it |
//! | `PROMISE` | `object` inode, `offset`, `arg[0]` = length (at most `MAX_TRANSFER`) | 0 once the blocks a later `WRITE` of the range needs are kept for it (see "Promises"); ENOSPC |
//!
//! **Ordering.** Reads and writes run concurrently and complete in any
//! order (the device reorders them); a write waits only for writes in
//! flight on the same blocks of the same file. Every other operation is a
//! barrier: it starts once every request taken before it (on any channel)
//! completed, and none taken after it starts before it completed; except
//! `FORGET` of a grant no request in flight uses, which completes at once.
//!
//! **Durability.** A completed `WRITE` is visible to every later request
//! (on any channel, and to the kernel's IPC clients); it is durable only
//! after a later `FLUSH` completed: write-back, as Linux's page cache
//! (`fsync` is `FLUSH`). A `FLUSH` writes the data to the device and
//! empties the device's cache before the metadata that points to it (the
//! block maps, sizes, bitmaps) is written, then empties the cache again:
//! after a crash, a file never shows blocks whose data did not reach the
//! disk. Other operations commit their own metadata changes before they
//! complete, as the IPC protocol does (`fsproto`), and with it what
//! completed writes changed (also data first).
//!
//! **Holds.** A client holds every inode it named in a request or got
//! back from `LOOKUP`, `CREATE`, `UNLINK` or `RENAME`, until it sends
//! `RELEASE` for it or its channel goes. An inode whose last link went is
//! freed (with its blocks) only when no client holds it, the kernel's IPC
//! client included: one client's `RELEASE` never frees what another still
//! uses. (A client releases what it no longer caches; until then an
//! unlinked inode stays allocated.)
//!
//! **Room.** The service takes a request only when the completion ring
//! has room for its completion; a request that waits for room does not
//! keep the service busy. A client that took completions while requests
//! of its were still waiting in the submission ring rings the submission
//! doorbell (`Producer::ring_doorbell`, a no-op unless the service sleeps),
//! or they wait until its next request.
//!
//! **Promises.** A client that caches writes promises the range before it
//! accepts a write into its cache (`PROMISE`, as delayed allocation
//! reserves blocks): the service counts the data blocks the file lacks
//! there and the indirect blocks they need against its free blocks
//! (ENOSPC if they do not cover them), and no other allocation takes them
//! until the `WRITE` that comes later does. Promising again what the same
//! channel promised costs nothing. A promise ends when its blocks are
//! written, truncated away or freed with the file, or when the channel
//! goes; `STATFS` counts promised blocks as used. A `PROMISE` runs at
//! once, like `FORGET`: it touches no file.
//!
//! **Grants.** The service keeps what it learnt of a grant (its size, the
//! device addresses of its pages) until `FORGET`: the client sends it
//! before it revokes the grant, so that the revoke never waits for the
//! service (`restricted::SYS_REVOKE`, `REVOKE_DRAINING`).

#![no_std]

pub use ring::Desc;

/// The service's IPC name, and the slots per ring of its channels.
pub const SERVICE: &str = "diskfs";
pub const SLOTS: u32 = 128;
/// The most bytes one `READ` or `WRITE` moves (split larger transfers).
pub const MAX_TRANSFER: u32 = 1 << 20;
/// The longest name, and the longest symlink target.
pub const NAME_MAX: u32 = 255;
pub const TARGET_MAX: u32 = 4095;

pub mod op {
    pub const READ: u16 = 1;
    pub const WRITE: u16 = 2;
    pub const FLUSH: u16 = 3;
    pub const STAT: u16 = 4;
    pub const LOOKUP: u16 = 5;
    pub const CREATE: u16 = 6;
    pub const UNLINK: u16 = 7;
    pub const RENAME: u16 = 8;
    pub const TRUNCATE: u16 = 9;
    pub const READDIR: u16 = 10;
    pub const RELEASE: u16 = 11;
    pub const READLINK: u16 = 12;
    pub const SETPERM: u16 = 13;
    pub const STATFS: u16 = 14;
    pub const FORGET: u16 = 15;
    pub const PROMISE: u16 = 16;
    pub const SETTIMES: u16 = 17;
}

/// `SETTIMES`'s choice of times (`offset`).
pub const TIME_ATIME: u64 = 1;
pub const TIME_MTIME: u64 = 2;
pub const TIME_CTIME: u64 = 4;

/// The errors the protocol itself gives (others come from the filesystem).
pub mod errno {
    pub const ENOENT: i64 = 2;
    pub const EIO: i64 = 5;
    /// An unknown or revoked grant.
    pub const EBADF: i64 = 9;
    pub const ENOMEM: i64 = 12;
    /// A result buffer in a read-only grant.
    pub const EACCES: i64 = 13;
    /// The grant was revoked while the service copied to or from it.
    pub const EFAULT: i64 = 14;
    pub const EINVAL: i64 = 22;
    pub const ENOSPC: i64 = 28;
    pub const ERANGE: i64 = 34;
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOSYS: i64 = 38;
}
use errno::*;

/// What `CREATE` makes (`arg[0]`).
pub const KIND_FILE: u64 = 1;
pub const KIND_DIR: u64 = 2;
pub const KIND_SYMLINK: u64 = 3;
/// A socket's name: an inode without data.
pub const KIND_SOCKET: u64 = 4;

/// Entry types in `READDIR` results (ext2's).
pub const TYPE_FILE: u8 = 1;
pub const TYPE_DIR: u8 = 2;
pub const TYPE_SOCKET: u8 = 6;
pub const TYPE_SYMLINK: u8 = 7;

/// A byte range of a grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buf {
    pub grant: u32,
    pub offset: u32,
    pub len: u32,
}

impl Buf {
    /// The byte after the range, in the grant.
    pub fn end(&self) -> u64 {
        self.offset as u64 + self.len as u64
    }

    /// The range right after this one, `len` bytes long, in the same grant.
    fn after(&self, len: u64) -> Result<Buf, i64> {
        let offset = u32::try_from(self.end()).map_err(|_| EINVAL)?;
        let len = u32::try_from(len).map_err(|_| EINVAL)?;
        Ok(Buf { grant: self.grant, offset, len })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// The target is in the grant, right after the name.
    Symlink(Buf),
    Socket,
}

/// A request, validated (`decode`) or to be sent (`encode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Read { ino: u32, offset: u64, buf: Buf },
    Write { ino: u32, offset: u64, buf: Buf },
    Flush,
    Stat { ino: u32 },
    Lookup { dir: u32, name: Buf },
    Create { dir: u32, name: Buf, kind: Kind, perm: u32 },
    Unlink { dir: u32, name: Buf, is_dir: bool },
    Rename { from: u32, name: Buf, to: u32, new_name: Buf },
    Truncate { ino: u32, len: u64 },
    Readdir { dir: u32, cursor: u64, buf: Buf },
    Release { ino: u32 },
    Readlink { ino: u32, buf: Buf },
    SetPerm { ino: u32, perm: u32 },
    /// The times to set: access, modification, change (seconds; None:
    /// keep).
    SetTimes { ino: u32, atime: Option<u32>, mtime: Option<u32>, ctime: Option<u32> },
    Statfs,
    Forget { grant: u32 },
    Promise { ino: u32, offset: u64, len: u64 },
}

/// The fields of a `Desc` an operation reads, for the check that the
/// others are 0.
struct Used {
    object: bool,
    offset: bool,
    buf: bool,
    grant: bool,
    args: usize,
}

const NONE: Used = Used { object: false, offset: false, buf: false, grant: false, args: 0 };

fn ino(object: u64) -> Result<u32, i64> {
    u32::try_from(object).map_err(|_| EINVAL)
}

fn name_len(len: u64) -> Result<u64, i64> {
    match len {
        0 => Err(EINVAL),
        n if n > NAME_MAX as u64 => Err(ENAMETOOLONG),
        n => Ok(n),
    }
}

fn perm(arg: u64) -> Result<u32, i64> {
    if arg > 0o7777 {
        return Err(EINVAL);
    }
    Ok(arg as u32)
}

impl Request {
    /// Validates a request copied out of the ring (copied once: `d` is the
    /// service's own). A negative errno to complete it with if it is
    /// malformed: ENOSYS for an unknown operation, EINVAL for a field out
    /// of range or set where the operation takes none, ENAMETOOLONG for a
    /// name or target over the limit. (What the service checks itself: the
    /// grant exists and holds the buffer, the inode exists.)
    pub fn decode(d: &Desc) -> Result<Request, i64> {
        let buf = Buf { grant: d.grant, offset: d.buf_off, len: d.len };
        let (request, used) = match d.op {
            op::READ | op::WRITE => {
                if d.len > MAX_TRANSFER || d.offset.checked_add(d.len as u64).is_none() {
                    return Err(EINVAL);
                }
                let (ino, offset) = (ino(d.object)?, d.offset);
                let r = if d.op == op::READ { Request::Read { ino, offset, buf } } else { Request::Write { ino, offset, buf } };
                (r, Used { object: true, offset: true, buf: true, ..NONE })
            }
            op::FLUSH => (Request::Flush, NONE),
            op::STATFS => (Request::Statfs, NONE),
            op::STAT => (Request::Stat { ino: ino(d.object)? }, Used { object: true, ..NONE }),
            op::RELEASE => (Request::Release { ino: ino(d.object)? }, Used { object: true, ..NONE }),
            op::LOOKUP => {
                name_len(d.len as u64)?;
                (Request::Lookup { dir: ino(d.object)?, name: buf }, Used { object: true, buf: true, ..NONE })
            }
            op::CREATE => {
                name_len(d.len as u64)?;
                let target = d.arg[2];
                let kind = match d.arg[0] {
                    KIND_FILE if target == 0 => Kind::File,
                    KIND_DIR if target == 0 => Kind::Dir,
                    KIND_SOCKET if target == 0 => Kind::Socket,
                    KIND_SYMLINK if target == 0 => return Err(EINVAL),
                    KIND_SYMLINK if target > TARGET_MAX as u64 => return Err(ENAMETOOLONG),
                    KIND_SYMLINK => Kind::Symlink(buf.after(target)?),
                    _ => return Err(EINVAL),
                };
                let r = Request::Create { dir: ino(d.object)?, name: buf, kind, perm: perm(d.arg[1])? };
                (r, Used { object: true, buf: true, args: 3, ..NONE })
            }
            op::UNLINK => {
                name_len(d.len as u64)?;
                let is_dir = match d.arg[0] {
                    0 => false,
                    1 => true,
                    _ => return Err(EINVAL),
                };
                (Request::Unlink { dir: ino(d.object)?, name: buf, is_dir }, Used { object: true, buf: true, args: 1, ..NONE })
            }
            op::RENAME => {
                name_len(d.len as u64)?;
                let new_name = buf.after(name_len(d.arg[1])?)?;
                let r = Request::Rename { from: ino(d.object)?, name: buf, to: ino(d.arg[0])?, new_name };
                (r, Used { object: true, buf: true, args: 2, ..NONE })
            }
            op::TRUNCATE => (Request::Truncate { ino: ino(d.object)?, len: d.offset }, Used { object: true, offset: true, ..NONE }),
            op::READDIR => {
                if d.len == 0 || d.len > MAX_TRANSFER {
                    return Err(EINVAL);
                }
                (Request::Readdir { dir: ino(d.object)?, cursor: d.offset, buf }, Used { object: true, offset: true, buf: true, ..NONE })
            }
            op::READLINK => {
                if d.len == 0 || d.len > MAX_TRANSFER {
                    return Err(EINVAL);
                }
                (Request::Readlink { ino: ino(d.object)?, buf }, Used { object: true, buf: true, ..NONE })
            }
            op::SETPERM => (Request::SetPerm { ino: ino(d.object)?, perm: perm(d.arg[0])? }, Used { object: true, args: 1, ..NONE }),
            op::SETTIMES => {
                let which = d.offset;
                if which & !(TIME_ATIME | TIME_MTIME | TIME_CTIME) != 0 {
                    return Err(EINVAL);
                }
                let time = |bit: u64, value: u64| -> Result<Option<u32>, i64> {
                    match (which & bit != 0, value) {
                        (false, 0) => Ok(None),
                        (false, _) => Err(EINVAL),
                        (true, v) => u32::try_from(v).map(Some).map_err(|_| EINVAL),
                    }
                };
                let r = Request::SetTimes {
                    ino: ino(d.object)?,
                    atime: time(TIME_ATIME, d.arg[0])?,
                    mtime: time(TIME_MTIME, d.arg[1])?,
                    ctime: time(TIME_CTIME, d.arg[2])?,
                };
                (r, Used { object: true, offset: true, args: 3, ..NONE })
            }
            op::FORGET => (Request::Forget { grant: d.grant }, Used { grant: true, ..NONE }),
            op::PROMISE => {
                let len = d.arg[0];
                if len > MAX_TRANSFER as u64 || d.offset.checked_add(len).is_none() {
                    return Err(EINVAL);
                }
                (Request::Promise { ino: ino(d.object)?, offset: d.offset, len }, Used { object: true, offset: true, args: 1, ..NONE })
            }
            _ => return Err(ENOSYS),
        };
        let unused = d.flags != 0
            || (!used.object && d.object != 0)
            || (!used.offset && d.offset != 0)
            || (!used.buf && !used.grant && d.grant != 0)
            || (!used.buf && (d.buf_off != 0 || d.len != 0))
            || d.arg[used.args..].iter().any(|&a| a != 0);
        if unused {
            return Err(EINVAL);
        }
        Ok(request)
    }

    /// The request as a descriptor with tag `tag`.
    pub fn encode(&self, tag: u64) -> Desc {
        let mut d = Desc { op: self.op(), tag, ..Desc::default() };
        let set_buf = |d: &mut Desc, b: &Buf| {
            d.grant = b.grant;
            d.buf_off = b.offset;
            d.len = b.len;
        };
        match *self {
            Request::Read { ino, offset, buf } | Request::Write { ino, offset, buf } => {
                d.object = ino as u64;
                d.offset = offset;
                set_buf(&mut d, &buf);
            }
            Request::Flush | Request::Statfs => {}
            Request::Stat { ino } | Request::Release { ino } => d.object = ino as u64,
            Request::Lookup { dir, name } => {
                d.object = dir as u64;
                set_buf(&mut d, &name);
            }
            Request::Create { dir, name, kind, perm } => {
                d.object = dir as u64;
                set_buf(&mut d, &name);
                d.arg = match kind {
                    Kind::File => [KIND_FILE, perm as u64, 0],
                    Kind::Dir => [KIND_DIR, perm as u64, 0],
                    Kind::Socket => [KIND_SOCKET, perm as u64, 0],
                    Kind::Symlink(target) => [KIND_SYMLINK, perm as u64, target.len as u64],
                };
            }
            Request::Unlink { dir, name, is_dir } => {
                d.object = dir as u64;
                set_buf(&mut d, &name);
                d.arg[0] = is_dir as u64;
            }
            Request::Rename { from, name, to, new_name } => {
                d.object = from as u64;
                set_buf(&mut d, &name);
                d.arg = [to as u64, new_name.len as u64, 0];
            }
            Request::Truncate { ino, len } => {
                d.object = ino as u64;
                d.offset = len;
            }
            Request::Readdir { dir, cursor, buf } => {
                d.object = dir as u64;
                d.offset = cursor;
                set_buf(&mut d, &buf);
            }
            Request::Readlink { ino, buf } => {
                d.object = ino as u64;
                set_buf(&mut d, &buf);
            }
            Request::SetPerm { ino, perm } => {
                d.object = ino as u64;
                d.arg[0] = perm as u64;
            }
            Request::SetTimes { ino, atime, mtime, ctime } => {
                d.object = ino as u64;
                let bit = |t: Option<u32>, b: u64| if t.is_some() { b } else { 0 };
                d.offset = bit(atime, TIME_ATIME) | bit(mtime, TIME_MTIME) | bit(ctime, TIME_CTIME);
                d.arg = [atime.unwrap_or(0) as u64, mtime.unwrap_or(0) as u64, ctime.unwrap_or(0) as u64];
            }
            Request::Forget { grant } => d.grant = grant,
            Request::Promise { ino, offset, len } => {
                d.object = ino as u64;
                d.offset = offset;
                d.arg[0] = len;
            }
        }
        d
    }

    pub fn op(&self) -> u16 {
        match self {
            Request::Read { .. } => op::READ,
            Request::Write { .. } => op::WRITE,
            Request::Flush => op::FLUSH,
            Request::Stat { .. } => op::STAT,
            Request::Lookup { .. } => op::LOOKUP,
            Request::Create { .. } => op::CREATE,
            Request::Unlink { .. } => op::UNLINK,
            Request::Rename { .. } => op::RENAME,
            Request::Truncate { .. } => op::TRUNCATE,
            Request::Readdir { .. } => op::READDIR,
            Request::Release { .. } => op::RELEASE,
            Request::Readlink { .. } => op::READLINK,
            Request::SetPerm { .. } => op::SETPERM,
            Request::SetTimes { .. } => op::SETTIMES,
            Request::Statfs => op::STATFS,
            Request::Forget { .. } => op::FORGET,
            Request::Promise { .. } => op::PROMISE,
        }
    }

    /// Whether it runs concurrently with others (see "Ordering").
    pub fn is_data(&self) -> bool {
        matches!(self, Request::Read { .. } | Request::Write { .. })
    }
}

/// Checks a name copied out of a grant: UTF-8 (ext2fs takes `str`),
/// without '/' or NUL.
pub fn check_name(bytes: &[u8]) -> Result<&str, i64> {
    let name = core::str::from_utf8(bytes).map_err(|_| EINVAL)?;
    if name.is_empty() || name.bytes().any(|b| b == b'/' || b == 0) {
        return Err(EINVAL);
    }
    Ok(name)
}

/// Checks a symlink target copied out of a grant: UTF-8, without NUL.
pub fn check_target(bytes: &[u8]) -> Result<&str, i64> {
    let target = core::str::from_utf8(bytes).map_err(|_| EINVAL)?;
    if target.is_empty() || target.bytes().any(|b| b == 0) {
        return Err(EINVAL);
    }
    Ok(target)
}

/// A completion: the request's tag and operation, the status, values (the
/// encoding every protocol on the rings shares).
pub use ring::Completion;

/// `STAT`'s result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Stat {
    pub mode: u32,
    pub links: u32,
    pub size: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    /// Changes whenever the inode number is given to a new file (ext2's
    /// i_generation): a client that held an inode before a restart of the
    /// service tells it from a new file of that number.
    pub generation: u32,
}

impl Stat {
    /// v0 = mode | links << 32, v1 = size, v2 = atime | mtime << 32,
    /// v3 = ctime | generation << 32.
    pub fn to_values(&self) -> [u64; 4] {
        [
            self.mode as u64 | (self.links as u64) << 32,
            self.size,
            self.atime as u64 | (self.mtime as u64) << 32,
            self.ctime as u64 | (self.generation as u64) << 32,
        ]
    }

    pub fn from_values(v: &[u64; 4]) -> Stat {
        Stat {
            mode: v[0] as u32,
            links: (v[0] >> 32) as u32,
            size: v[1],
            atime: v[2] as u32,
            mtime: (v[2] >> 32) as u32,
            ctime: v[3] as u32,
            generation: (v[3] >> 32) as u32,
        }
    }
}

/// `STATFS`'s result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Usage {
    pub block_size: u32,
    pub blocks: u32,
    pub free_blocks: u32,
    pub inodes: u32,
    pub free_inodes: u32,
    /// The largest file the filesystem can hold (EFBIG beyond).
    pub max_file_size: u64,
}

impl Usage {
    /// v0 = block size, v1 = blocks | free blocks << 32, v2 = inodes |
    /// free inodes << 32, v3 = the largest file size.
    pub fn to_values(&self) -> [u64; 4] {
        [self.block_size as u64, self.blocks as u64 | (self.free_blocks as u64) << 32, self.inodes as u64 | (self.free_inodes as u64) << 32, self.max_file_size]
    }

    pub fn from_values(v: &[u64; 4]) -> Usage {
        Usage {
            block_size: v[0] as u32,
            blocks: v[1] as u32,
            free_blocks: (v[1] >> 32) as u32,
            inodes: v[2] as u32,
            free_inodes: (v[2] >> 32) as u32,
            max_file_size: v[3],
        }
    }
}

/// Bytes of a `READDIR` entry's header: u32 inode, u8 type, u8 name length
/// (little-endian), then the name.
pub const DIRENT_HEADER: usize = 6;

/// Writes an entry at the start of `out`; its length, or None if it does
/// not fit (or the name is longer than `NAME_MAX`).
pub fn put_dirent(out: &mut [u8], ino: u32, kind: u8, name: &[u8]) -> Option<usize> {
    let len = DIRENT_HEADER + name.len();
    if name.len() > NAME_MAX as usize || out.len() < len {
        return None;
    }
    out[..4].copy_from_slice(&ino.to_le_bytes());
    out[4] = kind;
    out[5] = name.len() as u8;
    out[DIRENT_HEADER..len].copy_from_slice(name);
    Some(len)
}

/// The entries of a `READDIR` result: (inode, type, name). Stops at the
/// first incomplete one.
pub fn dirents(mut p: &[u8]) -> impl Iterator<Item = (u32, u8, &[u8])> {
    core::iter::from_fn(move || {
        if p.len() < DIRENT_HEADER {
            return None;
        }
        let (ino, kind, len) = (u32::from_le_bytes([p[0], p[1], p[2], p[3]]), p[4], p[5] as usize);
        let name = p.get(DIRENT_HEADER..DIRENT_HEADER + len)?;
        p = &p[DIRENT_HEADER + len..];
        Some((ino, kind, name))
    })
}
