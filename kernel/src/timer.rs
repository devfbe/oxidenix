//! Timers: per-CPU queues of deadlines on the monotonic clock, and the
//! local APIC timer programmed for the earliest one.
//!
//! A sleep with a timeout, an interval timer (ITIMER_REAL) and the
//! scheduler tick are all deadlines in nanoseconds. A deadline is queued
//! on the CPU that arms it, and that CPU's local APIC interrupts when the
//! first one is due: in TSC-deadline mode where the CPU has it (the
//! interrupt comes when the TSC reaches a value), else with a one-shot
//! count. A sleep therefore ends when it is due, not at the next tick.
//!
//! Cancelling is lazy: the owner of a timer (a task, a process's interval
//! timer) holds the sequence number of its live arming, and an entry whose
//! number is no longer current is skipped when it comes up. Entries refer
//! to their owner weakly, so a stale one keeps no task alive. The queues
//! never allocate after start-up: there is at most one live entry per task
//! and per process, a queue holds twice that many, and a full queue drops
//! its stale entries first. So arming works in interrupt context too.
//!
//! Nothing re-queues itself from the interrupt (an interval timer is
//! reloaded when its signal is taken), and the interrupt runs only what is
//! due when it starts: its work is bounded by the queue's size.

use crate::interrupts::apic;
use crate::process::sched::{self, MAX_PROCS};
use crate::process::task::{State, Task, ThreadGroup};
use crate::smp;
use crate::sync::IrqSpinLock;
use crate::time;
use alloc::collections::BinaryHeap;
use alloc::sync::{Arc, Weak};
use core::cmp::Ordering as CmpOrdering;
use core::sync::atomic::{AtomicU64, Ordering};

/// The scheduler's time slice and the period of its accounting tick.
pub const TICK_NS: u64 = time::NSEC_PER_SEC / crate::process::TIMER_HZ;

/// Live entries: one sleep per task, one interval timer per process.
const CAPACITY: usize = 2 * MAX_PROCS + 2;

/// Who a timer belongs to, and what happens when it expires.
enum Owner {
    /// A sleeping task is woken.
    Task(Weak<Task>),
    /// A process's ITIMER_REAL sends SIGALRM (and reloads itself).
    Alarm(Weak<ThreadGroup>),
}

struct Entry {
    deadline: u64,
    seq: u64,
    owner: Owner,
}

impl Entry {
    /// The owner's sequence number of its live arming (0: none).
    fn live(&self) -> bool {
        match &self.owner {
            Owner::Task(t) => t.upgrade().is_some_and(|t| t.timer_seq.load(Ordering::Acquire) == self.seq),
            Owner::Alarm(g) => g.upgrade().is_some_and(|g| g.alarm_seq.load(Ordering::Acquire) == self.seq),
        }
    }
}

// Earliest deadline first: BinaryHeap is a max-heap, so the order is reversed.
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (other.deadline, other.seq).cmp(&(self.deadline, self.seq))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}

impl Eq for Entry {}

/// One CPU's timers.
pub struct Queue {
    heap: BinaryHeap<Entry>,
    /// When this CPU's next scheduler tick is due.
    next_tick: u64,
    /// The deadline the local APIC is programmed for (u64::MAX: none).
    programmed: u64,
}

impl Queue {
    pub const fn new() -> Queue {
        Queue { heap: BinaryHeap::new(), next_tick: 0, programmed: u64::MAX }
    }

    /// Adds an entry without allocating: a full queue first drops what is
    /// no longer live, which leaves room (see CAPACITY).
    fn push(&mut self, entry: Entry) {
        if self.heap.len() == self.heap.capacity() {
            self.heap.retain(|e| e.live());
        }
        assert!(self.heap.len() < self.heap.capacity(), "timer queue full of live timers");
        self.heap.push(entry);
    }
}

pub type CpuTimers = IrqSpinLock<Queue>;

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

/// How the local APIC timer is driven, decided once at boot.
static DEADLINE_MODE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Local APIC timer counts per second (one-shot mode).
static APIC_HZ: AtomicU64 = AtomicU64::new(0);

/// Chooses the timer mode; `apic_hz` is the local APIC timer's measured
/// frequency. Called once on the bootstrap CPU.
pub fn init(apic_hz: u64) {
    const TSC_DEADLINE: u32 = 1 << 24;
    let deadline = core::arch::x86_64::__cpuid(1).ecx & TSC_DEADLINE != 0;
    DEADLINE_MODE.store(deadline, Ordering::Relaxed);
    APIC_HZ.store(apic_hz.max(1), Ordering::Relaxed);
    crate::printkln!("[time] timers: local APIC in {} mode", if deadline { "TSC-deadline" } else { "one-shot" });
}

/// Starts this CPU's timers: its queue gets its fixed capacity, the
/// local APIC its mode, and the first tick is programmed.
pub fn init_cpu() {
    let cpu = smp::cpu();
    let mut q = cpu.timers.lock();
    q.heap.reserve_exact(CAPACITY);
    apic::set_timer_mode(DEADLINE_MODE.load(Ordering::Relaxed));
    q.next_tick = time::now() + TICK_NS;
    let first = q.next_tick;
    program(&mut q, first);
}

/// Programs this CPU's local APIC to interrupt at `deadline`.
fn program(q: &mut Queue, deadline: u64) {
    q.programmed = deadline;
    if DEADLINE_MODE.load(Ordering::Relaxed) {
        apic::set_tsc_deadline(time::tsc_at(deadline).max(1));
    } else {
        let ns = deadline.saturating_sub(time::now());
        let count = (ns as u128 * APIC_HZ.load(Ordering::Relaxed) as u128 / time::NSEC_PER_SEC as u128) as u64;
        apic::set_oneshot_count(count.clamp(1, u32::MAX as u64) as u32);
    }
}

/// Queues `entry` on this CPU and reprograms the APIC if it is the
/// earliest.
fn queue(entry: Entry) {
    let cpu = smp::cpu();
    let mut q = cpu.timers.lock();
    let deadline = entry.deadline;
    q.push(entry);
    if deadline < q.programmed {
        program(&mut q, deadline);
    }
}

fn next_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Wakes `task` (asleep by then) at `deadline`. Arming again or `disarm`
/// cancels it.
pub fn wake_at(task: &Arc<Task>, deadline: u64) {
    let seq = next_seq();
    task.timer_seq.store(seq, Ordering::Release);
    queue(Entry { deadline, seq, owner: Owner::Task(Arc::downgrade(task)) });
}

pub fn disarm(task: &Task) {
    task.timer_seq.store(0, Ordering::Release);
}

/// Arms `group`'s interval timer for `deadline`; the caller holds the
/// group's signal lock, which serializes this with the timer's expiry.
pub fn arm_alarm(group: &Arc<ThreadGroup>, deadline: u64) {
    let seq = next_seq();
    group.alarm_seq.store(seq, Ordering::Release);
    queue(Entry { deadline, seq, owner: Owner::Alarm(Arc::downgrade(group)) });
}

/// Cancels `group`'s interval timer (under its signal lock).
pub fn disarm_alarm(group: &ThreadGroup) {
    group.alarm_seq.store(0, Ordering::Release);
}

/// The local APIC timer interrupt: runs what is due, the scheduler tick
/// if it is due, and programs the next interrupt. Returns whether the
/// interrupted user task should give up the CPU (its time slice ended, or
/// a task woke up to run here).
pub fn interrupt(from_user: bool) -> bool {
    let cpu = smp::cpu();
    let mut woke = false;
    // Only what is due now: entries armed while this runs wait for the
    // next interrupt.
    let now = time::now();
    loop {
        let mut due: heapless::Vec<Entry, 16> = heapless::Vec::new();
        {
            let mut q = cpu.timers.lock();
            while q.heap.peek().is_some_and(|e| e.deadline <= now) && !due.is_full() {
                let _ = due.push(q.heap.pop().expect("peeked"));
            }
        }
        if due.is_empty() {
            break;
        }
        for entry in due {
            woke |= expire(entry);
        }
    }
    let now = time::now();
    let mut q = cpu.timers.lock();
    let tick = now >= q.next_tick;
    if tick {
        // Ticks missed while interrupts were off are not made up.
        q.next_tick = (q.next_tick + TICK_NS).max(now + TICK_NS / 2);
    }
    let next = q.heap.peek().map_or(q.next_tick, |e| e.deadline.min(q.next_tick));
    program(&mut q, next);
    drop(q);
    if tick {
        sched::tick(from_user);
    }
    tick || woke
}

/// Runs an expired entry if it is still live. Returns whether it woke a
/// task.
fn expire(entry: Entry) -> bool {
    match &entry.owner {
        Owner::Task(t) => {
            let Some(task) = t.upgrade() else { return false };
            if task.timer_seq.compare_exchange(entry.seq, 0, Ordering::AcqRel, Ordering::Relaxed).is_err() {
                return false;
            }
            sched::try_wake(&task, State::Sleeping)
        }
        Owner::Alarm(g) => {
            let Some(group) = g.upgrade() else { return false };
            crate::process::signal::alarm_expired(&group, entry.seq, entry.deadline);
            false
        }
    }
}
