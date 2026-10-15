//! membarrier(2)'s mechanisms (`restricted::SYS_MEMBARRIER`, ADR 0021): barriers on other
//! CPUs, as Linux's kernel/sched/membarrier.c makes them. The commands and the private
//! registrations are the Linux server's; what the kernel must see is here.
//!
//! - **Private expedited** (`MEMBARRIER_SPACE`): an interrupt on each CPU that has the
//!   caller's address space loaded (`tlb::barrier_on`).
//! - **Global expedited** (`MEMBARRIER_GLOBAL_EXPEDITED`): an interrupt on each CPU that has
//!   an address space loaded that registered for it (`MEMBARRIER_REGISTER_GLOBAL`), of any
//!   process tree (a tree's processes share memory with other trees': /data's shared
//!   mappings, ADR 0015), as Linux's membarrier_global_expedited interrupts only the CPUs
//!   whose current mm registered. Nobody else pays for it. The registration belongs to the
//!   address space: fork copies it (Linux's mm flags), a process sharing the space shares it,
//!   execve's new space has none.
//! - **Global** (`MEMBARRIER_GLOBAL`): no interrupt at all, as Linux's synchronize_rcu: the
//!   caller sleeps until every other CPU has passed a quiescent state since the call began,
//!   one where everything it ran before is ordered before what it runs after: its timer
//!   interrupt (every CPU takes one each tick, 10 ms, idle or not; the handler counts it with
//!   a locked instruction, `tick`), or being idle (it runs no task, and leaving the idle loop
//!   goes through the scheduler's locked operations). So a call takes at most about a tick.
//!   Linux's waits as long as it takes and cannot be interrupted; this one is bounded: a CPU
//!   that has not passed one after a second (interrupts off that long would be a bug) gets
//!   the interrupt after all.

use super::tlb::{self, Tlb};
use crate::smp;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{fence, AtomicU64, Ordering};

/// Each CPU's timer interrupts, by index: its quiescent states.
static TICKS: [AtomicU64; smp::MAX_CPUS] = [const { AtomicU64::new(0) }; smp::MAX_CPUS];

/// The address spaces registered for global expedited barriers (dead ones are pruned as
/// the list is walked).
static REGISTERED: spin::Mutex<Vec<Weak<Tlb>>> = spin::Mutex::new(Vec::new());

/// How long `global` waits for a CPU's quiescent state before it interrupts it.
const QUIESCE_MAX_NS: u64 = 1_000_000_000;

/// From the timer interrupt: this CPU passed a quiescent state. (A locked RMW: everything
/// the interrupted code did is visible before the count moves.)
pub fn tick() {
    TICKS[smp::cpu().index].fetch_add(1, Ordering::SeqCst);
}

/// The CPUs online, as a mask.
fn online() -> u64 {
    (0..smp::MAX_CPUS).filter(|&i| smp::by_index(i).is_some()).fold(0u64, |m, i| m | 1 << i)
}

/// Private expedited: an interrupt on each CPU that runs `space`.
pub fn private(space: &Tlb) {
    tlb::barrier_on(space.cpus());
}

/// Whether `space` registered for global expedited barriers.
fn registered(space: &Arc<Tlb>) -> bool {
    REGISTERED.lock().iter().any(|w| core::ptr::eq(w.as_ptr(), Arc::as_ptr(space)))
}

/// Registers `space` for global expedited barriers (nothing if it is already); Err without
/// memory for the entry.
pub fn register_global(space: &Arc<Tlb>) -> Result<(), ()> {
    let mut list = REGISTERED.lock();
    if list.iter().any(|w| core::ptr::eq(w.as_ptr(), Arc::as_ptr(space))) {
        return Ok(());
    }
    list.retain(|w| w.strong_count() > 0);
    list.try_reserve(1).map_err(|_| ())?;
    list.push(Arc::downgrade(space));
    Ok(())
}

/// fork: the child's address space registered if the parent's is (Linux's mm flags).
pub fn inherit(parent: &Arc<Tlb>, child: &Arc<Tlb>) -> Result<(), ()> {
    if registered(parent) { register_global(child) } else { Ok(()) }
}

/// Global expedited: an interrupt on each CPU that runs a registered address space.
pub fn global_expedited() {
    fence(Ordering::SeqCst);
    let mut targets = 0;
    {
        let mut list = REGISTERED.lock();
        list.retain(|w| w.strong_count() > 0);
        for w in list.iter() {
            if let Some(space) = w.upgrade() {
                targets |= space.cpus();
            }
        }
    }
    tlb::barrier_on(targets);
}

/// Global: sleeps until every other CPU passed a quiescent state (a tick, or idle) since
/// the call began; none is interrupted (but one that has not after `QUIESCE_MAX_NS`).
pub fn global() {
    fence(Ordering::SeqCst);
    let me = smp::cpu().index;
    let mut start = [0u64; smp::MAX_CPUS];
    let mut pending = online() & !(1 << me);
    for (i, s) in start.iter_mut().enumerate() {
        *s = TICKS[i].load(Ordering::SeqCst);
    }
    let began = crate::time::now();
    loop {
        for i in 0..smp::MAX_CPUS {
            if pending & (1 << i) == 0 {
                continue;
            }
            let passed = TICKS[i].load(Ordering::SeqCst) != start[i] || smp::by_index(i).is_none_or(super::sched::halted);
            if passed {
                pending &= !(1 << i);
            }
        }
        if pending == 0 {
            break;
        }
        let now = crate::time::now();
        if now >= began + QUIESCE_MAX_NS {
            tlb::barrier_on(pending);
            break;
        }
        // (A nap that a signal does not end: the call is bounded anyway.)
        super::sched::prepare_to_sleep().sleep_until(now + crate::timer::TICK_NS / 2);
    }
    fence(Ordering::SeqCst);
}
