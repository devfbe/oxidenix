//! epoll (phase R6e): an interest list of open file descriptions and a
//! ready list, the event loop interface of libuv (and so of Node.js), as
//! Linux's fs/eventpoll.c has it.
//!
//! Each interest (`Item`) is subscribed to its description's watch
//! (`poll::Watch`). A report of the file (`files::ready`) queues the item
//! on its instance's ready list, without asking the file anything, and
//! wakes one thread waiting in epoll_wait (more wake as events stay
//! unconsumed: `pass_on`); it also reports the instance itself, for a poll
//! on it and for the instances that watch it (nesting). epoll_wait takes
//! items off the ready list and asks each file for its readiness now: a
//! level-triggered item that is still ready goes back to the end of the
//! list (so a full `maxevents` rotates through them), an edge-triggered one
//! waits for the next report, a one-shot one is disabled until
//! EPOLL_CTL_MOD. EPOLLEXCLUSIVE interests in one file wake their
//! instances one at a time (`poll::Watch::notify`).
//!
//! Interests belong to the description, not the descriptor: they are keyed
//! by (descriptor number, description), as Linux's, and go when the
//! description goes (its last reference: `poll::forget`), whatever
//! descriptors still name the number. Instances may watch each other, at
//! most `MAX_NESTING` deep and without cycles (ELOOP), so reports, which
//! go up through the instances, end.
//!
//! The ready list never allocates in a report: it holds each item at most
//! once and keeps room for all of them. No lock of an instance is held
//! while program memory is copied (the pager reports too): events are taken
//! off the list, copied, and the level-triggered ones put back after.
//!
//! Lock order: the interest list → a target's watch; a file's lock → its
//! watch → an instance's ready list → the instance's own watch → an outer
//! instance's ready list → ...

use crate::files::{self, Description, File, FileRef, O_CLOEXEC, O_RDWR};
use crate::poll::{self, Sub, Watch};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst};
use restricted::*;

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EEXIST: i64 = 17;
const EINVAL: i64 = 22;
const ESPIPE: i64 = 29;
const ENOTTY: i64 = 25;
const ELOOP: i64 = 40;
const ENOSPC: i64 = 28;

/// Most interests (items) in all epoll instances of the tree (Linux's
/// fs.epoll.max_user_watches, per user: everyone in a tree is root): beyond
/// it EPOLL_CTL_ADD is ENOSPC, so instances times descriptors cannot fill
/// the server's heap.
const MAX_WATCHES: usize = 65536;
static WATCHES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
const ETIMEDOUT: i64 = 110;

const EPOLLIN: u32 = 0x1;
const EPOLLOUT: u32 = 0x4;
const EPOLLERR: u32 = 0x8;
const EPOLLHUP: u32 = 0x10;
const EPOLLRDNORM: u32 = 0x40;
const EPOLLWRNORM: u32 = 0x100;
const EPOLLEXCLUSIVE: u32 = 1 << 28;
const EPOLLWAKEUP: u32 = 1 << 29;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
/// The input flags; the rest are events.
const FLAGS: u32 = EPOLLEXCLUSIVE | EPOLLWAKEUP | EPOLLONESHOT | EPOLLET;
/// What an EPOLLEXCLUSIVE interest may ask for (Linux's
/// EPOLLEXCLUSIVE_OK_BITS).
const EXCLUSIVE_OK: u32 = EPOLLIN | EPOLLOUT | EPOLLRDNORM | EPOLLWRNORM | EPOLLERR | EPOLLHUP | EPOLLWAKEUP | EPOLLET | EPOLLEXCLUSIVE;

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;

/// How deep instances may watch each other (Linux's EPOLL_MAX_NESTS).
const MAX_NESTING: usize = 4;
/// Size of a `struct epoll_event` (packed on x86-64).
const EVENT_SIZE: u64 = 12;

/// An event taken off the ready list for delivery: its events and data,
/// the item, and the item's flags before (a one-shot one is disabled).
struct Taken {
    events: u32,
    data: u64,
    item: Arc<Item>,
    flags: u32,
}

/// Serializes adding an instance to another, so that the loop check and
/// the insertion are one step.
static NESTING: Mutex<()> = Mutex::new(());

pub struct Epoll {
    /// Its description's id: what it reports as (`files::ready`).
    id: u64,
    /// Interests by descriptor number and description.
    interest: Mutex<BTreeMap<(i32, usize), Arc<Item>>>,
    ready: Mutex<Ready>,
    /// Advanced by every item queued; epoll_wait sleeps on it.
    seq: AtomicU32,
    /// Threads in epoll_wait on it.
    waiters: AtomicU32,
    /// Its description's watch (set once it has one): the instance reports
    /// its own readiness there directly, and only while something watches
    /// it (a poll on it, an instance that watches it).
    watch: Mutex<Weak<Watch>>,
}

struct Ready {
    list: VecDeque<Arc<Item>>,
    /// A report found the list full (it never should): the next
    /// epoll_wait checks every item.
    overflow: bool,
}

/// An interest of an instance in a description.
pub struct Item {
    /// Its key in the instance's interest list.
    key: (i32, usize),
    file: Weak<Description>,
    /// The description's watch, which the item is subscribed to.
    watch: Arc<Watch>,
    epoll: Weak<Epoll>,
    /// The requested events and flags; only flags once a one-shot fired.
    events: AtomicU32,
    data: AtomicU64,
    /// On the ready list.
    queued: AtomicBool,
    removed: AtomicBool,
}

impl Drop for Item {
    fn drop(&mut self) {
        // (Never below zero: a wrapped count would be no bound.)
        let _ = WATCHES.try_update(SeqCst, SeqCst, |c| Some(c.saturating_sub(1)));
    }
}

impl Item {
    pub fn exclusive(&self) -> bool {
        self.events.load(SeqCst) & EPOLLEXCLUSIVE != 0
    }

    /// A report of the file (now `ready`, 0: not said): queued, and the
    /// instance's waiters woken, if it asks for such events. Whether this
    /// woke a waiter of an exclusive interest (the end of an exclusive
    /// wakeup).
    pub fn event(self: &Arc<Self>, ready: i16) -> bool {
        if self.removed.load(SeqCst) {
            return false;
        }
        let wanted = self.events.load(SeqCst);
        if wanted & !FLAGS == 0 {
            return false;
        }
        if ready != 0 && ready as u16 as u32 & (wanted | EPOLLERR | EPOLLHUP) == 0 {
            return false;
        }
        let Some(ep) = self.epoll.upgrade() else { return false };
        ep.enqueue(self);
        let woke = ep.wake();
        // The instance itself became readable: for a poll on it and the
        // instances that watch it, if any.
        if let Some(own) = ep.own_watch().filter(|w| w.watched()) {
            own.notify(POLLIN_NOW);
        }
        wanted & EPOLLEXCLUSIVE != 0 && woke
    }

    /// The events among those requested that `file` has now.
    fn check(&self, file: &Description) -> u32 {
        let wanted = self.events.load(SeqCst);
        if wanted & !FLAGS == 0 {
            return 0;
        }
        file.poll_mask() as u16 as u32 & (wanted | EPOLLERR | EPOLLHUP) & !FLAGS
    }

    /// The description went (`poll::forget`, which took the item off its
    /// watch): the interest goes from its instance.
    pub fn file_gone(self: &Arc<Self>) {
        let Some(ep) = self.epoll.upgrade() else { return };
        let removed = {
            let mut interest = ep.interest.lock();
            // Its own entry only (a later interest may have the key now).
            match interest.get(&self.key) {
                Some(i) if Arc::ptr_eq(i, self) => interest.remove(&self.key),
                _ => None,
            }
        };
        if removed.is_some() {
            ep.unlist(self);
        }
    }
}

const POLLIN_NOW: i16 = EPOLLIN as i16;

impl Epoll {
    fn new(id: u64) -> Arc<Epoll> {
        Arc::new(Epoll {
            id,
            interest: Mutex::new(BTreeMap::new()),
            ready: Mutex::new(Ready { list: VecDeque::new(), overflow: false }),
            seq: AtomicU32::new(0),
            waiters: AtomicU32::new(0),
            watch: Mutex::new(Weak::new()),
        })
    }

    /// Its description's watch.
    fn own_watch(&self) -> Option<Arc<Watch>> {
        self.watch.lock().upgrade()
    }

    /// Puts the item on the ready list (once).
    fn enqueue(&self, item: &Arc<Item>) {
        if item.queued.swap(true, SeqCst) {
            return;
        }
        let mut ready = self.ready.lock();
        // Checked under the list's lock, which `unlist` sets it under: a
        // removed item never gets (back) on the list.
        if item.removed.load(SeqCst) {
            return;
        }
        if ready.list.len() < ready.list.capacity() {
            ready.list.push_back(item.clone());
        } else {
            item.queued.store(false, SeqCst);
            ready.overflow = true;
        }
    }

    /// Something was queued: one waiter wakes (whether there was one).
    fn wake(&self) -> bool {
        self.seq.fetch_add(1, SeqCst);
        if self.waiters.load(SeqCst) == 0 {
            return false;
        }
        syscall(SYS_SERVER_FUTEX_WAKE, [&self.seq as *const AtomicU32 as u64, 1, 0, 0, 0, 0]);
        true
    }

    /// A waiter leaves without taking what is listed (a signal, the
    /// deadline): another one gets the wake it may have taken.
    fn pass_on(&self) {
        let listed = !self.ready.lock().list.is_empty();
        if listed {
            self.wake();
        }
    }

    /// Takes a removed item off the ready list and its watch.
    fn unlist(&self, item: &Arc<Item>) {
        let unlisted = {
            let mut ready = self.ready.lock();
            item.removed.store(true, SeqCst);
            let i = ready.list.iter().position(|i| Arc::ptr_eq(i, item));
            i.and_then(|i| ready.list.remove(i))
        };
        drop(unlisted);
        item.watch.unsubscribe(&Sub::Item(item.clone()));
    }

    /// Its description went: every interest goes.
    pub fn closed(&self) {
        let items: Vec<Arc<Item>> = core::mem::take(&mut *self.interest.lock()).into_values().collect();
        for item in &items {
            self.unlist(item);
        }
    }

    /// Lists the item and wakes the instance's waiters if the file has the
    /// events now (after EPOLL_CTL_ADD or MOD).
    fn wake_if_ready(item: &Arc<Item>, file: &Description) {
        if item.check(file) != 0 {
            item.event(0);
        }
    }

    fn add(self: &Arc<Self>, fd: i32, target: &FileRef, events: u32, data: u64) -> Result<(), i64> {
        let inner = match &target.file {
            File::Epoll(e) => Some(e.clone()),
            _ => None,
        };
        // The descriptions the loop check reached, let go of after `NESTING` (the last
        // reference to one closes its file, which may wait for netd).
        let mut reached: Vec<Arc<Description>> = Vec::new();
        let nesting = match &inner {
            Some(inner) => {
                let guard = NESTING.lock();
                // A chain of watching instances stays short in both
                // directions: what watches this one, it, and what the
                // target watches.
                if levels_above(self) + 1 + levels_below(inner, self, &mut reached)? > MAX_NESTING + 1 {
                    return Err(ELOOP);
                }
                Some(guard)
            }
            None => None,
        };
        let mut interest = self.interest.lock();
        let key = (fd, target.ptr());
        if interest.contains_key(&key) {
            return Err(EEXIST);
        }
        // (Files that are always ready have no watch: EPERM before.)
        let watch = target.watch.clone().ok_or(EPERM)?;
        // Counted from here; the item value owns it and its drop gives it back, exactly once
        // (also when no memory is found for it: `Arc::try_new` drops the value).
        if WATCHES.try_update(SeqCst, SeqCst, |c| (c < MAX_WATCHES).then_some(c + 1)).is_err() {
            return Err(ENOSPC);
        }
        let item = Arc::try_new(Item {
            key,
            file: Arc::downgrade(target.arc()),
            watch: watch.clone(),
            epoll: Arc::downgrade(self),
            events: AtomicU32::new(events),
            data: AtomicU64::new(data),
            queued: AtomicBool::new(false),
            removed: AtomicBool::new(false),
        })
        .map_err(|_| ENOMEM)?;
        // Room for every item on the ready list before anything can fail
        // halfway.
        {
            let mut ready = self.ready.lock();
            let room = (interest.len() + 1).saturating_sub(ready.list.len());
            ready.list.try_reserve(room).map_err(|_| ENOMEM)?;
        }
        watch.subscribe(Sub::Item(item.clone()))?;
        interest.insert(key, item.clone());
        drop(interest);
        drop(nesting);
        drop(reached);
        Epoll::wake_if_ready(&item, target);
        Ok(())
    }

    fn modify(&self, fd: i32, target: &FileRef, events: u32, data: u64) -> Result<(), i64> {
        let item = self.interest.lock().get(&(fd, target.ptr())).cloned().ok_or(ENOENT)?;
        if events & EPOLLEXCLUSIVE != 0 || item.exclusive() {
            return Err(EINVAL);
        }
        item.data.store(data, SeqCst);
        item.events.store(events, SeqCst);
        Epoll::wake_if_ready(&item, target);
        Ok(())
    }

    fn delete(&self, fd: i32, target: &FileRef) -> Result<(), i64> {
        let item = self.interest.lock().remove(&(fd, target.ptr())).ok_or(ENOENT)?;
        self.unlist(&item);
        Ok(())
    }

    fn items(&self) -> Vec<Arc<Item>> {
        self.interest.lock().values().cloned().collect()
    }

    /// Takes up to `max` events off the ready list: (events, data, item).
    /// One-shot items are disabled; the caller puts level-triggered ones
    /// back once it delivered them (`settle`).
    fn collect(&self, max: usize) -> Vec<Taken> {
        // A report that found no room: check every item.
        let overflow = core::mem::replace(&mut self.ready.lock().overflow, false);
        if overflow {
            for item in self.items() {
                self.enqueue(&item);
            }
        }
        let mut out = Vec::new();
        let pending = self.ready.lock().list.len();
        for _ in 0..pending {
            if out.len() == max {
                break;
            }
            let Some(item) = self.ready.lock().list.pop_front() else { break };
            item.queued.store(false, SeqCst);
            // (Its last reference may go here: no lock of this instance is
            // held.)
            let Some(file) = item.file.upgrade() else { continue };
            let events = item.check(&file);
            drop(file);
            if events == 0 {
                continue;
            }
            if out.try_reserve(1).is_err() {
                self.enqueue(&item);
                break;
            }
            let flags = item.events.load(SeqCst);
            if flags & EPOLLONESHOT != 0 {
                item.events.store(flags & FLAGS, SeqCst);
            }
            out.push(Taken { events, data: item.data.load(SeqCst), item, flags });
        }
        out
    }

    /// After delivery: the level-triggered items that were reported stay
    /// ready, at the end of the list; those not delivered (a fault) go back,
    /// a one-shot one armed again (unless EPOLL_CTL_MOD changed it
    /// meanwhile).
    fn settle(&self, taken: Vec<Taken>, delivered: usize) {
        for (i, t) in taken.into_iter().enumerate() {
            if i >= delivered {
                if t.flags & EPOLLONESHOT != 0 {
                    let _ = t.item.events.compare_exchange(t.flags & FLAGS, t.flags, SeqCst, SeqCst);
                }
                self.enqueue(&t.item);
            } else if t.flags & (EPOLLET | EPOLLONESHOT) == 0 {
                self.enqueue(&t.item);
            }
        }
    }

    /// Collects events into the program's array at `out` (at most `max`);
    /// how many, EFAULT if not even one could be written.
    fn transfer(&self, out: u64, max: usize) -> Result<usize, i64> {
        let taken = self.collect(max);
        let mut delivered = 0;
        let mut fault = false;
        for Taken { events, data, .. } in &taken {
            let mut record = [0u8; EVENT_SIZE as usize];
            record[..4].copy_from_slice(&events.to_ne_bytes());
            record[4..].copy_from_slice(&data.to_ne_bytes());
            if usercopy::to_program(out + delivered as u64 * EVENT_SIZE, &record).is_err() {
                fault = true;
                break;
            }
            delivered += 1;
        }
        self.settle(taken, delivered);
        if fault && delivered == 0 {
            return Err(EFAULT);
        }
        Ok(delivered)
    }

    /// Whether an epoll_wait would report something now (a poll on the
    /// instance itself, or an instance watching it).
    pub fn has_events(&self) -> bool {
        let listed: Option<Vec<Arc<Item>>> = {
            let ready = self.ready.lock();
            (!ready.overflow).then(|| ready.list.iter().cloned().collect())
        };
        // (Overflowed: every item is a candidate.)
        let candidates = listed.unwrap_or_else(|| self.items());
        candidates.iter().any(|i| i.file.upgrade().is_some_and(|f| i.check(&f) != 0))
    }

    /// fstat: an anonymous inode (no file type), as on Linux.
    pub fn stat(&self) -> [u8; 144] {
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.id.to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&0o600u32.to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st
    }
}

/// The instances in the longest chain that starts at `target` and goes
/// down through the instances it watches (`target` included). ELOOP if
/// it reaches `epoll` (a cycle) or gets too long. Each instance is
/// visited once (its result is remembered).
fn levels_below(target: &Arc<Epoll>, epoll: &Epoll, reached: &mut Vec<Arc<Description>>) -> Result<usize, i64> {
    levels_below_from(target, epoll, 1, &mut BTreeMap::new(), reached)
}

fn levels_below_from(
    target: &Arc<Epoll>,
    epoll: &Epoll,
    depth: usize,
    seen: &mut BTreeMap<usize, usize>,
    reached: &mut Vec<Arc<Description>>,
) -> Result<usize, i64> {
    if core::ptr::eq(Arc::as_ptr(target), epoll) || depth > MAX_NESTING + 1 {
        return Err(ELOOP);
    }
    let key = Arc::as_ptr(target) as usize;
    if let Some(&levels) = seen.get(&key) {
        return Ok(levels);
    }
    let mut levels = 1;
    for inner in watched_instances(target, reached) {
        levels = levels.max(1 + levels_below_from(&inner, epoll, depth + 1, seen, reached)?);
    }
    seen.insert(key, levels);
    Ok(levels)
}

/// The instances `epoll` watches. The descriptions are reached only after
/// the interest list's lock is released (letting go of one closed
/// meanwhile takes that lock to forget it), and kept in `reached`: the
/// caller lets go of them once it holds no lock (one may be the last
/// reference, whose close may wait for netd).
fn watched_instances(epoll: &Epoll, reached: &mut Vec<Arc<Description>>) -> Vec<Arc<Epoll>> {
    let files: Vec<Weak<Description>> = epoll.interest.lock().values().map(|i| i.file.clone()).collect();
    let mut instances = Vec::new();
    for f in files.iter().filter_map(Weak::upgrade) {
        if let File::Epoll(inner) = &f.file {
            instances.push(inner.clone());
        }
        reached.push(f);
    }
    instances
}

/// The instances in the longest chain of instances that watch `epoll`,
/// one watching the next (`epoll` not included).
fn levels_above(epoll: &Epoll) -> usize {
    levels_above_from(epoll, 0, &mut BTreeMap::new())
}

fn levels_above_from(epoll: &Epoll, depth: usize, seen: &mut BTreeMap<usize, usize>) -> usize {
    let key = epoll as *const Epoll as usize;
    if let Some(&levels) = seen.get(&key) {
        return levels;
    }
    // Bounded by the checks; the cut-off only guards the stack.
    if depth > MAX_NESTING {
        return depth;
    }
    let Some(watch) = epoll.own_watch() else { return 0 };
    let owners: Vec<Arc<Epoll>> = watch
        .subscribers()
        .into_iter()
        .filter_map(|s| match s {
            Sub::Item(i) if !i.removed.load(SeqCst) => i.epoll.upgrade(),
            _ => None,
        })
        .collect();
    let levels = owners.iter().map(|owner| 1 + levels_above_from(owner, depth + 1, seen)).max().unwrap_or(0);
    seen.insert(key, levels);
    levels
}

/// The calls on an epoll instance's descriptor that are file calls.
pub fn on_file(nr: u64, e: &Epoll, a1: u64) -> Result<i64, i64> {
    match nr {
        files::SYS_FSTAT => usercopy::to_program(a1, &e.stat()).map(|_| 0),
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(ESPIPE),
        files::SYS_IOCTL => Err(ENOTTY),
        _ => Err(EINVAL),
    }
}

// ------------------------------------------------------------ syscalls

/// The instance behind `epfd`: EBADF, EINVAL for another file.
fn instance(epfd: u64) -> Result<(FileRef, Arc<Epoll>), i64> {
    let f = files::lookup(epfd)?;
    let ep = match &f.file {
        File::Epoll(e) => e.clone(),
        _ => return Err(EINVAL),
    };
    Ok((f, ep))
}

/// epoll_create1(flags), and epoll_create(size) with flags 0.
fn epoll_create1(flags: u64) -> Result<i64, i64> {
    if flags & !(O_CLOEXEC as u64) != 0 {
        return Err(EINVAL);
    }
    let id = files::new_id();
    let ep = Epoll::new(id);
    // Its own watch before anything can report it.
    let watch = Watch::new(id);
    *ep.watch.lock() = Arc::downgrade(&watch);
    files::install_watched(id, File::Epoll(ep), O_RDWR | flags as u32, Some(watch))
}

fn epoll_ctl(epfd: u64, op: u64, fd: u64, event: u64) -> Result<i64, i64> {
    let (events, data) = if op == EPOLL_CTL_DEL {
        (0, 0)
    } else {
        let raw: [u8; EVENT_SIZE as usize] = usercopy::read(event)?;
        (u32::from_ne_bytes(raw[..4].try_into().unwrap_or_default()), u64::from_ne_bytes(raw[4..].try_into().unwrap_or_default()))
    };
    let epfile = files::lookup(epfd)?;
    let target = files::lookup(fd)?;
    // Files without a poll method of their own (Linux's file_can_poll).
    if target.file.always_ready() {
        return Err(EPERM);
    }
    if op != EPOLL_CTL_DEL && events & EPOLLEXCLUSIVE != 0 {
        if op == EPOLL_CTL_MOD || matches!(target.file, File::Epoll(_)) || events & !EXCLUSIVE_OK != 0 {
            return Err(EINVAL);
        }
    }
    let File::Epoll(epoll) = &epfile.file else { return Err(EINVAL) };
    if epfile.ptr() == target.ptr() {
        return Err(EINVAL);
    }
    match op {
        EPOLL_CTL_ADD => epoll.add(fd as i32, &target, events, data)?,
        EPOLL_CTL_MOD => epoll.modify(fd as i32, &target, events, data)?,
        EPOLL_CTL_DEL => epoll.delete(fd as i32, &target)?,
        _ => return Err(EINVAL),
    }
    Ok(0)
}

/// epoll_wait's core: events into `out` (at most `max`), waiting until
/// there are some, `deadline` or a signal (EINTR, not restarted: Linux's),
/// with `mask` as the temporary signal mask (`signal::with_mask`: if the
/// call does not end with EINTR, the caller's own mask comes back without
/// the signal only the temporary mask let through, Linux's
/// restore_saved_sigmask_unless).
fn wait(epfd: u64, out: u64, max: u64, deadline: Option<u64>, mask: Option<u64>) -> Result<i64, i64> {
    crate::signal::with_mask(mask, || wait_events(epfd, out, max, deadline))
}

fn wait_events(epfd: u64, out: u64, max: u64, deadline: Option<u64>) -> Result<i64, i64> {
    let max = max as i32;
    if max <= 0 || max as u64 > i32::MAX as u64 / EVENT_SIZE {
        return Err(EINVAL);
    }
    if out.checked_add(max as u64 * EVENT_SIZE).is_none_or(|end| end > SHARED_BASE) {
        return Err(EFAULT);
    }
    let (_file, ep) = instance(epfd)?;
    let mut timed_out = deadline.is_some_and(|d| poll::now() >= d);
    loop {
        // Counted before the list is looked at: a report after this sees
        // the waiter (and wakes it), or the list shows its item.
        ep.waiters.fetch_add(1, SeqCst);
        let seen = ep.seq.load(SeqCst);
        let n = match ep.transfer(out, max as usize) {
            Ok(n) => n,
            Err(e) => {
                ep.waiters.fetch_sub(1, SeqCst);
                return Err(e);
            }
        };
        if n > 0 || timed_out {
            ep.waiters.fetch_sub(1, SeqCst);
            return Ok(n as i64);
        }
        let r = poll::wait(&[(&ep.seq as *const AtomicU32 as u64, seen)], deadline);
        ep.waiters.fetch_sub(1, SeqCst);
        match r {
            Ok(()) => {}
            Err(ETIMEDOUT) => timed_out = true,
            Err(e) => {
                // Events that came with the signal go first (Linux's
                // ep_poll looks before it gives up): then the call does not
                // end with EINTR.
                let r = ep.transfer(out, max as usize);
                if !matches!(r, Ok(0)) {
                    return r.map(|n| n as i64);
                }
                ep.pass_on();
                return Err(e);
            }
        }
    }
}

/// A timeout in milliseconds as a deadline (negative: none).
fn deadline_ms(ms: i32) -> Option<u64> {
    (ms >= 0).then(|| poll::now().saturating_add(ms as u64 * 1_000_000))
}

fn read_mask(ptr: u64, size: u64) -> Result<Option<u64>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    if size != 8 {
        return Err(EINVAL);
    }
    Ok(Some(usercopy::read(ptr)?))
}

const SYS_EPOLL_CREATE: u64 = 213;
const SYS_EPOLL_WAIT: u64 = 232;
const SYS_EPOLL_CTL: u64 = 233;
const SYS_EPOLL_PWAIT: u64 = 281;
const SYS_EPOLL_CREATE1: u64 = 291;
const SYS_EPOLL_PWAIT2: u64 = 441;

/// The result of an epoll call in `s`, or None.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3, a4, a5) = (s.rdi, s.rsi, s.rdx, s.r10, s.r8, s.r9);
    let result = match s.rax {
        SYS_EPOLL_CREATE if (a0 as i32) <= 0 => Err(EINVAL),
        SYS_EPOLL_CREATE => epoll_create1(0),
        SYS_EPOLL_CREATE1 => epoll_create1(a0),
        SYS_EPOLL_CTL => epoll_ctl(a0, a1, a2, a3),
        SYS_EPOLL_WAIT => wait(a0, a1, a2, deadline_ms(a3 as i32), None),
        SYS_EPOLL_PWAIT => read_mask(a4, a5).and_then(|m| wait(a0, a1, a2, deadline_ms(a3 as i32), m)),
        SYS_EPOLL_PWAIT2 => {
            // A timespec (null: forever).
            let deadline = if a3 == 0 {
                Ok(None)
            } else {
                usercopy::read::<[i64; 2]>(a3).and_then(|[sec, ns]| {
                    if sec < 0 || !(0..1_000_000_000).contains(&ns) {
                        return Err(EINVAL);
                    }
                    Ok(Some(poll::now().saturating_add((sec as u64).saturating_mul(1_000_000_000).saturating_add(ns as u64))))
                })
            };
            deadline.and_then(|d| read_mask(a4, a5).and_then(|m| wait(a0, a1, a2, d, m)))
        }
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}
