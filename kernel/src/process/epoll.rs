//! epoll: an interest list of open files and a ready list, the event loop
//! interface of libuv (and so of Node.js).
//!
//! Each interest is an `Item` registered as a waker on the wait queue of
//! its file (see `poll::PollSource`). A wakeup puts the item on the ready
//! list, without checking anything (it may run in interrupt context), and
//! wakes whoever waits on the instance's own queue: epoll_wait, a poll on
//! the instance, an outer instance that watches this one. epoll_wait then
//! takes items off the ready list and asks each file for its readiness:
//! a level-triggered item that is still ready goes back to the end of the
//! list (so a full `maxevents` rotates through them), an edge-triggered
//! one waits for the next wakeup, a one-shot one is disabled until
//! EPOLL_CTL_MOD.
//!
//! The ready list never allocates in a wakeup: it holds each item at most
//! once and keeps room for all of them. Interests belong to the open file,
//! not the descriptor: closing the last descriptor of a file removes it
//! from every instance that watches it.
//!
//! Lock order: loop check → interest list → (wait queue of a file →)
//! ready list → the instance's queue → an outer instance's ready list.
//! Instances may watch each other only without cycles and at most
//! `MAX_NESTING` deep, so this order has no cycle either.

use super::errno::*;
use super::poll::{PollSource, PollTable, Registration};
use super::sched::{WaitQueue, Waker};
use super::{signal, uaccess, with_current};
use crate::fs::file::{Kind, OpenFile, O_CLOEXEC};
use crate::sync::IrqSpinLock;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

const EPOLLERR: u32 = 0x8;
const EPOLLHUP: u32 = 0x10;
const EPOLLEXCLUSIVE: u32 = 1 << 28;
const EPOLLWAKEUP: u32 = 1 << 29;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
/// The input flags; the rest are events.
const FLAGS: u32 = EPOLLEXCLUSIVE | EPOLLWAKEUP | EPOLLONESHOT | EPOLLET;

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;

/// How deep instances may watch each other (Linux: 4).
const MAX_NESTING: usize = 4;
/// Size of a `struct epoll_event` (packed on x86-64).
const EVENT_SIZE: u64 = 12;

/// Serializes adding an instance to another, so that the loop check and
/// the insertion are one step.
static NESTING: spin::Mutex<()> = spin::Mutex::new(());

pub struct Epoll {
    /// Interests by descriptor number and open file.
    interest: spin::Mutex<BTreeMap<(i32, usize), Arc<Item>>>,
    shared: Arc<Shared>,
    /// The open file of this instance, whose watchers are the instances
    /// that watch this one.
    file: spin::Mutex<Weak<OpenFile>>,
}

/// What the items of an instance share with it.
struct Shared {
    ready: IrqSpinLock<VecDeque<Arc<Item>>>,
    /// Woken when an item becomes ready.
    queue: WaitQueue,
    /// A wakeup found the ready list full (it never should): the next
    /// epoll_wait checks every item.
    overflow: AtomicBool,
    /// Items whose files announce nothing (sockets): checked every time.
    rechecked: AtomicUsize,
}

pub struct Item {
    me: Weak<Item>,
    file: Weak<OpenFile>,
    epoll: Weak<Epoll>,
    shared: Arc<Shared>,
    /// The requested events and flags; only flags once a one-shot fired.
    events: AtomicU32,
    data: AtomicU64,
    /// On the ready list.
    queued: AtomicBool,
    removed: AtomicBool,
    recheck: bool,
    registration: spin::Mutex<Option<Registration>>,
}

impl Waker for Item {
    /// The file's readiness may have changed: onto the ready list, and
    /// wake the instance's waiters (even if it was listed already: an
    /// outer instance or a poll may not have seen it yet).
    fn wake(&self) {
        if self.removed.load(Ordering::Acquire) || self.events.load(Ordering::Relaxed) & !FLAGS == 0 {
            return;
        }
        self.enqueue();
        self.shared.queue.wake(0);
    }
}

impl Item {
    /// Puts the item on the ready list (once) without waking anyone.
    fn enqueue(&self) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        // Alive: whoever calls this holds a reference.
        let Some(me) = self.me.upgrade() else { return };
        let mut ready = self.shared.ready.lock();
        // Checked under the list's lock, which `forget` sets it under: a
        // removed item never gets (back) on the list.
        if self.removed.load(Ordering::Acquire) {
            return;
        }
        if ready.len() < ready.capacity() {
            ready.push_back(me);
        } else {
            self.queued.store(false, Ordering::Release);
            self.shared.overflow.store(true, Ordering::Release);
        }
    }

    /// The instance this interest belongs to.
    pub fn owner(&self) -> Option<Arc<Epoll>> {
        self.epoll.upgrade()
    }

    fn waker(self: &Arc<Self>) -> Arc<dyn Waker> {
        self.clone()
    }

    /// The events among those requested that `file` has now.
    fn check(&self, file: &OpenFile) -> u32 {
        let wanted = self.events.load(Ordering::Relaxed);
        if wanted & !FLAGS == 0 {
            return 0;
        }
        file.poll(wanted as u16 as i16) as u16 as u32 & (wanted | EPOLLERR | EPOLLHUP) & !FLAGS
    }
}

impl Epoll {
    pub fn new() -> Arc<Epoll> {
        Arc::new(Epoll {
            interest: spin::Mutex::new(BTreeMap::new()),
            shared: Arc::new(Shared {
                ready: IrqSpinLock::new(VecDeque::new()),
                queue: WaitQueue::new(),
                overflow: AtomicBool::new(false),
                rechecked: AtomicUsize::new(0),
            }),
            file: spin::Mutex::new(Weak::new()),
        })
    }

    /// The queue woken when an item becomes ready.
    pub fn queue(&self) -> &WaitQueue {
        &self.shared.queue
    }

    fn add(self: &Arc<Self>, fd: i32, file: &Arc<OpenFile>, events: u32, data: u64) -> Result<(), i64> {
        let source = file.poll_source();
        if matches!(source, PollSource::Always) {
            return Err(EPERM);
        }
        if events & EPOLLEXCLUSIVE != 0 && (events & EPOLLONESHOT != 0 || matches!(source, PollSource::Epoll(_))) {
            return Err(EINVAL);
        }
        let _nesting = match &source {
            PollSource::Epoll(target) => {
                let guard = NESTING.lock();
                // A chain of watching instances stays short in both
                // directions: what watches this one, it, and what the
                // target watches.
                if levels_above(self) + 1 + levels_below(target, self)? > MAX_NESTING + 1 {
                    return Err(ELOOP);
                }
                Some(guard)
            }
            _ => None,
        };
        let mut interest = self.interest.lock();
        let key = (fd, Arc::as_ptr(file) as usize);
        if interest.contains_key(&key) {
            return Err(EEXIST);
        }
        let recheck = matches!(source, PollSource::Recheck);
        let item = Arc::new_cyclic(|me| Item {
            me: me.clone(),
            file: Arc::downgrade(file),
            epoll: Arc::downgrade(self),
            shared: self.shared.clone(),
            events: AtomicU32::new(events),
            data: AtomicU64::new(data),
            queued: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            recheck,
            registration: spin::Mutex::new(None),
        });
        // Room for every item on the ready list, and in the file's list of
        // watchers, before anything can fail halfway.
        {
            let mut ready = self.shared.ready.lock();
            let room = (interest.len() + 1).saturating_sub(ready.len());
            ready.try_reserve(room).map_err(|_| ENOMEM)?;
        }
        {
            let mut watchers = file.watchers.lock();
            watchers.retain(|w| w.strong_count() > 0);
            watchers.try_reserve(1).map_err(|_| ENOMEM)?;
            watchers.push(Arc::downgrade(&item));
        }
        let registration = match Registration::new(&source, item.waker()) {
            Ok(r) => r,
            Err(e) => {
                file.watchers.lock().retain(|w| !core::ptr::eq(w.as_ptr(), Arc::as_ptr(&item)));
                return Err(e);
            }
        };
        *item.registration.lock() = registration;
        if recheck {
            self.shared.rechecked.fetch_add(1, Ordering::Relaxed);
        }
        interest.insert(key, item.clone());
        drop(interest);
        // Whether it is ready already, the next epoll_wait finds out.
        item.wake();
        Ok(())
    }

    fn modify(&self, fd: i32, file: &Arc<OpenFile>, events: u32, data: u64) -> Result<(), i64> {
        if events & EPOLLEXCLUSIVE != 0 {
            return Err(EINVAL);
        }
        let item = self.interest.lock().get(&(fd, Arc::as_ptr(file) as usize)).cloned().ok_or(ENOENT)?;
        if item.events.load(Ordering::Relaxed) & EPOLLEXCLUSIVE != 0 {
            return Err(EINVAL);
        }
        item.data.store(data, Ordering::Relaxed);
        item.events.store(events, Ordering::Release);
        item.wake();
        Ok(())
    }

    fn delete(&self, fd: i32, file: &Arc<OpenFile>) -> Result<(), i64> {
        let item = self.interest.lock().remove(&(fd, Arc::as_ptr(file) as usize)).ok_or(ENOENT)?;
        self.forget(&item);
        Ok(())
    }

    /// The last descriptor of an item's file was closed.
    pub fn file_closed(&self, item: &Arc<Item>) {
        let removed = {
            let mut interest = self.interest.lock();
            let key = interest.iter().find(|(_, i)| Arc::ptr_eq(i, item)).map(|(k, _)| *k);
            key.and_then(|k| interest.remove(&k))
        };
        if let Some(item) = removed {
            self.forget(&item);
        }
    }

    /// Takes a removed item off its file's queue and the ready list.
    fn forget(&self, item: &Arc<Item>) {
        let unlisted = {
            let mut ready = self.shared.ready.lock();
            item.removed.store(true, Ordering::Release);
            let i = ready.iter().position(|i| Arc::ptr_eq(i, item));
            i.and_then(|i| ready.remove(i))
        };
        drop(unlisted);
        drop(item.registration.lock().take());
        if item.recheck {
            self.shared.rechecked.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn items(&self) -> Vec<Arc<Item>> {
        self.interest.lock().values().cloned().collect()
    }

    /// Fills up to `max` events at `out`; returns how many.
    fn collect(&self, out: u64, max: usize) -> Result<usize, i64> {
        // Items that wakeups cannot put on the list are checked every time.
        if self.shared.overflow.swap(false, Ordering::AcqRel) {
            self.items().iter().for_each(|i| i.enqueue());
        } else if self.shared.rechecked.load(Ordering::Relaxed) > 0 {
            self.items().iter().filter(|i| i.recheck).for_each(|i| i.enqueue());
        }
        let mut again = Vec::new();
        let mut n = 0;
        let pending = self.shared.ready.lock().len();
        for _ in 0..pending {
            if n == max {
                break;
            }
            let Some(item) = self.shared.ready.lock().pop_front() else { break };
            item.queued.store(false, Ordering::Release);
            let Some(file) = item.file.upgrade() else { continue };
            let events = item.check(&file);
            drop(file);
            if events == 0 {
                continue;
            }
            let mut record = [0u8; EVENT_SIZE as usize];
            record[..4].copy_from_slice(&events.to_ne_bytes());
            record[4..].copy_from_slice(&item.data.load(Ordering::Relaxed).to_ne_bytes());
            if let Err(e) = uaccess::write(out + n as u64 * EVENT_SIZE, record) {
                item.enqueue();
                return if n > 0 { Ok(n) } else { Err(e) };
            }
            n += 1;
            let flags = item.events.load(Ordering::Relaxed);
            if flags & EPOLLONESHOT != 0 {
                item.events.store(flags & FLAGS, Ordering::Release);
            } else if flags & EPOLLET == 0 {
                again.try_reserve(1).map_err(|_| ENOMEM)?;
                again.push(item);
            }
        }
        // Level-triggered items that were reported stay ready, at the end.
        for item in again {
            item.enqueue();
        }
        Ok(n)
    }

    /// Whether an epoll_wait would report something now (a poll on the
    /// instance itself).
    pub fn has_events(&self) -> bool {
        let mut candidates: Vec<Arc<Item>> = self.shared.ready.lock().iter().cloned().collect();
        if self.shared.rechecked.load(Ordering::Relaxed) > 0 || self.shared.overflow.load(Ordering::Relaxed) {
            candidates = self.items();
        }
        candidates.iter().any(|i| i.file.upgrade().is_some_and(|f| i.check(&f) != 0))
    }

    fn has_recheck(&self) -> bool {
        self.shared.rechecked.load(Ordering::Relaxed) > 0
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        let items: Vec<Arc<Item>> = core::mem::take(&mut *self.interest.lock()).into_values().collect();
        for item in &items {
            self.forget(item);
        }
    }
}

/// The instances in the longest chain that starts at `target` and goes
/// down through the instances it watches (`target` included). ELOOP if
/// it reaches `epoll` (a cycle) or gets too long.
fn levels_below(target: &Arc<Epoll>, epoll: &Epoll) -> Result<usize, i64> {
    levels_below_from(target, epoll, 1)
}

fn levels_below_from(target: &Arc<Epoll>, epoll: &Epoll, depth: usize) -> Result<usize, i64> {
    if core::ptr::eq(Arc::as_ptr(target), epoll) || depth > MAX_NESTING + 1 {
        return Err(ELOOP);
    }
    let mut levels = 1;
    for item in target.items() {
        if let Some(file) = item.file.upgrade() {
            if let Kind::Epoll(inner) = &file.kind {
                levels = levels.max(1 + levels_below_from(inner, epoll, depth + 1)?);
            }
        }
    }
    Ok(levels)
}

/// The instances in the longest chain of instances that watch `epoll`,
/// one watching the next (`epoll` not included). The checks on every
/// add keep it within MAX_NESTING.
fn levels_above(epoll: &Epoll) -> usize {
    let Some(file) = epoll.file.lock().upgrade() else { return 0 };
    let watchers: Vec<Arc<Item>> = file.watchers.lock().iter().filter_map(Weak::upgrade).collect();
    watchers
        .iter()
        .filter(|i| !i.removed.load(Ordering::Acquire))
        .filter_map(|i| i.owner())
        .map(|owner| 1 + levels_above(&owner))
        .max()
        .unwrap_or(0)
}

// ------------------------------------------------------------ syscalls

fn instance(epfd: u64) -> Result<(Arc<OpenFile>, Arc<Epoll>), i64> {
    let file = super::sys_file::file(epfd)?;
    let epoll = match &file.kind {
        Kind::Epoll(e) => e.clone(),
        _ => return Err(EINVAL),
    };
    Ok((file, epoll))
}

/// epoll_create1(flags), and epoll_create(size) with flags 0.
pub fn epoll_create1(flags: u64) -> SysResult {
    if flags & !(O_CLOEXEC as u64) != 0 {
        return Err(EINVAL);
    }
    let epoll = Epoll::new();
    let file = OpenFile::new(Kind::Epoll(epoll.clone()), crate::fs::file::O_RDWR, None);
    *epoll.file.lock() = Arc::downgrade(&file);
    with_current(|p| p.alloc_fd(file, flags != 0, 0))
}

pub fn epoll_create(size: u64) -> SysResult {
    if size as i32 <= 0 {
        return Err(EINVAL);
    }
    epoll_create1(0)
}

pub fn epoll_ctl(epfd: u64, op: u64, fd: u64, event: u64) -> SysResult {
    let (events, data) = if op == EPOLL_CTL_DEL {
        (0, 0)
    } else {
        let raw: [u8; EVENT_SIZE as usize] = uaccess::read(event)?;
        (u32::from_ne_bytes(raw[..4].try_into().unwrap_or_default()), u64::from_ne_bytes(raw[4..].try_into().unwrap_or_default()))
    };
    let (epfile, epoll) = instance(epfd)?;
    let file = super::sys_file::file(fd)?;
    if Arc::ptr_eq(&epfile, &file) {
        return Err(EINVAL);
    }
    match op {
        EPOLL_CTL_ADD => epoll.add(fd as i32, &file, events, data)?,
        EPOLL_CTL_MOD => epoll.modify(fd as i32, &file, events, data)?,
        EPOLL_CTL_DEL => epoll.delete(fd as i32, &file)?,
        _ => return Err(EINVAL),
    }
    Ok(0)
}

/// epoll_wait with a timeout in nanoseconds (None: forever).
fn wait(epfd: u64, out: u64, max: u64, timeout: Option<u64>) -> SysResult {
    let max = max as i32;
    if max <= 0 || max as u64 > i32::MAX as u64 / EVENT_SIZE {
        return Err(EINVAL);
    }
    let (_file, epoll) = instance(epfd)?;
    let deadline = timeout.map(|t| crate::time::now().saturating_add(t));
    let mut table = PollTable::new();
    table.watch_source(&PollSource::Epoll(epoll.clone()))?;
    loop {
        table.rearm();
        table.set_recheck(epoll.has_recheck());
        let n = epoll.collect(out, max as usize)?;
        if n > 0 || deadline.is_some_and(|d| crate::time::now() >= d) {
            return Ok(n as i64);
        }
        table.wait(deadline)?;
    }
}

/// epoll_pwait(epfd, events, maxevents, timeout_ms, sigmask, size);
/// epoll_wait is the same without a mask.
pub fn epoll_pwait(epfd: u64, out: u64, max: u64, timeout_ms: u64, mask: u64, size: u64) -> SysResult {
    let timeout = super::sys_file::poll_timeout(timeout_ms as i32 as i64);
    let mask = signal::read_mask(mask, size)?;
    signal::with_mask(mask, || wait(epfd, out, max, timeout))
}

/// epoll_pwait2: the timeout is a timespec (null: forever).
pub fn epoll_pwait2(epfd: u64, out: u64, max: u64, ts: u64, mask: u64, size: u64) -> SysResult {
    let timeout = super::sys_file::timeout(ts, crate::time::NSEC_PER_SEC)?;
    let mask = signal::read_mask(mask, size)?;
    signal::with_mask(mask, || wait(epfd, out, max, timeout))
}
