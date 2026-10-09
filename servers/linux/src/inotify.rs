//! inotify(7): watches on the files of the server's filesystems (tmpfs and
//! /data) and the queue of their events, a file of the server with a
//! placeholder in the kernel's descriptor table, as eventfd. libuv (and so
//! Node.js's fs.watch) watches directories with it.
//!
//! The filesystem calls report what they did (`event`, `child`, `moved`,
//! `deleted`), and every watch of the inode, or of its directory for an
//! event about a name in it, whose mask takes the event gets it. A watch
//! keeps its inode (as Linux pins it) until it is removed: by
//! inotify_rm_watch, IN_ONESHOT, the inode's last link going
//! (IN_DELETE_SELF), or the instance's last descriptor. The kernel's
//! files (/dev, /proc, /sys) can be watched but report nothing, as most of
//! Linux's pseudo files.
//!
//! As Linux: an event equal to the last one queued and not read yet is
//! merged with it; beyond `MAX_EVENTS` queued events one IN_Q_OVERFLOW
//! (watch -1) stands for the ones lost; read(2) returns whole events
//! (EINVAL if the first does not fit); names are padded with NULs to a
//! multiple of 16 bytes. The directory an open file's events also go to
//! is the one of the path it was opened by.

use crate::datafs::DInode;
use crate::files::{self, File, EBADF, EFAULT, EINVAL, O_NONBLOCK, O_RDWR};
use crate::namespace::{self, Node};
use crate::sync::Mutex;
use crate::syscall;
use crate::tmpfs;
use crate::usercopy;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use restricted::*;

pub const IN_ACCESS: u32 = 0x1;
pub const IN_MODIFY: u32 = 0x2;
pub const IN_ATTRIB: u32 = 0x4;
pub const IN_CLOSE_WRITE: u32 = 0x8;
pub const IN_CLOSE_NOWRITE: u32 = 0x10;
pub const IN_OPEN: u32 = 0x20;
pub const IN_MOVED_FROM: u32 = 0x40;
pub const IN_MOVED_TO: u32 = 0x80;
pub const IN_CREATE: u32 = 0x100;
pub const IN_DELETE: u32 = 0x200;
pub const IN_DELETE_SELF: u32 = 0x400;
pub const IN_MOVE_SELF: u32 = 0x800;
const IN_ALL_EVENTS: u32 = 0xfff;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
/// The bits a watch's mask may have.
const WATCH_BITS: u32 = IN_ALL_EVENTS | IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK | IN_MASK_CREATE | IN_MASK_ADD | IN_ONESHOT;

const EAGAIN: i64 = 11;
const EINTR: i64 = 4;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const ESPIPE: i64 = 29;
const ENOTTY: i64 = 25;
const POLLIN: i16 = 0x1;

/// Linux's default fs.inotify.max_queued_events.
const MAX_EVENTS: usize = 16384;
/// `struct inotify_event` without its name.
const EVENT_HEADER: usize = 16;
const FIONREAD: u64 = 0x541b;

/// An inode that can have watches.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Key {
    Tmp(u64),
    Data(u32),
}

impl Key {
    pub fn of(node: &Node) -> Option<Key> {
        match node {
            Node::Kernel(_) => None,
            Node::Tmp(t) => Some(Key::Tmp(t.ino)),
            Node::Data(d) => Some(Key::Data(d.ino)),
        }
    }

    pub fn tmp(inode: &tmpfs::Inode) -> Key {
        Key::Tmp(inode.ino)
    }

    pub fn data(inode: &DInode) -> Key {
        Key::Data(inode.ino)
    }
}

/// What keeps a watched inode.
enum Pin {
    /// One of the kernel's: watched, without events.
    None,
    Tmp(#[allow(dead_code)] Arc<tmpfs::Inode>),
    Data(#[allow(dead_code)] Arc<DInode>),
}

struct Watch {
    key: Option<Key>,
    mask: u32,
    _pin: Pin,
}

#[derive(PartialEq, Eq)]
struct Event {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

impl Event {
    /// The name's length as reported: padded with NULs (at least one) to
    /// a multiple of 16; 0 without a name.
    fn name_len(&self) -> usize {
        if self.name.is_empty() { 0 } else { (self.name.len() + 1).next_multiple_of(EVENT_HEADER) }
    }

    fn size(&self) -> usize {
        EVENT_HEADER + self.name_len()
    }
}

struct State {
    watches: BTreeMap<i32, Watch>,
    next_wd: i32,
    queue: VecDeque<Event>,
    /// An IN_Q_OVERFLOW is queued (until it is read).
    overflowed: bool,
    reported: i16,
}

pub struct Inotify {
    id: u64,
    state: Mutex<State>,
    /// Bumped on every new event; readers sleep on it.
    seq: AtomicU32,
}

/// Every watch of the instance by inode: (instance, watch descriptor).
static REGISTRY: Mutex<BTreeMap<Key, Vec<(Weak<Inotify>, i32)>>> = Mutex::new(BTreeMap::new());
/// How many watches the registry has: the filesystem calls skip all of
/// this while there are none.
static WATCHES: AtomicUsize = AtomicUsize::new(0);
/// The cookie that ties an IN_MOVED_FROM to its IN_MOVED_TO.
static COOKIE: AtomicU32 = AtomicU32::new(1);

/// Whether any inode is watched.
pub fn active() -> bool {
    WATCHES.load(Ordering::Relaxed) != 0
}

/// inotify_init1(flags).
pub fn init(flags: u64) -> Result<i64, i64> {
    let allowed = (O_NONBLOCK | files::O_CLOEXEC) as u64;
    if flags & !allowed != 0 {
        return Err(EINVAL);
    }
    let state = State { watches: BTreeMap::new(), next_wd: 1, queue: VecDeque::new(), overflowed: false, reported: 0 };
    let i = Arc::new(Inotify { id: files::new_id(), state: Mutex::new(state), seq: AtomicU32::new(0) });
    files::install(i.id, File::Inotify(i.clone()), O_RDWR | flags as u32, 0)
}

impl Inotify {
    /// inotify_add_watch on the resolved `node` (a directory if `is_dir`).
    pub fn add_watch(self: &Arc<Self>, node: Node, is_dir: bool, mask: u32) -> Result<i64, i64> {
        if mask & !WATCH_BITS != 0 || mask & IN_ALL_EVENTS == 0 {
            return Err(EINVAL);
        }
        if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
            return Err(EINVAL);
        }
        if mask & IN_ONLYDIR != 0 && !is_dir {
            return Err(ENOTDIR);
        }
        let key = Key::of(&node);
        let events = mask & (IN_ALL_EVENTS | IN_EXCL_UNLINK | IN_ONESHOT);
        let wd = {
            let mut st = self.state.lock();
            let existing = key.and_then(|k| st.watches.iter().find(|(_, w)| w.key == Some(k)).map(|(&wd, _)| wd));
            if let Some(wd) = existing {
                if mask & IN_MASK_CREATE != 0 {
                    return Err(EEXIST);
                }
                let w = st.watches.get_mut(&wd).expect("found above");
                w.mask = if mask & IN_MASK_ADD != 0 { w.mask | events } else { events };
                return Ok(wd as i64);
            }
            let wd = st.next_wd;
            st.next_wd = st.next_wd.checked_add(1).unwrap_or(1);
            let pin = match node {
                Node::Kernel(_) => Pin::None,
                Node::Tmp(t) => Pin::Tmp(t),
                Node::Data(d) => Pin::Data(d),
            };
            st.watches.insert(wd, Watch { key, mask: events, _pin: pin });
            wd
        };
        if let Some(k) = key {
            REGISTRY.lock().entry(k).or_default().push((Arc::downgrade(self), wd));
            WATCHES.fetch_add(1, Ordering::Relaxed);
        }
        Ok(wd as i64)
    }

    /// inotify_rm_watch: the watch goes, with an IN_IGNORED.
    pub fn rm_watch(&self, wd: i32) -> Result<i64, i64> {
        let key = {
            let mut st = self.state.lock();
            let w = st.watches.remove(&wd).ok_or(EINVAL)?;
            self.queue(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
            w.key
        };
        if let Some(k) = key {
            unregister(k, self, wd);
        }
        Ok(0)
    }

    /// Queues `e` (lock held), merged with an equal last one, and wakes the
    /// readers.
    fn queue(&self, st: &mut State, e: Event) {
        if st.queue.back() == Some(&e) {
            return;
        }
        if st.queue.len() >= MAX_EVENTS {
            if st.overflowed {
                return;
            }
            st.overflowed = true;
            st.queue.push_back(Event { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: Vec::new() });
        } else {
            st.queue.push_back(e);
        }
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        // Each event is an edge for EPOLLET.
        st.reported = POLLIN;
        files::ready(self.id, POLLIN);
    }

    /// Delivers event `mask` to watch `wd` if its mask takes it; whether
    /// the watch ended (IN_ONESHOT).
    fn deliver(&self, wd: i32, mask: u32, cookie: u32, name: &[u8]) -> bool {
        let mut st = self.state.lock();
        let Some(w) = st.watches.get(&wd) else { return false };
        if w.mask & mask & IN_ALL_EVENTS == 0 {
            return false;
        }
        let oneshot = w.mask & IN_ONESHOT != 0;
        self.queue(&mut st, Event { wd, mask, cookie, name: name.to_vec() });
        if oneshot {
            st.watches.remove(&wd);
            self.queue(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
        }
        oneshot
    }

    /// The inode's watches end (its last link went): IN_IGNORED.
    fn ignore(&self, wd: i32) {
        let mut st = self.state.lock();
        if st.watches.remove(&wd).is_some() {
            self.queue(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
        }
    }

    /// read(2): whole events into the program's buffer at `buf`.
    fn read(&self, buf: u64, len: u64, nonblock: bool) -> Result<i64, i64> {
        loop {
            let seen;
            {
                let mut st = self.state.lock();
                if !st.queue.is_empty() {
                    let mut out = Vec::new();
                    while let Some(e) = st.queue.front() {
                        if out.len() + e.size() > len as usize {
                            break;
                        }
                        out.extend_from_slice(&e.wd.to_le_bytes());
                        out.extend_from_slice(&e.mask.to_le_bytes());
                        out.extend_from_slice(&e.cookie.to_le_bytes());
                        out.extend_from_slice(&(e.name_len() as u32).to_le_bytes());
                        out.extend_from_slice(&e.name);
                        out.resize(out.len() + e.name_len() - e.name.len(), 0);
                        st.queue.pop_front();
                    }
                    if out.is_empty() {
                        return Err(EINVAL);
                    }
                    usercopy::to_program(buf, &out)?;
                    if st.queue.iter().all(|e| e.mask != IN_Q_OVERFLOW) {
                        st.overflowed = false;
                    }
                    let now = if st.queue.is_empty() { 0 } else { POLLIN };
                    if now != st.reported {
                        st.reported = now;
                        files::ready(self.id, now);
                    }
                    return Ok(out.len() as i64);
                }
                seen = self.seq.load(Ordering::Acquire);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            let word = &self.seq as *const AtomicU32 as u64;
            if syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, 0, FUTEX_INTERRUPTIBLE, 0, 0]) == -EINTR {
                return Err(EINTR);
            }
        }
    }

    /// Its `struct stat`: an anonymous inode, as on Linux.
    pub fn stat(&self) -> [u8; 144] {
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.id.to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&0o600u32.to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        let keys: Vec<Key> = self.state.lock().watches.values().filter_map(|w| w.key).collect();
        let mut r = REGISTRY.lock();
        for k in keys {
            if let Some(list) = r.get_mut(&k) {
                let before = list.len();
                list.retain(|(w, _)| w.strong_count() > 0);
                WATCHES.fetch_sub(before - list.len(), Ordering::Relaxed);
                if list.is_empty() {
                    r.remove(&k);
                }
            }
        }
    }
}

fn unregister(key: Key, instance: &Inotify, wd: i32) {
    let mut r = REGISTRY.lock();
    if let Some(list) = r.get_mut(&key) {
        let before = list.len();
        list.retain(|(w, d)| !(*d == wd && core::ptr::eq(w.as_ptr(), instance)));
        WATCHES.fetch_sub(before - list.len(), Ordering::Relaxed);
        if list.is_empty() {
            r.remove(&key);
        }
    }
}

/// The watches of `key` (taken out of the registry's lock).
fn watchers(key: Key) -> Vec<(Arc<Inotify>, i32)> {
    REGISTRY.lock().get(&key).map(|l| l.iter().filter_map(|(w, wd)| Some((w.upgrade()?, *wd))).collect()).unwrap_or_default()
}

fn send(key: Key, mask: u32, cookie: u32, name: &[u8]) {
    for (i, wd) in watchers(key) {
        if i.deliver(wd, mask, cookie, name) {
            unregister(key, &i, wd);
        }
    }
}

/// Event `mask` on the inode `key` (a directory if `dir`), and on the
/// directory `parent` about its name there.
pub fn event(key: Key, dir: bool, mask: u32, parent: Option<(Key, &str)>) {
    if !active() {
        return;
    }
    let isdir = if dir { IN_ISDIR } else { 0 };
    send(key, mask | isdir, 0, &[]);
    if let Some((p, name)) = parent {
        send(p, mask | isdir, 0, name.as_bytes());
    }
}

/// Event `mask` (IN_CREATE, IN_DELETE) about `name` in the directory
/// `dir`; `is_dir`: the name is a directory's.
pub fn child(dir: &Node, mask: u32, name: &str, is_dir: bool) {
    if !active() {
        return;
    }
    if let Some(k) = Key::of(dir) {
        send(k, mask | if is_dir { IN_ISDIR } else { 0 }, 0, name.as_bytes());
    }
}

/// A rename: IN_MOVED_FROM and IN_MOVED_TO with one cookie, IN_MOVE_SELF
/// for the inode that moved.
pub fn moved(odir: &Node, oname: &str, ndir: &Node, nname: &str, node: Option<Key>, is_dir: bool) {
    if !active() {
        return;
    }
    let cookie = COOKIE.fetch_add(1, Ordering::Relaxed);
    let isdir = if is_dir { IN_ISDIR } else { 0 };
    if let Some(k) = Key::of(odir) {
        send(k, IN_MOVED_FROM | isdir, cookie, oname.as_bytes());
    }
    if let Some(k) = Key::of(ndir) {
        send(k, IN_MOVED_TO | isdir, cookie, nname.as_bytes());
    }
    if let Some(k) = node {
        send(k, IN_MOVE_SELF | isdir, 0, &[]);
    }
}

/// The inode `key` lost its last link: IN_DELETE_SELF, and its watches
/// end. (Its IN_ATTRIB for the link count comes before, from the caller.)
pub fn deleted(key: Key, dir: bool) {
    if !active() {
        return;
    }
    let isdir = if dir { IN_ISDIR } else { 0 };
    send(key, IN_DELETE_SELF | isdir, 0, &[]);
    let gone = REGISTRY.lock().remove(&key).unwrap_or_default();
    WATCHES.fetch_sub(gone.len(), Ordering::Relaxed);
    for (w, wd) in gone {
        if let Some(i) = w.upgrade() {
            i.ignore(wd);
        }
    }
}

/// The directory an absolute `path` names its last component in, and that
/// name (for the events of a file opened by `path`).
pub fn parent_of(path: &str) -> Option<(Key, String)> {
    if !active() {
        return None;
    }
    let (dir, name) = namespace::resolve_parent("/", path).ok()?;
    Some((Key::of(&dir.node)?, name))
}

/// Event `mask` on an inode reached by `path` (absolute): to it, and to
/// its directory about its name.
pub fn on_path(key: Key, dir: bool, mask: u32, path: &str) {
    if !active() {
        return;
    }
    let parent = parent_of(path);
    event(key, dir, mask, parent.as_ref().map(|(k, n)| (*k, n.as_str())));
}

pub const SYS_INOTIFY_INIT: u64 = 253;
pub const SYS_INOTIFY_ADD_WATCH: u64 = 254;
pub const SYS_INOTIFY_RM_WATCH: u64 = 255;
pub const SYS_INOTIFY_INIT1: u64 = 294;

/// The calls on an inotify descriptor (`a1`, `a2`: the arguments after it).
pub fn call(nr: u64, i: &Inotify, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        files::SYS_READ => i.read(a1, a2, nonblock),
        files::SYS_READV => {
            // The first buffer, as Linux's read_iter takes one event list.
            let vecs = files::iovecs(a1, a2)?;
            match vecs.iter().find(|v| v.1 > 0) {
                Some(&(base, len)) => i.read(base, len, nonblock),
                None => Ok(0),
            }
        }
        files::SYS_WRITE | files::SYS_WRITEV => Err(EINVAL),
        files::SYS_FSTAT => usercopy::to_program(a1, &i.stat()).map(|_| 0).map_err(|_| EFAULT),
        files::SYS_IOCTL if a1 == FIONREAD => {
            let n: usize = i.state.lock().queue.iter().map(Event::size).sum();
            usercopy::write(a2, &(n as i32))?;
            Ok(0)
        }
        files::SYS_IOCTL => Err(ENOTTY),
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(ESPIPE),
        _ => Err(EINVAL),
    }
}

/// inotify_rm_watch(fd, wd) and inotify_add_watch's descriptor check.
pub fn instance(fd: u64) -> Result<Arc<Inotify>, i64> {
    match files::lookup(fd) {
        Some((File::Inotify(i), _)) => Ok(i),
        // Another file (also one of the kernel's): not an inotify instance.
        Some(_) => Err(EINVAL),
        None => {
            let mut st = [0u8; 144];
            match syscall(SYS_KFD_STAT, [fd, st.as_mut_ptr() as u64, 0, 0, 0, 0]) {
                r if r == -EBADF => Err(EBADF),
                _ => Err(EINVAL),
            }
        }
    }
}
