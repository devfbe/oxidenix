pub mod address_space;
pub mod channel;
pub mod clone;
pub mod elf;
pub mod errno;
mod exec;
mod exit;
mod futex;
pub mod ipc;
pub mod irq;
mod loader;
mod prctl;
pub mod query;
pub mod epoll;
pub mod poll;
pub mod sched;
pub mod signal;
mod sys_file;
mod sys_mem;
mod sys_net;
mod sys_time;
pub mod syscall;
pub mod task;
pub mod tlb;
pub mod linux;
mod linux_inode;
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
use sched::{current, TABLE};
use syscall::Frame;
use task::{Files, FsInfo, Info, KernelStack, State, Task, ThreadGroup};
use x86_64::instructions::interrupts;

pub use clone::{clone, set_tid_address};
pub use exec::exec;
pub use exit::{decode_status, exit_group, exit_thread, notify_parent, reap_orphans, wait4, wait_for, WaitStatus};
pub use sched::{prepare_to_sleep, schedule, wakeup};
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

    /// The descriptor table (EBADF after exit).
    pub fn files(&self) -> Result<&Arc<Files>, i64> {
        self.files.as_ref().ok_or(EBADF)
    }

    pub fn file(&self, fd: u64) -> Result<Arc<OpenFile>, i64> {
        self.files()?.get(fd)
    }

    /// Lowest free descriptor >= `min`.
    pub fn alloc_fd(&self, file: Arc<OpenFile>, cloexec: bool, min: usize) -> Result<i64, i64> {
        self.files()?.alloc(file, cloexec, min)
    }

    pub fn cwd(&self) -> String {
        self.fs.as_ref().map_or_else(|| "/".to_string(), |f| f.cwd())
    }

    pub fn set_cwd(&self, cwd: String) {
        if let Some(f) = &self.fs {
            f.set_cwd(cwd);
        }
    }
}

/// Runs `f` on the running task's own state. `f` must not sleep or
/// switch tasks.
pub fn with_current<R>(f: impl FnOnce(&mut Process) -> R) -> R {
    interrupts::without_interrupts(|| f(unsafe { current().own() }))
}

/// The running task's address space.
pub fn current_mm() -> Option<Arc<address_space::Mm>> {
    with_current(|p| p.mm.clone())
}

/// The running task's descriptor table.
pub fn current_files() -> Result<Arc<Files>, i64> {
    with_current(|p| p.files().cloned())
}

/// Process 0, the kernel monitor, runs on the boot stack of the bootstrap
/// CPU; each CPU also has an idle task.
pub fn init() {
    enable_sse();
    tlb::init_cpu(true);
    syscall::init();
    let group = ThreadGroup::new(0, Info::new(0, 0, 0, "kernel".to_string()), Default::default()).expect("boot");
    let mut own = Process::empty();
    own.files = Files::new(Vec::new());
    own.fs = FsInfo::new("/".to_string());
    let kernel = Arc::new(Task::new(0, group.clone(), "kernel".to_string(), own, None, 0));
    group.info.lock().threads.push(kernel.clone());
    {
        let mut table = TABLE.lock();
        table.tasks.insert(0, kernel.clone());
        table.groups.insert(0, group);
    }
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

/// The calling process's id (its thread group).
pub fn current_pid() -> Pid {
    current().tgid()
}

/// The calling thread's id.
pub fn current_tid() -> Pid {
    current().tid()
}

pub fn current_ppid() -> Pid {
    current().group.info.lock().ppid
}

/// The thread with id `tid` (0: the caller).
pub fn task(tid: Pid) -> Option<Arc<Task>> {
    if tid == 0 {
        return Some(sched::current_arc());
    }
    TABLE.lock().tasks.get(&tid).cloned()
}

/// The process with id `pid` (0: the caller's), also found by the id of
/// one of its threads, as Linux does for process ids.
pub fn group(pid: Pid) -> Option<Arc<ThreadGroup>> {
    if pid == 0 {
        return Some(current().group.clone());
    }
    let table = TABLE.lock();
    table.groups.get(&pid).cloned().or_else(|| table.tasks.get(&pid).map(|t| t.group.clone()))
}

pub fn setpgid(pid: Pid, pgid: Pid) -> SysResult {
    let me = current_pid();
    let g = group(pid).ok_or(ESRCH)?;
    let mut info = g.info.lock();
    if g.tgid != me && info.ppid != me {
        return Err(ESRCH);
    }
    info.pgid = if pgid == 0 { g.tgid } else { pgid };
    Ok(0)
}

pub fn getpgid(pid: Pid) -> SysResult {
    Ok(group(pid).ok_or(ESRCH)?.info.lock().pgid as i64)
}

pub fn getsid(pid: Pid) -> SysResult {
    Ok(group(pid).ok_or(ESRCH)?.info.lock().sid as i64)
}

pub fn setsid() -> SysResult {
    let g = &current().group;
    let mut info = g.info.lock();
    info.sid = g.tgid;
    info.pgid = g.tgid;
    Ok(g.tgid as i64)
}

/// ioperm(from, count, on) for privileged servers: grants or revokes
/// access to I/O ports via the TSS bitmap. Only the ports the kernel
/// assigned to the server can be granted.
pub fn ioperm(from: u64, count: u64, on: u64) -> SysResult {
    let end = from.checked_add(count).filter(|&e| e <= 0x10000).ok_or(EINVAL)?;
    if !current().group.privileged.load(Ordering::Relaxed) {
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

/// (pid, parent, name, state, server, cpu, threads) of every process, for
/// the monitor.
pub fn list() -> Vec<(Pid, Pid, String, &'static str, bool, usize, usize)> {
    let groups: Vec<Arc<ThreadGroup>> = TABLE.lock().groups.values().cloned().collect();
    groups
        .iter()
        .map(|g| {
            let info = g.info.lock();
            let main = info.threads.first();
            let state = match main.map(|t| t.state()) {
                None => "zombie",
                Some(State::Running) => "running",
                Some(State::Runnable) => "ready",
                Some(State::Sleeping) => "sleeping",
                Some(State::Stopped) => "stopped",
                Some(State::Dead) => "exiting",
            };
            let cpu = main.map_or(0, |t| t.last_cpu.load(Ordering::Relaxed));
            (g.tgid, info.ppid, info.name.clone(), state, g.privileged.load(Ordering::Relaxed), cpu, info.threads.len())
        })
        .collect()
}

/// Bit mask of the CPUs that run.
pub fn online_mask() -> u64 {
    (0..crate::smp::MAX_CPUS).filter(|&i| crate::smp::by_index(i).is_some()).fold(0, |m, i| m | 1 << i)
}

/// sched_getaffinity(tid, size, mask): the CPUs the thread may run on, as
/// a 64-bit mask; returns the bytes written, as Linux does.
pub fn sched_getaffinity(pid: Pid, size: u64, mask: u64) -> SysResult {
    if size < 8 {
        return Err(EINVAL);
    }
    let t = task(pid).ok_or(ESRCH)?;
    uaccess::write(mask, t.affinity.load(Ordering::Relaxed) & online_mask())?;
    Ok(8)
}

/// sched_setaffinity(tid, size, mask): restricts the thread to the CPUs in
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
    if t.group.privileged.load(Ordering::Relaxed) && current_pid() != 0 {
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

/// Sleeps until `deadline` (nanoseconds since boot); a signal ends the
/// sleep early with EINTR.
pub fn sleep_until(deadline: u64) -> Result<(), i64> {
    loop {
        let wait = prepare_to_sleep();
        if crate::time::now() >= deadline {
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
fn new_task(tid: Pid, group: Arc<ThreadGroup>, comm: String, own: Process, frame: Frame) -> Result<Arc<Task>, i64> {
    // Kernel stacks are demanded by user space (clone): charged to the
    // commit limit, so running out is ENOMEM.
    let kstack = KernelStack::new(true).ok_or(ENOMEM)?;
    let rsp = sched::prepare_stack(&kstack, Some(frame), true);
    Arc::try_new(Task::new(tid, group, comm, own, Some(kstack), rsp)).map_err(|_| ENOMEM)
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
    load_inode(fs::resolve(cwd, path, true)?, args, envs)
}

fn load_inode(inode: Arc<fs::Inode>, args: &[String], envs: &[String]) -> Result<loader::Image, i64> {
    if inode.file_type() != fs::S_IFREG {
        return Err(if inode.is_dir() { EISDIR } else { ENOEXEC });
    }
    // Not while it is open for writing; nobody may write it while it runs.
    let exe = inode.deny_write_access()?;
    let cache = inode.cache()?;
    let file: address_space::Hold = fs::MappedFile::new(inode, false)?;
    let exe: address_space::Hold = Arc::try_new(exe).map_err(|_| ENOMEM)?;
    loader::load(&cache, Some(file), Some(exe), args, envs)
}

/// A user-space server the kernel starts and restarts.
pub struct Server {
    /// Name of the IPC service it registers.
    pub name: &'static str,
    pub path: &'static str,
    /// The program, copied once at boot: a restart must not run whatever
    /// was written to `path` since then with the server's privileges.
    image: Arc<fs::cache::PageCache>,
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
    /// How long a (re)started server may take to register its service,
    /// in nanoseconds.
    start_timeout: u64,
    restarts: core::sync::atomic::AtomicU32,
    restarting: core::sync::atomic::AtomicBool,
    /// Where its devices reach the pages granted to it (channels).
    pub domain: channel::DmaDomain,
}

/// The servers the kernel started, by name: a channel to a dead one
/// starts it again (`server_named`).
static SERVERS: spin::Mutex<Vec<Arc<Server>>> = spin::Mutex::new(Vec::new());

/// The server the kernel started under the IPC name `name`.
pub fn server_named(name: &str) -> Option<Arc<Server>> {
    SERVERS.lock().iter().find(|s| s.name == name).cloned()
}

/// How often a dead server is restarted before the kernel gives up on it.
const MAX_RESTARTS: u32 = 5;

impl Server {
    pub fn load(name: &'static str, path: &'static str) -> Result<Server, i64> {
        let inode = fs::resolve("/", path, true)?;
        if inode.file_type() != fs::S_IFREG {
            return Err(ENOEXEC);
        }
        let image = fs::cache::PageCache::copy_of(&inode)?;
        Ok(Server {
            name,
            path,
            image,
            ports: Vec::new(),
            irq: None,
            dma_pages: 0,
            dma: spin::Mutex::new(None),
            args: Vec::new(),
            start_timeout: 3 * crate::time::NSEC_PER_SEC,
            restarts: core::sync::atomic::AtomicU32::new(0),
            restarting: core::sync::atomic::AtomicBool::new(false),
            domain: channel::DmaDomain::default(),
        })
    }

    /// Starts the server and waits for it to register: (service, argument).
    pub fn start(self: &Arc<Self>) -> Result<(usize, u64), i64> {
        {
            let mut servers = SERVERS.lock();
            if !servers.iter().any(|s| Arc::ptr_eq(s, self)) {
                servers.push(self.clone());
            }
        }
        spawn_server(self)?;
        ipc::wait_for(self.name, self.start_timeout).ok_or(EIO)
    }

    /// The server's service, restarting the server if it died. The first
    /// caller restarts it, concurrent callers wait for the registration.
    /// After MAX_RESTARTS restarts the server stays dead (EIO).
    pub fn revive(self: &Arc<Self>) -> Result<(usize, u64), i64> {
        if let Some(found) = ipc::lookup(self.name) {
            return Ok(found);
        }
        if self.restarting.swap(true, Ordering::Relaxed) {
            return ipc::wait_for(self.name, self.start_timeout).ok_or(EIO);
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
    pub fn start_timeout(mut self, ns: u64) -> Self {
        self.start_timeout = ns;
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
    let image = loader::load(&server.image, None, None, &args, &start_env())?;
    spawn_with(server.path, image, Some(server), &args)
}

/// The pager process of a Linux server instance: the server's view alone
/// (no program), one thread that supplies the pages of the instance's
/// paged objects. It belongs to the kernel, like the servers, and ends
/// when the instance's last program is gone.
fn spawn_pager(instance: &Arc<linux::Instance>) -> Result<Pid, i64> {
    let slot = sched::reserve_pid()?;
    let pid = slot.pid;
    let mut space = address_space::AddressSpace::new().ok_or(ENOMEM)?;
    space.attach(instance.clone(), false).map_err(|_| ENOMEM)?;
    let (thread, start) = linux::LinuxThread::pager(instance.clone())?;
    let mut own = Process::empty();
    own.mm = Some(address_space::Mm::new(space).ok_or(ENOMEM)?);
    own.linux = Some(thread);
    let mut info = Info::new(0, pid, pid, "linux-pager".to_string());
    info.exe = String::from("/sbin/linux");
    let group = ThreadGroup::new(pid, info, Default::default()).ok_or(ENOMEM)?;
    // Protected from signals of programs, as the servers are.
    group.privileged.store(true, Ordering::Relaxed);
    let t = new_task(pid, group, "linux-pager".to_string(), own, start)?;
    slot.insert(t.clone())?;
    sched::start(t);
    Ok(pid)
}

fn spawn_with(path: &str, image: loader::Image, server: Option<&Arc<Server>>, args: &[String]) -> Result<Pid, i64> {
    let console = OpenFile::console();
    let slot = sched::reserve_pid()?;
    let pid = slot.pid;
    // Servers belong to the kernel, even when a program's request
    // (re)started them, so no program can wait for or signal them.
    let parent = if server.is_some() { 0 } else { current_pid() };
    let name = basename(path).to_string();
    let mut info = Info::new(parent, pid, pid, name.clone());
    info.cmdline = cmdline_of(args);
    info.exe = path.to_string();
    info.mem = Some(image.space.stats.clone());
    let mut own = Process::empty();
    let mut space = image.space;
    let mut frame = Frame::user_start(image.entry, image.sp);
    if server.is_none() {
        // A new process tree: a new instance of the Linux server, whose
        // first thread starts in the server (ADR 0002).
        let instance = linux::Instance::new()?;
        spawn_pager(&instance)?;
        if let Err(e) = space.attach(instance.clone(), true) {
            instance.close();
            return Err(if e == address_space::Fault::Oom { ENOMEM } else { EINVAL });
        }
        let (thread, start) = linux::LinuxThread::new(instance, &frame)?;
        own.linux = Some(thread);
        frame = start;
    }
    own.mm = Some(address_space::Mm::new(space).ok_or(ENOMEM)?);
    own.files = Some(Files::new(vec![Some(FdEntry { file: console, cloexec: false }); 3]).ok_or(ENOMEM)?);
    own.fs = Some(FsInfo::new("/".to_string()).ok_or(ENOMEM)?);
    own.server = server.cloned();
    let group = ThreadGroup::new(pid, info, Default::default()).ok_or(ENOMEM)?;
    group.privileged.store(server.is_some(), Ordering::Relaxed);
    let t = new_task(pid, group, name, own, frame)?;
    slot.insert(t.clone())?;
    if server.is_none() {
        crate::drivers::tty::set_foreground(pid);
    }
    sched::start(t);
    Ok(pid)
}
