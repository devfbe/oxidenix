//! In-memory filesystem (tmpfs-like), populated from the initramfs at boot.

pub mod cpio;
pub mod ext2;
pub mod file;

use crate::process::errno::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::{Mutex, Once};

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFCHR: u32 = 0o020000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Console,
    Null,
    Zero,
}

pub const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;
/// File contents live on the kernel heap; this caps all of them together.
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

/// (bytes charged, quota) of the in-memory filesystem.
pub fn quota_usage() -> (usize, usize) {
    (FILE_BYTES.load(Ordering::Relaxed), FILE_QUOTA)
}

/// File contents. Every change of the owned size goes through `resize`,
/// which charges it against `FILE_QUOTA`.
pub enum Data {
    /// Unmodified file from the initramfs; copied on first write.
    Static(&'static [u8]),
    Owned(Vec<u8>),
}

impl Data {
    pub fn empty() -> Data {
        Data::Owned(Vec::new())
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Data::Static(b) => b,
            Data::Owned(v) => v,
        }
    }

    fn owned_len(&self) -> usize {
        match self {
            Data::Static(_) => 0,
            Data::Owned(v) => v.len(),
        }
    }

    pub fn resize(&mut self, len: usize) -> Result<(), i64> {
        if len > MAX_FILE_SIZE {
            return Err(EFBIG);
        }
        let old = self.owned_len();
        if len > old {
            charge(len - old)?;
        }
        // Allocation failures become ENOMEM instead of a kernel panic.
        let grown = match self {
            Data::Static(b) => {
                let mut v = Vec::new();
                let ok = v.try_reserve_exact(len).is_ok();
                if ok {
                    v.extend_from_slice(&b[..b.len().min(len)]);
                    v.resize(len, 0);
                    *self = Data::Owned(v);
                }
                ok
            }
            Data::Owned(v) => {
                let ok = len <= v.len() || v.try_reserve(len - v.len()).is_ok();
                if ok {
                    v.resize(len, 0);
                }
                ok
            }
        };
        if !grown {
            release(len - old);
            return Err(ENOMEM);
        }
        if len < old {
            release(old - len);
        }
        Ok(())
    }

    pub fn write_at(&mut self, start: usize, buf: &[u8]) -> Result<(), i64> {
        let end = start.checked_add(buf.len()).ok_or(EFBIG)?;
        self.resize(end.max(self.bytes().len()))?;
        match self {
            Data::Owned(v) => v[start..end].copy_from_slice(buf),
            Data::Static(_) => unreachable!("resize makes the data owned"),
        }
        Ok(())
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        release(self.owned_len());
    }
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
    File(Data),
    Symlink(String),
    Device(Device),
    /// An inode on the ext2 data disk; its contents live there.
    Disk(DiskRef),
}

#[derive(Clone)]
pub struct DiskRef {
    pub fs: Arc<ext2::Ext2>,
    pub ino: u32,
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
/// for ext2 inodes alike; callers never look at `node` directly.
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
        }))
    }

    /// Disk inodes are cached by their filesystem and charged no quota.
    pub fn disk(fs: Arc<ext2::Ext2>, ino: u32) -> Arc<Inode> {
        Arc::new(Inode {
            ino: DISK_INO_BASE | ino as u64,
            perm: Mutex::new(0),
            node: Mutex::new(Node::Disk(DiskRef { fs, ino })),
            charged: 0,
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
            return d.fs.stat(d.ino).map_or(S_IFREG, |i| i.mode() as u32);
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
            return d.fs.stat(d.ino).map_or(0, |i| i.size());
        }
        match &*self.node.lock() {
            Node::File(d) => d.bytes().len() as u64,
            Node::Symlink(t) => t.len() as u64,
            Node::Dir(m) => m.len() as u64,
            Node::Device(_) | Node::Disk(_) => 0,
        }
    }

    /// (link count, access time, modification time, change time)
    pub fn stat_extra(&self) -> (u64, u64, u64, u64) {
        if let Some(d) = self.disk_ref() {
            if let Ok(i) = d.fs.stat(d.ino) {
                return (i.links() as u64, i.atime() as u64, i.mtime() as u64, i.ctime() as u64);
            }
        }
        let boot = crate::drivers::rtc::now() - crate::process::ticks() / crate::process::TIMER_HZ;
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
            NewNode::File => Node::File(Data::empty()),
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

    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        if let Some(d) = self.disk_ref() {
            if self.is_dir() {
                return Err(EISDIR);
            }
            return d.fs.read(d.ino, off, buf);
        }
        match &*self.node.lock() {
            Node::File(data) => {
                let bytes = data.bytes();
                let start = (off as usize).min(bytes.len());
                let n = buf.len().min(bytes.len() - start);
                buf[..n].copy_from_slice(&bytes[start..start + n]);
                Ok(n)
            }
            Node::Dir(_) => Err(EISDIR),
            _ => Err(EINVAL),
        }
    }

    pub fn write_at(&self, off: u64, buf: &[u8]) -> Result<usize, i64> {
        if let Some(d) = self.disk_ref() {
            if self.is_dir() {
                return Err(EISDIR);
            }
            return d.fs.write(d.ino, off, buf);
        }
        match &mut *self.node.lock() {
            Node::File(data) => {
                data.write_at(usize::try_from(off).map_err(|_| EFBIG)?, buf)?;
                Ok(buf.len())
            }
            Node::Dir(_) => Err(EISDIR),
            _ => Err(EINVAL),
        }
    }

    pub fn truncate(&self, len: u64) -> Result<(), i64> {
        if let Some(d) = self.disk_ref() {
            return d.fs.truncate(d.ino, len);
        }
        let result = match &mut *self.node.lock() {
            Node::File(data) => data.resize(usize::try_from(len).map_err(|_| EFBIG)?),
            Node::Dir(_) => Err(EISDIR),
            _ => Err(EINVAL),
        };
        result
    }

    pub fn set_perm(&self, perm: u32) -> Result<(), i64> {
        if let Some(d) = self.disk_ref() {
            return d.fs.set_perm(d.ino, perm);
        }
        *self.perm.lock() = perm & 0o7777;
        Ok(())
    }

    /// Calls `f` with the whole file contents (read from disk if needed).
    pub fn with_contents<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R, i64> {
        if self.disk_ref().is_some() {
            let size = usize::try_from(self.size()).map_err(|_| EFBIG)?;
            let mut buf = Vec::new();
            buf.try_reserve_exact(size).map_err(|_| ENOMEM)?;
            buf.resize(size, 0);
            let n = self.read_at(0, &mut buf)?;
            return Ok(f(&buf[..n]));
        }
        match &*self.node.lock() {
            Node::File(data) => Ok(f(data.bytes())),
            Node::Dir(_) => Err(EISDIR),
            _ => Err(EINVAL),
        }
    }

    /// Filesystem the inode lives on, for statfs.
    pub fn filesystem(&self) -> Option<Arc<ext2::Ext2>> {
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

pub fn init(ramdisk: Option<&'static [u8]>) {
    let root = ROOT.call_once(|| Inode::new(Node::Dir(BTreeMap::new()), 0o755).expect("file quota exhausted at boot"));
    if let Some(data) = ramdisk {
        if let Err(e) = cpio::unpack(root, data) {
            crate::printkln!("[fs] corrupt initramfs: {}", e);
        }
    }
    let dev = mkdir_p(root, "dev");
    for (name, d) in [("console", Device::Console), ("tty", Device::Console), ("null", Device::Null), ("zero", Device::Zero)] {
        let inode = Inode::new(Node::Device(d), 0o666).expect("file quota exhausted at boot");
        let _ = dev.insert(name, inode);
    }
    mkdir_p(root, "tmp");
    let mut mounts = String::from("rootfs / tmpfs rw 0 0\n");
    match ext2::Ext2::mount() {
        Ok(fs) => {
            let (bs, blocks, free, _, _) = fs.usage();
            let _ = root.insert("data", fs.inode(ext2::ROOT_INO));
            mounts.push_str("/dev/hdb /data ext2 rw 0 0\n");
            crate::printkln!("[fs] ext2 data disk mounted at /data ({} of {} KiB free)", free * bs / 1024, blocks * bs / 1024);
        }
        Err(e) => crate::printkln!("[fs] no data disk mounted: {}", e),
    }
    // A static /proc/mounts, which tools like df read.
    let proc_dir = mkdir_p(root, "proc");
    let mut data = Data::empty();
    let _ = data.write_at(0, mounts.as_bytes());
    if let Ok(inode) = Inode::new(Node::File(data), 0o444) {
        let _ = proc_dir.insert("mounts", inode);
    }
}

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
