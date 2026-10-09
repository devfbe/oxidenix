//! The server's namespace (phase R6c.2): mounts and path resolution.
//!
//! A mount puts a filesystem at a path: the server's tmpfs (`tmpfs`) at
//! the root and as devpts at /dev/pts (`pty`), diskfs's disk at /data
//! (`datafs`, over the rings), or a
//! directory of the kernel's tree (/dev, /proc, /sys), reached through
//! handles on its inodes (`restricted::SYS_INODE_*`), until the server's
//! own filesystems serve them. Mounts are found by name, as everything here: ".." is resolved
//! lexically before symlinks are looked at (as the kernel's VFS did:
//! "/a/link/.." is "/a"), so the longest mount whose path begins a
//! normalized path is the filesystem that path lies in. Each symlink is
//! read and resolution starts over from the root with its target in front
//! of the rest (at most 16, else ELOOP). In the kernel's tree one call
//! walks as many names as it can and stops after a symlink, so a path
//! without symlinks costs one call; the tmpfs is walked in the server,
//! /data one `LOOKUP` per name.

use crate::datafs::{self, DInode};
use crate::sync::Mutex;
use crate::syscall;
use crate::tmpfs;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use restricted::*;
use vfs::path::{join, normalize};

pub const ENOENT: i64 = 2;
pub const EEXIST: i64 = 17;
pub const EXDEV: i64 = 18;
pub const ENOTDIR: i64 = 20;
pub const EBUSY: i64 = 16;
pub const ELOOP: i64 = 40;

const MAX_LINKS: u32 = 16;

/// A handle on an inode of the kernel's tree, closed when dropped.
pub struct KInode(u64);

impl KInode {
    pub fn handle(&self) -> u64 {
        self.0
    }

    /// Takes a handle the kernel returned (or its error).
    pub fn from_result(r: i64) -> Result<KInode, i64> {
        if r < 0 { Err(-r) } else { Ok(KInode(r as u64)) }
    }

    fn stat(&self) -> Result<[u8; 144], i64> {
        let mut st = [0u8; 144];
        check(syscall(SYS_INODE_STAT, [self.0, st.as_mut_ptr() as u64, 0, 0, 0, 0]))?;
        kernel_times(&mut st);
        Ok(st)
    }

    fn readlink(&self) -> Result<String, i64> {
        let mut buf = alloc::vec![0u8; 4096];
        let n = check(syscall(SYS_INODE_READLINK, [self.0, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]))?;
        buf.truncate(n as usize);
        String::from_utf8(buf).map_err(|_| ENOENT)
    }
}

impl Drop for KInode {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.0, 0, 0, 0, 0, 0]);
    }
}

/// The times set on files of the kernel's tree (/dev, /proc, /sys), which
/// keeps none that change: as Linux's devtmpfs, procfs and sysfs take
/// utimensat, the server keeps them (atime, mtime, ctime) by device and
/// inode number, for the instance's life, at most `KERNEL_TIMES_MAX` of
/// them (the oldest set go first, as a pseudo file's inode would be
/// evicted).
struct KernelTimes {
    times: BTreeMap<(u64, u64), [vfs::stat::Time; 3]>,
    order: VecDeque<(u64, u64)>,
}

const KERNEL_TIMES_MAX: usize = 1024;

static KERNEL_TIMES: Mutex<KernelTimes> = Mutex::new(KernelTimes { times: BTreeMap::new(), order: VecDeque::new() });

/// Sets the times of the kernel's file whose `struct stat` is `st`.
pub fn set_kernel_times(st: &[u8; 144], atime: vfs::stat::SetTime, mtime: vfs::stat::SetTime) {
    if atime == vfs::stat::SetTime::Omit && mtime == vfs::stat::SetTime::Omit {
        return;
    }
    let now = crate::time::realtime();
    let current = vfs::stat::Stat::from_bytes(st);
    let key = (current.dev, current.ino);
    let mut k = KERNEL_TIMES.lock();
    let mut t = k.times.get(&key).copied().unwrap_or([current.atime, current.mtime, current.ctime]);
    if let Some(a) = atime.resolve(now) {
        t[0] = a;
    }
    if let Some(m) = mtime.resolve(now) {
        t[1] = m;
    }
    t[2] = now;
    if k.times.insert(key, t).is_none() {
        k.order.push_back(key);
        if k.order.len() > KERNEL_TIMES_MAX {
            if let Some(old) = k.order.pop_front() {
                k.times.remove(&old);
            }
        }
    }
}

/// Puts the times set here into a kernel file's `struct stat`.
pub fn kernel_times(st: &mut [u8; 144]) {
    let s = vfs::stat::Stat::from_bytes(st);
    if let Some(t) = KERNEL_TIMES.lock().times.get(&(s.dev, s.ino)) {
        *st = vfs::stat::Stat { atime: t[0], mtime: t[1], ctime: t[2], ..s }.to_bytes();
    }
}

pub fn check(r: i64) -> Result<i64, i64> {
    if r < 0 { Err(-r) } else { Ok(r) }
}

pub fn mode_of(st: &[u8; 144]) -> u32 {
    u32::from_le_bytes([st[24], st[25], st[26], st[27]])
}

/// An inode of any of the filesystems.
pub enum Node {
    Kernel(KInode),
    Tmp(Arc<tmpfs::Inode>),
    Data(Arc<DInode>),
}

impl Node {
    /// Its status, with the birth time where the filesystem has one
    /// (statx).
    pub fn status(&self) -> Result<vfs::stat::Stat, i64> {
        match self {
            Node::Tmp(t) => Ok(t.status()),
            _ => self.stat().map(|st| vfs::stat::Stat::from_bytes(&st)),
        }
    }

    pub fn stat(&self) -> Result<[u8; 144], i64> {
        match self {
            Node::Kernel(k) => k.stat(),
            Node::Tmp(t) => Ok(t.stat()),
            Node::Data(d) => datafs::stat(d),
        }
    }

    pub fn readlink(&self) -> Result<String, i64> {
        match self {
            Node::Kernel(k) => k.readlink(),
            Node::Tmp(t) => t.readlink(),
            Node::Data(d) => datafs::readlink(d),
        }
    }
}

/// What a mount puts at its path.
enum Fs {
    /// The kernel's tree at this path (names below its root).
    Kernel(Vec<String>),
    Tmpfs(Arc<tmpfs::Inode>),
    /// diskfs's disk.
    Data,
}

struct Mount {
    at: Vec<String>,
    fs: Fs,
}

/// The directories of the kernel's tree the namespace shows: the devices,
/// procfs and sysfs, until the server serves them.
const KERNEL_MOUNTS: [&str; 3] = ["dev", "proc", "sys"];
/// Where diskfs's disk is mounted.
const DATA_MOUNT: &str = "data";

/// The mount table: the instance's own tmpfs at the root, unpacked from
/// the initramfs when the instance first resolves a path, /data, and the
/// kernel's directories above.
static MOUNTS: Mutex<Vec<Mount>> = Mutex::new(Vec::new());

fn with_mounts<R>(f: impl FnOnce(&Vec<Mount>) -> R) -> R {
    let mut m = MOUNTS.lock();
    if m.is_empty() {
        let root = tmpfs::new_root();
        root.set_perm(0o755);
        crate::initramfs::unpack(&root);
        let _ = root.subdir("tmp", 0o1777);
        m.push(Mount { at: Vec::new(), fs: Fs::Tmpfs(root.clone()) });
        for name in KERNEL_MOUNTS {
            // The mount point, so that the root lists it.
            let _ = root.subdir(name, 0o755);
            m.push(Mount { at: alloc::vec![String::from(name)], fs: Fs::Kernel(alloc::vec![String::from(name)]) });
        }
        let _ = root.subdir(DATA_MOUNT, 0o755);
        m.push(Mount { at: alloc::vec![String::from(DATA_MOUNT)], fs: Fs::Data });
        // devpts (the ptys' nodes, `pty`), on the kernel's /dev/pts.
        m.push(Mount { at: alloc::vec![String::from("dev"), String::from("pts")], fs: Fs::Tmpfs(crate::pty::devpts()) });
    }
    f(&m)
}

/// The mount `comps` lies in: its filesystem and how many of `comps` name
/// the mount point.
fn mount_of(comps: &[String]) -> (Fs, usize) {
    with_mounts(|mounts| {
        let m = mounts
            .iter()
            .filter(|m| comps.len() >= m.at.len() && comps[..m.at.len()] == m.at[..])
            .max_by_key(|m| m.at.len())
            .expect("the root is mounted");
        let fs = match &m.fs {
            Fs::Kernel(base) => Fs::Kernel(base.clone()),
            Fs::Tmpfs(root) => Fs::Tmpfs(root.clone()),
            Fs::Data => Fs::Data,
        };
        (fs, m.at.len())
    })
}

/// Whether `comps` is a mount point (other than the root): it cannot be
/// removed, renamed or replaced.
pub fn is_mount_point(comps: &[String]) -> bool {
    !comps.is_empty() && with_mounts(|mounts| mounts.iter().any(|m| m.at[..] == comps[..]))
}

/// The handle on the kernel's root, taken once for the instance and never
/// closed.
pub fn kernel_root() -> u64 {
    static ROOT: AtomicU64 = AtomicU64::new(0);
    let h = ROOT.load(Ordering::Acquire);
    if h != 0 {
        return h;
    }
    let new = syscall(SYS_INODE_ROOT, [0; 6]);
    if new <= 0 {
        // No handle left: the walks fail with it.
        return 0;
    }
    match ROOT.compare_exchange(0, new as u64, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => new as u64,
        Err(other) => {
            syscall(SYS_HANDLE_CLOSE, [new as u64, 0, 0, 0, 0, 0]);
            other
        }
    }
}

/// A resolved path: its inode, its mode (of a /data inode: its type
/// only), and its absolute path as given
/// (normalized, symlinks not replaced: what the descriptor and the working
/// directory remember).
pub struct Resolved {
    pub node: Node,
    pub mode: u32,
    pub path: Vec<String>,
}

/// One step of a walk in one filesystem: where it ended, or the symlink it
/// stopped at after `n` of the names.
enum Step {
    Done(Node, u32),
    Link(Node, usize),
}

/// Walks `names` in the kernel's tree below `base`.
fn walk_kernel(base: &[String], names: &[String]) -> Result<Step, i64> {
    let mut rel = String::new();
    for c in base.iter().chain(names) {
        if !rel.is_empty() {
            rel.push('/');
        }
        rel.push_str(c);
    }
    let mut walk = Walk::default();
    let inode = KInode::from_result(syscall(
        SYS_INODE_WALK,
        [kernel_root(), rel.as_ptr() as u64, rel.len() as u64, &mut walk as *mut Walk as u64, 0, 0],
    ))?;
    let walked = if rel.is_empty() { 0 } else { rel[..walk.consumed as usize].split('/').count() };
    let in_names = walked.saturating_sub(base.len());
    if walk.mode & vfs::S_IFMT == vfs::S_IFLNK && in_names > 0 {
        return Ok(Step::Link(Node::Kernel(inode), in_names));
    }
    if in_names != names.len() {
        // A walk stops early only at a symlink (one in the mount's own
        // path is the kernel's business: refuse it).
        return Err(ENOENT);
    }
    Ok(Step::Done(Node::Kernel(inode), walk.mode))
}

/// Walks `names` in the tmpfs from `root`; `follow`: also at the last name.
fn walk_tmpfs(root: &Arc<tmpfs::Inode>, names: &[String], follow: bool) -> Result<Step, i64> {
    let mut cur = root.clone();
    for (i, name) in names.iter().enumerate() {
        let child = cur.lookup(name)?;
        let mode = child.mode();
        if mode & vfs::S_IFMT == vfs::S_IFLNK && (follow || i + 1 < names.len()) {
            return Ok(Step::Link(Node::Tmp(child), i + 1));
        }
        cur = child;
    }
    let mode = cur.mode();
    Ok(Step::Done(Node::Tmp(cur), mode))
}

/// Walks `names` in /data from its root; `follow`: also at the last name.
fn walk_data(names: &[String], follow: bool) -> Result<Step, i64> {
    let mut cur = datafs::root()?;
    for (i, name) in names.iter().enumerate() {
        if cur.kind != vfs::S_IFDIR {
            return Err(ENOTDIR);
        }
        let child = datafs::lookup(&cur, name)?;
        if child.kind == vfs::S_IFLNK && (follow || i + 1 < names.len()) {
            return Ok(Step::Link(Node::Data(child), i + 1));
        }
        cur = child;
    }
    // Its type (what resolution's callers look at): no STAT for the
    // permissions.
    let mode = cur.kind | 0o777;
    Ok(Step::Done(Node::Data(cur), mode))
}

/// Resolves `path` relative to the absolute directory `base`; `follow`:
/// a symlink as the last name is followed.
pub fn resolve(base: &str, path: &str, follow: bool) -> Result<Resolved, i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    let given = normalize(base, path);
    let mut comps: VecDeque<String> = given.clone().into();
    let mut links = 0;
    loop {
        let all: Vec<String> = comps.iter().cloned().collect();
        let (fs, at) = mount_of(&all);
        let names = &all[at..];
        let step = match &fs {
            Fs::Kernel(base) => walk_kernel(base, names)?,
            Fs::Tmpfs(root) => walk_tmpfs(root, names, follow)?,
            Fs::Data => walk_data(names, follow)?,
        };
        let (link, walked) = match step {
            Step::Done(node, mode) => return Ok(Resolved { node, mode, path: given }),
            Step::Link(node, n) => (node, at + n),
        };
        // The kernel's walk stops at any symlink: the last one is followed
        // only when asked for.
        if walked == all.len() && !follow {
            let mode = mode_of(&link.stat()?);
            return Ok(Resolved { node: link, mode, path: given });
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(ELOOP);
        }
        let target = link.readlink()?;
        let mut next: VecDeque<String> = normalize(&join(&all[..walked - 1]), &target).into();
        next.extend(all[walked..].iter().cloned());
        comps = next;
    }
}

/// The directory a new name goes into and the name (EEXIST for the root,
/// which has none; EBUSY for a mount point).
pub fn resolve_parent(base: &str, path: &str) -> Result<(Resolved, String), i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    let mut comps = normalize(base, path);
    if is_mount_point(&comps) {
        return Err(EBUSY);
    }
    let name = comps.pop().ok_or(EEXIST)?;
    let parent = resolve("/", &join(&comps), true)?;
    if parent.mode & vfs::S_IFMT != vfs::S_IFDIR {
        return Err(ENOTDIR);
    }
    Ok((parent, name))
}
