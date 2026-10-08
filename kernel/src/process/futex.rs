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
use super::{signal, uaccess};
use crate::sync::IrqSpinLock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

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
    task: Arc<Task>,
    bitset: u32,
}

const BUCKETS: usize = 256;
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
    wait_on(key, || uaccess::read_u32_atomic(uaddr), || uaccess::read::<u32>(uaddr).map(|_| ()), hung_up, val, deadline, bitset, true)
}

/// Waits on `key` while the word is `val`: `peek` reads it without
/// faulting (None if it cannot), `fault_in` makes it readable. A signal
/// ends the wait if `interruptible`, a fatal one always. EPIPE if
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
    interruptible: bool,
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
                b.push(Waiter { key, task: current_arc(), bitset });
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
        if if interruptible { signal::interrupted() } else { signal::dying() } {
            break Err(EINTR);
        }
        match deadline {
            Some(d) => wait.sleep_until(d),
            None => wait.sleep(),
        }
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
        if let Some(i) = b.iter().position(|w| core::ptr::eq(&*w.task, me)) {
            b.swap_remove(i);
            return true;
        }
    }
}

/// Wakes waiters of `bucket` with `key` (and a bitset bit in common), at
/// most `n`; returns how many.
fn wake_in(b: &mut Vec<Waiter>, key: &Key, n: u64, bitset: u32) -> u64 {
    let mut woken = 0;
    let mut i = 0;
    while i < b.len() && woken < n {
        if b[i].key == *key && b[i].bitset & bitset != 0 {
            let w = b.remove(i);
            w.task.futex_woken.store(true, Ordering::Release);
            try_wake(&w.task, State::Sleeping);
            woken += 1;
        } else {
            i += 1;
        }
    }
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
        // Room first, so that nothing fails halfway.
        let matching = src.iter().filter(|w| w.key == from).count() as u64;
        let movable = matching.saturating_sub(n_wake).min(n_move);
        if let Some(d) = dst.as_mut() {
            d.try_reserve(movable as usize).map_err(|_| ENOMEM)?;
        }
        let woken = wake_in(src, &from, n_wake, FUTEX_BITSET_MATCH_ANY);
        let mut moved = 0;
        let mut i = 0;
        while i < src.len() && moved < movable {
            if src[i].key != from {
                i += 1;
                continue;
            }
            src[i].key = to;
            match dst.as_mut() {
                None => i += 1,
                Some(d) => {
                    let w = src.remove(i);
                    w.task.futex_bucket.store(i2, Ordering::Release);
                    d.push(w);
                }
            }
            moved += 1;
        }
        return Ok((woken + moved) as i64);
    }
}

/// Wakes one waiter of the (shared-keyed) futex at `uaddr`: the join of a
/// thread that exits (CLONE_CHILD_CLEARTID).
/// Waits on the word at `addr` of the Linux server's memory (instance
/// `instance`), read through `word`, while it holds `val`.
pub fn server_wait(instance: usize, addr: u64, word: &core::sync::atomic::AtomicU32, val: u32, deadline: Option<u64>, interruptible: bool) -> Result<i64, i64> {
    let key = Key { base: Base::Server(instance), offset: addr };
    wait_on(key, || Some(word.load(Ordering::SeqCst)), || Ok(()), || false, val, deadline, FUTEX_BITSET_MATCH_ANY, interruptible)
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
    interruptible: bool,
) -> Result<i64, i64> {
    let key = Key { base: Base::Shared(Arc::as_ptr(object) as usize), offset };
    let hung_up = || object.is_hung_up();
    wait_on(key, || Some(word.load(Ordering::SeqCst)), || Ok(()), hung_up, val, deadline, FUTEX_BITSET_MATCH_ANY, interruptible)
}

/// Wakes up to `n` waiters on the word at `offset` of `object`.
pub fn object_wake(object: &PageCache, offset: u64, n: u64) -> i64 {
    let key = Key { base: Base::Shared(object as *const PageCache as usize), offset };
    let mut b = BUCKETS_[bucket_of(&key)].lock();
    wake_in(&mut b, &key, n, FUTEX_BITSET_MATCH_ANY) as i64
}

/// Wakes every waiter on any word of `object` (after `hang_up`).
pub fn wake_object(object: &PageCache) {
    let base = Base::Shared(object as *const PageCache as usize);
    for bucket in BUCKETS_.iter() {
        let mut b = bucket.lock();
        let mut i = 0;
        while i < b.len() {
            if b[i].key.base == base {
                let w = b.swap_remove(i);
                w.task.futex_woken.store(true, Ordering::Release);
                try_wake(&w.task, State::Sleeping);
            } else {
                i += 1;
            }
        }
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
