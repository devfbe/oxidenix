//! futex(2): sleeping on a user-space word, the base of every pthread
//! mutex, condition variable and join.
//!
//! A futex is identified by a key: for private futexes (and futexes in
//! private memory) the address space and the address, for futexes in shared
//! memory (shared mappings) the file's page cache and the offset in the file, so that processes mapping
//! it at different addresses meet (and a copy-on-write break of a private
//! page does not change the key).
//!
//! The Linux server's own memory (its shared region, see `linux`) has a
//! third kind of key: the server instance and the address. That memory is
//! pinned and in no address space's areas; the kernel reads its words
//! directly (`server_wait`, `server_wake`). A memory object mapped into
//! that region (a channel, see `channel`) keeps the object's key, so the
//! server and a service mapping the object meet on it (`object_wait`).
//!
//! An object can be hung up (`PageCache::hang_up`, a channel whose peer is
//! gone): waits on it fail with EPIPE, checked under the bucket lock, and
//! `wake_object` wakes every waiter on it, so none sleeps on after.
//!
//! Besides tasks, a bucket holds doorbell watches (`object_watch`): a
//! service's event loop sleeps in `ipc_receive`, not on one word, so a
//! wake of a watched word marks the service's doorbell pending and wakes
//! its `ipc_receive` instead of a task. A watch is one-shot, compares the
//! word like a wait, and goes with the wake (or the object's hang-up).
//!
//! Waiters sit in hashed buckets. A waiter compares the futex word under
//! its bucket lock and enqueues itself before releasing it; a waker changes
//! the word in user space first and then takes the same lock, so a wakeup
//! is never lost. The word is read with a load that never resolves page
//! faults (a spinlock is held); if the page is not there, the waiter drops
//! the lock, faults it in and tries again.

use super::address_space::Backing;
use super::errno::*;
use crate::fs::cache::PageCache;
use super::sched::{current, current_arc, prepare_to_sleep, try_wake};
use super::task::{State, Task};
use super::{signal, uaccess, Pid};
use crate::sync::IrqSpinLock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Base {
    /// An address space (by its address): private futexes.
    Mm(usize),
    /// A shared memory object (by its address).
    Shared(usize),
    /// A Linux server instance's memory (by the instance's address).
    Server(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    base: Base,
    offset: u64,
}

struct Waiter {
    key: Key,
    sleeper: Sleeper,
    bitset: u32,
}

enum Sleeper {
    Task(Arc<Task>),
    /// A doorbell watch of a server's process `Pid` (`object_watch`): its
    /// server's doorbell flag (only the flag: dropping it under the bucket
    /// lock frees nothing else).
    Watch(Arc<AtomicBool>, Pid),
}

impl Waiter {
    fn is_task(&self, t: &Task) -> bool {
        matches!(&self.sleeper, Sleeper::Task(w) if core::ptr::eq(&**w, t))
    }

    /// Wakes the task, or rings the watching service's doorbell.
    fn wake(self) {
        match self.sleeper {
            Sleeper::Task(task) => {
                task.futex_woken.store(true, Ordering::Release);
                try_wake(&task, State::Sleeping);
            }
            Sleeper::Watch(doorbell, pid) => {
                doorbell.store(true, Ordering::Release);
                super::wakeup(super::irq::server_chan(pid));
            }
        }
    }
}

const BUCKETS: usize = 256;
/// Room a bucket keeps when it empties; a larger one (a burst of waiters)
/// is given back then, so the buckets hold at most this much each beyond
/// what is waiting now.
const KEEP: usize = 4;

/// After waiters left `b`: an empty bucket gives back more room than
/// `KEEP` (no allocation: the vector goes, the next waiter makes one).
fn tidy(b: &mut Vec<Waiter>) {
    if b.is_empty() && b.capacity() > KEEP {
        *b = Vec::new();
    }
}

static BUCKETS_: [IrqSpinLock<Vec<Waiter>>; BUCKETS] = [const { IrqSpinLock::new(Vec::new()) }; BUCKETS];

fn bucket_of(key: &Key) -> usize {
    let base = match key.base {
        Base::Mm(a) | Base::Shared(a) | Base::Server(a) => a as u64,
    };
    let h = (base ^ key.offset.rotate_left(17)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (h >> 56) as usize % BUCKETS
}

/// The key of the futex word at `uaddr` for the running task, and the
/// shared object it lies in.
fn key_of(uaddr: u64, private: bool) -> Result<(Key, Option<Arc<PageCache>>), i64> {
    if uaddr % 4 != 0 {
        return Err(EINVAL);
    }
    let mm = super::current_mm().ok_or(EFAULT)?;
    let mm_key = Key { base: Base::Mm(Arc::as_ptr(&mm) as usize), offset: uaddr };
    if private {
        return Ok((mm_key, None));
    }
    let space = mm.lock();
    let v = space.vma(uaddr).ok_or(EFAULT)?;
    Ok(match &v.backing {
        Backing::File { cache, offset, shared: true, .. } => {
            (Key { base: Base::Shared(Arc::as_ptr(cache) as usize), offset: offset + (uaddr - v.start) }, Some(cache.clone()))
        }
        _ => (mm_key, None),
    })
}

/// Deadline (nanoseconds of the monotonic clock) for a futex timeout at
/// `ts` (a timespec): an interval for FUTEX_WAIT, an absolute time
/// (monotonic, or the realtime clock) for FUTEX_WAIT_BITSET.
fn deadline(ts: u64, absolute: bool, realtime: bool) -> Result<Option<u64>, i64> {
    if ts == 0 {
        return Ok(None);
    }
    let t = super::sys_time::read_timespec(ts)?;
    let now = crate::time::now();
    Ok(Some(if !absolute {
        now.saturating_add(t)
    } else if realtime {
        now.saturating_add(t.saturating_sub(crate::time::realtime()))
    } else {
        t
    }))
}

fn wait(uaddr: u64, val: u32, deadline: Option<u64>, bitset: u32, private: bool) -> Result<i64, i64> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    let (key, object) = key_of(uaddr, private)?;
    let hung_up = || object.as_ref().is_some_and(|o| o.is_hung_up());
    // Not readable without a fault: fault it in (not under the bucket's
    // lock), then try again.
    wait_on(key, || uaccess::read_u32_atomic(uaddr), || uaccess::read::<u32>(uaddr).map(|_| ()), hung_up, val, deadline, bitset, Ends::Interrupted)
}

/// What ends a futex wait besides a wake, its word changing and its deadline.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ends {
    /// A signal (for a Linux program's thread: a kick), and the thread's death.
    Interrupted,
    /// Only the thread's death.
    Dying,
    /// Nothing more: a lock of the Linux server's in its own memory
    /// (`restricted::FUTEX_LOCK`), held for bounded work only (as a kernel's
    /// spinlock or a mutex that is not killable), which a dying thread's
    /// server still takes to end the thread.
    Lock,
}

impl Ends {
    /// Whether the wait ends now.
    fn now(self) -> bool {
        match self {
            Ends::Interrupted => signal::interrupted(),
            Ends::Dying => signal::dying(),
            Ends::Lock => false,
        }
    }
}

/// Sleeps on `wait` until woken or `deadline`.
fn sleep_on(wait: super::sched::Wait, deadline: Option<u64>) {
    match deadline {
        Some(d) => wait.sleep_until(d),
        None => wait.sleep(),
    }
}

/// Waits on `key` while the word is `val`: `peek` reads it without
/// faulting (None if it cannot), `fault_in` makes it readable. `ends` says
/// what else ends the wait (EINTR). EPIPE if
/// `hung_up` (the word's object was hung up; checked under the bucket
/// lock, which `wake_object` takes after hanging it up).
#[allow(clippy::too_many_arguments)]
fn wait_on(
    key: Key,
    peek: impl Fn() -> Option<u32>,
    fault_in: impl Fn() -> Result<(), i64>,
    hung_up: impl Fn() -> bool,
    val: u32,
    deadline: Option<u64>,
    bitset: u32,
    ends: Ends,
) -> Result<i64, i64> {
    let me = current();
    let index = bucket_of(&key);
    let mut wait = loop {
        let wait = prepare_to_sleep();
        let mut b = BUCKETS_[index].lock();
        if hung_up() {
            return Err(EPIPE);
        }
        match peek() {
            Some(v) if v != val => return Err(EAGAIN),
            Some(_) => {
                b.try_reserve(1).map_err(|_| ENOMEM)?;
                me.futex_woken.store(false, Ordering::Relaxed);
                me.futex_bucket.store(index, Ordering::Relaxed);
                b.push(Waiter { key, sleeper: Sleeper::Task(current_arc()), bitset });
                break wait;
            }
            None => {
                drop(b);
                drop(wait);
                fault_in()?;
            }
        }
    };
    let result = loop {
        if me.futex_woken.load(Ordering::Acquire) {
            break Ok(0);
        }
        if deadline.is_some_and(|d| crate::time::now() >= d) {
            break Err(ETIMEDOUT);
        }
        if ends.now() {
            break Err(EINTR);
        }
        sleep_on(wait, deadline);
        wait = prepare_to_sleep();
    };
    drop(wait);
    match result {
        Ok(n) => Ok(n),
        // Leave the queue, unless a wakeup came first after all.
        Err(e) => {
            if unqueue(me) {
                Err(e)
            } else {
                Ok(0)
            }
        }
    }
}

/// Removes the current task's waiter; false if a waker took it already.
fn unqueue(me: &Task) -> bool {
    loop {
        // A requeue may move the waiter between buckets; it updates
        // `futex_bucket` with both locked, so this converges.
        let index = me.futex_bucket.load(Ordering::Acquire);
        let mut b = BUCKETS_[index].lock();
        if me.futex_woken.load(Ordering::Acquire) {
            return false;
        }
        if let Some(i) = b.iter().position(|w| w.is_task(me)) {
            b.swap_remove(i);
            tidy(&mut b);
            return true;
        }
    }
}

/// Whether a waiter is a task some other wake already woke: a word of a
/// `server_waitv` whose task left through another word (it removes such
/// entries itself as it returns). A task's own entries are otherwise never
/// in a bucket once it was woken: the flag is cleared before it enqueues.
fn stale(w: &Waiter) -> bool {
    matches!(&w.sleeper, Sleeper::Task(t) if t.futex_woken.load(Ordering::Acquire))
}

/// Wakes waiters of `bucket` with `key` (and a bitset bit in common), at
/// most `n`; returns how many. Stale entries of a vectored wait go without
/// counting, so they never take a wake another waiter needs.
fn wake_in(b: &mut Vec<Waiter>, key: &Key, n: u64, bitset: u32) -> u64 {
    let mut woken = 0;
    let mut i = 0;
    while i < b.len() && woken < n {
        if b[i].key == *key && stale(&b[i]) {
            b.remove(i);
            continue;
        }
        if b[i].key == *key && b[i].bitset & bitset != 0 {
            b.remove(i).wake();
            woken += 1;
        } else {
            i += 1;
        }
    }
    tidy(b);
    woken
}

fn wake(uaddr: u64, n: u64, bitset: u32, private: bool) -> Result<i64, i64> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    let (key, _) = key_of(uaddr, private)?;
    let mut b = BUCKETS_[bucket_of(&key)].lock();
    Ok(wake_in(&mut b, &key, n, bitset) as i64)
}

/// Wakes up to `n_wake` waiters of `uaddr` and moves up to `n_move` more
/// to `uaddr2`; with `cmp`, only if the word still holds that value.
fn requeue(uaddr: u64, n_wake: u64, n_move: u64, uaddr2: u64, cmp: Option<u32>, private: bool) -> Result<i64, i64> {
    let (from, to) = (key_of(uaddr, private)?.0, key_of(uaddr2, private)?.0);
    let (i1, i2) = (bucket_of(&from), bucket_of(&to));
    loop {
        // Both buckets, in index order (once if they are the same).
        let mut first = BUCKETS_[i1.min(i2)].lock();
        let mut second = (i1 != i2).then(|| BUCKETS_[i1.max(i2)].lock());
        if let Some(expected) = cmp {
            match uaccess::read_u32_atomic(uaddr) {
                Some(v) if v != expected => return Err(EAGAIN),
                Some(_) => {}
                None => {
                    drop(second);
                    drop(first);
                    uaccess::read::<u32>(uaddr)?;
                    continue;
                }
            }
        }
        let (src, mut dst): (&mut Vec<Waiter>, Option<&mut Vec<Waiter>>) = match second.as_mut() {
            None => (&mut *first, None),
            Some(s) if i1 < i2 => (&mut *first, Some(&mut **s)),
            Some(s) => (&mut **s, Some(&mut *first)),
        };
        // Room first, so that nothing fails halfway. Only tasks move: a
        // doorbell watch stays on its channel's word, whose hang-up is
        // what removes it for good (moved elsewhere, it would outlive the
        // channel and escape `object_watch`'s one-per-word rule).
        let moves = |w: &Waiter| w.key == from && matches!(w.sleeper, Sleeper::Task(_));
        let matching = src.iter().filter(|w| moves(w)).count() as u64;
        let movable = matching.saturating_sub(n_wake).min(n_move);
        if let Some(d) = dst.as_mut() {
            d.try_reserve(movable as usize).map_err(|_| ENOMEM)?;
        }
        let woken = wake_in(src, &from, n_wake, FUTEX_BITSET_MATCH_ANY);
        let mut moved = 0;
        let mut i = 0;
        while i < src.len() && moved < movable {
            if !moves(&src[i]) {
                i += 1;
                continue;
            }
            src[i].key = to;
            match dst.as_mut() {
                None => i += 1,
                Some(d) => {
                    let w = src.remove(i);
                    if let Sleeper::Task(task) = &w.sleeper {
                        task.futex_bucket.store(i2, Ordering::Release);
                    }
                    d.push(w);
                }
            }
            moved += 1;
        }
        tidy(src);
        return Ok((woken + moved) as i64);
    }
}

/// Wakes one waiter of the (shared-keyed) futex at `uaddr`: the join of a
/// thread that exits (CLONE_CHILD_CLEARTID).
/// Waits on the word at `addr` of the Linux server's memory (instance
/// `instance`), read through `word`, while it holds `val`.
pub fn server_wait(instance: usize, addr: u64, word: &core::sync::atomic::AtomicU32, val: u32, deadline: Option<u64>, ends: Ends) -> Result<i64, i64> {
    let key = Key { base: Base::Server(instance), offset: addr };
    wait_on(key, || Some(word.load(Ordering::SeqCst)), || Ok(()), || false, val, deadline, FUTEX_BITSET_MATCH_ANY, ends)
}

/// One word of a `server_waitv`: where it is (the Linux server's memory, or
/// an object mapped into it, which the entry keeps), and the value the
/// caller expects it to hold.
pub struct WaitWord<'a> {
    key: Key,
    word: &'a core::sync::atomic::AtomicU32,
    val: u32,
    object: Option<Arc<PageCache>>,
}

impl<'a> WaitWord<'a> {
    /// The word at `addr` of instance `instance`'s memory, read through `word`.
    pub fn server(instance: usize, addr: u64, word: &'a core::sync::atomic::AtomicU32, val: u32) -> WaitWord<'a> {
        WaitWord { key: Key { base: Base::Server(instance), offset: addr }, word, val, object: None }
    }

    /// The word at `offset` of `object` (mapped into the server's region),
    /// read through `word` (the object's page, which `object` keeps).
    pub fn object(object: Arc<PageCache>, offset: u64, word: &'a core::sync::atomic::AtomicU32, val: u32) -> WaitWord<'a> {
        let key = Key { base: Base::Shared(Arc::as_ptr(&object) as usize), offset };
        WaitWord { key, word, val, object: Some(object) }
    }
}

/// Waits until any of `words` is woken, while each holds its value (EAGAIN
/// at once if one does not), until `deadline` or, if `interruptible`, a
/// signal (a fatal one always): the Linux server's wait for any of several
/// events (`restricted::SYS_SERVER_WAIT`: poll, select and epoll_wait wait
/// on their own word and on the words netd wakes). EPIPE if a word's object
/// was hung up.
///
/// The task sits in the bucket of every word at once. The first wake takes
/// its entry there and marks the task woken; the others are stale from then
/// on: a wake that meets one drops it without counting it (`wake_in`), and
/// the task removes what is left before it returns.
pub fn server_waitv(words: &[WaitWord], deadline: Option<u64>, ends: Ends) -> Result<i64, i64> {
    let me = current();
    let mut wait = prepare_to_sleep();
    me.futex_woken.store(false, Ordering::Release);
    let mut queued = 0;
    let mut early = None;
    for w in words {
        let mut b = BUCKETS_[bucket_of(&w.key)].lock();
        if w.object.as_ref().is_some_and(|o| o.is_hung_up()) {
            early = Some(EPIPE);
            break;
        }
        if w.word.load(Ordering::SeqCst) != w.val {
            early = Some(EAGAIN);
            break;
        }
        if b.try_reserve(1).is_err() {
            early = Some(ENOMEM);
            break;
        }
        b.push(Waiter { key: w.key, sleeper: Sleeper::Task(current_arc()), bitset: FUTEX_BITSET_MATCH_ANY });
        queued += 1;
    }
    let result = match early {
        Some(e) => Err(e),
        None => loop {
            if me.futex_woken.load(Ordering::Acquire) {
                break Ok(0);
            }
            if deadline.is_some_and(|d| crate::time::now() >= d) {
                break Err(ETIMEDOUT);
            }
            if ends.now() {
                break Err(EINTR);
            }
            sleep_on(wait, deadline);
            wait = prepare_to_sleep();
        },
    };
    drop(wait);
    // Leave every bucket the task is still in.
    for w in &words[..queued] {
        let mut b = BUCKETS_[bucket_of(&w.key)].lock();
        b.retain(|x| !(x.key == w.key && x.is_task(me)));
        tidy(&mut b);
    }
    // A wake that came before the wait gave up wins (its word changed).
    match result {
        Err(ETIMEDOUT | EINTR) if me.futex_woken.load(Ordering::Acquire) => Ok(0),
        r => r,
    }
}

/// Waits on the word at `offset` of the memory object `object`, read
/// through `word` (the object's page, which the caller keeps), while it
/// holds `val`: the Linux server's wait on an object mapped into its
/// region, meeting a service's futex on its own mapping of the object.
pub fn object_wait(
    object: &Arc<PageCache>,
    offset: u64,
    word: &core::sync::atomic::AtomicU32,
    val: u32,
    deadline: Option<u64>,
    ends: Ends,
) -> Result<i64, i64> {
    let key = Key { base: Base::Shared(Arc::as_ptr(object) as usize), offset };
    let hung_up = || object.is_hung_up();
    wait_on(key, || Some(word.load(Ordering::SeqCst)), || Ok(()), hung_up, val, deadline, FUTEX_BITSET_MATCH_ANY, ends)
}

/// Arms a doorbell watch of the server process `pid` on the word at
/// `offset` of `object` (read through `word`, the object's page, which the
/// caller keeps), if it holds `val`: the next wake of the word (or the
/// object's hang-up) sets `doorbell` (its server's) and wakes the
/// process's `ipc_receive`. EAGAIN if the word holds another value, EPIPE
/// if the object was hung up. A watch that process armed on the word
/// before is kept, not doubled, and requeues leave watches where they are:
/// at most one per word and process exists, and it goes with the wake or
/// the object's hang-up (`wake_object`), which every teardown of a
/// channel does before its memory can go. So the watches a service holds
/// are bounded by the channel words it may watch (`channel::watch`: one
/// per channel attached to it).
pub fn object_watch(
    object: &Arc<PageCache>,
    offset: u64,
    word: &core::sync::atomic::AtomicU32,
    val: u32,
    doorbell: &Arc<AtomicBool>,
    pid: Pid,
) -> Result<i64, i64> {
    let key = Key { base: Base::Shared(Arc::as_ptr(object) as usize), offset };
    let mut b = BUCKETS_[bucket_of(&key)].lock();
    if object.is_hung_up() {
        return Err(EPIPE);
    }
    if word.load(Ordering::SeqCst) != val {
        return Err(EAGAIN);
    }
    let armed = |w: &Waiter| w.key == key && matches!(&w.sleeper, Sleeper::Watch(_, p) if *p == pid);
    if !b.iter().any(armed) {
        b.try_reserve(1).map_err(|_| ENOMEM)?;
        b.push(Waiter { key, sleeper: Sleeper::Watch(doorbell.clone(), pid), bitset: FUTEX_BITSET_MATCH_ANY });
    }
    Ok(0)
}

/// Wakes up to `n` waiters on the word at `offset` of `object`.
pub fn object_wake(object: &PageCache, offset: u64, n: u64) -> i64 {
    let key = Key { base: Base::Shared(object as *const PageCache as usize), offset };
    let mut b = BUCKETS_[bucket_of(&key)].lock();
    wake_in(&mut b, &key, n, FUTEX_BITSET_MATCH_ANY) as i64
}

/// Wakes every waiter on any word of `object` (after `hang_up`). It looks
/// through every bucket: O(buckets + waiters), each waiter a sleeping task
/// or one watch per attached channel, so no caller can inflate it beyond
/// what it is charged for; a channel runs it at most once per end.
pub fn wake_object(object: &PageCache) {
    let base = Base::Shared(object as *const PageCache as usize);
    for bucket in BUCKETS_.iter() {
        let mut b = bucket.lock();
        let mut i = 0;
        while i < b.len() {
            if b[i].key.base == base {
                b.swap_remove(i).wake();
            } else {
                i += 1;
            }
        }
        tidy(&mut b);
    }
}

/// Wakes up to `n` waiters on `addr` of the Linux server's memory.
pub fn server_wake(instance: usize, addr: u64, n: u64) -> i64 {
    let key = Key { base: Base::Server(instance), offset: addr };
    let mut b = BUCKETS_[bucket_of(&key)].lock();
    wake_in(&mut b, &key, n, FUTEX_BITSET_MATCH_ANY) as i64
}

pub fn wake_one(uaddr: u64) -> Result<i64, i64> {
    wake(uaddr, 1, FUTEX_BITSET_MATCH_ANY, false)
}

/// futex(uaddr, op, val, timeout/val2, uaddr2, val3).
pub fn futex(uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> SysResult {
    let private = op & FUTEX_PRIVATE_FLAG != 0;
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    if realtime && !matches!(cmd, FUTEX_WAIT | FUTEX_WAIT_BITSET) {
        return Err(ENOSYS);
    }
    let count = |n: u64| (n as u32 as i32).max(0) as u64;
    match cmd {
        FUTEX_WAIT => wait(uaddr, val as u32, deadline(timeout, false, false)?, FUTEX_BITSET_MATCH_ANY, private),
        FUTEX_WAIT_BITSET => wait(uaddr, val as u32, deadline(timeout, true, realtime)?, val3 as u32, private),
        FUTEX_WAKE => wake(uaddr, count(val), FUTEX_BITSET_MATCH_ANY, private),
        FUTEX_WAKE_BITSET => wake(uaddr, count(val), val3 as u32, private),
        // For the requeue operations the timeout argument is a count.
        FUTEX_REQUEUE => requeue(uaddr, count(val), count(timeout), uaddr2, None, private),
        FUTEX_CMP_REQUEUE => requeue(uaddr, count(val), count(timeout), uaddr2, Some(val3 as u32), private),
        // FUTEX_WAKE_OP and the priority-inheritance operations.
        _ => Err(ENOSYS),
    }
}
