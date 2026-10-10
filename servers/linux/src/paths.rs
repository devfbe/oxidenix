//! The system calls that take a path (phase R6c.2b), and the working
//! directory and umask, which live in the caller's record (`records`).
//!
//! Each call resolves its path in the server's namespace (`namespace`) and
//! acts on the inode it reaches: one of the server's tmpfs, one of /data
//! (`datafs`), or one of the kernel's tree through its handle. The semantics are the kernel's VFS's,
//! which they replace: no permission checks and no owners (one user), and
//! a descriptor's or the working directory's path is the one it was
//! opened by (normalized, symlinks not replaced). New: umask(2) is real,
//! and so are the files' times (utimensat and the calls that change them),
//! statx, and the inotify events of what the calls do.

use crate::datafile;
use crate::datafs;
use crate::files;
use crate::inotify;
use crate::namespace::{check, mode_of, resolve, resolve_parent, KInode, Node, Origin, Resolved, EBUSY, ENOENT, ENOTDIR, EXDEV};
use crate::records;
use crate::syscall;
use crate::tmpfile;
use crate::tmpfs;
use crate::usercopy::{self, read_cstr};
use alloc::string::String;
use restricted::*;
use vfs::path::join;
use vfs::stat::SetTime;

const EPERM: i64 = 1;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const ERANGE: i64 = 34;
const ELOOP: i64 = 40;
const EINVAL: i64 = 22;
const EFAULT: i64 = 14;
const EOPNOTSUPP: i64 = 95;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EMPTY_PATH: u64 = 0x1000;

const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_NOFOLLOW: u32 = 0o400000;
const O_DIRECTORY: u32 = 0o200000;
const O_PATH: u32 = 0o10000000;

const SYS_OPEN: u64 = 2;
const SYS_STAT: u64 = 4;
const SYS_LSTAT: u64 = 6;
const SYS_ACCESS: u64 = 21;
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
const SYS_FCHMOD: u64 = 91;
const SYS_CHOWN: u64 = 92;
const SYS_FCHOWN: u64 = 93;
const SYS_LCHOWN: u64 = 94;
const SYS_FCHOWNAT: u64 = 260;
const SYS_FCHMODAT2: u64 = 452;
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
const SYS_STATX: u64 = 332;
const SYS_FACCESSAT2: u64 = 439;

const CWD: u64 = AT_FDCWD as i64 as u64;

/// The result of a path call in `s`, or None to pass it through (the calls
/// on a descriptor alone). execve is `exec`'s, which resolves its program here
/// (`exec_open`).
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3) = (s.rdi, s.rsi, s.rdx, s.r10);
    let result = match s.rax {
        SYS_OPEN => openat(CWD, a0, a1 as u32, a2 as u32),
        SYS_OPENAT => openat(a0, a1, a2 as u32, a3 as u32),
        SYS_STAT => fstatat(CWD, a0, a1, 0),
        SYS_LSTAT => fstatat(CWD, a0, a1, AT_SYMLINK_NOFOLLOW),
        SYS_NEWFSTATAT => {
            if a3 & AT_EMPTY_PATH != 0 && path_is_empty(a1) {
                stat_dirfd(a0).and_then(|st| usercopy::to_program(a2, &st.to_bytes())).map(|_| 0)
            } else {
                fstatat(a0, a1, a2, a3)
            }
        }
        SYS_ACCESS => at(CWD, a0, true).map(|_| 0),
        SYS_FACCESSAT | SYS_FACCESSAT2 => at(a0, a1, true).map(|_| 0),
        SYS_TRUNCATE => at(CWD, a0, true).and_then(|r| {
            truncate(&r.node, a1)?;
            inotify::node_event(&r.node, inotify::IN_MODIFY);
            Ok(0)
        }),
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
        SYS_FCHMOD => fchmod(a0, a1),
        SYS_FCHMODAT2 => fchmodat2(a0, a1, a2, a3),
        SYS_CHOWN => chownat(CWD, a0, 0),
        SYS_LCHOWN => chownat(CWD, a0, AT_SYMLINK_NOFOLLOW),
        SYS_FCHOWN => fd_inode(a0, false).and_then(chown_target),
        SYS_FCHOWNAT => chownat(a0, a1, s.r8),
        SYS_STATX => statx(a0, a1, a2 as u32, a3 as u32, s.r8),
        inotify::SYS_INOTIFY_ADD_WATCH => inotify_add_watch(a0, a1, a2 as u32),
        SYS_UMASK => Ok(umask(a0 as u32) as i64),
        SYS_STATFS => statfs(a0, a1),
        SYS_UTIMES => utimes(CWD, a0, a1),
        SYS_FUTIMESAT => utimes(a0, a1, a2),
        SYS_UTIMENSAT => utimensat(a0, a1, a2, a3),
        _ => return None,
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
    // A descriptor that keeps its origin: its path, without another reference to
    // the node; ENOTDIR unless it is a directory (an O_PATH|O_NOFOLLOW symlink's
    // path must not be resolved through the link).
    if let Some(o) = files::origin_of(dirfd) {
        return if o.kind == vfs::S_IFDIR { Ok(o.path) } else { Err(ENOTDIR) };
    }
    Ok(fd_node(dirfd)?.1)
}

/// The inode behind a descriptor and the path it was opened by (ENOTDIR
/// for one without: pipes, sockets, eventfds), an O_PATH descriptor's too.
fn fd_node(fd: u64) -> Result<(Node, String), i64> {
    fd_node_of(fd).map(|(node, path, _)| (node, path))
}

/// `fd_node`, and whether the descriptor is an O_PATH one.
fn fd_node_of(fd: u64) -> Result<(Node, String, bool), i64> {
    if let Some(f) = files::tmp_of(fd) {
        return Ok((Node::Tmp(f.inode.clone()), f.path.clone(), false));
    }
    if let Some(f) = files::data_of(fd) {
        return Ok((Node::Data(f.inode.clone()), f.path.clone(), false));
    }
    if let Some(f) = files::proc_of(fd) {
        return Ok((Node::Proc(f.node.clone()), f.path.clone(), false));
    }
    // Terminals, null and zero, O_PATH: the node they were opened by.
    if let Some(o) = files::origin_of(fd) {
        return Ok((o.node()?, o.path, o.o_path));
    }
    // An open file of the kernel's tree: its inode; any other file has none.
    match &files::lookup_raw(fd)?.file {
        files::File::Kernel(k) => Ok((Node::Kernel(k.inode()?), k.path.clone(), false)),
        _ => Err(ENOTDIR),
    }
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

const NEW_FILE: u64 = INODE_FILE;
const NEW_DIR: u64 = INODE_DIR;

/// A new file or directory `name` in the directory `dir`.
fn create(dir: &Node, name: &str, kind: u64, perm: u32) -> Result<Node, i64> {
    match dir {
        Node::Kernel(d) => KInode::from_result(syscall(
            SYS_INODE_CREATE,
            [d.handle(), name.as_ptr() as u64, name.len() as u64, kind, perm as u64, 0],
        ))
        .map(Node::Kernel),
        Node::Tmp(d) => d.create(name, kind == NEW_DIR, perm).map(Node::Tmp),
        Node::Data(d) => {
            let new = if kind == NEW_DIR { datafs::New::Dir } else { datafs::New::File };
            datafs::create(d, name, new, perm).map(Node::Data)
        }
        // /proc and /sys make no names (Linux: no create, EACCES; no
        // mkdir, EPERM).
        Node::Proc(p) if crate::procfs::lookup(p, name).is_ok() => Err(EEXIST),
        Node::Proc(_) => Err(if kind == NEW_DIR { EPERM } else { EACCES }),
    }
}

/// The program `path` names (relative to the absolute directory `base`; `follow`: a
/// symlink as the last name is followed, else ELOOP), opened for execve (`exec`): a handle
/// on its file object that holds it (nobody may write it while it runs: ETXTBSY), and its
/// absolute path.
pub fn exec_open(base: &str, path: &str, follow: bool) -> Result<(u64, String), i64> {
    let r = resolve(base, path, follow)?;
    exec_node(&r.node, join(&r.path))
}

/// The program a descriptor names (execveat with AT_EMPTY_PATH).
pub fn exec_open_fd(fd: u64) -> Result<(u64, String), i64> {
    let (node, path) = fd_node(fd)?;
    exec_node(&node, path)
}

fn exec_node(node: &Node, path: String) -> Result<(u64, String), i64> {
    const EACCES: i64 = 13;
    let mode = mode_of(&node.stat()?);
    if mode & vfs::S_IFMT == vfs::S_IFLNK {
        return Err(ELOOP);
    }
    crate::exec::check_mode(mode)?;
    let held = match node {
        Node::Tmp(t) => tmpfile::exec_hold(t)?,
        Node::Data(d) => datafile::exec_hold(d)?,
        // The kernel's tree (/dev) and /proc and /sys hold no programs.
        Node::Kernel(_) | Node::Proc(_) => return Err(EACCES),
    };
    Ok((held, path))
}

/// The directory a relative path of an *at call (execveat) starts from.
pub fn base_dir_of(dirfd: u64, path: &str) -> Result<String, i64> {
    base_dir(dirfd, path)
}

fn openat(dirfd: u64, addr: u64, flags: u32, mode: u32) -> Result<i64, i64> {
    let path = read_cstr(addr)?;
    let base = base_dir(dirfd, &path)?;
    let nofollow = flags & O_NOFOLLOW != 0;
    // O_PATH names the node and opens nothing (`pathfile`): O_CREAT creates
    // nothing, O_NOFOLLOW names a symlink itself, the access mode is ignored.
    if flags & O_PATH != 0 {
        let r = resolve(&base, &path, !nofollow)?;
        if flags & O_DIRECTORY != 0 && r.mode & vfs::S_IFMT != vfs::S_IFDIR {
            return Err(ENOTDIR);
        }
        let path = join(&r.path);
        return crate::pathfile::open(flags, Origin::new(r.node, path, r.mode));
    }
    // A name created by someone else between our lookup and our create is
    // opened instead, once: a name that exists but does not resolve (a
    // dangling symlink) stays EEXIST, as with the kernel's VFS.
    let mut retried = false;
    let resolved = loop {
        match resolve(&base, &path, !nofollow) {
            Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(EEXIST),
            Ok(r) => break r,
            Err(ENOENT) if flags & O_CREAT != 0 => {
                let (dir, name) = resolve_parent(&base, &path).map_err(|e| if e == EBUSY { EEXIST } else { e })?;
                let perm = mode & !umask_now() & 0o7777;
                match create(&dir.node, &name, NEW_FILE, perm) {
                    Ok(node) => {
                        inotify::child(&dir.node, inotify::IN_CREATE, &name, false);
                        break Resolved { node, mode: vfs::S_IFREG | perm, path: vfs::path::normalize(&base, &path) };
                    }
                    Err(EEXIST) if flags & O_EXCL == 0 && !retried => {
                        retried = true;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    };
    // Opening a symlink itself would hand out its target bytes as a file:
    // one named with O_NOFOLLOW, or one a magic link led to (an O_PATH
    // descriptor of a symlink, /proc/self/fd/N), is ELOOP (O_PATH alone
    // names it, above).
    if resolved.mode & vfs::S_IFMT == vfs::S_IFLNK {
        return Err(ELOOP);
    }
    let abs = join(&resolved.path);
    // A character device node names its driver by its number, wherever the
    // node is (the kernel's /dev, the server's tmpfs or devpts): the terminals
    // are the server's (`tty`); null (1,3) and zero (1,5) the kernel's for its
    // own nodes, the server's (`devices`) for its nodes; any other number has
    // no driver (ENXIO). The server's opens keep the node they were opened by
    // (`Origin`: a live fstat, fchmod and the like). O_DIRECTORY is ENOTDIR.
    if resolved.mode & vfs::S_IFMT == vfs::S_IFCHR {
        use crate::devices::{self, Kind};
        if flags & O_DIRECTORY != 0 {
            return Err(ENOTDIR);
        }
        let rdev = vfs::stat::Stat::from_bytes(&resolved.node.stat()?).rdev;
        let kernel_node = matches!(resolved.node, Node::Kernel(_));
        let kind = match vfs::stat::dev_split(rdev) {
            (1, 3) => Some(Kind::Null),
            (1, 5) => Some(Kind::Zero),
            _ => None,
        };
        if !kernel_node || kind.is_none() {
            let origin = Origin::new(resolved.node, abs, vfs::S_IFCHR);
            if let Some(kind) = kind {
                return devices::open(kind, flags, origin);
            }
            return crate::tty::open_device(rdev, flags, origin).unwrap_or(Err(crate::tty::ENXIO));
        }
    }
    match resolved.node {
        Node::Kernel(k) => {
            let handle = check(syscall(SYS_INODE_OPEN, [k.handle(), flags as u64, abs.as_ptr() as u64, abs.len() as u64, 0, 0]))?;
            crate::kfile::install(handle as u64, flags, abs)
        }
        Node::Tmp(t) => tmpfile::open(t, flags, abs),
        Node::Data(d) => datafile::open(d, flags, abs),
        Node::Proc(p) => crate::procfile::open(p, flags, abs),
    }
}

fn fstatat(dirfd: u64, addr: u64, buf: u64, flags: u64) -> Result<i64, i64> {
    let r = at(dirfd, addr, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    usercopy::to_program(buf, &r.node.stat()?)?;
    Ok(0)
}

/// inotify_add_watch(fd, path, mask): a watch on the file at `path` (its
/// final symlink followed unless IN_DONT_FOLLOW).
fn inotify_add_watch(fd: u64, addr: u64, mask: u32) -> Result<i64, i64> {
    const IN_DONT_FOLLOW: u32 = 0x0200_0000;
    let instance = inotify::instance(fd)?;
    let r = at(CWD, addr, mask & IN_DONT_FOLLOW == 0)?;
    let is_dir = r.mode & vfs::S_IFMT == vfs::S_IFDIR;
    instance.add_watch(&r.node, is_dir, mask)
}

/// The `struct stat` an empty path with AT_EMPTY_PATH names: descriptor
/// `dirfd` itself (any kind), or the working directory for AT_FDCWD.
fn stat_dirfd(dirfd: u64) -> Result<vfs::stat::Stat, i64> {
    if dirfd as i32 == AT_FDCWD {
        let cwd = records::current().state.lock().cwd.clone();
        return resolve(&cwd, &cwd, true)?.node.status();
    }
    files::stat_of(dirfd)
}

/// statx(dirfd, path, flags, mask, buf) (statx(2)): the file's status as
/// a `struct statx`, with every field the filesystem knows (`stx_mask`
/// says which), whatever `mask` asks for. An empty path with
/// AT_EMPTY_PATH (or none at all, as since Linux 6.11) means `dirfd`
/// itself, any descriptor, or the working directory for AT_FDCWD.
fn statx(dirfd: u64, addr: u64, flags: u32, mask: u32, buf: u64) -> Result<i64, i64> {
    use vfs::stat::statx_check;
    statx_check(flags, mask)?;
    let empty_path_ok = flags as u64 & AT_EMPTY_PATH != 0;
    let path = if addr == 0 && empty_path_ok { String::new() } else { read_cstr(addr)? };
    let st = if path.is_empty() {
        if !empty_path_ok {
            return Err(ENOENT);
        }
        stat_dirfd(dirfd)?
    } else {
        resolve(&base_dir(dirfd, &path)?, &path, flags as u64 & AT_SYMLINK_NOFOLLOW == 0)?.node.status()?
    };
    usercopy::to_program(buf, &st.to_statx())?;
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

/// The longest working directory kept (every relative path call works from it, so its
/// length is bounded as a path's work is): a deeper one is ENAMETOOLONG.
const CWD_MAX: usize = 64 * 1024;
const ENAMETOOLONG: i64 = 36;

fn chdir(addr: u64) -> Result<i64, i64> {
    let r = at(CWD, addr, true)?;
    if r.mode & vfs::S_IFMT != vfs::S_IFDIR {
        return Err(ENOTDIR);
    }
    let cwd = join(&r.path);
    if cwd.len() > CWD_MAX {
        return Err(ENAMETOOLONG);
    }
    records::current().state.lock().cwd = cwd;
    Ok(0)
}

fn fchdir(fd: u64) -> Result<i64, i64> {
    let (node, path) = fd_node(fd)?;
    if mode_of(&node.stat()?) & vfs::S_IFMT != vfs::S_IFDIR {
        return Err(ENOTDIR);
    }
    if path.len() > CWD_MAX {
        return Err(ENAMETOOLONG);
    }
    records::current().state.lock().cwd = path;
    Ok(0)
}

fn renameat(odirfd: u64, oaddr: u64, ndirfd: u64, naddr: u64) -> Result<i64, i64> {
    let (odir, oname) = parent_at(odirfd, oaddr)?;
    let (ndir, nname) = parent_at(ndirfd, naddr)?;
    // What moved and what it replaced (its last link gone), for inotify.
    let (moved, replaced): (Option<(inotify::Key, bool)>, Option<Gone>) = match (&odir.node, &ndir.node) {
        (Node::Kernel(o), Node::Kernel(n)) => {
            check(syscall(
                SYS_INODE_RENAME,
                [o.handle(), oname.as_ptr() as u64, oname.len() as u64, n.handle(), nname.as_ptr() as u64, nname.len() as u64],
            ))?;
            (None, None)
        }
        (Node::Tmp(o), Node::Tmp(n)) => {
            let (node, old) = tmpfs::rename(o, &oname, n, &nname)?;
            (Some((inotify::Key::tmp(&node), node.is_dir())), old.map(Gone::Tmp))
        }
        (Node::Data(o), Node::Data(n)) => {
            let (m, gone) = datafs::rename(o, &oname, n, &nname)?;
            // An inode not cached here: the name finds it now.
            let m = m.or_else(|| inotify::active().then(|| datafs::lookup(n, &nname).ok().map(|i| i.ino)).flatten());
            let moved = m.and_then(datafs::cached).map(|i| (inotify::Key::data(&i), i.kind == vfs::S_IFDIR));
            (moved, gone.map(Gone::Data))
        }
        // Nothing of /proc and /sys can be renamed (Linux: EPERM), within
        // one of them; across them it is another filesystem.
        (Node::Proc(o), Node::Proc(n)) if crate::procfs::same_fs(o, n) => {
            crate::procfs::lookup(o, &oname)?;
            return Err(EPERM);
        }
        _ => return Err(EXDEV),
    };
    if let Some((key, is_dir)) = moved {
        inotify::moved(&odir.node, &oname, &ndir.node, &nname, Some(key), is_dir);
    }
    if let Some(g) = replaced {
        g.report();
    }
    Ok(0)
}

/// An inode whose last link a removal or a rename took: inotify's events
/// for it (IN_ATTRIB for the link count, IN_DELETE_SELF now or at its last
/// close).
enum Gone {
    Tmp(alloc::sync::Arc<tmpfs::Inode>),
    Data(u32),
}

impl Gone {
    fn is_dir(&self) -> bool {
        match self {
            Gone::Tmp(t) => t.is_dir(),
            Gone::Data(ino) => datafs::cached(*ino).is_some_and(|i| i.kind == vfs::S_IFDIR),
        }
    }

    /// Its key (for /data: with its generation, if the server still has
    /// it; else any watch of the number), and whether it is still open.
    /// (SeqCst after the removal's store: see `TmpOpen`'s and `DataOpen`'s
    /// drop.)
    fn key(&self) -> (inotify::Key, bool) {
        use core::sync::atomic::{fence, Ordering::SeqCst};
        fence(SeqCst);
        match self {
            Gone::Tmp(t) => (inotify::Key::tmp(t), t.opens.load(SeqCst) > 0),
            Gone::Data(ino) => match datafs::cached(*ino) {
                Some(i) => (inotify::Key::data(&i), i.opens.load(SeqCst) > 0),
                None => (inotify::Key::DataAny(*ino), false),
            },
        }
    }

    /// IN_ATTRIB for the link count (a file's).
    fn attrib(&self, dir: bool) {
        if !dir {
            inotify::event(self.key().0, false, inotify::IN_ATTRIB, None);
        }
    }

    /// IN_DELETE_SELF unless it is still open: then it goes with its last
    /// close (`TmpOpen`, `DataOpen`).
    fn finish(&self, dir: bool) {
        let (key, open) = self.key();
        if !open {
            inotify::deleted(key, dir);
        }
    }

    fn report(&self) {
        if inotify::active() {
            let dir = self.is_dir();
            self.attrib(dir);
            self.finish(dir);
        }
    }
}

fn mkdirat(dirfd: u64, addr: u64, mode: u32) -> Result<i64, i64> {
    // A mount point exists.
    let (dir, name) = parent_at(dirfd, addr).map_err(|e| if e == EBUSY { EEXIST } else { e })?;
    let perm = mode & !umask_now() & 0o7777;
    create(&dir.node, &name, NEW_DIR, perm)?;
    inotify::child(&dir.node, inotify::IN_CREATE, &name, true);
    Ok(0)
}

fn unlinkat(dirfd: u64, addr: u64, flags: u64) -> Result<i64, i64> {
    let (dir, name) = parent_at(dirfd, addr)?;
    let dir_only = flags & AT_REMOVEDIR != 0;
    let gone = match &dir.node {
        Node::Kernel(d) => {
            check(syscall(SYS_INODE_UNLINK, [d.handle(), name.as_ptr() as u64, name.len() as u64, dir_only as u64, 0, 0]))?;
            None
        }
        Node::Tmp(d) => Some(Gone::Tmp(d.unlink(&name, dir_only)?)),
        Node::Data(d) => datafs::unlink(d, &name, dir_only)?.map(Gone::Data),
        // Nothing of /proc and /sys can be removed (Linux: EPERM).
        Node::Proc(d) => {
            crate::procfs::lookup(d, &name)?;
            return Err(EPERM);
        }
    };
    // As Linux: the link count's IN_ATTRIB, the name's IN_DELETE, then
    // IN_DELETE_SELF once the inode goes.
    if inotify::active() {
        if let Some(g) = &gone {
            g.attrib(dir_only);
        }
        inotify::child(&dir.node, inotify::IN_DELETE, &name, dir_only);
        if let Some(g) = &gone {
            g.finish(dir_only);
        }
    }
    Ok(0)
}

fn symlinkat(target: u64, dirfd: u64, addr: u64) -> Result<i64, i64> {
    let target = read_cstr(target)?;
    let (dir, name) = parent_at(dirfd, addr).map_err(|e| if e == EBUSY { EEXIST } else { e })?;
    match &dir.node {
        Node::Kernel(d) => check(syscall(
            SYS_INODE_SYMLINK,
            [d.handle(), name.as_ptr() as u64, name.len() as u64, target.as_ptr() as u64, target.len() as u64, 0],
        )),
        Node::Tmp(d) => d.symlink(&name, target).map(|_| 0),
        Node::Data(d) => datafs::create(d, &name, datafs::New::Symlink(&target), 0o777).map(|_| 0),
        Node::Proc(d) if crate::procfs::lookup(d, &name).is_ok() => Err(EEXIST),
        Node::Proc(_) => Err(EPERM),
    }?;
    inotify::child(&dir.node, inotify::IN_CREATE, &name, false);
    Ok(0)
}

fn readlinkat(dirfd: u64, addr: u64, buf: u64, size: u64) -> Result<i64, i64> {
    // An empty path: the symlink `dirfd` names itself (an O_PATH|O_NOFOLLOW
    // descriptor's), EINVAL for another descriptor, ENOENT without one (Linux's).
    let target = if path_is_empty(addr) {
        if dirfd as i32 == AT_FDCWD {
            return Err(ENOENT);
        }
        match files::origin_of(dirfd) {
            Some(o) if o.kind == vfs::S_IFLNK => o.origin().node.readlink()?,
            _ => {
                files::stat_of(dirfd)?;
                return Err(EINVAL);
            }
        }
    } else {
        at(dirfd, addr, false)?.node.readlink()?
    };
    let n = target.len().min(size as usize);
    usercopy::to_program(buf, &target.as_bytes()[..n])?;
    Ok(n as i64)
}

fn chmodat(dirfd: u64, addr: u64, mode: u64) -> Result<i64, i64> {
    let r = at(dirfd, addr, true)?;
    chmod_node(&r.node, mode)?;
    attrib(&r.node, &join(&r.path));
    Ok(0)
}

fn chmod_node(node: &Node, mode: u64) -> Result<i64, i64> {
    match node {
        Node::Kernel(k) => check(syscall(SYS_INODE_CHMOD, [k.handle(), mode, 0, 0, 0, 0])),
        // The modes of /proc's and /sys's files are what they are (Linux's
        // proc_setattr: EPERM).
        Node::Proc(_) => Err(EPERM),
        Node::Tmp(t) => {
            t.set_perm(mode as u32);
            Ok(0)
        }
        Node::Data(d) => datafs::chmod(d, mode as u32).map(|_| 0),
    }
}

/// IN_ATTRIB for an inode, and its directory.
fn attrib(node: &Node, _path: &str) {
    inotify::node_event(node, inotify::IN_ATTRIB);
}

/// The file a descriptor names and the path it was opened by, for the
/// calls that change its inode (fchmod, fchown, futimens): None for one
/// without a path of its own (a pipe, a socket, an eventfd), whose inode is
/// anonymous.
/// An O_PATH descriptor is EBADF here (it changes nothing through itself), unless
/// `path_ok` (an *at call with AT_EMPTY_PATH, which takes one).
fn fd_inode(fd: u64, path_ok: bool) -> Result<Option<(Node, String)>, i64> {
    const EBADF: i64 = 9;
    match fd_node_of(fd) {
        Ok((_, _, true)) if !path_ok => Err(EBADF),
        Ok((node, path, _)) => Ok(Some((node, path))),
        Err(ENOTDIR) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The working directory and its path.
fn cwd_inode() -> Result<(Node, String), i64> {
    let cwd = records::current().state.lock().cwd.clone();
    let node = resolve(&cwd, &cwd, true)?.node;
    Ok((node, cwd))
}

/// The inode an *at call with AT_EMPTY_PATH and an empty path names:
/// `dirfd` itself, or the working directory.
fn empty_path_target(dirfd: u64) -> Result<Option<(Node, String)>, i64> {
    if dirfd as i32 == AT_FDCWD { cwd_inode().map(Some) } else { fd_inode(dirfd, true) }
}

/// fchmod(fd, mode). An anonymous inode's mode (a pipe's, a socket's) is
/// fixed here: the call succeeds without changing it.
fn fchmod(fd: u64, mode: u64) -> Result<i64, i64> {
    chmod_target(fd_inode(fd, false)?, mode)
}

fn chmod_target(target: Option<(Node, String)>, mode: u64) -> Result<i64, i64> {
    if let Some((node, path)) = target {
        chmod_node(&node, mode)?;
        attrib(&node, &path);
    }
    Ok(0)
}

/// fchmodat2(dirfd, path, mode, flags): with AT_SYMLINK_NOFOLLOW a symlink
/// itself (EOPNOTSUPP: Linux's filesystems keep no mode for one), with
/// AT_EMPTY_PATH and an empty path `dirfd` itself.
fn fchmodat2(dirfd: u64, addr: u64, mode: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    if flags & AT_EMPTY_PATH != 0 && path_is_empty(addr) {
        return chmod_target(empty_path_target(dirfd)?, mode);
    }
    let r = at(dirfd, addr, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    if r.mode & vfs::S_IFMT == vfs::S_IFLNK {
        return Err(EOPNOTSUPP);
    }
    chmod_target(Some((r.node, join(&r.path))), mode)
}

/// Sets the times of a target. The kernel's tree (/dev), /proc and /sys
/// keep no times that change: the server keeps them
/// (`namespace::set_pseudo_times`). An anonymous inode's (a pipe's, a
/// socket's) are not kept: nothing to do.
fn set_times(target: Option<(Node, String)>, atime: SetTime, mtime: SetTime) -> Result<i64, i64> {
    let Some((node, path)) = target else { return Ok(0) };
    match &node {
        Node::Kernel(_) | Node::Proc(_) => crate::namespace::set_pseudo_times(&node.stat()?, atime, mtime),
        Node::Tmp(t) => t.set_times(atime, mtime),
        Node::Data(d) => datafs::set_times(d, atime, mtime)?,
    }
    if atime != SetTime::Omit || mtime != SetTime::Omit {
        attrib(&node, &path);
    }
    Ok(0)
}

/// The target of the utimes family: the path at `addr` (resolved from
/// `dirfd`, following a final symlink with `follow`), or with a null path
/// the descriptor `dirfd` itself.
fn times_target(dirfd: u64, addr: u64, follow: bool) -> Result<Option<(Node, String)>, i64> {
    if addr == 0 {
        if dirfd as i32 == AT_FDCWD {
            return Err(EFAULT);
        }
        return fd_inode(dirfd, false);
    }
    let r = at(dirfd, addr, follow)?;
    Ok(Some((r.node, join(&r.path))))
}

/// utimes(path, tv) and futimesat(dirfd, path, tv): microseconds; no
/// times means now.
fn utimes(dirfd: u64, addr: u64, tv: u64) -> Result<i64, i64> {
    let (atime, mtime) = if tv == 0 {
        (SetTime::Now, SetTime::Now)
    } else {
        let [asec, ausec, msec, musec]: [i64; 4] = usercopy::read(tv)?;
        let one = |sec: i64, usec: i64| if (0..1_000_000).contains(&usec) { Ok(SetTime::To(vfs::stat::Time { sec, nsec: usec as u32 * 1000 })) } else { Err(EINVAL) };
        (one(asec, ausec)?, one(msec, musec)?)
    };
    set_times(times_target(dirfd, addr, true)?, atime, mtime)
}

/// utimensat(dirfd, path, times, flags): nanoseconds, UTIME_NOW and
/// UTIME_OMIT; no times means now. A null path (futimens) or an empty
/// one with AT_EMPTY_PATH is `dirfd` itself.
fn utimensat(dirfd: u64, addr: u64, ts: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let (atime, mtime) = if ts == 0 {
        (SetTime::Now, SetTime::Now)
    } else {
        let [asec, ansec, msec, mnsec]: [i64; 4] = usercopy::read(ts)?;
        (SetTime::from_timespec(asec, ansec)?, SetTime::from_timespec(msec, mnsec)?)
    };
    // futimens (a null path) takes no flags.
    if addr == 0 && flags != 0 {
        return Err(EINVAL);
    }
    let target = if addr != 0 && flags & AT_EMPTY_PATH != 0 && path_is_empty(addr) {
        empty_path_target(dirfd)?
    } else {
        times_target(dirfd, addr, flags & AT_SYMLINK_NOFOLLOW == 0)?
    };
    set_times(target, atime, mtime)
}

/// The chown family: owners are not stored (one user: every file is
/// root's, as stat says), so a change of owner checks its target and
/// arguments and keeps it root's (an IN_ATTRIB all the same, as Linux
/// sends for any chown). `follow`: chown resolves a final symlink, lchown
/// does not.
fn chownat(dirfd: u64, addr: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let target = if flags & AT_EMPTY_PATH != 0 && path_is_empty(addr) {
        empty_path_target(dirfd)?
    } else {
        let r = at(dirfd, addr, flags & AT_SYMLINK_NOFOLLOW == 0)?;
        Some((r.node, join(&r.path)))
    };
    chown_target(target)
}

fn chown_target(target: Option<(Node, String)>) -> Result<i64, i64> {
    if let Some((node, path)) = target {
        attrib(&node, &path);
    }
    Ok(0)
}

/// truncate(2): with the right to write, as through a writable descriptor.
fn truncate(node: &Node, len: u64) -> Result<i64, i64> {
    match node {
        Node::Kernel(k) => check(syscall(SYS_INODE_TRUNCATE, [k.handle(), len, 0, 0, 0, 0])),
        Node::Tmp(t) => {
            let object = t.object()?;
            t.get_write()?;
            let r = check(syscall(SYS_MO_TRUNCATE, [object, len, 0, 0, 0, 0]));
            t.put_write();
            if r.is_ok() {
                t.modified();
            }
            r
        }
        Node::Data(d) => {
            if d.kind == vfs::S_IFDIR {
                return Err(datafs::EISDIR);
            }
            datafs::get_write(d)?;
            let r = datafs::truncate(d, len).map(|_| 0);
            datafs::put_write(d);
            r
        }
        // As open for writing: /proc's and /sys's files are read-only.
        Node::Proc(p) if crate::procfs::is_dir(p) => Err(datafs::EISDIR),
        Node::Proc(_) => Err(EACCES),
    }
}

fn statfs(addr: u64, buf: u64) -> Result<i64, i64> {
    statfs_node(&at(CWD, addr, true)?.node, buf)
}

/// The `struct statfs` of the filesystem `node` is on, to `buf`.
pub fn statfs_node(node: &Node, buf: u64) -> Result<i64, i64> {
    match node {
        Node::Kernel(k) => {
            let mut words = [0u8; 120];
            check(syscall(SYS_INODE_STATFS, [k.handle(), words.as_mut_ptr() as u64, 0, 0, 0, 0]))?;
            usercopy::to_program(buf, &words)?;
            Ok(0)
        }
        Node::Tmp(_) => tmpfile::statfs(buf),
        Node::Data(_) => {
            usercopy::to_program(buf, &datafs::statfs()?)?;
            Ok(0)
        }
        Node::Proc(p) => {
            usercopy::to_program(buf, &crate::procfs::statfs(p))?;
            Ok(0)
        }
    }
}

