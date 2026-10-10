//! Open files of /data (phase R6c.3): what an open file description of the
//! server's names (offset, the directory cursor, write access, O_DIRECT and
//! O_SYNC); the calls on it are the server's (`datafs`), as for the
//! server's tmpfs files.

use crate::datafs::{self, DInode, HoldKind, EISDIR};
use crate::files::{self, File, EBADF, EINVAL, O_ACCMODE, O_WRONLY};
use crate::inotify;
use crate::namespace::ENOTDIR;
use crate::usercopy;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
pub const O_DSYNC: u32 = 0o10000;
const O_DIRECT: u32 = 0o40000;
const O_DIRECTORY: u32 = 0o200000;
const O_SYNC: u32 = 0o4010000;

pub struct DataOpen {
    pub inode: Arc<DInode>,
    /// The path it was opened by (for *at calls and fchdir).
    pub path: String,
    /// The file position; for a directory, the index of the next entry.
    offset: crate::sync::SleepMutex<u64>,
    /// A directory's entries, taken when it is read from the start (as
    /// tmpfs's: removing entries while reading skips none).
    snapshot: crate::sync::SleepMutex<Option<(Vec<(u32, u8, Vec<u8>)>, crate::files::SnapshotCharge)>>,
    /// Holds write access (opened for writing).
    write: bool,
    /// O_DIRECT reads come from the disk; O_SYNC and O_DSYNC writes are
    /// durable when they return (both kept here: the kernel's table keeps
    /// only O_APPEND and O_NONBLOCK).
    direct: bool,
    sync: bool,
}

impl Drop for DataOpen {
    fn drop(&mut self) {
        self.notify(if self.write { inotify::IN_CLOSE_WRITE } else { inotify::IN_CLOSE_NOWRITE });
        if self.write {
            datafs::put_write(&self.inode);
        }
        // An unlinked file's last close: it goes.
        // (SeqCst, as the unlink's store of `unlinked` and load of `opens`:
        // one of the two sees the other, the inode's going is never missed.)
        if self.inode.opens.fetch_sub(1, core::sync::atomic::Ordering::SeqCst) == 1 && self.inode.unlinked() {
            inotify::deleted(inotify::Key::data(&self.inode), self.inode.kind == vfs::S_IFDIR);
        }
        datafs::let_go(&self.inode);
    }
}

/// open(2) of a resolved /data inode: a descriptor of the calling process.
pub fn open(inode: Arc<DInode>, flags: u32, path: String) -> Result<i64, i64> {
    const ENXIO: i64 = 6;
    // A socket is connected to, not opened.
    if inode.kind == vfs::S_IFSOCK {
        return Err(ENXIO);
    }
    let writable = flags & O_ACCMODE != 0;
    let dir = inode.kind == vfs::S_IFDIR;
    if dir && writable {
        return Err(EISDIR);
    }
    if flags & O_DIRECTORY != 0 && !dir {
        return Err(ENOTDIR);
    }
    // Not while it runs as a program (ETXTBSY).
    let write = writable && inode.kind == vfs::S_IFREG;
    if write {
        datafs::get_write(&inode)?;
    }
    inode.opens.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    let open = Arc::new(DataOpen {
        inode,
        path,
        offset: crate::sync::SleepMutex::new(0),
        snapshot: crate::sync::SleepMutex::new(None),
        write,
        direct: flags & O_DIRECT != 0,
        sync: flags & O_DSYNC != 0 || flags & O_SYNC == O_SYNC,
    });
    if write && flags & O_TRUNC != 0 {
        datafs::truncate(&open.inode, 0)?;
        open.notify(inotify::IN_MODIFY);
    }
    open.notify(inotify::IN_OPEN);
    let kept = flags & (O_ACCMODE | files::O_NONBLOCK | O_APPEND | files::O_CLOEXEC);
    files::install(files::new_id(), File::Data(open), kept)
}

/// The calls on an open /data file (`a1`..`a3`: the call's arguments after
/// the descriptor). `flags` may carry O_DSYNC for one call (RWF_DSYNC).
pub fn call(nr: u64, f: &DataOpen, flags: u32, a1: u64, a2: u64, a3: u64) -> Result<i64, i64> {
    let readable = flags & O_ACCMODE != O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    let sync = f.sync || flags & O_DSYNC != 0;
    match nr {
        files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if !readable => Err(EBADF),
        files::SYS_WRITE | files::SYS_WRITEV | files::SYS_PWRITE64 | files::SYS_PWRITEV if !writable => Err(EBADF),
        files::SYS_READ | files::SYS_READV | files::SYS_PREAD64 | files::SYS_PREADV if f.inode.kind == vfs::S_IFDIR => Err(EISDIR),
        files::SYS_READ => f.read_at_offset(&[(a1, a2)]),
        files::SYS_READV => f.read_at_offset(&files::iovecs(a1, a2)?),
        files::SYS_PREAD64 => f.read(&[(a1, a2)], signed(a3)?).map(|n| n as i64),
        files::SYS_PREADV => f.read(&files::iovecs(a1, a2)?, signed(a3)?).map(|n| n as i64),
        files::SYS_WRITE => f.write_at_offset(&[(a1, a2)], flags & O_APPEND != 0, sync),
        files::SYS_WRITEV => f.write_at_offset(&files::iovecs(a1, a2)?, flags & O_APPEND != 0, sync),
        files::SYS_PWRITE64 => f.write_positional(&[(a1, a2)], signed(a3)?, flags & O_APPEND != 0, sync),
        files::SYS_PWRITEV => f.write_positional(&files::iovecs(a1, a2)?, signed(a3)?, flags & O_APPEND != 0, sync),
        files::SYS_LSEEK => f.lseek(a1 as i64, a2),
        files::SYS_FSTAT => {
            usercopy::to_program(a1, &datafs::stat(&f.inode)?)?;
            Ok(0)
        }
        files::SYS_FTRUNCATE if !writable || f.inode.kind != vfs::S_IFREG => Err(EINVAL),
        files::SYS_FTRUNCATE => {
            datafs::truncate(&f.inode, a1)?;
            f.notify(inotify::IN_MODIFY);
            Ok(0)
        }
        files::SYS_FSYNC | files::SYS_FDATASYNC => datafs::fsync(&f.inode).map(|_| 0),
        files::SYS_GETDENTS64 => f.getdents(a1, a2),
        files::SYS_FSTATFS => {
            usercopy::to_program(a1, &datafs::statfs()?)?;
            Ok(0)
        }
        files::SYS_IOCTL => Err(files::ENOTTY),
        _ => Err(EINVAL),
    }
}

fn signed(offset: u64) -> Result<u64, i64> {
    if (offset as i64) < 0 { Err(EINVAL) } else { Ok(offset) }
}

impl DataOpen {
    /// inotify's event `mask` for the file and its directory.
    fn notify(&self, mask: u32) {
        inotify::data_event(&self.inode, mask);
    }

    /// Reads into the program's buffers from `offset`: as far as the file
    /// goes. An error after some bytes ends the read with them.
    fn read(&self, vecs: &[(u64, u64)], offset: u64) -> Result<u64, i64> {
        let mut done = 0u64;
        for &(base, len) in vecs {
            if len == 0 {
                continue;
            }
            let at = offset.checked_add(done).ok_or(EINVAL)?;
            let r = if self.direct { datafs::read_direct(&self.inode, at, base, len) } else { datafs::read(&self.inode, at, base, len) };
            match r {
                Ok(n) => {
                    done += n;
                    if n < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        self.notify(inotify::IN_ACCESS);
        Ok(done)
    }

    fn write(&self, vecs: &[(u64, u64)], offset: u64, sync: bool) -> Result<u64, i64> {
        let mut done = 0u64;
        for &(base, len) in vecs {
            if len == 0 {
                continue;
            }
            let at = offset.checked_add(done).ok_or(datafs::EFBIG)?;
            match datafs::write(&self.inode, at, base, len) {
                Ok(n) => {
                    done += n;
                    if n < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        if done > 0 {
            self.notify(inotify::IN_MODIFY);
        }
        // O_SYNC, O_DSYNC, RWF_(D)SYNC: durable now; O_DIRECT: on the disk
        // (not flushed), as Linux.
        if done > 0 && (sync || self.direct) {
            let end = offset + done;
            if sync {
                datafs::fsync_range(&self.inode, offset, end)?;
            } else {
                datafs::writeback(&self.inode, offset / 4096..end.div_ceil(4096))?;
            }
        }
        Ok(done)
    }

    /// pwrite/pwritev: at `offset`, or with O_APPEND (as on Linux) at the
    /// end; the description's offset stays.
    fn write_positional(&self, vecs: &[(u64, u64)], offset: u64, append: bool, sync: bool) -> Result<i64, i64> {
        let n = if append {
            let _append = self.inode.append.lock()?;
            self.write(vecs, datafs::size(&self.inode)?, sync)?
        } else {
            self.write(vecs, offset, sync)?
        };
        Ok(n as i64)
    }

    /// read/readv: at the description's offset, which moves by what was
    /// read (the offset's lock serializes the description's reads).
    fn read_at_offset(&self, vecs: &[(u64, u64)]) -> Result<i64, i64> {
        let mut off = self.offset.lock()?;
        let n = self.read(vecs, *off)?;
        *off += n;
        Ok(n as i64)
    }

    /// write/writev: at the offset, or with O_APPEND at the end (the
    /// inode's append lock makes finding the end and writing there one
    /// step for every appender).
    fn write_at_offset(&self, vecs: &[(u64, u64)], append: bool, sync: bool) -> Result<i64, i64> {
        let mut off = self.offset.lock()?;
        let n = if append {
            let _append = self.inode.append.lock()?;
            *off = datafs::size(&self.inode)?;
            self.write(vecs, *off, sync)?
        } else {
            self.write(vecs, *off, sync)?
        };
        *off += n;
        Ok(n as i64)
    }

    fn lseek(&self, offset: i64, whence: u64) -> Result<i64, i64> {
        let mut off = self.offset.lock()?;
        let base = match whence {
            0 => 0,
            1 => *off as i64,
            2 if self.inode.kind == vfs::S_IFREG => datafs::size(&self.inode)? as i64,
            2 => 0,
            _ => return Err(EINVAL),
        };
        let new = base.checked_add(offset).filter(|&n| n >= 0).ok_or(EINVAL)?;
        *off = new as u64;
        Ok(new)
    }

    /// getdents64: the entries from the offset (an index), as many as fit;
    /// a read from the start takes a new snapshot of the directory.
    fn getdents(&self, buf: u64, len: u64) -> Result<i64, i64> {
        if self.inode.kind != vfs::S_IFDIR {
            return Err(ENOTDIR);
        }
        let mut off = self.offset.lock()?;
        let mut snapshot = self.snapshot.lock()?;
        if *off == 0 || snapshot.is_none() {
            // (The old one goes first: its charge with it.)
            *snapshot = None;
            let mut charge = crate::files::SnapshotCharge::new();
            let entries = self.list(&mut charge)?;
            *snapshot = Some((entries, charge));
        }
        let entries = &snapshot.as_ref().expect("taken above").0;
        // At most `GETDENTS_MAX` bytes a call (a short getdents64 is a valid one: the program
        // asks again), reserved first: the program's buffer size sets no allocation.
        let room = (len as usize).min(crate::files::GETDENTS_MAX);
        let mut out = Vec::new();
        out.try_reserve_exact(room).map_err(|_| 12i64)?;
        let mut next = *off;
        while let Some((ino, dtype, name)) = entries.get(next as usize) {
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
            rec[0..8].copy_from_slice(&(*ino as u64).to_le_bytes());
            rec[8..16].copy_from_slice(&(next + 1).to_le_bytes());
            rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
            rec[18] = *dtype;
            rec[19..19 + name.len()].copy_from_slice(name);
            next += 1;
        }
        // The position moves only once the entries reached the program.
        usercopy::to_program(buf, &out)?;
        *off = next;
        self.notify(inotify::IN_ACCESS);
        Ok(out.len() as i64)
    }

    /// Every entry of the directory ("." and ".." included), at most
    /// `MAX_ENTRIES` (more than an ext2 directory holds: diskfs's listing
    /// is not trusted to end).
    fn list(&self, charge: &mut crate::files::SnapshotCharge) -> Result<Vec<(u32, u8, Vec<u8>)>, i64> {
        const MAX_ENTRIES: usize = 1 << 20;
        const ENOMEM: i64 = 12;
        let mut all = Vec::new();
        let mut cursor = 0;
        loop {
            let (entries, next) = datafs::readdir(&self.inode, cursor)?;
            charge.add(entries.iter().map(|(_, _, n)| n.len() + 48).sum())?;
            all.try_reserve(entries.len()).map_err(|_| ENOMEM)?;
            all.extend(entries);
            if all.len() > MAX_ENTRIES {
                return Err(ENOMEM);
            }
            if next == 0 {
                return Ok(all);
            }
            cursor = next;
        }
    }

    /// Reads into the server's memory (sendfile), at the offset.
    pub fn read_server(&self, buf: &mut [u8]) -> Result<usize, i64> {
        let mut off = self.offset.lock()?;
        let n = datafs::read_server(&self.inode, *off, buf)?;
        *off += n as u64;
        Ok(n)
    }

    /// Writes from the server's memory (sendfile), at the offset or the end.
    pub fn write_server(&self, data: &[u8], append: bool) -> Result<usize, i64> {
        let mut off = self.offset.lock()?;
        let _append = if append { Some(self.inode.append.lock()?) } else { None };
        if append {
            *off = datafs::size(&self.inode)?;
        }
        let at = *off;
        let n = self.write_at(at, data)?;
        *off += n as u64;
        Ok(n)
    }

    /// The description's file position (for the calls that take one; EINTR if the thread
    /// dies while it waits for it).
    pub fn position(&self) -> Result<crate::sync::SleepMutexGuard<'_, u64>, i64> {
        self.offset.lock()
    }

    /// Reads into the server's memory at `off` (copy_file_range).
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let n = datafs::read_server(&self.inode, off, buf)?;
        self.notify(inotify::IN_ACCESS);
        Ok(n)
    }

    /// Writes the server's memory at `off` (sendfile, copy_file_range):
    /// durable before it returns with O_SYNC or O_DSYNC.
    pub fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        let n = datafs::write_server(&self.inode, off, data)?;
        if n > 0 {
            self.notify(inotify::IN_MODIFY);
        }
        if self.sync {
            datafs::fsync_range(&self.inode, off, off + n as u64)?;
        }
        Ok(n)
    }

    /// The object to map, for mmap: a handle the caller closes once mapped
    /// (it keeps the inode, and write access for a shared mapping through a
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
        if self.inode.kind != vfs::S_IFREG {
            return Err(ENODEV);
        }
        if shared && writable {
            return Ok((datafs::hold(&self.inode, HoldKind::Write)?, false));
        }
        Ok((datafs::hold(&self.inode, HoldKind::Plain)?, shared))
    }
}

/// The right to run a /data file: a held handle for `exec_target`.
pub fn exec_hold(inode: &Arc<DInode>) -> Result<u64, i64> {
    if inode.kind != vfs::S_IFREG {
        const ENOEXEC: i64 = 8;
        const EACCES: i64 = 13;
        return Err(if inode.kind == vfs::S_IFDIR { EACCES } else { ENOEXEC });
    }
    datafs::hold(inode, HoldKind::Run)
}
