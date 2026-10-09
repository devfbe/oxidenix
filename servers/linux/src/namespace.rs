//! The server's namespace (phase R6c.2): mounts and path resolution.
//!
//! A mount puts a filesystem at a path: the server's tmpfs (`tmpfs`) at
//! the root, at /dev (devtmpfs: the device nodes and the descriptors'
//! links, made when the instance starts; R9, until then the kernel's tree)
//! and as devpts at /dev/pts (`pty`), diskfs's disk at /data (`datafs`,
//! over the rings), /proc and /sys (`procfs`: procfs's files over the
//! rings and the server's own per-process part). Mounts are found by
//! name, as everything here: ".." is resolved
//! lexically before symlinks are looked at (as the kernel's VFS did:
//! "/a/link/.." is "/a"), so the longest mount whose path begins a
//! normalized path is the filesystem that path lies in. Each symlink is
//! read and resolution starts over from the root with its target in front
//! of the rest (at most 16, else ELOOP); /proc's magic links
//! (/proc/<pid>/fd/N) lead to the open file itself instead (`procfs::follow`). The tmpfs and
//! /proc are walked in the server, /data one `LOOKUP` per name.

use crate::datafs::{self, DInode};
use crate::procfs::{self, ProcNode};
use crate::sync::Mutex;
use crate::tmpfs;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::path::{join, normalize};

pub const ENOENT: i64 = 2;
pub const EEXIST: i64 = 17;
pub const EXDEV: i64 = 18;
pub const ENOTDIR: i64 = 20;
pub const EBUSY: i64 = 16;
pub const ELOOP: i64 = 40;

const MAX_LINKS: u32 = 16;

/// The times set on pseudo files, which keep none that change: /proc and
/// /sys. As Linux's procfs and sysfs take utimensat, the server keeps them (atime, mtime, ctime) by
/// device and inode number, for the instance's life, at most
/// `PSEUDO_TIMES_MAX` of them (the oldest set go first, as a pseudo file's
/// inode would be evicted).
struct PseudoTimes {
    times: BTreeMap<(u64, u64), [vfs::stat::Time; 3]>,
    order: VecDeque<(u64, u64)>,
}

const PSEUDO_TIMES_MAX: usize = 1024;

static PSEUDO_TIMES: Mutex<PseudoTimes> = Mutex::new(PseudoTimes { times: BTreeMap::new(), order: VecDeque::new() });

/// Sets the times of the pseudo file whose `struct stat` is `st`.
pub fn set_pseudo_times(st: &[u8; 144], atime: vfs::stat::SetTime, mtime: vfs::stat::SetTime) {
    if atime == vfs::stat::SetTime::Omit && mtime == vfs::stat::SetTime::Omit {
        return;
    }
    let now = crate::time::realtime();
    let current = vfs::stat::Stat::from_bytes(st);
    let key = (current.dev, current.ino);
    let mut k = PSEUDO_TIMES.lock();
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
        if k.order.len() > PSEUDO_TIMES_MAX {
            if let Some(old) = k.order.pop_front() {
                k.times.remove(&old);
            }
        }
    }
}

/// Puts the times set here into a pseudo file's `struct stat`.
pub fn pseudo_times(st: &mut [u8; 144]) {
    let s = vfs::stat::Stat::from_bytes(st);
    if let Some(t) = PSEUDO_TIMES.lock().times.get(&(s.dev, s.ino)) {
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
    Tmp(Arc<tmpfs::Inode>),
    Data(Arc<DInode>),
    /// One of /proc or /sys (procfs's, or the server's own).
    Proc(ProcNode),
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
            Node::Tmp(t) => Ok(t.stat()),
            Node::Data(d) => datafs::stat(d),
            Node::Proc(p) => procfs::stat(p),
        }
    }

    pub fn readlink(&self) -> Result<String, i64> {
        match self {
            Node::Tmp(t) => t.readlink(),
            Node::Data(d) => datafs::readlink(d),
            Node::Proc(p) => procfs::readlink(p),
        }
    }
}

impl Node {
    /// Another reference to the same inode.
    pub fn duplicate(&self) -> Result<Node, i64> {
        Ok(match self {
            Node::Tmp(t) => Node::Tmp(t.clone()),
            Node::Data(d) => Node::Data(d.clone()),
            Node::Proc(p) => Node::Proc(p.clone()),
        })
    }
}

/// What an open file description of the server's that is not a regular file or a
/// directory (a terminal, null or zero, an `O_PATH` descriptor) was opened by: the node
/// and its absolute path. Its status is the node's, live (fstat), and the calls on the
/// descriptor that change the node (fchmod, fchown, futimens) or take it as a directory
/// work on it.
pub struct Origin {
    pub node: Node,
    pub path: String,
    /// The node's file type (`S_IFMT` bits) when it was opened (it never changes).
    pub kind: u32,
}

impl Origin {
    pub fn new(node: Node, path: String, mode: u32) -> Origin {
        Origin { node, path, kind: mode & vfs::S_IFMT }
    }

    pub fn stat(&self) -> Result<[u8; 144], i64> {
        self.node.stat()
    }
}

impl Drop for Origin {
    /// A /data inode is let go of as an open file's is (`datafs::let_go`): unlinked, it
    /// goes with its blocks at the end of the call that closed the last descriptor.
    fn drop(&mut self) {
        if let Node::Data(d) = &self.node {
            datafs::let_go(d);
        }
    }
}

/// What a mount puts at its path.
#[derive(Clone)]
enum Fs {
    Tmpfs(Arc<tmpfs::Inode>),
    /// diskfs's disk.
    Data,
    /// /proc (procfs's system-wide files and the server's per-process
    /// ones), or /sys (procfs's).
    Proc,
    Sys,
}

struct Mount {
    at: Vec<String>,
    fs: Fs,
    /// What /proc/<pid>/mounts says: the source and the filesystem's type.
    source: &'static str,
    fstype: &'static str,
}

/// The mount table: the instance's own tmpfs at the root, unpacked from
/// the initramfs when the instance first resolves a path, /dev (`dev`),
/// /proc and /sys, /data and devpts.
static MOUNTS: Mutex<Vec<Mount>> = Mutex::new(Vec::new());

fn with_mounts<R>(f: impl FnOnce(&Vec<Mount>) -> R) -> R {
    let mut m = MOUNTS.lock();
    if m.is_empty() {
        let root = tmpfs::new_root(tmpfs::DEV);
        root.set_perm(0o755);
        crate::initramfs::unpack(&root);
        let _ = root.subdir("tmp", 0o1777);
        m.push(Mount { at: Vec::new(), fs: Fs::Tmpfs(root.clone()), source: "rootfs", fstype: "tmpfs" });
        let path = |name: &str| alloc::vec![String::from(name)];
        let mounts = [
            ("dev", Fs::Tmpfs(dev()), "devtmpfs", "devtmpfs"),
            ("proc", Fs::Proc, "proc", "proc"),
            ("sys", Fs::Sys, "sysfs", "sysfs"),
            ("data", Fs::Data, "/dev/vda", "ext2"),
        ];
        for (name, fs, source, fstype) in mounts {
            // The mount point, so that the root lists it.
            let _ = root.subdir(name, 0o755);
            m.push(Mount { at: path(name), fs, source, fstype });
        }
        // devpts (the ptys' nodes, `pty`), on /dev/pts.
        let at = alloc::vec![String::from("dev"), String::from("pts")];
        m.push(Mount { at, fs: Fs::Tmpfs(crate::pty::devpts()), source: "devpts", fstype: "devpts" });
    }
    f(&m)
}

/// The instance's /dev, as Linux's devtmpfs with udev's links: the
/// terminals (`tty`: the controlling terminal, the console, ptmx), null
/// and zero (`devices`), `fd`, `stdin`, `stdout` and `stderr` as links into
/// /proc/self/fd, the mount point of devpts and `shm` for POSIX shared
/// memory. A tmpfs like the root (root may make files and directories in it).
fn dev() -> Arc<tmpfs::Inode> {
    use vfs::stat::dev_make;
    let dev = tmpfs::new_root(tmpfs::DEVTMPFS_DEV);
    dev.set_perm(0o755);
    let nodes = [
        ("console", dev_make(5, 1), 0o600),
        ("tty", dev_make(5, 0), 0o666),
        ("ptmx", dev_make(5, 2), 0o666),
        ("null", dev_make(1, 3), 0o666),
        ("zero", dev_make(1, 5), 0o666),
    ];
    for (name, rdev, perm) in nodes {
        let _ = dev.insert_device(name, rdev, perm);
    }
    for (name, target) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
        let _ = dev.symlink(name, String::from(target));
    }
    let _ = dev.subdir("pts", 0o755);
    let _ = dev.subdir("shm", 0o1777);
    dev
}

/// The mount table as /proc/<pid>/mounts shows it (proc_pid_mounts(5)).
pub fn mounts_text() -> String {
    with_mounts(|mounts| {
        let mut out = String::new();
        for m in mounts {
            out.push_str(&alloc::format!("{} {} {} rw 0 0\n", m.source, join(&m.at), m.fstype));
        }
        out
    })
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
        (m.fs.clone(), m.at.len())
    })
}

/// Whether `comps` is a mount point (other than the root): it cannot be
/// removed, renamed or replaced.
pub fn is_mount_point(comps: &[String]) -> bool {
    !comps.is_empty() && with_mounts(|mounts| mounts.iter().any(|m| m.at[..] == comps[..]))
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

/// Walks `names` in the tmpfs from `root`; `follow`: also at the last name.
fn walk_tmpfs(root: &Arc<tmpfs::Inode>, names: &[String], follow: bool) -> Result<Step, i64> {
    let mut cur = root.clone();
    for (i, name) in names.iter().enumerate() {
        let child = cur.lookup(name)?;
        if child.file_type() == vfs::S_IFLNK && (follow || i + 1 < names.len()) {
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

/// Walks `names` in /proc or /sys from its root; `follow`: also at the
/// last name.
fn walk_proc(sys: bool, names: &[String], follow: bool) -> Result<Step, i64> {
    let mut cur = procfs::root(sys);
    for (i, name) in names.iter().enumerate() {
        let child = procfs::lookup(&cur, name)?;
        if procfs::mode(&child) & vfs::S_IFMT == vfs::S_IFLNK && (follow || i + 1 < names.len()) {
            return Ok(Step::Link(Node::Proc(child), i + 1));
        }
        cur = child;
    }
    let mode = procfs::mode(&cur);
    Ok(Step::Done(Node::Proc(cur), mode))
}

/// Resolves `path` relative to the absolute directory `base`; `follow`:
/// a symlink as the last name is followed.
pub fn resolve(base: &str, path: &str, follow: bool) -> Result<Resolved, i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    let given = normalize(base, path);
    // The components with the symlinks met so far replaced; None while
    // there were none (then they are `given`, which needs no copy).
    let mut replaced: Option<Vec<String>> = None;
    let mut links = 0;
    loop {
        let all = replaced.as_deref().unwrap_or(&given);
        let (fs, at) = mount_of(all);
        let names = &all[at..];
        let step = match &fs {
            Fs::Tmpfs(root) => walk_tmpfs(root, names, follow)?,
            Fs::Data => walk_data(names, follow)?,
            Fs::Proc => walk_proc(false, names, follow)?,
            Fs::Sys => walk_proc(true, names, follow)?,
        };
        let (link, walked) = match step {
            Step::Done(node, mode) => return Ok(Resolved { node, mode, path: given }),
            Step::Link(node, n) => (node, at + n),
        };
        // A walk stops at a symlink it must follow (a last one only when
        // asked for).
        if walked == all.len() && !follow {
            let mode = mode_of(&link.stat()?);
            return Ok(Resolved { node: link, mode, path: given });
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(ELOOP);
        }
        // A magic link (/proc/<pid>/fd/N) leads to the open file itself:
        // as the last name, its node (an unlinked file, a pipe); with names
        // after it, through the path the file was opened by.
        let target = match &link {
            Node::Proc(p) => match procfs::follow(p)? {
                Some(procfs::Follow { node, mode, path }) if walked == all.len() => {
                    let path = path.map(|p| normalize("/", &p)).unwrap_or(given);
                    return Ok(Resolved { node, mode, path });
                }
                Some(procfs::Follow { path: Some(path), .. }) => path,
                Some(_) => return Err(ENOTDIR),
                None => link.readlink()?,
            },
            _ => link.readlink()?,
        };
        let mut next = normalize(&join(&all[..walked - 1]), &target);
        next.extend(all[walked..].iter().cloned());
        replaced = Some(next);
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
