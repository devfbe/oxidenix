//! In-memory filesystem (tmpfs-like), populated from the initramfs at boot.

pub mod cache;
pub mod cpio;
pub mod file;
pub mod remote;

use crate::process::errno::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use spin::{Mutex, Once};

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFCHR: u32 = 0o020000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// The kernel's own output to the console, for the native servers' standard
    /// output and error (not in /dev: Linux programs' terminals are the Linux
    /// server's, ADR 0007).
    Console,
    Null,
    Zero,
    /// A device the Linux server implements (its terminals: /dev/tty,
    /// /dev/console, /dev/ptmx), named here by its number (major, minor); the
    /// kernel cannot open it (ENXIO).
    Server(u32, u32),
}

impl Device {
    /// Its device number, encoded as Linux's `st_rdev` (`new_encode_dev`).
    pub fn rdev(&self) -> u64 {
        let (major, minor) = match *self {
            Device::Console => (5, 1),
            Device::Null => (1, 3),
            Device::Zero => (1, 5),
            Device::Server(major, minor) => (major, minor),
        };
        (minor as u64 & 0xff) | ((major as u64 & 0xfff) << 8) | ((minor as u64 & !0xff) << 12)
    }
}

/// Inodes, symlink targets and pipe buffers live on the kernel heap; this
/// caps all of them together. (File contents are in the page cache.)
pub const FILE_QUOTA: usize = 8 * 1024 * 1024;
static FILE_BYTES: AtomicUsize = AtomicUsize::new(0);

pub fn charge(bytes: usize) -> Result<(), i64> {
    FILE_BYTES
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|&n| n <= FILE_QUOTA)
        })
        .map(|_| ())
        .map_err(|_| ENOSPC)
}

pub fn release(bytes: usize) {
    FILE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
}

/// Whether `target` is `dir` itself or lies anywhere below it. Iterative,
/// because user space can nest directories deeper than the kernel stack.
pub fn contains(dir: &Arc<Inode>, target: &Arc<Inode>) -> bool {
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        if Arc::ptr_eq(&d, target) {
            return true;
        }
        if let Node::Dir(m) = &*d.node.lock() {
            stack.extend(m.values().cloned());
        }
    }
    false
}

pub enum Node {
    Dir(BTreeMap<String, Arc<Inode>>),
    /// A tmpfs file: its contents are its page cache.
    File(Arc<cache::PageCache>),
    Symlink(String),
    Device(Device),
    /// An inode served by a user-space filesystem server (procfs).
    Disk(DiskRef),
}

#[derive(Clone)]
pub struct DiskRef {
    pub fs: Arc<remote::RemoteFs>,
    pub ino: u32,
}

pub struct InodeStat {
    pub mode: u32,
    /// A device's number (`st_rdev`), else 0.
    pub rdev: u64,
    pub size: u64,
    pub nlink: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

/// What `Inode::create` makes.
pub enum NewNode {
    File,
    Dir,
    Symlink(String),
}

/// Heap cost charged per inode, covering the inode, its directory entry and
/// a name of up to `NAME_MAX` bytes.
const INODE_COST: usize = 512;
pub const NAME_MAX: usize = 255;
/// Disk inode numbers are offset so they never collide with memory inodes.
const DISK_INO_BASE: u64 = 1 << 32;

pub struct Inode {
    pub ino: u64,
    pub perm: Mutex<u32>,
    pub node: Mutex<Node>,
    charged: usize,
    /// Open descriptors that may write (> 0), or address spaces running
    /// the file as their program (< 0): the two exclude each other
    /// (ETXTBSY), since a program's pages are the file's pages.
    writers: AtomicI64,
}

/// The right to write a file, held by a descriptor opened for writing.
pub struct WriteAccess(Arc<Inode>);

impl Drop for WriteAccess {
    fn drop(&mut self) {
        self.0.writers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a mapping of a file holds: the file (alive while mapped, even
/// deleted) and, if it was mapped through a descriptor open for writing,
/// the right to write it, as that descriptor did (so it cannot run as a
/// program while a mapping may still change it).
pub struct MappedFile {
    _inode: Arc<Inode>,
    _write: Option<WriteAccess>,
}

impl MappedFile {
    pub fn new(inode: Arc<Inode>, writable: bool) -> Result<Arc<MappedFile>, i64> {
        let write = if writable { Some(inode.get_write_access()?) } else { None };
        Arc::try_new(MappedFile { _inode: inode, _write: write }).map_err(|_| ENOMEM)
    }
}

/// Held while a file runs as a program: nobody may write it meanwhile.
pub struct DenyWrite(Arc<Inode>);

impl Clone for DenyWrite {
    fn clone(&self) -> DenyWrite {
        // Already denied, so this cannot conflict with a writer.
        self.0.writers.fetch_sub(1, Ordering::AcqRel);
        DenyWrite(self.0.clone())
    }
}

impl Drop for DenyWrite {
    fn drop(&mut self) {
        self.0.writers.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for Inode {
    fn drop(&mut self) {
        release(self.charged);
    }
}

static NEXT_INO: AtomicU64 = AtomicU64::new(1);
static ROOT: Once<Arc<Inode>> = Once::new();

fn dtype(file_type: u32) -> u8 {
    match file_type {
        S_IFDIR => 4,
        S_IFREG => 8,
        S_IFLNK => 10,
        _ => 2,
    }
}

/// The VFS view of an inode. Every operation works for memory inodes and
/// for inodes of user-space filesystem servers alike; callers never look
/// at `node` directly.
impl Inode {
    pub fn new(node: Node, perm: u32) -> Result<Arc<Inode>, i64> {
        let charged = INODE_COST
            + match &node {
                Node::Symlink(t) => t.len(),
                _ => 0,
            };
        charge(charged)?;
        Ok(Arc::new(Inode {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            perm: Mutex::new(perm & 0o7777),
            node: Mutex::new(node),
            charged,
            writers: AtomicI64::new(0),
        }))
    }

    /// Disk inodes are cached by their filesystem and charged no quota.
    pub fn disk(fs: Arc<remote::RemoteFs>, ino: u32) -> Arc<Inode> {
        Arc::new(Inode {
            ino: DISK_INO_BASE | ino as u64,
            perm: Mutex::new(0),
            node: Mutex::new(Node::Disk(DiskRef { fs, ino })),
            charged: 0,
            writers: AtomicI64::new(0),
        })
    }

    /// A copy of the disk reference, so no inode lock is held during I/O.
    fn disk_ref(&self) -> Option<DiskRef> {
        match &*self.node.lock() {
            Node::Disk(d) => Some(d.clone()),
            _ => None,
        }
    }

    pub fn device(&self) -> Option<Device> {
        match &*self.node.lock() {
            Node::Device(d) => Some(*d),
            _ => None,
        }
    }

    pub fn mode(&self) -> u32 {
        if let Some(d) = self.disk_ref() {
            return d.fs.stat(d.ino).map_or(S_IFREG, |s| s.mode);
        }
        let kind = match &*self.node.lock() {
            Node::Dir(_) => S_IFDIR,
            Node::File(_) => S_IFREG,
            Node::Symlink(_) => S_IFLNK,
            Node::Device(_) => S_IFCHR,
            Node::Disk(_) => unreachable!("handled above"),
        };
        kind | *self.perm.lock()
    }

    pub fn file_type(&self) -> u32 {
        self.mode() & S_IFMT
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }

    pub fn size(&self) -> u64 {
        if let Some(d) = self.disk_ref() {
            return d.fs.stat(d.ino).map_or(0, |s| s.size);
        }
        match &*self.node.lock() {
            Node::File(c) => c.size(),
            Node::Symlink(t) => t.len() as u64,
            Node::Dir(m) => m.len() as u64,
            Node::Device(_) | Node::Disk(_) => 0,
        }
    }

    /// Everything stat(2) reports about an inode.
    pub fn stat(&self) -> Result<InodeStat, i64> {
        if let Some(d) = self.disk_ref() {
            // One request instead of three, and errors (e.g. a dead server)
            // are reported instead of being mistaken for defaults.
            let s = d.fs.stat(d.ino)?;
            return Ok(InodeStat { mode: s.mode, rdev: 0, size: s.size, nlink: s.links, atime: s.atime, mtime: s.mtime, ctime: s.ctime });
        }
        let (nlink, atime, mtime, ctime) = self.stat_extra();
        let rdev = self.device().map_or(0, |d| d.rdev());
        Ok(InodeStat { mode: self.mode(), rdev, size: self.size(), nlink, atime, mtime, ctime })
    }

    /// (link count, access time, modification time, change time)
    pub fn stat_extra(&self) -> (u64, u64, u64, u64) {
        if let Some(d) = self.disk_ref() {
            if let Ok(s) = d.fs.stat(d.ino) {
                return (s.links, s.atime, s.mtime, s.ctime);
            }
        }
        let boot = crate::time::boot_time();
        (if self.is_dir() { 2 } else { 1 }, boot, boot, boot)
    }

    pub fn child(&self, name: &str) -> Result<Arc<Inode>, i64> {
        if let Some(d) = self.disk_ref() {
            let ino = d.fs.lookup(d.ino, name)?;
            return Ok(d.fs.inode(ino));
        }
        match &*self.node.lock() {
            Node::Dir(m) => m.get(name).cloned().ok_or(ENOENT),
            _ => Err(ENOTDIR),
        }
    }

    /// Directory entries as (name, inode number, dirent type), including "." and "..".
    pub fn list(&self) -> Result<Vec<(String, u64, u8)>, i64> {
        if let Some(d) = self.disk_ref() {
            let ft = |t: u8| match t {
                2 => 4,
                7 => 10,
                1 => 8,
                _ => 0,
            };
            return Ok(d.fs.list(d.ino)?.into_iter().map(|(n, i, t)| (n, DISK_INO_BASE | i as u64, ft(t))).collect());
        }
        let children: Vec<(String, Arc<Inode>)> = match &*self.node.lock() {
            Node::Dir(m) => m.iter().map(|(n, c)| (n.clone(), c.clone())).collect(),
            _ => return Err(ENOTDIR),
        };
        let mut out = vec![(".".into(), self.ino, 4u8), ("..".into(), self.ino, 4u8)];
        out.extend(children.into_iter().map(|(n, c)| (n, c.ino, dtype(c.file_type()))));
        Ok(out)
    }

    /// Adds an existing memory inode (used at boot and for mounting).
    pub fn insert(&self, name: &str, inode: Arc<Inode>) -> Result<(), i64> {
        if name.len() > NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        match &mut *self.node.lock() {
            Node::Dir(m) if m.contains_key(name) => Err(EEXIST),
            Node::Dir(m) => {
                m.insert(name.to_string(), inode);
                Ok(())
            }
            _ => Err(ENOTDIR),
        }
    }

    pub fn create(&self, name: &str, kind: NewNode, perm: u32) -> Result<Arc<Inode>, i64> {
        if let Some(d) = self.disk_ref() {
            let ino = d.fs.create(d.ino, name, &kind, perm)?;
            return Ok(d.fs.inode(ino));
        }
        let node = match kind {
            NewNode::File => Node::File(cache::PageCache::memory(&[])?),
            NewNode::Dir => Node::Dir(BTreeMap::new()),
            NewNode::Symlink(t) => Node::Symlink(t),
        };
        let inode = Inode::new(node, perm)?;
        self.insert(name, inode.clone())?;
        Ok(inode)
    }

    /// Removes `name`; `dir` selects rmdir semantics.
    pub fn unlink(&self, name: &str, dir: bool) -> Result<(), i64> {
        if let Some(d) = self.disk_ref() {
            return d.fs.unlink(d.ino, name, dir);
        }
        let child = self.child(name)?;
        if child.disk_ref().is_some() {
            return Err(EBUSY); // a mount point
        }
        match (&*child.node.lock(), dir) {
            (Node::Dir(m), true) if !m.is_empty() => return Err(ENOTEMPTY),
            (Node::Dir(_), true) => {}
            (Node::Dir(_), false) => return Err(EISDIR),
            (_, true) => return Err(ENOTDIR),
            (_, false) => {}
        }
        if let Node::Dir(m) = &mut *self.node.lock() {
            m.remove(name);
        }
        Ok(())
    }

    pub fn readlink(&self) -> Result<String, i64> {
        if let Some(d) = self.disk_ref() {
            return d.fs.readlink(d.ino);
        }
        match &*self.node.lock() {
            Node::Symlink(t) => Ok(t.clone()),
            _ => Err(EINVAL),
        }
    }


    /// Only regular files have contents to read or write.
    fn require_regular(&self) -> Result<(), i64> {
        match self.file_type() {
            S_IFREG => Ok(()),
            S_IFDIR => Err(EISDIR),
            _ => Err(EINVAL),
        }
    }

    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        if let Some(d) = self.disk_ref() {
            self.require_regular()?;
            return d.fs.read(d.ino, off, buf);
        }
        self.cache()?.read(off, buf)
    }

    pub fn write_at(&self, off: u64, buf: &[u8]) -> Result<usize, i64> {
        if let Some(d) = self.disk_ref() {
            self.require_regular()?;
            return d.fs.write(d.ino, off, buf);
        }
        self.cache()?.write(off, buf)
    }

    pub fn truncate(&self, len: u64) -> Result<(), i64> {
        if let Some(d) = self.disk_ref() {
            self.require_regular()?;
            return d.fs.truncate(d.ino, len);
        }
        self.cache()?.truncate(len)
    }

    /// The page cache of a regular file (EISDIR, EINVAL for others; ENODEV
    /// for a file of a filesystem server: generated on every read, it has
    /// none). No inode lock is held while the cache is used: its
    /// operations may sleep.
    pub fn cache(&self) -> Result<Arc<cache::PageCache>, i64> {
        if self.disk_ref().is_some() {
            return Err(ENODEV);
        }
        match &*self.node.lock() {
            Node::File(c) => Ok(c.clone()),
            Node::Dir(_) => Err(EISDIR),
            _ => Err(EINVAL),
        }
    }

    pub fn set_perm(&self, perm: u32) -> Result<(), i64> {
        if let Some(d) = self.disk_ref() {
            return d.fs.set_perm(d.ino, perm);
        }
        *self.perm.lock() = perm & 0o7777;
        Ok(())
    }

    /// The right to write this file (ETXTBSY while it runs as a program).
    pub fn get_write_access(self: &Arc<Self>) -> Result<WriteAccess, i64> {
        self.writers.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n >= 0).then_some(n + 1)).map_err(|_| ETXTBSY)?;
        Ok(WriteAccess(self.clone()))
    }

    /// Keeps writers away while the file runs as a program (ETXTBSY if
    /// it is open for writing).
    pub fn deny_write_access(self: &Arc<Self>) -> Result<DenyWrite, i64> {
        self.writers.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n <= 0).then_some(n - 1)).map_err(|_| ETXTBSY)?;
        Ok(DenyWrite(self.clone()))
    }

    /// Filesystem the inode lives on, for statfs.
    pub fn filesystem(&self) -> Option<Arc<remote::RemoteFs>> {
        self.disk_ref().map(|d| d.fs)
    }
}

/// Moves `oname` in `odir` to `nname` in `ndir` (same filesystem only).
pub fn rename(odir: &Arc<Inode>, oname: &str, ndir: &Arc<Inode>, nname: &str) -> Result<(), i64> {
    if nname.len() > NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    match (odir.disk_ref(), ndir.disk_ref()) {
        (Some(a), Some(b)) if Arc::ptr_eq(&a.fs, &b.fs) => return a.fs.rename(a.ino, oname, b.ino, nname),
        (None, None) => {}
        _ => return Err(EXDEV),
    }
    let node = odir.child(oname)?;
    if node.disk_ref().is_some() {
        return Err(EBUSY);
    }
    // Moving a directory below itself would detach it in a reference cycle.
    if contains(&node, ndir) {
        return Err(EINVAL);
    }
    if let Ok(existing) = ndir.child(nname) {
        if Arc::ptr_eq(&existing, &node) {
            return Ok(());
        }
        // Read both properties first: the inode locks are not reentrant.
        let existing_dir = match &*existing.node.lock() {
            Node::Dir(m) => Some(m.is_empty()),
            Node::Disk(_) => return Err(EBUSY),
            _ => None,
        };
        // Replacing only ever drops a file or an empty directory, never a
        // whole subtree (whose recursive drop could overflow the stack).
        match (existing_dir, node.is_dir()) {
            (Some(false), true) => return Err(ENOTEMPTY),
            (Some(_), false) => return Err(EISDIR),
            (None, true) => return Err(ENOTDIR),
            _ => {}
        }
    }
    if let Node::Dir(m) = &mut *odir.node.lock() {
        m.remove(oname);
    }
    let replaced = match &mut *ndir.node.lock() {
        Node::Dir(m) => m.insert(nname.to_string(), node),
        _ => None,
    };
    drop(replaced);
    Ok(())
}

pub fn root() -> Arc<Inode> {
    ROOT.get().expect("fs::init not called").clone()
}

/// The initramfs the kernel booted with.
static INITRAMFS: Once<&'static [u8]> = Once::new();

/// The boot image's initramfs, for the Linux server (`SYS_INITRAMFS`).
pub fn initramfs() -> Option<&'static [u8]> {
    INITRAMFS.get().copied()
}

pub fn init(ramdisk: Option<&'static [u8]>) {
    cache::init();
    if let Some(data) = ramdisk {
        INITRAMFS.call_once(|| data);
    }
    let root = ROOT.call_once(|| Inode::new(Node::Dir(BTreeMap::new()), 0o755).expect("file quota exhausted at boot"));
    if let Some(data) = ramdisk {
        if let Err(e) = cpio::unpack(root, data) {
            crate::printkln!("[fs] corrupt initramfs: {}", e);
        }
    }
    let dev = mkdir_p(root, "dev");
    // The terminals are the Linux server's, named by their numbers (ADR 0007);
    // /dev/pts is where it mounts its devpts.
    let nodes = [
        ("console", Device::Server(5, 1), 0o600),
        ("tty", Device::Server(5, 0), 0o666),
        ("ptmx", Device::Server(5, 2), 0o666),
        ("null", Device::Null, 0o666),
        ("zero", Device::Zero, 0o666),
    ];
    for (name, d, perm) in nodes {
        let inode = Inode::new(Node::Device(d), perm).expect("file quota exhausted at boot");
        let _ = dev.insert(name, inode);
    }
    mkdir_p(&dev, "pts");
    mkdir_p(root, "tmp");
    write_proc_mounts("");
}

/// Rewrites the static /proc/mounts, which tools like df read.
/// The mount table in /proc/mounts format.
static MOUNTS: spin::Mutex<String> = spin::Mutex::new(String::new());

pub fn mounts() -> String {
    let m = MOUNTS.lock();
    alloc::format!("rootfs / tmpfs rw 0 0\n{}", *m)
}

/// Records a mount; /proc/mounts stays a static file until procfs serves it.
fn write_proc_mounts(extra: &str) {
    MOUNTS.lock().push_str(extra);
    write_proc("mounts", &mounts());
}

/// (Re)writes the static, read-only file /proc/<name>, as long as no
/// procfs server provides /proc.
pub fn write_proc(name: &str, content: &str) {
    if PROC_SERVED.load(Ordering::Relaxed) {
        return;
    }
    let proc_dir = mkdir_p(&root(), "proc");
    let _ = proc_dir.unlink(name, false);
    if let Ok(file) = proc_dir.create(name, NewNode::File, 0o444) {
        let _ = file.write_at(0, content.as_bytes());
    }
}

/// Mounts the filesystem a server registered under `service` at `/<name>`,
/// replacing what is there (the static /proc of the boot, for procfs).
pub fn mount_remote(
    server: Arc<crate::process::Server>,
    service: usize,
    root_ino: u32,
    name: &str,
    device: &str,
    fstype: &str,
) -> Result<(), i64> {
    let fs = remote::RemoteFs::new(service, server);
    let root = root();
    if let Ok(old) = root.child(name) {
        for (entry, _, _) in old.list()? {
            if entry != "." && entry != ".." {
                old.unlink(&entry, false)?;
            }
        }
        root.unlink(name, true)?;
    }
    root.insert(name, fs.inode(root_ino))?;
    if name == "proc" {
        PROC_SERVED.store(true, Ordering::Relaxed);
    }
    record_mount(device, &alloc::format!("/{name}"), fstype);
    crate::printkln!("[fs] /{} mounted from user-space server ({})", name, fstype);
    Ok(())
}

/// Lists a mount in /proc/mounts: the kernel's own, and diskfs's disk,
/// which every Linux server instance mounts at /data (until procfs is the
/// server's, /proc/mounts is the kernel's list).
pub fn record_mount(device: &str, at: &str, fstype: &str) {
    write_proc_mounts(&alloc::format!("{device} {at} {fstype} rw 0 0\n"));
}

/// Whether a procfs server provides /proc (then no static files are written).
static PROC_SERVED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Creates all missing directories and returns the last one.
pub fn mkdir_p(base: &Arc<Inode>, path: &str) -> Arc<Inode> {
    let mut cur = base.clone();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        cur = match cur.child(c) {
            Ok(n) => n,
            Err(_) => {
                let d = Inode::new(Node::Dir(BTreeMap::new()), 0o755).expect("file quota exhausted at boot");
                let _ = cur.insert(c, d.clone());
                d
            }
        };
    }
    cur
}

/// Splits `path` relative to `cwd` into absolute components without "." and "..".
pub fn normalize(cwd: &str, path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let joined = if path.starts_with('/') { path.to_string() } else { alloc::format!("{cwd}/{path}") };
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c.to_string()),
        }
    }
    out
}

pub fn join(components: &[String]) -> String {
    if components.is_empty() {
        return "/".to_string();
    }
    components.iter().fold(String::new(), |acc, c| acc + "/" + c)
}

/// Resolves a path. `follow`: follow a symlink in the last component.
/// (Symlink targets are read through `Inode::readlink`, so this works on
/// every filesystem.)
pub fn resolve(cwd: &str, path: &str, follow: bool) -> Result<Arc<Inode>, i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    let mut pending: VecDeque<String> = normalize(cwd, path).into();
    let mut done: Vec<String> = Vec::new();
    let mut cur = root();
    let mut links = 0;
    while let Some(c) = pending.pop_front() {
        let child = cur.child(&c)?;
        let is_last = pending.is_empty();
        let target = if (follow || !is_last) && child.file_type() == S_IFLNK {
            Some(child.readlink()?)
        } else {
            None
        };
        match target {
            Some(t) => {
                links += 1;
                if links > 16 {
                    return Err(ELOOP);
                }
                let mut next: VecDeque<String> = normalize(&join(&done), &t).into();
                next.extend(pending.drain(..));
                pending = next;
                done.clear();
                cur = root();
            }
            None => {
                done.push(c);
                cur = child;
            }
        }
    }
    Ok(cur)
}

/// Returns the parent directory and the last name (for create/unlink).
pub fn resolve_parent(cwd: &str, path: &str) -> Result<(Arc<Inode>, String), i64> {
    let mut comps = normalize(cwd, path);
    let name = comps.pop().ok_or(EEXIST)?;
    let parent = resolve("/", &join(&comps), true)?;
    if !parent.is_dir() {
        return Err(ENOTDIR);
    }
    Ok((parent, name))
}
