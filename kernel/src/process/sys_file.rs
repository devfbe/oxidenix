//! File syscalls: on the kernel's descriptor tables (the native servers'), and
//! on an open file the Linux server holds by handle (`file_call`, its
//! `restricted::SYS_KFILE_CALL`: the inodes of the kernel's tree a Linux program
//! opened; its descriptor table is the server's since R6e).

use super::errno::*;
use super::{current_files, uaccess, with_current, FdEntry};
use crate::fs::file::*;
use crate::fs::{self, Inode, NewNode};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EMPTY_PATH: u64 = 0x1000;
const S_IFIFO: u32 = 0o010000;
const UMASK: u32 = 0o022;

/// The open file behind `fd`.
pub fn file(fd: u64) -> Result<Arc<OpenFile>, i64> {
    with_current(|p| p.file(fd))
}

/// Linux's call `nr` on the open file `f`, with the arguments that follow
/// the descriptor (`restricted::SYS_KFILE_CALL`; fstat and fstatfs, which
/// answer into the server's memory, are the caller's).
pub fn file_call(f: &Arc<OpenFile>, nr: u64, a: [u64; 4]) -> SysResult {
    const F_SETFL: u64 = 4;
    match nr {
        0 => Ok(read_file(f, a[0], a[1])? as i64),
        1 => Ok(write_file(f, a[0], a[1])? as i64),
        17 if (a[2] as i64) < 0 => Err(EINVAL),
        17 => read_vecs(f, &[(a[0], a[1])], a[2] as i64, 0),
        18 if (a[2] as i64) < 0 => Err(EINVAL),
        18 => write_vecs(f, &[(a[0], a[1])], a[2] as i64, 0),
        19 => vectored(&iovecs(a[0], a[1])?, |base, len, _| read_file(f, base, len)),
        20 => vectored(&iovecs(a[0], a[1])?, |base, len, _| write_file(f, base, len)),
        295 if (a[2] as i64) < 0 => Err(EINVAL),
        295 => read_vecs(f, &iovecs(a[0], a[1])?, a[2] as i64, 0),
        296 if (a[2] as i64) < 0 => Err(EINVAL),
        296 => write_vecs(f, &iovecs(a[0], a[1])?, a[2] as i64, 0),
        327 => read_vecs(f, &iovecs(a[0], a[1])?, a[2] as i64, a[3]),
        328 => write_vecs(f, &iovecs(a[0], a[1])?, a[2] as i64, a[3]),
        8 => lseek_file(f, a[0] as i64, a[1]),
        217 => getdents_file(f, a[0], a[1]),
        // The kernel's files take no requests of their own.
        16 => Err(ENOTTY),
        74 | 75 => Ok(0),
        77 => ftruncate_file(f, a[0]),
        72 if a[0] == F_SETFL => {
            set_status_flags(f, a[1] as u32);
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// F_SETFL: O_APPEND and O_NONBLOCK change, atomically (another thread may
/// change the same open file's flags at once).
fn set_status_flags(f: &OpenFile, flags: u32) {
    let changeable = O_APPEND | O_NONBLOCK;
    let _ = f.flags.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| Some(old & !changeable | flags & changeable));
}

/// Directory against which a relative path of an *at syscall is resolved.
fn base_dir(dirfd: u64, path: &str) -> Result<String, i64> {
    if path.starts_with('/') || dirfd as i64 == AT_FDCWD {
        return Ok(with_current(|p| p.cwd()));
    }
    file(dirfd)?.path.clone().ok_or(ENOTDIR)
}

fn resolve_at(dirfd: u64, path: &str, follow: bool) -> Result<(Arc<Inode>, String), i64> {
    let base = base_dir(dirfd, path)?;
    let inode = fs::resolve(&base, path, follow)?;
    Ok((inode, fs::join(&fs::normalize(&base, path))))
}

/// Whether a read of `f` that filled a whole buffer may go on with the
/// next one: for files and memory devices, which never block. Pipes and
/// sockets answer with what they have, and asking again could block.
fn reads_on(f: &OpenFile) -> bool {
    f.inode().is_some()
}

fn read_file(f: &OpenFile, buf: u64, len: u64) -> Result<usize, i64> {
    uaccess::read_to_user(buf, len, reads_on(f), |chunk, _| f.read(chunk))
}

fn write_file(f: &OpenFile, buf: u64, len: u64) -> Result<usize, i64> {
    uaccess::write_from_user(buf, len, |chunk, _| f.write(chunk))
}

pub fn read(fd: u64, buf: u64, len: u64) -> SysResult {
    let f = file(fd)?;
    Ok(read_file(&f, buf, len)? as i64)
}

pub fn write(fd: u64, buf: u64, len: u64) -> SysResult {
    let f = file(fd)?;
    Ok(write_file(&f, buf, len)? as i64)
}

pub fn pread(fd: u64, buf: u64, len: u64, off: i64) -> SysResult {
    // Only preadv2 takes -1 for the file position.
    if off < 0 {
        return Err(EINVAL);
    }
    read_vecs(&file(fd)?, &[(buf, len)], off, 0)
}

pub fn pwrite(fd: u64, buf: u64, len: u64, off: i64) -> SysResult {
    if off < 0 {
        return Err(EINVAL);
    }
    write_vecs(&file(fd)?, &[(buf, len)], off, 0)
}

pub fn preadv(fd: u64, iov: u64, count: u64, off: i64) -> SysResult {
    if off < 0 {
        return Err(EINVAL);
    }
    read_vecs(&file(fd)?, &iovecs(iov, count)?, off, 0)
}

pub fn pwritev(fd: u64, iov: u64, count: u64, off: i64) -> SysResult {
    if off < 0 {
        return Err(EINVAL);
    }
    write_vecs(&file(fd)?, &iovecs(iov, count)?, off, 0)
}

fn iovecs(iov: u64, count: u64) -> Result<Vec<(u64, u64)>, i64> {
    if count > 1024 {
        return Err(EINVAL);
    }
    (0..count).map(|i| uaccess::read::<(u64, u64)>(iov + i * 16)).collect()
}

/// Moves the buffers one after the other with `one(base, len, done)`
/// (`done`: bytes moved before this buffer) until one comes up short; an
/// error after some bytes ends the call with them.
fn vectored(vecs: &[(u64, u64)], mut one: impl FnMut(u64, u64, u64) -> Result<usize, i64>) -> SysResult {
    let mut total = 0u64;
    for &(base, len) in vecs {
        let n = match one(base, len, total) {
            Ok(n) => n,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        };
        total += n as u64;
        if n < len as usize {
            break;
        }
    }
    Ok(total as i64)
}

pub fn readv(fd: u64, iov: u64, count: u64) -> SysResult {
    let f = file(fd)?;
    vectored(&iovecs(iov, count)?, |base, len, _| read_file(&f, base, len))
}

pub fn writev(fd: u64, iov: u64, count: u64) -> SysResult {
    let f = file(fd)?;
    vectored(&iovecs(iov, count)?, |base, len, _| write_file(&f, base, len))
}

/// preadv2 (and the other reads at an offset): at `off`, or at the file
/// position for -1 (`vfs::rw::plan`).
pub fn preadv2(fd: u64, iov: u64, count: u64, off: i64, flags: u64) -> SysResult {
    read_vecs(&file(fd)?, &iovecs(iov, count)?, off, flags)
}

/// pwritev2 (and the other writes at an offset): at `off` or the file
/// position, appending as O_APPEND and the flags say, durable before it
/// returns with RWF_DSYNC/RWF_SYNC.
pub fn pwritev2(fd: u64, iov: u64, count: u64, off: i64, flags: u64) -> SysResult {
    write_vecs(&file(fd)?, &iovecs(iov, count)?, off, flags)
}

fn read_vecs(f: &Arc<OpenFile>, vecs: &[(u64, u64)], off: i64, flags: u64) -> SysResult {
    // Access first, as Linux checks it before the flags.
    if !f.readable() {
        return Err(EBADF);
    }
    let plan = vfs::rw::plan(false, off, flags, f.appends())?;
    match plan.at {
        None => vectored(vecs, |base, len, _| read_file(f, base, len)),
        Some(at) => vectored(vecs, |base, len, done| {
            uaccess::read_to_user(base, len, true, |chunk, within| f.read_at(at + done + within, chunk))
        }),
    }
}

fn write_vecs(f: &Arc<OpenFile>, vecs: &[(u64, u64)], off: i64, flags: u64) -> SysResult {
    if !f.writable() {
        return Err(EBADF);
    }
    let plan = vfs::rw::plan(true, off, flags, f.appends())?;
    let n = match plan.at {
        None => vectored(vecs, |base, len, _| uaccess::write_from_user(base, len, |chunk, _| f.write_appending(chunk, plan.append)))?,
        Some(_) if plan.append => vectored(vecs, |base, len, _| uaccess::write_from_user(base, len, |chunk, _| f.write_end(chunk)))?,
        Some(at) => vectored(vecs, |base, len, done| {
            uaccess::write_from_user(base, len, |chunk, within| f.write_at(at + done + within, chunk))
        })?,
    };
    // (RWF_DSYNC, RWF_SYNC: the kernel's files are memory or generated,
    // nothing to wait for.)
    Ok(n)
}

pub fn openat(dirfd: u64, path: u64, flags: u64, mode: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let flags = flags as u32;
    let base = base_dir(dirfd, &path)?;
    let inode = match fs::resolve(&base, &path, flags & O_NOFOLLOW == 0) {
        Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(EEXIST),
        Ok(inode) => inode,
        Err(ENOENT) if flags & O_CREAT != 0 => {
            let (dir, name) = fs::resolve_parent(&base, &path)?;
            dir.create(&name, NewNode::File, mode as u32 & !UMASK)?
        }
        Err(e) => return Err(e),
    };
    // Opening a symlink itself would hand out its target bytes as a file.
    if flags & O_NOFOLLOW != 0 && inode.file_type() == fs::S_IFLNK {
        return Err(ELOOP);
    }
    open_inode(inode, flags, fs::join(&fs::normalize(&base, &path)))
}

/// A descriptor for a resolved inode, as open(2) makes it: `abs` is its
/// absolute path.
pub fn open_inode(inode: Arc<Inode>, flags: u32, abs: String) -> SysResult {
    let f = open_inode_file(inode, flags, abs)?;
    with_current(|p| p.alloc_fd(f, flags & O_CLOEXEC != 0, 0))
}

/// An open file description of a resolved inode, as open(2) makes it
/// (also for the Linux server's `inode_open`): `abs` is its absolute path.
pub fn open_inode_file(inode: Arc<Inode>, flags: u32, abs: String) -> Result<Arc<OpenFile>, i64> {
    // The Linux server's devices (its terminals) are its to open (with
    // O_PATH too: it opens device nodes alone itself).
    if matches!(inode.device(), Some(fs::Device::Server(..))) {
        return Err(ENXIO);
    }
    let writable = flags & O_ACCMODE != 0;
    if inode.is_dir() && writable {
        return Err(EISDIR);
    }
    if flags & O_DIRECTORY != 0 && !inode.is_dir() {
        return Err(ENOTDIR);
    }
    // Not while the file runs as a program (ETXTBSY).
    let access = if writable && inode.file_type() == fs::S_IFREG { Some(inode.get_write_access()?) } else { None };
    if flags & O_TRUNC != 0 && access.is_some() {
        inode.truncate(0)?;
    }
    Ok(OpenFile::inode_file(inode, flags, abs, access))
}

pub fn close(fd: u64) -> SysResult {
    let old = current_files()?.take(fd);
    // Dropped only here: may wake the other end of a pipe.
    old.ok_or(EBADF).map(|_| 0)
}

/// A `struct stat` as Linux lays it out.
fn stat_bytes(ino: u64, mode: u32, rdev: u64, size: u64, extra: (u64, u64, u64, u64)) -> [u8; 144] {
    let (nlink, atime, mtime, ctime) = extra;
    let mut st = [0u8; 144];
    st[8..16].copy_from_slice(&ino.to_le_bytes());
    st[16..24].copy_from_slice(&nlink.to_le_bytes());
    st[24..28].copy_from_slice(&mode.to_le_bytes());
    st[40..48].copy_from_slice(&rdev.to_le_bytes());
    st[72..80].copy_from_slice(&atime.to_le_bytes());
    st[88..96].copy_from_slice(&mtime.to_le_bytes());
    st[104..112].copy_from_slice(&ctime.to_le_bytes());
    st[48..56].copy_from_slice(&size.to_le_bytes());
    st[56..64].copy_from_slice(&4096u64.to_le_bytes()); // blksize
    st[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes()); // blocks
    st
}

/// An inode's `struct stat`.
pub fn inode_stat(inode: &Inode) -> Result<[u8; 144], i64> {
    let s = inode.stat()?;
    Ok(stat_bytes(inode.ino, s.mode, s.rdev, s.size, (s.nlink, s.atime, s.mtime, s.ctime)))
}

fn stat_inode(inode: &Inode, buf: u64) -> SysResult {
    uaccess::write(buf, inode_stat(inode)?)?;
    Ok(0)
}

pub fn fstat(fd: u64, buf: u64) -> SysResult {
    uaccess::write(buf, fstat_bytes(fd)?)?;
    Ok(0)
}

/// The `struct stat` of descriptor `fd` (fstat's answer).
pub fn fstat_bytes(fd: u64) -> Result<[u8; 144], i64> {
    file_stat(&*file(fd)?)
}

/// The `struct stat` of the open file `f`.
pub fn file_stat(f: &OpenFile) -> Result<[u8; 144], i64> {
    let anon = |mode: u32| Ok(stat_bytes(f.number, mode, 0, 0, (1, 0, 0, 0)));
    match f.inode() {
        Some(inode) => inode_stat(inode),
        // An anonymous inode, as on Linux: no file type.
        None if matches!(f.kind, Kind::EventFd(_)) => anon(0o600),
        None => anon(S_IFIFO | 0o600),
    }
}

pub fn newfstatat(dirfd: u64, path: u64, buf: u64, flags: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return fstat(dirfd, buf);
    }
    let (inode, _) = resolve_at(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    stat_inode(&inode, buf)
}

pub fn lseek(fd: u64, offset: i64, whence: u64) -> SysResult {
    lseek_file(&*file(fd)?, offset, whence)
}

fn lseek_file(f: &OpenFile, offset: i64, whence: u64) -> SysResult {
    let inode = f.inode().ok_or(ESPIPE)?;
    if inode.device() == Some(fs::Device::Console) {
        return Err(ESPIPE);
    }
    let mut off = f.offset.lock();
    let base = match whence {
        0 => 0,
        1 => *off as i64,
        2 => inode.size() as i64,
        _ => return Err(EINVAL),
    };
    let new = base.checked_add(offset).filter(|&n| n >= 0).ok_or(EINVAL)?;
    *off = new as u64;
    Ok(new)
}

pub fn getdents64(fd: u64, buf: u64, len: u64) -> SysResult {
    getdents_file(&*file(fd)?, buf, len)
}

fn getdents_file(f: &OpenFile, buf: u64, len: u64) -> SysResult {
    let inode = f.inode().ok_or(ENOTDIR)?;
    let mut snapshot = f.dir_snapshot.lock();
    let off = f.offset.lock();
    if *off == 0 || snapshot.is_none() {
        *snapshot = Some(inode.list()?);
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
    let start = *off;
    drop(off);
    drop(snapshot);
    // No lock is held while user memory is written (a fault may sleep);
    // the position moves only once the entries reached the caller.
    uaccess::copy_to(buf, &out)?;
    let mut off = f.offset.lock();
    if *off == start {
        *off = next;
    }
    Ok(out.len() as i64)
}

pub fn pipe2(fds: u64, flags: u64) -> SysResult {
    let (r, w) = OpenFile::pipe()?;
    let nonblock = flags as u32 & O_NONBLOCK;
    r.flags.fetch_or(nonblock, Ordering::Relaxed);
    w.flags.fetch_or(nonblock, Ordering::Relaxed);
    let cloexec = flags as u32 & O_CLOEXEC != 0;
    let (rfd, wfd) = with_current(|p| -> Result<_, i64> {
        let rfd = p.alloc_fd(r, cloexec, 0)?;
        Ok((rfd, p.alloc_fd(w, cloexec, 0)?))
    })?;
    uaccess::write(fds, [rfd as i32, wfd as i32])?;
    Ok(0)
}

/// eventfd2(initval, flags), and eventfd (flags 0).
pub fn eventfd2(initval: u64, flags: u64) -> SysResult {
    const EFD_SEMAPHORE: u64 = 1;
    if flags & !(EFD_SEMAPHORE | (O_NONBLOCK | O_CLOEXEC) as u64) != 0 {
        return Err(EINVAL);
    }
    let file = OpenFile::eventfd(initval as u32 as u64, flags & EFD_SEMAPHORE != 0, flags as u32 & O_NONBLOCK);
    with_current(|p| p.alloc_fd(file, flags as u32 & O_CLOEXEC != 0, 0))
}

pub fn dup(fd: u64) -> SysResult {
    let f = file(fd)?;
    with_current(|p| p.alloc_fd(f, false, 0))
}

pub fn dup3(old: u64, new: u64, flags: u64, allow_same: bool) -> SysResult {
    let f = file(old)?;
    if old == new {
        return if allow_same { Ok(new as i64) } else { Err(EINVAL) };
    }
    if new >= 256 {
        return Err(EBADF);
    }
    let cloexec = flags as u32 & O_CLOEXEC != 0;
    let replaced = current_files()?.replace(new, FdEntry::new(f, cloexec))?;
    // Closed only here, after the table's lock.
    drop(replaced);
    Ok(new as i64)
}

pub fn fcntl(fd: u64, cmd: u64, arg: u64) -> SysResult {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    const FD_CLOEXEC: u64 = 1;
    let f = file(fd)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => with_current(|p| p.alloc_fd(f, cmd == F_DUPFD_CLOEXEC, arg as usize)),
        F_GETFD => Ok(if current_files()?.cloexec(fd)? { FD_CLOEXEC as i64 } else { 0 }),
        F_SETFD => current_files()?.set_cloexec(fd, arg & FD_CLOEXEC != 0).map(|_| 0),
        F_GETFL => Ok(f.flags.load(Ordering::Relaxed) as i64),
        F_SETFL => {
            set_status_flags(&f, arg as u32);
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// ioctl(2) on the kernel's files: only the requests on the descriptor
/// (Linux's do_vfs_ioctl); the kernel's files take no requests of their own
/// (ENOTTY).
pub fn ioctl(fd: u64, request: u64, arg: u64) -> SysResult {
    const FIONBIO: u64 = 0x5421;
    const FIONCLEX: u64 = 0x5450;
    const FIOCLEX: u64 = 0x5451;
    match request {
        FIONBIO => {
            // The descriptor first: a bad one is EBADF whatever `arg` is.
            let f = file(fd)?;
            if uaccess::read::<i32>(arg)? != 0 {
                f.flags.fetch_or(O_NONBLOCK, Ordering::Relaxed);
            } else {
                f.flags.fetch_and(!O_NONBLOCK, Ordering::Relaxed);
            }
            return Ok(0);
        }
        FIOCLEX | FIONCLEX => {
            file(fd)?;
            return current_files()?.set_cloexec(fd, request == FIOCLEX).map(|_| 0);
        }
        _ => {}
    }
    file(fd)?;
    Err(ENOTTY)
}

pub fn faccessat(dirfd: u64, path: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    resolve_at(dirfd, &path, true).map(|_| 0)
}

pub fn getcwd(buf: u64, size: u64) -> SysResult {
    let cwd = with_current(|p| p.cwd());
    if (size as usize) < cwd.len() + 1 {
        return Err(ERANGE);
    }
    let mut out = cwd.clone().into_bytes();
    out.push(0);
    uaccess::copy_to(buf, &out)?;
    Ok(cwd.len() as i64 + 1)
}

pub fn chdir(path: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, abs) = resolve_at(AT_FDCWD as u64, &path, true)?;
    if !inode.is_dir() {
        return Err(ENOTDIR);
    }
    with_current(|p| p.set_cwd(abs));
    Ok(0)
}

pub fn fchdir(fd: u64) -> SysResult {
    let f = file(fd)?;
    if !f.inode().is_some_and(|i| i.is_dir()) {
        return Err(ENOTDIR);
    }
    let path = f.path.clone().ok_or(ENOTDIR)?;
    with_current(|p| p.set_cwd(path));
    Ok(0)
}

fn create_at(dirfd: u64, path: u64, kind: NewNode, perm: u32) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let base = base_dir(dirfd, &path)?;
    let (dir, name) = fs::resolve_parent(&base, &path)?;
    dir.create(&name, kind, perm)?;
    Ok(0)
}

pub fn mkdirat(dirfd: u64, path: u64, mode: u64) -> SysResult {
    create_at(dirfd, path, NewNode::Dir, mode as u32 & !UMASK)
}

pub fn symlinkat(target: u64, dirfd: u64, path: u64) -> SysResult {
    let target = uaccess::read_cstr(target)?;
    create_at(dirfd, path, NewNode::Symlink(target), 0o777)
}

pub fn unlinkat(dirfd: u64, path: u64, flags: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let base = base_dir(dirfd, &path)?;
    let (dir, name) = fs::resolve_parent(&base, &path)?;
    dir.unlink(&name, flags & AT_REMOVEDIR != 0)?;
    Ok(0)
}

pub fn renameat(olddirfd: u64, oldpath: u64, newdirfd: u64, newpath: u64) -> SysResult {
    let oldpath = uaccess::read_cstr(oldpath)?;
    let newpath = uaccess::read_cstr(newpath)?;
    let (odir, oname) = fs::resolve_parent(&base_dir(olddirfd, &oldpath)?, &oldpath)?;
    let (ndir, nname) = fs::resolve_parent(&base_dir(newdirfd, &newpath)?, &newpath)?;
    fs::rename(&odir, &oname, &ndir, &nname)?;
    Ok(0)
}

pub fn readlinkat(dirfd: u64, path: u64, buf: u64, size: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, _) = resolve_at(dirfd, &path, false)?;
    let target = inode.readlink()?;
    let n = target.len().min(size as usize);
    uaccess::copy_to(buf, &target.as_bytes()[..n])?;
    Ok(n as i64)
}

pub fn fchmodat(dirfd: u64, path: u64, mode: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, _) = resolve_at(dirfd, &path, true)?;
    inode.set_perm(mode as u32)?;
    Ok(0)
}

/// fsync/fdatasync: the kernel's files are memory or generated (disk
/// files are the Linux server's): nothing to write back.
pub fn fsync(fd: u64) -> SysResult {
    file(fd)?;
    Ok(0)
}

/// sync(2): nothing of the kernel's to write back (the Linux server
/// writes its disk files back itself).
pub fn sync() -> SysResult {
    Ok(0)
}

/// truncate(2): like ftruncate on a descriptor opened for writing.
pub fn truncate(path: u64, len: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, _) = resolve_at(AT_FDCWD as u64, &path, true)?;
    if inode.is_dir() {
        return Err(EISDIR);
    }
    let _access = inode.get_write_access()?;
    inode.truncate(len)?;
    Ok(0)
}

pub fn ftruncate(fd: u64, len: u64) -> SysResult {
    ftruncate_file(&*file(fd)?, len)
}

fn ftruncate_file(f: &OpenFile, len: u64) -> SysResult {
    if !f.writable() {
        return Err(EINVAL);
    }
    f.inode().ok_or(EINVAL)?.truncate(len)?;
    Ok(0)
}

pub fn sendfile(out_fd: u64, in_fd: u64, offset: u64, count: u64) -> SysResult {
    if offset != 0 {
        return Err(EINVAL);
    }
    let (out, input) = (file(out_fd)?, file(in_fd)?);
    let mut buf = vec![0u8; 4096];
    let mut total = 0;
    while total < count {
        let want = (count - total).min(buf.len() as u64) as usize;
        let n = input.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        out.write(&buf[..n])?;
        total += n as u64;
        // Like Linux, return after a short read (a terminal line, a pipe
        // chunk) instead of blocking for more; the caller sees EOF as 0.
        if n < want {
            break;
        }
    }
    Ok(total as i64)
}

/// Timestamps are not stored; this only checks that the target exists.
pub fn utimensat(dirfd: u64, path: u64, flags: u64) -> SysResult {
    if path == 0 {
        return file(dirfd).map(|_| 0);
    }
    let path = uaccess::read_cstr(path)?;
    resolve_at(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0).map(|_| 0)
}

/// statfs/fstatfs: a server's filesystem reports its real usage, everything else
/// the in-memory filesystem (tmpfs) and its size limit.
fn write_statfs(inode: &Inode, buf: u64) -> SysResult {
    uaccess::write(buf, statfs_words(inode))?;
    Ok(0)
}

/// The `struct statfs` of an inode's filesystem.
pub fn statfs_words(inode: &Inode) -> [u64; 15] {
    const EXT2_MAGIC: u64 = 0xef53;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    let (kind, bsize, blocks, free, files, ffree) = match inode.filesystem() {
        Some(fs) => {
            let (bs, blocks, free, inodes, free_inodes) = fs.usage();
            (EXT2_MAGIC, bs, blocks, free, inodes, free_inodes)
        }
        None => {
            let (used, limit) = fs::cache::tmpfs_usage();
            (TMPFS_MAGIC, 4096, limit, limit.saturating_sub(used), 0, 0)
        }
    };
    [kind, bsize, blocks, free, free, files, ffree, 0, fs::NAME_MAX as u64, bsize, 0, 0, 0, 0, 0]
}

pub fn statfs(path: u64, buf: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, _) = resolve_at(AT_FDCWD as u64, &path, true)?;
    write_statfs(&inode, buf)
}

pub fn fstatfs(fd: u64, buf: u64) -> SysResult {
    let f = file(fd)?;
    write_statfs(f.inode().ok_or(EINVAL)?, buf)
}
