//! inotify(7): watches on the files of the server's filesystems (tmpfs and
//! /data) and the queue of their events, a file of the server, as eventfd.
//! libuv (and so
//! Node.js's fs.watch) watches directories with it.
//!
//! Watches are keyed by inode (`Key`: tmpfs's inode numbers are never
//! reused in an instance, /data's are the disk's), so they need not keep
//! their inode: a /data inode the server let go of and looked up again is
//! the same key. The filesystem calls report what they did (`event`,
//! `child`, `moved`, `deleted`), and every watch of the inode, or of its
//! directory for an event about a name in it, whose mask takes the event
//! gets it. The directory and name come from the inode itself (`link`: the
//! name it was created, found or renamed by), not from a path. A watch
//! ends by inotify_rm_watch, IN_ONESHOT, the instance's last descriptor,
//! or when its inode goes: IN_DELETE_SELF once its last link went and its
//! last open file description closed, as Linux sends it when the inode is
//! evicted. The kernel's files (/dev, /proc, /sys) can be watched but
//! report nothing, as most of Linux's pseudo files; changes another
//! instance makes on /data are not seen (its calls are its own server's).
//!
//! As Linux: an event equal to the last one queued and not read yet is
//! merged with it; beyond `MAX_EVENTS` queued events one IN_Q_OVERFLOW
//! (watch -1) stands for the ones lost; at most `MAX_INSTANCES` instances
//! (EMFILE) and `MAX_WATCHES` watches (ENOSPC) per user, Linux's defaults
//! of fs.inotify.max_user_instances and max_user_watches (one user: the
//! instance's); read(2) returns whole events (EINVAL if the first does not
//! fit); names are padded with NULs to a multiple of 16 bytes. No lock is
//! held while events are copied to the program (a fault there may need the
//! pager, which reports closes here).

use crate::datafs::DInode;
use crate::files::{self, File, EFAULT, EINVAL, O_NONBLOCK, O_RDWR};
use crate::namespace::Node;
use crate::sync::Mutex;
use crate::syscall;
use crate::tmpfs;
use crate::usercopy;
use alloc::collections::{BTreeMap, VecDeque};
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
const EMFILE: i64 = 24;
const ENOTTY: i64 = 25;
const ENOSPC: i64 = 28;
const ESPIPE: i64 = 29;
const POLLIN: i16 = 0x1;

/// Linux's defaults of fs.inotify.max_queued_events, max_user_instances
/// and max_user_watches.
pub const MAX_EVENTS: usize = 16384;
pub const MAX_INSTANCES: usize = 128;
pub const MAX_WATCHES: usize = 8192;
/// Most bytes all instances of the tree may hold in queued events (with what each takes
/// besides its name): beyond it events are lost as by a full queue (IN_Q_OVERFLOW), so
/// 128 instances' queues cannot fill the server's heap.
const MAX_QUEUED_BYTES: usize = 8 << 20;
static QUEUED_BYTES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// What a queued event takes.
fn cost(e: &Event) -> usize {
    48 + e.name.len()
}
/// `struct inotify_event` without its name.
const EVENT_HEADER: usize = 16;
const FIONREAD: u64 = 0x541b;

/// An inode that can have watches.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Key {
    Tmp(u64),
    /// A /data inode: its number and its ext2 generation (a number another
    /// instance freed and gave to a new file is another file).
    Data(u32, u32),
    /// Every /data inode of this number (`deleted` only: one this instance
    /// removed and no longer has, so its generation is unknown here).
    DataAny(u32),
    /// A pseudo file's (the kernel's /dev, /proc, /sys) by device and
    /// inode number: watched, without events (as Linux's procfs sends
    /// none for contents that change by themselves).
    Pseudo(u64, u64),
}

impl Key {
    pub fn of(node: &Node) -> Option<Key> {
        match node {
            Node::Kernel(_) | Node::Proc(_) => {
                let st = node.status().ok()?;
                Some(Key::Pseudo(st.dev, st.ino))
            }
            Node::Tmp(t) => Some(Key::Tmp(t.ino)),
            Node::Data(d) => Some(Key::data(d)),
        }
    }

    pub fn tmp(inode: &tmpfs::Inode) -> Key {
        Key::Tmp(inode.ino)
    }

    pub fn data(inode: &DInode) -> Key {
        Key::Data(inode.ino, inode.generation())
    }
}

struct Watch {
    key: Key,
    mask: u32,
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

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.wd.to_le_bytes());
        out.extend_from_slice(&self.mask.to_le_bytes());
        out.extend_from_slice(&self.cookie.to_le_bytes());
        out.extend_from_slice(&(self.name_len() as u32).to_le_bytes());
        out.extend_from_slice(&self.name);
        out.resize(out.len() + self.name_len() - self.name.len(), 0);
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
/// How many watches the registry has (`MAX_WATCHES`): the filesystem calls
/// skip all of this while there are none.
static WATCHES: AtomicUsize = AtomicUsize::new(0);
/// How many instances there are (`MAX_INSTANCES`).
static INSTANCES: AtomicUsize = AtomicUsize::new(0);
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
    INSTANCES.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < MAX_INSTANCES).then_some(n + 1)).map_err(|_| EMFILE)?;
    let state = State { watches: BTreeMap::new(), next_wd: 1, queue: VecDeque::new(), overflowed: false, reported: 0 };
    let i = Arc::new(Inotify { id: files::new_id(), state: Mutex::new(state), seq: AtomicU32::new(0) });
    // (Dropping `i` on failure gives the instance back.)
    files::install(i.id, File::Inotify(i.clone()), O_RDWR | flags as u32)
}

impl Inotify {
    /// Its readiness for poll and epoll now: readable with events queued.
    pub fn readiness_now(&self) -> i16 {
        if self.state.lock().queue.is_empty() { 0 } else { POLLIN }
    }

    /// inotify_add_watch on the resolved `node` (a directory if `is_dir`).
    pub fn add_watch(self: &Arc<Self>, node: &Node, is_dir: bool, mask: u32) -> Result<i64, i64> {
        if mask & !WATCH_BITS != 0 || mask & IN_ALL_EVENTS == 0 {
            return Err(EINVAL);
        }
        if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
            return Err(EINVAL);
        }
        if mask & IN_ONLYDIR != 0 && !is_dir {
            return Err(ENOTDIR);
        }
        let key = Key::of(node).ok_or(EINVAL)?;
        let events = mask & (IN_ALL_EVENTS | IN_EXCL_UNLINK | IN_ONESHOT);
        let wd = {
            let mut st = self.state.lock();
            let existing = st.watches.iter().find(|(_, w)| w.key == key).map(|(&wd, _)| wd);
            if let Some(wd) = existing {
                if mask & IN_MASK_CREATE != 0 {
                    return Err(EEXIST);
                }
                let w = st.watches.get_mut(&wd).expect("found above");
                w.mask = if mask & IN_MASK_ADD != 0 { w.mask | events } else { events };
                return Ok(wd as i64);
            }
            WATCHES.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < MAX_WATCHES).then_some(n + 1)).map_err(|_| ENOSPC)?;
            // The next descriptor not in use (they wrap at i32::MAX).
            let mut wd = st.next_wd;
            while st.watches.contains_key(&wd) {
                wd = if wd == i32::MAX { 1 } else { wd + 1 };
            }
            st.next_wd = if wd == i32::MAX { 1 } else { wd + 1 };
            st.watches.insert(wd, Watch { key, mask: events });
            // In the registry under the instance's lock (instance, then
            // registry: the only nesting), so rm_watch never misses it.
            REGISTRY.lock().entry(key).or_default().push((Arc::downgrade(self), wd));
            wd
        };
        Ok(wd as i64)
    }

    /// inotify_rm_watch: the watch goes, with an IN_IGNORED.
    pub fn rm_watch(&self, wd: i32) -> Result<i64, i64> {
        let mut st = self.state.lock();
        let w = st.watches.remove(&wd).ok_or(EINVAL)?;
        self.queue(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
        unregister(w.key, self, wd);
        Ok(0)
    }

    /// Queues `e` (lock held), merged with an equal last one, and wakes the
    /// readers.
    fn queue(&self, st: &mut State, e: Event) {
        if st.queue.back() == Some(&e) {
            return;
        }
        let room = st.queue.len() < MAX_EVENTS
            && QUEUED_BYTES.load(Ordering::Relaxed) + cost(&e) <= MAX_QUEUED_BYTES
            && st.queue.try_reserve(1).is_ok();
        let e = if room {
            e
        } else {
            if st.overflowed {
                return;
            }
            st.overflowed = true;
            // (The overflow event's room: within the reservation the bounds above leave, or
            // none: then it is dropped too, and the reader sees the overflow as the queue's end.)
            if st.queue.try_reserve(1).is_err() {
                return;
            }
            Event { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: Vec::new() }
        };
        QUEUED_BYTES.fetch_add(cost(&e), Ordering::Relaxed);
        st.queue.push_back(e);
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        // Each event is an edge for EPOLLET.
        st.reported = POLLIN;
        files::ready(self.id, POLLIN);
    }

    /// Delivers event `mask` about `key` to watch `wd` if it is still that
    /// inode's and its mask takes it; whether the watch ended (IN_ONESHOT).
    fn deliver(&self, key: Key, wd: i32, mask: u32, cookie: u32, name: &[u8]) -> bool {
        let mut st = self.state.lock();
        let Some(w) = st.watches.get(&wd) else { return false };
        if w.key != key || w.mask & mask & IN_ALL_EVENTS == 0 {
            return false;
        }
        let oneshot = w.mask & IN_ONESHOT != 0;
        let key = w.key;
        self.queue(&mut st, Event { wd, mask, cookie, name: name.to_vec() });
        if oneshot {
            st.watches.remove(&wd);
            self.queue(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
            unregister(key, self, wd);
        }
        oneshot
    }

    /// The watch's inode went: IN_IGNORED.
    fn ignore(&self, wd: i32) {
        let mut st = self.state.lock();
        if st.watches.remove(&wd).is_some() {
            self.queue(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
        }
    }

    /// read(2): whole events into the program's buffer at `buf`. They are
    /// taken out under the lock and copied after it (lost if the copy
    /// faults, as on Linux).
    fn read(&self, buf: u64, len: u64, nonblock: bool) -> Result<i64, i64> {
        let out = loop {
            let seen;
            {
                let mut st = self.state.lock();
                if !st.queue.is_empty() {
                    let mut out = Vec::new();
                    while let Some(e) = st.queue.front() {
                        if out.len() + e.size() > len as usize {
                            break;
                        }
                        e.encode(&mut out);
                        let (overflow, taken) = (e.mask == IN_Q_OVERFLOW, cost(e));
                        if overflow {
                            st.overflowed = false;
                        }
                        QUEUED_BYTES.fetch_sub(taken, Ordering::Relaxed);
                        st.queue.pop_front();
                    }
                    if out.is_empty() {
                        return Err(EINVAL);
                    }
                    let now = if st.queue.is_empty() { 0 } else { POLLIN };
                    if now != st.reported {
                        st.reported = now;
                        files::ready(self.id, now);
                    }
                    break out;
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
        };
        usercopy::to_program(buf, &out)?;
        Ok(out.len() as i64)
    }

    /// The bytes of the queued events (FIONREAD).
    fn queued_bytes(&self) -> usize {
        self.state.lock().queue.iter().map(Event::size).sum()
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
        INSTANCES.fetch_sub(1, Ordering::Relaxed);
        let queued: usize = self.state.lock().queue.iter().map(cost).sum();
        QUEUED_BYTES.fetch_sub(queued, Ordering::Relaxed);
        let keys: Vec<Key> = self.state.lock().watches.values().map(|w| w.key).collect();
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
        i.deliver(key, wd, mask, cookie, name);
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

/// Event `mask` on a tmpfs inode and on its directory.
pub fn tmp_event(inode: &tmpfs::Inode, mask: u32) {
    if active() {
        let link = inode.link();
        event(Key::tmp(inode), inode.is_dir(), mask, link.as_ref().map(|(d, n)| (Key::Tmp(*d), n.as_str())));
    }
}

/// Event `mask` on a /data inode and on its directory.
pub fn data_event(inode: &DInode, mask: u32) {
    if active() {
        let link = inode.link();
        event(Key::data(inode), inode.kind == vfs::S_IFDIR, mask, link.as_ref().map(|(d, g, n)| (Key::Data(*d, *g), n.as_str())));
    }
}

/// Event `mask` on the inode `node` and on its directory.
pub fn node_event(node: &Node, mask: u32) {
    match node {
        Node::Tmp(t) => tmp_event(t, mask),
        Node::Data(d) => data_event(d, mask),
        Node::Kernel(_) | Node::Proc(_) => {}
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

/// The inode `key` went (its last link and its last open file description
/// are gone): IN_DELETE_SELF, and its watches end.
pub fn deleted(key: Key, dir: bool) {
    if !active() {
        return;
    }
    let isdir = if dir { IN_ISDIR } else { 0 };
    let keys: Vec<Key> = match key {
        Key::DataAny(ino) => REGISTRY.lock().keys().filter(|k| matches!(k, Key::Data(i, _) if *i == ino)).copied().collect(),
        k => alloc::vec![k],
    };
    for key in keys {
        send(key, IN_DELETE_SELF | isdir, 0, &[]);
        let gone = REGISTRY.lock().remove(&key).unwrap_or_default();
        WATCHES.fetch_sub(gone.len(), Ordering::Relaxed);
        for (w, wd) in gone {
            if let Some(i) = w.upgrade() {
                i.ignore(wd);
            }
        }
    }
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
            let n = i.queued_bytes();
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
    // (An O_PATH descriptor is no file: EBADF, Linux's fdget.)
    match &files::lookup(fd)?.file {
        File::Inotify(i) => Ok(i.clone()),
        // Another file (also one of the kernel's): not an inotify instance.
        _ => Err(EINVAL),
    }
}
