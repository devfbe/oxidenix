//! The SMP scheduler: per-CPU run queues, wait queues, context switches.
//!
//! Lock order (outer to inner): process table → task info → task signals →
//! wait-queue bucket → task wake_lock → run queue. The heap and the frame
//! allocator are leaves.
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

use super::task::{KernelStack, State, Task};
use super::Pid;
use crate::smp::{self, Cpu};
use crate::sync::IrqSpinLock;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

pub const MAX_PROCS: usize = 256;

/// All processes by pid (idle tasks are not listed).
pub struct Table {
    pub tasks: BTreeMap<Pid, Arc<Task>>,
    pub next_pid: Pid,
    /// Pids handed out whose task is still being built: they count against
    /// the limit, so concurrent forks on several CPUs cannot exceed it.
    pub reserved: usize,
}

pub static TABLE: IrqSpinLock<Table> = IrqSpinLock::new(Table { tasks: BTreeMap::new(), next_pid: 1, reserved: 0 });

/// A pid taken under the process limit; `insert` turns it into a listed
/// task, dropping it gives the slot back.
pub struct PidReservation {
    pub pid: Pid,
    done: bool,
}

/// Takes the next pid, or EAGAIN at the process limit.
pub fn reserve_pid() -> Result<PidReservation, i64> {
    let mut table = TABLE.lock();
    if table.tasks.len() + table.reserved >= MAX_PROCS {
        return Err(super::errno::EAGAIN);
    }
    let pid = table.next_pid;
    table.next_pid += 1;
    table.reserved += 1;
    Ok(PidReservation { pid, done: false })
}

impl PidReservation {
    pub fn insert(mut self, task: Arc<Task>) {
        let mut table = TABLE.lock();
        table.tasks.insert(self.pid, task);
        table.reserved -= 1;
        self.done = true;
    }
}

impl Drop for PidReservation {
    fn drop(&mut self) {
        if !self.done {
            TABLE.lock().reserved -= 1;
        }
    }
}

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
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
    /// Timer ticks spent running tasks and idling, and context switches.
    pub busy_ticks: AtomicU64,
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
            busy_ticks: AtomicU64::new(0),
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
    unsafe {
        *cs.current.get() = Some(task);
        *cs.idle.get() = Some(idle);
    }
    // Run queues are filled from interrupt context too: never reallocate.
    let _ = cs.rq.lock().try_reserve(MAX_PROCS);
}

// ---------------------------------------------------------------- queues

fn halted(cpu: &Cpu) -> bool {
    cpu.sched.halted.load(Ordering::SeqCst)
}

/// Claims a halted CPU for new work: the first enqueuer to clear its flag
/// wakes it, the next one picks another idle CPU instead of piling on.
fn claim(cpu: &Cpu) -> bool {
    cpu.sched.halted.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Puts a runnable task into the run queue of a CPU it may run on: an idle
/// one if there is one (preferring the task's last CPU, and claiming it so
/// that concurrent wakeups spread out), else its last CPU's, else the first
/// allowed.
fn enqueue(task: Arc<Task>) {
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
    target.sched.rq.lock().push_back(task);
    // Pairs with the fence in `idle_loop`: either the idle CPU sees the
    // task, or we see it halted and wake it.
    fence(Ordering::SeqCst);
    let wake = claimed.is_some() || claim(target);
    if wake && target.index != smp::cpu().index {
        crate::interrupts::apic::ipi::send_vector(target.apic_id(), crate::interrupts::apic::ipi::RESCHEDULE_VECTOR);
    }
}

/// The next task for this CPU: its own queue first, else one stolen from
/// the back of another CPU's queue (one that may run here). A task whose
/// affinity no longer allows this CPU moves on to one it may run on.
fn pick_next(cpu: &Cpu) -> Option<Arc<Task>> {
    loop {
        let t = cpu.sched.rq.lock().pop_front();
        match t {
            Some(t) if t.may_run_on(cpu.index) => return Some(t),
            Some(t) => enqueue(t),
            None => break,
        }
    }
    (0..smp::MAX_CPUS).filter(|&i| i != cpu.index).filter_map(smp::by_index).find_map(|other| {
        let mut rq = other.sched.rq.lock();
        let i = rq.iter().rposition(|t| t.may_run_on(cpu.index))?;
        rq.remove(i)
    })
}

/// (busy ticks, idle ticks, context switches, queued tasks) of a CPU.
pub fn cpu_stats(cpu: &Cpu) -> (u64, u64, u64, usize) {
    let s = &cpu.sched;
    (s.busy_ticks.load(Ordering::Relaxed), s.idle_ticks.load(Ordering::Relaxed), s.switches.load(Ordering::Relaxed), s.rq.lock().len())
}

fn has_work(cpu: &Cpu) -> bool {
    (0..smp::MAX_CPUS).filter_map(smp::by_index).any(|c| !c.sched.rq.lock().is_empty()) || !cpu.sched.rq.lock().is_empty()
}

// ---------------------------------------------------------- wait queues

const BUCKETS: usize = 64;
static WAITQ: [IrqSpinLock<Vec<Arc<Task>>>; BUCKETS] = [const { IrqSpinLock::new(Vec::new()) }; BUCKETS];

fn bucket(chan: usize) -> &'static IrqSpinLock<Vec<Arc<Task>>> {
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
    let mut q = bucket(chan).lock();
    if !q.iter().any(|t| Arc::ptr_eq(t, &me)) {
        q.push(me.clone());
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
    prepare_to_wait(private_chan(current().pid))
}

impl Wait {
    pub fn sleep(self) {
        schedule();
    }

    /// Sleeps at most until timer tick `deadline`.
    pub fn sleep_until(self, deadline: u64) {
        current().wake_at.store(deadline.max(1), Ordering::Relaxed);
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
            me.wake_at.store(0, Ordering::Relaxed);
        }
        bucket(self.chan).lock().retain(|t| !core::ptr::eq(&**t, me));
    }
}

/// Wakes every task sleeping on `chan`. Safe in interrupt context.
pub fn wakeup(chan: usize) {
    let q = bucket(chan).lock();
    for t in q.iter() {
        if t.wait_chan.load(Ordering::Relaxed) == chan {
            try_wake(t, State::Sleeping);
        }
    }
}

/// Moves `t` from state `from` (Sleeping, or Stopped for SIGCONT/SIGKILL)
/// back to running. Returns whether it did.
pub fn try_wake(t: &Arc<Task>, from: State) -> bool {
    let _w = t.wake_lock.lock();
    if t.state() != from {
        return false;
    }
    t.wake_at.store(0, Ordering::Relaxed);
    if t.on_rq.load(Ordering::Acquire) {
        // Still on its CPU, not descheduled yet: it simply keeps running.
        t.set_state(State::Running);
        return true;
    }
    t.set_state(State::Runnable);
    t.on_rq.store(true, Ordering::Release);
    enqueue(t.clone());
    true
}

/// Queues a new task for the first time.
pub fn start(task: Arc<Task>) {
    task.on_rq.store(true, Ordering::Relaxed);
    task.set_state(State::Runnable);
    task.last_cpu.store(smp::cpu().index, Ordering::Relaxed);
    enqueue(task);
}

// ------------------------------------------------------------- switching

/// Picks the next task for this CPU and switches to it. The current task
/// stays runnable (back of the queue) unless it prepared to sleep, stopped
/// or exited.
pub fn schedule() {
    interrupts::without_interrupts(|| {
        let cpu = smp::cpu();
        let cs = &cpu.sched;
        let cur = current();
        {
            let _w = cur.wake_lock.lock();
            match cur.state() {
                State::Running if !cur.idle && cur.may_run_on(cpu.index) => {
                    cur.set_state(State::Runnable);
                    cs.rq.lock().push_back(current_arc());
                }
                // Its affinity excludes this CPU now: move it.
                State::Running if !cur.idle => {
                    cur.set_state(State::Runnable);
                    enqueue(current_arc());
                }
                State::Running | State::Runnable => {}
                // Sleeping, stopped or dead: leave the run queues.
                _ => cur.on_rq.store(false, Ordering::Release),
            }
        }
        let next = match pick_next(cpu) {
            Some(next) => next,
            None => unsafe { (*cs.idle.get()).clone().expect("no idle task") },
        };
        if core::ptr::eq(&*next, cur) {
            cur.set_state(State::Running);
            return;
        }
        context_switch(next);
    });
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
    unsafe {
        let p = prev.own();
        p.fs_base = FsBase::read().as_u64();
        fxsave(&mut p.fpu);
        let n = next.own();
        match &n.space {
            Some(space) => space.activate(),
            // Idle loops and kernel tasks: no user address space stays loaded,
            // so an address space is only ever active on the CPU running it.
            None => Cr3::write(crate::memory::kernel_l4(), Cr3Flags::empty()),
        }
        if let Some(top) = next.kstack_top() {
            cpu.set_kernel_stack(top);
        }
        cpu.tables().set_io_bitmap(n.io_bitmap.as_deref());
        FsBase::write(VirtAddr::new(n.fs_base));
        fxrstor(&n.fpu);
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

/// Prepares `stack` so that the first switch to it runs `entry`; `frame`
/// (if any) is placed at the top for `user_return`. Returns the saved rsp.
pub fn prepare_stack(stack: &mut KernelStack, frame: Option<super::syscall::Frame>, user: bool) -> u64 {
    let top = stack.0.as_mut_ptr() as u64 + stack.0.len() as u64;
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
    let mut stack = unsafe { Box::<KernelStack>::new_zeroed().assume_init() };
    let rsp = prepare_stack(&mut stack, None, false);
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

/// Called on every CPU's timer interrupt. The bootstrap CPU keeps the
/// global time: ticks, sleep deadlines and interval timers.
pub fn tick() {
    let cpu = smp::cpu();
    let counter = if current().idle { &cpu.sched.idle_ticks } else { &cpu.sched.busy_ticks };
    counter.fetch_add(1, Ordering::Relaxed);
    if cpu.index != 0 {
        return;
    }
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let mut due: heapless::Vec<Arc<Task>, MAX_PROCS> = heapless::Vec::new();
    {
        let table = TABLE.lock();
        for t in table.tasks.values() {
            let deadline = t.wake_at.load(Ordering::Relaxed);
            if deadline != 0 && deadline <= now {
                let _ = due.push(t.clone());
            }
        }
    }
    for t in &due {
        try_wake(t, State::Sleeping);
    }
    super::signal::expire_alarms(now);
}
