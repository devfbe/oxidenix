//! In-Memory-Dateisystem (tmpfs-artig), beim Boot aus der Initramfs befuellt.

pub mod cpio;
pub mod file;

use crate::process::errno::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
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

pub enum Data {
    /// Unveraenderte Datei aus der Initramfs; wird beim ersten Schreiben kopiert.
    Static(&'static [u8]),
    Owned(Vec<u8>),
}

impl Data {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Data::Static(b) => b,
            Data::Owned(v) => v,
        }
    }

    pub fn make_mut(&mut self) -> &mut Vec<u8> {
        if let Data::Static(b) = self {
            *self = Data::Owned(b.to_vec());
        }
        match self {
            Data::Owned(v) => v,
            Data::Static(_) => unreachable!(),
        }
    }
}

pub enum Node {
    Dir(BTreeMap<String, Arc<Inode>>),
    File(Data),
    Symlink(String),
    Device(Device),
}

pub struct Inode {
    pub ino: u64,
    pub perm: Mutex<u32>,
    pub node: Mutex<Node>,
}

static NEXT_INO: AtomicU64 = AtomicU64::new(1);
static ROOT: Once<Arc<Inode>> = Once::new();

impl Inode {
    pub fn new(node: Node, perm: u32) -> Arc<Inode> {
        Arc::new(Inode {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            perm: Mutex::new(perm & 0o7777),
            node: Mutex::new(node),
        })
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
    ROOT.get().expect("fs::init fehlt").clone()
}

pub fn init(ramdisk: Option<&'static [u8]>) {
    let root = ROOT.call_once(|| Inode::new(Node::Dir(BTreeMap::new()), 0o755));
    if let Some(data) = ramdisk {
        if let Err(e) = cpio::unpack(root, data) {
            crate::printkln!("[fs] initramfs kaputt: {}", e);
        }
    }
    let dev = mkdir_p(root, "dev");
    for (name, d) in [("console", Device::Console), ("tty", Device::Console), ("null", Device::Null), ("zero", Device::Zero)] {
        let _ = dev.insert(name, Inode::new(Node::Device(d), 0o666));
    }
    mkdir_p(root, "tmp");
}

/// Legt alle fehlenden Verzeichnisse an und liefert das letzte.
pub fn mkdir_p(base: &Arc<Inode>, path: &str) -> Arc<Inode> {
    let mut cur = base.clone();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        cur = match cur.child(c) {
            Ok(n) => n,
            Err(_) => {
                let d = Inode::new(Node::Dir(BTreeMap::new()), 0o755);
                let _ = cur.insert(c, d.clone());
                d
            }
        };
    }
    cur
}

/// Zerlegt `path` relativ zu `cwd` in absolute Komponenten ohne "." und "..".
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

/// Loest einen Pfad auf. `follow`: Symlink in der letzten Komponente folgen.
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

/// Liefert das Elternverzeichnis und den letzten Namen (fuer create/unlink).
pub fn resolve_parent(cwd: &str, path: &str) -> Result<(Arc<Inode>, String), i64> {
    let mut comps = normalize(cwd, path);
    let name = comps.pop().ok_or(EEXIST)?;
    let parent = resolve("/", &join(&comps), true)?;
    if !parent.is_dir() {
        return Err(ENOTDIR);
    }
    Ok((parent, name))
}
