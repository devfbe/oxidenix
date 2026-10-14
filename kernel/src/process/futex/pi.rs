//! Priority-inheritance locks on futex words: the mechanism under Linux's
//! PI futexes (FUTEX_LOCK_PI, FUTEX_UNLOCK_PI, FUTEX_TRYLOCK_PI,
//! FUTEX_WAIT_REQUEUE_PI, FUTEX_CMP_REQUEUE_PI), whose decoding, timeouts
//! and restarts are the Linux server's (`restricted::SYS_FUTEX_LOCK_PI` and
//! the calls after it).
//!
//! A PI lock is a word of the program's memory in Linux's format: the
//! owner's id (`PI_ID_MASK`, the Linux tid, which the kernel only compares
//! and copies: ids are the server's), `PI_WAITERS` while threads wait in the
//! kernel, `PI_OWNER_DIED` after an owner's death. A free lock is taken and
//! an uncontended one let go of in user space; the kernel sees a lock only
//! while somebody waits for it. Then it keeps a record (`Lock`: the key of
//! the word, as every futex is keyed, and its owner's task) and the waiters
//! (`PiWaiter`), both in one table under one lock (`PI`), which the word's
//! changes are made under too (a compare-and-swap that never resolves a
//! fault there; a fault is resolved with the lock released and the step
//! tried again). The first waiter names the owner from the word: the Linux
//! server looks the id up in its thread table and passes the task's key
//! (`Owner`); if the word names another owner by the time the kernel looks
//! under the lock, the server looks again (`PI_RETRY`).
//!
//! Inheritance. The fair scheduler's priority is a task's weight (its nice
//! value's): the owner of a lock runs with at least the largest weight of
//! its waiters, and theirs counts what they inherit in turn from the locks
//! they own, so a whole chain of owners runs with the weight of the most
//! important thread blocked at its end (`Task::pi_weight`,
//! `sched::effective_weight`). Every change of the waiters or owners, and a
//! waiter's nice value, recomputes the owners up the chain as far as one
//! changes (`Table::reweigh`), bounded by the task limit. A wait that would
//! close a cycle of owners and waiters is refused (EDEADLK), as Linux's
//! full chain walk does.
//!
//! An unlock with waiters hands the lock to the waiter of the largest
//! weight (the longest waiting of equals): it writes that waiter's id into
//! the word and wakes it owning the lock, so nobody can take the lock in
//! between. An owner whose task ends (or execs, `release_all`) hands each
//! lock it owns the same way, but the word stays to the new owner to fix
//! in its own address space (a shared lock's dead owner may have had
//! another): it writes its id with `PI_OWNER_DIED` before its call returns
//! (`Lock::fixup` keeps the record meanwhile, so the word's stale owner
//! misleads nobody).
//!
//! Requeueing. A waiter of `futex_wait_requeue_pi` waits on a plain word
//! (its bucket's waiter carries the lock it is meant for,
//! `Waiter::requeue_pi`); `cmp_requeue_pi` takes the lock for the first such
//! waiter if it is free and moves the others into this table, under the
//! plain word's bucket lock and then `PI` (the order every path takes).
//!
//! Cost. One lock for all PI locks of the machine, as FreeBSD's umtx PI
//! chains have: PI operations of unrelated programs serialize on it, for
//! work linear in the waiters (at most one per task). PI locks are
//! contended in the kernel only (their fast paths are user space's), and
//! only programs asking for PTHREAD_PRIO_INHERIT use them; Linux's
//! per-lock wait locks with per-task PI locks, whose chain walk drops and
//! retakes locks at every step, would buy scalability across many CPUs at
//! the price of that walk's subtlety. The tables are reserved once, for
//! as many entries as there can be tasks, so no PI operation allocates
//! under the lock.

use super::super::errno::*;
use super::super::sched::{self, current, current_arc, prepare_to_sleep, try_wake, MAX_PROCS};
use super::super::task::{State as TaskState, Task};
use super::super::uaccess;
use super::{bucket_of, key_of, make_room, next_seq, sleep_on, spare_for, tidy, Ends, Key, Sleeper, Waiter, BUCKETS_, FUTEX_BITSET_MATCH_ANY};
use crate::sync::IrqSpinLock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::Ordering;
use restricted::{PI_ID_MASK, PI_OWNER_DIED, PI_RETRY, PI_WAITERS};

/// Where a task's wait for a PI lock stands (`Task::pi_wait`): not waiting
/// in this table (also on the plain word of a requeue wait), waiting here,
/// handed the lock (the word holds its id), handed it by an owner that died
/// (the word is its to fix), refused by a requeue (waiting would deadlock).
pub const NONE: u8 = 0;
pub const QUEUED: u8 = 1;
pub const GRANTED: u8 = 2;
pub const GRANTED_DIED: u8 = 3;
pub const DEADLOCK: u8 = 4;

/// The PI lock a requeue wait is meant for, and the waiter's id.
#[derive(Clone, Copy)]
pub struct Target {
    key: Key,
    id: u32,
}

/// The owner of a PI lock as the Linux server found it in the word: its id,
/// and its task (None: no thread of the instance has the id).
pub struct Owner {
    pub id: u32,
    pub task: Option<Arc<Task>>,
}

/// A PI lock somebody waits for (or whose new owner fixes its word).
struct Lock {
    key: Key,
    owner: Arc<Task>,
    /// The owner's id, which the word holds.
    id: u32,
    /// The owner died and handed the lock on: the new owner has not written
    /// its id into the word yet.
    fixup: bool,
}

struct PiWaiter {
    key: Key,
    task: Arc<Task>,
    id: u32,
    /// The order of arrival (`futex::SEQ`), among waiters of equal weight.
    seq: u64,
}

struct Table {
    locks: Vec<Lock>,
    waiters: Vec<PiWaiter>,
}

static PI: IrqSpinLock<Table> = IrqSpinLock::new(Table { locks: Vec::new(), waiters: Vec::new() });

/// The longest chain of owners and waiters there can be.
const DEPTH: usize = MAX_PROCS;

/// The task references an operation takes out of the table, dropped after
/// its lock (a last reference's drop takes other locks, see `futex::Sink`).
/// No operation takes out more than three.
type Grave = heapless::Vec<Arc<Task>, 4>;

fn bury(grave: &mut Grave, t: Arc<Task>) {
    // (Never full: see `Grave`.)
    let _ = grave.push(t);
}

/// Room for as many locks and waiters as there can be tasks, made once
/// with the lock released: a task waits for one lock at a time, and a lock
/// is recorded only while a task waits for it or its new owner fixes its
/// word, so the tables never grow beyond that.
fn reserve() -> Result<(), i64> {
    if PI.lock().locks.capacity() >= MAX_PROCS {
        return Ok(());
    }
    let mut locks = Vec::new();
    let mut waiters = Vec::new();
    locks.try_reserve_exact(MAX_PROCS).map_err(|_| ENOMEM)?;
    waiters.try_reserve_exact(MAX_PROCS).map_err(|_| ENOMEM)?;
    let mut t = PI.lock();
    if t.locks.capacity() < MAX_PROCS {
        // (Empty until now: only reserved tables take entries.)
        core::mem::swap(&mut t.locks, &mut locks);
        core::mem::swap(&mut t.waiters, &mut waiters);
    }
    Ok(())
}

impl Table {
    fn lock_of(&self, key: &Key) -> Option<usize> {
        self.locks.iter().position(|l| l.key == *key)
    }

    /// Where `t` waits in the table.
    fn waiting(&self, t: &Task) -> Option<usize> {
        self.waiters.iter().position(|w| ptr::eq(&*w.task, t))
    }

    fn has_waiters(&self, key: &Key) -> bool {
        self.waiters.iter().any(|w| w.key == *key)
    }

    /// The waiter of `key` the lock goes to next: the largest weight, the
    /// longest waiting of equals.
    fn top(&self, key: &Key) -> Option<usize> {
        self.waiters
            .iter()
            .enumerate()
            .filter(|(_, w)| w.key == *key)
            .max_by(|(_, a), (_, b)| sched::lent_weight(&a.task).cmp(&sched::lent_weight(&b.task)).then(b.seq.cmp(&a.seq)))
            .map(|(i, _)| i)
    }

    /// Whether the tables have room for one more lock and waiter (always:
    /// see `reserve`).
    fn room(&self) -> bool {
        self.locks.len() < self.locks.capacity() && self.waiters.len() < self.waiters.capacity()
    }

    /// The owner `start`'s inherited weight anew, and that of the owners up
    /// the chain of locks it waits for, as far as one changes.
    fn reweigh(&self, start: &Task) {
        let mut t = start;
        for _ in 0..DEPTH {
            let w = self
                .locks
                .iter()
                .filter(|l| ptr::eq(&*l.owner, t))
                .flat_map(|l| self.waiters.iter().filter(move |x| x.key == l.key))
                .map(|x| sched::lent_weight(&x.task))
                .max()
                .unwrap_or(0);
            if t.pi_weight.swap(w, Ordering::Relaxed) == w {
                return;
            }
            let Some(i) = self.waiting(t) else { return };
            let Some(l) = self.lock_of(&self.waiters[i].key) else { return };
            t = &self.locks[l].owner;
        }
    }

    /// Whether `waiter` waiting for a lock of `owner`'s would close a cycle
    /// (or a chain longer than there can be).
    fn deadlocks(&self, waiter: &Task, owner: &Task) -> bool {
        let mut t = owner;
        for _ in 0..DEPTH {
            if ptr::eq(t, waiter) {
                return true;
            }
            let Some(i) = self.waiting(t) else { return false };
            let Some(l) = self.lock_of(&self.waiters[i].key) else { return false };
            t = &self.locks[l].owner;
        }
        true
    }

    /// Whether the word `word` agrees with the record `l` (Linux's
    /// attach_to_pi_state): it names the owner, or the owner died (the
    /// record waits for the new owner's fix, or the dead one's robust list
    /// cleared the word and its end has not handed the lock on yet).
    fn agrees(&self, l: usize, word: u32) -> bool {
        let lock = &self.locks[l];
        lock.fixup || word & PI_ID_MASK == lock.id || (word & PI_OWNER_DIED != 0 && word & PI_ID_MASK == 0)
    }

    /// Removes the record `l` if nobody waits for it and its word is fixed.
    fn tidy_lock(&mut self, l: usize, grave: &mut Grave) {
        if !self.locks[l].fixup && !self.has_waiters(&self.locks[l].key) {
            let lock = self.locks.swap_remove(l);
            self.reweigh(&lock.owner);
            bury(grave, lock.owner);
        }
    }
}

/// What a compare-and-swap of a word under the lock found.
enum Swap {
    Done,
    /// The program changed the word meanwhile: look again.
    Changed,
    /// Not writable now: release the locks, fault it in, try again.
    Fault,
}

fn swap(uaddr: u64, old: u32, new: u32) -> Swap {
    match uaccess::cmpxchg_u32_atomic(uaddr, old, new) {
        Some(v) if v == old => Swap::Done,
        Some(_) => Swap::Changed,
        None => Swap::Fault,
    }
}

/// `restricted::SYS_FUTEX_LOCK_PI`: takes the PI lock at `uaddr` for the
/// caller, whose id is `id`; the word's owner as the server found it is
/// `owner`. With `try_only` it never waits (EAGAIN).
pub fn lock(uaddr: u64, id: u32, owner: Owner, deadline: Option<u64>, private: bool, try_only: bool) -> Result<i64, i64> {
    if id == 0 || id & !PI_ID_MASK != 0 {
        return Err(EINVAL);
    }
    let (key, _object) = key_of(uaddr, private)?;
    reserve()?;
    let me = current();
    let index = bucket_of(&key);
    // (Declared before the guards: dropped after them.)
    let mut grave = Grave::new();
    loop {
        let b = BUCKETS_[index].lock();
        let mut t = PI.lock();
        // A plain wait on the word: it is no PI lock (Linux: EINVAL).
        if b.iter().any(|w| w.key == key) {
            return Err(EINVAL);
        }
        let Some(word) = uaccess::read_u32_atomic(uaddr) else {
            drop(t);
            drop(b);
            uaccess::read::<u32>(uaddr)?;
            continue;
        };
        if word & PI_ID_MASK == id {
            return Err(EDEADLK);
        }
        let mark = |word: u32| if word & PI_WAITERS == 0 { swap(uaddr, word, word | PI_WAITERS) } else { Swap::Done };
        let lock_owner = match t.lock_of(&key) {
            Some(l) => {
                if !t.agrees(l, word) {
                    return Err(EINVAL);
                }
                if try_only {
                    return Err(EAGAIN);
                }
                match mark(word) {
                    Swap::Done => {}
                    Swap::Changed => continue,
                    Swap::Fault => {
                        drop(t);
                        drop(b);
                        uaccess::fault_in_writable(uaddr)?;
                        continue;
                    }
                }
                t.locks[l].owner.clone()
            }
            None if word & PI_ID_MASK == 0 => {
                // Free (its owner died, or the program's unlock is under
                // way): the caller takes it, keeping a dead owner's mark.
                match swap(uaddr, word, (word & PI_OWNER_DIED) | id) {
                    Swap::Done => return Ok(0),
                    Swap::Changed => continue,
                    Swap::Fault => {
                        drop(t);
                        drop(b);
                        uaccess::fault_in_writable(uaddr)?;
                        continue;
                    }
                }
            }
            None => {
                if word & PI_ID_MASK != owner.id {
                    return Err(PI_RETRY);
                }
                // The waiters' mark first, then the owner (no harm done if
                // there is none: an unlock goes through the kernel then).
                match mark(word) {
                    Swap::Done => {}
                    Swap::Changed => continue,
                    Swap::Fault => {
                        drop(t);
                        drop(b);
                        uaccess::fault_in_writable(uaddr)?;
                        continue;
                    }
                }
                let Some(o) = owner.task.as_ref().filter(|o| !o.pi_gone.load(Ordering::Acquire)) else { return Err(ESRCH) };
                if try_only {
                    return Err(EAGAIN);
                }
                if !t.room() {
                    return Err(ENOMEM);
                }
                t.locks.push(Lock { key, owner: o.clone(), id: owner.id, fixup: false });
                o.clone()
            }
        };
        if t.deadlocks(me, &lock_owner) || !t.room() {
            let deadlock = t.deadlocks(me, &lock_owner);
            if let Some(l) = t.lock_of(&key) {
                t.tidy_lock(l, &mut grave);
            }
            bury(&mut grave, lock_owner);
            return Err(if deadlock { EDEADLK } else { ENOMEM });
        }
        me.pi_wait.store(QUEUED, Ordering::Release);
        t.waiters.push(PiWaiter { key, task: current_arc(), id, seq: next_seq() });
        t.reweigh(&lock_owner);
        bury(&mut grave, lock_owner);
        break;
    }
    drop(grave);
    let phase = match sleep_for_lock(me, deadline) {
        Ok(phase) => phase,
        Err(e) => leave(me, e)?,
    };
    granted(uaddr, &key, id, phase)
}

/// The end of a wait that got the lock (`phase`): an owner's death leaves
/// the word to the caller to fix.
fn granted(uaddr: u64, key: &Key, id: u32, phase: u8) -> Result<i64, i64> {
    current().pi_wait.store(NONE, Ordering::Relaxed);
    match phase {
        GRANTED => Ok(0),
        GRANTED_DIED => fix_word(uaddr, key, id).map(|_| 0),
        _ => Err(EDEADLK),
    }
}

/// Sleeps until the caller is handed the lock (or refused), `deadline` or
/// a kick: its `pi_wait` then, or the error that ended the wait.
fn sleep_for_lock(me: &Task, deadline: Option<u64>) -> Result<u8, i64> {
    loop {
        let wait = prepare_to_sleep();
        let phase = me.pi_wait.load(Ordering::Acquire);
        if phase >= GRANTED {
            return Ok(phase);
        }
        if deadline.is_some_and(|d| crate::time::now() >= d) {
            return Err(ETIMEDOUT);
        }
        if Ends::Interrupted.now() {
            return Err(EINTR);
        }
        sleep_on(wait, deadline);
    }
}

/// The caller's wait in the table ends with `err` (its deadline, a kick):
/// it leaves the waiters (its owner's weight is computed anew), unless the
/// lock came to it first (then its phase).
fn leave(me: &Task, err: i64) -> Result<u8, i64> {
    let mut grave = Grave::new();
    let mut t = PI.lock();
    let phase = me.pi_wait.load(Ordering::Acquire);
    if phase >= GRANTED {
        return Ok(phase);
    }
    me.pi_wait.store(NONE, Ordering::Relaxed);
    if let Some(i) = t.waiting(me) {
        let w = t.waiters.swap_remove(i);
        if let Some(l) = t.lock_of(&w.key) {
            let owner = t.locks[l].owner.clone();
            t.tidy_lock(l, &mut grave);
            t.reweigh(&owner);
            bury(&mut grave, owner);
        }
        bury(&mut grave, w.task);
    }
    drop(t);
    drop(grave);
    Err(err)
}

/// The lock at `uaddr` came to the caller (`id`) from an owner that died:
/// it writes its id into the word, with `PI_WAITERS` and `PI_OWNER_DIED`
/// (Linux's fixup_pi_state_owner), and the record no longer waits for it.
/// EFAULT if the word cannot be written (the caller owns the lock anyway).
fn fix_word(uaddr: u64, key: &Key, id: u32) -> Result<(), i64> {
    let mut grave = Grave::new();
    let result = loop {
        let mut t = PI.lock();
        let Some(word) = uaccess::read_u32_atomic(uaddr) else {
            drop(t);
            if let Err(e) = uaccess::read::<u32>(uaddr) {
                break Err(e);
            }
            continue;
        };
        match swap(uaddr, word, PI_OWNER_DIED | PI_WAITERS | id) {
            Swap::Done => {}
            Swap::Changed => continue,
            Swap::Fault => {
                drop(t);
                if let Err(e) = uaccess::fault_in_writable(uaddr) {
                    break Err(e);
                }
                continue;
            }
        }
        if let Some(l) = t.lock_of(key) {
            t.locks[l].fixup = false;
            t.tidy_lock(l, &mut grave);
        }
        break Ok(());
    };
    if result.is_err() {
        let mut t = PI.lock();
        if let Some(l) = t.lock_of(key) {
            t.locks[l].fixup = false;
            t.tidy_lock(l, &mut grave);
        }
    }
    result
}

/// `restricted::SYS_FUTEX_UNLOCK_PI`: the caller (`id`) lets go of the PI
/// lock at `uaddr`: its top waiter gets it, or the word becomes 0.
pub fn unlock(uaddr: u64, id: u32, private: bool) -> Result<i64, i64> {
    let (key, _object) = key_of(uaddr, private)?;
    let me = current();
    let mut grave = Grave::new();
    loop {
        let mut t = PI.lock();
        let Some(word) = uaccess::read_u32_atomic(uaddr) else {
            drop(t);
            uaccess::read::<u32>(uaddr)?;
            continue;
        };
        if word & PI_ID_MASK != id || id == 0 {
            return Err(EPERM);
        }
        if let Some(l) = t.lock_of(&key) {
            if !ptr::eq(&*t.locks[l].owner, me) {
                return Err(EINVAL);
            }
            if let Some(i) = t.top(&key) {
                let next = t.waiters[i].id;
                match swap(uaddr, word, next | PI_WAITERS) {
                    Swap::Done => {}
                    Swap::Changed => continue,
                    Swap::Fault => {
                        drop(t);
                        uaccess::fault_in_writable(uaddr)?;
                        continue;
                    }
                }
                let w = t.waiters.swap_remove(i);
                let old = core::mem::replace(&mut t.locks[l].owner, w.task.clone());
                t.locks[l].id = next;
                w.task.pi_wait.store(GRANTED, Ordering::Release);
                try_wake(&w.task, TaskState::Sleeping);
                t.tidy_lock(l, &mut grave);
                t.reweigh(me);
                t.reweigh(&w.task);
                bury(&mut grave, old);
                bury(&mut grave, w.task);
                return Ok(0);
            }
            // (A record nobody waits for any more.)
            t.tidy_lock(l, &mut grave);
        }
        match swap(uaddr, word, 0) {
            Swap::Done => return Ok(0),
            Swap::Changed => continue,
            Swap::Fault => {
                drop(t);
                uaccess::fault_in_writable(uaddr)?;
            }
        }
    }
}

/// The task `task` ends (`gone`: no lock is attached to it from now on) or
/// execs: each PI lock it owns goes to its top waiter, which fixes the
/// word (`fix_word`; Linux's exit_pi_state_list), or its record goes.
pub fn release_all(task: &Task, gone: bool) {
    loop {
        let mut grave = Grave::new();
        let mut t = PI.lock();
        if gone {
            task.pi_gone.store(true, Ordering::Release);
        }
        let Some(l) = t.locks.iter().position(|lk| ptr::eq(&*lk.owner, task)) else {
            task.pi_weight.store(0, Ordering::Relaxed);
            return;
        };
        let key = t.locks[l].key;
        match t.top(&key) {
            Some(i) => {
                let w = t.waiters.swap_remove(i);
                let old = core::mem::replace(&mut t.locks[l].owner, w.task.clone());
                t.locks[l].id = w.id;
                t.locks[l].fixup = true;
                w.task.pi_wait.store(GRANTED_DIED, Ordering::Release);
                try_wake(&w.task, TaskState::Sleeping);
                t.reweigh(&w.task);
                bury(&mut grave, old);
                bury(&mut grave, w.task);
            }
            None => {
                let lock = t.locks.swap_remove(l);
                bury(&mut grave, lock.owner);
            }
        }
        drop(t);
        drop(grave);
    }
}

/// The nice value of `task` changed: if it waits for a PI lock, the owners
/// up its chain get their weights anew.
pub fn nice_changed(task: &Task) {
    let t = PI.lock();
    if let Some(i) = t.waiting(task) {
        if let Some(l) = t.lock_of(&t.waiters[i].key) {
            t.reweigh(&t.locks[l].owner);
        }
    }
}

/// `restricted::SYS_FUTEX_WAIT_REQUEUE_PI`: waits on `uaddr` while it
/// holds `val`, for `cmp_requeue_pi` to move the caller (`id`) to the PI
/// lock at `uaddr2`; returns once it owns that lock.
pub fn wait_requeue_pi(uaddr: u64, val: u32, deadline: Option<u64>, uaddr2: u64, id: u32, private: bool) -> Result<i64, i64> {
    if id == 0 || id & !PI_ID_MASK != 0 {
        return Err(EINVAL);
    }
    let (key, _object) = key_of(uaddr, private)?;
    let (key2, _object2) = key_of(uaddr2, private)?;
    if key == key2 {
        return Err(EINVAL);
    }
    reserve()?;
    let me = current();
    let index = bucket_of(&key);
    // (Declared before the guards: the old buffer goes after the lock.)
    let mut spare = Vec::new();
    let mut wait = loop {
        let wait = prepare_to_sleep();
        let mut b = BUCKETS_[index].lock();
        match uaccess::read_u32_atomic(uaddr) {
            Some(v) if v != val => return Err(EAGAIN),
            Some(_) => {
                if !make_room(&mut b, 1, &mut spare) {
                    let len = b.len();
                    drop(b);
                    drop(wait);
                    spare = spare_for(len, 1)?;
                    continue;
                }
                me.futex_woken.store(false, Ordering::Relaxed);
                me.futex_bucket.store(index, Ordering::Relaxed);
                me.pi_wait.store(NONE, Ordering::Relaxed);
                let target = Some(Target { key: key2, id });
                b.push(Waiter { key, sleeper: Sleeper::Task(current_arc()), bitset: FUTEX_BITSET_MATCH_ANY, seq: next_seq(), movable: false, requeue_pi: target });
                drop(b);
                drop(core::mem::take(&mut spare));
                break wait;
            }
            None => {
                drop(b);
                drop(wait);
                uaccess::read::<u32>(uaddr)?;
            }
        }
    };
    let result = loop {
        let phase = me.pi_wait.load(Ordering::Acquire);
        if phase >= GRANTED {
            break Ok(phase);
        }
        if deadline.is_some_and(|d| crate::time::now() >= d) {
            break Err(ETIMEDOUT);
        }
        if Ends::Interrupted.now() {
            break Err(EINTR);
        }
        sleep_on(wait, deadline);
        wait = prepare_to_sleep();
    };
    drop(wait);
    let phase = match result {
        Ok(phase) => phase,
        Err(e) => {
            let mut b = BUCKETS_[index].lock();
            let phase = me.pi_wait.load(Ordering::Acquire);
            if phase >= GRANTED {
                phase
            } else if let Some(i) = b.iter().position(|w| w.is_task(me)) {
                // Still on `uaddr`: the wait ends as a futex wait does. (Its
                // own reference: never the last.)
                b.swap_remove(i);
                tidy(&mut b);
                return Err(e);
            } else {
                drop(b);
                // Moved to the lock: a kick ends that wait with EAGAIN, as
                // Linux's (it cannot restart on `uaddr`).
                leave(me, if e == EINTR { EAGAIN } else { e })?
            }
        }
    };
    granted(uaddr2, &key2, id, phase)
}

/// `restricted::SYS_FUTEX_CMP_REQUEUE_PI`: if `uaddr` holds `val`, takes
/// the PI lock at `uaddr2` for the first waiter of `uaddr` if it is free,
/// and moves up to `n_move` more waiters (one more if none took it) to wait
/// for the lock (Linux's futex_requeue with requeue_pi); how many were
/// woken and moved. `owner` is the word's owner as the server found it.
pub fn cmp_requeue_pi(uaddr: u64, n_move: u64, uaddr2: u64, val: u32, owner: Owner, private: bool) -> Result<i64, i64> {
    let (from, _object) = key_of(uaddr, private)?;
    let (to, _object2) = key_of(uaddr2, private)?;
    if from == to {
        return Err(EINVAL);
    }
    reserve()?;
    let (i1, i2) = (bucket_of(&from), bucket_of(&to));
    // (Declared before the guards: dropped after them.)
    let mut grave = Grave::new();
    'retry: loop {
        // Both buckets in index order (once if they are the same), then the
        // table.
        let mut first = BUCKETS_[i1.min(i2)].lock();
        let mut second = (i1 != i2).then(|| BUCKETS_[i1.max(i2)].lock());
        let src: &mut Vec<Waiter> = match second.as_mut() {
            Some(s) if i1 > i2 => &mut **s,
            _ => &mut *first,
        };
        let mut t = PI.lock();
        // A word to make accessible with the locks released (its address,
        // and whether for writing) before the step is tried again.
        let fault: (u64, bool) = 'step: {
            match uaccess::read_u32_atomic(uaddr) {
                None => break 'step (uaddr, false),
                Some(v) if v != val => return Err(EAGAIN),
                Some(_) => {}
            }
            // The waiters of `uaddr` in the order they came.
            let first_of = |src: &Vec<Waiter>| src.iter().enumerate().filter(|(_, w)| w.key == from).min_by_key(|(_, w)| w.seq).map(|(i, _)| i);
            let Some(top) = first_of(src) else { return Ok(0) };
            let target = match src[top].requeue_pi {
                Some(target) if target.key == to => target,
                // A plain wait, or one meant for another lock (Linux: EINVAL).
                _ => return Err(EINVAL),
            };
            let Some(word) = uaccess::read_u32_atomic(uaddr2) else { break 'step (uaddr2, false) };
            if word & PI_ID_MASK == target.id {
                return Err(EDEADLK);
            }
            let mut woken = 0;
            // The owner the moved waiters wait for, and its id.
            let (lock_owner, lock_id) = match t.lock_of(&to) {
                Some(l) => {
                    if !t.agrees(l, word) {
                        return Err(EINVAL);
                    }
                    (t.locks[l].owner.clone(), t.locks[l].id)
                }
                None if word & PI_ID_MASK == 0 => {
                    // Free: the first waiter takes it and wakes owning it.
                    match swap(uaddr2, word, (word & PI_OWNER_DIED) | target.id | PI_WAITERS) {
                        Swap::Done => {}
                        Swap::Changed => continue 'retry,
                        Swap::Fault => break 'step (uaddr2, true),
                    }
                    let w = src.swap_remove(top);
                    let Sleeper::Task(task) = w.sleeper else { unreachable!("a requeue wait is a task's") };
                    task.pi_wait.store(GRANTED, Ordering::Release);
                    try_wake(&task, TaskState::Sleeping);
                    woken = 1;
                    (task, target.id)
                }
                None => {
                    if word & PI_ID_MASK != owner.id {
                        return Err(PI_RETRY);
                    }
                    if word & PI_WAITERS == 0 {
                        match swap(uaddr2, word, word | PI_WAITERS) {
                            Swap::Done => {}
                            Swap::Changed => continue 'retry,
                            Swap::Fault => break 'step (uaddr2, true),
                        }
                    }
                    let Some(o) = owner.task.as_ref().filter(|o| !o.pi_gone.load(Ordering::Acquire)) else { return Err(ESRCH) };
                    (o.clone(), owner.id)
                }
            };
            // Up to `n_move` more (one more if the lock was not taken).
            let limit = n_move.saturating_add(1 - woken);
            let mut moved = 0;
            let mut error = None;
            while moved < limit {
                let Some(i) = first_of(src) else { break };
                let id = match src[i].requeue_pi {
                    Some(target) if target.key == to => target.id,
                    _ => {
                        error = Some(EINVAL);
                        break;
                    }
                };
                let Sleeper::Task(task) = &src[i].sleeper else { unreachable!("a requeue wait is a task's") };
                if t.deadlocks(task, &lock_owner) {
                    // (As Linux: that waiter learns it, the requeue stops.)
                    let w = src.swap_remove(i);
                    let Sleeper::Task(task) = w.sleeper else { unreachable!() };
                    task.pi_wait.store(DEADLOCK, Ordering::Release);
                    try_wake(&task, TaskState::Sleeping);
                    bury(&mut grave, task);
                    error = Some(EDEADLK);
                    break;
                }
                if t.lock_of(&to).is_none() {
                    // (Room: see `reserve`.)
                    if !t.room() {
                        error = Some(ENOMEM);
                        break;
                    }
                    t.locks.push(Lock { key: to, owner: lock_owner.clone(), id: lock_id, fixup: false });
                }
                if !t.room() {
                    error = Some(ENOMEM);
                    break;
                }
                let w = src.swap_remove(i);
                let Sleeper::Task(task) = w.sleeper else { unreachable!() };
                task.pi_wait.store(QUEUED, Ordering::Release);
                t.waiters.push(PiWaiter { key: to, task, id, seq: w.seq });
                moved += 1;
            }
            tidy(src);
            t.reweigh(&lock_owner);
            bury(&mut grave, lock_owner);
            return match error {
                Some(e) => Err(e),
                None => Ok((woken + moved) as i64),
            };
        };
        drop(t);
        drop(second);
        drop(first);
        match fault {
            (addr, true) => uaccess::fault_in_writable(addr)?,
            (addr, false) => {
                uaccess::read::<u32>(addr)?;
            }
        }
    }
}

