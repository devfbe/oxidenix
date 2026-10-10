//! Processes and threads: the process table, the current task, the servers the kernel starts
//! and the process trees it starts with their Linux server instance; the submodules hold
//! scheduling, address spaces, exit and kill, futexes, IPC and channels, restricted mode (the
//! Linux server's interface) and the native servers' system calls. The kernel implements no
//! Linux system call (R9).

pub mod address_space;
pub mod channel;
mod clock;
pub mod elf;
pub mod errno;
mod exec;
mod exit;
mod futex;
pub mod ipc;
pub mod irq;
pub mod kill;
mod loader;
mod native;
pub mod query;
pub mod sched;
pub mod syscall;
pub mod task;
pub mod tlb;
pub mod linux;
pub mod uaccess;
mod vm;

use crate::memory;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::Ordering;
use errno::*;
use sched::{current, TABLE};
use syscall::Frame;
use task::{Info, KernelStack, State, Task, ThreadGroup};
use x86_64::instructions::interrupts;

pub use exit::{decode_status, exit_group, exit_thread, reap_orphans, wait_for, WaitStatus};
pub use sched::{prepare_to_sleep, schedule, wakeup};
pub use task::Process;

pub type Pid = u64;

pub const TIMER_HZ: u64 = 100;

impl Process {
    /// The address space, or EINVAL for a task without one.
    pub fn mm(&self) -> Result<Arc<address_space::Mm>, i64> {
        self.mm.clone().ok_or(EINVAL)
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

/// The calling process's id (the kernel's: its thread group's).
pub fn current_pid() -> Pid {
    current().tgid()
}

/// The process with the kernel's id `pid`.
pub fn group(pid: Pid) -> Option<Arc<ThreadGroup>> {
    TABLE.lock().groups.get(&pid).cloned()
}

/// Process 0, the kernel monitor, runs on the boot stack of the bootstrap
/// CPU; each CPU also has an idle task.
pub fn init() {
    enable_sse();
    tlb::init_cpu(true);
    syscall::init();
    let group = ThreadGroup::new(0, Info::new("kernel".to_string())).expect("boot");
    let kernel = Arc::new(Task::new(0, group.clone(), "kernel".to_string(), Process::empty(), None, 0));
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

/// (pid, name, state, server, cpu, threads) of every process, for the
/// monitor.
pub fn list() -> Vec<(Pid, String, &'static str, bool, usize, usize)> {
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
                Some(State::Dead) => "exiting",
            };
            let cpu = main.map_or(0, |t| t.last_cpu.load(Ordering::Relaxed));
            (g.tgid, info.name.clone(), state, g.privileged.load(Ordering::Relaxed), cpu, info.threads.len())
        })
        .collect()
}

/// Bit mask of the CPUs that run.
pub fn online_mask() -> u64 {
    (0..crate::smp::MAX_CPUS).filter(|&i| crate::smp::by_index(i).is_some()).fold(0, |m, i| m | 1 << i)
}

/// Sleeps until `deadline` (nanoseconds since boot); a kick (or the end
/// of the process) ends the sleep early with EINTR.
pub fn sleep_until(deadline: u64) -> Result<(), i64> {
    loop {
        let wait = prepare_to_sleep();
        if crate::time::now() >= deadline {
            return Ok(());
        }
        if kill::interrupted() {
            return Err(EINTR);
        }
        wait.sleep_until(deadline);
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

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A user-space server the kernel starts and restarts.
pub struct Server {
    /// Name of the IPC service it registers.
    pub name: &'static str,
    pub path: &'static str,
    /// The program: `path` of the boot image (read-only, so a restart runs
    /// the same program), committed at boot.
    image: Arc<crate::fs::cache::PageCache>,
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
    /// Its incarnations: the state of the restart policy.
    lives: spin::Mutex<Lives>,
    restarting: core::sync::atomic::AtomicBool,
    /// Where its devices reach the pages granted to it (channels).
    pub domain: channel::DmaDomain,
    /// A doorbell it watches rang (`channel::watch`): its `ipc_receive`
    /// reports it. (A restarted process may see its predecessor's: a
    /// spurious wakeup, nothing more.)
    pub doorbell: Arc<core::sync::atomic::AtomicBool>,
    /// Channels bound to it: attached by one of its processes, with their
    /// client still there and not detached (`channel`, "Successors").
    pub bound: core::sync::atomic::AtomicU64,
}

/// The servers the kernel started, by name: a channel to a dead one
/// starts it again (`server_named`).
static SERVERS: spin::Mutex<Vec<Arc<Server>>> = spin::Mutex::new(Vec::new());

/// The server the kernel started under the IPC name `name`.
pub fn server_named(name: &str) -> Option<Arc<Server>> {
    SERVERS.lock().iter().find(|s| s.name == name).cloned()
}

// The restart policy (ADR 0006). A dead server is restarted at its next
// use; a crash loop (a server that cannot start, or dies whenever it is
// used) must not cost a start, and its clients a wait, at every use for
// ever, nor may one program that crashes a server a few times take the
// service away for the rest of the boot.

/// A life (registration to exit) this long shows the server works: its
/// death is not part of a crash loop, and the next restart is immediate.
/// (A server dying at every use lives milliseconds.)
const STABLE_LIFE: u64 = crate::time::NSEC_PER_SEC / 2;
/// The delay before restarting a server after its k-th young death in a
/// row (measured from the death): BACKOFF_BASE << (k - 1).
const BACKOFF_BASE: u64 = 100 * 1_000_000;
/// Young deaths in a row the kernel restarts after (backing off); one more
/// is a crash loop.
const MAX_YOUNG: u32 = 5;
/// Restart intensity, deaths young or not: more than RESTART_INTENSITY
/// restarts within RESTART_PERIOD is a crash loop too (a server killed
/// over and over after short use).
const RESTART_INTENSITY: usize = 20;
const RESTART_PERIOD: u64 = 60 * crate::time::NSEC_PER_SEC;
/// After a crash loop the service stays down (EIO at once) for
/// COOLDOWN_BASE << (crash loops in a row - 1), at most COOLDOWN_MAX; then
/// the next use tries again with a clean history. A life of
/// RESTART_PERIOD ends a row of crash loops.
const COOLDOWN_BASE: u64 = 5 * crate::time::NSEC_PER_SEC;
const COOLDOWN_MAX: u64 = 300 * crate::time::NSEC_PER_SEC;

/// A server's incarnations: the state of the restart policy.
#[derive(Default)]
struct Lives {
    /// The running incarnation (spawned, not exited yet).
    pid: Option<Pid>,
    /// When it registered its service (nanoseconds since boot).
    up_since: Option<u64>,
    /// When the last incarnation exited.
    died_at: u64,
    /// Young deaths in a row (lives shorter than STABLE_LIFE, or never
    /// registered).
    young: u32,
    /// Restarts within the last RESTART_PERIOD.
    restarts: alloc::collections::VecDeque<u64>,
    /// A crash loop: the service is down until then.
    down_until: Option<u64>,
    /// Crash loops in a row.
    loops: u32,
}

/// What `Server::revive` does with a server whose service is gone.
enum Next {
    /// Its incarnation has not exited yet (starting, or exiting): wait.
    Wait,
    /// Restart it at that time (nanoseconds since boot).
    RestartAt(u64),
    /// Down (a crash loop).
    Down,
}

impl Lives {
    /// The policy's decision at `now`.
    fn next(&mut self, name: &str, now: u64) -> Next {
        if self.pid.is_some() {
            return Next::Wait;
        }
        match self.down_until {
            Some(until) if now < until => return Next::Down,
            Some(_) => {
                crate::printkln!("[kernel] {} was down after a crash loop; trying again", name);
                self.down_until = None;
                self.young = 0;
                self.restarts.clear();
            }
            None => {}
        }
        while self.restarts.front().is_some_and(|&t| now.saturating_sub(t) >= RESTART_PERIOD) {
            self.restarts.pop_front();
        }
        if self.young > MAX_YOUNG || self.restarts.len() >= RESTART_INTENSITY {
            self.loops += 1;
            let cooldown = (COOLDOWN_BASE << (self.loops - 1).min(16)).min(COOLDOWN_MAX);
            self.down_until = Some(now + cooldown);
            crate::printkln!(
                "[kernel] {} is in a crash loop ({} young deaths in a row, {} restarts in {} s); down for {} s",
                name,
                self.young,
                self.restarts.len(),
                RESTART_PERIOD / crate::time::NSEC_PER_SEC,
                cooldown / crate::time::NSEC_PER_SEC
            );
            return Next::Down;
        }
        let delay = if self.young == 0 { 0 } else { BACKOFF_BASE << (self.young - 1) };
        Next::RestartAt(self.died_at + delay)
    }

    /// The incarnation `pid` exited at `now`.
    fn exited(&mut self, name: &str, pid: Pid, now: u64) {
        if self.pid != Some(pid) {
            return;
        }
        let life = self.up_since.map(|since| now.saturating_sub(since));
        let young = !life.is_some_and(|ns| ns >= STABLE_LIFE);
        self.young = if young { self.young + 1 } else { 0 };
        if life.is_some_and(|ns| ns >= RESTART_PERIOD) {
            self.loops = 0;
        }
        self.pid = None;
        self.up_since = None;
        self.died_at = now;
        match life {
            Some(ns) => crate::printkln!("[kernel] {} died after {} ms{}", name, ns / 1_000_000, if young { " (young)" } else { "" }),
            None => crate::printkln!("[kernel] {} died before it registered", name),
        }
    }
}

/// The server process `pid` exited (before its services are marked dead,
/// so whoever finds them dead finds its death recorded).
pub fn server_exited(pid: Pid) {
    let now = crate::time::now();
    for server in SERVERS.lock().iter() {
        server.lives.lock().exited(server.name, pid, now);
    }
}

impl Server {
    pub fn load(name: &'static str, path: &'static str) -> Result<Server, i64> {
        let image = crate::fs::cache::PageCache::program(crate::fs::program(path).ok_or(ENOENT)?)?;
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
            lives: spin::Mutex::new(Lives::default()),
            restarting: core::sync::atomic::AtomicBool::new(false),
            domain: channel::DmaDomain::default(),
            doorbell: Arc::new(core::sync::atomic::AtomicBool::new(false)),
            bound: core::sync::atomic::AtomicU64::new(0),
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

    /// Its running incarnation's process id.
    pub fn pid(&self) -> Option<Pid> {
        self.lives.lock().pid
    }

    /// Rings its doorbell: its process's `ipc_receive` returns (a spurious
    /// wakeup if nothing came: it looks and sleeps again).
    pub fn ring(&self) {
        self.doorbell.store(true, core::sync::atomic::Ordering::Release);
        if let Some(pid) = self.pid() {
            wakeup(irq::server_chan(pid));
        }
    }

    /// The incarnation `pid` registered its service (whenever: a start
    /// that timed out may still come up).
    pub fn registered(&self, pid: Pid) {
        let mut lives = self.lives.lock();
        if lives.pid == Some(pid) && lives.up_since.is_none() {
            lives.up_since = Some(crate::time::now());
        }
    }

    /// The server's service, restarting the server if it died, as the
    /// restart policy says (ADR 0006): at once after a life of
    /// STABLE_LIFE, else after a backoff; EIO while it is down after a
    /// crash loop. One caller restarts it, the others wait for it.
    pub fn revive(self: &Arc<Self>) -> Result<(usize, u64), i64> {
        // Waiting for an incarnation that has neither registered nor
        // exited is bounded: one that never does is hung.
        let deadline = crate::time::now() + self.start_timeout;
        loop {
            if let Some(found) = ipc::lookup(self.name) {
                return Ok(found);
            }
            if self.restarting.swap(true, Ordering::Acquire) {
                // Another caller restarts it: wait for its outcome, then
                // look again.
                while self.restarting.load(Ordering::Acquire) {
                    sleep_until(crate::time::now() + crate::timer::TICK_NS)?;
                }
                if let Some(found) = ipc::lookup(self.name) {
                    return Ok(found);
                }
                if self.lives.lock().down_until.is_some_and(|until| crate::time::now() < until) {
                    return Err(EIO);
                }
                continue;
            }
            let result = self.restart_dead(deadline);
            self.restarting.store(false, Ordering::Release);
            match result {
                Some(result) => return result,
                None => sleep_until(crate::time::now() + crate::timer::TICK_NS)?,
            }
        }
    }

    /// `revive` for the caller that may restart the server: the result, or
    /// None to wait for an incarnation still starting or exiting.
    fn restart_dead(self: &Arc<Self>, deadline: u64) -> Option<Result<(usize, u64), i64>> {
        // Another caller may have restarted it since this one looked.
        if let Some(found) = ipc::lookup(self.name) {
            return Some(Ok(found));
        }
        let now = crate::time::now();
        let at = match self.lives.lock().next(self.name, now) {
            Next::Wait if now < deadline => return None,
            Next::Wait | Next::Down => return Some(Err(EIO)),
            Next::RestartAt(at) => at,
        };
        if at > now {
            if let Err(e) = sleep_until(at) {
                return Some(Err(e));
            }
        }
        {
            let mut lives = self.lives.lock();
            lives.restarts.push_back(crate::time::now());
            crate::printkln!(
                "[kernel] restarting {} ({} young deaths in a row, {} restarts in {} s)",
                self.name,
                lives.young,
                lives.restarts.len(),
                RESTART_PERIOD / crate::time::NSEC_PER_SEC
            );
        }
        Some(self.start())
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

/// Starts a program as a child of the kernel shell: a new process tree,
/// with its own instance of the Linux server (ADR 0002), the console
/// granted to it (ADR 0007). Names without '/' are looked up in /bin. The
/// server runs the program (ADR 0010): the process starts with an empty
/// address space and its first thread in the server (`ROLE_INIT`), which
/// makes it pid 1 of the tree, gives it standard input, output and error
/// on the console and execs the program; if that fails, it exits with 127.
pub fn spawn(name: &str, args: &[&str]) -> Result<Pid, i64> {
    let path = if name.contains('/') { name.to_string() } else { alloc::format!("/bin/{name}") };
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let slot = sched::reserve_pid()?;
    let pid = slot.pid;
    let instance = linux::Instance::new()?;
    instance.set_init_args(&path, &args, &start_env())?;
    spawn_pager(&instance)?;
    let task = (|| {
        let mut space = address_space::AddressSpace::new().ok_or(ENOMEM)?;
        space.attach(instance.clone(), true).map_err(|e| if e == address_space::Fault::Oom { ENOMEM } else { EINVAL })?;
        let (thread, start) = linux::LinuxThread::new(instance.clone(), &Frame::user_start(0, 0), restricted::ROLE_INIT, 0)?;
        let name = basename(&path).to_string();
        let mut info = Info::new(name.clone());
        info.exe = path.clone();
        info.mem = Some(space.stats.clone());
        let mut own = Process::empty();
        own.mm = Some(address_space::Mm::new(space).ok_or(ENOMEM)?);
        own.linux = Some(thread);
        let group = ThreadGroup::new(pid, info).ok_or(ENOMEM)?;
        group.instance.store(instance.id, Ordering::Release);
        let t = new_task(pid, group, name, own, start)?;
        unsafe { t.own() }.linux.as_mut().expect("set above").announce(&t)?;
        Ok::<_, i64>(t)
    })();
    let t = match task.and_then(|t| slot.insert(t.clone()).map(|_| t)) {
        Ok(t) => t,
        Err(e) => {
            // No program will run: the pager's process ends.
            instance.close();
            return Err(e);
        }
    };
    // The tree the kernel starts gets the console (ADR 0007) and, the only
    // tree the machine runs at a time, the host grant (ADR 0011).
    linux::host_grant(&instance, true);
    linux::console_grant(Some(&instance));
    sched::start(t);
    Ok(pid)
}

/// Starts a privileged server process: it may register IPC services and
/// request its I/O ports, and it does not take over the terminal.
pub fn spawn_server(server: &Arc<Server>) -> Result<Pid, i64> {
    let mut args = alloc::vec![server.path.to_string()];
    args.extend(server.args.iter().cloned());
    let image = loader::load(&server.image, None, None, &args, &start_env())?;
    spawn_with(server.path, image, server, &args)
}

/// The pager process of a Linux server instance: the server's view alone
/// (no program), one thread that supplies the pages of the instance's
/// paged objects, and the instance's two other service threads (the worker
/// and the net thread). It belongs to the kernel, like the servers, and
/// ends when the instance's last program is gone.
fn spawn_pager(instance: &Arc<linux::Instance>) -> Result<Pid, i64> {
    let slot = sched::reserve_pid()?;
    let pid = slot.pid;
    let mut space = address_space::AddressSpace::new().ok_or(ENOMEM)?;
    space.attach(instance.clone(), false).map_err(|_| ENOMEM)?;
    let (thread, start) = linux::LinuxThread::pager(instance.clone())?;
    let mut own = Process::empty();
    own.mm = Some(address_space::Mm::new(space).ok_or(ENOMEM)?);
    own.linux = Some(thread);
    let mut info = Info::new("linux-pager".to_string());
    info.exe = String::from("/sbin/linux");
    let group = ThreadGroup::new(pid, info).ok_or(ENOMEM)?;
    // A process of the kernel's, as the servers are.
    group.privileged.store(true, Ordering::Relaxed);
    let mm = own.mm.clone();
    let t = new_task(pid, group.clone(), "linux-pager".to_string(), own, start)?;
    slot.insert(t.clone())?;
    sched::start(t);
    // The worker, the net thread and the timer thread: threads of the
    // pager's process (they end with it).
    for (role, name) in [(restricted::ROLE_WORKER, "linux-worker"), (restricted::ROLE_NET, "linux-net"), (restricted::ROLE_TIMER, "linux-timer")] {
        let wslot = sched::reserve_pid()?;
        let (thread, start) = linux::LinuxThread::service(instance.clone(), role)?;
        let mut own = Process::empty();
        own.mm = mm.clone();
        own.linux = Some(thread);
        let w = new_task(wslot.pid, group.clone(), name.to_string(), own, start)?;
        wslot.insert(w.clone())?;
        sched::start(w);
    }
    Ok(pid)
}

/// Starts the native server `server` from its loaded `image`.
fn spawn_with(path: &str, image: loader::Image, server: &Arc<Server>, args: &[String]) -> Result<Pid, i64> {
    let slot = sched::reserve_pid()?;
    let pid = slot.pid;
    // Servers belong to the kernel, even when a program's request
    // (re)started them: only the kernel waits for them or ends them.
    let name = basename(path).to_string();
    let mut info = Info::new(name.clone());
    info.cmdline = cmdline_of(args);
    info.exe = path.to_string();
    info.mem = Some(image.space.stats.clone());
    let mut own = Process::empty();
    let frame = Frame::user_start(image.entry, image.sp);
    own.mm = Some(address_space::Mm::new(image.space).ok_or(ENOMEM)?);
    own.server = Some(server.clone());
    let group = ThreadGroup::new(pid, info).ok_or(ENOMEM)?;
    group.privileged.store(true, Ordering::Relaxed);
    let t = new_task(pid, group, name, own, frame)?;
    slot.insert(t.clone())?;
    {
        // Before it runs, so its registration and exit find it.
        let mut lives = server.lives.lock();
        lives.pid = Some(pid);
        lives.up_since = None;
    }
    sched::start(t);
    Ok(pid)
}
