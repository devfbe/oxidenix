//! The SMP scheduler: per-CPU run queues, wait queues, context switches.
//!
//! Lock order (outer to inner): process table → group info → group
//! signals → thread signals → wait queue → task wake_lock → run queue. The
//! heap and the frame allocator are leaves.
//!
//! Sleeping uses the classic protocol that cannot lose a wakeup:
//!
//! ```text
//! loop {
//!     let wait = prepare_to_wait(chan); // Sleeping, listed in chan's bucket
//!     if condition() { break }          // a wakeup from here on is not lost
//!     if interrupted() { return EINTR }
//!     wait.sleep();                     // deschedules unless already woken
//! }
//! ```
//!
//! A wakeup that finds the task still on its CPU (not yet descheduled) only
//! sets it Running again; `schedule` then keeps it. Otherwise it is queued.
//! (poll, select and epoll are the Linux server's, over its futexes: see
//! `futex::server_waitv`.)

use super::task::{KernelStack, State, Task, ThreadGroup};
use super::Pid;
use crate::smp::{self, Cpu};
use crate::sync::IrqSpinLock;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

pub const MAX_PROCS: usize = 256;

/// All threads by thread id and all processes (live and zombies) by
/// process id; idle tasks are not listed. Thread and process ids share
/// one number space: a process's id is its main thread's.
pub struct Table {
    pub tasks: BTreeMap<Pid, Arc<Task>>,
    pub groups: BTreeMap<Pid, Arc<ThreadGroup>>,
    pub next_pid: Pid,
    /// Ids handed out whose task is still being built: they count against
    /// the limit, so concurrent forks on several CPUs cannot exceed it.
    pub reserved: usize,
    /// Processes whose threads all exited and that wait to be reaped.
    pub zombies: usize,
}

pub static TABLE: IrqSpinLock<Table> =
    IrqSpinLock::new(Table { tasks: BTreeMap::new(), groups: BTreeMap::new(), next_pid: 1, reserved: 0, zombies: 0 });

/// An id taken under the task limit; `insert` turns it into a listed
/// task, dropping it gives the slot back.
pub struct PidReservation {
    pub pid: Pid,
    done: bool,
}

static FORKS: AtomicU64 = AtomicU64::new(0);

/// Processes created since boot.
pub fn forks() -> u64 {
    FORKS.load(Ordering::Relaxed)
}

/// Takes the next id, or EAGAIN at the limit of tasks (threads and
/// zombies).
pub fn reserve_pid() -> Result<PidReservation, i64> {
    FORKS.fetch_add(1, Ordering::Relaxed);
    let mut table = TABLE.lock();
    if table.tasks.len() + table.zombies + table.reserved >= MAX_PROCS {
        return Err(super::errno::EAGAIN);
    }
    let pid = table.next_pid;
    table.next_pid += 1;
    table.reserved += 1;
    Ok(PidReservation { pid, done: false })
}

impl PidReservation {
    /// Lists `task` (and its process, if it is the main thread of a new
    /// one). A new thread of a process that is already ending is refused
    /// (EAGAIN), so no thread outlives a group exit or exec.
    pub fn insert(mut self, task: Arc<Task>) -> Result<(), i64> {
        let mut table = TABLE.lock();
        let group = task.group.clone();
        // Nothing new comes out of a process that is ending or exec'ing:
        // neither a thread nor a forked child.
        if current().group.sig.lock().exit != super::signal::GroupExit::None {
            return Err(super::errno::EAGAIN);
        }
        let mut info = group.info.lock();
        if task.tid() != group.tgid && group.sig.lock().joins_stop() {
            // A thread born during a group stop stops too.
            task.sig.lock().set_stop();
        }
        info.threads.try_reserve(1).map_err(|_| super::errno::ENOMEM)?;
        info.threads.push(task.clone());
        drop(info);
        table.tasks.insert(self.pid, task);
        if self.pid == group.tgid {
            table.groups.insert(self.pid, group);
        }
        table.reserved -= 1;
        self.done = true;
        Ok(())
    }
}

impl Drop for PidReservation {
    fn drop(&mut self) {
        if !self.done {
            TABLE.lock().reserved -= 1;
        }
    }
}

/// Scheduler ticks since boot (clock ticks of procfs and `times`).
pub fn ticks() -> u64 {
    crate::time::now() / crate::timer::TICK_NS
}

/// The scheduler's part of a CPU block. `current`, `prev` and `idle` are
/// touched only by their own CPU with interrupts disabled.
pub struct CpuSched {
    rq: IrqSpinLock<VecDeque<Arc<Task>>>,
    current: UnsafeCell<Option<Arc<Task>>>,
    /// The task this CPU just switched away from, until `finish_switch`.
    prev: UnsafeCell<Option<Arc<Task>>>,
    idle: UnsafeCell<Option<Arc<Task>>>,
    /// In the idle loop's `hlt`: new work for it needs an IPI.
    halted: AtomicBool,
    /// The running task should give up the CPU at its next return to user
    /// space (its time slice ended, or a task woke up here while the CPU
    /// was in the kernel, which is not preempted).
    need_resched: AtomicBool,
    /// Fair scheduling: the smallest virtual runtime of this CPU's tasks
    /// so far (it only grows); the running task's virtual runtime, weight
    /// and whether it is the idle task, as of `curr_since`; when its time
    /// slice began.
    min_vruntime: AtomicU64,
    curr_vruntime: AtomicU64,
    curr_weight: AtomicU32,
    curr_since: AtomicU64,
    curr_idle: AtomicBool,
    /// The sequence count that makes the curr_* fields one value for
    /// readers on other CPUs (odd while they change).
    curr_seq: AtomicU32,
    slice_start: AtomicU64,
    /// Timer ticks spent in user mode, in the kernel and idling, and
    /// context switches.
    pub user_ticks: AtomicU64,
    pub system_ticks: AtomicU64,
    pub idle_ticks: AtomicU64,
    pub switches: AtomicU64,
}

impl CpuSched {
    pub const fn new() -> Self {
        CpuSched {
            rq: IrqSpinLock::new(VecDeque::new()),
            current: UnsafeCell::new(None),
            prev: UnsafeCell::new(None),
            idle: UnsafeCell::new(None),
            halted: AtomicBool::new(false),
            need_resched: AtomicBool::new(false),
            min_vruntime: AtomicU64::new(0),
            curr_vruntime: AtomicU64::new(0),
            curr_weight: AtomicU32::new(DEFAULT_WEIGHT),
            curr_since: AtomicU64::new(0),
            curr_idle: AtomicBool::new(true),
            curr_seq: AtomicU32::new(0),
            slice_start: AtomicU64::new(0),
            user_ticks: AtomicU64::new(0),
            system_ticks: AtomicU64::new(0),
            idle_ticks: AtomicU64::new(0),
            switches: AtomicU64::new(0),
        }
    }
}

/// The task running on this CPU. Valid while it stays current, which the
/// caller (that task itself) guarantees.
pub fn current() -> &'static Task {
    let cs = &smp::cpu().sched;
    let t: *const Task = unsafe { (*cs.current.get()).as_deref().expect("no current task") };
    unsafe { &*t }
}

/// A counted reference to the running task (for storing it elsewhere).
pub fn current_arc() -> Arc<Task> {
    let cs = &smp::cpu().sched;
    unsafe { (*cs.current.get()).clone().expect("no current task") }
}

/// Makes `task` the running task of this CPU at start-up.
pub fn set_initial(task: Arc<Task>, idle: Arc<Task>) {
    let cs = &smp::cpu().sched;
    task.on_cpu.store(true, Ordering::Relaxed);
    task.set_state(State::Running);
    task.last_cpu.store(smp::cpu().index, Ordering::Relaxed);
    task.start_running(crate::time::now());
    rebase(&task, smp::cpu());
    set_curr(smp::cpu(), task.vruntime.load(Ordering::Relaxed), crate::time::now(), task_weight(&task), task.idle);
    unsafe {
        *cs.current.get() = Some(task);
        *cs.idle.get() = Some(idle);
    }
    // Run queues are filled from interrupt context too: never reallocate.
    let _ = cs.rq.lock().try_reserve(MAX_PROCS);
}

// ---------------------------------------------------------------- queues

/// Claims a halted CPU for new work: the first enqueuer to clear its flag
/// wakes it, the next one picks another idle CPU instead of piling on.
fn claim(cpu: &Cpu) -> bool {
    cpu.sched.halted.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Puts a runnable task into the run queue of a CPU it may run on: an idle
/// one if there is one (preferring the task's last CPU, and claiming it so
/// that concurrent wakeups spread out), else its last CPU's, else the first
/// allowed. A woken or new task that is owed time (its virtual runtime
/// a wakeup granularity behind the running task's) preempts it.
fn enqueue(task: Arc<Task>, how: Arrival) {
    let last = task.last_cpu.load(Ordering::Relaxed);
    let allowed = |c: &&Cpu| task.may_run_on(c.index);
    let cpus = || (0..smp::MAX_CPUS).filter_map(smp::by_index);
    let claimed = smp::by_index(last)
        .filter(|c| allowed(c) && claim(c))
        .or_else(|| cpus().filter(allowed).find(|c| claim(c)));
    let target = claimed
        .or_else(|| smp::by_index(last).filter(allowed))
        .or_else(|| cpus().find(allowed))
        .unwrap_or_else(smp::cpu);
    rebase(&task, target);
    if how == Arrival::Wake {
        let floor = target.sched.min_vruntime.load(Ordering::Relaxed).wrapping_sub(LATENCY / 2);
        task.vruntime.store(vmax(task.vruntime.load(Ordering::Relaxed), floor), Ordering::Relaxed);
    }
    front_if_holding(&task, target);
    let owed = vtime(WAKEUP_GRANULARITY, task_weight(&task));
    let v = task.vruntime.load(Ordering::Relaxed);
    target.sched.rq.lock().push_back(task);
    // Pairs with the fence in `idle_loop`: either the idle CPU sees the
    // task, or we see it halted and wake it.
    fence(Ordering::SeqCst);
    let mut wake = claimed.is_some() || claim(target);
    if !wake && how != Arrival::Move && curr_state(target, crate::time::now()).is_some_and(|cv| vbefore(v.wrapping_add(owed), cv)) {
        target.sched.need_resched.store(true, Ordering::Relaxed);
        wake = true;
    }
    if wake && target.index != smp::cpu().index {
        crate::interrupts::apic::ipi::send_vector(target.apic_id(), crate::interrupts::apic::ipi::RESCHEDULE_VECTOR);
    }
}

/// The next task for this CPU: its own queue first, else one stolen from
/// the back of another CPU's queue (one that may run here). A task whose
/// affinity no longer allows this CPU moves on to one it may run on.
///
/// A task another CPU is still switching away from (`on_cpu`; a CPU puts
/// its current task back in a queue before it switches) is left where it
/// is: `context_switch` would wait for it with interrupts off, and two
/// CPUs that each took the other's outgoing task (one from its own queue,
/// one by stealing the newest entry of the other's) waited for each other
/// for good. It is taken at a later schedule, once its switch is done.
fn pick_next(cpu: &Cpu) -> Option<Arc<Task>> {
    let cur: *const Task = current();
    let switched_out = |t: &Arc<Task>| !t.on_cpu.load(Ordering::Acquire) || core::ptr::eq(&**t, cur);
    loop {
        let t = {
            let mut rq = cpu.sched.rq.lock();
            // A task that may not run here first (it moves on), else the
            // one with the smallest virtual runtime (the first of equals).
            let min = cpu.sched.min_vruntime.load(Ordering::Relaxed);
            let i = rq
                .iter()
                .position(|t| !t.may_run_on(cpu.index))
                .or_else(|| {
                    rq.iter()
                        .enumerate()
                        .filter(|(_, t)| switched_out(t))
                        .min_by_key(|(_, t)| t.vruntime.load(Ordering::Relaxed).wrapping_sub(min) as i64)
                        .map(|(i, _)| i)
                });
            i.and_then(|i| rq.remove(i))
        };
        match t {
            Some(t) if t.may_run_on(cpu.index) => return Some(t),
            Some(t) => enqueue(t, Arrival::Move),
            None => break,
        }
    }
    // Stealing takes the most owed task another CPU has (the smallest
    // virtual runtime), placed as far from this CPU's smallest as it was
    // from its own (taken under that queue's lock).
    let stolen = (0..smp::MAX_CPUS).filter(|&i| i != cpu.index).filter_map(smp::by_index).find_map(|other| {
        let mut rq = other.sched.rq.lock();
        let omin = other.sched.min_vruntime.load(Ordering::Relaxed);
        let i = rq
            .iter()
            .enumerate()
            .filter(|(_, t)| t.may_run_on(cpu.index) && !t.on_cpu.load(Ordering::Acquire))
            .min_by_key(|(_, t)| t.vruntime.load(Ordering::Relaxed).wrapping_sub(omin) as i64)
            .map(|(i, _)| i)?;
        let t = rq.remove(i)?;
        let lag = if t.vcpu.load(Ordering::Relaxed) == other.index { t.vruntime.load(Ordering::Relaxed).wrapping_sub(omin) } else { 0 };
        Some((t, lag))
    });
    stolen.map(|(t, lag)| {
        t.vcpu.store(cpu.index, Ordering::Relaxed);
        t.vruntime.store(cpu.sched.min_vruntime.load(Ordering::Relaxed).wrapping_add(lag), Ordering::Relaxed);
        t
    })
}

/// The weight of each nice value, -20 to 19 (Linux's
/// `sched_prio_to_weight`): each step is about 10% of CPU time.
const WEIGHTS: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904, 3906, 3121, 2501, 1991, 1586, 1277,
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];
/// The weight of nice 0.
const DEFAULT_WEIGHT: u32 = 1024;
/// The period in which every runnable task of a CPU runs once (shared by
/// weight), the shortest time slice, and how far a woken task must be
/// behind the running one to preempt it (as Linux's sched_latency,
/// min_granularity and wakeup_granularity on a few CPUs).
const LATENCY: u64 = 12_000_000;
const MIN_GRANULARITY: u64 = 1_500_000;
const WAKEUP_GRANULARITY: u64 = 2_000_000;

/// The scheduling weight of nice value `nice`.
pub fn weight(nice: i8) -> u32 {
    WEIGHTS[(nice.clamp(-20, 19) + 20) as usize]
}

fn task_weight(t: &Task) -> u32 {
    weight(t.nice.load(Ordering::Relaxed))
}

/// The virtual time `ns` of running is for a task of weight `w`: the
/// heavier the task, the slower its virtual time goes.
fn vtime(ns: u64, w: u32) -> u64 {
    (ns as u128 * DEFAULT_WEIGHT as u128 / w.max(1) as u128) as u64
}

/// The weight a task of the Linux server holding one of its locks runs
/// with (that of nice -20; see `server_locks`).
const BOOST_WEIGHT: u32 = WEIGHTS[0];

/// How many of the Linux server's locks `t` holds now (0 for a task that
/// is no Linux thread): the word the server keeps in the thread's State
/// page (`restricted::SERVER_LOCKS_OFFSET`).
fn server_locks(t: &Task) -> u32 {
    locks_at(t.server_locks.load(Ordering::Relaxed))
}

fn locks_at(word: u64) -> u32 {
    if word == 0 {
        return 0;
    }
    // A word of the thread's State page, mapped while the task lives.
    unsafe { (*(word as *const core::sync::atomic::AtomicU32)).load(Ordering::Relaxed) }
}

/// The weight `t` runs with: its nice value's, or `BOOST_WEIGHT` while it
/// holds a server lock: then it gets the CPU as the most favored program
/// does (and no more), so a low nice value cannot keep it in the lock
/// while threads of its instance wait (priority inversion).
fn effective_weight(t: &Task) -> u32 {
    if server_locks(t) > 0 { BOOST_WEIGHT } else { task_weight(t) }
}

/// Wrap-safe order of virtual runtimes: whether `a` is before `b`.
fn vbefore(a: u64, b: u64) -> bool {
    (a.wrapping_sub(b) as i64) < 0
}

fn vmax(a: u64, b: u64) -> u64 {
    if vbefore(a, b) { b } else { a }
}

fn vmin(a: u64, b: u64) -> u64 {
    if vbefore(a, b) { a } else { b }
}

/// A task that leaves the CPU, or comes back to a queue, holding a server
/// lock is placed at the front of the CPU's virtual time (at its smallest)
/// so it runs again soon and lets go of the lock: the instance's threads
/// may wait for it. (It gets no more than the most favored program would.)
fn front_if_holding(t: &Task, cpu: &Cpu) {
    if server_locks(t) > 0 {
        let min = cpu.sched.min_vruntime.load(Ordering::Relaxed);
        t.vruntime.store(vmin(t.vruntime.load(Ordering::Relaxed), min), Ordering::Relaxed);
    }
}

/// The running task's virtual runtime on `cpu` at `now` (None: it idles),
/// read consistently against the CPU updating it (a sequence count).
fn curr_state(cpu: &Cpu, now: u64) -> Option<u64> {
    let s = &cpu.sched;
    loop {
        let seq = s.curr_seq.load(Ordering::Acquire);
        if seq & 1 == 1 {
            core::hint::spin_loop();
            continue;
        }
        let idle = s.curr_idle.load(Ordering::Relaxed);
        let (v, since, w) = (s.curr_vruntime.load(Ordering::Relaxed), s.curr_since.load(Ordering::Relaxed), s.curr_weight.load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        if s.curr_seq.load(Ordering::Relaxed) != seq {
            continue;
        }
        if idle {
            return None;
        }
        return Some(v.wrapping_add(vtime(now.saturating_sub(since), w)));
    }
}

/// Writes the running task's state (its own CPU, interrupts off).
fn set_curr(cpu: &Cpu, v: u64, since: u64, w: u32, idle: bool) {
    let s = &cpu.sched;
    s.curr_seq.fetch_add(1, Ordering::Relaxed);
    fence(Ordering::Release);
    s.curr_vruntime.store(v, Ordering::Relaxed);
    s.curr_since.store(since, Ordering::Relaxed);
    s.curr_weight.store(w, Ordering::Relaxed);
    s.curr_idle.store(idle, Ordering::Relaxed);
    s.curr_seq.fetch_add(1, Ordering::Release);
}

/// Puts `t`'s virtual runtime on `cpu`'s scale: what it was ahead of or
/// behind its old CPU's smallest, it is of this one's; a new task starts a
/// slice's worth after the smallest (as CFS's START_DEBIT: a fork loop
/// gains nothing).
fn rebase(t: &Task, cpu: &Cpu) {
    let min = cpu.sched.min_vruntime.load(Ordering::Relaxed);
    let from = t.vcpu.swap(cpu.index, Ordering::Relaxed);
    let v = t.vruntime.load(Ordering::Relaxed);
    let placed = match from {
        usize::MAX => min.wrapping_add(vtime(MIN_GRANULARITY, task_weight(t))),
        f if f == cpu.index => v,
        f => {
            let old = smp::by_index(f).map_or(min, |c| c.sched.min_vruntime.load(Ordering::Relaxed));
            min.wrapping_add(v.wrapping_sub(old))
        }
    };
    t.vruntime.store(placed, Ordering::Relaxed);
}

/// Brings the running task's virtual runtime and the CPU's smallest up to
/// `now` (Linux's update_curr; every tick, slice end and schedule): what
/// it ran since counts with the weight it had (a nice value or a server
/// lock changes it from here on). Interrupts off, on its CPU.
fn update_curr(cpu: &Cpu, cur: &Task, now: u64) {
    let s = &cpu.sched;
    if !cur.idle {
        let since = s.curr_since.load(Ordering::Relaxed);
        // Holding a server lock now: the time is counted as a holder's
        // (the lock was taken since, as like as not).
        let w = if server_locks(cur) > 0 { BOOST_WEIGHT } else { s.curr_weight.load(Ordering::Relaxed) };
        let v = cur.vruntime.load(Ordering::Relaxed).wrapping_add(vtime(now.saturating_sub(since), w));
        cur.vruntime.store(v, Ordering::Relaxed);
        set_curr(cpu, v, now, effective_weight(cur), false);
    }
    update_min(cpu, cur);
}

/// Moves the CPU's smallest virtual runtime up to its tasks' (it never
/// goes back: new and woken tasks are placed from it). Its own CPU only.
fn update_min(cpu: &Cpu, cur: &Task) {
    let queued = cpu
        .sched
        .rq
        .lock()
        .iter()
        .filter(|t| t.vcpu.load(Ordering::Relaxed) == cpu.index)
        .map(|t| t.vruntime.load(Ordering::Relaxed))
        .reduce(|a, b| if vbefore(b, a) { b } else { a });
    let running = (!cur.idle).then(|| cur.vruntime.load(Ordering::Relaxed));
    let lowest = match (queued, running) {
        (Some(a), Some(b)) => if vbefore(a, b) { a } else { b },
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return,
    };
    let min = &cpu.sched.min_vruntime;
    min.store(vmax(min.load(Ordering::Relaxed), lowest), Ordering::Relaxed);
}

/// The time slice of a task of weight `w` on `cpu`: its share of
/// `LATENCY` among the tasks queued there, at least `MIN_GRANULARITY`;
/// None while nothing else waits.
fn slice(cpu: &Cpu, w: u32) -> Option<u64> {
    let others: u64 = cpu.sched.rq.lock().iter().filter(|t| t.may_run_on(cpu.index)).map(|t| task_weight(t) as u64).sum();
    if others == 0 {
        return None;
    }
    Some((LATENCY * w as u64 / (w as u64 + others)).max(MIN_GRANULARITY))
}

/// `t` starts (or goes on) running on this CPU at `now`: its slice's end
/// is programmed.
fn start_slice(cpu: &Cpu, t: &Task, now: u64) {
    let w = effective_weight(t);
    set_curr(cpu, t.vruntime.load(Ordering::Relaxed), now, w, t.idle);
    cpu.sched.slice_start.store(now, Ordering::Relaxed);
    let end = if t.idle { None } else { slice(cpu, w) };
    crate::timer::set_slice_end(end.map_or(u64::MAX, |d| now + d));
}

/// Whether the running task's time slice is over at `now` (after its
/// virtual runtime was brought up to date): always for the idle task, else
/// when it ran its share since others came (a slice that began alone has
/// no end of its own: the tick asks).
fn slice_over(cpu: &Cpu, cur: &Task, now: u64) -> bool {
    if cur.idle {
        return true;
    }
    let ran = now.saturating_sub(cpu.sched.slice_start.load(Ordering::Relaxed));
    slice(cpu, effective_weight(cur)).is_some_and(|s| ran >= s)
}

/// The timer's slice end on this CPU: whether to switch.
pub fn slice_end() -> bool {
    let cpu = smp::cpu();
    let cur = current();
    let now = crate::time::now();
    update_curr(cpu, cur, now);
    slice_over(cpu, cur, now)
}

/// How a task comes into a run queue.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arrival {
    /// It woke up: it may preempt, and what it slept is credited up to
    /// half a `LATENCY`.
    Wake,
    /// A new task: it may preempt.
    New,
    /// Moved between CPUs.
    Move,
}

/// Time and switches of one CPU.
pub struct CpuStats {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
    pub switches: u64,
    pub queued: usize,
}

pub fn cpu_stats(cpu: &Cpu) -> CpuStats {
    let s = &cpu.sched;
    CpuStats {
        user: s.user_ticks.load(Ordering::Relaxed),
        system: s.system_ticks.load(Ordering::Relaxed),
        idle: s.idle_ticks.load(Ordering::Relaxed),
        switches: s.switches.load(Ordering::Relaxed),
        queued: s.rq.lock().len(),
    }
}

/// Load average as Linux keeps it: exponentially decaying averages of the
/// number of runnable tasks over 1, 5 and 15 minutes, in fixed point with
/// 11 fractional bits, updated every 5 seconds.
pub const LOAD_SHIFT: u32 = 11;
const LOAD_INTERVAL: u64 = 5 * crate::time::NSEC_PER_SEC;
static NEXT_LOAD: AtomicU64 = AtomicU64::new(LOAD_INTERVAL);
const LOAD_ONE: u64 = 1 << LOAD_SHIFT;
const LOAD_EXP: [u64; 3] = [1884, 2014, 2037];
static LOAD: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub fn loadavg() -> [u64; 3] {
    [0, 1, 2].map(|i| LOAD[i].load(Ordering::Relaxed))
}

fn update_load(runnable: u64) {
    for (i, exp) in LOAD_EXP.iter().enumerate() {
        let old = LOAD[i].load(Ordering::Relaxed);
        let new = (old * exp + runnable * LOAD_ONE * (LOAD_ONE - exp)) >> LOAD_SHIFT;
        LOAD[i].store(new, Ordering::Relaxed);
    }
}

/// Whether this CPU has work: its own queue, or a task in another CPU's
/// queue that may run here (`pick_next` steals it). Tasks only another
/// CPU may run (pinned there) are not this CPU's work: counting them kept
/// an idle CPU looping through `schedule` with interrupts off while a CPU
/// it cannot help was busy, so its timers (sleeps ending) never fired.
fn has_work(cpu: &Cpu) -> bool {
    !cpu.sched.rq.lock().is_empty()
        || (0..smp::MAX_CPUS)
            .filter(|&i| i != cpu.index)
            .filter_map(smp::by_index)
            .any(|c| c.sched.rq.lock().iter().any(|t| t.may_run_on(cpu.index)))
}

// ---------------------------------------------------------- wait queues

/// A wait queue: who waits on which channel. The global queues below are
/// shared by all channels that hash to them.
pub struct WaitQueue {
    waiters: IrqSpinLock<Vec<Waiter>>,
}

/// A task in `prepare_to_wait`, woken if it still waits on the channel.
struct Waiter {
    chan: usize,
    task: Arc<Task>,
}

impl WaitQueue {
    pub const fn new() -> WaitQueue {
        WaitQueue { waiters: IrqSpinLock::new(Vec::new()) }
    }

    /// Wakes the tasks waiting on `chan`.
    pub fn wake(&self, chan: usize) {
        let q = self.waiters.lock();
        for w in q.iter().filter(|w| w.chan == chan) {
            wake_if(&w.task, |t| t.wait_chan.load(Ordering::Relaxed) == chan);
        }
    }
}

const BUCKETS: usize = 64;
static WAITQ: [WaitQueue; BUCKETS] = [const { WaitQueue::new() }; BUCKETS];

/// The global wait queue that holds the waiters of `chan`.
pub fn queue_of(chan: usize) -> &'static WaitQueue {
    let h = chan ^ (chan >> 7) ^ (chan >> 17) ^ (chan >> 33);
    &WAITQ[h % BUCKETS]
}

/// A prepared sleep on a channel; see the module comment. Dropping it ends
/// the wait (the task is Running again and leaves the channel).
pub struct Wait {
    chan: usize,
    _not_send: core::marker::PhantomData<*const ()>,
}

/// Channel for sleeps that only a deadline (or a signal) ends.
fn private_chan(pid: Pid) -> usize {
    0x4_0000_0000 + pid as usize
}

pub fn prepare_to_wait(chan: usize) -> Wait {
    let me = current_arc();
    let mut q = queue_of(chan).waiters.lock();
    if !q.iter().any(|w| w.chan == chan && Arc::ptr_eq(&w.task, &me)) {
        q.push(Waiter { chan, task: me.clone() });
    }
    let _w = me.wake_lock.lock();
    me.wait_chan.store(chan, Ordering::Relaxed);
    me.set_state(State::Sleeping);
    drop(_w);
    drop(q);
    Wait { chan, _not_send: core::marker::PhantomData }
}

/// Prepares a sleep that ends only at a deadline or by a signal.
pub fn prepare_to_sleep() -> Wait {
    prepare_to_wait(private_chan(current().tid()))
}

impl Wait {
    pub fn sleep(self) {
        schedule();
    }

    /// Sleeps at most until `deadline` (nanoseconds since boot). A
    /// deadline that has passed ends the wait at once.
    pub fn sleep_until(self, deadline: u64) {
        if deadline <= crate::time::now() {
            return;
        }
        crate::timer::wake_at(&current_arc(), deadline);
        schedule();
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        let me = current();
        {
            let _w = me.wake_lock.lock();
            me.set_state(State::Running);
            me.wait_chan.store(0, Ordering::Relaxed);
        }
        crate::timer::disarm(me);
        let chan = self.chan;
        queue_of(chan).waiters.lock().retain(|w| !(w.chan == chan && core::ptr::eq(&*w.task, me)));
    }
}

/// Wakes every task sleeping on `chan`.
/// Safe in interrupt context.
pub fn wakeup(chan: usize) {
    queue_of(chan).wake(chan);
}

/// Moves `t` from state `from` (Sleeping, or Stopped for SIGCONT/SIGKILL)
/// back to running. Returns whether it did.
pub fn try_wake(t: &Arc<Task>, from: State) -> bool {
    let _w = t.wake_lock.lock();
    if t.state() != from {
        return false;
    }
    make_runnable(t)
}

/// Wakes `t` if it is sleeping and `still_waiting` holds (checked under
/// its wake lock, which serializes it with the sleep ending).
fn wake_if(t: &Arc<Task>, still_waiting: impl Fn(&Task) -> bool) -> bool {
    let _w = t.wake_lock.lock();
    if t.state() != State::Sleeping || !still_waiting(t) {
        return false;
    }
    make_runnable(t)
}

/// The rest of a wakeup, under the task's wake lock.
fn make_runnable(t: &Arc<Task>) -> bool {
    if t.on_rq.load(Ordering::Acquire) {
        // Still on its CPU, not descheduled yet: it simply keeps running.
        t.set_state(State::Running);
        return true;
    }
    t.set_state(State::Runnable);
    t.on_rq.store(true, Ordering::Release);
    enqueue(t.clone(), Arrival::Wake);
    true
}

/// Queues a new task for the first time.
pub fn start(task: Arc<Task>) {
    task.on_rq.store(true, Ordering::Relaxed);
    task.set_state(State::Runnable);
    task.last_cpu.store(smp::cpu().index, Ordering::Relaxed);
    enqueue(task, Arrival::New);
}

// ------------------------------------------------------------- switching

/// Picks the next task for this CPU and switches to it. The current task
/// stays runnable (back of the queue) unless it prepared to sleep, stopped
/// or exited.
pub fn schedule() {
    interrupts::without_interrupts(|| {
        let cpu = smp::cpu();
        let cs = &cpu.sched;
        cs.need_resched.store(false, Ordering::Relaxed);
        let cur = current();
        let now = crate::time::now();
        update_curr(cpu, cur, now);
        {
            let _w = cur.wake_lock.lock();
            match cur.state() {
                State::Running if !cur.idle && cur.may_run_on(cpu.index) => {
                    front_if_holding(cur, cpu);
                    cur.set_state(State::Runnable);
                    cs.rq.lock().push_back(current_arc());
                }
                // Its affinity excludes this CPU now: move it.
                State::Running if !cur.idle => {
                    cur.set_state(State::Runnable);
                    enqueue(current_arc(), Arrival::Move);
                }
                State::Running | State::Runnable => {}
                // Sleeping, stopped or dead: leave the run queues.
                _ => cur.on_rq.store(false, Ordering::Release),
            }
        }
        update_min(cpu, cur);
        let next = match pick_next(cpu) {
            Some(next) => next,
            None => unsafe { (*cs.idle.get()).clone().expect("no idle task") },
        };
        if core::ptr::eq(&*next, cur) {
            cur.set_state(State::Running);
            start_slice(cpu, cur, now);
            return;
        }
        context_switch(next);
    });
}

/// Asks this CPU to switch tasks at its next return to user space.
pub fn set_need_resched() {
    smp::cpu().sched.need_resched.store(true, Ordering::Relaxed);
}

/// Every return to user space: switches tasks if one is due. The kernel
/// is not preemptive, so this is where a task that woke up while the CPU
/// was busy in the kernel (in a syscall, or with interrupts held off) gets
/// its turn; without it, the request would be lost until a timer interrupt
/// happened to land in user mode.
pub fn resched_on_return() {
    if smp::cpu().sched.need_resched.load(Ordering::Relaxed) {
        schedule();
    }
}

/// A voluntary preemption point for long kernel work (as Linux's
/// `cond_resched`): switches tasks if one is due. Only where interrupts
/// are on, which rules out interrupt handlers and holders of interrupt-safe
/// locks; callers hold no other spinlock either.
pub fn cond_resched() {
    if interrupts::are_enabled() {
        resched_on_return();
    }
}

/// Switches from the current task to `next` (interrupts are off).
fn context_switch(next: Arc<Task>) {
    let cpu = smp::cpu();
    let cs = &cpu.sched;
    // Its previous CPU may still be on its stack, finishing the switch away.
    while next.on_cpu.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    next.on_cpu.store(true, Ordering::Relaxed);
    next.set_state(State::Running);
    next.last_cpu.store(cpu.index, Ordering::Relaxed);
    cs.switches.fetch_add(1, Ordering::Relaxed);

    let prev = unsafe { (*cs.current.get()).take().expect("no current task") };
    let now = crate::time::now();
    prev.stop_running(now);
    next.start_running(now);
    start_slice(cpu, &next, now);
    unsafe {
        let saved = prev.cpu_state();
        saved.fs_base = FsBase::read().as_u64();
        fxsave(&mut saved.fpu);
        let n = next.own();
        // Idle loops and kernel tasks load the kernel's tables: a CPU has a
        // user address space loaded only while it runs one of its tasks.
        // Threads of one process switch without reloading CR3.
        let from = prev.own().mm.as_ref().map(|m| &*m.tlb);
        // A Linux thread in its server sees the normal view.
        let server = n.linux.as_ref().is_some_and(|l| l.normal_view());
        super::tlb::switch(from, n.mm.as_ref().map(|m| &*m.tlb), server);
        if let Some(top) = next.kstack_top() {
            cpu.set_kernel_stack(top);
        }
        cpu.tables().set_io_bitmap(n.io_bitmap.as_deref());
        let restored = next.cpu_state();
        FsBase::write(VirtAddr::new(restored.fs_base));
        fxrstor(&restored.fpu);
    }
    let prev_rsp = prev.kernel_rsp.get();
    let next_rsp = unsafe { *next.kernel_rsp.get() };
    // No counted reference to either task stays on this stack: a task that
    // exits never comes back here to drop it.
    unsafe {
        *cs.prev.get() = Some(prev);
        *cs.current.get() = Some(next);
        switch_stacks(prev_rsp, next_rsp);
    }
    finish_switch();
}

/// Runs on the new task's stack right after a switch, on whatever CPU that
/// is now: the previous task's stack is free from here on.
extern "C" fn finish_switch() {
    let cs = &smp::cpu().sched;
    if let Some(prev) = unsafe { (*cs.prev.get()).take() } {
        prev.on_cpu.store(false, Ordering::Release);
    }
}

unsafe fn fxsave(area: &mut super::task::FpuState) {
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) area.0.as_mut_ptr(), options(nostack)) };
}

unsafe fn fxrstor(area: &super::task::FpuState) {
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) area.0.as_ptr(), options(nostack)) };
}

/// Saves the callee-saved registers on the current kernel stack, stores
/// rsp to `*save` and continues on stack `next`.
#[unsafe(naked)]
unsafe extern "sysv64" fn switch_stacks(save: *mut u64, next: u64) {
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}

/// First code of a new user task: finish the switch, then enter ring 3
/// through the frame prepared on its stack.
#[unsafe(naked)]
unsafe extern "C" fn task_first_run() {
    core::arch::naked_asm!(
        "call {finish}",
        "jmp {ret}",
        finish = sym finish_switch,
        ret = sym super::syscall::user_return,
    );
}

/// First code of an idle task with its own stack.
#[unsafe(naked)]
unsafe extern "C" fn idle_first_run() {
    core::arch::naked_asm!(
        "call {finish}",
        "call {idle}",
        "ud2",
        finish = sym finish_switch,
        idle = sym idle_loop,
    );
}

/// First code of a kernel thread: finish the switch, then run the
/// function `prepare_kernel_stack` left in rbx, with interrupts on.
#[unsafe(naked)]
unsafe extern "C" fn kthread_first_run() {
    core::arch::naked_asm!("call {finish}", "sti", "call rbx", "ud2", finish = sym finish_switch);
}

/// Prepares `stack` so that the first switch to it runs `f` (a kernel
/// thread). Returns the saved rsp.
pub fn prepare_kernel_stack(stack: &KernelStack, f: fn() -> !) -> u64 {
    let rsp = prepare_stack(stack, None, false);
    unsafe {
        // switch_stacks pops r15, r14, r13, r12, rbp, rbx, then returns.
        ((rsp + 40) as *mut u64).write(f as usize as u64);
        ((rsp + 48) as *mut u64).write(kthread_first_run as *const () as u64);
    }
    rsp
}

/// Starts a kernel thread running `f` (no address space, no signals, not
/// in the process table).
pub fn spawn_kernel_thread(name: &str, f: fn() -> !) -> Result<(), i64> {
    static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let stack = KernelStack::new(false).ok_or(super::errno::ENOMEM)?;
    let rsp = prepare_kernel_stack(&stack, f);
    // Far above any pid, but within 32 bits like pids (the channels made
    // from a task id, as `private_chan`, add it to a 32-bit base).
    let id = 0xffff_0000 + NEXT.fetch_add(1, Ordering::Relaxed);
    let task = Task::kernel_thread(id, name, stack, rsp).ok_or(super::errno::ENOMEM)?;
    start(task);
    Ok(())
}

/// Prepares `stack` so that the first switch to it runs `entry`; `frame`
/// (if any) is placed at the top for `user_return`. Returns the saved rsp.
pub fn prepare_stack(stack: &KernelStack, frame: Option<super::syscall::Frame>, user: bool) -> u64 {
    let top = stack.top();
    let frame_addr = match frame {
        Some(f) => {
            let a = top - core::mem::size_of::<super::syscall::Frame>() as u64;
            unsafe { (a as *mut super::syscall::Frame).write(f) };
            a
        }
        None => top - 16,
    };
    let entry = if user { task_first_run as *const () as u64 } else { idle_first_run as *const () as u64 };
    unsafe {
        // Expected by switch_stacks: r15..rbx (6 words) and a return address.
        ((frame_addr - 8) as *mut u64).write(entry);
        for i in 1..=6 {
            ((frame_addr - 8 - i * 8) as *mut u64).write(0);
        }
    }
    frame_addr - 56
}

/// A new idle task with its own stack (for the bootstrap CPU, whose boot
/// stack belongs to the kernel monitor).
pub fn new_idle_task(cpu: usize) -> Arc<Task> {
    let stack = KernelStack::new(false).expect("no kernel stack for an idle task");
    let rsp = prepare_stack(&stack, None, false);
    Arc::new(Task::idle_task(cpu, Some(stack), rsp))
}

/// What a CPU does when nothing is runnable: halt until an interrupt, then
/// look for work (its own queue or another CPU's).
pub extern "C" fn idle_loop() -> ! {
    loop {
        interrupts::disable();
        let cpu = smp::cpu();
        cpu.sched.halted.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if has_work(cpu) {
            cpu.sched.halted.store(false, Ordering::SeqCst);
            schedule();
            continue;
        }
        // sti; hlt — an interrupt between the check and hlt still wakes it.
        interrupts::enable_and_hlt();
        smp::cpu().sched.halted.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- timer

/// The scheduler tick (see `timer`), every 10 ms on every CPU; `user`
/// tells whether it hit user code. It samples where time goes; the
/// bootstrap CPU also updates the load average. Whether the running
/// task's time slice is over (`slice`).
pub fn tick(user: bool) -> bool {
    let cpu = smp::cpu();
    let cur = current();
    // Whether the running task's time slice is over: the idle task's
    // always; another's when it ran its share since others came (a slice
    // that began with nothing else queued has no end of its own).
    let now = crate::time::now();
    update_curr(cpu, cur, now);
    let over = slice_over(cpu, cur, now);
    let (cpu_counter, task_counter) = match (cur.idle, user) {
        (true, _) => (&cpu.sched.idle_ticks, None),
        (false, true) => (&cpu.sched.user_ticks, Some(&cur.utime)),
        (false, false) => (&cpu.sched.system_ticks, Some(&cur.stime)),
    };
    cpu_counter.fetch_add(1, Ordering::Relaxed);
    if let Some(c) = task_counter {
        c.fetch_add(1, Ordering::Relaxed);
    }
    if cpu.index != 0 {
        return over;
    }
    let now = crate::time::now();
    if now >= NEXT_LOAD.load(Ordering::Relaxed) {
        NEXT_LOAD.store(now + LOAD_INTERVAL, Ordering::Relaxed);
        let runnable = TABLE
            .lock()
            .tasks
            .values()
            .filter(|t| matches!(t.state(), State::Running | State::Runnable) && t.tid() != 0)
            .count();
        update_load(runnable as u64);
    }
    over
}
