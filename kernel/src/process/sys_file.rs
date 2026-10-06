//! File syscalls.

use super::errno::*;
use super::{uaccess, with_current, FdEntry};
use crate::fs::file::*;
use crate::fs::{self, Data, Inode, Node, S_IFDIR, S_IFLNK, S_IFREG};
use alloc::collections::BTreeMap;
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

fn file(fd: u64) -> Result<Arc<OpenFile>, i64> {
    with_current(|p| p.file(fd))
}

/// Directory against which a relative path of an *at syscall is resolved.
fn base_dir(dirfd: u64, path: &str) -> Result<String, i64> {
    if path.starts_with('/') || dirfd as i64 == AT_FDCWD {
        return Ok(with_current(|p| p.cwd.clone()));
    }
    file(dirfd)?.path.clone().ok_or(ENOTDIR)
}

fn resolve_at(dirfd: u64, path: &str, follow: bool) -> Result<(Arc<Inode>, String), i64> {
    let base = base_dir(dirfd, path)?;
    let inode = fs::resolve(&base, path, follow)?;
    Ok((inode, fs::join(&fs::normalize(&base, path))))
}

pub fn read(fd: u64, buf: u64, len: u64) -> SysResult {
    let f = file(fd)?;
    Ok(f.read(uaccess::slice_mut(buf, len)?)? as i64)
}

pub fn write(fd: u64, buf: u64, len: u64) -> SysResult {
    let f = file(fd)?;
    Ok(f.write(uaccess::slice(buf, len)?)? as i64)
}

fn iovecs(iov: u64, count: u64) -> Result<Vec<(u64, u64)>, i64> {
    if count > 1024 {
        return Err(EINVAL);
    }
    (0..count).map(|i| uaccess::read::<(u64, u64)>(iov + i * 16)).collect()
}

pub fn readv(fd: u64, iov: u64, count: u64) -> SysResult {
    let f = file(fd)?;
    let mut total = 0;
    for (base, len) in iovecs(iov, count)? {
        let n = f.read(uaccess::slice_mut(base, len)?)?;
        total += n as i64;
        if n < len as usize {
            break;
        }
    }
    Ok(total)
}

pub fn writev(fd: u64, iov: u64, count: u64) -> SysResult {
    let f = file(fd)?;
    let mut total = 0;
    for (base, len) in iovecs(iov, count)? {
        total += f.write(uaccess::slice(base, len)?)? as i64;
    }
    Ok(total)
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
            let inode = Inode::new(Node::File(Data::empty()), mode as u32 & !UMASK)?;
            dir.insert(&name, inode.clone())?;
            inode
        }
        Err(e) => return Err(e),
    };
    let writable = flags & O_ACCMODE != 0;
    if inode.is_dir() && writable {
        return Err(EISDIR);
    }
    if flags & O_DIRECTORY != 0 && !inode.is_dir() {
        return Err(ENOTDIR);
    }
    if flags & O_TRUNC != 0 && writable {
        if let Node::File(data) = &mut *inode.node.lock() {
            *data = Data::empty();
        }
    }
    let abs = fs::join(&fs::normalize(&base, &path));
    let f = OpenFile::new(Kind::Inode(inode), flags, Some(abs));
    with_current(|p| p.alloc_fd(f, flags & O_CLOEXEC != 0, 0))
}

pub fn close(fd: u64) -> SysResult {
    let old = with_current(|p| p.fds.get_mut(fd as usize).and_then(|e| e.take()));
    // Dropped only here: may wake the other end of a pipe.
    old.ok_or(EBADF).map(|_| 0)
}

fn write_stat(buf: u64, ino: u64, mode: u32, size: u64) -> SysResult {
    let mut st = [0u8; 144];
    st[8..16].copy_from_slice(&ino.to_le_bytes());
    st[16..24].copy_from_slice(&1u64.to_le_bytes()); // nlink
    st[24..28].copy_from_slice(&mode.to_le_bytes());
    st[48..56].copy_from_slice(&size.to_le_bytes());
    st[56..64].copy_from_slice(&4096u64.to_le_bytes()); // blksize
    st[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes()); // blocks
    uaccess::write(buf, st)?;
    Ok(0)
}

fn stat_inode(inode: &Inode, buf: u64) -> SysResult {
    write_stat(buf, inode.ino, inode.mode(), inode.size())
}

pub fn fstat(fd: u64, buf: u64) -> SysResult {
    let f = file(fd)?;
    match f.inode() {
        Some(inode) => stat_inode(inode, buf),
        None => write_stat(buf, Arc::as_ptr(&f) as u64, S_IFIFO | 0o600, 0),
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
    let entries: Vec<(String, u64, u8)> = match &*inode.node.lock() {
        Node::Dir(map) => {
            let mut v = vec![(".".into(), inode.ino, 4u8), ("..".into(), inode.ino, 4u8)];
            v.extend(map.iter().map(|(name, child)| {
                let dtype = match child.file_type() {
                    S_IFDIR => 4,
                    S_IFREG => 8,
                    S_IFLNK => 10,
                    _ => 2,
                };
                (name.clone(), child.ino, dtype)
            }));
            v
        }
        _ => return Err(ENOTDIR),
    };
    let out = uaccess::slice_mut(buf, len)?;
    let mut off = f.offset.lock();
    let mut pos = 0;
    while let Some((name, ino, dtype)) = entries.get(*off as usize) {
        let reclen = (19 + name.len() + 1).next_multiple_of(8);
        if pos + reclen > out.len() {
            if pos == 0 {
                return Err(EINVAL);
            }
            break;
        }
        let rec = &mut out[pos..pos + reclen];
        rec.fill(0);
        rec[0..8].copy_from_slice(&ino.to_le_bytes());
        rec[8..16].copy_from_slice(&(*off + 1).to_le_bytes());
        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        rec[18] = *dtype;
        rec[19..19 + name.len()].copy_from_slice(name.as_bytes());
        pos += reclen;
        *off += 1;
    }
    Ok(pos as i64)
}

pub fn pipe2(fds: u64, flags: u64) -> SysResult {
    let (r, w) = OpenFile::pipe();
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
    let replaced = with_current(|p| {
        if p.fds.len() <= new as usize {
            p.fds.resize(new as usize + 1, None);
        }
        p.fds[new as usize].replace(FdEntry { file: f, cloexec })
    });
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
        F_GETFD => with_current(|p| {
            let entry = p.fds[fd as usize].as_ref().ok_or(EBADF)?;
            Ok(if entry.cloexec { FD_CLOEXEC as i64 } else { 0 })
        }),
        F_SETFD => with_current(|p| {
            p.fds[fd as usize].as_mut().ok_or(EBADF)?.cloexec = arg & FD_CLOEXEC != 0;
            Ok(0)
        }),
        F_GETFL => Ok(f.flags.load(Ordering::Relaxed) as i64),
        F_SETFL => {
            let changeable = O_APPEND | O_NONBLOCK;
            let old = f.flags.load(Ordering::Relaxed);
            f.flags.store(old & !changeable | arg as u32 & changeable, Ordering::Relaxed);
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

pub fn ioctl(fd: u64, request: u64, arg: u64) -> SysResult {
    const TIOCGWINSZ: u64 = 0x5413;
    let f = file(fd)?;
    if !f.is_console() || request != TIOCGWINSZ {
        return Err(ENOTTY);
    }
    let (cols, rows) = crate::drivers::console::size();
    uaccess::write(arg, [rows as u16, cols as u16, 0, 0])?;
    Ok(0)
}

pub fn faccessat(dirfd: u64, path: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    resolve_at(dirfd, &path, true).map(|_| 0)
}

pub fn getcwd(buf: u64, size: u64) -> SysResult {
    let cwd = with_current(|p| p.cwd.clone());
    if (size as usize) < cwd.len() + 1 {
        return Err(ERANGE);
    }
    let out = uaccess::slice_mut(buf, cwd.len() as u64 + 1)?;
    out[..cwd.len()].copy_from_slice(cwd.as_bytes());
    out[cwd.len()] = 0;
    Ok(cwd.len() as i64 + 1)
}

pub fn chdir(path: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, abs) = resolve_at(AT_FDCWD as u64, &path, true)?;
    if !inode.is_dir() {
        return Err(ENOTDIR);
    }
    with_current(|p| p.cwd = abs);
    Ok(0)
}

pub fn fchdir(fd: u64) -> SysResult {
    let f = file(fd)?;
    if !f.inode().is_some_and(|i| i.is_dir()) {
        return Err(ENOTDIR);
    }
    let path = f.path.clone().ok_or(ENOTDIR)?;
    with_current(|p| p.cwd = path);
    Ok(0)
}

fn create_at(dirfd: u64, path: u64, node: Node, perm: u32) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let base = base_dir(dirfd, &path)?;
    let (dir, name) = fs::resolve_parent(&base, &path)?;
    dir.insert(&name, Inode::new(node, perm)?)?;
    Ok(0)
}

pub fn mkdirat(dirfd: u64, path: u64, mode: u64) -> SysResult {
    create_at(dirfd, path, Node::Dir(BTreeMap::new()), mode as u32 & !UMASK)
}

pub fn symlinkat(target: u64, dirfd: u64, path: u64) -> SysResult {
    let target = uaccess::read_cstr(target)?;
    create_at(dirfd, path, Node::Symlink(target), 0o777)
}

pub fn unlinkat(dirfd: u64, path: u64, flags: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let base = base_dir(dirfd, &path)?;
    let (dir, name) = fs::resolve_parent(&base, &path)?;
    let child = dir.child(&name)?;
    match (&*child.node.lock(), flags & AT_REMOVEDIR != 0) {
        (Node::Dir(m), true) if !m.is_empty() => return Err(ENOTEMPTY),
        (Node::Dir(_), true) => {}
        (Node::Dir(_), false) => return Err(EISDIR),
        (_, true) => return Err(ENOTDIR),
        (_, false) => {}
    }
    if let Node::Dir(m) = &mut *dir.node.lock() {
        m.remove(&name);
    }
    Ok(0)
}

pub fn renameat(olddirfd: u64, oldpath: u64, newdirfd: u64, newpath: u64) -> SysResult {
    let oldpath = uaccess::read_cstr(oldpath)?;
    let newpath = uaccess::read_cstr(newpath)?;
    let (obase, nbase) = (base_dir(olddirfd, &oldpath)?, base_dir(newdirfd, &newpath)?);
    let (odir, oname) = fs::resolve_parent(&obase, &oldpath)?;
    let (ndir, nname) = fs::resolve_parent(&nbase, &newpath)?;
    let node = odir.child(&oname)?;
    // Moving a directory below itself would detach it in a reference cycle.
    if fs::contains(&node, &ndir) {
        return Err(EINVAL);
    }
    if let Ok(existing) = ndir.child(&nname) {
        if Arc::ptr_eq(&existing, &node) {
            return Ok(0);
        }
        // Replacing only ever drops a file or an empty directory, never a
        // whole subtree (whose recursive drop could overflow the stack).
        match (&*existing.node.lock(), node.is_dir()) {
            (Node::Dir(m), true) if !m.is_empty() => return Err(ENOTEMPTY),
            (Node::Dir(_), false) => return Err(EISDIR),
            (_, true) if !existing.is_dir() => return Err(ENOTDIR),
            _ => {}
        }
    }
    if nname.len() > fs::NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    if let Node::Dir(m) = &mut *odir.node.lock() {
        m.remove(&oname);
    }
    let replaced = match &mut *ndir.node.lock() {
        Node::Dir(m) => m.insert(nname, node),
        _ => None,
    };
    drop(replaced);
    Ok(0)
}

pub fn readlinkat(dirfd: u64, path: u64, buf: u64, size: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, _) = resolve_at(dirfd, &path, false)?;
    let target = match &*inode.node.lock() {
        Node::Symlink(t) => t.clone(),
        _ => return Err(EINVAL),
    };
    let n = target.len().min(size as usize);
    uaccess::slice_mut(buf, n as u64)?.copy_from_slice(&target.as_bytes()[..n]);
    Ok(n as i64)
}

pub fn fchmodat(dirfd: u64, path: u64, mode: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let (inode, _) = resolve_at(dirfd, &path, true)?;
    *inode.perm.lock() = mode as u32 & 0o7777;
    Ok(0)
}

pub fn ftruncate(fd: u64, len: u64) -> SysResult {
    let f = file(fd)?;
    let inode = f.inode().ok_or(EINVAL)?;
    let result = match &mut *inode.node.lock() {
        Node::File(data) => data.resize(usize::try_from(len).map_err(|_| EFBIG)?).map(|_| 0),
        _ => Err(EINVAL),
    };
    result
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
