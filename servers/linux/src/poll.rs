//! Readiness and waiting for many files at once (phase R6e): the watch
//! lists of the server's open file descriptions, poll, ppoll, select,
//! pselect6 and restart_syscall (epoll is `epoll`'s).
//!
//! **Watches** are Linux's wait queues for poll: every open file
//! description has one (`Watch`), which pollers and epoll interests
//! subscribe to. A file reports each change of its readiness, and each
//! event that is an edge for EPOLLET (new data, room), under its own lock
//! (`files::ready`, by the description's id): the report wakes the
//! subscribed pollers and queues the subscribed epoll interests. Reports
//! find the watch by id in a sharded table, and not at all while nothing
//! anywhere is subscribed (`WATCHED`): a pipe's reports cost a load then.
//! No lost wakeup: a poller subscribes before it checks readiness, and a
//! file reports after it changed under the lock the check takes.
//!
//! **Readiness** is asked of each file when it is checked
//! (`Description::poll_mask`, Linux's poll methods), never taken from the
//! reports, which only say that something happened.
//!
//! **Waiting**: a poller sleeps on its own word (`Waiter`), which reports
//! advance and wake, with the kernel's wait for any of several words
//! (`SYS_SERVER_WAIT`): also on the control-block words of the internet
//! sockets it polls, which netd wakes directly, so a poll on a socket does
//! not wait for the instance's net thread to pass netd's change on. The
//! kernel's wait applies ppoll's and pselect6's temporary signal mask.
//! The kernel's open files are always ready (regular files, /proc and /sys,
//! null and zero): nothing to wait for.
//!
//! **Signals**: interrupted with nothing ready, poll answers
//! ERESTART_RESTARTBLOCK (restarted by restart_syscall with its deadline,
//! kept in the thread's words, unless a handler runs: then EINTR), ppoll,
//! select and pselect6 ERESTARTNOHAND (restarted with the time left, which
//! they write back first, as Linux's poll_select_finish).
//!
//! Lock order: a file's own lock → its watch's subscribers → an epoll
//! instance's ready list → (that instance's watch, for instances watching
//! it: `epoll`) → ... ; no lock is held while program memory is copied.

use crate::files::{self, FileRef, POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
use restricted::*;

const EINTR: i64 = 4;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EINVAL: i64 = 22;
const EPIPE: i64 = 32;
const ETIMEDOUT: i64 = 110;

const POLLPRI: i16 = 0x2;
const POLLRDNORM: i16 = 0x40;
const POLLRDBAND: i16 = 0x80;
const POLLWRNORM: i16 = 0x100;
const POLLWRBAND: i16 = 0x200;
/// What select takes as readable, writable and exceptional (Linux's
/// POLLIN_SET, POLLOUT_SET, POLLEX_SET).
const IN_SET: i16 = POLLRDNORM | POLLRDBAND | POLLIN | POLLHUP | POLLERR;
const OUT_SET: i16 = POLLWRBAND | POLLWRNORM | POLLOUT | POLLERR;
const EX_SET: i16 = POLLPRI;

/// A thread waiting in poll or select: reports advance and wake its word.
pub struct Waiter {
    word: AtomicU32,
    /// Set while the poller sleeps (or is about to): only then does a report
    /// make the kernel call that wakes it. The poller sets it after it read
    /// the word, a report reads it after it advanced the word: either the
    /// report sees it set, or the poller's wait finds the word moved.
    sleeping: AtomicU32,
}

impl Waiter {
    fn new() -> Arc<Waiter> {
        Arc::new(Waiter { word: AtomicU32::new(0), sleeping: AtomicU32::new(0) })
    }

    fn wake(&self) {
        self.word.fetch_add(1, SeqCst);
        if self.sleeping.load(SeqCst) != 0 {
            syscall(SYS_SERVER_FUTEX_WAKE, [&self.word as *const AtomicU32 as u64, 1, 0, 0, 0, 0]);
        }
    }
}

/// Who hears of a description's reports.
#[derive(Clone)]
pub enum Sub {
    Waiter(Arc<Waiter>),
    Item(Arc<crate::epoll::Item>),
}

/// An open file description's watch list (see the module comment).
pub struct Watch {
    subs: Mutex<Vec<Sub>>,
    /// How many there are (read without the lock: an epoll instance
    /// reports itself only while something watches it).
    count: AtomicUsize,
}

/// Watches with subscribers, anywhere in the instance.
static WATCHED: AtomicUsize = AtomicUsize::new(0);

const SHARDS: usize = 32;
/// The watches by their description's id.
static HUB: [Mutex<BTreeMap<u64, Weak<Watch>>>; SHARDS] = [const { Mutex::new(BTreeMap::new()) }; SHARDS];

fn shard(id: u64) -> &'static Mutex<BTreeMap<u64, Weak<Watch>>> {
    &HUB[(id as usize).wrapping_mul(0x9e37_79b9) >> 7 & (SHARDS - 1)]
}

impl Watch {
    /// The watch of a new description `id`.
    pub fn new(id: u64) -> Arc<Watch> {
        let watch = Arc::new(Watch { subs: Mutex::new(Vec::new()), count: AtomicUsize::new(0) });
        shard(id).lock().insert(id, Arc::downgrade(&watch));
        watch
    }

    pub fn subscribe(&self, sub: Sub) -> Result<(), i64> {
        let mut subs = self.subs.lock();
        subs.try_reserve(1).map_err(|_| ENOMEM)?;
        if subs.is_empty() {
            WATCHED.fetch_add(1, SeqCst);
        }
        subs.push(sub);
        self.count.store(subs.len(), SeqCst);
        Ok(())
    }

    pub fn unsubscribe(&self, gone: &Sub) {
        let removed = {
            let mut subs = self.subs.lock();
            let i = subs.iter().position(|s| same(s, gone));
            let removed = i.map(|i| subs.remove(i));
            if removed.is_some() && subs.is_empty() {
                WATCHED.fetch_sub(1, SeqCst);
            }
            self.count.store(subs.len(), SeqCst);
            removed
        };
        // Its last reference may go here, after the lock.
        drop(removed);
    }

    /// Whether anybody listens.
    pub fn watched(&self) -> bool {
        self.count.load(SeqCst) != 0
    }

    /// Who listens (a poller's or epoll's waiters).
    pub fn subscribers(&self) -> Vec<Sub> {
        self.subs.lock().clone()
    }

    /// A report: pollers wake; epoll interests that want the events
    /// (`ready`) are queued, all those without EPOLLEXCLUSIVE, the exclusive
    /// ones in order up to the first that woke a waiter of its instance
    /// (Linux's exclusive wakeup).
    pub fn notify(&self, ready: i16) {
        let subs = self.subs.lock();
        for s in subs.iter() {
            match s {
                Sub::Waiter(w) => w.wake(),
                Sub::Item(i) if !i.exclusive() => {
                    i.event(ready);
                }
                Sub::Item(_) => {}
            }
        }
        for s in subs.iter() {
            if let Sub::Item(i) = s {
                if i.exclusive() && i.event(ready) {
                    break;
                }
            }
        }
    }
}

fn same(a: &Sub, b: &Sub) -> bool {
    match (a, b) {
        (Sub::Waiter(x), Sub::Waiter(y)) => Arc::ptr_eq(x, y),
        (Sub::Item(x), Sub::Item(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

/// `files::ready`: description `id` reported (see `Watch::notify`).
pub fn report(id: u64, ready: i16) {
    if WATCHED.load(SeqCst) == 0 {
        return;
    }
    let watch = shard(id).lock().get(&id).and_then(Weak::upgrade);
    if let Some(watch) = watch {
        watch.notify(ready);
    }
}

/// Description `id` went: its watch is no longer found, and the epoll
/// interests in it go (as Linux's eventpoll_release: interests belong to
/// the description).
pub fn forget(id: u64, watch: &Arc<Watch>) {
    shard(id).lock().remove(&id);
    let items: Vec<Arc<crate::epoll::Item>> = {
        let mut subs = watch.subs.lock();
        let had = !subs.is_empty();
        let items = subs.iter().filter_map(|s| if let Sub::Item(i) = s { Some(i.clone()) } else { None }).collect();
        subs.retain(|s| matches!(s, Sub::Waiter(_)));
        if had && subs.is_empty() {
            WATCHED.fetch_sub(1, SeqCst);
        }
        watch.count.store(subs.len(), SeqCst);
        items
    };
    for item in items {
        item.file_gone();
    }
}

/// One word of a wait: its address and the value seen.
pub type Word = (u64, u32);

/// Waits until one of `words` is woken (`SYS_SERVER_WAIT`), the deadline
/// (monotonic ns) or a signal, with `mask` as the signal mask while it
/// waits: Ok when woken or a word moved, ETIMEDOUT, EINTR, EPIPE for a word
/// of an object that was hung up (netd died: such words wake nobody any
/// more, the caller stops waiting on them).
pub fn wait(words: &[Word], deadline: Option<u64>, mask: Option<u64>) -> Result<(), i64> {
    let list: Vec<[u64; 2]> = words.iter().take(WAIT_MAX as usize).map(|&(a, v)| [a, v as u64]).collect();
    let mask_word = mask.unwrap_or(0);
    let mask_ptr = if mask.is_some() { &mask_word as *const u64 as u64 } else { 0 };
    let r = syscall(SYS_SERVER_WAIT, [list.as_ptr() as u64, list.len() as u64, deadline.unwrap_or(0), FUTEX_INTERRUPTIBLE, mask_ptr, 0]);
    match r {
        r if r == -EINTR => Err(EINTR),
        r if r == -ETIMEDOUT => Err(ETIMEDOUT),
        r if r >= 0 || r == -EAGAIN => Ok(()),
        r => Err(-r),
    }
}

/// The monotonic clock (ns).
pub fn now() -> u64 {
    const CLOCK_MONOTONIC: u64 = 1;
    syscall(SYS_CLOCK_READ, [CLOCK_MONOTONIC, 0, 0, 0, 0, 0]) as u64
}

/// A timeout in a timespec (a timeval with `micro`) at `ptr`, as a deadline;
/// None for a null pointer (forever). EINVAL for a negative one, or a
/// timespec's nanoseconds beyond a second; a timeval's microseconds beyond
/// a second count as seconds (Linux's select).
fn deadline_of(ptr: u64, micro: bool) -> Result<Option<u64>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    let [sec, sub]: [i64; 2] = usercopy::read(ptr)?;
    let per = if micro { 1_000_000 } else { 1_000_000_000 };
    if sec < 0 || sub < 0 || (!micro && sub >= per) {
        return Err(EINVAL);
    }
    let ns = (sec as u64).saturating_mul(1_000_000_000).saturating_add((sub as u64).saturating_mul(1_000_000_000 / per as u64));
    Ok(Some(now().saturating_add(ns)))
}

/// The time left until `deadline`, written back to the program's timespec
/// (timeval with `micro`) at `ptr` (Linux's poll_select_finish): what a
/// restarted call waits. A zero timeout is not written; if it cannot be
/// written, a restart would wait too long: EINTR instead.
fn finish(ptr: u64, micro: bool, deadline: Option<u64>, result: Result<i64, i64>, zero: bool) -> Result<i64, i64> {
    let Some(deadline) = deadline else { return result };
    if ptr == 0 || zero {
        return result;
    }
    let left = deadline.saturating_sub(now());
    let (sec, sub) = (left / 1_000_000_000, left % 1_000_000_000);
    let value = [sec as i64, if micro { (sub / 1000) as i64 } else { sub as i64 }];
    match usercopy::write(ptr, &value) {
        Ok(()) => result,
        Err(_) if result == Err(ERESTARTNOHAND) => Err(EINTR),
        Err(_) => result,
    }
}

/// The descriptions a poll or select listens to for the call: subscribed
/// once each, kept (so their files stay while it waits), and the internet
/// sockets among them whose control blocks it waits on directly.
struct Listening {
    waiter: Arc<Waiter>,
    files: Vec<FileRef>,
    inet: Vec<Arc<crate::inet::InetSock>>,
}

impl Listening {
    fn new() -> Listening {
        Listening { waiter: Waiter::new(), files: Vec::new(), inet: Vec::new() }
    }

    fn listen(&mut self, f: &FileRef) -> Result<(), i64> {
        let Some(watch) = f.watch.as_ref().filter(|_| !f.is_path()) else { return Ok(()) };
        if self.files.iter().any(|g| g.ptr() == f.ptr()) {
            return Ok(());
        }
        self.files.try_reserve(1).map_err(|_| ENOMEM)?;
        watch.subscribe(Sub::Waiter(self.waiter.clone()))?;
        self.files.push(f.clone());
        if let files::File::Inet(s) = &f.file {
            if self.inet.len() + 1 < WAIT_MAX as usize && self.inet.try_reserve(1).is_ok() {
                s.watch_ctl();
                self.inet.push(s.clone());
            }
        }
        Ok(())
    }

    /// A channel's words were hung up: netd's directly woken words go (the
    /// net thread still reports the sockets through their watches).
    fn without_direct(&mut self) {
        for s in self.inet.drain(..) {
            s.unwatch_ctl();
        }
    }

    /// Sleeps on `words` (read before readiness was checked; see `wait`),
    /// announced to the reports (`Waiter::sleeping`).
    fn sleep(&self, words: &[Word], deadline: Option<u64>, mask: Option<u64>) -> Result<(), i64> {
        self.waiter.sleeping.store(1, SeqCst);
        let r = wait(words, deadline, mask);
        self.waiter.sleeping.store(0, SeqCst);
        r
    }

    /// The words to wait on, read before readiness is checked.
    fn words(&self) -> Vec<Word> {
        let mut words = Vec::with_capacity(1 + self.inet.len());
        words.push((&self.waiter.word as *const AtomicU32 as u64, self.waiter.word.load(SeqCst)));
        for s in &self.inet {
            words.push(s.ctl_word());
        }
        words
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        let me = Sub::Waiter(self.waiter.clone());
        for f in &self.files {
            if let Some(watch) = &f.watch {
                watch.unsubscribe(&me);
            }
        }
        for s in &self.inet {
            s.unwatch_ctl();
        }
    }
}

/// One entry of poll's array.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// The core of poll and ppoll: checks every entry, sleeps until one may
/// have changed, until one is ready or `deadline` passed; the count of ready
/// entries, or EINTR if a signal came first.
fn do_poll(entries: &mut [PollFd], deadline: Option<u64>, mask: Option<u64>) -> Result<i64, i64> {
    let table = crate::fdtable::current();
    let mut listening = Listening::new();
    for e in entries.iter().filter(|e| e.fd >= 0) {
        if let Ok(f) = table.get(e.fd as u64) {
            listening.listen(&f)?;
        }
    }
    let mut timed_out = deadline.is_some_and(|d| now() >= d);
    loop {
        let words = listening.words();
        let mut ready = 0;
        for e in entries.iter_mut() {
            e.revents = if e.fd < 0 {
                0
            } else {
                match table.get(e.fd as u64) {
                    Ok(f) => {
                        // A description that came meanwhile (the
                        // descriptor was replaced) is listened to as well.
                        listening.listen(&f)?;
                        f.poll_mask() & (e.events | POLLERR | POLLHUP | POLLNVAL)
                    }
                    Err(_) => POLLNVAL,
                }
            };
            if e.revents != 0 {
                ready += 1;
            }
        }
        if ready > 0 || timed_out {
            return Ok(ready);
        }
        match listening.sleep(&words, deadline, mask) {
            Ok(()) => {}
            Err(ETIMEDOUT) => timed_out = true,
            Err(EPIPE) => listening.without_direct(),
            Err(e) => return Err(e),
        }
    }
}

/// Reads poll's array (at most RLIMIT_NOFILE entries, as Linux).
fn read_pollfds(fds: u64, nfds: u64) -> Result<Vec<PollFd>, i64> {
    if nfds > crate::fdtable::nofile() {
        return Err(EINVAL);
    }
    let mut entries = Vec::new();
    entries.try_reserve_exact(nfds as usize).map_err(|_| ENOMEM)?;
    entries.resize(nfds as usize, PollFd::default());
    let bytes = unsafe { core::slice::from_raw_parts_mut(entries.as_mut_ptr() as *mut u8, nfds as usize * 8) };
    usercopy::from_program(fds, bytes)?;
    Ok(entries)
}

/// Writes every entry's revents back.
fn write_revents(fds: u64, entries: &[PollFd]) -> Result<(), i64> {
    for (i, e) in entries.iter().enumerate() {
        usercopy::write(fds + i as u64 * 8 + 6, &e.revents)?;
    }
    Ok(())
}

/// poll(fds, nfds, timeout) until `deadline`; interrupted, it is restarted
/// by restart_syscall with the same deadline.
fn poll_until(fds: u64, nfds: u64, deadline: Option<u64>) -> Result<i64, i64> {
    let mut entries = read_pollfds(fds, nfds)?;
    let result = do_poll(&mut entries, deadline, None);
    write_revents(fds, &entries)?;
    match result {
        Err(EINTR) => {
            crate::thread::set_restart([crate::thread::RESTART_POLL, fds, nfds, deadline.unwrap_or(u64::MAX)]);
            Err(ERESTART_RESTARTBLOCK)
        }
        r => r,
    }
}

fn poll(fds: u64, nfds: u64, timeout_ms: i32) -> Result<i64, i64> {
    let deadline = (timeout_ms >= 0).then(|| now().saturating_add(timeout_ms as u64 * 1_000_000));
    poll_until(fds, nfds, deadline)
}

/// A temporary signal mask argument: none for a null pointer, EINVAL for a
/// size but 8.
fn read_mask(ptr: u64, size: u64) -> Result<Option<u64>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    if size != 8 {
        return Err(EINVAL);
    }
    Ok(Some(usercopy::read(ptr)?))
}

fn ppoll(fds: u64, nfds: u64, ts: u64, mask: u64, size: u64) -> Result<i64, i64> {
    let deadline = deadline_of(ts, false)?;
    let zero = ts != 0 && usercopy::read::<[i64; 2]>(ts).is_ok_and(|t| t == [0, 0]);
    let mask = read_mask(mask, size)?;
    let mut entries = read_pollfds(fds, nfds)?;
    let result = do_poll(&mut entries, deadline, mask);
    write_revents(fds, &entries)?;
    let result = if result == Err(EINTR) { Err(ERESTARTNOHAND) } else { result };
    finish(ts, false, deadline, result, zero)
}

/// The core of select and pselect6.
fn do_select(nfds: u64, sets: [u64; 3], deadline: Option<u64>, mask: Option<u64>) -> Result<i64, i64> {
    if (nfds as i64) < 0 {
        return Err(EINVAL);
    }
    let table = crate::fdtable::current();
    // Bits beyond the table are ignored (Linux's max_fds: the table's
    // size, at least 64).
    let nfds = nfds.min(table.size().next_multiple_of(64).max(64) as u64);
    let words = nfds.div_ceil(64) as usize;
    let load = |ptr: u64| -> Result<Vec<u64>, i64> {
        let mut set = Vec::new();
        set.try_reserve_exact(words).map_err(|_| ENOMEM)?;
        set.resize(words, 0);
        if ptr != 0 {
            let bytes = unsafe { core::slice::from_raw_parts_mut(set.as_mut_ptr() as *mut u8, words * 8) };
            usercopy::from_program(ptr, bytes)?;
        }
        Ok(set)
    };
    let want = [load(sets[0])?, load(sets[1])?, load(sets[2])?];
    let wanted = |fd: u64| -> u8 {
        let (w, b) = ((fd / 64) as usize, 1u64 << (fd % 64));
        (0..3).fold(0, |m, k| m | if want[k][w] & b != 0 { 1 << k } else { 0 })
    };
    // Every descriptor asked about must be open (Linux's max_select_fd);
    // an O_PATH one is, but never ready (its fdget finds no file).
    let mut listening = Listening::new();
    for fd in 0..nfds {
        if wanted(fd) != 0 {
            let f = files::lookup_raw(fd)?;
            listening.listen(&f)?;
        }
    }
    let mut timed_out = deadline.is_some_and(|d| now() >= d);
    loop {
        let words_now = listening.words();
        let mut got = [alloc::vec![0u64; words], alloc::vec![0u64; words], alloc::vec![0u64; words]];
        let mut ready = 0;
        for fd in 0..nfds {
            let asked = wanted(fd);
            if asked == 0 {
                continue;
            }
            // Closed meanwhile: not ready (Linux's do_select).
            let Ok(f) = files::lookup(fd) else { continue };
            listening.listen(&f)?;
            let m = f.poll_mask();
            let (w, b) = ((fd / 64) as usize, 1u64 << (fd % 64));
            for (k, set) in [IN_SET, OUT_SET, EX_SET].into_iter().enumerate() {
                if asked & 1 << k != 0 && m & set != 0 {
                    got[k][w] |= b;
                    ready += 1;
                }
            }
        }
        if ready > 0 || timed_out {
            for (k, set) in got.iter().enumerate() {
                if sets[k] != 0 {
                    let bytes = unsafe { core::slice::from_raw_parts(set.as_ptr() as *const u8, words * 8) };
                    usercopy::to_program(sets[k], bytes)?;
                }
            }
            return Ok(ready);
        }
        match listening.sleep(&words_now, deadline, mask) {
            Ok(()) => {}
            Err(ETIMEDOUT) => timed_out = true,
            Err(EPIPE) => listening.without_direct(),
            Err(EINTR) => return Err(ERESTARTNOHAND),
            Err(e) => return Err(e),
        }
    }
}

fn select(nfds: u64, r: u64, w: u64, e: u64, tv: u64) -> Result<i64, i64> {
    let deadline = deadline_of(tv, true)?;
    let zero = tv != 0 && usercopy::read::<[i64; 2]>(tv).is_ok_and(|t| t == [0, 0]);
    let result = do_select(nfds, [r, w, e], deadline, None);
    finish(tv, true, deadline, result, zero)
}

fn pselect6(nfds: u64, r: u64, w: u64, e: u64, ts: u64, sig: u64) -> Result<i64, i64> {
    let deadline = deadline_of(ts, false)?;
    let zero = ts != 0 && usercopy::read::<[i64; 2]>(ts).is_ok_and(|t| t == [0, 0]);
    // { const sigset_t *ss; size_t ss_len }.
    let mask = match sig {
        0 => None,
        at => {
            let [ptr, size]: [u64; 2] = usercopy::read(at)?;
            read_mask(ptr, size)?
        }
    };
    let result = do_select(nfds, [r, w, e], deadline, mask);
    finish(ts, false, deadline, result, zero)
}

/// restart_syscall(2): the call the thread was in when a signal without a
/// handler came (poll), with what it kept; EINTR for nothing to restart.
fn restart_syscall() -> Result<i64, i64> {
    let [kind, a, b, deadline] = crate::thread::restart();
    crate::thread::set_restart([crate::thread::RESTART_NONE, 0, 0, 0]);
    match kind {
        crate::thread::RESTART_POLL => poll_until(a, b, (deadline != u64::MAX).then_some(deadline)),
        _ => Err(EINTR),
    }
}

const SYS_POLL: u64 = 7;
const SYS_SELECT: u64 = 23;
const SYS_PSELECT6: u64 = 270;
const SYS_PPOLL: u64 = 271;

/// The result of a poll, select or restart_syscall in `s`, or None.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3, a4, a5) = (s.rdi, s.rsi, s.rdx, s.r10, s.r8, s.r9);
    let result = match s.rax {
        SYS_POLL => poll(a0, a1, a2 as i32),
        SYS_PPOLL => ppoll(a0, a1, a2, a3, a4),
        SYS_SELECT => select(a0, a1, a2, a3, a4),
        SYS_PSELECT6 => pselect6(a0, a1, a2, a3, a4, a5),
        SYS_RESTART_SYSCALL => restart_syscall(),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}
