//! The server's tmpfs (phase R6c.2c): directories, files and symlinks in
//! the server's memory; a file's contents are a file object of the
//! kernel's (`SYS_MO_CREATE_FILE`), read, written and mapped without the
//! kernel's VFS, and charged to the same tmpfs limit as the kernel's tmpfs.
//!
//! Locking: each inode has its own lock (its directory map, permissions
//! and write count). Code holds one inode lock at a time, except under
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
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use restricted::*;

pub const EEXIST: i64 = 17;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const ENOSPC: i64 = 28;
pub const ENAMETOOLONG: i64 = 36;
pub const ENOTEMPTY: i64 = 39;
pub const ETXTBSY: i64 = 26;

/// st_dev of the server's tmpfs (the kernel's files have 0), so that
/// tools that tell files apart by device and inode number never mistake
/// one of each for the same file.
pub const DEV: u64 = 0x1a;

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
}

pub struct State {
    pub perm: u32,
    pub kind: Kind,
    /// > 0: write accesses; < 0: programs running from it (see above).
    writers: i64,
    /// A directory that was removed: nothing new goes into it (a lookup
    /// may have found it just before).
    removed: bool,
}

pub struct Inode {
    pub ino: u64,
    pub state: Mutex<State>,
    /// Taken by O_APPEND writes: finding the end and writing there is one
    /// step for every appender.
    pub append: Mutex<()>,
}

static NEXT_INO: AtomicU64 = AtomicU64::new(1);
/// Taken by renames and removals (see the module comment).
static RENAME: Mutex<()> = Mutex::new(());

/// A new, empty tmpfs: its root directory.
pub fn new_root() -> Arc<Inode> {
    Inode::new(Kind::Dir(BTreeMap::new()), 0o1777)
}

impl Inode {
    fn new(kind: Kind, perm: u32) -> Arc<Inode> {
        Arc::new(Inode { ino: NEXT_INO.fetch_add(1, Ordering::Relaxed), state: Mutex::new(State { perm, kind, writers: 0, removed: false }), append: Mutex::new(()) })
    }

    pub fn mode(&self) -> u32 {
        let st = self.state.lock();
        kind_bits(&st.kind) | st.perm
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.state.lock().kind, Kind::Dir(_))
    }

    /// The file object's handle (EISDIR for a directory, EINVAL else).
    pub fn object(&self) -> Result<u64, i64> {
        match &self.state.lock().kind {
            Kind::File(o) => Ok(o.handle()),
            Kind::Dir(_) => Err(EISDIR),
            Kind::Symlink(_) => Err(EINVAL),
        }
    }

    pub fn lookup(&self, name: &str) -> Result<Arc<Inode>, i64> {
        match &self.state.lock().kind {
            Kind::Dir(m) => m.get(name).cloned().ok_or(ENOENT),
            _ => Err(ENOTDIR),
        }
    }

    pub fn readlink(&self) -> Result<String, i64> {
        match &self.state.lock().kind {
            Kind::Symlink(t) => Ok(t.clone()),
            _ => Err(EINVAL),
        }
    }

    pub fn size(&self) -> u64 {
        match &self.state.lock().kind {
            Kind::File(o) => syscall(SYS_MO_FILE_SIZE, [o.handle(), 0, 0, 0, 0, 0]).max(0) as u64,
            Kind::Symlink(t) => t.len() as u64,
            Kind::Dir(m) => m.len() as u64,
        }
    }

    /// Its `struct stat`, as the kernel's tmpfs reports one.
    pub fn stat(&self) -> [u8; 144] {
        let mode = self.mode();
        let size = self.size();
        let nlink: u64 = if mode & vfs::S_IFMT == vfs::S_IFDIR { 2 } else { 1 };
        let time = mount_time();
        let mut st = [0u8; 144];
        st[0..8].copy_from_slice(&DEV.to_le_bytes());
        st[8..16].copy_from_slice(&self.ino.to_le_bytes());
        st[16..24].copy_from_slice(&nlink.to_le_bytes());
        st[24..28].copy_from_slice(&mode.to_le_bytes());
        st[48..56].copy_from_slice(&size.to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes());
        for at in [72, 88, 104] {
            st[at..at + 8].copy_from_slice(&time.to_le_bytes());
        }
        st
    }

    pub fn set_perm(&self, perm: u32) {
        self.state.lock().perm = perm & 0o7777;
    }

    /// Directory entries (name, inode number, dirent type), "." and ".."
    /// first.
    pub fn list(&self) -> Result<Vec<(String, u64, u8)>, i64> {
        let children: Vec<(String, Arc<Inode>)> = match &self.state.lock().kind {
            Kind::Dir(m) => m.iter().map(|(n, c)| (n.clone(), c.clone())).collect(),
            _ => return Err(ENOTDIR),
        };
        let mut out = Vec::with_capacity(children.len() + 2);
        out.push((String::from("."), self.ino, 4));
        out.push((String::from(".."), self.ino, 4));
        out.extend(children.into_iter().map(|(n, c)| (n, c.ino, dtype(c.mode()))));
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
        let inode = Inode::new(kind, perm & 0o7777);
        self.insert(name, inode.clone())?;
        Ok(inode)
    }

    /// A new file `name` whose contents are the file object `handle` (the
    /// file takes it over, also on failure).
    pub fn insert_object(&self, name: &str, handle: u64, perm: u32) -> Result<(), i64> {
        let object = Object(handle);
        check_name(name)?;
        self.insert(name, Inode::new(Kind::File(object), perm & 0o7777))
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
        self.insert(name, Inode::new(Kind::Symlink(target), 0o777))
    }

    fn insert(&self, name: &str, inode: Arc<Inode>) -> Result<(), i64> {
        let mut st = self.state.lock();
        if st.removed {
            return Err(ENOENT);
        }
        match &mut st.kind {
            Kind::Dir(m) if m.contains_key(name) => Err(EEXIST),
            Kind::Dir(m) => {
                m.insert(String::from(name), inode);
                Ok(())
            }
            _ => Err(ENOTDIR),
        }
    }

    /// Removes `name`: a directory only with `dir_only` (rmdir) and only
    /// when empty, anything else only without (checked under the child's
    /// lock, so no file appears in a directory being removed).
    pub fn unlink(&self, name: &str, dir_only: bool) -> Result<(), i64> {
        let _nesting = RENAME.lock();
        let mut st = self.state.lock();
        let Kind::Dir(m) = &mut st.kind else { return Err(ENOTDIR) };
        let child = m.get(name).ok_or(ENOENT)?.clone();
        let mut cst = child.state.lock();
        let child_dir = match &cst.kind {
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
        drop(cst);
        let removed = m.remove(name);
        drop(st);
        drop(removed);
        Ok(())
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
/// queued so far.
fn settle() {
    let target = syscall(SYS_EVENT_RELEASES, [0; 6]) as u32;
    loop {
        let done = RELEASES_HANDLED.load(Ordering::Acquire);
        if done.wrapping_sub(target) as i32 >= 0 {
            return;
        }
        syscall(SYS_SERVER_FUTEX_WAIT, [&RELEASES_HANDLED as *const AtomicU32 as u64, done as u64, 0, 0, 0, 0]);
    }
}

/// `try_once`, and if it finds the file busy (ETXTBSY), once more after the
/// releases reported until then are in (also for /data's files).
pub(crate) fn settled(try_once: impl Fn() -> Result<(), i64>) -> Result<(), i64> {
    match try_once() {
        Err(ETXTBSY) => {
            settle();
            try_once()
        }
        r => r,
    }
}

/// Renames `odir/oname` to `ndir/nname` (both in this tmpfs), replacing a
/// file or an empty directory there.
pub fn rename(odir: &Arc<Inode>, oname: &str, ndir: &Arc<Inode>, nname: &str) -> Result<(), i64> {
    check_name(nname)?;
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
    drop(second_guard);
    drop(first_guard);
    drop(replaced);
    Ok(())
}

fn dir_map(st: &mut State) -> Result<&mut BTreeMap<String, Arc<Inode>>, i64> {
    match &mut st.kind {
        Kind::Dir(m) => Ok(m),
        _ => Err(ENOTDIR),
    }
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
        let existing_dir = match &est.kind {
            Kind::Dir(m) => Some(m.is_empty()),
            _ => None,
        };
        // Replacing only ever drops a file or an empty directory.
        match (existing_dir, node_dir) {
            (Some(false), true) => return Err(ENOTEMPTY),
            (Some(_), false) => return Err(EISDIR),
            (None, true) => return Err(ENOTDIR),
            (Some(true), true) => est.removed = true,
            _ => {}
        }
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
        if let Kind::Dir(m) = &d.state.lock().kind {
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

fn kind_bits(kind: &Kind) -> u32 {
    match kind {
        Kind::Dir(_) => vfs::S_IFDIR,
        Kind::File(_) => vfs::S_IFREG,
        Kind::Symlink(_) => vfs::S_IFLNK,
    }
}

pub fn dtype(mode: u32) -> u8 {
    match mode & vfs::S_IFMT {
        vfs::S_IFDIR => 4,
        vfs::S_IFREG => 8,
        vfs::S_IFLNK => 10,
        _ => 0,
    }
}

/// The time every inode reports (the kernel's tmpfs stores no times
/// either): when the instance's tmpfs came up, in seconds since 1970.
fn mount_time() -> u64 {
    static TIME: AtomicU64 = AtomicU64::new(0);
    let t = TIME.load(Ordering::Relaxed);
    if t != 0 {
        return t;
    }
    const CLOCK_REALTIME: u64 = 0;
    let now = syscall(SYS_CLOCK_READ, [CLOCK_REALTIME, 0, 0, 0, 0, 0]).max(0) as u64 / 1_000_000_000;
    TIME.store(now, Ordering::Relaxed);
    now
}
