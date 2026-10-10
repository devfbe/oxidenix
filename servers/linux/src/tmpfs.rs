//! The server's tmpfs (phase R6c.2c): directories, files, symlinks, socket
//! inodes (AF_UNIX names, `unix`) and device nodes (devpts's, `pty`) in the
//! server's memory; a file's
//! contents are a file object of the kernel's (`SYS_MO_CREATE_FILE`), read,
//! written and mapped through the object, and charged to the kernel's limit
//! of file objects (`SYS_FILE_PAGES`).
//!
//! Locking: each inode has its own lock (its directory map, permissions,
//! times and write count). Code holds one inode lock at a time, except under
//! `RENAME` (as Linux's rename mutex), which renames and removals take
//! first: a rename then locks both directories in address order and,
//! nested, the inode it replaces; a removal locks the directory and,
//! nested, the child. Since every nesting happens under `RENAME`, no two
//! of them wait for each other. Directories move only under it, so a
//! rename's check that a directory does not move below itself holds until
//! the move.
//!
//! Write access (ETXTBSY, as the kernel's VFS has it): `writers` counts
//! descriptors open for writing and shared mappings through them (> 0), or
//! programs running from the file (< 0); the two exclude each other. A
//! mapping and a running program hold their count through a hold on the
//! file object (`SYS_MO_HOLD`): the kernel reports when the last holder is
//! gone (`EVENT_RELEASE` with the hold's word, tagged with bit 0). The
//! service thread handles that event a little later; so a thread that
//! finds the count in the way first waits for every release the kernel
//! reported until then (`settle`): once a program has ended and was
//! reaped, its file can be written, as on Linux.

use crate::namespace::{check, ENOENT, ENOTDIR};
use crate::sync::Mutex;
use crate::syscall;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use restricted::*;
use vfs::stat::{SetTime, Stat, Times};

pub const EPERM: i64 = 1;
pub const EACCES: i64 = 13;
pub const EEXIST: i64 = 17;
pub const EXDEV: i64 = 18;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const ENOSPC: i64 = 28;
const ENOMEM: i64 = 12;
pub const ENAMETOOLONG: i64 = 36;
pub const ENOTEMPTY: i64 = 39;
pub const ETXTBSY: i64 = 26;

/// st_dev of each of the server's tmpfs mounts (Linux's numbers of the
/// kind): the root, /dev (devtmpfs) and devpts. Each is its own filesystem
/// (`df`, `find -xdev`, rename's EXDEV); their inode numbers are one
/// sequence, so no two files share device and inode number.
pub const DEV: u64 = 0x1a;
pub const DEVTMPFS_DEV: u64 = 0x5;
pub const DEVPTS_DEV: u64 = 0x18;

/// A file object of the kernel's, closed when the file goes.
pub struct Object(u64);

impl Object {
    pub fn handle(&self) -> u64 {
        self.0
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.0, 0, 0, 0, 0, 0]);
    }
}

pub enum Kind {
    Dir(BTreeMap<String, Arc<Inode>>),
    File(Object),
    Symlink(String),
    /// A socket's name in the filesystem (bind(2) of an AF_UNIX socket):
    /// the socket it leads to is found by the inode (`unix`).
    Socket,
    /// A character device: its number (`st_rdev`) names its driver (the
    /// terminals of devpts, `pty`).
    Device(u64),
}

mod content {
    use super::{Inode, Kind};
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::sync::Arc;

    /// An inode's `Kind`, whose variant never changes once made: the
    /// inode's file type (`Inode::file_type`), read without its lock,
    /// is taken from it at creation. Only a directory's map can be
    /// changed in place; nothing can replace the kind (the field is
    /// private to this module).
    pub struct Content(Kind);

    impl Content {
        pub fn new(kind: Kind) -> Content {
            Content(kind)
        }

        pub fn get(&self) -> &Kind {
            &self.0
        }

        /// A directory's map, to change (None for any other kind).
        pub fn dir_mut(&mut self) -> Option<&mut BTreeMap<String, Arc<Inode>>> {
            match &mut self.0 {
                Kind::Dir(m) => Some(m),
                _ => None,
            }
        }
    }
}

pub struct State {
    pub perm: u32,
    /// Its kind and contents (the variant is fixed: `content::Content`).
    kind: content::Content,
    /// A directory whose names only the server makes and removes (devpts):
    /// creating a name there is EACCES, removing or renaming one EPERM, as
    /// Linux's devpts (no create, unlink or rename operations).
    sealed: bool,
    /// Its times, with nanoseconds and a birth time (statx).
    pub times: Times,
    /// > 0: write accesses; < 0: programs running from it (see above).
    writers: i64,
    /// Removed (its name went): nothing new goes into a directory (a
    /// lookup may have found it just before).
    removed: bool,
    /// The directory it is in (its inode number) and its name there: where
    /// its inotify events about it go besides its own watches.
    link: Option<(u64, String)>,
}

pub struct Inode {
    pub ino: u64,
    /// The tmpfs it is in (`DEV`, `DEVTMPFS_DEV`, `DEVPTS_DEV`), its root's.
    pub dev: u64,
    /// Its file type (`S_IFMT` bits), fixed at creation: known without the
    /// lock (a path walk asks each name's). It stays right because the
    /// kind's variant cannot change (`content::Content`).
    file_type: u32,
    pub state: Mutex<State>,
    /// Taken by O_APPEND writes: finding the end and writing there is one
    /// step for every appender.
    pub append: crate::sync::SleepLock,
    /// Open file descriptions of it: a removed file's last close is when
    /// it goes (inotify's IN_DELETE_SELF).
    pub opens: AtomicUsize,
    /// What it charged to the tmpfs's bounds (`INODES`, `META_BYTES`): given back when it
    /// goes.
    charged: usize,
}

/// Most inodes the programs may make in the tree's tmpfs (Linux's nr_inodes; the server's
/// own, the root and the device nodes, do not count): directories, symlinks and socket
/// names take the server's heap, not file pages, so they are bounded apart (ENOSPC).
const MAX_INODES: usize = 32768;
static INODES: AtomicUsize = AtomicUsize::new(0);
/// Most bytes the programs' symlink targets may take in the tree's tmpfs (ENOSPC beyond).
const MAX_META_BYTES: usize = 4 << 20;
static META_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Gives `n` back to a bound's count, never below zero (a count that wrapped would be no
/// bound at all).
fn give_back(count: &AtomicUsize, n: usize) {
    let _ = count.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| Some(c.saturating_sub(n)));
}

/// The inode itself goes (its last reference: its names, open files and walks are gone):
/// what it charged at its creation is given back, here and only here.
impl Drop for Inode {
    fn drop(&mut self) {
        if self.charged != usize::MAX {
            give_back(&INODES, 1);
            give_back(&META_BYTES, self.charged);
        }
    }
}

static NEXT_INO: AtomicU64 = AtomicU64::new(1);
/// Taken by renames and removals (see the module comment).
static RENAME: Mutex<()> = Mutex::new(());

/// A new, empty tmpfs with device number `dev`: its root directory.
pub fn new_root(dev: u64) -> Arc<Inode> {
    // (Made at the server's start: without memory for it the instance cannot begin.)
    Inode::server(Kind::Dir(BTreeMap::new()), 0o1777, dev).expect("memory for the root")
}

impl Inode {
    /// A programs' inode, charged to the tmpfs's bounds (ENOSPC beyond them, ENOMEM
    /// without memory).
    fn new(kind: Kind, perm: u32, dev: u64) -> Result<Arc<Inode>, i64> {
        let meta = match &kind {
            Kind::Symlink(t) => t.len(),
            _ => 0,
        };
        if INODES.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| (c < MAX_INODES).then_some(c + 1)).is_err() {
            return Err(ENOSPC);
        }
        if META_BYTES.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| (c + meta <= MAX_META_BYTES).then_some(c + meta)).is_err() {
            give_back(&INODES, 1);
            return Err(ENOSPC);
        }
        // Charged: from here on the inode value owns the charge, and its drop gives it back
        // (also when no memory is found for it: `Arc::try_new` drops the value).
        Inode::make(kind, perm, dev, meta)
    }

    /// The server's own inode (the roots, devpts and its nodes): not charged.
    fn server(kind: Kind, perm: u32, dev: u64) -> Result<Arc<Inode>, i64> {
        Inode::make(kind, perm, dev, usize::MAX)
    }

    fn make(kind: Kind, perm: u32, dev: u64, charged: usize) -> Result<Arc<Inode>, i64> {
        let file_type = kind_bits(&kind);
        let state = State { perm, kind: content::Content::new(kind), sealed: false, times: Times::new(now()), writers: 0, removed: false, link: None };
        Arc::try_new(Inode {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            dev,
            file_type,
            state: Mutex::new(state),
            append: crate::sync::SleepLock::new(()),
            opens: AtomicUsize::new(0),
            charged,
        })
        .map_err(|_| ENOMEM)
    }

    /// The directory it is in and its name there (None: removed, or the
    /// root).
    pub fn link(&self) -> Option<(u64, String)> {
        self.state.lock().link.clone()
    }

    /// Whether its name went.
    pub fn removed(&self) -> bool {
        self.state.lock().removed
    }

    pub fn mode(&self) -> u32 {
        self.file_type | self.state.lock().perm
    }

    /// Its file type (`S_IFMT` bits).
    pub fn file_type(&self) -> u32 {
        self.file_type
    }

    pub fn is_dir(&self) -> bool {
        self.file_type == vfs::S_IFDIR
    }

    /// The file object's handle (EISDIR for a directory, EINVAL else).
    pub fn object(&self) -> Result<u64, i64> {
        match self.state.lock().kind.get() {
            Kind::File(o) => Ok(o.handle()),
            Kind::Dir(_) => Err(EISDIR),
            Kind::Symlink(_) | Kind::Socket | Kind::Device(_) => Err(EINVAL),
        }
    }

    pub fn lookup(&self, name: &str) -> Result<Arc<Inode>, i64> {
        match self.state.lock().kind.get() {
            Kind::Dir(m) => m.get(name).cloned().ok_or(ENOENT),
            _ => Err(ENOTDIR),
        }
    }

    pub fn readlink(&self) -> Result<String, i64> {
        match self.state.lock().kind.get() {
            Kind::Symlink(t) => Ok(t.clone()),
            _ => Err(EINVAL),
        }
    }

    pub fn size(&self) -> u64 {
        stat_size(self.state.lock().kind.get())
    }

    /// Its status: every field of `struct stat`, and the birth time (one
    /// snapshot, under one lock).
    pub fn status(&self) -> Stat {
        let (perm, size, times, rdev) = {
            let st = self.state.lock();
            let rdev = match st.kind.get() {
                Kind::Device(rdev) => *rdev,
                _ => 0,
            };
            (st.perm, stat_size(st.kind.get()), st.times, rdev)
        };
        Stat {
            dev: self.dev,
            ino: self.ino,
            nlink: if self.file_type == vfs::S_IFDIR { 2 } else { 1 },
            mode: self.file_type | perm,
            rdev,
            size,
            blksize: 4096,
            blocks: size.div_ceil(512),
            atime: times.atime,
            mtime: times.mtime,
            ctime: times.ctime,
            btime: Some(times.btime),
            ..Stat::default()
        }
    }

    /// Its `struct stat`.
    pub fn stat(&self) -> [u8; 144] {
        self.status().to_bytes()
    }

    pub fn set_perm(&self, perm: u32) {
        let mut st = self.state.lock();
        st.perm = perm & 0o7777;
        st.times.changed(now());
    }

    /// The contents changed (a write, a truncation).
    pub fn modified(&self) {
        self.state.lock().times.modified(now());
    }

    /// The contents were read (relatime).
    pub fn accessed(&self) {
        self.state.lock().times.accessed(now());
    }

    /// utimensat.
    pub fn set_times(&self, atime: SetTime, mtime: SetTime) {
        self.state.lock().times.set(atime, mtime, now());
    }

    /// Directory entries (name, inode number, dirent type), "." and ".."
    /// first.
    /// Its entries ("." and ".." first), charged to the tree's snapshot bound (`charge`).
    pub fn list(&self, charge: &mut crate::files::SnapshotCharge) -> Result<Vec<(String, u64, u8)>, i64> {
        // The names are copied under the lock, after their room is charged and reserved.
        let st = self.state.lock();
        let Kind::Dir(m) = st.kind.get() else { return Err(ENOTDIR) };
        charge.add(m.keys().map(|n| n.len() + 48).sum::<usize>() + 2 * 48)?;
        let mut out = Vec::new();
        out.try_reserve_exact(m.len() + 2).map_err(|_| ENOMEM)?;
        // Each name copied into room reserved first (ENOMEM without it).
        let copy = |n: &str| -> Result<String, i64> {
            let mut s = String::new();
            s.try_reserve_exact(n.len()).map_err(|_| ENOMEM)?;
            s.push_str(n);
            Ok(s)
        };
        // ".." is the directory it is in (the root's, and a removed one's, itself).
        let parent = st.link.as_ref().map_or(self.ino, |&(p, _)| p);
        out.push((copy(".")?, self.ino, 4));
        out.push((copy("..")?, parent, 4));
        for (n, c) in m.iter() {
            out.push((copy(n)?, c.ino, dtype(c.file_type())));
        }
        Ok(out)
    }

    /// A new file or directory `name` (EEXIST if taken).
    pub fn create(&self, name: &str, dir: bool, perm: u32) -> Result<Arc<Inode>, i64> {
        check_name(name)?;
        let kind = if dir {
            Kind::Dir(BTreeMap::new())
        } else {
            Kind::File(Object(check(syscall(SYS_MO_CREATE_FILE, [0; 6])).map_err(|e| if e == 24 { ENOSPC } else { e })? as u64))
        };
        let inode = Inode::new(kind, perm & 0o7777, self.dev)?;
        self.insert(name, inode.clone())?;
        Ok(inode)
    }

    /// A new file `name` whose contents are the file object `handle` (the
    /// file takes it over, also on failure).
    pub fn insert_object(&self, name: &str, handle: u64, perm: u32) -> Result<(), i64> {
        let object = Object(handle);
        check_name(name)?;
        self.insert(name, Inode::new(Kind::File(object), perm & 0o7777, self.dev)?)
    }

    /// The directory `name`, made if missing.
    pub fn subdir(&self, name: &str, perm: u32) -> Result<Arc<Inode>, i64> {
        match self.lookup(name) {
            Ok(d) if d.is_dir() => Ok(d),
            Ok(_) => Err(ENOTDIR),
            Err(_) => match self.create(name, true, perm) {
                Err(EEXIST) => self.lookup(name),
                other => other,
            },
        }
    }

    pub fn symlink(&self, name: &str, target: String) -> Result<(), i64> {
        check_name(name)?;
        self.insert(name, Inode::new(Kind::Symlink(target), 0o777, self.dev)?)
    }

    /// A new socket inode `name` (EEXIST if taken), for bind(2).
    pub fn socket(&self, name: &str, perm: u32) -> Result<Arc<Inode>, i64> {
        check_name(name)?;
        let inode = Inode::new(Kind::Socket, perm & 0o7777, self.dev)?;
        self.insert(name, inode.clone())?;
        Ok(inode)
    }

    /// A sealed directory, the root of a tmpfs with device number `dev`
    /// (devpts, see `State::sealed`).
    pub fn new_sealed_dir(perm: u32, dev: u64) -> Arc<Inode> {
        // (Made at the server's start, as a root.)
        let dir = Inode::server(Kind::Dir(BTreeMap::new()), perm & 0o7777, dev).expect("memory for devpts");
        dir.state.lock().sealed = true;
        dir
    }

    /// A new device node `name` with number `rdev` (EEXIST if taken), also in
    /// a sealed directory: the server's own.
    pub fn insert_device(&self, name: &str, rdev: u64, perm: u32) -> Result<Arc<Inode>, i64> {
        check_name(name)?;
        let inode = Inode::server(Kind::Device(rdev), perm & 0o7777, self.dev)?;
        self.insert_as(name, inode.clone(), true)?;
        Ok(inode)
    }

    /// Removes the server's device node `name` (also from a sealed directory).
    pub fn remove_device(&self, name: &str) {
        let _nesting = RENAME.lock();
        let mut st = self.state.lock();
        let Some(m) = st.kind.dir_mut() else { return };
        let Some(child) = m.remove(name) else { return };
        let now = now();
        {
            let mut cst = child.state.lock();
            cst.removed = true;
            cst.link = None;
            cst.times.changed(now);
        }
        st.times.modified(now);
    }

    fn insert(&self, name: &str, inode: Arc<Inode>) -> Result<(), i64> {
        self.insert_as(name, inode, false)
    }

    /// Inserts `inode` as `name`; `server`: the server's own name, which a
    /// sealed directory takes.
    fn insert_as(&self, name: &str, inode: Arc<Inode>, server: bool) -> Result<(), i64> {
        // (Not visible to anyone else yet: its lock is not nested.)
        inode.state.lock().link = Some((self.ino, String::from(name)));
        let mut st = self.state.lock();
        if st.removed {
            return Err(ENOENT);
        }
        if st.sealed && !server {
            return Err(EACCES);
        }
        match st.kind.dir_mut() {
            Some(m) if m.contains_key(name) => Err(EEXIST),
            Some(m) => {
                m.insert(String::from(name), inode);
                st.times.modified(now());
                Ok(())
            }
            None => Err(ENOTDIR),
        }
    }

    /// Removes `name`: a directory only with `dir_only` (rmdir) and only
    /// when empty, anything else only without (checked under the child's
    /// lock, so no file appears in a directory being removed).
    /// The inode it removed.
    pub fn unlink(&self, name: &str, dir_only: bool) -> Result<Arc<Inode>, i64> {
        let _nesting = RENAME.lock();
        let mut st = self.state.lock();
        if st.sealed {
            return Err(EPERM);
        }
        let Some(m) = st.kind.dir_mut() else { return Err(ENOTDIR) };
        let child = m.get(name).ok_or(ENOENT)?.clone();
        let mut cst = child.state.lock();
        let child_dir = match cst.kind.get() {
            Kind::Dir(c) => Some(c.is_empty()),
            _ => None,
        };
        match (child_dir, dir_only) {
            (Some(false), true) => return Err(ENOTEMPTY),
            (Some(_), false) => return Err(EISDIR),
            (None, true) => return Err(ENOTDIR),
            _ => {}
        }
        cst.removed = true;
        cst.link = None;
        let now = now();
        cst.times.changed(now);
        drop(cst);
        m.remove(name);
        st.times.modified(now);
        drop(st);
        Ok(child)
    }

    /// The right to write the file (ETXTBSY while a program runs from it).
    pub fn get_write(&self) -> Result<(), i64> {
        settled(|| {
            let mut st = self.state.lock();
            if st.writers < 0 {
                return Err(ETXTBSY);
            }
            st.writers += 1;
            Ok(())
        })
    }

    pub fn put_write(&self) {
        self.state.lock().writers -= 1;
    }

    /// The right to run the file (ETXTBSY while open for writing).
    fn deny_write(&self) -> Result<(), i64> {
        settled(|| {
            let mut st = self.state.lock();
            if st.writers > 0 {
                return Err(ETXTBSY);
            }
            st.writers -= 1;
            Ok(())
        })
    }

    fn allow_write(&self) {
        self.state.lock().writers += 1;
    }

    /// A handle on the file object that holds write access (for a shared
    /// mapping through a writable descriptor) or the right to run it,
    /// returned to the inode when the kernel lets the hold go.
    pub fn hold(self: &Arc<Self>, run: bool) -> Result<u64, i64> {
        let object = self.object()?;
        if run { self.deny_write()? } else { self.get_write()? }
        let word = Box::into_raw(Box::new(Hold { inode: self.clone(), run })) as u64 | 1;
        match check(syscall(SYS_MO_HOLD, [object, word, 0, 0, 0, 0])) {
            Ok(h) => Ok(h as u64),
            Err(e) => {
                // Never handed over.
                released(word);
                Err(e)
            }
        }
    }
}

struct Hold {
    inode: Arc<Inode>,
    run: bool,
}

/// `EVENT_RELEASE` for a hold's word (bit 0 set).
pub fn released(word: u64) {
    let hold = unsafe { Box::from_raw((word & !1) as *mut Hold) };
    if hold.run { hold.inode.allow_write() } else { hold.inode.put_write() }
}

/// How many `EVENT_RELEASE` events the service thread has handled (records'
/// too), modulo 2^32; a futex word for `settle`.
static RELEASES_HANDLED: AtomicU32 = AtomicU32::new(0);

/// The service thread handled an `EVENT_RELEASE`.
pub fn release_handled() {
    RELEASES_HANDLED.fetch_add(1, Ordering::Release);
    syscall(SYS_SERVER_FUTEX_WAKE, [&RELEASES_HANDLED as *const AtomicU32 as u64, u32::MAX as u64, 0, 0, 0, 0]);
}

/// Waits until the service thread has handled every release the kernel has
/// queued so far (or the calling thread dies: EINTR).
fn settle() -> Result<(), i64> {
    const EINTR: i64 = 4;
    let target = syscall(SYS_EVENT_RELEASES, [0; 6]) as u32;
    loop {
        let done = RELEASES_HANDLED.load(Ordering::Acquire);
        if done.wrapping_sub(target) as i32 >= 0 {
            return Ok(());
        }
        if syscall(SYS_SERVER_FUTEX_WAIT, [&RELEASES_HANDLED as *const AtomicU32 as u64, done as u64, 0, 0, 0, 0]) == -EINTR {
            return Err(EINTR);
        }
    }
}

/// `try_once`, and if it finds the file busy (ETXTBSY), once more after the
/// releases reported until then are in (also for /data's files).
pub(crate) fn settled(try_once: impl Fn() -> Result<(), i64>) -> Result<(), i64> {
    match try_once() {
        Err(ETXTBSY) => {
            settle()?;
            try_once()
        }
        r => r,
    }
}

/// Renames `odir/oname` to `ndir/nname` (both in this tmpfs), replacing a
/// file or an empty directory there: the inode that moved, and the one it
/// replaced.
pub fn rename(odir: &Arc<Inode>, oname: &str, ndir: &Arc<Inode>, nname: &str) -> Result<(Arc<Inode>, Option<Arc<Inode>>), i64> {
    check_name(nname)?;
    // Another tmpfs is another filesystem.
    if odir.dev != ndir.dev {
        return Err(EXDEV);
    }
    let _shape = RENAME.lock();
    let node = odir.lookup(oname)?;
    // Moving a directory below itself would detach it in a reference cycle.
    // (The tree's shape cannot change meanwhile: renames wait for RENAME,
    // and creating or removing names moves no directory.)
    if contains(&node, ndir) {
        return Err(EINVAL);
    }
    let node_dir = node.is_dir();
    // The directories' locks, in address order.
    let same = Arc::ptr_eq(odir, ndir);
    let (first, second) = if Arc::as_ptr(odir) <= Arc::as_ptr(ndir) { (odir, ndir) } else { (ndir, odir) };
    let mut first_guard = first.state.lock();
    let mut second_guard = if same { None } else { Some(second.state.lock()) };
    if first_guard.sealed || second_guard.as_ref().is_some_and(|g| g.sealed) {
        return Err(EPERM);
    }
    let first_map = dir_map(&mut first_guard)?;
    let second_map = match second_guard.as_mut() {
        Some(g) => Some(dir_map(g)?),
        None => None,
    };
    let (om, nm) = match second_map {
        None => (None, first_map),
        Some(sm) if Arc::ptr_eq(first, odir) => (Some(first_map), sm),
        Some(sm) => (Some(sm), first_map),
    };
    let replaced = move_entry(om, nm, oname, nname, &node, node_dir, odir)?;
    // Both directories' entries changed (their locks are held), and the
    // inode moved (nested, as above).
    let now = now();
    {
        let mut nst = node.state.lock();
        nst.times.changed(now);
        nst.link = Some((ndir.ino, String::from(nname)));
    }
    first_guard.times.modified(now);
    if let Some(g) = second_guard.as_mut() {
        g.times.modified(now);
    }
    drop(second_guard);
    drop(first_guard);
    Ok((node, replaced))
}

fn dir_map(st: &mut State) -> Result<&mut BTreeMap<String, Arc<Inode>>, i64> {
    st.kind.dir_mut().ok_or(ENOTDIR)
}

/// The move itself, under both directories' locks: `om` is the old
/// directory's map (None: the same as `nm`). Returns what it replaced.
fn move_entry(
    om: Option<&mut BTreeMap<String, Arc<Inode>>>,
    nm: &mut BTreeMap<String, Arc<Inode>>,
    oname: &str,
    nname: &str,
    node: &Arc<Inode>,
    node_dir: bool,
    odir: &Arc<Inode>,
) -> Result<Option<Arc<Inode>>, i64> {
    // Still the inode looked up (it may have been removed meanwhile).
    let source_ok = match &om {
        Some(om) => om.get(oname).is_some_and(|c| Arc::ptr_eq(c, node)),
        None => nm.get(oname).is_some_and(|c| Arc::ptr_eq(c, node)),
    };
    if !source_ok {
        return Err(ENOENT);
    }
    if let Some(existing) = nm.get(nname) {
        if Arc::ptr_eq(existing, node) {
            return Ok(None);
        }
        // The old directory holds `node`, so it is not empty (and it is
        // locked already); any other inode here is locked nested, after
        // the directories (see the module comment).
        if Arc::ptr_eq(existing, odir) {
            return Err(if node_dir { ENOTEMPTY } else { EISDIR });
        }
        let mut est = existing.state.lock();
        let existing_dir = match est.kind.get() {
            Kind::Dir(m) => Some(m.is_empty()),
            _ => None,
        };
        // Replacing only ever drops a file or an empty directory.
        match (existing_dir, node_dir) {
            (Some(false), true) => return Err(ENOTEMPTY),
            (Some(_), false) => return Err(EISDIR),
            (None, true) => return Err(ENOTDIR),
            _ => {}
        }
        // It loses its link.
        est.removed = true;
        est.link = None;
        est.times.changed(now());
    }
    let moved = match om {
        Some(om) => om.remove(oname),
        None => nm.remove(oname),
    };
    Ok(moved.and_then(|m| nm.insert(String::from(nname), m)))
}

/// Whether `target` is `dir` or lies below it (iterative: directories nest
/// deeper than a stack).
fn contains(dir: &Arc<Inode>, target: &Arc<Inode>) -> bool {
    let mut stack = alloc::vec![dir.clone()];
    while let Some(d) = stack.pop() {
        if Arc::ptr_eq(&d, target) {
            return true;
        }
        if let Kind::Dir(m) = d.state.lock().kind.get() {
            stack.extend(m.values().cloned());
        }
    }
    false
}

fn check_name(name: &str) -> Result<(), i64> {
    if name.len() > vfs::NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(EINVAL);
    }
    Ok(())
}

/// The size `struct stat` reports for an inode of `kind`.
fn stat_size(kind: &Kind) -> u64 {
    match kind {
        Kind::File(o) => syscall(SYS_MO_FILE_SIZE, [o.handle(), 0, 0, 0, 0, 0]).max(0) as u64,
        Kind::Symlink(t) => t.len() as u64,
        Kind::Dir(m) => m.len() as u64,
        Kind::Socket | Kind::Device(_) => 0,
    }
}

fn kind_bits(kind: &Kind) -> u32 {
    match kind {
        Kind::Dir(_) => vfs::S_IFDIR,
        Kind::File(_) => vfs::S_IFREG,
        Kind::Symlink(_) => vfs::S_IFLNK,
        Kind::Socket => vfs::S_IFSOCK,
        Kind::Device(_) => vfs::S_IFCHR,
    }
}

pub fn dtype(mode: u32) -> u8 {
    match mode & vfs::S_IFMT {
        vfs::S_IFCHR => 2,
        vfs::S_IFDIR => 4,
        vfs::S_IFREG => 8,
        vfs::S_IFLNK => 10,
        vfs::S_IFSOCK => 12,
        _ => 0,
    }
}

fn now() -> vfs::stat::Time {
    crate::time::realtime()
}
