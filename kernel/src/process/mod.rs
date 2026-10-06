pub mod address_space;
pub mod elf;
pub mod errno;
pub mod ipc;
pub mod irq;
mod loader;
pub mod signal;
mod sys_file;
mod sys_mem;
mod sys_net;
pub mod syscall;
pub mod uaccess;

use crate::fs::file::OpenFile;
use crate::fs;
use crate::interrupts::gdt;
use crate::memory;
use address_space::AddressSpace;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};
use errno::*;
use syscall::Frame;
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

pub type Pid = u64;

const KSTACK_SIZE: usize = 64 * 1024;
/// Upper bound on processes, so fork bombs fail with EAGAIN.
const MAX_PROCS: usize = 256;
const MMAP_TOP: u64 = 0x0000_7000_0000_0000;
pub const TIMER_HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);
const TICK_CHAN: usize = 1;

#[repr(C, align(16))]
struct KernelStack([u8; KSTACK_SIZE]);

#[repr(C, align(16))]
struct FpuState([u8; 512]);

impl FpuState {
    fn initial() -> Box<Self> {
        let mut s = Box::new(FpuState([0; 512]));
        s.0[0..2].copy_from_slice(&0x037f_u16.to_le_bytes()); // FCW
        s.0[24..28].copy_from_slice(&0x1f80_u32.to_le_bytes()); // MXCSR
        s
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ready,
    Running,
    WaitChild,
    /// Sleeps until `wakeup` is called with this channel.
    Sleeping(usize),
    /// Stopped by a signal until SIGCONT.
    Stopped,
    /// Wait status in Linux format (exit code << 8 or signal number).
    Zombie(i32),
}

#[derive(Clone)]
struct FdEntry {
    file: Arc<OpenFile>,
    cloexec: bool,
}

struct Process {
    pid: Pid,
    ppid: Pid,
    pgid: Pid,
    sid: Pid,
    name: String,
    state: State,
    space: Option<AddressSpace>,
    kstack: Option<Box<KernelStack>>,
    kernel_rsp: u64,
    fs_base: u64,
    fpu: Box<FpuState>,
    fds: Vec<Option<FdEntry>>,
    cwd: String,
    brk_start: u64,
    brk_end: u64,
    mmap_next: u64,
    signals: signal::Signals,
    /// Stop/continue event not yet collected by the parent's wait4.
    report: Option<i32>,
    /// Servers started by the kernel may register IPC services and ask
    /// for I/O ports.
    privileged: bool,
    /// I/O permission bitmap (0 = allowed) installed in the TSS while
    /// this process runs.
    io_bitmap: Option<Box<[u8; gdt::IOMAP_BYTES]>>,
    /// The server this process runs, with the resources assigned to it.
    server: Option<Arc<Server>>,
    /// Timer tick at which a sleep ends on its own (0: none).
    wake_at: u64,
    /// ITIMER_REAL: next SIGALRM tick (0: off) and the reload interval.
    alarm_at: u64,
    alarm_every: u64,
}

impl Process {
    fn kstack_top(&self) -> Option<u64> {
        self.kstack
            .as_ref()
            .map(|s| s.0.as_ptr() as u64 + KSTACK_SIZE as u64)
    }

    fn space(&mut self) -> Result<&mut AddressSpace, i64> {
        self.space.as_mut().ok_or(EINVAL)
    }

    fn file(&self, fd: u64) -> Result<Arc<OpenFile>, i64> {
        self.fds
            .get(fd as usize)
            .and_then(|e| e.as_ref())
            .map(|e| e.file.clone())
            .ok_or(EBADF)
    }

    /// Kleinster freier Deskriptor >= `min`.
    fn alloc_fd(&mut self, file: Arc<OpenFile>, cloexec: bool, min: usize) -> Result<i64, i64> {
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

struct Scheduler {
    procs: BTreeMap<Pid, Box<Process>>,
    ready: VecDeque<Pid>,
    current: Pid,
    next_pid: Pid,
}

impl Scheduler {
    fn cur(&mut self) -> &mut Process {
        let pid = self.current;
        self.procs.get_mut(&pid).expect("current process missing")
    }

    fn make_ready(&mut self, pid: Pid) {
        if let Some(p) = self.procs.get_mut(&pid) {
            p.state = State::Ready;
            self.ready.push_back(pid);
        }
    }
}

/// Single-core system: access only with interrupts disabled. A mutex would
/// not work because no lock may be held across a context switch.
struct SchedCell(UnsafeCell<Option<Scheduler>>);
unsafe impl Sync for SchedCell {}
static SCHED: SchedCell = SchedCell(UnsafeCell::new(None));

fn sched() -> &'static mut Scheduler {
    debug_assert!(!interrupts::are_enabled());
    unsafe { (*SCHED.0.get()).as_mut().expect("process::init not called") }
}

/// Runs `f` on the current process. Must not do anything that re-enters
/// the scheduler (sleeping, closing files).
fn with_current<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    interrupts::without_interrupts(|| f(sched().cur()))
}

pub fn init() {
    enable_sse();
    syscall::init();
    let kernel = Process {
        pid: 0,
        ppid: 0,
        pgid: 0,
        sid: 0,
        name: "kernel".to_string(),
        state: State::Running,
        space: None,
        kstack: None,
        kernel_rsp: 0,
        fs_base: 0,
        fpu: FpuState::initial(),
        fds: Vec::new(),
        cwd: "/".to_string(),
        brk_start: 0,
        brk_end: 0,
        mmap_next: 0,
        signals: signal::Signals::default(),
        report: None,
        privileged: false,
        io_bitmap: None,
        server: None,
        wake_at: 0,
        alarm_at: 0,
        alarm_every: 0,
    };
    let mut procs = BTreeMap::new();
    procs.insert(0, Box::new(kernel));
    unsafe {
        *SCHED.0.get() = Some(Scheduler {
            procs,
            ready: VecDeque::new(),
            current: 0,
            next_pid: 1,
        })
    };
}

/// musl uses SSE; without OSFXSR every SSE instruction raises #UD.
fn enable_sse() {
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
    with_current(|p| p.pid)
}

pub fn current_ppid() -> Pid {
    with_current(|p| p.ppid)
}

/// `pid` 0 means the calling process.
fn with_process<R>(pid: Pid, f: impl FnOnce(&mut Process, Pid) -> R) -> Result<R, i64> {
    interrupts::without_interrupts(|| {
        let s = sched();
        let me = s.current;
        let target = if pid == 0 { me } else { pid };
        s.procs.get_mut(&target).map(|p| f(p, me)).ok_or(ESRCH)
    })
}

pub fn setpgid(pid: Pid, pgid: Pid) -> SysResult {
    with_process(pid, |p, me| {
        if p.pid != me && p.ppid != me {
            return Err(ESRCH);
        }
        p.pgid = if pgid == 0 { p.pid } else { pgid };
        Ok(0)
    })?
}

pub fn getpgid(pid: Pid) -> SysResult {
    with_process(pid, |p, _| p.pgid as i64)
}

pub fn getsid(pid: Pid) -> SysResult {
    with_process(pid, |p, _| p.sid as i64)
}

pub fn setsid() -> SysResult {
    with_current(|p| {
        p.sid = p.pid;
        p.pgid = p.pid;
        Ok(p.pid as i64)
    })
}

/// ioperm(from, count, on) for privileged servers: grants or revokes
/// access to I/O ports via the TSS bitmap. Only the ports the kernel
/// assigned to the server can be granted.
pub fn ioperm(from: u64, count: u64, on: u64) -> SysResult {
    let end = from.checked_add(count).filter(|&e| e <= 0x10000).ok_or(EINVAL)?;
    with_current(|p| {
        if !p.privileged {
            return Err(EPERM);
        }
        let assigned = p.server.as_ref().map_or(&[][..], |s| &s.ports[..]);
        if on != 0 && !assigned.iter().any(|r| r.start <= from && end <= r.end) {
            return Err(EPERM);
        }
        let bitmap = p.io_bitmap.get_or_insert_with(|| Box::new([0xff; gdt::IOMAP_BYTES]));
        for port in from..end {
            let (byte, bit) = ((port / 8) as usize, 1u8 << (port % 8));
            if on != 0 {
                bitmap[byte] &= !bit;
            } else {
                bitmap[byte] |= bit;
            }
        }
        gdt::set_io_bitmap(Some(bitmap));
        Ok(0)
    })
}

/// (pid, parent, name, state, server) of every process, for the monitor.
pub fn list() -> Vec<(Pid, Pid, String, &'static str, bool)> {
    interrupts::without_interrupts(|| {
        sched()
            .procs
            .values()
            .map(|p| {
                let state = match p.state {
                    State::Running => "running",
                    State::Ready => "ready",
                    State::WaitChild | State::Sleeping(_) => "sleeping",
                    State::Stopped => "stopped",
                    State::Zombie(_) => "zombie",
                };
                (p.pid, p.ppid, p.name.clone(), state, p.privileged)
            })
            .collect()
    })
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Called from the timer interrupt.
pub fn tick() {
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    wakeup(TICK_CHAN);
    interrupts::without_interrupts(|| {
        let s = sched();
        let due: Vec<Pid> = s
            .procs
            .values()
            .filter(|p| p.wake_at != 0 && p.wake_at <= now && matches!(p.state, State::Sleeping(_)))
            .map(|p| p.pid)
            .collect();
        for pid in due {
            s.make_ready(pid);
        }
        // Expired timers whose signal does not fit here stay expired and
        // fire on the next tick, so no SIGALRM is ever lost.
        let mut alarms: heapless::Vec<Pid, 64> = heapless::Vec::new();
        for p in s.procs.values_mut().filter(|p| p.alarm_at != 0 && p.alarm_at <= now) {
            if alarms.push(p.pid).is_err() {
                break;
            }
            p.alarm_at = if p.alarm_every != 0 { now.saturating_add(p.alarm_every) } else { 0 };
        }
        for pid in alarms {
            signal::send(pid, signal::SIGALRM);
        }
    });
}

/// setitimer(ITIMER_REAL, new, old) with timer-tick resolution. `new` and
/// `old` are (interval, value) in microseconds; a value of 0 disarms.
pub fn set_alarm(value_us: u64, interval_us: u64) -> (u64, u64) {
    let to_ticks = |us: u64| us.saturating_mul(TIMER_HZ).div_ceil(1_000_000);
    let to_us = |ticks: u64| ticks.saturating_mul(1_000_000) / TIMER_HZ;
    interrupts::without_interrupts(|| {
        let now = ticks();
        let p = sched().cur();
        let old = (to_us(p.alarm_at.saturating_sub(now)), to_us(p.alarm_every));
        p.alarm_at = if value_us == 0 { 0 } else { now.saturating_add(to_ticks(value_us).max(1)) };
        p.alarm_every = if value_us == 0 { 0 } else { to_ticks(interval_us) };
        old
    })
}

/// The current ITIMER_REAL setting: (remaining, interval) in microseconds.
pub fn get_alarm() -> (u64, u64) {
    interrupts::without_interrupts(|| {
        let now = ticks();
        let p = sched().cur();
        let remaining = if p.alarm_at == 0 { 0 } else { p.alarm_at.saturating_sub(now).max(1) };
        let to_us = |ticks: u64| ticks.saturating_mul(1_000_000) / TIMER_HZ;
        (to_us(remaining), to_us(p.alarm_every))
    })
}

/// Like `sleep_on`, but also wakes up at timer tick `deadline`.
pub fn sleep_on_until(chan: usize, deadline: u64) {
    interrupts::without_interrupts(|| {
        let p = sched().cur();
        p.state = State::Sleeping(chan);
        p.wake_at = deadline.max(1);
        schedule();
        sched().cur().wake_at = 0;
    });
}

/// Sleeps for `n` timer ticks; a signal ends the sleep early with EINTR.
pub fn sleep_ticks(n: u64) -> Result<(), i64> {
    let deadline = ticks().saturating_add(n);
    while ticks() < deadline {
        sleep_on(TICK_CHAN);
        if signal::interrupted() {
            return Err(EINTR);
        }
    }
    Ok(())
}

/// pause(2): sleeps until a signal arrives.
pub fn pause() -> SysResult {
    const PAUSE_CHAN: usize = 3;
    loop {
        if signal::interrupted() {
            return Err(EINTR);
        }
        sleep_on(PAUSE_CHAN);
    }
}

pub fn sleep_on(chan: usize) {
    interrupts::without_interrupts(|| {
        sched().cur().state = State::Sleeping(chan);
        schedule();
    });
}

pub fn wakeup(chan: usize) {
    interrupts::without_interrupts(|| {
        let s = sched();
        let pids: Vec<Pid> = s
            .procs
            .values()
            .filter(|p| p.state == State::Sleeping(chan))
            .map(|p| p.pid)
            .collect();
        for pid in pids {
            s.make_ready(pid);
        }
    });
}

/// New process that, when first scheduled, enters ring 3 with `frame`
/// via `user_return`.
fn new_process(pid: Pid, ppid: Pid, name: String, space: AddressSpace, frame: Frame) -> Result<Box<Process>, i64> {
    // Kernel stacks are demanded by user space (fork), so they must not
    // eat into the reserve the kernel heap relies on.
    if !crate::memory::with_frames(|f| f.user_may_take((KSTACK_SIZE / 4096) as u64)) {
        return Err(ENOMEM);
    }
    // try_new_zeroed instead of Box::new: no temporary on the (small) kernel
    // stack, and running out of memory is an error, not a panic.
    let mut kstack = unsafe { Box::<KernelStack>::try_new_zeroed().map_err(|_| ENOMEM)?.assume_init() };
    let top = kstack.0.as_mut_ptr() as u64 + KSTACK_SIZE as u64;
    let frame_addr = top - core::mem::size_of::<Frame>() as u64;
    unsafe {
        (frame_addr as *mut Frame).write(frame);
        // Expected by switch_stacks: r15..rbx (6 words) and a return address.
        ((frame_addr - 8) as *mut u64).write(syscall::user_return as *const () as u64);
        for i in 1..=6 {
            ((frame_addr - 8 - i * 8) as *mut u64).write(0);
        }
    }
    Ok(Box::new(Process {
        pid,
        ppid,
        pgid: pid,
        sid: pid,
        name,
        state: State::Ready,
        space: Some(space),
        kstack: Some(kstack),
        kernel_rsp: frame_addr - 56,
        fs_base: 0,
        fpu: FpuState::initial(),
        fds: Vec::new(),
        cwd: "/".to_string(),
        brk_start: 0,
        brk_end: 0,
        mmap_next: MMAP_TOP,
        signals: signal::Signals::default(),
        report: None,
        privileged: false,
        io_bitmap: None,
        server: None,
        wake_at: 0,
        alarm_at: 0,
        alarm_every: 0,
    }))
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
}

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
        Ok(Server { name, path, image, ports: Vec::new(), irq: None, dma_pages: 0, dma: spin::Mutex::new(None), args: Vec::new() })
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
    let start = with_current(|p| -> Result<u64, i64> {
        p.mmap_next = p.mmap_next.checked_sub(len).filter(|&a| a > p.brk_end).ok_or(ENOMEM)?;
        let start = p.mmap_next;
        let flags = x86_64::structures::paging::PageTableFlags::WRITABLE | x86_64::structures::paging::PageTableFlags::NO_EXECUTE;
        p.space()?.map_phys(start, phys, server.dma_pages, flags).map_err(|_| ENOMEM)?;
        Ok(start)
    })?;
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
    spawn_with(&path, image, None)
}

/// Starts a privileged server process: it may register IPC services and
/// request its I/O ports, and it does not take over the terminal.
pub fn spawn_server(server: &Arc<Server>) -> Result<Pid, i64> {
    let mut args = alloc::vec![server.path.to_string()];
    args.extend(server.args.iter().cloned());
    let image = loader::load(&server.image, &args, &start_env())?;
    spawn_with(server.path, image, Some(server))
}

fn spawn_with(path: &str, image: loader::Image, server: Option<&Arc<Server>>) -> Result<Pid, i64> {
    let console = OpenFile::console();
    interrupts::without_interrupts(|| {
        let s = sched();
        if s.procs.len() >= MAX_PROCS {
            return Err(EAGAIN);
        }
        let pid = s.next_pid;
        s.next_pid += 1;
        let frame = Frame::user_start(image.entry, image.sp);
        // Servers belong to the kernel, even when a program's request
        // (re)started them, so no program can wait for or signal them.
        let parent = if server.is_some() { 0 } else { s.current };
        let mut p = new_process(pid, parent, basename(path).to_string(), image.space, frame)?;
        p.fds = vec![Some(FdEntry { file: console, cloexec: false }); 3];
        p.brk_start = image.brk;
        p.brk_end = image.brk;
        p.privileged = server.is_some();
        p.server = server.cloned();
        s.procs.insert(pid, p);
        s.ready.push_back(pid);
        if server.is_none() {
            crate::drivers::tty::set_foreground(pid);
        }
        Ok(pid)
    })
}

pub fn fork(frame: &Frame) -> Result<Pid, i64> {
    let s = sched();
    if s.procs.len() >= MAX_PROCS {
        return Err(EAGAIN);
    }
    let space = s
        .cur()
        .space
        .as_ref()
        .ok_or(EINVAL)?
        .clone_user()
        .map_err(|_| ENOMEM)?;
    let mut child_frame = frame.clone();
    child_frame.rax = 0;
    let pid = s.next_pid;
    s.next_pid += 1;
    let parent = s.cur();
    let mut child = new_process(pid, parent.pid, parent.name.clone(), space, child_frame)?;
    child.pgid = parent.pgid;
    child.sid = parent.sid;
    child.fds = parent.fds.clone();
    child.cwd = parent.cwd.clone();
    child.brk_start = parent.brk_start;
    child.brk_end = parent.brk_end;
    child.mmap_next = parent.mmap_next;
    child.signals = parent.signals.for_child();
    child.fs_base = FsBase::read().as_u64();
    unsafe { fxsave(&mut child.fpu) };
    s.procs.insert(pid, child);
    s.ready.push_back(pid);
    Ok(pid)
}

pub fn exec(frame: &mut Frame, path: &str, args: &[String], envs: &[String]) -> Result<(), i64> {
    let cwd = with_current(|p| p.cwd.clone());
    let image = load_path(&cwd, path, args, envs)?;
    image.space.activate();
    let closed: Vec<FdEntry> = with_current(|p| {
        // The old address space is freed here; it is no longer active.
        p.space = Some(image.space);
        p.name = basename(path).to_string();
        p.brk_start = image.brk;
        p.brk_end = image.brk;
        p.mmap_next = MMAP_TOP;
        p.signals.reset_on_exec();
        // A new program gets no inherited hardware access.
        p.privileged = false;
        p.io_bitmap = None;
        p.server = None;
        gdt::set_io_bitmap(None);
        p.fds
            .iter_mut()
            .filter(|e| e.as_ref().is_some_and(|e| e.cloexec))
            .filter_map(|e| e.take())
            .collect()
    });
    drop(closed);
    FsBase::write(VirtAddr::new(0));
    unsafe { fxrstor(&FpuState::initial()) };
    *frame = Frame::user_start(image.entry, image.sp);
    Ok(())
}

pub fn exit(status: i32) -> ! {
    interrupts::disable();
    // Close files first: this may wake pipe readers/writers.
    let fds = core::mem::take(&mut sched().cur().fds);
    drop(fds);
    ipc::on_exit(sched().current);
    irq::on_exit(sched().current);

    let s = sched();
    let pid = s.current;
    assert!(pid != 0, "kernel task must not call exit");
    for p in s.procs.values_mut() {
        if p.ppid == pid {
            p.ppid = 0;
        }
    }
    unsafe { Cr3::write(crate::memory::kernel_l4(), Cr3Flags::empty()) };
    let me = s.cur();
    me.space = None;
    me.state = State::Zombie(status);
    let ppid = me.ppid;
    if s.procs.get(&ppid).is_some_and(|p| p.state == State::WaitChild) {
        s.make_ready(ppid);
    }
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
    loop {
        let s = sched();
        let me = s.current;
        let my_pgid = s.cur().pgid;
        let is_target = |p: &Process| {
            p.ppid == me
                && p.pid != me
                && match pid {
                    p_ if p_ > 0 => p.pid == p_ as Pid,
                    0 => p.pgid == my_pgid,
                    -1 => true,
                    p_ => p.pgid == p_.unsigned_abs(),
                }
        };
        if !s.procs.values().any(|p| is_target(p)) {
            return Err(ECHILD);
        }
        let zombie = s.procs.values().find(|p| is_target(p) && matches!(p.state, State::Zombie(_)));
        if let Some(z) = zombie {
            let (pid, State::Zombie(status)) = (z.pid, z.state) else { unreachable!() };
            s.procs.remove(&pid);
            return Ok(Some((pid, status)));
        }
        let wanted = |r: i32| {
            (r == signal::CONTINUED_STATUS && options & WCONTINUED != 0)
                || (r != signal::CONTINUED_STATUS && options & WUNTRACED != 0)
        };
        if let Some(p) = s.procs.values_mut().find(|p| is_target(p) && p.report.is_some_and(wanted)) {
            let status = p.report.take().expect("checked above");
            return Ok(Some((p.pid, status)));
        }
        if options & WNOHANG != 0 {
            return Ok(None);
        }
        // Checked after the scans: a finished child wins over a signal.
        if signal::interrupted() {
            return Err(EINTR);
        }
        s.cur().state = State::WaitChild;
        schedule();
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
    interrupts::without_interrupts(|| wait_child(pid as i64, WUNTRACED))
        .map(|r| r.expect("blocking wait always yields a result").1)
}

/// Reaps zombies whose parent (the kernel) no longer waits for them.
pub fn reap_orphans() {
    interrupts::without_interrupts(|| while let Ok(Some(_)) = wait_child(-1, WNOHANG) {});
}

pub fn yield_now() {
    interrupts::without_interrupts(schedule);
}

/// Picks the next runnable process (round robin). Must be called with
/// interrupts disabled.
pub fn schedule() {
    loop {
        let s = sched();
        let cur = s.current;
        if s.cur().state == State::Running {
            s.make_ready(cur);
        }
        if let Some(next) = s.ready.pop_front() {
            if next == cur {
                s.cur().state = State::Running;
            } else {
                switch_to(next);
            }
            return;
        }
        // Nothing runnable: wait for an interrupt.
        interrupts::enable_and_hlt();
        interrupts::disable();
    }
}

fn switch_to(next: Pid) {
    let s = sched();
    let prev = s.cur();
    prev.fs_base = FsBase::read().as_u64();
    unsafe { fxsave(&mut prev.fpu) };
    let prev_rsp: *mut u64 = &mut prev.kernel_rsp;

    let n = s.procs.get_mut(&next).expect("next process missing");
    n.state = State::Running;
    match &n.space {
        Some(space) => space.activate(),
        None => unsafe { Cr3::write(crate::memory::kernel_l4(), Cr3Flags::empty()) },
    }
    if let Some(top) = n.kstack_top() {
        gdt::set_kernel_stack(VirtAddr::new(top));
        syscall::set_kernel_stack(top);
    }
    gdt::set_io_bitmap(n.io_bitmap.as_deref());
    FsBase::write(VirtAddr::new(n.fs_base));
    unsafe { fxrstor(&n.fpu) };
    let next_rsp = n.kernel_rsp;
    s.current = next;
    unsafe { switch_stacks(prev_rsp, next_rsp) };
}

unsafe fn fxsave(area: &mut FpuState) {
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) area.0.as_mut_ptr(), options(nostack)) };
}

unsafe fn fxrstor(area: &FpuState) {
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
