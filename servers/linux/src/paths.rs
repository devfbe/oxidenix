//! The system calls that take a path (phase R6c.2b), and the working
//! directory and umask, which live in the caller's record (`records`).
//!
//! The semantics are the kernel's VFS's, which they replace: no permission
//! checks (one user), timestamps not stored, and a descriptor's or the
//! working directory's path is the one it was opened by (normalized,
//! symlinks not replaced). New: umask(2) is real.

use crate::files;
use crate::namespace::{check, mode_of, resolve, resolve_parent, KInode, Resolved, ENOENT, ENOTDIR};
use crate::records;
use crate::syscall;
use crate::usercopy::{self, read_cstr};
use alloc::string::String;
use restricted::*;
use vfs::path::join;

const EEXIST: i64 = 17;
const ERANGE: i64 = 34;
const ELOOP: i64 = 40;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EMPTY_PATH: u64 = 0x1000;

const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_NOFOLLOW: u32 = 0o400000;

const SYS_OPEN: u64 = 2;
const SYS_STAT: u64 = 4;
const SYS_LSTAT: u64 = 6;
const SYS_ACCESS: u64 = 21;
const SYS_EXECVE: u64 = 59;
const SYS_TRUNCATE: u64 = 76;
const SYS_GETCWD: u64 = 79;
const SYS_CHDIR: u64 = 80;
const SYS_FCHDIR: u64 = 81;
const SYS_RENAME: u64 = 82;
const SYS_MKDIR: u64 = 83;
const SYS_RMDIR: u64 = 84;
const SYS_UNLINK: u64 = 87;
const SYS_SYMLINK: u64 = 88;
const SYS_READLINK: u64 = 89;
const SYS_CHMOD: u64 = 90;
const SYS_UMASK: u64 = 95;
const SYS_STATFS: u64 = 137;
const SYS_UTIMES: u64 = 235;
const SYS_OPENAT: u64 = 257;
const SYS_MKDIRAT: u64 = 258;
const SYS_FUTIMESAT: u64 = 261;
const SYS_NEWFSTATAT: u64 = 262;
const SYS_UNLINKAT: u64 = 263;
const SYS_RENAMEAT: u64 = 264;
const SYS_SYMLINKAT: u64 = 266;
const SYS_READLINKAT: u64 = 267;
const SYS_FCHMODAT: u64 = 268;
const SYS_FACCESSAT: u64 = 269;
const SYS_UTIMENSAT: u64 = 280;
const SYS_RENAMEAT2: u64 = 316;
const SYS_FACCESSAT2: u64 = 439;

const CWD: u64 = AT_FDCWD as i64 as u64;

/// The result of a path call in `s`, or None to pass it through (execve
/// once its program is resolved, and the calls on a descriptor alone).
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3, a4) = (s.rdi, s.rsi, s.rdx, s.r10, s.r8);
    let result = match s.rax {
        SYS_OPEN => openat(CWD, a0, a1 as u32, a2 as u32),
        SYS_OPENAT => openat(a0, a1, a2 as u32, a3 as u32),
        SYS_STAT => fstatat(CWD, a0, a1, 0),
        SYS_LSTAT => fstatat(CWD, a0, a1, AT_SYMLINK_NOFOLLOW),
        SYS_NEWFSTATAT => {
            if a3 & AT_EMPTY_PATH != 0 && path_is_empty(a1) {
                // fstat of the descriptor: not a path call.
                return None;
            }
            fstatat(a0, a1, a2, a3)
        }
        SYS_ACCESS => at(CWD, a0, true).map(|_| 0),
        SYS_FACCESSAT | SYS_FACCESSAT2 => at(a0, a1, true).map(|_| 0),
        SYS_EXECVE => match at(CWD, a0, true) {
            Ok(r) => {
                let path = join(&r.path);
                let set = syscall(SYS_EXEC_TARGET, [r.inode.handle(), path.as_ptr() as u64, path.len() as u64, 0, 0, 0]);
                if set < 0 {
                    return Some(set);
                }
                return None;
            }
            Err(e) => Err(e),
        },
        SYS_TRUNCATE => at(CWD, a0, true).and_then(|r| check(syscall(SYS_INODE_TRUNCATE, [r.inode.handle(), a1, 0, 0, 0, 0]))),
        SYS_GETCWD => getcwd(a0, a1),
        SYS_CHDIR => chdir(a0),
        SYS_FCHDIR => fchdir(a0),
        SYS_RENAME => renameat(CWD, a0, CWD, a1),
        SYS_RENAMEAT | SYS_RENAMEAT2 => renameat(a0, a1, a2, a3),
        SYS_MKDIR => mkdirat(CWD, a0, a1 as u32),
        SYS_MKDIRAT => mkdirat(a0, a1, a2 as u32),
        SYS_RMDIR => unlinkat(CWD, a0, AT_REMOVEDIR),
        SYS_UNLINK => unlinkat(CWD, a0, 0),
        SYS_UNLINKAT => unlinkat(a0, a1, a2),
        SYS_SYMLINK => symlinkat(a0, CWD, a1),
        SYS_SYMLINKAT => symlinkat(a0, a1, a2),
        SYS_READLINK => readlinkat(CWD, a0, a1, a2),
        SYS_READLINKAT => readlinkat(a0, a1, a2, a3),
        SYS_CHMOD => chmodat(CWD, a0, a1),
        SYS_FCHMODAT => chmodat(a0, a1, a2),
        SYS_UMASK => Ok(umask(a0 as u32) as i64),
        SYS_STATFS => statfs(a0, a1),
        // Timestamps are not stored: these only check the target exists.
        SYS_UTIMES => at(CWD, a0, true).map(|_| 0),
        SYS_FUTIMESAT if a1 != 0 => at(a0, a1, true).map(|_| 0),
        SYS_UTIMENSAT if a1 != 0 => at(a0, a1, a3 & AT_SYMLINK_NOFOLLOW == 0).map(|_| 0),
        _ => {
            let _ = a4;
            return None;
        }
    };
    Some(result.unwrap_or_else(|e| -e))
}

fn path_is_empty(addr: u64) -> bool {
    usercopy::read::<u8>(addr).is_ok_and(|b| b == 0)
}

/// The directory a relative path of an *at call starts from.
fn base_dir(dirfd: u64, path: &str) -> Result<String, i64> {
    if path.starts_with('/') || dirfd as i32 == AT_FDCWD {
        return Ok(records::current().state.lock().cwd.clone());
    }
    Ok(fd_inode(dirfd)?.1)
}

/// The inode behind a descriptor of the kernel's, and its path (ENOTDIR
/// for the server's own files, which have no path).
fn fd_inode(fd: u64) -> Result<(KInode, String), i64> {
    if files::is_server_file(fd) {
        return Err(ENOTDIR);
    }
    let mut buf = alloc::vec![0u8; 4096];
    let mut len = 0u64;
    let inode = KInode::from_result(syscall(
        SYS_KFD_INODE,
        [fd, buf.as_mut_ptr() as u64, buf.len() as u64, &mut len as *mut u64 as u64, 0, 0],
    ))?;
    buf.truncate(len as usize);
    Ok((inode, String::from_utf8(buf).map_err(|_| ENOENT)?))
}

/// Resolves the program's path at `addr` relative to `dirfd`.
fn at(dirfd: u64, addr: u64, follow: bool) -> Result<Resolved, i64> {
    let path = read_cstr(addr)?;
    resolve(&base_dir(dirfd, &path)?, &path, follow)
}

/// The parent directory and name for a new or removed name at `addr`.
fn parent_at(dirfd: u64, addr: u64) -> Result<(Resolved, String), i64> {
    let path = read_cstr(addr)?;
    resolve_parent(&base_dir(dirfd, &path)?, &path)
}

fn umask_now() -> u32 {
    records::current().state.lock().umask
}

fn umask(new: u32) -> u32 {
    let context = records::current();
    let mut st = context.state.lock();
    core::mem::replace(&mut st.umask, new & 0o777)
}

fn openat(dirfd: u64, addr: u64, flags: u32, mode: u32) -> Result<i64, i64> {
    let path = read_cstr(addr)?;
    let base = base_dir(dirfd, &path)?;
    let nofollow = flags & O_NOFOLLOW != 0;
    let resolved = loop {
        match resolve(&base, &path, !nofollow) {
            Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(EEXIST),
            Ok(r) => break r,
            Err(ENOENT) if flags & O_CREAT != 0 => {
                let (dir, name) = resolve_parent(&base, &path)?;
                let perm = mode & !umask_now() & 0o7777;
                let created = syscall(SYS_INODE_CREATE, [dir.inode.handle(), name.as_ptr() as u64, name.len() as u64, INODE_FILE, perm as u64, 0]);
                match KInode::from_result(created) {
                    Ok(inode) => {
                        break Resolved { inode, mode: vfs::S_IFREG | perm, path: vfs::path::normalize(&base, &path) };
                    }
                    // Created meanwhile by someone else: open that one.
                    Err(EEXIST) if flags & O_EXCL == 0 => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    };
    // Opening a symlink itself would hand out its target bytes as a file.
    if nofollow && resolved.mode & vfs::S_IFMT == vfs::S_IFLNK {
        return Err(ELOOP);
    }
    let abs = join(&resolved.path);
    check(syscall(SYS_INODE_OPEN, [resolved.inode.handle(), flags as u64, abs.as_ptr() as u64, abs.len() as u64, 0, 0]))
}

fn fstatat(dirfd: u64, addr: u64, buf: u64, flags: u64) -> Result<i64, i64> {
    let r = at(dirfd, addr, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    usercopy::to_program(buf, &r.inode.stat()?)?;
    Ok(0)
}

fn getcwd(buf: u64, size: u64) -> Result<i64, i64> {
    let cwd = records::current().state.lock().cwd.clone();
    if (size as usize) < cwd.len() + 1 {
        return Err(ERANGE);
    }
    let mut out = cwd.into_bytes();
    out.push(0);
    usercopy::to_program(buf, &out)?;
    Ok(out.len() as i64)
}

fn chdir(addr: u64) -> Result<i64, i64> {
    let r = at(CWD, addr, true)?;
    if r.mode & vfs::S_IFMT != vfs::S_IFDIR {
        return Err(ENOTDIR);
    }
    records::current().state.lock().cwd = join(&r.path);
    Ok(0)
}

fn fchdir(fd: u64) -> Result<i64, i64> {
    let (inode, path) = fd_inode(fd)?;
    if mode_of(&inode.stat()?) & vfs::S_IFMT != vfs::S_IFDIR {
        return Err(ENOTDIR);
    }
    records::current().state.lock().cwd = path;
    Ok(0)
}

fn renameat(odirfd: u64, oaddr: u64, ndirfd: u64, naddr: u64) -> Result<i64, i64> {
    let (odir, oname) = parent_at(odirfd, oaddr)?;
    let (ndir, nname) = parent_at(ndirfd, naddr)?;
    check(syscall(
        SYS_INODE_RENAME,
        [odir.inode.handle(), oname.as_ptr() as u64, oname.len() as u64, ndir.inode.handle(), nname.as_ptr() as u64, nname.len() as u64],
    ))
}

fn mkdirat(dirfd: u64, addr: u64, mode: u32) -> Result<i64, i64> {
    let (dir, name) = parent_at(dirfd, addr)?;
    let perm = mode & !umask_now() & 0o7777;
    let created = syscall(SYS_INODE_CREATE, [dir.inode.handle(), name.as_ptr() as u64, name.len() as u64, INODE_DIR, perm as u64, 0]);
    KInode::from_result(created).map(|_| 0)
}

fn unlinkat(dirfd: u64, addr: u64, flags: u64) -> Result<i64, i64> {
    let (dir, name) = parent_at(dirfd, addr)?;
    let dir_only = (flags & AT_REMOVEDIR != 0) as u64;
    check(syscall(SYS_INODE_UNLINK, [dir.inode.handle(), name.as_ptr() as u64, name.len() as u64, dir_only, 0, 0]))
}

fn symlinkat(target: u64, dirfd: u64, addr: u64) -> Result<i64, i64> {
    let target = read_cstr(target)?;
    let (dir, name) = parent_at(dirfd, addr)?;
    check(syscall(
        SYS_INODE_SYMLINK,
        [dir.inode.handle(), name.as_ptr() as u64, name.len() as u64, target.as_ptr() as u64, target.len() as u64, 0],
    ))
}

fn readlinkat(dirfd: u64, addr: u64, buf: u64, size: u64) -> Result<i64, i64> {
    let r = at(dirfd, addr, false)?;
    let target = r.inode.readlink()?;
    let n = target.len().min(size as usize);
    usercopy::to_program(buf, &target.as_bytes()[..n])?;
    Ok(n as i64)
}

fn chmodat(dirfd: u64, addr: u64, mode: u64) -> Result<i64, i64> {
    let r = at(dirfd, addr, true)?;
    check(syscall(SYS_INODE_CHMOD, [r.inode.handle(), mode, 0, 0, 0, 0]))
}

fn statfs(addr: u64, buf: u64) -> Result<i64, i64> {
    let r = at(CWD, addr, true)?;
    let mut words = [0u8; 120];
    check(syscall(SYS_INODE_STATFS, [r.inode.handle(), words.as_mut_ptr() as u64, 0, 0, 0, 0]))?;
    usercopy::to_program(buf, &words)?;
    Ok(0)
}
