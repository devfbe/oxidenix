//! Tasks and thread groups.
//!
//! A task is one thread as the scheduler sees it (or a CPU's idle loop); a
//! thread group is a process: the tasks that share a process id, signal
//! handlers and pending process signals, relations, and the exit status.
//! Like on Linux, what threads share beyond that is chosen by clone flags:
//! address space (`Mm`), descriptor table (`Files`) and working directory
//! (`FsInfo`) are separate shared objects that a task holds references to.
//!
//! A task's state splits by who may touch it:
//! - `Process` (references to those objects, I/O permissions, the futex
//!   word to clear at exit) belongs to the task itself: only the CPU
//!   running it, or the CPU switching to or from it, may use it.
//! - `sig` (its signal mask and pending signals) is locked, after the
//!   group's signal lock.
//! - The scheduling fields are atomics, written under `wake_lock`.

use super::address_space::Mm;
use super::signal::{GroupSignals, ThreadSignals};
use super::{FdEntry, Pid, Server};
use crate::fs::file::OpenFile;
use crate::interrupts::gdt;
use crate::sync::IrqSpinLock;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicU64, AtomicU8, AtomicUsize, Ordering};

pub use crate::memory::kstack::KernelStack;

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
    /// Exited; never runs again.
    Dead = 4,
}

impl State {
    fn from_u8(v: u8) -> State {
        match v {
            0 => State::Running,
            1 => State::Runnable,
            2 => State::Sleeping,
            3 => State::Stopped,
            _ => State::Dead,
        }
    }
}

/// Process information, shared by the threads and locked.
pub struct Info {
    pub ppid: Pid,
    pub pgid: Pid,
    pub sid: Pid,
    /// Process name (the main thread's name).
    pub name: String,
    /// Wait status once the last thread exited (exit code << 8, or the
    /// signal number): the process is a zombie until its parent reaps it.
    pub exit_status: Option<i32>,
    /// Stop/continue event not yet collected by the parent's wait4.
    pub report: Option<i32>,
    /// Exit status of the main thread if it exited on its own (the
    /// process's status unless a group exit sets one).
    pub main_status: Option<i32>,
    /// Signal the parent gets when the process ends (SIGCHLD; 0: none).
    pub exit_signal: u32,
    /// prctl state: signal sent when the parent dies (0: none), core
    /// dumps allowed, no new privileges (kept across fork and exec).
    pub pdeath_sig: u32,
    pub dumpable: bool,
    pub no_new_privs: bool,
    /// Arguments of the running program, NUL-terminated (capped at 4 KiB),
    /// and its absolute path.
    pub cmdline: Vec<u8>,
    pub exe: String,
    /// Page counts of its address space (None for kernel tasks and zombies).
    pub mem: Option<Arc<super::address_space::MemStats>>,
    /// Live threads, in creation order.
    pub threads: Vec<Arc<Task>>,
    /// CPU time in nanoseconds (user, system) of threads that exited, and
    /// of reaped children (with their own reaped children).
    pub dead_time: (u64, u64),
    pub children_time: (u64, u64),
    /// The most pages it ever had resident, kept when its address space
    /// goes at exit (wait4's ru_maxrss), and the most of its reaped
    /// children's (with theirs).
    pub peak_pages: u64,
    pub children_peak: u64,
}

impl Info {
    /// (user, system) CPU time in nanoseconds of the whole process: live
    /// and exited threads, not children.
    pub fn cputime(&self) -> (u64, u64) {
        self.threads.iter().map(|t| t.cputime()).fold(self.dead_time, |a, b| (a.0 + b.0, a.1 + b.1))
    }

    pub fn new(ppid: super::Pid, pgid: super::Pid, sid: super::Pid, name: String) -> Info {
        Info {
            ppid,
            pgid,
            sid,
            name,
            exit_status: None,
            report: None,
            main_status: None,
            exit_signal: super::signal::SIGCHLD,
            pdeath_sig: 0,
            dumpable: true,
            no_new_privs: false,
            cmdline: Vec::new(),
            exe: String::new(),
            mem: None,
            threads: Vec::new(),
            dead_time: (0, 0),
            children_time: (0, 0),
            peak_pages: 0,
            children_peak: 0,
        }
    }
}

/// A process: the threads that share a pid.
pub struct ThreadGroup {
    pub tgid: Pid,
    /// Servers started by the kernel: may register IPC services and ask
    /// for I/O ports; protected from user signals.
    pub privileged: AtomicBool,
    pub start_ticks: u64,
    pub info: IrqSpinLock<Info>,
    /// Handlers, pending process signals, the interval timer, and group
    /// stops and exits. Taken before any thread's `sig`.
    pub sig: IrqSpinLock<GroupSignals>,
    /// Sequence number of the live arming of the interval timer (0: off),
    /// changed under `sig`.
    pub alarm_seq: AtomicU64,
    /// CPUs whose timer queues it armed its interval timer on.
    pub alarm_cpus: AtomicU64,
    /// The id of the Linux server instance whose tree the process belongs
    /// to (`linux::Instance::id`; 0: none, a server of the kernel's or the
    /// instance's pager). Set when its first thread is made.
    pub instance: AtomicU64,
    /// Linux system calls of this process the Linux server passed back to
    /// the kernel (`linux::legacy`): the process's share of the counter in
    /// `/proc/counters`, which `/proc/<pid>/counters` shows, so that a
    /// program can count its own calls whatever else runs.
    pub legacy_calls: AtomicU64,
}

impl ThreadGroup {
    pub fn new(tgid: Pid, info: Info, sig: GroupSignals) -> Option<Arc<ThreadGroup>> {
        Arc::try_new(ThreadGroup {
            tgid,
            privileged: AtomicBool::new(false),
            start_ticks: super::sched::ticks(),
            info: IrqSpinLock::new(info),
            sig: IrqSpinLock::new(sig),
            alarm_seq: AtomicU64::new(0),
            alarm_cpus: AtomicU64::new(0),
            instance: AtomicU64::new(0),
            legacy_calls: AtomicU64::new(0),
        })
        .ok()
    }
}

/// What a context switch saves and restores. Kept apart from `Process`, so
/// switching away from a task never touches the state the task itself may
/// hold borrowed while it sleeps (its address space during a page fault
/// that reads a file, for example).
pub struct CpuState {
    pub fs_base: u64,
    pub fpu: Box<FpuState>,
}

/// A descriptor table, shared by the tasks cloned with CLONE_FILES.
/// Descriptors are taken out under the lock and dropped after it: closing
/// a file may wake others or talk to a server. A Linux program's table
/// holds no descriptors (they are its server's): only the server's record
/// of its own table (`restricted::SYS_FILES_RECORD`), which goes back to the
/// server with this table.
pub struct Files {
    fds: IrqSpinLock<Vec<Option<FdEntry>>>,
    record: IrqSpinLock<Option<super::linux::Record>>,
}

/// Most descriptors a process may have open: RLIMIT_NOFILE, which
/// `prlimit` reports (Linux's default hard limit). The table grows as
/// descriptors are used.
pub const MAX_FDS: usize = 4096;

impl Files {
    pub fn new(fds: Vec<Option<FdEntry>>) -> Option<Arc<Files>> {
        Arc::try_new(Files { fds: IrqSpinLock::new(fds), record: IrqSpinLock::new(None) }).ok()
    }

    pub fn record_word(&self) -> u64 {
        self.record.lock().as_ref().map_or(0, |r| r.word())
    }

    /// Gives a new table the server's record (at its creation, or once for
    /// a table the kernel made without one); never replaces one (see
    /// `FsInfo::set_record`). Returns the record back if the table has one.
    pub fn set_record(&self, record: super::linux::Record) -> Result<(), super::linux::Record> {
        let mut slot = self.record.lock();
        if slot.is_some() {
            return Err(record);
        }
        *slot = Some(record);
        Ok(())
    }

    /// Takes the record out (the table's last holder hands it back
    /// directly: execve, `linux::Record::into_word`).
    pub fn take_record(&self) -> Option<super::linux::Record> {
        self.record.lock().take()
    }

    /// A copy for a new process (fork), or for exec of a shared table
    /// (without a record: the caller gives it the server's, if any).
    pub fn duplicate(&self) -> Option<Arc<Files>> {
        // Memory is reserved outside the lock, for the table's length as it
        // was; if it grew meanwhile, again.
        let mut copy = Vec::new();
        loop {
            let len = self.fds.lock().len();
            copy.try_reserve_exact(len).ok()?;
            let fds = self.fds.lock();
            if fds.len() <= copy.capacity() {
                copy.extend(fds.iter().cloned());
                break;
            }
        }
        Files::new(copy)
    }

    pub fn get(&self, fd: u64) -> Result<Arc<OpenFile>, i64> {
        self.fds.lock().get(fd as usize).and_then(|e| e.as_ref()).map(|e| e.file().clone()).ok_or(super::errno::EBADF)
    }

    /// Installs `file` at the lowest free descriptor >= `min`.
    pub fn alloc(&self, file: Arc<OpenFile>, cloexec: bool, min: usize) -> Result<i64, i64> {
        let mut fds = self.fds.lock();
        let fd = (min..MAX_FDS).find(|&i| fds.get(i).is_none_or(|e| e.is_none())).ok_or(super::errno::EMFILE)?;
        if fds.len() <= fd {
            let more = fd + 1 - fds.len();
            fds.try_reserve(more).map_err(|_| super::errno::ENOMEM)?;
            fds.resize(fd + 1, None);
        }
        fds[fd] = Some(FdEntry::new(file, cloexec));
        Ok(fd as i64)
    }

    /// Puts `entry` at `fd`; returns what was there (for the caller to drop
    /// after the lock).
    pub fn replace(&self, fd: u64, entry: FdEntry) -> Result<Option<FdEntry>, i64> {
        if fd as usize >= MAX_FDS {
            return Err(super::errno::EBADF);
        }
        let mut fds = self.fds.lock();
        if fds.len() <= fd as usize {
            let more = fd as usize + 1 - fds.len();
            fds.try_reserve(more).map_err(|_| super::errno::ENOMEM)?;
            fds.resize(fd as usize + 1, None);
        }
        Ok(fds[fd as usize].replace(entry))
    }

    pub fn take(&self, fd: u64) -> Option<FdEntry> {
        self.fds.lock().get_mut(fd as usize).and_then(|e| e.take())
    }

    pub fn cloexec(&self, fd: u64) -> Result<bool, i64> {
        self.fds.lock().get(fd as usize).and_then(|e| e.as_ref()).map(|e| e.cloexec).ok_or(super::errno::EBADF)
    }

    pub fn set_cloexec(&self, fd: u64, on: bool) -> Result<(), i64> {
        let mut fds = self.fds.lock();
        fds.get_mut(fd as usize).and_then(|e| e.as_mut()).ok_or(super::errno::EBADF)?.cloexec = on;
        Ok(())
    }

    /// Takes out the descriptors marked close-on-exec (to drop after the lock).
    pub fn take_cloexec(&self) -> Vec<FdEntry> {
        // Room for every descriptor of the table, reserved outside the lock
        // (a descriptor that does not fit, should the table have grown
        // meanwhile and memory run out, stays open).
        let mut out = Vec::new();
        let len = self.fds.lock().len();
        let _ = out.try_reserve_exact(len);
        let mut fds = self.fds.lock();
        if fds.len() > out.capacity() {
            drop(fds);
            let _ = out.try_reserve_exact(MAX_FDS);
            fds = self.fds.lock();
        }
        for e in fds.iter_mut() {
            if e.as_ref().is_some_and(|e| e.cloexec) && out.len() < out.capacity() {
                out.push(e.take().expect("checked"));
            }
        }
        out
    }
}

/// Working directory, shared by the tasks cloned with CLONE_FS.
pub struct FsInfo {
    cwd: IrqSpinLock<String>,
    /// The Linux server's record for this context (`restricted::SYS_FS_RECORD`).
    record: IrqSpinLock<Option<super::linux::Record>>,
}

impl FsInfo {
    pub fn new(cwd: String) -> Option<Arc<FsInfo>> {
        Arc::try_new(FsInfo { cwd: IrqSpinLock::new(cwd), record: IrqSpinLock::new(None) }).ok()
    }

    pub fn record_word(&self) -> u64 {
        self.record.lock().as_ref().map_or(0, |r| r.word())
    }

    /// Gives a new context the server's record (at its creation, or once
    /// for a context the kernel made without one). Never replaces one: the
    /// server's threads use their context's record without holding a
    /// reference of their own (see `restricted::SYS_FS_RECORD`). Returns the
    /// record back if the context has one (to be dropped without the lock).
    pub fn set_record(&self, record: super::linux::Record) -> Result<(), super::linux::Record> {
        let mut slot = self.record.lock();
        if slot.is_some() {
            return Err(record);
        }
        *slot = Some(record);
        Ok(())
    }

    pub fn cwd(&self) -> String {
        self.cwd.lock().clone()
    }

    pub fn set_cwd(&self, cwd: String) {
        let old = core::mem::replace(&mut *self.cwd.lock(), cwd);
        drop(old);
    }
}

/// State owned by the task itself (see the module comment).
pub struct Process {
    /// The address space (None for kernel tasks and after exit).
    pub mm: Option<Arc<Mm>>,
    /// Descriptors and working directory (None only after exit).
    pub files: Option<Arc<Files>>,
    pub fs: Option<Arc<FsInfo>>,
    /// I/O permission bitmap (0 = allowed) installed in the TSS while
    /// this task runs (per thread, as ioperm is on Linux).
    pub io_bitmap: Option<Box<[u8; gdt::IOMAP_BYTES]>>,
    /// The server this task runs, with the resources assigned to it.
    pub server: Option<Arc<Server>>,
    /// CLONE_CHILD_CLEARTID / set_tid_address: cleared and woken (futex)
    /// when the task exits, which is how a thread is joined.
    pub clear_child_tid: u64,
    /// A vfork parent sleeps until this is set: the child exec'd or exited.
    pub vfork_done: Option<Arc<AtomicBool>>,
    /// A thread of a Linux program: its place in restricted mode.
    pub linux: Option<super::linux::LinuxThread>,
    /// A service's copy routine on granted memory (`channel::set_copy_fixup`):
    /// a fault of the instruction at `.0` that cannot be resolved resumes at
    /// `.1`. Reset by exec.
    pub copy_fixup: Option<(u64, u64)>,
}

pub struct Task {
    /// Thread id; the main thread's equals the process id. Changes only
    /// when another thread execs and takes over the main thread's id.
    tid: AtomicU64,
    pub group: Arc<ThreadGroup>,
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
    /// Its nice value (-20..=19, see `sched::weight`).
    pub nice: AtomicI8,
    /// Fair scheduling (`sched`): its virtual runtime, on the scale of the
    /// CPU `vcpu` (usize::MAX: a new task, not placed yet).
    pub vruntime: AtomicU64,
    pub vcpu: AtomicUsize,
    /// The address of the Linux server's count of the locks it holds on
    /// this thread (`restricted::SERVER_LOCKS_OFFSET` in its State page),
    /// 0 for a task that is no Linux thread: the scheduler boosts a holder.
    /// Valid for the task's whole life: the word's slot is the task's own
    /// `LinuxThread`'s (`own.linux`, set before the task first runs and
    /// never taken or replaced), which gives the slot back only when it is
    /// dropped, with this task; so no reader of this field can see another
    /// thread's count in a reused slot.
    pub server_locks: AtomicU64,
    /// Channel it waits on (0: none), and the sequence number of the
    /// timer that ends its sleep (0: none; see `timer`).
    pub wait_chan: AtomicUsize,
    pub timer_seq: AtomicU64,
    /// CPUs whose timer queues it armed a timer on (bit per index).
    pub timer_cpus: AtomicU64,
    /// Serializes wakeups with the task descheduling itself.
    pub wake_lock: IrqSpinLock<()>,
    /// futex wait: set by the waker that dequeued it, and the hash bucket
    /// it waits in (a requeue may move it).
    pub futex_woken: AtomicBool,
    pub futex_bucket: AtomicUsize,
    /// Timer ticks that found it in user mode and in the kernel: they
    /// split its exactly measured run time into user and system time.
    pub utime: AtomicU64,
    pub stime: AtomicU64,
    /// Run time: nanoseconds of finished time slices, and the start of the
    /// current one (0 while not running), written by the CPU switching it
    /// under a sequence counter that readers on other CPUs retry on.
    run_seq: AtomicU64,
    run_ns: AtomicU64,
    run_since: AtomicU64,
    /// Thread name (comm), at most 15 bytes.
    pub comm: IrqSpinLock<String>,
    /// Its signal mask and the signals sent to this thread.
    pub sig: IrqSpinLock<ThreadSignals>,
    /// Saved kernel stack pointer while switched out.
    pub kernel_rsp: UnsafeCell<u64>,
    /// None for tasks running on a stack they did not allocate (the
    /// kernel monitor on the boot stack, an AP's idle loop).
    pub kstack: Option<KernelStack>,
    own: UnsafeCell<Process>,
    cpu: UnsafeCell<CpuState>,
}

// Shared across CPUs; the UnsafeCells follow the ownership rules above.
unsafe impl Sync for Task {}
unsafe impl Send for Task {}

impl Task {
    pub fn new(tid: Pid, group: Arc<ThreadGroup>, comm: String, own: Process, kstack: Option<KernelStack>, kernel_rsp: u64) -> Task {
        Task {
            tid: AtomicU64::new(tid),
            group,
            idle: false,
            state: AtomicU8::new(State::Runnable as u8),
            on_cpu: AtomicBool::new(false),
            on_rq: AtomicBool::new(true),
            last_cpu: AtomicUsize::new(0),
            affinity: AtomicU64::new(u64::MAX),
            nice: AtomicI8::new(0),
            vruntime: AtomicU64::new(0),
            vcpu: AtomicUsize::new(usize::MAX),
            server_locks: AtomicU64::new(own.linux.as_ref().map_or(0, |l| l.locks_word())),
            wait_chan: AtomicUsize::new(0),
            timer_seq: AtomicU64::new(0),
            timer_cpus: AtomicU64::new(0),
            wake_lock: IrqSpinLock::new(()),
            futex_woken: AtomicBool::new(false),
            futex_bucket: AtomicUsize::new(0),
            utime: AtomicU64::new(0),
            stime: AtomicU64::new(0),
            run_seq: AtomicU64::new(0),
            run_ns: AtomicU64::new(0),
            run_since: AtomicU64::new(0),
            comm: IrqSpinLock::new(comm),
            sig: IrqSpinLock::new(ThreadSignals::default()),
            kernel_rsp: UnsafeCell::new(kernel_rsp),
            kstack,
            own: UnsafeCell::new(own),
            cpu: UnsafeCell::new(CpuState { fs_base: 0, fpu: FpuState::initial() }),
        }
    }

    pub fn idle_task(cpu: usize, kstack: Option<KernelStack>, kernel_rsp: u64) -> Task {
        let id = u64::MAX - cpu as u64;
        let name = alloc::format!("idle/{cpu}");
        let group = ThreadGroup::new(id, Info::new(0, 0, 0, name.clone()), GroupSignals::default()).expect("idle task at boot");
        let mut t = Task::new(id, group, name, Process::empty(), kstack, kernel_rsp);
        t.idle = true;
        t.state = AtomicU8::new(State::Running as u8);
        t
    }

    /// A kernel thread (see `sched::spawn_kernel_thread`).
    pub fn kernel_thread(id: u64, name: &str, kstack: KernelStack, kernel_rsp: u64) -> Option<Arc<Task>> {
        let group = ThreadGroup::new(id, Info::new(0, 0, 0, name.into()), GroupSignals::default())?;
        Arc::try_new(Task::new(id, group, name.into(), Process::empty(), Some(kstack), kernel_rsp)).ok()
    }

    pub fn tid(&self) -> Pid {
        self.tid.load(Ordering::Relaxed)
    }

    /// Takes over the main thread's id (exec in another thread).
    pub fn set_tid(&self, tid: Pid) {
        self.tid.store(tid, Ordering::Relaxed);
    }

    pub fn tgid(&self) -> Pid {
        self.group.tgid
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
        self.kstack.as_ref().map(|s| s.top())
    }

    /// Updates the run time under the sequence counter (only the CPU that
    /// switches the task calls these, so there is one writer at a time).
    fn update_run(&self, f: impl FnOnce()) {
        self.run_seq.fetch_add(1, Ordering::Relaxed);
        core::sync::atomic::fence(Ordering::Release);
        f();
        self.run_seq.fetch_add(1, Ordering::Release);
    }

    /// A time slice starts at `now` (nanoseconds since boot).
    pub fn start_running(&self, now: u64) {
        self.update_run(|| self.run_since.store(now.max(1), Ordering::Relaxed));
    }

    /// The current time slice ends at `now`.
    pub fn stop_running(&self, now: u64) {
        self.update_run(|| {
            let since = self.run_since.swap(0, Ordering::Relaxed);
            if since != 0 {
                self.run_ns.fetch_add(now.saturating_sub(since), Ordering::Relaxed);
            }
        });
    }

    /// Nanoseconds it ran, the current time slice included.
    pub fn runtime(&self) -> u64 {
        loop {
            let seq = self.run_seq.load(Ordering::Acquire);
            let (run, since) = (self.run_ns.load(Ordering::Relaxed), self.run_since.load(Ordering::Relaxed));
            core::sync::atomic::fence(Ordering::Acquire);
            if seq & 1 == 0 && self.run_seq.load(Ordering::Relaxed) == seq {
                return run + if since != 0 { crate::time::now().saturating_sub(since) } else { 0 };
            }
            core::hint::spin_loop();
        }
    }

    /// (user, system) CPU time in nanoseconds.
    pub fn cputime(&self) -> (u64, u64) {
        crate::time::split(self.runtime(), self.utime.load(Ordering::Relaxed), self.stime.load(Ordering::Relaxed))
    }

    /// The task's own state.
    ///
    /// SAFETY: only the task itself (running), or the CPU switching to or
    /// from it, may call this, and never twice at the same time.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn own(&self) -> &mut Process {
        unsafe { &mut *self.own.get() }
    }

    /// The saved CPU state.
    ///
    /// SAFETY: only the CPU switching to or from the task, or its creator
    /// before it first runs.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn cpu_state(&self) -> &mut CpuState {
        unsafe { &mut *self.cpu.get() }
    }
}

impl Process {
    pub fn empty() -> Process {
        Process {
            mm: None,
            files: None,
            fs: None,
            io_bitmap: None,
            server: None,
            clear_child_tid: 0,
            vfork_done: None,
            linux: None,
            copy_fixup: None,
        }
    }
}
