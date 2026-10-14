//! futex(2) (phase R9), set_robust_list and get_robust_list: Linux's
//! operations over the kernel's futexes on the program's memory
//! (`restricted::SYS_FUTEX_WAIT`, `SYS_FUTEX_WAKE`, `SYS_FUTEX_REQUEUE`,
//! `SYS_FUTEX_WAKE_OP`: wait, wake, requeue and wake-op by key, as the
//! kernel keys a word: by address space and address, or by memory object
//! and offset for a shared mapping unless the operation is private) and its
//! priority-inheritance locks (`SYS_FUTEX_LOCK_PI` and after: the kernel
//! keeps the lock's waiters and lends their weight to the owner). The
//! server decodes the operations, reads the timeouts (relative on the
//! monotonic clock for FUTEX_WAIT; absolute for FUTEX_WAIT_BITSET,
//! FUTEX_WAIT_REQUEUE_PI and FUTEX_LOCK_PI2, on the wall clock with
//! FUTEX_CLOCK_REALTIME, and always on it for FUTEX_LOCK_PI) and restarts
//! an interrupted wait as Linux does: without a timeout the call restarts
//! unless a handler without SA_RESTART runs (ERESTARTSYS), with one it goes
//! on to the same deadline through restart_syscall unless a handler runs
//! (ERESTART_RESTARTBLOCK); a PI lock's wait always restarts
//! (ERESTARTNOINTR: its deadline is absolute).
//!
//! The PI calls name ids, which are the server's: the caller's tid, and the
//! owner a PI word names, which the server looks up in its thread table and
//! passes on by the thread's key; when the word changed owners meanwhile,
//! the kernel says so (`PI_RETRY`) and the server looks again.
//!
//! A thread's robust list (its robust mutexes, which the C library links
//! in the program's memory) is walked when it exits or execs, as Linux's
//! exit_robust_list: each lock it still holds gets `PI_OWNER_DIED` and a
//! waiter woken (`SYS_FUTEX_OWNER_DIED`, the kernel's atomic step on the
//! program's word), so a waiter of a process-shared robust mutex whose
//! owner died gets EOWNERDEAD.

use crate::local;
use crate::process;
use crate::signal::{self, RestartBlock, ERESTARTNOINTR, ERESTARTSYS, ERESTART_RESTARTBLOCK};
use crate::syscall;
use crate::usercopy;
use restricted::*;

const EINVAL: i64 = 22;
const EINTR: i64 = 4;
const ENOSYS: i64 = 38;
const ESRCH: i64 = 3;

pub const SYS_FUTEX: u64 = 202;
const SYS_SET_ROBUST_LIST: u64 = 273;
const SYS_GET_ROBUST_LIST: u64 = 274;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAKE_OP: u64 = 5;
const FUTEX_LOCK_PI: u64 = 6;
const FUTEX_UNLOCK_PI: u64 = 7;
const FUTEX_TRYLOCK_PI: u64 = 8;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_WAIT_REQUEUE_PI: u64 = 11;
const FUTEX_CMP_REQUEUE_PI: u64 = 12;
const FUTEX_LOCK_PI2: u64 = 13;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;
const FUTEX_BITSET_MATCH_ANY: u64 = u32::MAX as u64;
const NSEC_PER_SEC: u64 = 1_000_000_000;

/// `struct robust_list_head`: the list (a pointer to the first entry), the
/// offset from an entry to its futex word, the entry being locked or
/// unlocked (`list_op_pending`).
const ROBUST_HEAD_SIZE: u64 = 24;
/// The most entries an exit walks (Linux's ROBUST_LIST_LIMIT): a looping
/// or corrupt list ends there.
const ROBUST_LIST_LIMIT: usize = 2048;

/// The result of futex(2), set_robust_list or get_robust_list in `s`, or
/// None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let r = match s.rax {
        SYS_FUTEX => futex(s.rdi, s.rsi, s.rdx, s.r10, s.r8, s.r9),
        SYS_SET_ROBUST_LIST => set_robust_list(s.rdi, s.rsi),
        SYS_GET_ROBUST_LIST => get_robust_list(s.rdi, s.rsi, s.rdx),
        _ => return None,
    };
    Some(r.unwrap_or_else(|e| -e))
}

fn check(r: i64) -> Result<i64, i64> {
    if r < 0 { Err(-r) } else { Ok(r) }
}

/// A count argument (an int; negative counts as none).
fn count(n: u64) -> u64 {
    (n as u32 as i32).max(0) as u64
}

/// futex(uaddr, op, val, timeout or val2, uaddr2, val3).
fn futex(uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> Result<i64, i64> {
    let private = if op & FUTEX_PRIVATE_FLAG != 0 { FUTEX_PRIVATE } else { 0 };
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    if realtime && !matches!(cmd, FUTEX_WAIT | FUTEX_WAIT_BITSET | FUTEX_WAIT_REQUEUE_PI | FUTEX_LOCK_PI2) {
        return Err(ENOSYS);
    }
    // Program memory only (the kernel checks again).
    if uaddr >= SHARED_BASE {
        return Err(usercopy::EFAULT);
    }
    // A second word for the operations that take one.
    let second = || if uaddr2 >= SHARED_BASE { Err(usercopy::EFAULT) } else { Ok(uaddr2) };
    match cmd {
        FUTEX_WAIT => wait(uaddr, val as u32, deadline(timeout, false, realtime)?, FUTEX_BITSET_MATCH_ANY, private),
        FUTEX_WAIT_BITSET => wait(uaddr, val as u32, deadline(timeout, true, realtime)?, val3 as u32 as u64, private),
        FUTEX_WAKE => check(syscall(SYS_FUTEX_WAKE, [uaddr, count(val), FUTEX_BITSET_MATCH_ANY, private, 0, 0])),
        FUTEX_WAKE_BITSET => check(syscall(SYS_FUTEX_WAKE, [uaddr, count(val), val3 as u32 as u64, private, 0, 0])),
        // For the requeue operations the timeout argument is a count.
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            // Negative counts are EINVAL here (Linux's futex_requeue).
            if (val as u32 as i32) < 0 || (timeout as u32 as i32) < 0 {
                return Err(EINVAL);
            }
            let uaddr2 = second()?;
            let flags = private | if cmd == FUTEX_CMP_REQUEUE { FUTEX_CMP } else { 0 };
            check(syscall(SYS_FUTEX_REQUEUE, [uaddr, count(val), count(timeout), uaddr2, val3 as u32 as u64, flags]))
        }
        // The timeout argument is the second count; `val3` encodes the change.
        FUTEX_WAKE_OP => {
            let uaddr2 = second()?;
            check(syscall(SYS_FUTEX_WAKE_OP, [uaddr, count(val), uaddr2, count(timeout), val3 as u32 as u64, private]))
        }
        FUTEX_LOCK_PI | FUTEX_LOCK_PI2 => {
            // FUTEX_LOCK_PI's deadline is always the wall clock's.
            let at = deadline(timeout, true, realtime || cmd == FUTEX_LOCK_PI)?;
            lock_pi(uaddr, at, private)
        }
        FUTEX_TRYLOCK_PI => lock_pi(uaddr, None, private | FUTEX_TRY),
        FUTEX_UNLOCK_PI => check(syscall(SYS_FUTEX_UNLOCK_PI, [uaddr, local::tid() as u64, private, 0, 0, 0])),
        FUTEX_WAIT_REQUEUE_PI => {
            if uaddr == uaddr2 {
                return Err(EINVAL);
            }
            let uaddr2 = second()?;
            let at = deadline(timeout, true, realtime)?.map_or(0, |d| d.max(1));
            match check(syscall(SYS_FUTEX_WAIT_REQUEUE_PI, [uaddr, val as u32 as u64, at, uaddr2, local::tid() as u64, private])) {
                // Still on `uaddr` (after a requeue the kernel ends a kicked
                // wait with EAGAIN): restarted after the signal.
                Err(EINTR) => Err(ERESTARTNOINTR),
                r => r,
            }
        }
        FUTEX_CMP_REQUEUE_PI => {
            // One waiter is woken (taking the lock), the rest requeued.
            if val as u32 as i32 != 1 || (timeout as u32 as i32) < 0 {
                return Err(EINVAL);
            }
            if uaddr == uaddr2 {
                return Err(EINVAL);
            }
            let uaddr2 = second()?;
            loop {
                let (id, key) = owner_of(uaddr2)?;
                let r = syscall(SYS_FUTEX_CMP_REQUEUE_PI, [uaddr, count(timeout), uaddr2, val3 as u32 as u64, key, private | (id as u64) << 32]);
                if r != -PI_RETRY {
                    return check(r);
                }
            }
        }
        _ => Err(ENOSYS),
    }
}

/// The owner a PI word at `uaddr` names: its id, and its thread's key (0:
/// no thread of the instance has the id, or the word has no owner).
fn owner_of(uaddr: u64) -> Result<(u32, u64), i64> {
    let word: u32 = usercopy::read(uaddr)?;
    let id = word & PI_ID_MASK;
    // (`key_of(0)` would be the caller's.)
    let key = if id == 0 { 0 } else { process::key_of(id).unwrap_or(0) };
    Ok((id, key))
}

/// FUTEX_LOCK_PI, FUTEX_LOCK_PI2 and FUTEX_TRYLOCK_PI (`flags` with
/// `FUTEX_TRY`): the caller takes the PI lock at `uaddr` or waits for it
/// until `deadline`.
fn lock_pi(uaddr: u64, deadline: Option<u64>, flags: u64) -> Result<i64, i64> {
    let at = deadline.map_or(0, |d| d.max(1));
    loop {
        let (id, key) = owner_of(uaddr)?;
        let r = syscall(SYS_FUTEX_LOCK_PI, [uaddr, local::tid() as u64, id as u64, key, at, flags]);
        match r {
            r if r == -PI_RETRY => continue,
            // Restarted after the handler, whatever its SA_RESTART (the
            // deadline is absolute).
            r if r == -EINTR => return Err(ERESTARTNOINTR),
            r => return check(r),
        }
    }
}

/// The monotonic deadline of a timeout at `ts` (a timespec; 0: none): an
/// interval, or with `absolute` a time of the monotonic clock or, with
/// `realtime`, of the wall clock (taken as the monotonic time it
/// corresponds to now).
fn deadline(ts: u64, absolute: bool, realtime: bool) -> Result<Option<u64>, i64> {
    if ts == 0 {
        return Ok(None);
    }
    let [sec, nsec]: [i64; 2] = usercopy::read(ts)?;
    if sec < 0 || !(0..NSEC_PER_SEC as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    let t = (sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(nsec as u64);
    let now = syscall(SYS_CLOCK_READ, [CLOCK_MONO, 0, 0, 0, 0, 0]).max(0) as u64;
    Ok(Some(if !absolute {
        now.saturating_add(t)
    } else if realtime {
        let wall = syscall(SYS_CLOCK_READ, [CLOCK_WALL, 0, 0, 0, 0, 0]).max(0) as u64;
        now.saturating_add(t.saturating_sub(wall))
    } else {
        t
    }))
}

/// Waits while the word at `uaddr` holds `val`, until a wake with a bit of
/// `bitset`, the deadline or a signal.
fn wait(uaddr: u64, val: u32, deadline: Option<u64>, bitset: u64, private: u64) -> Result<i64, i64> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    wait_until(uaddr, val, deadline, bitset, private)
}

fn wait_until(uaddr: u64, val: u32, deadline: Option<u64>, bitset: u64, private: u64) -> Result<i64, i64> {
    // A deadline of 0 means none to the kernel: one that passed is 1.
    let at = deadline.map_or(0, |d| d.max(1));
    match check(syscall(SYS_FUTEX_WAIT, [uaddr, val as u64, at, bitset, private, 0])) {
        Err(EINTR) => match deadline {
            None => Err(ERESTARTSYS),
            Some(deadline) => {
                signal::save_block(RestartBlock::Futex { uaddr, val, deadline, bitset, private });
                Err(ERESTART_RESTARTBLOCK)
            }
        },
        r => r,
    }
}

/// restart_syscall of a futex wait: on to the same deadline.
pub fn wait_rest(block: RestartBlock) -> Result<i64, i64> {
    let RestartBlock::Futex { uaddr, val, deadline, bitset, private } = block else { return Err(EINTR) };
    wait_until(uaddr, val, Some(deadline), bitset, private)
}

/// set_robust_list(head, len): the calling thread's robust list (any
/// address: it is read only when the thread exits or execs).
fn set_robust_list(head: u64, len: u64) -> Result<i64, i64> {
    if len != ROBUST_HEAD_SIZE {
        return Err(EINVAL);
    }
    process::set_robust(head);
    Ok(0)
}

/// get_robust_list(tid, head_ptr, len_ptr): the robust list of thread
/// `tid` (0: the caller). Everyone is root here, so every thread's may be
/// read (Linux's PTRACE_MODE_READ check passes).
fn get_robust_list(tid: u64, head_ptr: u64, len_ptr: u64) -> Result<i64, i64> {
    // (A pid_t: a negative one names no thread.)
    let tid = u32::try_from(tid as u32 as i32).map_err(|_| ESRCH)?;
    let head = process::robust_of(tid).ok_or(ESRCH)?;
    usercopy::write(len_ptr, &ROBUST_HEAD_SIZE)?;
    usercopy::write(head_ptr, &head)?;
    Ok(0)
}

/// The thread `tid` ends or execs with the robust list at `head` (0: none):
/// every lock on it, and the one it was taking or letting go of
/// (`list_op_pending`), that it still holds is marked as its owner's death
/// (Linux's exit_robust_list). An entry's bit 0 says the lock is a PI lock.
/// The walk ends at the first entry it cannot read or whose word is
/// misaligned, and after `ROBUST_LIST_LIMIT` entries.
pub fn exit_robust(head: u64, tid: u32) {
    if head == 0 || head >= SHARED_BASE {
        return;
    }
    // An entry pointer and its PI bit.
    let fetch = |at: u64| usercopy::read::<u64>(at).ok().map(|v| (v & !1, v & 1 != 0));
    let Some((mut entry, mut pi)) = fetch(head) else { return };
    let Ok(offset) = usercopy::read::<i64>(head + 8) else { return };
    let Some((pending, pending_pi)) = fetch(head + 16) else { return };
    let died = |entry: u64, pi: bool, pending: bool| {
        let word = entry.wrapping_add(offset as u64);
        if word % 4 != 0 || word >= SHARED_BASE {
            return false;
        }
        let flags = if pi { FUTEX_PI } else { 0 } | if pending { FUTEX_PENDING } else { 0 };
        syscall(SYS_FUTEX_OWNER_DIED, [word, tid as u64, flags, 0, 0, 0]) >= 0
    };
    for _ in 0..ROBUST_LIST_LIMIT {
        if entry == head {
            break;
        }
        // The next entry before this one's word changes hands.
        let next = fetch(entry);
        // (The pending entry may be on the list too: it is done once, below.)
        if entry != pending && !died(entry, pi, false) {
            return;
        }
        let Some((e, p)) = next else { return };
        (entry, pi) = (e, p);
    }
    if pending != 0 {
        died(pending, pending_pi, true);
    }
}
