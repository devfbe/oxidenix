//! Open files of the server's tmpfs (phase R6c.2c): a placeholder in the
//! kernel's descriptor table names one (an open file description: offset,
//! directory snapshot, write access); the calls on it are the server's.
//! Reads and writes move bytes between the file object and the program's
//! memory in the kernel (`SYS_MO_FILE_READ`/`WRITE`), as the kernel's tmpfs
//! files did.

use crate::files::{self, File, EBADF, EINVAL, O_ACCMODE, O_WRONLY};
use crate::namespace::{check, ENOTDIR};
use crate::sync::Mutex;
use crate::syscall;
use crate::tmpfs::{Inode, EISDIR};
use crate::usercopy;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use restricted::*;

const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const O_DIRECTORY: u32 = 0o200000;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;

pub struct TmpOpen {
    pub inode: Arc<Inode>,
    /// The path it was opened by (for *at calls and fchdir).
    pub path: String,
    offset: Mutex<u64>,
    /// A directory's entries, taken when it is read from the start.
    snapshot: Mutex<Option<Vec<(String, u64, u8)>>>,
    /// Holds write access (opened for writing).
    write: bool,
}

impl Drop for TmpOpen {
    fn drop(&mut self) {
        if self.write {
            self.inode.put_write();
        }
    }
}

/// open(2) of a resolved tmpfs inode: a descriptor of the calling process.
pub fn open(inode: Arc<Inode>, flags: u32, path: String) -> Result<i64, i64> {
    let writable = flags & O_ACCMODE != 0;
    let dir = inode.is_dir();
    if dir && writable {
        return Err(EISDIR);
    }
    if flags & O_DIRECTORY != 0 && !dir {
        return Err(ENOTDIR);
    }
    let regular = inode.mode() & vfs::S_IFMT == vfs::S_IFREG;
    // Not while it runs as a program (ETXTBSY).
    let write = writable && regular;
    if write {
        inode.get_write()?;
    }
    let open = Arc::new(TmpOpen { inode, path, offset: Mutex::new(0), snapshot: Mutex::new(None), write });
    if write && flags & O_TRUNC != 0 {
        check(syscall(SYS_MO_TRUNCATE, [open.inode.object()?, 0, 0, 0, 0, 0]))?;
    }
    let kept = flags & (O_ACCMODE | files::O_NONBLOCK | O_APPEND | files::O_CLOEXEC);
    files::install(files::new_id(), File::Tmp(open), kept, POLLIN | POLLOUT)
}

/// The calls on an open tmpfs file (`a1`..`a3`: the call's arguments after
/// the descriptor).
pub fn call(nr: u64, f: &TmpOpen, flags: u32, a1: u64, a2: u64, a3: u64) -> Result<i64, i64> {
    let readable = flags & O_ACCMODE != O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    match nr {
        files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if !readable => Err(EBADF),
        files::SYS_WRITE | files::SYS_WRITEV | files::SYS_PWRITE64 | files::SYS_PWRITEV if !writable => Err(EBADF),
        files::SYS_READ => f.read_at_offset(&[(a1, a2)]),
        files::SYS_READV => f.read_at_offset(&files::iovecs(a1, a2)?),
        files::SYS_PREAD64 => f.read(&[(a1, a2)], signed(a3)?).map(|n| n as i64),
        files::SYS_PREADV => f.read(&files::iovecs(a1, a2)?, signed(a3)?).map(|n| n as i64),
        files::SYS_WRITE => f.write_at_offset(&[(a1, a2)], flags & O_APPEND != 0),
        files::SYS_WRITEV => f.write_at_offset(&files::iovecs(a1, a2)?, flags & O_APPEND != 0),
        files::SYS_PWRITE64 => f.write_positional(&[(a1, a2)], signed(a3)?, flags & O_APPEND != 0),
        files::SYS_PWRITEV => f.write_positional(&files::iovecs(a1, a2)?, signed(a3)?, flags & O_APPEND != 0),
        files::SYS_LSEEK => f.lseek(a1 as i64, a2),
        files::SYS_FSTAT => {
            usercopy::to_program(a1, &f.inode.stat())?;
            Ok(0)
        }
        files::SYS_FTRUNCATE if !writable => Err(EINVAL),
        files::SYS_FTRUNCATE => {
            let object = f.inode.object().map_err(|_| EINVAL)?;
            check(syscall(SYS_MO_TRUNCATE, [object, a1, 0, 0, 0, 0]))
        }
        // The contents are memory: nothing to write back.
        files::SYS_FSYNC | files::SYS_FDATASYNC => Ok(0),
        files::SYS_GETDENTS64 => f.getdents(a1, a2),
        files::SYS_FSTATFS => statfs(a1),
        files::SYS_IOCTL => Err(files::ENOTTY),
        _ => Err(EINVAL),
    }
}

fn signed(offset: u64) -> Result<u64, i64> {
    if (offset as i64) < 0 { Err(EINVAL) } else { Ok(offset) }
}

/// A tmpfs `struct statfs` (the kernel's tmpfs numbers: both are charged
/// to the same limit) at the program's `buf`.
pub fn statfs(buf: u64) -> Result<i64, i64> {
    let mut words = [0u8; 120];
    check(syscall(SYS_INODE_STATFS, [crate::namespace::kernel_root(), words.as_mut_ptr() as u64, 0, 0, 0, 0]))?;
    usercopy::to_program(buf, &words)?;
    Ok(0)
}

impl TmpOpen {
    fn object(&self) -> Result<u64, i64> {
        self.inode.object()
    }

    /// Reads into the program's buffers from `offset`: as far as the file
    /// goes. An error after some bytes ends the read with them.
    fn read(&self, vecs: &[(u64, u64)], offset: u64) -> Result<u64, i64> {
        let object = self.object()?;
        let mut done = 0u64;
        for &(base, len) in vecs {
            if len == 0 {
                continue;
            }
            match check(syscall(SYS_MO_FILE_READ, [object, offset + done, base, len, 0, 0])) {
                Ok(n) => {
                    done += n as u64;
                    if (n as u64) < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    }

    fn write(&self, vecs: &[(u64, u64)], offset: u64) -> Result<u64, i64> {
        let object = self.object()?;
        let mut done = 0u64;
        for &(base, len) in vecs {
            if len == 0 {
                continue;
            }
            match check(syscall(SYS_MO_FILE_WRITE, [object, offset + done, base, len, 0, 0])) {
                Ok(n) => {
                    done += n as u64;
                    if (n as u64) < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    }

    /// pwrite/pwritev: at `offset`, or with O_APPEND (as on Linux) at the
    /// end; the description's offset stays.
    fn write_positional(&self, vecs: &[(u64, u64)], offset: u64, append: bool) -> Result<i64, i64> {
        let n = if append {
            let _append = self.inode.append.lock();
            self.write(vecs, self.inode.size())?
        } else {
            self.write(vecs, offset)?
        };
        Ok(n as i64)
    }

    /// read/readv: at the description's offset, which moves by what was
    /// read (the offset's lock serializes the description's reads).
    fn read_at_offset(&self, vecs: &[(u64, u64)]) -> Result<i64, i64> {
        let mut off = self.offset.lock();
        let n = self.read(vecs, *off)?;
        *off += n;
        Ok(n as i64)
    }

    /// write/writev: at the offset, or with O_APPEND at the end (the
    /// inode's append lock makes finding the end and writing there one step
    /// for every appender).
    fn write_at_offset(&self, vecs: &[(u64, u64)], append: bool) -> Result<i64, i64> {
        let mut off = self.offset.lock();
        let n = if append {
            let _append = self.inode.append.lock();
            *off = self.inode.size();
            self.write(vecs, *off)?
        } else {
            self.write(vecs, *off)?
        };
        *off += n;
        Ok(n as i64)
    }

    fn lseek(&self, offset: i64, whence: u64) -> Result<i64, i64> {
        let mut off = self.offset.lock();
        let base = match whence {
            0 => 0,
            1 => *off as i64,
            2 => self.inode.size() as i64,
            _ => return Err(EINVAL),
        };
        let new = base.checked_add(offset).filter(|&n| n >= 0).ok_or(EINVAL)?;
        *off = new as u64;
        Ok(new)
    }

    /// getdents64: the entries from the offset (an index), as many as fit;
    /// a read from the start takes a new snapshot of the directory.
    fn getdents(&self, buf: u64, len: u64) -> Result<i64, i64> {
        if !self.inode.is_dir() {
            return Err(ENOTDIR);
        }
        let mut off = self.offset.lock();
        let mut snapshot = self.snapshot.lock();
        if *off == 0 || snapshot.is_none() {
            *snapshot = Some(self.inode.list()?);
        }
        let entries = snapshot.as_ref().expect("taken above");
        let mut out = Vec::new();
        let mut next = *off;
        while let Some((name, ino, dtype)) = entries.get(next as usize) {
            let reclen = (19 + name.len() + 1).next_multiple_of(8);
            if out.len() + reclen > len as usize {
                if out.is_empty() {
                    return Err(EINVAL);
                }
                break;
            }
            let at = out.len();
            out.resize(at + reclen, 0);
            let rec = &mut out[at..];
            rec[0..8].copy_from_slice(&ino.to_le_bytes());
            rec[8..16].copy_from_slice(&(next + 1).to_le_bytes());
            rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
            rec[18] = *dtype;
            rec[19..19 + name.len()].copy_from_slice(name.as_bytes());
            next += 1;
        }
        // The position moves only once the entries reached the program.
        usercopy::to_program(buf, &out)?;
        *off = next;
        Ok(out.len() as i64)
    }

    /// Reads into the server's memory (sendfile), at the offset.
    pub fn read_server(&self, buf: &mut [u8]) -> Result<usize, i64> {
        let mut off = self.offset.lock();
        let n = check(syscall(SYS_MO_READ, [self.object()?, *off, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0]))? as usize;
        *off += n as u64;
        Ok(n)
    }

    /// Writes from the server's memory (sendfile), at the offset or the end.
    pub fn write_server(&self, data: &[u8], append: bool) -> Result<usize, i64> {
        let mut off = self.offset.lock();
        let _append = if append { Some(self.inode.append.lock()) } else { None };
        if append {
            *off = self.inode.size();
        }
        let n = check(syscall(SYS_MO_WRITE, [self.object()?, *off, data.as_ptr() as u64, data.len() as u64, 0, 0]))? as usize;
        *off += n as u64;
        Ok(n)
    }

    /// The object to map, for mmap: a handle the caller closes once mapped
    /// (with a hold of write access for a shared mapping through a
    /// writable descriptor, which may become writable), and whether the
    /// mapping must stay read-only.
    pub fn map_object(&self, flags: u32, shared: bool, prot_write: bool) -> Result<(u64, bool), i64> {
        let readable = flags & O_ACCMODE != O_WRONLY;
        let writable = flags & O_ACCMODE != 0;
        const EACCES: i64 = 13;
        const ENODEV: i64 = 19;
        if !readable || (shared && prot_write && !writable) {
            return Err(EACCES);
        }
        if self.inode.mode() & vfs::S_IFMT != vfs::S_IFREG {
            return Err(ENODEV);
        }
        if shared && writable {
            return Ok((self.inode.hold(false)?, false));
        }
        // A handle of its own (the mapping keeps the contents, not this).
        let h = check(syscall(SYS_MO_HOLD, [self.object()?, 0, 0, 0, 0, 0]))? as u64;
        Ok((h, shared))
    }
}

/// The right to run a tmpfs file: a held handle for `exec_target`.
pub fn exec_hold(inode: &Arc<Inode>) -> Result<u64, i64> {
    if inode.mode() & vfs::S_IFMT != vfs::S_IFREG {
        const ENOEXEC: i64 = 8;
        return Err(if inode.is_dir() { EISDIR } else { ENOEXEC });
    }
    inode.hold(true)
}
