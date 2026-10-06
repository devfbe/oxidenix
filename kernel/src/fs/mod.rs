//! In-memory filesystem (tmpfs-like), populated from the initramfs at boot.

pub mod cpio;
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

fn charge(bytes: usize) -> Result<(), i64> {
    FILE_BYTES
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|&n| n <= FILE_QUOTA)
        })
        .map(|_| ())
        .map_err(|_| ENOSPC)
}

fn release(bytes: usize) {
    FILE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
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
        match self {
            Data::Static(b) => {
                let mut v = b.to_vec();
                v.resize(len, 0);
                *self = Data::Owned(v);
            }
            Data::Owned(v) => v.resize(len, 0),
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
}

/// Heap cost charged per inode, covering the inode, its directory entry and
/// a name of up to `NAME_MAX` bytes.
const INODE_COST: usize = 512;
pub const NAME_MAX: usize = 255;

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

    pub fn file_type(&self) -> u32 {
        match &*self.node.lock() {
            Node::Dir(_) => S_IFDIR,
            Node::File(_) => S_IFREG,
            Node::Symlink(_) => S_IFLNK,
            Node::Device(_) => S_IFCHR,
        }
    }

    pub fn mode(&self) -> u32 {
        self.file_type() | *self.perm.lock()
    }

    pub fn size(&self) -> u64 {
        match &*self.node.lock() {
            Node::File(d) => d.bytes().len() as u64,
            Node::Symlink(t) => t.len() as u64,
            Node::Dir(m) => m.len() as u64,
            Node::Device(_) => 0,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(&*self.node.lock(), Node::Dir(_))
    }

    pub fn child(&self, name: &str) -> Result<Arc<Inode>, i64> {
        match &*self.node.lock() {
            Node::Dir(m) => m.get(name).cloned().ok_or(ENOENT),
            _ => Err(ENOTDIR),
        }
    }

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
        let target = match &*child.node.lock() {
            Node::Symlink(t) if follow || !is_last => Some(t.clone()),
            _ => None,
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
