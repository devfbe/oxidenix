//! Open files of /proc and /sys (I/O rings step 5): a placeholder in the
//! kernel's descriptor table names one (an open file description: the
//! node, its offset and a snapshot of its contents); the calls on it are
//! the server's (`procfs` makes the contents).
//!
//! As Linux's seq_file: a read from offset 0 makes the contents anew and
//! keeps them; reads further on continue in what was kept (made now if
//! nothing was), so a file read in pieces is consistent, and a program that
//! reads it again from the start (top, htop: `pread(fd, buf, n, 0)`) sees
//! it current. A directory likewise: its entries are taken when it is read
//! from the start. Files are read-only (open for writing: EACCES); the
//! copies to the program are made with no lock of the server held but the
//! description's offset (which only its own calls take, never the pager).

use crate::files::{self, File, EBADF, EINVAL, O_ACCMODE, O_WRONLY};
use crate::procfs::{self, Opened, ProcNode};
use crate::sync::Mutex;
use crate::usercopy;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const EACCES: i64 = 13;
const ENXIO: i64 = 6;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const O_CREAT: u32 = 0o100;
const O_TRUNC: u32 = 0o1000;
const O_DIRECTORY: u32 = 0o200000;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;

pub struct ProcOpen {
    pub node: ProcNode,
    /// The path it was opened by (for *at calls and fchdir).
    pub path: String,
    dir: bool,
    /// The file position; for a directory, the index of the next entry.
    offset: Mutex<u64>,
    /// The contents (a file) as of the last read from the start.
    contents: Mutex<Option<Arc<Vec<u8>>>>,
    /// The entries (a directory) as of the last read from the start.
    entries: Mutex<Option<Arc<Vec<(String, u64, u8)>>>>,
}

/// open(2) of a resolved node of /proc or /sys: a descriptor of the calling
/// process. An open file a magic link led to is opened again (a pipe: a
/// new end of it; anything else without a node: ENXIO, as on Linux).
pub fn open(node: ProcNode, flags: u32, path: String) -> Result<i64, i64> {
    if let ProcNode::Open(opened) = &node {
        return reopen(opened, flags);
    }
    let dir = procfs::is_dir(&node);
    let writable = flags & O_ACCMODE != 0;
    if dir && writable {
        return Err(EISDIR);
    }
    if flags & O_DIRECTORY != 0 && !dir {
        return Err(ENOTDIR);
    }
    if writable || flags & (O_CREAT | O_TRUNC) != 0 {
        return Err(EACCES);
    }
    // It must exist now (a process may have ended since the walk), and be
    // the caller's to open.
    procfs::may_open(&node)?;
    let open = Arc::new(ProcOpen { node, path, dir, offset: Mutex::new(0), contents: Mutex::new(None), entries: Mutex::new(None) });
    let kept = flags & (O_ACCMODE | files::O_NONBLOCK | files::O_CLOEXEC);
    files::install(files::new_id(), File::Proc(open), kept, POLLIN | POLLOUT)
}

fn reopen(opened: &Opened, flags: u32) -> Result<i64, i64> {
    let Opened::Server(File::Pipe(end)) = opened else { return Err(ENXIO) };
    if flags & O_DIRECTORY != 0 {
        return Err(ENOTDIR);
    }
    let new = crate::pipe::reopen(end, flags);
    let kept = flags & (O_ACCMODE | files::O_NONBLOCK | files::O_CLOEXEC);
    files::install(new.id(), File::Pipe(new.clone()), kept, new.readiness()).inspect_err(|_| new.close())
}

/// The calls on an open file of /proc or /sys (`a1`..`a3`: the call's
/// arguments after the descriptor).
pub fn call(nr: u64, f: &ProcOpen, flags: u32, a1: u64, a2: u64, a3: u64) -> Result<i64, i64> {
    let readable = flags & O_ACCMODE != O_WRONLY;
    match nr {
        files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if !readable => Err(EBADF),
        files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if f.dir => Err(EISDIR),
        files::SYS_READ => f.read_at_offset(&[(a1, a2)]),
        files::SYS_READV => f.read_at_offset(&files::iovecs(a1, a2)?),
        files::SYS_PREAD64 => f.read(&[(a1, a2)], signed(a3)?),
        files::SYS_PREADV => f.read(&files::iovecs(a1, a2)?, signed(a3)?),
        files::SYS_WRITE | files::SYS_WRITEV | files::SYS_PWRITE64 | files::SYS_PWRITEV => Err(EBADF),
        files::SYS_LSEEK => f.lseek(a1 as i64, a2),
        files::SYS_FSTAT => usercopy::to_program(a1, &procfs::stat(&f.node)?).map(|_| 0),
        files::SYS_FTRUNCATE => Err(EINVAL),
        // Nothing to write back.
        files::SYS_FSYNC | files::SYS_FDATASYNC => Ok(0),
        files::SYS_GETDENTS64 => f.getdents(a1, a2),
        files::SYS_FSTATFS => usercopy::to_program(a1, &procfs::statfs(&f.node)).map(|_| 0),
        files::SYS_IOCTL => Err(files::ENOTTY),
        _ => Err(EINVAL),
    }
}

fn signed(offset: u64) -> Result<u64, i64> {
    if (offset as i64) < 0 { Err(EINVAL) } else { Ok(offset) }
}

impl ProcOpen {
    /// The contents to read at `offset`: made now for a read from the
    /// start (or the first read), else those kept.
    fn snapshot(&self, offset: u64) -> Result<Arc<Vec<u8>>, i64> {
        if offset != 0 {
            if let Some(c) = self.contents.lock().clone() {
                return Ok(c);
            }
        }
        let made = Arc::new(procfs::contents(&self.node)?);
        *self.contents.lock() = Some(made.clone());
        Ok(made)
    }

    /// Reads into the program's buffers from `offset`.
    fn read(&self, vecs: &[(u64, u64)], offset: u64) -> Result<i64, i64> {
        let data = self.snapshot(offset)?;
        let mut at = offset.min(data.len() as u64) as usize;
        let mut done = 0usize;
        for &(base, len) in vecs {
            let n = (len as usize).min(data.len() - at);
            if n == 0 {
                break;
            }
            match usercopy::to_program(base, &data[at..at + n]) {
                Ok(()) => {}
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
            at += n;
            done += n;
        }
        Ok(done as i64)
    }

    /// read/readv: at the description's offset, which moves by what was
    /// read (its lock serializes the description's reads).
    fn read_at_offset(&self, vecs: &[(u64, u64)]) -> Result<i64, i64> {
        let mut off = self.offset.lock();
        let n = self.read(vecs, *off)?;
        *off += n as u64;
        Ok(n)
    }

    /// Reads into the server's memory (sendfile), at the offset.
    pub fn read_server(&self, buf: &mut [u8]) -> Result<usize, i64> {
        if self.dir {
            return Err(EISDIR);
        }
        let mut off = self.offset.lock();
        let data = self.snapshot(*off)?;
        let at = (*off).min(data.len() as u64) as usize;
        let n = buf.len().min(data.len() - at);
        buf[..n].copy_from_slice(&data[at..at + n]);
        *off += n as u64;
        Ok(n)
    }

    /// As Linux's seq_lseek: from the start or the position, not the end
    /// (the size is not known).
    fn lseek(&self, offset: i64, whence: u64) -> Result<i64, i64> {
        let mut off = self.offset.lock();
        let base = match whence {
            0 => 0,
            1 => *off as i64,
            _ => return Err(EINVAL),
        };
        let new = base.checked_add(offset).filter(|&n| n >= 0).ok_or(EINVAL)?;
        *off = new as u64;
        Ok(new)
    }

    /// getdents64: the entries from the offset (an index), as many as fit;
    /// a read from the start takes the directory's entries anew.
    fn getdents(&self, buf: u64, len: u64) -> Result<i64, i64> {
        if !self.dir {
            return Err(ENOTDIR);
        }
        let mut off = self.offset.lock();
        let entries = {
            let kept = self.entries.lock().clone();
            match kept {
                Some(e) if *off != 0 => e,
                _ => {
                    let e = Arc::new(procfs::list(&self.node)?);
                    *self.entries.lock() = Some(e.clone());
                    e
                }
            }
        };
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
}
