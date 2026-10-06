pub mod address_space;
pub mod elf;
pub mod errno;
mod loader;
mod sys_file;
mod sys_mem;
pub mod syscall;
pub mod uaccess;

use crate::fs::file::OpenFile;
use crate::fs::{self, Node};
use crate::interrupts::gdt;
use address_space::AddressSpace;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};
use errno::*;
use syscall::Frame;
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

pub type Pid = u64;

const KSTACK_SIZE: usize = 64 * 1024;
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

pub fn exists(pid: Pid) -> bool {
    interrupts::without_interrupts(|| sched().procs.contains_key(&pid))
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

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Called from the timer interrupt.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    wakeup(TICK_CHAN);
}

pub fn sleep_ticks(n: u64) {
    let deadline = ticks().saturating_add(n);
    while ticks() < deadline {
        sleep_on(TICK_CHAN);
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
/// via `syscall_return`.
fn new_process(pid: Pid, ppid: Pid, name: String, space: AddressSpace, frame: Frame) -> Box<Process> {
    // new_zeroed instead of Box::new: no temporary on the (small) kernel stack.
    let mut kstack = unsafe { Box::<KernelStack>::new_zeroed().assume_init() };
    let top = kstack.0.as_mut_ptr() as u64 + KSTACK_SIZE as u64;
    let frame_addr = top - core::mem::size_of::<Frame>() as u64;
    unsafe {
        (frame_addr as *mut Frame).write(frame);
        // Expected by switch_stacks: r15..rbx (6 words) and a return address.
        ((frame_addr - 8) as *mut u64).write(syscall::syscall_return as *const () as u64);
        for i in 1..=6 {
            ((frame_addr - 8 - i * 8) as *mut u64).write(0);
        }
    }
    Box::new(Process {
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
    })
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn load_path(cwd: &str, path: &str, args: &[String], envs: &[String]) -> Result<loader::Image, i64> {
    let inode = fs::resolve(cwd, path, true)?;
    let node = inode.node.lock();
    match &*node {
        Node::File(data) => loader::load(data.bytes(), args, envs),
        Node::Dir(_) => Err(EISDIR),
        _ => Err(ENOEXEC),
    }
}

/// Starts a program as a child of the kernel shell, with the console as
/// stdin/stdout/stderr. Names without '/' are looked up in /bin.
pub fn spawn(name: &str, args: &[&str]) -> Result<Pid, i64> {
    let path = if name.contains('/') { name.to_string() } else { alloc::format!("/bin/{name}") };
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let envs: Vec<String> = ["PATH=/bin", "HOME=/root", "TERM=linux", "PS1=\\w # "]
        .iter()
        .map(|e| e.to_string())
        .collect();
    let image = load_path("/", &path, &args, &envs)?;
    let console = OpenFile::console();
    Ok(interrupts::without_interrupts(|| {
        let s = sched();
        let pid = s.next_pid;
        s.next_pid += 1;
        let frame = Frame::user_start(image.entry, image.sp);
        let mut p = new_process(pid, s.current, basename(&path).to_string(), image.space, frame);
        p.fds = vec![Some(FdEntry { file: console, cloexec: false }); 3];
        p.brk_start = image.brk;
        p.brk_end = image.brk;
        s.procs.insert(pid, p);
        s.ready.push_back(pid);
        crate::drivers::tty::set_foreground(pid);
        pid
    }))
}

pub fn fork(frame: &Frame) -> Result<Pid, i64> {
    let s = sched();
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
    let mut child = new_process(pid, parent.pid, parent.name.clone(), space, child_frame);
    child.pgid = parent.pgid;
    child.sid = parent.sid;
    child.fds = parent.fds.clone();
    child.cwd = parent.cwd.clone();
    child.brk_start = parent.brk_start;
    child.brk_end = parent.brk_end;
    child.mmap_next = parent.mmap_next;
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
    schedule();
    unreachable!("zombie was scheduled again");
}

/// Waits for a child (`target` = None: any). Returns (pid, status), or
/// None with `nohang` if none has finished yet.
fn wait_child(target: Option<Pid>, nohang: bool) -> Result<Option<(Pid, i32)>, i64> {
    loop {
        let s = sched();
        let me = s.current;
        let is_target = |p: &Process| p.ppid == me && p.pid != me && target.is_none_or(|t| t == p.pid);
        if !s.procs.values().any(|p| is_target(p)) {
            return Err(ECHILD);
        }
        let zombie = s.procs.values().find(|p| is_target(p) && matches!(p.state, State::Zombie(_)));
        if let Some(z) = zombie {
            let (pid, State::Zombie(status)) = (z.pid, z.state) else { unreachable!() };
            s.procs.remove(&pid);
            return Ok(Some((pid, status)));
        }
        if nohang {
            return Ok(None);
        }
        s.cur().state = State::WaitChild;
        schedule();
    }
}

pub fn wait4(pid: i64, status_ptr: u64, options: u64) -> SysResult {
    const WNOHANG: u64 = 1;
    let target = if pid > 0 { Some(pid as Pid) } else { None };
    match wait_child(target, options & WNOHANG != 0)? {
        Some((pid, status)) => {
            if status_ptr != 0 {
                uaccess::write(status_ptr, status)?;
            }
            Ok(pid as i64)
        }
        None => Ok(0),
    }
}

/// For the kernel shell: blocks until a specific child exits.
pub fn wait_for(pid: Pid) -> Result<i32, i64> {
    interrupts::without_interrupts(|| wait_child(Some(pid), false))
        .map(|r| r.expect("blocking wait always yields a result").1)
}

/// Reaps zombies whose parent (the kernel) no longer waits for them.
pub fn reap_orphans() {
    interrupts::without_interrupts(|| while let Ok(Some(_)) = wait_child(None, true) {});
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

/// (true, exit code) on normal exit, (false, signal) when killed.
pub fn decode_status(status: i32) -> (bool, i32) {
    if status & 0x7f == 0 {
        (true, (status >> 8) & 0xff)
    } else {
        (false, status & 0x7f)
    }
}
