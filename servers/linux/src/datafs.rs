//! /data in the server (phase R6c.3, I/O rings step 4): diskfs's ext2
//! filesystem through the file protocol (`fsring`, over `fsclient`'s
//! channel), with the server's own page cache.
//!
//! **Inodes.** A `DInode` is a disk inode the server uses. There is at most
//! one per inode number (`TABLE`), and it exists exactly while the server
//! holds the inode in diskfs (fsring, "Holds"): from the request that
//! returned it until its `RELEASE`, sent when the server lets go of it
//! (`evict`). Its users hold an `Arc`: open files, path walks, the holds
//! of mappings and running programs (`hold`), the table itself. Unused
//! inodes stay cached (up to `MAX_CACHED`, least recently used first out:
//! repeated paths need no lookup of their inodes again, and a file's
//! pages outlive its last close); an unlinked one goes as soon as nothing
//! uses it, so its blocks are free when unlink(2) or close(2) returns. The
//! release must not overtake a request that returns the same inode again:
//! requests that can return an inode (LOOKUP, CREATE, UNLINK, RENAME) hold
//! `NAMES` for reading from their submission until the inode is in the
//! table, a release holds it for writing while it takes the inode out and
//! submits; requests in one ring are taken in order, so a lookup submitted
//! later re-establishes the hold.
//!
//! **The page cache.** A regular file the server reads, writes, maps or
//! runs has a cached object of the kernel's (`SYS_MO_CREATE_CACHED`, made
//! on first use, kept with the inode): one set of pages for every
//! descriptor, mapping and program, whose size is the file's size here.
//! Reads and writes copy between the object and the program in the kernel
//! (`SYS_MO_FILE_READ`/`WRITE` with `MO_NOFILL`); a missing page they meet
//! is filled here and the call repeated; a fault on a mapping (or the
//! kernel reading the object) asks the pager thread (`EVENT_PAGE`). A fill
//! grants a run of missing pages (`GRANT_FILL`) and has diskfs read into
//! them by DMA: the pages programs map are the pages the device wrote.
//! Read-ahead: a fill takes `READAHEAD` pages, doubling up to `MAX_WINDOW`
//! (4 MiB: four `READ`s of `MAX_RUN`, 1 MiB, in flight at once) while the
//! file is read in order.
//!
//! **Write-back.** Writes and stores make pages dirty; the data reaches the
//! disk when they are written back: by fsync, fdatasync, sync, msync
//! (MS_SYNC), O_SYNC and O_DSYNC writes and RWF_(D)SYNC (each with a
//! `FLUSH`: durable then), by the pager `WRITEBACK_AGE` after a file got
//! dirty (`EVENT_DIRTY`), when the kernel asks for room (`EVENT_WRITEBACK`)
//! and when the instance ends (`EVENT_CLOSING`). A write-back grants runs
//! of dirty pages (`GRANT_DIRTY`: clean and write-protected from then on)
//! and sends `WRITE`s from them, many in flight; a write that failed makes
//! its pages dirty again. As on Linux, write(2) does not wait for the
//! disk. Every fill and write-back lets diskfs go of its grant (`FORGET`)
//! before revoking it, so no revoke drains.
//!
//! **Memory.** Cached pages are the kernel's cached memory: clean ones are
//! reclaimed when memory is short, dirty ones are bounded by the kernel's
//! dirty limits (the pager is asked to write back above a tenth of the
//! commit limit, writers wait above a fifth), pinned ones by `PINNED` per
//! fill or write-back in flight. A fill that finds no memory for a page
//! writes the dirty files back (reclaim can drop their pages then) and
//! tries again, `FILL_ROUNDS` times at most (programs that keep dirtying
//! pages could otherwise keep it writing for ever); it fails (ENOMEM) when
//! nothing is left to write or the rounds are used up. Unused inodes are
//! bounded by `MAX_CACHED`; their objects keep pages only until reclaim
//! takes them.
//!
//! **Disk space.** A write secures its pages' blocks before their data
//! enters the cache (`secure`: diskfs `PROMISE`s them, at most a transfer
//! at a time; the kernel then takes the data, `MO_BACKED`), so a full disk
//! fails `write(2)` itself with ENOSPC (a short write if some went in),
//! never the write-back; a store through a mapping into a page not backed
//! asks the pager (`EVENT_MKWRITE`, `mkwrite`: SIGBUS without room). After
//! a diskfs restart the dirty pages are promised again (`repromise`). A
//! final write-back that fails anyway is logged on the console.
//!
//! **Bounded waits.** No loop here waits for ever on diskfs or on other
//! programs: read and write give up after 8 rounds without progress, a
//! grant's scan after `MAX_GRANT_CALLS` calls (EIO), a truncation waiting
//! for pinned pages after `TRUNCATE_WAIT` (EBUSY), and every request to
//! diskfs after `fsclient`'s `REQUEST_TIMEOUT` (EIO; the channel is given
//! up).
//!
//! **diskfs restarts.** A new channel is made (`client`); every request in
//! flight on the old one failed (a fill: EIO for its waiters, SIGBUS for a
//! mapping; a write-back: its pages are dirty again and written on the new
//! channel). Before anyone uses the new channel, the inodes in use are
//! named again (STAT) to hold them in the new diskfs; an unlinked one,
//! which diskfs freed when the channel went, and one that is gone or whose
//! number now has another type or ext2 generation (another file) are
//! stale: every call on them fails with EIO, page requests of
//! their objects included (`keys` keeps every live inode). A lookup that
//! finds a stale inode's number again, or one of another generation,
//! gets a new `DInode`. Writes that
//! completed but were not flushed before diskfs died may be lost: the next
//! fsync of their file, or the next sync, reports EIO once.

use crate::fsclient::{self, Client, Next, PAGE};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use fsring::{Buf, Completion, Kind, Request};
use restricted::*;

pub const ENOENT: i64 = 2;
pub const EIO: i64 = 5;
pub const EAGAIN: i64 = 11;
pub const ENOMEM: i64 = 12;
pub const EBUSY: i64 = 16;
const EINTR: i64 = 4;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const EFBIG: i64 = 27;
pub const ENOSPC: i64 = 28;
pub const ENAMETOOLONG: i64 = 36;

/// st_dev of /data's files (the disk's major and minor, 254:0).
pub const DEV: u64 = 0xfe00;
/// ext2's root directory.
const ROOT_INO: u32 = 2;
/// Unused inodes kept (beyond them the least recently used go).
const MAX_CACHED: usize = 512;
/// Pages a fill reads at least, and at most; pages of one `READ` (the
/// kernel's longest run, `fsring::MAX_TRANSFER`).
const READAHEAD: u64 = 16;
const MAX_WINDOW: u64 = 1024;
const MAX_RUN: u64 = 256;
/// Pages one fill or write-back keeps granted (pinned) at once.
const PINNED: u64 = 4096;
/// Rounds of a fill that writes back for memory (`fill`), calls of one
/// grant's scan (`grant_run`), and how long a truncation waits for pinned
/// pages (`cache_truncate`).
const FILL_ROUNDS: u32 = 4;
const MAX_GRANT_CALLS: u32 = 1 << 16;
const TRUNCATE_WAIT: u64 = 10_000_000_000;
/// How long a file stays dirty before the pager writes it back.
const WRITEBACK_AGE: u64 = 5_000_000_000;

const EXT2_MAGIC: u64 = 0xef53;

/// A disk inode the server holds (see the module comment).
pub struct DInode {
    pub ino: u32,
    /// The file's type (`S_IFMT` bits), which never changes.
    pub kind: u32,
    /// Its i_generation: another file of the number has another one.
    generation: u32,
    /// The cached object's key (`EVENT_PAGE`, `EVENT_DIRTY`).
    key: u64,
    /// The cached object, once there is one (regular files); held only to read or set it.
    object: Mutex<Option<u64>>,
    /// Making the cached object (diskfs asked for the size first), one maker at a time.
    making: crate::sync::SleepLock,
    /// Write accesses (> 0) or programs running from it (< 0), as tmpfs.
    writers: Mutex<i64>,
    /// Serializes write-back and truncation (held across diskfs's requests: a sleeping
    /// lock, `sync::SleepLock`).
    wb: crate::sync::SleepLock,
    /// Taken by O_APPEND writes (finding the end and writing there is one
    /// step for every appender).
    pub append: crate::sync::SleepLock,
    /// Its last link went: released as soon as nothing uses it.
    unlinked: AtomicBool,
    /// Gone with a diskfs that died: every call fails (EIO).
    stale: AtomicBool,
    /// The connection on which a write of it completed since the last
    /// FLUSH (0: none).
    unflushed: AtomicU64,
    /// Read-ahead: the page a read in order comes to next, and the window.
    readahead: Mutex<(u64, u64)>,
    /// Fills in flight (truncation waits for them), and their futex word.
    fills: AtomicU32,
    filled: AtomicU32,
    /// When it was last used (a tick, for eviction).
    used: AtomicU64,
    /// Times set here that diskfs may not have yet (see `Times`).
    times: Mutex<Times>,
    /// The directory and name it was last found by (lookup, create,
    /// rename): where its inotify events about it go besides its own
    /// watches. None once that name went.
    link: Mutex<Option<(u32, u32, String)>>,
    /// Open file descriptions of it: an unlinked file's last close is when
    /// it goes (inotify's IN_DELETE_SELF).
    pub opens: AtomicUsize,
}

impl DInode {
    /// The directory (its number and generation) and the name it was last
    /// found by.
    pub fn link(&self) -> Option<(u32, u32, String)> {
        self.link.lock().clone()
    }

    /// Its ext2 generation.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Whether its last link went.
    pub fn unlinked(&self) -> bool {
        self.unlinked.load(Ordering::SeqCst)
    }
}

/// The inode `ino` if the server has it now.
pub fn cached(ino: u32) -> Option<Arc<DInode>> {
    TABLE.lock().inodes.get(&ino).cloned()
}

/// Records that `inode` was found as `name` in `dir` (`Table::names`).
fn set_link(inode: &DInode, dir: &DInode, name: &str) {
    let mut t = TABLE.lock();
    let mut l = inode.link.lock();
    if let Some((d, _, n)) = l.take() {
        if t.names.get(&(d, n.clone())) == Some(&inode.ino) {
            t.names.remove(&(d, n));
        }
    }
    t.names.insert((dir.ino, String::from(name)), inode.ino);
    *l = Some((dir.ino, dir.generation, String::from(name)));
}

/// The names of the table's inodes after a name changed: the inode found
/// by `from` is now `to`'s (None: it has no name we know any more); one
/// found by `to` before lost it. The inode that had `from`, if cached.
/// (By the index of names: a removal costs no walk over the cache.)
fn relink(from: (u32, &str), to: Option<(&DInode, &str)>) -> Option<u32> {
    let mut t = TABLE.lock();
    if let Some((dir, name)) = to {
        if let Some(old) = t.names.remove(&(dir.ino, String::from(name))) {
            if let Some(i) = t.inodes.get(&old) {
                *i.link.lock() = None;
            }
        }
    }
    let moved = t.names.remove(&(from.0, String::from(from.1)))?;
    let inode = t.inodes.get(&moved).cloned();
    match (to, inode) {
        (Some((dir, name)), Some(i)) => {
            t.names.insert((dir.ino, String::from(name)), moved);
            *i.link.lock() = Some((dir.ino, dir.generation, String::from(name)));
        }
        (None, Some(i)) => *i.link.lock() = None,
        _ => {}
    }
    Some(moved)
}

/// A file's times as the server set them (seconds: ext2 keeps no more),
/// over diskfs's, while written data waits in the cache: a write sets the
/// modification and change times when it enters the cache, as on Linux,
/// but diskfs records its own time when the data reaches it; so after
/// each write-back these go to diskfs (`SETTIMES`, which follows the
/// `WRITE`s) and are forgotten, and until then stat reports them, and
/// what changes times meanwhile (utimensat, chmod, truncation) changes
/// them too. Without data waiting, diskfs's times are the file's
/// (utimensat sends its times at once).
#[derive(Clone, Copy, Default)]
struct Times {
    atime: Option<u32>,
    mtime: Option<u32>,
    ctime: Option<u32>,
    /// Data written since the last write-back (these times then wait for
    /// it).
    pending: bool,
    /// Counts changes: a write-back forgets only what it sent.
    seq: u64,
}

impl Times {
    fn request(&self, ino: u32) -> Request {
        Request::SetTimes { ino, atime: self.atime, mtime: self.mtime, ctime: self.ctime }
    }

    fn is_empty(&self) -> bool {
        self.atime.is_none() && self.mtime.is_none() && self.ctime.is_none()
    }
}

/// The wall-clock time in seconds (ext2's timestamps).
fn now_secs() -> u32 {
    ext2_time(crate::time::realtime().sec)
}

/// A time in seconds as ext2 keeps it (an inode of 128 bytes has no extra
/// time fields): a signed 32-bit number of seconds (1901 to 2038), stored
/// in its unsigned field as Linux does; beyond, clamped.
fn ext2_time(sec: i64) -> u32 {
    sec.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32
}

/// An ext2 time field read back: signed.
fn from_ext2(t: u32) -> i64 {
    t as i32 as i64
}

/// The contents of `inode` changed (a write, a truncation, a store through
/// a mapping): its modification and change times are now.
pub fn modified(inode: &DInode) {
    let now = now_secs();
    let mut t = inode.times.lock();
    t.mtime = Some(now);
    t.ctime = Some(now);
    t.pending = true;
    t.seq += 1;
}

/// What diskfs did to the times itself (a truncation, a change of
/// permissions: `mtime` too or only the change time), also over times
/// that wait for a write-back.
fn changed_on_disk(inode: &DInode, mtime: bool) {
    let mut t = inode.times.lock();
    if t.pending {
        let now = now_secs();
        if mtime {
            t.mtime = Some(now);
        }
        t.ctime = Some(now);
        t.seq += 1;
    }
}

/// utimensat: the times given (the change time now), sent to diskfs.
pub fn set_times(inode: &Arc<DInode>, atime: vfs::stat::SetTime, mtime: vfs::stat::SetTime) -> Result<(), i64> {
    live(inode)?;
    if atime == vfs::stat::SetTime::Omit && mtime == vfs::stat::SetTime::Omit {
        return Ok(());
    }
    let now = crate::time::realtime();
    let secs = |t: vfs::stat::Time| ext2_time(t.sec);
    let request = {
        let mut t = inode.times.lock();
        if let Some(a) = atime.resolve(now) {
            t.atime = Some(secs(a));
        }
        if let Some(m) = mtime.resolve(now) {
            t.mtime = Some(secs(m));
        }
        t.ctime = Some(secs(now));
        t.seq += 1;
        (t.request(inode.ino), t.seq)
    };
    let c = client()?;
    status(&c.call(request.0.encode(0))?)?;
    // Nothing waits for a write-back: diskfs has them now.
    let mut t = inode.times.lock();
    if !t.pending && t.seq == request.1 {
        *t = Times { seq: t.seq, ..Times::default() };
    }
    Ok(())
}

/// After a write-back: the times set here go to diskfs (after the data,
/// whose `WRITE`s made diskfs record its own), and are forgotten unless
/// they changed meanwhile or the write-back covered only part of the file
/// (`whole`: all of it).
fn send_times(c: &Client, inode: &DInode, whole: bool) -> Result<(), i64> {
    let t = *inode.times.lock();
    if t.is_empty() {
        return Ok(());
    }
    status(&c.call(t.request(inode.ino).encode(0))?)?;
    let mut now = inode.times.lock();
    if whole && now.seq == t.seq {
        *now = Times { seq: now.seq, ..Times::default() };
    }
    Ok(())
}

impl Drop for DInode {
    fn drop(&mut self) {
        if let Some(h) = *self.object.lock() {
            syscall(SYS_HANDLE_CLOSE, [h, 0, 0, 0, 0, 0]);
        }
    }
}

struct Table {
    inodes: BTreeMap<u32, Arc<DInode>>,
    /// Every live inode by its object's key, also one replaced in
    /// `inodes` (stale) that users still hold: its object's page requests
    /// must be answered (failed) too.
    keys: BTreeMap<u64, alloc::sync::Weak<DInode>>,
    /// Inodes to look at for release (unlinked ones a user let go of), by number and object
    /// key: the key names the very inode (a number may be another file's by the time it is
    /// looked at: evicted, released and given to a new file meanwhile).
    check: Vec<(u32, u64)>,
    /// Inodes whose last link went and that were not cached here then (`orphan`): released
    /// in diskfs by the next `reap` under `NAMES` alone, unless a lookup that was under way
    /// meanwhile cached one again (`found`: it is cached unlinked then, and goes as such).
    /// Each entry is taken by exactly one of them: no inode is released twice. By number and
    /// the generation of the channel to diskfs it was orphaned on: diskfs holds an orphan's
    /// number for the server until its release (it is no other file's meanwhile), but only
    /// that diskfs; one that died took its holds along, and an orphan of an older channel is
    /// neither released to its successor nor matched by a lookup on it (`revalidate` drops
    /// them).
    orphans: Vec<(u32, u64)>,
    /// Dirty files (`EVENT_DIRTY`), by inode: its object's key (the very inode) and since when.
    dirty: BTreeMap<u32, (u64, u64)>,
    /// The inodes' names (directory, name) as last found (`DInode::link`),
    /// indexed: renames and removals find what they move or drop by name.
    names: BTreeMap<(u32, String), u32>,
}

static TABLE: Mutex<Table> =
    Mutex::new(Table { inodes: BTreeMap::new(), keys: BTreeMap::new(), check: Vec::new(), orphans: Vec::new(), dirty: BTreeMap::new(), names: BTreeMap::new() });
/// Name operations (shared) against a reconnection's revalidation and an eviction or an
/// orphan's release (alone); held across diskfs's requests: a sleeping lock.
static NAMES: crate::sync::SleepRwLock = crate::sync::SleepRwLock::new();
static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);
/// One reconnection at a time (held across the channel's offer and the revalidation: a
/// sleeping lock).
static RECONNECT: crate::sync::SleepLock = crate::sync::SleepLock::new(());
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The cached objects' keys count up from here (the test objects' are
/// below).
pub const KEY_BASE: u64 = 1 << 32;
static NEXT_KEY: AtomicU64 = AtomicU64::new(KEY_BASE);
static TICK: AtomicU64 = AtomicU64::new(0);
/// The filesystem's largest file and its block size (0 until known).
static MAX_FILE: AtomicU64 = AtomicU64::new(0);
static BLOCK: AtomicU64 = AtomicU64::new(0);
/// Whether `reap` has work (inodes to check, or too many cached).
static REAP: AtomicBool = AtomicBool::new(false);

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64
}

/// The channel to diskfs, made (again) when there is none or diskfs died.
pub fn client() -> Result<Arc<Client>, i64> {
    if let Some(c) = CLIENT.lock().clone().filter(|c| !c.is_dead()) {
        return Ok(c);
    }
    let _one = RECONNECT.lock()?;
    let old = CLIENT.lock().clone();
    if let Some(c) = old.as_ref().filter(|c| !c.is_dead()) {
        return Ok(c.clone());
    }
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let c = Arc::new(Client::connect(fsring::SERVICE, generation, fsclient::SCRATCH_PAGES).map_err(|_| EIO)?);
    // Before anyone uses the new channel (they wait for RECONNECT): no
    // request names an inode diskfs may have freed meanwhile, and the
    // dirty pages have their disk space promised again.
    if old.is_some() {
        // (A thread that dies meanwhile installs nothing: the next caller connects anew.)
        revalidate(&c)?;
        repromise(&c);
    }
    *CLIENT.lock() = Some(c.clone());
    Ok(c)
}

/// After diskfs came back: the inodes in use are held again or stale.
fn revalidate(c: &Client) -> Result<(), i64> {
    let _names = NAMES.write()?;
    // The old diskfs's holds went with it: its orphans are nobody's to release.
    TABLE.lock().orphans.retain(|&(_, g)| g == c.generation);
    let inodes: Vec<Arc<DInode>> = TABLE.lock().inodes.values().cloned().collect();
    for inode in inodes {
        if inode.unlinked.load(Ordering::Relaxed) {
            inode.stale.store(true, Ordering::Relaxed);
            continue;
        }
        let same = c.call(Request::Stat { ino: inode.ino }.encode(0)).ok().filter(|r| r.status == 0).map(|r| fsring::Stat::from_values(&r.values));
        // Gone, or its number given to another file meanwhile.
        if !same.is_some_and(|s| s.mode & vfs::S_IFMT == inode.kind && s.generation == inode.generation) {
            inode.stale.store(true, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// After diskfs came back: its promises went with the old channel. Every
/// cached file's pages lose their backing; the dirty ones are promised
/// again (a store to a clean page asks anew).
fn repromise(c: &Client) {
    let Ok(bs) = block_size_on(c) else { return };
    let inodes: Vec<Arc<DInode>> = TABLE.lock().inodes.values().cloned().collect();
    for inode in inodes.iter().filter(|i| !i.stale.load(Ordering::Relaxed)) {
        let Some(object) = *inode.object.lock() else { continue };
        let mut from = 0u64;
        let mut out = [0u64; 2];
        loop {
            // (`from` grows with every answer: the loop ends.)
            match syscall(SYS_MO_UNBACK, [object, from, out.as_mut_ptr() as u64, 0, 0, 0]) {
                1 if out[1] > out[0] && out[0] >= from => {
                    let size = syscall(SYS_MO_FILE_SIZE, [object, 0, 0, 0, 0, 0]).max(0) as u64;
                    let first = out[0] * PAGE;
                    let end = (out[1] * PAGE).min(size.div_ceil(bs) * bs).max(first);
                    match promise_on(c, inode.ino, first, end) {
                        Ok(()) => {
                            syscall(SYS_MO_BACKED, [object, first, end, 1, 0, 0]);
                        }
                        Err(e) => log(&alloc::format!(
                            "/data: no room for the cached writes of inode {} after diskfs restarted (errno {}); they may fail",
                            inode.ino, e
                        )),
                    }
                    from = out[1];
                }
                r if r == -EAGAIN && out[0] > from => from = out[0],
                _ => break,
            }
        }
    }
}

/// Prints `text` on the kernel's console.
fn log(text: &str) {
    syscall(SYS_SERVER_LOG, [text.as_ptr() as u64, text.len() as u64, 0, 0, 0, 0]);
}

/// A status as a result (a value outside the errno range is EIO).
fn status(c: &Completion) -> Result<i64, i64> {
    match c.status {
        s if s >= 0 => Ok(s),
        s if s >= -4095 => Err(-s),
        _ => Err(EIO),
    }
}

fn live(inode: &DInode) -> Result<(), i64> {
    if inode.stale.load(Ordering::Relaxed) { Err(EIO) } else { Ok(()) }
}

/// The inode `ino` of type `mode` and generation `generation` that a
/// request returned (held now). A stale inode of that number, or one of
/// another generation, is another file now: it leaves the table (its users
/// keep it, failing), a new one takes its place.
fn found(c: &Client, ino: u64, mode: u64, generation: u64) -> Result<Arc<DInode>, i64> {
    let ino = u32::try_from(ino).ok().filter(|&i| i != 0).ok_or(EIO)?;
    let kind = mode as u32 & vfs::S_IFMT;
    let generation = generation as u32;
    let mut t = TABLE.lock();
    // A lookup under way while its name went (`orphan`): cached unlinked, no longer an orphan.
    let orphaned = match t.orphans.iter().position(|&o| o == (ino, c.generation)) {
        Some(at) => {
            t.orphans.swap_remove(at);
            true
        }
        None => false,
    };
    match t.inodes.get(&ino) {
        Some(i) if !i.stale.load(Ordering::Relaxed) && i.generation == generation => {
            i.used.store(TICK.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
            let i = i.clone();
            if orphaned {
                i.unlinked.store(true, Ordering::SeqCst);
                t.check.push((ino, i.key));
                REAP.store(true, Ordering::Relaxed);
            }
            return Ok(i);
        }
        Some(i) => {
            // (Its key stays: its users may still fault on its object.)
            i.stale.store(true, Ordering::Relaxed);
            t.dirty.remove(&ino);
            // An armed test failure was the replaced inode's.
            let _ = FAIL_MKWRITE.compare_exchange(ino as u64, 0, Ordering::Relaxed, Ordering::Relaxed);
        }
        None => {}
    }
    let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    let inode = Arc::new(DInode {
        ino,
        kind,
        generation,
        key,
        object: Mutex::new(None),
        making: crate::sync::SleepLock::new(()),
        writers: Mutex::new(0),
        wb: crate::sync::SleepLock::new(()),
        append: crate::sync::SleepLock::new(()),
        unlinked: AtomicBool::new(orphaned),
        stale: AtomicBool::new(false),
        unflushed: AtomicU64::new(0),
        readahead: Mutex::new((u64::MAX, 0)),
        fills: AtomicU32::new(0),
        filled: AtomicU32::new(0),
        used: AtomicU64::new(TICK.fetch_add(1, Ordering::Relaxed)),
        times: Mutex::new(Times::default()),
        link: Mutex::new(None),
        opens: AtomicUsize::new(0),
    });
    t.inodes.insert(ino, inode.clone());
    if orphaned {
        t.check.push((ino, inode.key));
        REAP.store(true, Ordering::Relaxed);
    }
    if t.keys.len() > 2 * t.inodes.len() + 64 {
        t.keys.retain(|_, w| w.strong_count() > 0);
    }
    t.keys.insert(key, Arc::downgrade(&inode));
    if t.inodes.len() > MAX_CACHED {
        REAP.store(true, Ordering::Relaxed);
    }
    Ok(inode)
}

/// The root of /data.
pub fn root() -> Result<Arc<DInode>, i64> {
    if let Some(r) = TABLE.lock().inodes.get(&ROOT_INO) {
        return Ok(r.clone());
    }
    let c = client()?;
    let _names = NAMES.read()?;
    let r = c.call(Request::Stat { ino: ROOT_INO }.encode(0))?;
    status(&r)?;
    let s = fsring::Stat::from_values(&r.values);
    found(&c, ROOT_INO as u64, s.mode as u64, s.generation as u64)
}

/// The inode by its object's key.
fn by_key(key: u64) -> Option<Arc<DInode>> {
    let t = TABLE.lock();
    t.keys.get(&key).and_then(alloc::sync::Weak::upgrade)
}

// ------------------------------------------------------------- names

fn name_ok(name: &str) -> Result<(), i64> {
    if name.len() > vfs::NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    Ok(())
}

pub fn lookup(dir: &Arc<DInode>, name: &str) -> Result<Arc<DInode>, i64> {
    live(dir)?;
    name_ok(name)?;
    let c = client()?;
    let scratch = c.scratch(1);
    scratch.put(0, name.as_bytes());
    let _names = NAMES.read()?;
    let r = c.call(Request::Lookup { dir: dir.ino, name: scratch.buf(0, name.len() as u64) }.encode(0))?;
    status(&r)?;
    let inode = found(&c, r.values[0], r.values[1], r.values[2])?;
    set_link(&inode, dir, name);
    Ok(inode)
}

/// What `create` makes.
pub enum New<'a> {
    File,
    Dir,
    Symlink(&'a str),
    /// A socket's name (bind(2)).
    Socket,
}

pub fn create(dir: &Arc<DInode>, name: &str, new: New, perm: u32) -> Result<Arc<DInode>, i64> {
    live(dir)?;
    name_ok(name)?;
    let c = client()?;
    let scratch = c.scratch(2);
    scratch.put(0, name.as_bytes());
    let n = name.len() as u64;
    let kind = match new {
        New::File => Kind::File,
        New::Dir => Kind::Dir,
        New::Socket => Kind::Socket,
        New::Symlink(target) => {
            if target.len() > fsring::TARGET_MAX as usize {
                return Err(ENAMETOOLONG);
            }
            scratch.put(n, target.as_bytes());
            Kind::Symlink(scratch.buf(n, target.len() as u64))
        }
    };
    let _names = NAMES.read()?;
    let r = c.call(Request::Create { dir: dir.ino, name: scratch.buf(0, n), kind, perm: perm & 0o7777 }.encode(0))?;
    status(&r)?;
    let inode = found(&c, r.values[0], r.values[1], r.values[2])?;
    set_link(&inode, dir, name);
    Ok(inode)
}

/// The inode whose last link went, if any.
pub fn unlink(dir: &Arc<DInode>, name: &str, dir_only: bool) -> Result<Option<u32>, i64> {
    live(dir)?;
    name_ok(name)?;
    let c = client()?;
    let scratch = c.scratch(1);
    scratch.put(0, name.as_bytes());
    let gone = {
        let _names = NAMES.read()?;
        let r = c.call(Request::Unlink { dir: dir.ino, name: scratch.buf(0, name.len() as u64), is_dir: dir_only }.encode(0))?;
        status(&r)?;
        // In the same step as the unlink (under `NAMES`): no gap in which anything else could
        // release or cache it.
        orphan(&c, r.values[0]);
        r.values[0]
    };
    drop(scratch);
    relink((dir.ino, name), None);
    release_orphans();
    Ok(u32::try_from(gone).ok().filter(|&g| g != 0))
}

/// The inode that moved (if cached here) and the one whose last link the
/// rename took, if any.
pub fn rename(odir: &Arc<DInode>, oname: &str, ndir: &Arc<DInode>, nname: &str) -> Result<(Option<u32>, Option<u32>), i64> {
    live(odir)?;
    live(ndir)?;
    name_ok(oname)?;
    name_ok(nname)?;
    let c = client()?;
    let scratch = c.scratch(1);
    scratch.put(0, oname.as_bytes());
    scratch.put(oname.len() as u64, nname.as_bytes());
    let gone = {
        let _names = NAMES.read()?;
        let (old, new) = (scratch.buf(0, oname.len() as u64), scratch.buf(oname.len() as u64, nname.len() as u64));
        let r = c.call(Request::Rename { from: odir.ino, name: old, to: ndir.ino, new_name: new }.encode(0))?;
        status(&r)?;
        orphan(&c, r.values[0]);
        r.values[0]
    };
    drop(scratch);
    let moved = relink((odir.ino, oname), Some((ndir, nname)));
    release_orphans();
    Ok((moved, u32::try_from(gone).ok().filter(|&g| g != 0)))
}

/// An unlink or rename took the last link of `ino` (0: none), its caller still holding
/// `NAMES`: the server holds the inode now; it goes once nothing uses it. Cached here: marked
/// unlinked (it goes when its users let go, `evict`). Else: an orphan for the next `reap`
/// (whose release needs `NAMES` alone: a lookup under way may still return it).
fn orphan(c: &Client, ino: u64) {
    let Ok(ino) = u32::try_from(ino) else { return };
    if ino == 0 {
        return;
    }
    let mut t = TABLE.lock();
    match t.inodes.get(&ino) {
        // (A stale entry is a former file of the number: as good as not cached.)
        Some(i) if !i.stale.load(Ordering::Relaxed) => {
            i.unlinked.store(true, Ordering::SeqCst);
            let key = i.key;
            t.check.push((ino, key));
            REAP.store(true, Ordering::Relaxed);
        }
        // (Released by the unlinker right after its share of `NAMES`: `release_orphans`.)
        _ => t.orphans.push((ino, c.generation)),
    }
}

/// The orphans not cached again meanwhile are released in diskfs (by the unlinker right after
/// its share of `NAMES`, and by `reap`; with `NAMES` alone, so no lookup returns one
/// meanwhile). A thread that cannot (it dies, or diskfs is away) leaves them to a later
/// `reap`.
fn release_orphans() {
    if TABLE.lock().orphans.is_empty() {
        return;
    }
    let (Ok(c), Ok(_names)) = (client(), NAMES.write()) else {
        REAP.store(true, Ordering::Relaxed);
        return;
    };
    let orphans = core::mem::take(&mut TABLE.lock().orphans);
    // (An older channel's: its diskfs, and its holds, are gone.)
    for (ino, _) in orphans.into_iter().filter(|&(_, g)| g == c.generation) {
        let _ = c.call(Request::Release { ino }.encode(0));
    }
}

pub fn readlink(inode: &Arc<DInode>) -> Result<String, i64> {
    live(inode)?;
    if inode.kind != vfs::S_IFLNK {
        return Err(EINVAL);
    }
    let c = client()?;
    let scratch = c.scratch(1);
    let r = c.call(Request::Readlink { ino: inode.ino, buf: scratch.buf(0, PAGE) }.encode(0))?;
    let n = status(&r)? as u64;
    if n == 0 || n > PAGE {
        return Err(EIO);
    }
    String::from_utf8(scratch.get(0, n)).map_err(|_| EIO)
}

pub fn chmod(inode: &Arc<DInode>, perm: u32) -> Result<(), i64> {
    live(inode)?;
    let c = client()?;
    status(&c.call(Request::SetPerm { ino: inode.ino, perm: perm & 0o7777 }.encode(0))?)?;
    changed_on_disk(inode, false);
    Ok(())
}

/// Its `struct stat`: diskfs's, with the size the cache has.
pub fn stat(inode: &Arc<DInode>) -> Result<[u8; 144], i64> {
    live(inode)?;
    let c = client()?;
    let r = c.call(Request::Stat { ino: inode.ino }.encode(0))?;
    status(&r)?;
    let s = fsring::Stat::from_values(&r.values);
    let size = match *inode.object.lock() {
        Some(h) => syscall(SYS_MO_FILE_SIZE, [h, 0, 0, 0, 0, 0]).max(0) as u64,
        None => s.size,
    };
    let mut st = [0u8; 144];
    st[0..8].copy_from_slice(&DEV.to_le_bytes());
    st[8..16].copy_from_slice(&(inode.ino as u64).to_le_bytes());
    st[16..24].copy_from_slice(&(s.links as u64).to_le_bytes());
    st[24..28].copy_from_slice(&(inode.kind | s.mode & 0o7777).to_le_bytes());
    st[48..56].copy_from_slice(&size.to_le_bytes());
    st[56..64].copy_from_slice(&4096u64.to_le_bytes());
    st[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes());
    let t = *inode.times.lock();
    st[72..80].copy_from_slice(&from_ext2(t.atime.unwrap_or(s.atime)).to_le_bytes());
    st[88..96].copy_from_slice(&from_ext2(t.mtime.unwrap_or(s.mtime)).to_le_bytes());
    st[104..112].copy_from_slice(&from_ext2(t.ctime.unwrap_or(s.ctime)).to_le_bytes());
    Ok(st)
}

/// The filesystem's usage, and its largest file.
pub(crate) fn usage() -> Result<fsring::Usage, i64> {
    let c = client()?;
    let r = c.call(Request::Statfs.encode(0))?;
    status(&r)?;
    let u = fsring::Usage::from_values(&r.values);
    if u.block_size == 0 || u.max_file_size == 0 {
        return Err(EIO);
    }
    MAX_FILE.store(u.max_file_size, Ordering::Relaxed);
    BLOCK.store(u.block_size as u64, Ordering::Relaxed);
    Ok(u)
}

/// The filesystem's block size, asked on `c` if not known yet.
fn block_size_on(c: &Client) -> Result<u64, i64> {
    if let b @ 1.. = BLOCK.load(Ordering::Relaxed) {
        return Ok(b);
    }
    let r = c.call(Request::Statfs.encode(0))?;
    status(&r)?;
    match fsring::Usage::from_values(&r.values).block_size as u64 {
        0 => Err(EIO),
        b => {
            BLOCK.store(b, Ordering::Relaxed);
            Ok(b)
        }
    }
}

/// Promises diskfs the blocks a later write-back of bytes `first..end`
/// of `ino` needs (`fsring` "Promises"), one transfer's worth per request.
fn promise_on(c: &Client, ino: u32, first: u64, end: u64) -> Result<(), i64> {
    let mut at = first;
    while at < end {
        let len = (end - at).min(fsring::MAX_TRANSFER as u64);
        status(&c.call(Request::Promise { ino, offset: at, len }.encode(0))?)?;
        at += len;
    }
    Ok(())
}

/// Secures the disk space for a write of bytes `at..to` of the cached file
/// (delayed allocation's reservation): its pages from the one `at` lies in,
/// up to the page `to` lies in but no further than the file's end (as the
/// write leaves it) rounded up to a block. Where the pages are backed now
/// (`MO_BACKED`'s `upto`).
fn secure(inode: &DInode, object: u64, at: u64, to: u64) -> Result<u64, i64> {
    let c = client()?;
    let bs = block_size_on(&c)?;
    let size = syscall(SYS_MO_FILE_SIZE, [object, 0, 0, 0, 0, 0]).max(0) as u64;
    let first = at & !(PAGE - 1);
    let end = to.div_ceil(PAGE).saturating_mul(PAGE).min(to.max(size).div_ceil(bs).saturating_mul(bs)).max(at);
    promise_on(&c, inode.ino, first, end)?;
    Ok(end)
}

/// `EVENT_MKWRITE`: a store through a mapping wants page `offset` of the
/// object `key`: its space is promised up to the file's end, or the store
/// fails (SIGBUS, as on Linux when `page_mkwrite` finds no room).
pub fn mkwrite(key: u64, offset: u64) {
    // An unknown key: its inode went, and with it the object's handle (the
    // inode's own; every hold of the object holds the inode). There is no
    // handle to answer with, and none is needed: at the object's last
    // handle the kernel ended its waits (`PageCache::orphan`, EIO).
    let Some(inode) = by_key(key) else { return };
    // A key names an object only once `object` made it (under this lock,
    // which keeps it set from then on).
    let Some(object) = *inode.object.lock() else {
        debug_assert!(false, "a backing asked for an inode without its object");
        return;
    };
    if FAIL_MKWRITE.compare_exchange(inode.ino as u64, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        syscall(SYS_MO_BACKED, [object, offset, offset, 0, 0, 0]);
        return;
    }
    let size = syscall(SYS_MO_FILE_SIZE, [object, 0, 0, 0, 0, 0]).max(0) as u64;
    let to = (offset + PAGE).min(size).max(offset);
    let (end, ok) = match live(&inode).and_then(|_| secure(&inode, object, offset, to)) {
        Ok(end) => (end, true),
        Err(_) => (offset, false),
    };
    if ok {
        modified(&inode);
    }
    syscall(SYS_MO_BACKED, [object, offset, end, ok as u64, 0, 0]);
}

fn max_file() -> Result<u64, i64> {
    match MAX_FILE.load(Ordering::Relaxed) {
        0 => usage().map(|u| u.max_file_size),
        m => Ok(m),
    }
}

/// statfs(2)'s `struct statfs` for /data.
pub fn statfs() -> Result<[u8; 120], i64> {
    let u = usage()?;
    let bs = u.block_size as u64;
    let words = [EXT2_MAGIC, bs, u.blocks as u64, u.free_blocks as u64, u.free_blocks as u64, u.inodes as u64, u.free_inodes as u64, 0, vfs::NAME_MAX as u64, bs, 0, 0, 0, 0, 0];
    let mut out = [0u8; 120];
    for (i, w) in words.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    Ok(out)
}

/// Directory entries from `cursor` (an index): (inode, dirent type, name)
/// and the cursor after them (0: the end).
pub fn readdir(dir: &Arc<DInode>, cursor: u64) -> Result<(Vec<(u32, u8, Vec<u8>)>, u64), i64> {
    live(dir)?;
    let c = client()?;
    let scratch = c.scratch(1);
    let r = c.call(Request::Readdir { dir: dir.ino, cursor, buf: scratch.buf(0, PAGE) }.encode(0))?;
    let n = status(&r)? as u64;
    if n > PAGE {
        return Err(EIO);
    }
    let bytes = scratch.get(0, n);
    let entries: Vec<(u32, u8, Vec<u8>)> = fsring::dirents(&bytes)
        .map(|(ino, kind, name)| {
            let dtype = match kind {
                fsring::TYPE_DIR => 4,
                fsring::TYPE_FILE => 8,
                fsring::TYPE_SYMLINK => 10,
                fsring::TYPE_SOCKET => 12,
                _ => 0,
            };
            (ino, dtype, Vec::from(name))
        })
        .collect();
    let next = r.values[0];
    // A cursor that does not move on would list the same entries forever.
    if next != 0 && next <= cursor {
        return Err(EIO);
    }
    Ok((entries, next))
}

// ------------------------------------------------------------ contents

/// The file's cached object (made on first use; regular files only).
pub fn object(inode: &Arc<DInode>) -> Result<u64, i64> {
    live(inode)?;
    if let Some(h) = *inode.object.lock() {
        return Ok(h);
    }
    match inode.kind {
        vfs::S_IFREG => {}
        vfs::S_IFDIR => return Err(EISDIR),
        _ => return Err(EINVAL),
    }
    // One maker; `object` itself is never held across diskfs's requests (a reconnection
    // takes it for every inode while it holds `RECONNECT`).
    let _making = inode.making.lock()?;
    if let Some(h) = *inode.object.lock() {
        return Ok(h);
    }
    let limit = max_file()?;
    let c = client()?;
    let r = c.call(Request::Stat { ino: inode.ino }.encode(0))?;
    status(&r)?;
    let size = fsring::Stat::from_values(&r.values).size.min(limit);
    let h = syscall(SYS_MO_CREATE_CACHED, [size, inode.key, limit, 0, 0, 0]);
    if h < 0 {
        return Err(-h);
    }
    *inode.object.lock() = Some(h as u64);
    Ok(h as u64)
}

pub fn size(inode: &Arc<DInode>) -> Result<u64, i64> {
    let h = object(inode)?;
    let s = syscall(SYS_MO_FILE_SIZE, [h, 0, 0, 0, 0, 0]);
    if s < 0 { Err(-s) } else { Ok(s as u64) }
}

/// The read-ahead window for a fill at `index` that needs `want` pages.
fn window(inode: &DInode, index: u64, want: u64) -> u64 {
    let mut ra = inode.readahead.lock();
    let (next, last) = *ra;
    let w = if index == next { (last * 2).clamp(READAHEAD, MAX_WINDOW) } else { READAHEAD };
    let w = w.max(want).min(MAX_WINDOW);
    *ra = (index + w, w);
    w
}

/// Grants the first run of missing (`GRANT_FILL`) or dirty (`GRANT_DIRTY`)
/// pages in `at..end` (the run at `out`): its grant, ENOENT if there is
/// none. The kernel looks at a bounded number of pages per call and says
/// where to go on (EAGAIN).
fn grant_run(c: &Client, object: u64, mut at: u64, end: u64, flags: u64, out: &mut [u64; 3]) -> Result<u64, i64> {
    // Each call moves on (the kernel's answer is checked); a file's pages
    // are fewer than this many calls look at.
    for _ in 0..MAX_GRANT_CALLS {
        if at >= end {
            return Err(ENOENT);
        }
        match syscall(SYS_GRANT, [c.handle(), object, at * PAGE, end - at, flags, out.as_mut_ptr() as u64]) {
            g if g > 0 => return Ok(g as u64),
            g if g == -EAGAIN && out[0] > at => at = out[0],
            g if g < 0 && g != -EAGAIN => return Err(-g),
            _ => return Err(EIO),
        }
    }
    Err(EIO)
}

/// What a request of a fill or a write-back is about.
enum Io {
    Fill { grant: u32, first: u64, count: u64 },
    Write { grant: u32, first: u64, count: u64, len: u64 },
    Forget { grant: u32, pages: u64 },
}

/// Fills the missing pages of the window from page `index` (read-ahead
/// included): runs of them granted, read by DMA, declared filled. Whether
/// any page was missing; EIO if a read failed. Without memory for a page,
/// dirty pages are written back first (reclaim may drop them then); only
/// when none is left to write it fails (ENOMEM).
pub fn fill(inode: &Arc<DInode>, index: u64, want: u64) -> Result<bool, i64> {
    // A few rounds: programs that keep dirtying pages could otherwise keep
    // a fill writing back for ever.
    for _ in 0..FILL_ROUNDS {
        match fill_once(inode, index, want) {
            Err(ENOMEM) if write_back_dirty() > 0 => {}
            r => return r,
        }
    }
    Err(ENOMEM)
}

fn fill_once(inode: &Arc<DInode>, index: u64, want: u64) -> Result<bool, i64> {
    live(inode)?;
    let object = object(inode)?;
    let c = client()?;
    let end = index.saturating_add(window(inode, index, want));
    inode.fills.fetch_add(1, Ordering::AcqRel);
    let (cursor, pinned, any, failed) = (Cell::new(index), Cell::new(0u64), Cell::new(false), Cell::new(None));
    let mut out = [0u64; 3];
    fsclient::run(
        &c,
        || {
            if cursor.get() >= end || failed.get().is_some() {
                return Next::Done;
            }
            if pinned.get() >= PINNED {
                return Next::Later;
            }
            let at = cursor.get();
            let g = match grant_run(&c, object, at, end, GRANT_WRITE | GRANT_FILL, &mut out) {
                Ok(g) => g,
                Err(e) => {
                    if e != ENOENT && !any.get() {
                        failed.set(Some(e));
                    }
                    return Next::Done;
                }
            };
            let (first, count) = (out[0], out[1]);
            if first < at || count == 0 || count > MAX_RUN {
                // Never what the kernel answers; no loop if it did.
                failed.set(Some(EIO));
                return Next::Done;
            }
            cursor.set(first + count);
            pinned.set(pinned.get() + count);
            any.set(true);
            let read = Request::Read { ino: inode.ino, offset: first * PAGE, buf: Buf { grant: g as u32, offset: 0, len: (count * PAGE) as u32 } };
            Next::Request(read.encode(0), Io::Fill { grant: g as u32, first, count })
        },
        |io, r| match io {
            Io::Fill { grant, first, count } => {
                // A short read ends at diskfs's end of the file: the rest of
                // the pages stays zero (a hole, or not written back yet).
                let ok = r.status >= 0 && r.status as u64 <= count * PAGE;
                syscall(SYS_MO_FILLED, [object, first * PAGE, count, ok as u64, 0, 0]);
                if !ok {
                    failed.set(Some(EIO));
                }
                Some((Request::Forget { grant }.encode(0), Io::Forget { grant, pages: count }))
            }
            Io::Forget { grant, pages } => {
                syscall(SYS_REVOKE, [c.handle(), grant as u64, 0, 0, 0, 0]);
                pinned.set(pinned.get() - pages);
                None
            }
            Io::Write { .. } => None,
        },
    );
    inode.fills.fetch_sub(1, Ordering::AcqRel);
    inode.filled.fetch_add(1, Ordering::Release);
    syscall(SYS_SERVER_FUTEX_WAKE, [&inode.filled as *const AtomicU32 as u64, u32::MAX as u64, 0, 0, 0, 0]);
    match failed.get() {
        Some(e) => Err(e),
        None => Ok(any.get()),
    }
}

/// Reads up to `len` bytes at `off` into the program's memory at `buf`.
pub fn read(inode: &Arc<DInode>, off: u64, buf: u64, len: u64) -> Result<u64, i64> {
    let object = object(inode)?;
    let mut done = 0u64;
    let mut stuck = 0;
    while done < len {
        let r = syscall(SYS_MO_FILE_READ, [object, off + done, buf + done, len - done, MO_NOFILL, 0]);
        match r {
            0 => break,
            r if r > 0 => {
                done += r as u64;
                stuck = 0;
            }
            r if r == -EAGAIN => {
                let index = (off + done) / PAGE;
                let want = off.saturating_add(len).div_ceil(PAGE).saturating_sub(index);
                match fill(inode, index, want) {
                    // Missing again (reclaimed meanwhile) more than a few
                    // times: memory is too short to cache it.
                    Ok(_) if stuck < 8 => stuck += 1,
                    Ok(_) => return if done == 0 { Err(ENOMEM) } else { Ok(done) },
                    Err(e) if done == 0 => return Err(e),
                    Err(_) => break,
                }
            }
            e if done == 0 => return Err(-e),
            _ => break,
        }
    }
    Ok(done)
}

/// Writes `len` bytes of the program's memory at `buf` at `off` into the
/// cache (dirty pages: written back later).
pub fn write(inode: &Arc<DInode>, off: u64, buf: u64, len: u64) -> Result<u64, i64> {
    let object = object(inode)?;
    let max = max_file()?;
    if off >= max && len > 0 {
        return Err(EFBIG);
    }
    let len = len.min(max.saturating_sub(off));
    let mut done = 0u64;
    let mut stuck = 0;
    // Where this write's pages are backed (`secure`), so far.
    let mut upto = 0u64;
    while done < len {
        let at = off + done;
        let backing = if upto > at { MO_BACKED } else { MO_CHECK_BACKED };
        let r = syscall(SYS_MO_FILE_WRITE, [object, at, buf + done, len - done, MO_NOFILL | backing, upto]);
        match r {
            r if r > 0 => {
                done += r as u64;
                stuck = 0;
            }
            r if r == -ENOSPC && stuck < 8 => {
                // The pages need their disk space first (one transfer's
                // worth at a time): no room is ENOSPC now, not a failed
                // write-back later.
                stuck += 1;
                let to = (off + len).min((at & !(PAGE - 1)) + fsring::MAX_TRANSFER as u64);
                match secure(inode, object, at, to) {
                    Ok(end) => upto = end,
                    Err(e) => return if done == 0 { Err(e) } else { Ok(done) },
                }
            }
            r if r == -EAGAIN && stuck < 8 => {
                // The page needs its data first.
                stuck += 1;
                if let Err(e) = fill(inode, (off + done) / PAGE, 1) {
                    return if done == 0 { Err(e) } else { Ok(done) };
                }
            }
            r if r == -ENOMEM && stuck < 8 => {
                // No room for a new page: dirty ones go to the disk first
                // (this file's, then everyone's), then reclaim can drop them.
                stuck += 1;
                if stuck == 1 {
                    let _ = writeback(inode, 0..u64::MAX);
                } else {
                    write_back_dirty();
                }
            }
            0 => break,
            e if done == 0 => return Err(-e),
            _ => break,
        }
    }
    if done > 0 {
        modified(inode);
    }
    Ok(done)
}

/// Reads into the server's memory at `off` (sendfile): the missing pages
/// filled first, then copied by the kernel.
pub fn read_server(inode: &Arc<DInode>, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
    let object = object(inode)?;
    let pages = off.saturating_add(buf.len() as u64).div_ceil(PAGE) - off / PAGE;
    fill(inode, off / PAGE, pages)?;
    let n = syscall(SYS_MO_READ, [object, off, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0]);
    if n < 0 { Err(-n) } else { Ok(n as usize) }
}

/// Writes the server's memory at `off` (sendfile).
pub fn write_server(inode: &Arc<DInode>, off: u64, data: &[u8]) -> Result<usize, i64> {
    let object = object(inode)?;
    let max = max_file()?;
    if off.saturating_add(data.len() as u64) > max {
        return Err(EFBIG);
    }
    // The disk space first, as for write(2) (the pages stay unmarked: a
    // store there later asks again, and finds it promised).
    let end = off + data.len() as u64;
    let mut at = off;
    while at < end {
        let to = end.min((at & !(PAGE - 1)) + fsring::MAX_TRANSFER as u64);
        secure(inode, object, at, to)?;
        at = to;
    }
    let n = syscall(SYS_MO_WRITE, [object, off, data.as_ptr() as u64, data.len() as u64, 0, 0]);
    if n < 0 {
        return Err(-n);
    }
    if n > 0 {
        modified(inode);
    }
    Ok(n as usize)
}

/// Writes back the dirty pages of `inode` among `pages` (page indices):
/// runs of them granted, written by DMA, many in flight. How many pages it
/// wrote; the error of a write that failed (its pages are dirty again:
/// ENOSPC for a full disk, EIO).
pub fn writeback(inode: &Arc<DInode>, pages: core::ops::Range<u64>) -> Result<u64, i64> {
    let Some(object) = *inode.object.lock() else { return Ok(0) };
    if inode.stale.load(Ordering::Relaxed) {
        return Err(EIO);
    }
    let _wb = inode.wb.lock()?;
    let c = client()?;
    let (cursor, pinned, wrote, failed) = (Cell::new(pages.start), Cell::new(0u64), Cell::new(0u64), Cell::new(None));
    let mut out = [0u64; 3];
    fsclient::run(
        &c,
        || {
            let at = cursor.get();
            if at >= pages.end || failed.get().is_some() {
                return Next::Done;
            }
            if pinned.get() >= PINNED {
                return Next::Later;
            }
            let g = match grant_run(&c, object, at, pages.end, GRANT_DIRTY, &mut out) {
                Ok(g) => g,
                Err(e) => {
                    if e != ENOENT {
                        failed.set(Some(e));
                    }
                    return Next::Done;
                }
            };
            let (first, count, size) = (out[0], out[1], out[2]);
            if first < at || count == 0 || count > MAX_RUN {
                failed.set(Some(EIO));
                return Next::Done;
            }
            cursor.set(first + count);
            pinned.set(pinned.get() + count);
            // The last page only as far as the file went when the run was
            // taken (its data ends there; writes since make pages dirty
            // again).
            let len = (count * PAGE).min(size.saturating_sub(first * PAGE));
            if len == 0 {
                return Next::Request(Request::Forget { grant: g as u32 }.encode(0), Io::Forget { grant: g as u32, pages: count });
            }
            let write = Request::Write { ino: inode.ino, offset: first * PAGE, buf: Buf { grant: g as u32, offset: 0, len: len as u32 } };
            Next::Request(write.encode(0), Io::Write { grant: g as u32, first, count, len })
        },
        |io, r| match io {
            Io::Write { grant, first, count, len } => {
                if r.status == len as i64 {
                    wrote.set(wrote.get() + count);
                } else {
                    syscall(SYS_MO_REDIRTY, [object, first * PAGE, count, 0, 0, 0]);
                    // diskfs's reason (a full disk), or EIO.
                    failed.set(Some(status(&r).err().unwrap_or(EIO)));
                }
                Some((Request::Forget { grant }.encode(0), Io::Forget { grant, pages: count }))
            }
            Io::Forget { grant, pages } => {
                syscall(SYS_REVOKE, [c.handle(), grant as u64, 0, 0, 0, 0]);
                pinned.set(pinned.get() - pages);
                None
            }
            Io::Fill { .. } => None,
        },
    );
    if wrote.get() > 0 {
        inode.unflushed.store(c.generation, Ordering::Release);
    }
    if let Some(e) = failed.get() {
        return Err(e);
    }
    send_times(&c, inode, pages.start == 0 && pages.end == u64::MAX)?;
    Ok(wrote.get())
}

/// Makes everything written so far durable (`FLUSH`); EIO once if a write
/// of `inode` completed on a connection to a diskfs that died before.
fn flush(inode: Option<&Arc<DInode>>) -> Result<(), i64> {
    let c = client()?;
    status(&c.call(Request::Flush.encode(0))?)?;
    if let Some(inode) = inode {
        let gen = inode.unflushed.swap(0, Ordering::AcqRel);
        if gen != 0 && gen != c.generation {
            return Err(EIO);
        }
    }
    Ok(())
}

/// fsync, fdatasync: the file's dirty pages written back, then a flush.
pub fn fsync(inode: &Arc<DInode>) -> Result<(), i64> {
    live(inode)?;
    writeback(inode, 0..u64::MAX)?;
    flush(Some(inode))
}

/// fsync of a range (msync, O_DSYNC writes).
pub fn fsync_range(inode: &Arc<DInode>, from: u64, to: u64) -> Result<(), i64> {
    live(inode)?;
    writeback(inode, from / PAGE..to.div_ceil(PAGE))?;
    flush(Some(inode))
}

/// msync(MS_SYNC) of pages `first..end` of the object `key`.
pub fn msync(key: u64, first: u64, end: u64) -> Result<(), i64> {
    let Some(inode) = by_key(key) else { return Ok(()) };
    fsync_range(&inode, first.saturating_mul(PAGE), end.saturating_mul(PAGE))
}

/// sync(2): every file written back, then a flush.
pub fn sync_all() -> Result<(), i64> {
    let inodes: Vec<Arc<DInode>> = TABLE.lock().inodes.values().cloned().collect();
    let mut result = Ok(());
    for inode in &inodes {
        if !inode.stale.load(Ordering::Relaxed) {
            if let Err(e) = writeback(inode, 0..u64::MAX) {
                result = Err(e);
            }
        }
    }
    let client = CLIENT.lock().clone();
    if let Some(c) = client {
        flush(None)?;
        // Writes completed on a connection to a diskfs that died before
        // they were flushed may be lost: reported, as fsync does.
        for inode in &inodes {
            let gen = inode.unflushed.swap(0, Ordering::AcqRel);
            if gen != 0 && gen != c.generation {
                result = Err(EIO);
            }
        }
    }
    result
}

/// truncate(2), ftruncate: growing, diskfs first (it refuses what it
/// cannot hold, EFBIG), then the cache; shrinking, the cache first (its
/// pages beyond go, also from mappings), then diskfs.
pub fn truncate(inode: &Arc<DInode>, len: u64) -> Result<(), i64> {
    truncate_file(inode, len)?;
    changed_on_disk(inode, true);
    Ok(())
}

fn truncate_file(inode: &Arc<DInode>, len: u64) -> Result<(), i64> {
    live(inode)?;
    match inode.kind {
        vfs::S_IFREG => {}
        vfs::S_IFDIR => return Err(EISDIR),
        _ => return Err(EINVAL),
    }
    if len > max_file()? {
        return Err(EFBIG);
    }
    let _wb = inode.wb.lock()?;
    let c = client()?;
    // A file with nothing cached: diskfs's size is all (its object, made
    // later, starts from it).
    let Some(object) = *inode.object.lock() else {
        return status(&c.call(Request::Truncate { ino: inode.ino, len }.encode(0))?).map(|_| ());
    };
    // Growing: diskfs first (it refuses what it cannot hold), then the
    // cache. Shrinking: the cache first, which may have to wait for fills
    // in flight (they pin pages); diskfs only once the cache could.
    let size = syscall(SYS_MO_FILE_SIZE, [object, 0, 0, 0, 0, 0]).max(0) as u64;
    if len >= size {
        status(&c.call(Request::Truncate { ino: inode.ino, len }.encode(0))?)?;
        return cache_truncate(inode, object, len, TRUNCATE_WAIT);
    }
    cache_truncate(inode, object, len, TRUNCATE_WAIT)?;
    status(&c.call(Request::Truncate { ino: inode.ino, len }.encode(0))?).map(|_| ())
}

/// Truncates the cached object, waiting for the fills that pin its pages
/// (at most `wait` ns: pins that do not go, a grant diskfs never lets go
/// of, end it with EBUSY).
fn cache_truncate(inode: &DInode, object: u64, len: u64, wait: u64) -> Result<(), i64> {
    let deadline = now() + wait;
    loop {
        let seen = inode.filled.load(Ordering::Acquire);
        match syscall(SYS_MO_TRUNCATE, [object, len, 0, 0, 0, 0]) {
            r if r == -EBUSY && now() >= deadline => return Err(EBUSY),
            r if r == -EBUSY => {
                // The fills that pin pages end, or (pinned by a grant still draining) a little
                // later; a thread that dies meanwhile stops waiting (a wait that only death
                // ends early: a pending signal does not make it spin).
                let until = if inode.fills.load(Ordering::Acquire) > 0 { deadline } else { (now() + 1_000_000).min(deadline) };
                if syscall(SYS_SERVER_FUTEX_WAIT, [&inode.filled as *const AtomicU32 as u64, seen as u64, until, 0, 0, 0]) == -EINTR {
                    return Err(EINTR);
                }
            }
            r if r < 0 => return Err(-r),
            _ => return Ok(()),
        }
    }
}

/// An O_DIRECT read: the range written back, then read from the disk
/// (into the scratch buffer, copied to the program's `buf`).
pub fn read_direct(inode: &Arc<DInode>, off: u64, buf: u64, len: u64) -> Result<u64, i64> {
    live(inode)?;
    if inode.kind != vfs::S_IFREG {
        return Err(if inode.kind == vfs::S_IFDIR { EISDIR } else { EINVAL });
    }
    if inode.object.lock().is_some() {
        writeback(inode, off / PAGE..off.saturating_add(len).div_ceil(PAGE))?;
    }
    let c = client()?;
    let scratch = c.scratch(16);
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(scratch.len());
        let r = c.call(Request::Read { ino: inode.ino, offset: off + done, buf: scratch.buf(0, n) }.encode(0))?;
        let got = match status(&r) {
            Ok(got) if got as u64 <= n => got as u64,
            Ok(_) => return Err(EIO),
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        let data = scratch.get(0, got);
        if let Err(e) = usercopy::to_program(buf + done, &data) {
            return if done == 0 { Err(e) } else { Ok(done) };
        }
        done += got;
        if got < n {
            break;
        }
    }
    Ok(done)
}

// ----------------------------------------------------------- the pager

/// `EVENT_PAGE`: a thread waits for page `offset` of the object `key`
/// (a fault on a mapping, or the kernel reading it): filled, or failed.
pub fn page(key: u64, offset: u64) {
    let index = offset / PAGE;
    // Every object's inode is in `keys` while it lives (its handle is the
    // inode's, and whatever maps or runs the object holds the inode): an
    // unknown key has no handle left to answer with, and the kernel ended
    // the waits for it at its last handle (`PageCache::orphan`).
    let Some(inode) = by_key(key) else { return };
    if let Err(_) = fill(&inode, index, 1) {
        // Whoever waits gets EIO (a mapping SIGBUS).
        if let Some(h) = *inode.object.lock() {
            syscall(SYS_MO_FILLED, [h, index * PAGE, 1, 0, 0, 0]);
        }
    }
}

/// `EVENT_DIRTY`: the file `key` has dirty pages now.
pub fn dirtied(key: u64) {
    // (A stale inode's pages cannot be written: nothing to list.)
    let Some(inode) = by_key(key).filter(|i| !i.stale.load(Ordering::Relaxed)) else { return };
    let mut t = TABLE.lock();
    // (Listed for this very inode: one the number names no longer is not.)
    if t.inodes.get(&inode.ino).is_some_and(|i| Arc::ptr_eq(i, &inode)) {
        t.dirty.entry(inode.ino).or_insert_with(|| (inode.key, now()));
    }
}

/// When the pager must look at the dirty files next (0: none dirty).
pub fn next_deadline() -> u64 {
    TABLE.lock().dirty.values().map(|&(_, since)| since).min().map_or(0, |since| since + WRITEBACK_AGE)
}

/// The pager writes back the dirty files: those dirty for
/// `WRITEBACK_AGE` (all of them with `all`), oldest first. A file whose
/// pages are all written leaves the list (a store makes it dirty again:
/// `EVENT_DIRTY`, which the pager takes after this).
pub fn write_dirty(all: bool) {
    let due: Vec<(u32, u64, u64)> = {
        let t = TABLE.lock();
        let horizon = now().saturating_sub(WRITEBACK_AGE);
        let mut due: Vec<(u32, u64, u64)> =
            t.dirty.iter().filter(|(_, &(_, since))| all || since <= horizon).map(|(&i, &(k, s))| (i, k, s)).collect();
        due.sort_by_key(|&(_, _, since)| since);
        due
    };
    for (ino, key, _) in due {
        // The very inode listed (its number may be another's by now: then nothing to do).
        let inode = TABLE.lock().inodes.get(&ino).filter(|i| i.key == key).cloned();
        let clean = match inode {
            Some(inode) => clean_after_writeback(&inode),
            None => true,
        };
        let mut t = TABLE.lock();
        // Only the entry of that inode (not one another file of the number made since).
        match t.dirty.get_mut(&ino) {
            Some(e) if e.0 == key && clean => {
                t.dirty.remove(&ino);
            }
            // Failed or dirtied again meanwhile: again later.
            Some(e) if e.0 == key => e.1 = now(),
            _ => {}
        }
    }
}

/// Writes back every dirty file once, leaving the list to the pager (only
/// it takes files off, so none dirtied meanwhile is lost from it).
fn write_back_dirty() -> u64 {
    let dirty: Vec<(u32, u64)> = TABLE.lock().dirty.iter().map(|(&i, &(k, _))| (i, k)).collect();
    let mut wrote = 0;
    for (ino, key) in dirty {
        let inode = TABLE.lock().inodes.get(&ino).filter(|i| i.key == key).cloned();
        if let Some(inode) = inode.filter(|i| !i.stale.load(Ordering::Relaxed)) {
            wrote += writeback(&inode, 0..u64::MAX).unwrap_or(0);
        }
    }
    wrote
}

/// Writes `inode` back until a pass finds nothing dirty (a few passes at
/// most: a store during a pass may dirty a page it had passed): whether it
/// is clean.
fn clean_after_writeback(inode: &Arc<DInode>) -> bool {
    if inode.stale.load(Ordering::Relaxed) {
        return true;
    }
    for _ in 0..4 {
        match writeback(inode, 0..u64::MAX) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

// --------------------------------------------- holds and write access

/// What a hold of the object keeps (`SYS_MO_HOLD`): the inode, and with
/// it write access (a shared writable mapping) or the right to run it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HoldKind {
    Plain,
    Write,
    Run,
}

struct Hold {
    inode: Arc<DInode>,
    kind: HoldKind,
}

/// The tag of a data file's hold word (bit 0 is tmpfs's).
pub const HOLD_TAG: u64 = 2;

/// A handle on the object that keeps the inode (and the access `kind`)
/// until the kernel lets it go (`EVENT_RELEASE`): for mappings and
/// programs.
pub fn hold(inode: &Arc<DInode>, kind: HoldKind) -> Result<u64, i64> {
    let object = object(inode)?;
    match kind {
        HoldKind::Plain => {}
        HoldKind::Write => get_write(inode)?,
        HoldKind::Run => deny_write(inode)?,
    }
    let word = Box::into_raw(Box::new(Hold { inode: inode.clone(), kind })) as u64 | HOLD_TAG;
    let h = syscall(SYS_MO_HOLD, [object, word, 0, 0, 0, 0]);
    if h < 0 {
        // Never handed over.
        released(word);
        return Err(-h);
    }
    Ok(h as u64)
}

/// `EVENT_RELEASE` for a data file's hold word.
pub fn released(word: u64) {
    let hold = unsafe { Box::from_raw((word & !HOLD_TAG) as *mut Hold) };
    match hold.kind {
        HoldKind::Plain => {}
        HoldKind::Write => put_write(&hold.inode),
        HoldKind::Run => allow_write(&hold.inode),
    }
    let_go(&hold.inode);
}

/// The right to write the file (ETXTBSY while a program runs from it).
pub fn get_write(inode: &DInode) -> Result<(), i64> {
    crate::tmpfs::settled(|| {
        let mut w = inode.writers.lock();
        if *w < 0 {
            return Err(crate::tmpfs::ETXTBSY);
        }
        *w += 1;
        Ok(())
    })
}

pub fn put_write(inode: &DInode) {
    *inode.writers.lock() -= 1;
}

fn deny_write(inode: &DInode) -> Result<(), i64> {
    crate::tmpfs::settled(|| {
        let mut w = inode.writers.lock();
        if *w > 0 {
            return Err(crate::tmpfs::ETXTBSY);
        }
        *w -= 1;
        Ok(())
    })
}

fn allow_write(inode: &DInode) {
    *inode.writers.lock() += 1;
}

/// A user lets go of `inode` (an open file, a hold): an unlinked inode is
/// looked at for release (`reap`).
pub fn let_go(inode: &Arc<DInode>) {
    if inode.unlinked.load(Ordering::Relaxed) {
        TABLE.lock().check.push((inode.ino, inode.key));
        REAP.store(true, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------ eviction

/// Releases the inodes nothing uses that must go: unlinked ones, and the
/// least recently used beyond `MAX_CACHED` (down to three quarters of it).
/// Called where the calling thread holds no lock (the end of a system
/// call, the pager between events).
pub fn reap() {
    if !REAP.swap(false, Ordering::AcqRel) {
        return;
    }
    release_orphans();
    let victims: Vec<(u32, u64)> = {
        let mut t = TABLE.lock();
        let mut victims: Vec<(u32, u64)> = core::mem::take(&mut t.check);
        if t.inodes.len() > MAX_CACHED {
            let mut unused: Vec<(u64, u32, u64)> = t
                .inodes
                .values()
                .filter(|i| Arc::strong_count(i) == 1 && i.ino != ROOT_INO)
                .map(|i| (i.used.load(Ordering::Relaxed), i.ino, i.key))
                .collect();
            unused.sort_unstable();
            let excess = t.inodes.len() - MAX_CACHED * 3 / 4;
            victims.extend(unused.iter().take(excess).map(|&(_, ino, key)| (ino, key)));
        }
        victims
    };
    for (ino, key) in victims {
        evict(ino, key);
    }
}

/// Lets go of inode `ino` (the one with object key `key`: not another file the number
/// names by now) if nothing uses it: written back (unless unlinked: its data is of no use),
/// taken out of the table and released in diskfs.
fn evict(ino: u32, key: u64) {
    let Some(inode) = TABLE.lock().inodes.get(&ino).filter(|i| i.key == key && Arc::strong_count(i) == 1).cloned() else { return };
    let unlinked = inode.unlinked.load(Ordering::Relaxed);
    let stale = inode.stale.load(Ordering::Relaxed);
    // Looked at again by a later `reap` (a thread that dies meanwhile, a write-back that
    // failed): the inode stays cached until then.
    let again = || {
        TABLE.lock().check.push((ino, key));
        REAP.store(true, Ordering::Relaxed);
    };
    if !unlinked && !stale {
        match writeback(&inode, 0..u64::MAX) {
            Ok(_) => {}
            // (Its user died: the next reap tries again.)
            Err(EINTR) => return again(),
            // Kept until its data could be written (a later write-back or eviction).
            Err(_) => return,
        }
    }
    let c = if stale { None } else { client().ok().filter(|c| !c.is_dead()) };
    let ticket = {
        let Ok(_names) = NAMES.write() else { return again() };
        let mut t = TABLE.lock();
        // Used again meanwhile (else only the table's reference and ours
        // are left), or no longer the table's.
        if Arc::strong_count(&inode) != 2 || !t.inodes.get(&ino).is_some_and(|i| Arc::ptr_eq(i, &inode)) {
            return;
        }
        t.inodes.remove(&ino);
        // An armed test failure is this inode's, not a later file's that
        // gets its number.
        let _ = FAIL_MKWRITE.compare_exchange(ino as u64, 0, Ordering::Relaxed, Ordering::Relaxed);
        if let Some((d, _, n)) = inode.link.lock().take() {
            if t.names.get(&(d, n.clone())) == Some(&ino) {
                t.names.remove(&(d, n));
            }
        }

        if t.dirty.get(&ino).is_some_and(|e| e.0 == key) {
            t.dirty.remove(&ino);
        }
        drop(t);
        c.as_ref().and_then(|c| c.submit(Request::Release { ino }.encode(0), true).ok().flatten())
    };
    if let (Some(c), Some(t)) = (&c, ticket) {
        c.wait(t);
    }
    // The object goes with the inode (its last handle).
}

/// `EVENT_CLOSING` (the instance ends) and `EVENT_SYNC`: everything
/// written back and flushed. No program can be told of a failure any
/// more: the console is.
pub fn closing() {
    if let Err(e) = sync_all() {
        log(&alloc::format!("/data: writing the cache back failed (errno {}); data may be lost", e));
    }
}

// ----------------------------------------------------------- self-test

/// The inode whose next backing `mkwrite` fails (`TEST_MKWRITE_FAIL`; 0:
/// none).
static FAIL_MKWRITE: AtomicU64 = AtomicU64::new(0);

/// `TEST_MKWRITE_FAIL` (see `restricted::TEST_MKWRITE_FAIL`): arms the
/// failure for `ino`, or disarms it (0).
pub fn fail_next_mkwrite(ino: u64) -> i64 {
    if ino > u32::MAX as u64 {
        return -EINVAL;
    }
    FAIL_MKWRITE.store(ino, Ordering::Relaxed);
    0
}

/// `TEST_CACHED` (see `restricted::TEST_CACHED`).
pub fn test(scenario: u64) -> i64 {
    macro_rules! check {
        ($n:expr, $cond:expr) => {
            if !$cond {
                return -$n;
            }
        };
    }
    const EINVAL: i64 = 22;
    let Ok(root) = root() else { return -1 };
    let name = "lxtest.cached";
    let _ = unlink(&root, name, false);
    let Ok(inode) = create(&root, name, New::File, 0o644) else { return -2 };
    let Ok(object) = object(&inode) else { return -3 };
    let result = (|| {
        match scenario {
            1 => {
                // Two pages, then failures past the end and over 256 pages.
                let data = alloc::vec![7u8; 2 * PAGE as usize];
                check!(10, syscall(SYS_MO_WRITE, [object, 0, data.as_ptr() as u64, data.len() as u64, 0, 0]) == data.len() as i64);
                check!(11, syscall(SYS_MO_FILLED, [object, 0, 300, 0, 0, 0]) == -EINVAL);
                check!(12, syscall(SYS_MO_FILLED, [object, 100 * PAGE, 256, 0, 0, 0]) == 0);
                check!(13, syscall(SYS_MO_FILLED, [object, u64::MAX & !(PAGE - 1), 1, 0, 0, 0]) == 0);
                // Grown: the pages that "failed" beyond the old end are holes.
                check!(14, truncate(&inode, 200 * PAGE).is_ok());
                let mut back = alloc::vec![1u8; 64];
                check!(15, syscall(SYS_MO_READ, [object, 100 * PAGE, back.as_mut_ptr() as u64, 64, 0, 0]) == 64);
                check!(16, back.iter().all(|&b| b == 0));
                0
            }
            2 => {
                // 1200 clean pages, then the last one dirty.
                let data = alloc::vec![9u8; 64 * PAGE as usize];
                for i in 0..19 {
                    check!(20, syscall(SYS_MO_WRITE, [object, i * 64 * PAGE, data.as_ptr() as u64, data.len() as u64, 0, 0]) == data.len() as i64);
                }
                check!(21, fsync(&inode).is_ok());
                check!(22, syscall(SYS_MO_WRITE, [object, 1199 * PAGE, data.as_ptr() as u64, 10, 0, 0]) == 10);
                let Ok(c) = client() else { return -23 };
                let mut out = [0u64; 3];
                let first = syscall(SYS_GRANT, [c.handle(), object, 0, 1 << 20, GRANT_DIRTY, out.as_mut_ptr() as u64]);
                check!(24, first == -EAGAIN && out[0] > 0 && out[0] < 1199);
                let g = grant_run(&c, object, 0, 1 << 20, GRANT_DIRTY, &mut out);
                check!(25, g.is_ok() && out[0] == 1199 && out[1] == 1);
                if let Ok(g) = g {
                    syscall(SYS_REVOKE, [c.handle(), g, 0, 0, 0, 0]);
                    syscall(SYS_MO_REDIRTY, [object, 1199 * PAGE, 1, 0, 0, 0]);
                }
                check!(26, fsync(&inode).is_ok());
                0
            }
            3 => {
                // A page pinned by a grant that is never let go of: the
                // truncation gives up (EBUSY) instead of waiting for ever.
                let data = alloc::vec![5u8; PAGE as usize];
                check!(30, syscall(SYS_MO_WRITE, [object, 0, data.as_ptr() as u64, data.len() as u64, 0, 0]) == data.len() as i64);
                let Ok(c) = client() else { return -31 };
                let mut out = [0u64; 3];
                let Ok(g) = grant_run(&c, object, 0, 1, GRANT_DIRTY, &mut out) else { return -32 };
                let start = now();
                let r = cache_truncate(&inode, object, 0, 50_000_000);
                let waited = now() - start;
                syscall(SYS_REVOKE, [c.handle(), g, 0, 0, 0, 0]);
                syscall(SYS_MO_REDIRTY, [object, 0, 1, 0, 0, 0]);
                check!(33, r == Err(EBUSY));
                check!(34, (50_000_000..5_000_000_000).contains(&waited));
                // Let go of: it truncates.
                check!(35, truncate(&inode, 0).is_ok());
                0
            }
            4 => {
                // A sync across instances: a ticket, waited for (no other
                // instance runs in the tests: at once); only the service
                // thread answers one.
                let ticket = syscall(SYS_SYNC_OTHERS, [0; 6]);
                check!(40, ticket > 0);
                let start = now();
                check!(41, syscall(SYS_SYNC_OTHERS, [ticket as u64, 0, 0, 0, 0, 0]) == 0);
                check!(42, now() - start < 1_000_000_000);
                check!(43, syscall(SYS_SYNC_OTHERS, [0; 6]) > ticket);
                check!(44, syscall(SYS_SYNC_DONE, [ticket as u64, 0, 0, 0, 0, 0]) == -1);
                0
            }
            _ => -1000,
        }
    })();
    drop(inode);
    let _ = unlink(&root, name, false);
    result
}
