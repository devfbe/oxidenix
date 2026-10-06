pub mod address_space;
pub mod elf;
pub mod errno;
mod futex;
pub mod ipc;
pub mod irq;
mod loader;
mod prctl;
pub mod query;
pub mod sched;
pub mod signal;
mod sys_file;
mod sys_mem;
mod sys_net;
pub mod syscall;
pub mod task;
pub mod tlb;
pub mod uaccess;

use crate::fs::file::OpenFile;
use crate::fs;
use crate::memory;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::Ordering;
use errno::*;
use sched::{current, prepare_to_wait, TABLE};
use syscall::Frame;
use task::{Info, KernelStack, State, Task};
use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

pub use sched::{prepare_to_sleep, schedule, ticks, wakeup};
pub use signal::{get_alarm, set_alarm};
pub use task::Process;

pub type Pid = u64;

pub const TIMER_HZ: u64 = 100;

#[derive(Clone)]
pub struct FdEntry {
    pub file: Arc<OpenFile>,
    pub cloexec: bool,
}

impl Process {
    /// The address space, or EINVAL for a task without one.
    pub fn mm(&self) -> Result<Arc<address_space::Mm>, i64> {
        self.mm.clone().ok_or(EINVAL)
    }

    pub fn file(&self, fd: u64) -> Result<Arc<OpenFile>, i64> {
        self.fds
            .get(fd as usize)
            .and_then(|e| e.as_ref())
            .map(|e| e.file.clone())
            .ok_or(EBADF)
    }

    /// Lowest free descriptor >= `min`.
    pub fn alloc_fd(&mut self, file: Arc<OpenFile>, cloexec: bool, min: usize) -> Result<i64, i64> {
        const MAX_FDS: usize = 256;
        let fd = (min..MAX_FDS)
            .find(|&i| self.fds.get(i).is_none_or(|e| e.is_none()))
            .ok_or(EMFILE)?;
        if self.fds.len() <= fd {
            self.fds.resize(fd + 1, None);
        }
        self.fds[fd] = Some(FdEntry { file, cloexec });
        Ok(fd as i64)
    }
}

/// Runs `f` on the running process's own state. `f` must not sleep or
/// switch tasks (closing files is fine: it never sleeps).
pub fn with_current<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    interrupts::without_interrupts(|| f(unsafe { current().own() }))
}

/// The running task's address space.
pub fn current_mm() -> Option<Arc<address_space::Mm>> {
    with_current(|p| p.mm.clone())
}

/// Process 0, the kernel monitor, runs on the boot stack of the bootstrap
/// CPU; each CPU also has an idle task.
pub fn init() {
    enable_sse();
    syscall::init();
    let info = Info::new(0, 0, 0, "kernel".to_string());
    let kernel = Arc::new(Task::new(0, info, Process::empty(), None, 0));
    TABLE.lock().tasks.insert(0, kernel.clone());
    sched::set_initial(kernel, sched::new_idle_task(0));
}

/// musl uses SSE; without OSFXSR every SSE instruction raises #UD.
pub fn enable_sse() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
    unsafe {
        Cr0::update(|f| {
            f.remove(Cr0Flags::EMULATE_COPROCESSOR);
            f.insert(Cr0Flags::MONITOR_COPROCESSOR);
        });
        Cr4::update(|f| f.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE));
    }
}

pub fn current_pid() -> Pid {
    current().pid
}

pub fn current_ppid() -> Pid {
    current().info.lock().ppid
}

/// The task with `pid` (0: the caller).
pub fn task(pid: Pid) -> Option<Arc<Task>> {
    if pid == 0 {
        return Some(sched::current_arc());
    }
    TABLE.lock().tasks.get(&pid).cloned()
}

pub fn setpgid(pid: Pid, pgid: Pid) -> SysResult {
    let me = current_pid();
    let t = task(pid).ok_or(ESRCH)?;
    let mut info = t.info.lock();
    if t.pid != me && info.ppid != me {
        return Err(ESRCH);
    }
    info.pgid = if pgid == 0 { t.pid } else { pgid };
    Ok(0)
}

pub fn getpgid(pid: Pid) -> SysResult {
    Ok(task(pid).ok_or(ESRCH)?.info.lock().pgid as i64)
}

pub fn getsid(pid: Pid) -> SysResult {
    Ok(task(pid).ok_or(ESRCH)?.info.lock().sid as i64)
}

pub fn setsid() -> SysResult {
    let me = current();
    let mut info = me.info.lock();
    info.sid = me.pid;
    info.pgid = me.pid;
    Ok(me.pid as i64)
}

/// ioperm(from, count, on) for privileged servers: grants or revokes
/// access to I/O ports via the TSS bitmap. Only the ports the kernel
/// assigned to the server can be granted.
pub fn ioperm(from: u64, count: u64, on: u64) -> SysResult {
    let end = from.checked_add(count).filter(|&e| e <= 0x10000).ok_or(EINVAL)?;
    if !current().privileged.load(Ordering::Relaxed) {
        return Err(EPERM);
    }
    with_current(|p| {
        let assigned = p.server.as_ref().map_or(&[][..], |s| &s.ports[..]);
        if on != 0 && !assigned.iter().any(|r| r.start <= from && end <= r.end) {
            return Err(EPERM);
        }
        let bitmap = p.io_bitmap.get_or_insert_with(|| Box::new([0xff; crate::interrupts::gdt::IOMAP_BYTES]));
        for port in from..end {
            let (byte, bit) = ((port / 8) as usize, 1u8 << (port % 8));
            if on != 0 {
                bitmap[byte] &= !bit;
            } else {
                bitmap[byte] |= bit;
            }
        }
        crate::smp::cpu().tables().set_io_bitmap(Some(bitmap));
        Ok(0)
    })
}

/// (pid, parent, name, state, server, cpu) of every process, for the monitor.
pub fn list() -> Vec<(Pid, Pid, String, &'static str, bool, usize)> {
    let tasks: Vec<Arc<Task>> = TABLE.lock().tasks.values().cloned().collect();
    tasks
        .iter()
        .map(|t| {
            let state = match t.state() {
                State::Running => "running",
                State::Runnable => "ready",
                State::Sleeping => "sleeping",
                State::Stopped => "stopped",
                State::Zombie => "zombie",
            };
            let info = t.info.lock();
            (t.pid, info.ppid, info.name.clone(), state, t.privileged.load(Ordering::Relaxed), t.last_cpu.load(Ordering::Relaxed))
        })
        .collect()
}

/// Bit mask of the CPUs that run.
pub fn online_mask() -> u64 {
    (0..crate::smp::MAX_CPUS).filter(|&i| crate::smp::by_index(i).is_some()).fold(0, |m, i| m | 1 << i)
}

/// sched_getaffinity(pid, size, mask): the CPUs the task may run on, as a
/// 64-bit mask; returns the bytes written, as Linux does.
pub fn sched_getaffinity(pid: Pid, size: u64, mask: u64) -> SysResult {
    if size < 8 {
        return Err(EINVAL);
    }
    let t = task(pid).ok_or(ESRCH)?;
    uaccess::write(mask, t.affinity.load(Ordering::Relaxed) & online_mask())?;
    Ok(8)
}

/// sched_setaffinity(pid, size, mask): restricts the task to the CPUs in
/// `mask` that run. If the caller excludes its own CPU it moves at once.
pub fn sched_setaffinity(pid: Pid, size: u64, mask: u64) -> SysResult {
    if size == 0 {
        return Err(EINVAL);
    }
    let mut bytes = [0u8; 8];
    let n = size.min(8);
    uaccess::copy_from(mask, &mut bytes[..n as usize])?;
    let wanted = u64::from_le_bytes(bytes) & online_mask();
    if wanted == 0 {
        return Err(EINVAL);
    }
    let t = task(pid).ok_or(ESRCH)?;
    if t.privileged.load(Ordering::Relaxed) && current_pid() != 0 {
        return Err(EPERM);
    }
    t.affinity.store(wanted, Ordering::Relaxed);
    if core::ptr::eq(&*t, current()) && !t.may_run_on(crate::smp::cpu().index) {
        schedule();
    }
    Ok(0)
}

/// getcpu(&cpu, &node, cache): the CPU the caller runs on; one NUMA node.
pub fn getcpu(cpu: u64, node: u64) -> SysResult {
    let index = crate::smp::cpu().index as u32;
    if cpu != 0 {
        uaccess::write(cpu, index)?;
    }
    if node != 0 {
        uaccess::write(node, 0u32)?;
    }
    Ok(0)
}

/// Called from every CPU's timer interrupt.
pub fn tick(user: bool) {
    sched::tick(user);
}

/// Sleeps for `n` timer ticks; a signal ends the sleep early with EINTR.
pub fn sleep_ticks(n: u64) -> Result<(), i64> {
    let deadline = ticks().saturating_add(n);
    loop {
        let wait = prepare_to_sleep();
        if ticks() >= deadline {
            return Ok(());
        }
        if signal::interrupted() {
            return Err(EINTR);
        }
        wait.sleep_until(deadline);
    }
}

/// pause(2): sleeps until a signal arrives.
pub fn pause() -> SysResult {
    loop {
        let wait = prepare_to_sleep();
        if signal::interrupted() {
            return Err(EINTR);
        }
        wait.sleep();
    }
}

pub fn yield_now() {
    schedule();
}

/// A new user task with a kernel stack whose first switch enters ring 3
/// with `frame`.
fn new_task(pid: Pid, info: Info, own: Process, frame: Frame) -> Result<Arc<Task>, i64> {
    // Kernel stacks are demanded by user space (fork), so they must not
    // eat into the reserve the kernel heap relies on.
    if !memory::with_frames(|f| f.user_may_take((task::KSTACK_SIZE / 4096) as u64)) {
        return Err(ENOMEM);
    }
    // try_new_zeroed: no temporary on the (small) kernel stack, and running
    // out of memory is an error, not a panic.
    let mut kstack = unsafe { Box::<KernelStack>::try_new_zeroed().map_err(|_| ENOMEM)?.assume_init() };
    let rsp = sched::prepare_stack(&mut kstack, Some(frame), true);
    Arc::try_new(Task::new(pid, info, own, Some(kstack), rsp)).map_err(|_| ENOMEM)
}

/// argv as /proc/<pid>/cmdline shows it: NUL-terminated, at most 4 KiB.
fn cmdline_of(args: &[String]) -> Vec<u8> {
    const MAX: usize = 4096;
    let mut out = Vec::new();
    for a in args {
        if out.len() + a.len() + 1 > MAX {
            break;
        }
        out.extend_from_slice(a.as_bytes());
        out.push(0);
    }
    out
}

/// Absolute form of `path` relative to `cwd` (for /proc/<pid>/exe).
fn absolute(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else if cwd.ends_with('/') {
        alloc::format!("{cwd}{path}")
    } else {
        alloc::format!("{cwd}/{path}")
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn load_path(cwd: &str, path: &str, args: &[String], envs: &[String]) -> Result<loader::Image, i64> {
    let inode = fs::resolve(cwd, path, true)?;
    if inode.file_type() != fs::S_IFREG {
        return Err(if inode.is_dir() { EISDIR } else { ENOEXEC });
    }
    inode.with_contents(|bytes| loader::load(bytes, args, envs))?
}

/// A user-space server the kernel starts and restarts.
pub struct Server {
    /// Name of the IPC service it registers.
    pub name: &'static str,
    pub path: &'static str,
    /// The program, read once at boot: a restart must not run whatever
    /// was written to `path` since then with the server's privileges.
    image: Vec<u8>,
    /// I/O port ranges (start..end) the server may request with ioperm.
    ports: Vec<Range<u64>>,
    /// Interrupt line it may enable.
    irq: Option<u8>,
    /// Size of its DMA area in pages, and the area once allocated. It
    /// outlives the process, so a restarted server gets the same memory.
    dma_pages: u64,
    dma: spin::Mutex<Option<u64>>,
    /// Extra command line arguments (e.g. the device's resources).
    args: Vec<String>,
    /// How long a (re)started server may take to register its service.
    start_ticks: u64,
    restarts: core::sync::atomic::AtomicU32,
    restarting: core::sync::atomic::AtomicBool,
}

/// How often a dead server is restarted before the kernel gives up on it.
const MAX_RESTARTS: u32 = 5;

impl Server {
    pub fn load(name: &'static str, path: &'static str) -> Result<Server, i64> {
        let inode = fs::resolve("/", path, true)?;
        if inode.file_type() != fs::S_IFREG {
            return Err(ENOEXEC);
        }
        let image = inode.with_contents(|bytes| {
            let mut image = Vec::new();
            image.try_reserve_exact(bytes.len()).map_err(|_| ENOMEM)?;
            image.extend_from_slice(bytes);
            Ok::<_, i64>(image)
        })??;
        Ok(Server {
            name,
            path,
            image,
            ports: Vec::new(),
            irq: None,
            dma_pages: 0,
            dma: spin::Mutex::new(None),
            args: Vec::new(),
            start_ticks: 3 * TIMER_HZ,
            restarts: core::sync::atomic::AtomicU32::new(0),
            restarting: core::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Starts the server and waits for it to register: (service, argument).
    pub fn start(self: &Arc<Self>) -> Result<(usize, u64), i64> {
        spawn_server(self)?;
        ipc::wait_for(self.name, self.start_ticks).ok_or(EIO)
    }

    /// The server's service, restarting the server if it died. The first
    /// caller restarts it, concurrent callers wait for the registration.
    /// After MAX_RESTARTS restarts the server stays dead (EIO).
    pub fn revive(self: &Arc<Self>) -> Result<(usize, u64), i64> {
        if let Some(found) = ipc::lookup(self.name) {
            return Ok(found);
        }
        if self.restarting.swap(true, Ordering::Relaxed) {
            return ipc::wait_for(self.name, self.start_ticks).ok_or(EIO);
        }
        let attempt = self.restarts.fetch_add(1, Ordering::Relaxed) + 1;
        let result = if attempt > MAX_RESTARTS {
            Err(EIO)
        } else {
            crate::printkln!("[kernel] {} died; restarting it (attempt {} of {})", self.name, attempt, MAX_RESTARTS);
            self.start()
        };
        self.restarting.store(false, Ordering::Relaxed);
        result
    }

    /// Time allowed for registration after a (re)start.
    pub fn start_timeout(mut self, ticks: u64) -> Self {
        self.start_ticks = ticks;
        self
    }

    pub fn ports(mut self, range: Range<u64>) -> Self {
        self.ports.push(range);
        self
    }

    pub fn irq(mut self, line: u8) -> Self {
        self.irq = Some(line);
        self
    }

    pub fn dma(mut self, pages: u64) -> Self {
        self.dma_pages = pages;
        self
    }

    pub fn arg(mut self, arg: String) -> Self {
        self.args.push(arg);
        self
    }
}

/// dma_map(&phys): maps the server's DMA area (physically contiguous,
/// allocated on first use) and returns its address; its physical address
/// is stored at `phys`.
pub fn dma_map(phys_out: u64) -> SysResult {
    let server = with_current(|p| p.server.clone()).ok_or(EPERM)?;
    if server.dma_pages == 0 {
        return Err(EPERM);
    }
    let phys = {
        let mut dma = server.dma.lock();
        match *dma {
            Some(phys) => phys,
            None => {
                let phys = memory::with_frames(|f| f.allocate_contiguous(server.dma_pages)).ok_or(ENOMEM)?;
                unsafe { core::ptr::write_bytes(memory::phys_to_virt(phys), 0, (server.dma_pages * 4096) as usize) };
                *dma = Some(phys);
                phys
            }
        }
    };
    let len = server.dma_pages * 4096;
    let start = {
        let mm = with_current(|p| p.mm())?;
        let mut space = mm.lock();
        let floor = space.brk_end;
        let start = space.find_free(len, floor).ok_or(ENOMEM)?;
        space.map_phys(start, phys, server.dma_pages, address_space::Prot::RW).map_err(|_| ENOMEM)?;
        start
    };
    uaccess::write(phys_out, phys)?;
    Ok(start as i64)
}

fn start_env() -> Vec<String> {
    ["PATH=/bin", "HOME=/root", "TERM=linux", "PS1=\\w # "].iter().map(|e| e.to_string()).collect()
}

/// Starts a program as a child of the kernel shell, with the console as
/// stdin/stdout/stderr. Names without '/' are looked up in /bin.
pub fn spawn(name: &str, args: &[&str]) -> Result<Pid, i64> {
    let path = if name.contains('/') { name.to_string() } else { alloc::format!("/bin/{name}") };
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let image = load_path("/", &path, &args, &start_env())?;
    spawn_with(&path, image, None, &args)
}

/// Starts a privileged server process: it may register IPC services and
/// request its I/O ports, and it does not take over the terminal.
pub fn spawn_server(server: &Arc<Server>) -> Result<Pid, i64> {
    let mut args = alloc::vec![server.path.to_string()];
    args.extend(server.args.iter().cloned());
    let image = loader::load(&server.image, &args, &start_env())?;
    spawn_with(server.path, image, Some(server), &args)
}

fn spawn_with(path: &str, image: loader::Image, server: Option<&Arc<Server>>, args: &[String]) -> Result<Pid, i64> {
    let console = OpenFile::console();
    let slot = sched::reserve_pid()?;
    let pid = slot.pid;
    // Servers belong to the kernel, even when a program's request
    // (re)started them, so no program can wait for or signal them.
    let parent = if server.is_some() { 0 } else { current_pid() };
    let name = basename(path).to_string();
    let mut info = Info::new(parent, pid, pid, name);
    info.cmdline = cmdline_of(args);
    info.exe = path.to_string();
    info.mem = Some(image.space.stats.clone());
    let mut own = Process::empty();
    own.mm = Some(address_space::Mm::new(image.space).ok_or(ENOMEM)?);
    own.fds = vec![Some(FdEntry { file: console, cloexec: false }); 3];
    own.server = server.cloned();
    let t = new_task(pid, info, own, Frame::user_start(image.entry, image.sp))?;
    t.privileged.store(server.is_some(), Ordering::Relaxed);
    slot.insert(t.clone());
    if server.is_none() {
        crate::drivers::tty::set_foreground(pid);
    }
    sched::start(t);
    Ok(pid)
}

pub fn fork(frame: &Frame) -> Result<Pid, i64> {
    let slot = sched::reserve_pid()?;
    let pid = slot.pid;
    let me = current();
    let parent = unsafe { me.own() };
    let space = parent.mm()?.lock().clone_user().map_err(|_| ENOMEM)?;
    let mut child_frame = *frame;
    child_frame.rax = 0;
    let info = {
        let i = me.info.lock();
        // The parent-death signal is cleared for the child, as on Linux.
        Info {
            dumpable: i.dumpable,
            no_new_privs: i.no_new_privs,
            cmdline: i.cmdline.clone(),
            exe: i.exe.clone(),
            mem: Some(space.stats.clone()),
            nice: i.nice,
            ..Info::new(me.pid, i.pgid, i.sid, i.name.clone())
        }
    };
    let own = Process {
        mm: Some(address_space::Mm::new(space).ok_or(ENOMEM)?),
        fds: parent.fds.clone(),
        cwd: parent.cwd.clone(),
        io_bitmap: None,
        server: None,
    };
    let child = new_task(pid, info, own, child_frame)?;
    // The child resumes with the parent's FPU registers and TLS pointer.
    unsafe {
        let state = child.cpu_state();
        state.fs_base = FsBase::read().as_u64();
        core::arch::asm!("fxsave64 [{}]", in(reg) state.fpu.0.as_mut_ptr(), options(nostack));
    }
    *child.sig.lock() = me.sig.lock().for_child();
    child.affinity.store(me.affinity.load(Ordering::Relaxed), Ordering::Relaxed);
    slot.insert(child.clone());
    sched::start(child);
    Ok(pid)
}

pub fn exec(frame: &mut Frame, path: &str, args: &[String], envs: &[String]) -> Result<(), i64> {
    let cwd = with_current(|p| p.cwd.clone());
    let image = load_path(&cwd, path, args, envs)?;
    let mm = address_space::Mm::new(image.space).ok_or(ENOMEM)?;
    let me = current();
    {
        let mut info = me.info.lock();
        info.name = basename(path).to_string();
        info.cmdline = cmdline_of(args);
        info.exe = absolute(&cwd, path);
        info.mem = Some(mm.stats.clone());
    }
    me.sig.lock().reset_on_exec();
    // A new program gets no inherited hardware access.
    me.privileged.store(false, Ordering::Relaxed);
    let (closed, old_mm) = with_current(|p| {
        tlb::switch(p.mm.as_ref().map(|m| &*m.tlb), Some(&mm.tlb));
        let old_mm = p.mm.replace(mm);
        p.io_bitmap = None;
        p.server = None;
        crate::smp::cpu().tables().set_io_bitmap(None);
        let closed: Vec<FdEntry> = p
            .fds
            .iter_mut()
            .filter(|e| e.as_ref().is_some_and(|e| e.cloexec))
            .filter_map(|e| e.take())
            .collect();
        (closed, old_mm)
    });
    drop(closed);
    // The old address space is freed here (unless a vfork parent shares
    // it); no CPU has it loaded for this task any more.
    drop(old_mm);
    FsBase::write(VirtAddr::new(0));
    let initial = task::FpuState::initial();
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) initial.0.as_ptr(), options(nostack)) };
    *frame = Frame::user_start(image.entry, image.sp);
    Ok(())
}

/// Channel a parent sleeps on in wait4.
fn child_chan(parent: Pid) -> usize {
    0x5_0000_0000 + parent as usize
}

/// Wakes a parent blocked in wait4.
pub fn notify_parent(ppid: Pid) {
    wakeup(child_chan(ppid));
}

pub fn exit(status: i32) -> ! {
    interrupts::disable();
    let me = current();
    assert!(me.pid != 0, "kernel task must not call exit");
    // Close files first: this may wake pipe readers/writers.
    let fds = core::mem::take(&mut unsafe { me.own() }.fds);
    drop(fds);
    ipc::on_exit(me.pid);
    irq::on_exit(me.pid);
    let mm = unsafe { me.own() }.mm.take();
    tlb::switch(mm.as_ref().map(|m| &*m.tlb), None);
    drop(mm);
    unsafe { me.own() }.server = None;
    me.info.lock().mem = None;
    // Orphans go to the kernel, which reaps them; those that asked for it
    // (PR_SET_PDEATHSIG) get a signal.
    let mut death_signals: Vec<(Pid, u32)> = Vec::new();
    {
        let table = TABLE.lock();
        for t in table.tasks.values() {
            let mut info = t.info.lock();
            if info.ppid == me.pid {
                info.ppid = 0;
                if info.pdeath_sig != 0 {
                    death_signals.push((t.pid, info.pdeath_sig));
                }
            }
        }
    }
    for (pid, sig) in death_signals {
        signal::send(pid, sig);
    }
    let ppid = {
        let mut info = me.info.lock();
        info.exit_status = Some(status);
        info.ppid
    };
    {
        let _w = me.wake_lock.lock();
        me.set_state(State::Zombie);
    }
    notify_parent(ppid);
    signal::send(ppid, signal::SIGCHLD);
    schedule();
    unreachable!("zombie was scheduled again");
}

const WNOHANG: u64 = 1;
const WUNTRACED: u64 = 2;
const WCONTINUED: u64 = 8;

/// Waits for a child selected like wait4's `pid` (> 0: that child, 0: same
/// process group, -1: any, < -1: group -pid). Returns (pid, wait status),
/// or None with WNOHANG if nothing is ready. Stops and continues are
/// reported with WUNTRACED and WCONTINUED.
fn wait_child(pid: i64, options: u64) -> Result<Option<(Pid, i32)>, i64> {
    let me = current();
    let my_pgid = me.info.lock().pgid;
    let wanted = |r: i32| {
        (r == signal::CONTINUED_STATUS && options & WCONTINUED != 0) || (r != signal::CONTINUED_STATUS && options & WUNTRACED != 0)
    };
    loop {
        let wait = prepare_to_wait(child_chan(me.pid));
        let mut any_child = false;
        let mut found: Option<(Arc<Task>, i32, bool)> = None;
        {
            let table = TABLE.lock();
            for t in table.tasks.values() {
                if t.pid == me.pid {
                    continue;
                }
                let mut info = t.info.lock();
                let selected = info.ppid == me.pid
                    && match pid {
                        p if p > 0 => t.pid == p as Pid,
                        0 => info.pgid == my_pgid,
                        -1 => true,
                        p => info.pgid == p.unsigned_abs(),
                    };
                if !selected {
                    continue;
                }
                any_child = true;
                if let Some(status) = info.exit_status {
                    found = Some((t.clone(), status, true));
                    break;
                }
                if let Some(r) = info.report.filter(|&r| wanted(r)) {
                    info.report = None;
                    found = Some((t.clone(), r, false));
                    break;
                }
            }
        }
        if !any_child {
            return Err(ECHILD);
        }
        if let Some((child, status, dead)) = found {
            if dead {
                TABLE.lock().tasks.remove(&child.pid);
                // Its CPU may still be switching away from it.
                while child.on_cpu.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
            }
            return Ok(Some((child.pid, status)));
        }
        if options & WNOHANG != 0 {
            return Ok(None);
        }
        // Checked after the scan: a finished child wins over a signal.
        if signal::interrupted() {
            return Err(EINTR);
        }
        wait.sleep();
    }
}

pub fn wait4(pid: i64, status_ptr: u64, options: u64) -> SysResult {
    match wait_child(pid, options)? {
        Some((pid, status)) => {
            if status_ptr != 0 {
                uaccess::write(status_ptr, status)?;
            }
            Ok(pid as i64)
        }
        None => Ok(0),
    }
}

/// For the kernel shell: blocks until a specific child exits or stops.
pub fn wait_for(pid: Pid) -> Result<i32, i64> {
    wait_child(pid as i64, WUNTRACED).map(|r| r.expect("blocking wait always yields a result").1)
}

/// Reaps zombies whose parent (the kernel) no longer waits for them.
pub fn reap_orphans() {
    while let Ok(Some(_)) = wait_child(-1, WNOHANG) {}
}

pub enum WaitStatus {
    Exited(i32),
    Killed(i32),
    Stopped(i32),
}

pub fn decode_status(status: i32) -> WaitStatus {
    match status & 0x7f {
        0 => WaitStatus::Exited((status >> 8) & 0xff),
        0x7f => WaitStatus::Stopped((status >> 8) & 0xff),
        sig => WaitStatus::Killed(sig),
    }
}
