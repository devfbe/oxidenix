//! Open files of /proc and /sys (I/O rings step 5): what an open file
//! description of the server's names (the node, its offset and a snapshot
//! of its contents); the calls on it are
//! the server's (`procfs` makes the contents).
//!
//! As Linux's seq_file: a read from offset 0 makes the contents anew and
//! keeps them; reads further on continue in what was kept (made now if
//! nothing was), so a file read in pieces is consistent, and a program that
//! reads it again from the start (top, htop: `pread(fd, buf, n, 0)`) sees
//! it current. A directory likewise: its entries are taken when it is read
//! from the start. A process's `mem` is no snapshot: it reads and writes the
//! process's memory at the position (`ptrace::mem_rw`, ADR 0019). Files are
//! read-only (open for writing: EACCES) but a process's mem, its
//! oom_score_adj and oom_adj and, for a tree with the host grant,
//! /proc/sys/vm's overcommit files and /sys/fs/cgroup's memory boundaries
//! and cgroup.procs (`procfs::may_write`): each write to the
//! latter is one value (`procfs::write`) wherever the offset is, as Linux's
//! for them, and `O_TRUNC` changes nothing there. The copies to the program are made with
//! no lock of the server held but the description's offset (which only its
//! own calls take, never the pager).

use crate::files::{self, File, EBADF, EINVAL, O_ACCMODE, O_WRONLY};
use crate::heap::{try_arc, try_vec};
use crate::procfs::{self, Opened, ProcNode};
use crate::sync::Mutex;
use crate::usercopy;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const ENXIO: i64 = 6;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const O_CREAT: u32 = 0o100;
const O_TRUNC: u32 = 0o1000;
const O_DIRECTORY: u32 = 0o200000;
/// The most one write to a /proc file takes (a page).
const WRITE_MAX: usize = 4096;

pub struct ProcOpen {
    pub node: ProcNode,
    /// The path it was opened by (for *at calls and fchdir).
    pub path: String,
    dir: bool,
    /// The file position; for a directory, the index of the next entry.
    offset: crate::sync::Position,
    /// The contents (a file) as of the last read from the start.
    contents: Mutex<Option<Arc<Vec<u8>>>>,
    /// The entries (a directory) as of the last read from the start.
    entries: Mutex<Option<Arc<Vec<(String, u64, u8)>>>>,
    /// /proc/<pid>/mem's: the kernel's handle on the address space the process had when
    /// it was opened (`ptrace::mem_open`; 0 for other files).
    space: u64,
}

impl Drop for ProcOpen {
    fn drop(&mut self) {
        if self.space != 0 {
            crate::syscall(restricted::SYS_HANDLE_CLOSE, [self.space, 0, 0, 0, 0, 0]);
        }
    }
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
        // (Only what may be written: EACCES for the rest.)
        procfs::may_write(&node)?;
    }
    // It must exist now (a process may have ended since the walk), and be
    // the caller's to open.
    procfs::may_open(&node)?;
    // mem reads the address space the process has now, also after its exec (as Linux's).
    let space = match &node {
        ProcNode::Pid { pid: id, file: procfs::PidFile::Mem } | ProcNode::Task { tid: id, file: procfs::PidFile::Mem, .. } => {
            crate::ptrace::mem_open(*id as crate::process::Pid).map_err(|e| if e == crate::process::ESRCH { crate::namespace::ENOENT } else { e })?
        }
        _ => 0,
    };
    let open = ProcOpen { node, path, dir, offset: crate::sync::Position::new(), contents: Mutex::new(None), entries: Mutex::new(None), space };
    let open = try_arc(open)?;
    let kept = files::status_flags(flags);
    files::install(files::new_id(), File::Proc(open), kept)
}

fn reopen(opened: &Opened, flags: u32) -> Result<i64, i64> {
    let Opened::Pipe(pipe) = opened else { return Err(ENXIO) };
    if flags & O_DIRECTORY != 0 {
        return Err(ENOTDIR);
    }
    let new = pipe.reopen(flags)?;
    let kept = files::status_flags(flags);
    match files::install(new.id(), File::Pipe(new.clone()), kept) {
        Ok(fd) => {
            new.installed();
            Ok(fd)
        }
        Err(e) => {
            new.close();
            Err(e)
        }
    }
}

/// The calls on an open file of /proc or /sys (`a1`..`a3`: the call's
/// arguments after the descriptor).
pub fn call(nr: u64, f: &ProcOpen, flags: u32, a1: u64, a2: u64, a3: u64) -> Result<i64, i64> {
    let readable = flags & O_ACCMODE != O_WRONLY;
    if f.space != 0 {
        if let Some(result) = f.mem_call(nr, readable, flags & O_ACCMODE != 0, a1, a2, a3) {
            return result;
        }
    }
    match nr {
        files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if !readable => Err(EBADF),
        files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if f.dir => Err(EISDIR),
        files::SYS_READ => f.read_at_offset(&[(a1, a2)]),
        files::SYS_READV => f.read_at_offset(&files::iovecs(a1, a2)?),
        files::SYS_PREAD64 => f.read(&[(a1, a2)], signed(a3)?),
        files::SYS_PREADV => f.read(&files::iovecs(a1, a2)?, signed(a3)?),
        files::SYS_WRITE | files::SYS_WRITEV | files::SYS_PWRITE64 | files::SYS_PWRITEV if flags & O_ACCMODE == 0 => Err(EBADF),
        files::SYS_WRITE | files::SYS_PWRITE64 => f.write(&[(a1, a2)]),
        files::SYS_WRITEV | files::SYS_PWRITEV => f.write(&files::iovecs(a1, a2)?),
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
    /// /proc/<pid>/mem's reads, writes and seeks (Linux's mem_rw and mem_lseek): at the
    /// position, an address in the process, which may take any value; through the space
    /// it was opened on (`ptrace::mem_rw`). None for the calls it shares with other files.
    fn mem_call(&self, nr: u64, readable: bool, writable: bool, a1: u64, a2: u64, a3: u64) -> Option<Result<i64, i64>> {
        let at_offset = |vecs: &[(u64, u64)], write: bool| -> Result<i64, i64> {
            let mut off = self.offset.lock()?;
            let n = crate::ptrace::mem_rw(self.space, *off, vecs, write)?;
            *off = off.wrapping_add(n as u64);
            Ok(n)
        };
        Some(match nr {
            files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if !readable => Err(EBADF),
            files::SYS_WRITE | files::SYS_WRITEV | files::SYS_PWRITE64 | files::SYS_PWRITEV if !writable => Err(EBADF),
            files::SYS_READ => at_offset(&[(a1, a2)], false),
            files::SYS_WRITE => at_offset(&[(a1, a2)], true),
            files::SYS_READV => files::iovecs(a1, a2).and_then(|v| at_offset(&v, false)),
            files::SYS_WRITEV => files::iovecs(a1, a2).and_then(|v| at_offset(&v, true)),
            files::SYS_PREAD64 => crate::ptrace::mem_rw(self.space, a3, &[(a1, a2)], false),
            files::SYS_PWRITE64 => crate::ptrace::mem_rw(self.space, a3, &[(a1, a2)], true),
            files::SYS_PREADV => files::iovecs(a1, a2).and_then(|v| crate::ptrace::mem_rw(self.space, a3, &v, false)),
            files::SYS_PWRITEV => files::iovecs(a1, a2).and_then(|v| crate::ptrace::mem_rw(self.space, a3, &v, true)),
            files::SYS_LSEEK => (|| {
                let mut off = self.offset.lock()?;
                *off = match a2 {
                    0 => a1,
                    1 => off.wrapping_add(a1),
                    _ => return Err(EINVAL),
                };
                Ok(*off as i64)
            })(),
            _ => return None,
        })
    }

    /// The contents to read at `offset`: made now for a read from the
    /// start (or the first read), else those kept.
    fn snapshot(&self, offset: u64) -> Result<Arc<Vec<u8>>, i64> {
        if offset != 0 {
            if let Some(c) = self.contents.lock().clone() {
                return Ok(c);
            }
        }
        let made = try_arc(procfs::contents(&self.node)?)?;
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

    /// write and its kin: the bytes at `vecs` (at most `WRITE_MAX` of them,
    /// what Linux's handlers of these files take at most) as one value
    /// (`procfs::write`); how many it took. The offset stays.
    fn write(&self, vecs: &[(u64, u64)]) -> Result<i64, i64> {
        let mut buf = [0u8; WRITE_MAX];
        let mut at = 0;
        for &(addr, len) in vecs {
            let n = (len as usize).min(WRITE_MAX - at);
            usercopy::from_program(addr, &mut buf[at..at + n])?;
            at += n;
        }
        Ok(procfs::write(&self.node, &buf[..at])? as i64)
    }

    /// read/readv: at the description's offset, which moves by what was
    /// read (its lock serializes the description's reads).
    fn read_at_offset(&self, vecs: &[(u64, u64)]) -> Result<i64, i64> {
        let mut off = self.offset.lock()?;
        let n = self.read(vecs, *off)?;
        *off += n as u64;
        Ok(n)
    }

    /// Reads into the server's memory (sendfile), at the offset.
    pub fn read_server(&self, buf: &mut [u8]) -> Result<usize, i64> {
        if self.dir {
            return Err(EISDIR);
        }
        let mut off = self.offset.lock()?;
        let data = self.snapshot(*off)?;
        let at = (*off).min(data.len() as u64) as usize;
        let n = buf.len().min(data.len() - at);
        buf[..n].copy_from_slice(&data[at..at + n]);
        *off += n as u64;
        Ok(n)
    }

    /// Its file position as the last call that moved it left it (/proc/<pid>/fdinfo's
    /// `pos`), without its lock: also of the description whose read asks.
    pub fn shown_position(&self) -> u64 {
        self.offset.get()
    }

    /// As Linux's seq_lseek: from the start or the position, not the end
    /// (the size is not known).
    fn lseek(&self, offset: i64, whence: u64) -> Result<i64, i64> {
        let mut off = self.offset.lock()?;
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
        let mut off = self.offset.lock()?;
        let entries = {
            let kept = self.entries.lock().clone();
            match kept {
                Some(e) if *off != 0 => e,
                _ => {
                    let e = try_arc(procfs::list(&self.node)?)?;
                    *self.entries.lock() = Some(e.clone());
                    e
                }
            }
        };
        // At most `GETDENTS_MAX` bytes a call, reserved first: the program's buffer size
        // sets no allocation.
        let room = (len as usize).min(files::GETDENTS_MAX);
        let mut out: Vec<u8> = try_vec(room)?;
        let mut next = *off;
        while let Some((name, ino, dtype)) = entries.get(next as usize) {
            let reclen = (19 + name.len() + 1).next_multiple_of(8);
            if out.len() + reclen > room {
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
