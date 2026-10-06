//! A task: one process (or a CPU's idle loop) as the scheduler sees it.
//!
//! Its state splits by who may touch it:
//! - `Process` (address space, descriptors, cwd, FPU, ...) belongs to the
//!   task itself: only the CPU running it, the CPU switching to or from it,
//!   or the reaper once it is dead and off every CPU may use it.
//! - `info` (relations, name, exit/stop reports) and `sig` (signal state and
//!   timer) are shared and locked.
//! - The scheduling fields are atomics, written under `wake_lock`.

use super::address_space::AddressSpace;
use super::signal::Signals;
use super::{FdEntry, Pid, Server};
use crate::interrupts::gdt;
use crate::sync::IrqSpinLock;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

pub const KSTACK_SIZE: usize = 64 * 1024;

#[repr(C, align(16))]
pub struct KernelStack(pub [u8; KSTACK_SIZE]);

#[repr(C, align(16))]
pub struct FpuState(pub [u8; 512]);

impl FpuState {
    pub fn initial() -> Box<Self> {
        let mut s = Box::new(FpuState([0; 512]));
        s.0[0..2].copy_from_slice(&0x037f_u16.to_le_bytes()); // FCW
        s.0[24..28].copy_from_slice(&0x1f80_u32.to_le_bytes()); // MXCSR
        s
    }
}

/// Scheduling state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    /// Running on a CPU, or about to run (it is or will be in a run queue).
    Running = 0,
    /// Waiting in a run queue.
    Runnable = 1,
    /// Waiting for `wakeup` of its channel, its deadline or a signal.
    Sleeping = 2,
    /// Stopped by a signal until SIGCONT (or SIGKILL).
    Stopped = 3,
    /// Exited; waits to be reaped.
    Zombie = 4,
}

impl State {
    fn from_u8(v: u8) -> State {
        match v {
            0 => State::Running,
            1 => State::Runnable,
            2 => State::Sleeping,
            3 => State::Stopped,
            _ => State::Zombie,
        }
    }
}

/// Shared, locked process information.
pub struct Info {
    pub ppid: Pid,
    pub pgid: Pid,
    pub sid: Pid,
    pub name: String,
    /// Wait status once exited (exit code << 8, or the signal number).
    pub exit_status: Option<i32>,
    /// Stop/continue event not yet collected by the parent's wait4.
    pub report: Option<i32>,
    /// prctl state: signal sent when the parent dies (0: none), core
    /// dumps allowed, no new privileges (kept across fork and exec).
    pub pdeath_sig: u32,
    pub dumpable: bool,
    pub no_new_privs: bool,
}

impl Info {
    pub fn new(ppid: super::Pid, pgid: super::Pid, sid: super::Pid, name: String) -> Info {
        Info { ppid, pgid, sid, name, exit_status: None, report: None, pdeath_sig: 0, dumpable: true, no_new_privs: false }
    }
}

/// State owned by the task itself (see the module comment).
pub struct Process {
    pub space: Option<AddressSpace>,
    pub fs_base: u64,
    pub fpu: Box<FpuState>,
    pub fds: Vec<Option<FdEntry>>,
    pub cwd: String,
    pub brk_start: u64,
    pub brk_end: u64,
    pub mmap_next: u64,
    /// I/O permission bitmap (0 = allowed) installed in the TSS while
    /// this process runs.
    pub io_bitmap: Option<Box<[u8; gdt::IOMAP_BYTES]>>,
    /// The server this process runs, with the resources assigned to it.
    pub server: Option<Arc<Server>>,
}

pub struct Task {
    pub pid: Pid,
    /// A CPU's idle loop: never queued, never in the process table.
    pub idle: bool,
    state: AtomicU8,
    /// Its kernel stack is in use: it runs, or a CPU is switching away
    /// from it. Another CPU must not switch to it before this clears.
    pub on_cpu: AtomicBool,
    /// Runnable as far as the scheduler knows: running (and not yet
    /// descheduled) or in a run queue. Cleared when it goes to sleep.
    pub on_rq: AtomicBool,
    /// CPU it ran on last; wakeups prefer it.
    pub last_cpu: AtomicUsize,
    /// CPUs it may run on (bit per CPU index), see sched_setaffinity.
    pub affinity: AtomicU64,
    /// Channel it waits on (0: none) and its deadline in ticks (0: none).
    pub wait_chan: AtomicUsize,
    pub wake_at: AtomicU64,
    /// Serializes wakeups with the task descheduling itself.
    pub wake_lock: IrqSpinLock<()>,
    /// Servers started by the kernel: may register IPC services and ask
    /// for I/O ports; protected from user signals.
    pub privileged: AtomicBool,
    pub info: IrqSpinLock<Info>,
    pub sig: IrqSpinLock<Signals>,
    /// Saved kernel stack pointer while switched out.
    pub kernel_rsp: UnsafeCell<u64>,
    /// None for tasks running on a stack they did not allocate (the
    /// kernel monitor on the boot stack, an AP's idle loop).
    pub kstack: Option<Box<KernelStack>>,
    own: UnsafeCell<Process>,
}

// Shared across CPUs; the UnsafeCells follow the ownership rules above.
unsafe impl Sync for Task {}
unsafe impl Send for Task {}

impl Task {
    pub fn new(pid: Pid, info: Info, own: Process, kstack: Option<Box<KernelStack>>, kernel_rsp: u64) -> Task {
        Task {
            pid,
            idle: false,
            state: AtomicU8::new(State::Runnable as u8),
            on_cpu: AtomicBool::new(false),
            on_rq: AtomicBool::new(true),
            last_cpu: AtomicUsize::new(0),
            affinity: AtomicU64::new(u64::MAX),
            wait_chan: AtomicUsize::new(0),
            wake_at: AtomicU64::new(0),
            wake_lock: IrqSpinLock::new(()),
            privileged: AtomicBool::new(false),
            info: IrqSpinLock::new(info),
            sig: IrqSpinLock::new(Signals::default()),
            kernel_rsp: UnsafeCell::new(kernel_rsp),
            kstack,
            own: UnsafeCell::new(own),
        }
    }

    pub fn idle_task(cpu: usize, kstack: Option<Box<KernelStack>>, kernel_rsp: u64) -> Task {
        let info = Info::new(0, 0, 0, alloc::format!("idle/{cpu}"));
        let mut t = Task::new(u64::MAX - cpu as u64, info, Process::empty(), kstack, kernel_rsp);
        t.idle = true;
        t.state = AtomicU8::new(State::Running as u8);
        t
    }

    pub fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Changes the state; callers hold `wake_lock` where a wakeup could race.
    pub fn set_state(&self, s: State) {
        self.state.store(s as u8, Ordering::Release);
    }

    pub fn may_run_on(&self, cpu: usize) -> bool {
        self.affinity.load(Ordering::Relaxed) & (1 << cpu) != 0
    }

    pub fn kstack_top(&self) -> Option<u64> {
        self.kstack.as_ref().map(|s| s.0.as_ptr() as u64 + KSTACK_SIZE as u64)
    }

    /// The task's own state.
    ///
    /// SAFETY: only the task itself (running), the CPU switching to or from
    /// it, or the reaper of a dead, off-CPU task may call this, and never
    /// twice at the same time.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn own(&self) -> &mut Process {
        unsafe { &mut *self.own.get() }
    }
}

impl Process {
    pub fn empty() -> Process {
        Process {
            space: None,
            fs_base: 0,
            fpu: FpuState::initial(),
            fds: Vec::new(),
            cwd: alloc::string::ToString::to_string("/"),
            brk_start: 0,
            brk_end: 0,
            mmap_next: 0,
            io_bitmap: None,
            server: None,
        }
    }
}
