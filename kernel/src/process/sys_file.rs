//! File syscalls.

use super::errno::*;
use super::poll::PollTable;
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
const S_IFSOCK: u32 = 0o140000;
const UMASK: u32 = 0o022;

pub fn file(fd: u64) -> Result<Arc<OpenFile>, i64> {
    with_current(|p| p.file(fd))
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
/// next one: for files and memory devices, which never block. Pipes, the
/// terminal and sockets answer with what they have, and asking again could
/// block.
fn reads_on(f: &OpenFile) -> bool {
    f.inode().is_some_and(|i| i.device() != Some(fs::Device::Console))
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
    read_vecs(fd, &[(buf, len)], off, 0)
}

pub fn pwrite(fd: u64, buf: u64, len: u64, off: i64) -> SysResult {
    if off < 0 {
        return Err(EINVAL);
    }
    write_vecs(fd, &[(buf, len)], off, 0)
}

pub fn preadv(fd: u64, iov: u64, count: u64, off: i64) -> SysResult {
    if off < 0 {
        return Err(EINVAL);
    }
    read_vecs(fd, &iovecs(iov, count)?, off, 0)
}

pub fn pwritev(fd: u64, iov: u64, count: u64, off: i64) -> SysResult {
    if off < 0 {
        return Err(EINVAL);
    }
    write_vecs(fd, &iovecs(iov, count)?, off, 0)
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
    read_vecs(fd, &iovecs(iov, count)?, off, flags)
}

/// pwritev2 (and the other writes at an offset): at `off` or the file
/// position, appending as O_APPEND and the flags say, durable before it
/// returns with RWF_DSYNC/RWF_SYNC.
pub fn pwritev2(fd: u64, iov: u64, count: u64, off: i64, flags: u64) -> SysResult {
    write_vecs(fd, &iovecs(iov, count)?, off, flags)
}

fn read_vecs(fd: u64, vecs: &[(u64, u64)], off: i64, flags: u64) -> SysResult {
    let f = file(fd)?;
    // Access first, as Linux checks it before the flags.
    if !f.readable() {
        return Err(EBADF);
    }
    let plan = vfs::rw::plan(false, off, flags, f.appends())?;
    match plan.at {
        None => vectored(vecs, |base, len, _| read_file(&f, base, len)),
        Some(at) => vectored(vecs, |base, len, done| {
            uaccess::read_to_user(base, len, true, |chunk, within| f.read_at(at + done + within, chunk))
        }),
    }
}

fn write_vecs(fd: u64, vecs: &[(u64, u64)], off: i64, flags: u64) -> SysResult {
    let f = file(fd)?;
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
/// absolute path (also for the Linux server's `inode_open`).
pub fn open_inode(inode: Arc<Inode>, flags: u32, abs: String) -> SysResult {
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
    let f = OpenFile::inode_file(inode, flags, abs, access);
    with_current(|p| p.alloc_fd(f, flags & O_CLOEXEC != 0, 0))
}

pub fn close(fd: u64) -> SysResult {
    let old = current_files()?.take(fd);
    // Dropped only here: may wake the other end of a pipe.
    old.ok_or(EBADF).map(|_| 0)
}

/// A `struct stat` as Linux lays it out.
fn stat_bytes(ino: u64, mode: u32, size: u64, extra: (u64, u64, u64, u64)) -> [u8; 144] {
    let (nlink, atime, mtime, ctime) = extra;
    let mut st = [0u8; 144];
    st[8..16].copy_from_slice(&ino.to_le_bytes());
    st[16..24].copy_from_slice(&nlink.to_le_bytes());
    st[24..28].copy_from_slice(&mode.to_le_bytes());
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
    Ok(stat_bytes(inode.ino, s.mode, s.size, (s.nlink, s.atime, s.mtime, s.ctime)))
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
    let f = file(fd)?;
    let anon = |mode: u32| Ok(stat_bytes(Arc::as_ptr(&f) as u64, mode, 0, (1, 0, 0, 0)));
    match f.inode() {
        Some(inode) => inode_stat(inode),
        None if f.socket().is_some() => anon(S_IFSOCK | 0o777),
        // An anonymous inode, as on Linux: no file type.
        None if matches!(f.kind, Kind::EventFd(_) | Kind::Epoll(_)) => anon(0o600),
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
    let f = file(fd)?;
    let inode = f.inode().ok_or(ESPIPE)?;
    if f.is_console() {
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
    let f = file(fd)?;
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
    let replaced = current_files()?.replace(new, FdEntry { file: f, cloexec })?;
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
            // Atomic: another thread may change the same open file's flags
            // at once (FIONBIO, F_SETFL).
            let changeable = O_APPEND | O_NONBLOCK;
            let _ = f.flags.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| Some(old & !changeable | arg as u32 & changeable));
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

pub fn ioctl(fd: u64, request: u64, arg: u64) -> SysResult {
    use crate::drivers::tty;
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const TCSETSW: u64 = 0x5403;
    const TCSETSF: u64 = 0x5404;
    const TCSBRK: u64 = 0x5409;
    const TCXONC: u64 = 0x540a;
    const TCFLSH: u64 = 0x540b;
    const TIOCSCTTY: u64 = 0x540e;
    const TIOCGPGRP: u64 = 0x540f;
    const TIOCSPGRP: u64 = 0x5410;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCSWINSZ: u64 = 0x5414;
    const FIONREAD: u64 = 0x541b;
    const TIOCNOTTY: u64 = 0x5422;
    const TIOCGSID: u64 = 0x5429;

    // The requests on the descriptor rather than the file, which every
    // descriptor takes (Linux's do_vfs_ioctl), the Linux server's files too
    // (it passes them through): libuv makes pipes and sockets non-blocking
    // with FIONBIO. Here only because the descriptor table is still the
    // kernel's; they move into the Linux server with it (R6e,
    // docs/design/linux-server.md).
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

    if !file(fd)?.is_console() {
        return Err(ENOTTY);
    }
    match request {
        TCGETS => uaccess::write(arg, tty::termios())?,
        TCSETS | TCSETSW => tty::set_termios(uaccess::read(arg)?, false),
        TCSETSF => tty::set_termios(uaccess::read(arg)?, true),
        TCFLSH if arg != 1 => tty::flush_input(),
        TCFLSH | TCSBRK | TCXONC | TIOCSCTTY | TIOCNOTTY | TIOCSWINSZ => {}
        TIOCGPGRP => uaccess::write(arg, tty::foreground() as i32)?,
        TIOCSPGRP => tty::set_foreground(uaccess::read::<i32>(arg)? as u64),
        TIOCGSID => uaccess::write(arg, super::getsid(0)? as i32)?,
        TIOCGWINSZ => {
            let (cols, rows) = crate::drivers::console::size();
            uaccess::write(arg, [rows as u16, cols as u16, 0, 0])?;
        }
        FIONREAD => uaccess::write(arg, tty::pending() as i32)?,
        _ => return Err(ENOTTY),
    }
    Ok(0)
}

/// The deadline of a timeout of `ns` nanoseconds (None: wait forever).
fn deadline_in(ns: Option<u64>) -> Option<u64> {
    ns.map(|ns| crate::time::now().saturating_add(ns))
}

/// poll(2): waits on the wait queues of the polled files (see
/// `poll::PollTable`); the timeout (`None`: none) ends the wait exactly.
pub fn poll(fds: u64, nfds: u64, timeout: Option<u64>) -> SysResult {
    const POLLNVAL: i16 = 0x20;
    if nfds > 256 {
        return Err(EINVAL);
    }
    let deadline = deadline_in(timeout);
    let mut table = PollTable::new();
    let mut first = true;
    loop {
        table.rearm();
        let mut ready = 0;
        for i in 0..nfds {
            let entry = fds + i * 8;
            let raw: [u8; 8] = uaccess::read(entry)?;
            let fd = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let events = i16::from_le_bytes([raw[4], raw[5]]);
            let revents = if fd < 0 {
                0
            } else {
                match file(fd as u64) {
                    Ok(f) => {
                        if first {
                            table.watch(&f)?;
                        }
                        f.poll(events)
                    }
                    Err(_) => POLLNVAL,
                }
            };
            uaccess::write(entry + 6, revents)?;
            if revents != 0 {
                ready += 1;
            }
        }
        if ready > 0 || deadline.is_some_and(|d| crate::time::now() >= d) {
            return Ok(ready);
        }
        first = false;
        table.wait(deadline)?;
    }
}

/// Shared core of select and pselect6: a timeout of `None` waits forever.
pub fn select(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: Option<u64>) -> SysResult {
    if nfds > 1024 {
        return Err(EINVAL);
    }
    let words = nfds.div_ceil(64);
    let load = |ptr: u64| -> Result<[u64; 16], i64> {
        let mut set = [0u64; 16];
        for (i, w) in set.iter_mut().enumerate().take(words as usize) {
            if ptr != 0 {
                *w = uaccess::read(ptr + i as u64 * 8)?;
            }
        }
        Ok(set)
    };
    let (want_r, want_w) = (load(readfds)?, load(writefds)?);
    let deadline = deadline_in(timeout);
    let mut table = PollTable::new();
    let mut first = true;
    loop {
        table.rearm();
        let (mut got_r, mut got_w, mut ready) = ([0u64; 16], [0u64; 16], 0);
        for fd in 0..nfds {
            let (w, b) = ((fd / 64) as usize, 1u64 << (fd % 64));
            if (want_r[w] | want_w[w]) & b == 0 {
                continue;
            }
            let f = file(fd)?;
            if first {
                table.watch(&f)?;
            }
            let revents = f.poll(POLLIN | POLLOUT);
            if want_r[w] & b != 0 && revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                got_r[w] |= b;
                ready += 1;
            }
            if want_w[w] & b != 0 && revents & (POLLOUT | POLLERR) != 0 {
                got_w[w] |= b;
                ready += 1;
            }
        }
        if ready > 0 || deadline.is_some_and(|d| crate::time::now() >= d) {
            for (ptr, set) in [(readfds, got_r), (writefds, got_w), (exceptfds, [0; 16])] {
                if ptr != 0 {
                    for (i, w) in set.iter().enumerate().take(words as usize) {
                        uaccess::write(ptr + i as u64 * 8, *w)?;
                    }
                }
            }
            return Ok(ready);
        }
        first = false;
        table.wait(deadline)?;
    }
}

/// A user timeout {seconds, sub-seconds} in nanoseconds, where a second has
/// `per_second` sub-seconds (a timeval or a timespec); a null pointer
/// means "wait forever". Negative or unnormalized values are EINVAL.
pub fn timeout(ptr: u64, per_second: u64) -> Result<Option<u64>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    let [sec, sub]: [i64; 2] = uaccess::read(ptr)?;
    if sec < 0 || !(0..per_second as i64).contains(&sub) {
        return Err(EINVAL);
    }
    let ns_per_sub = crate::time::NSEC_PER_SEC / per_second;
    Ok(Some((sec as u64).saturating_mul(crate::time::NSEC_PER_SEC).saturating_add(sub as u64 * ns_per_sub)))
}

/// poll's timeout in milliseconds (negative: wait forever).
pub fn poll_timeout(ms: i64) -> Option<u64> {
    (ms >= 0).then(|| (ms as u64).saturating_mul(1_000_000))
}

/// ppoll(fds, nfds, timeout, sigmask, size): poll with a timespec and a
/// temporary signal mask.
pub fn ppoll(fds: u64, nfds: u64, ts: u64, mask: u64, size: u64) -> SysResult {
    let timeout = timeout(ts, crate::time::NSEC_PER_SEC)?;
    let mask = super::signal::read_mask(mask, size)?;
    super::signal::with_mask(mask, || poll(fds, nfds, timeout))
}

/// pselect6(nfds, read, write, except, timeout, {sigmask, size}).
pub fn pselect6(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, ts: u64, sig: u64) -> SysResult {
    let timeout = timeout(ts, crate::time::NSEC_PER_SEC)?;
    let mask = match sig {
        0 => None,
        _ => {
            let [ptr, size]: [u64; 2] = uaccess::read(sig)?;
            super::signal::read_mask(ptr, size)?
        }
    };
    super::signal::with_mask(mask, || select(nfds, readfds, writefds, exceptfds, timeout))
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
    let f = file(fd)?;
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
    // The Linux server's files (it handles sendfile with them itself).
    if matches!(out.kind, Kind::Server(_)) || matches!(input.kind, Kind::Server(_)) {
        return Err(EINVAL);
    }
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
