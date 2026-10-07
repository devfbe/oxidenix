use super::address_space::USER_END;
use super::errno::*;
use super::sys_file::{self, AT_FDCWD};
use super::{epoll, signal, sys_mem, sys_net, sys_time, uaccess};
use crate::interrupts::gdt;
use x86_64::registers::model_specific::{Efer, EferFlags, FsBase, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

/// Complete register state at a kernel entry. The tail (rip..ss) is exactly
/// what the CPU pushes on an interrupt from ring 3; `vector` and `error`
/// identify the interrupt or exception (syscalls use `SYSCALL_VECTOR`). All
/// entries share this layout and return through `user_return` (iretq).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Frame {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl Frame {
    pub fn user_start(entry: u64, sp: u64) -> Self {
        Frame {
            rip: entry,
            cs: gdt::USER_CS as u64,
            rflags: 0x202,
            rsp: sp,
            ss: gdt::USER_SS as u64,
            ..Default::default()
        }
    }

    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

/// Marks frames built by `syscall_entry` (not a real interrupt vector).
pub const SYSCALL_VECTOR: u64 = 0x100;

/// Syscall MSRs of the calling CPU.
pub fn init() {
    Star::write(gdt::user_code(), gdt::user_data(), gdt::kernel_code(), gdt::kernel_data())
        .expect("GDT layout does not match syscall/sysret");
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
    // Clear IF on entry; `syscall_entry` enables interrupts once it is on
    // the kernel stack.
    SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG | RFlags::TRAP_FLAG);
    unsafe { Efer::update(|f| *f |= EferFlags::SYSTEM_CALL_EXTENSIONS) };
}

/// Builds a `Frame` on the kernel stack (iret part first, as an interrupt
/// would) and dispatches the syscall. Interrupts are off on entry (SFMASK),
/// so nothing runs between `swapgs` and the switch to the kernel stack;
/// once the frame is saved they are enabled for the syscall itself. The
/// kernel stays non-preemptive (an interrupt in kernel mode never
/// schedules), but long syscalls no longer delay timer ticks and device
/// interrupts on their CPU.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        "swapgs",
        "mov gs:[{user_rsp}], rsp",
        "mov rsp, gs:[{kernel_stack}]",
        "push {ss}",
        "push qword ptr gs:[{user_rsp}]",
        "push r11",
        "push {cs}",
        "push rcx",
        "push 0",
        "push {vector}",
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rbp",
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",
        "sti",
        "mov rdi, rsp",
        "call {dispatch}",
        "jmp {ret}",
        ss = const gdt::USER_SS,
        cs = const gdt::USER_CS,
        user_rsp = const crate::smp::USER_RSP_OFFSET,
        kernel_stack = const crate::smp::KERNEL_STACK_OFFSET,
        vector = const SYSCALL_VECTOR,
        dispatch = sym dispatch,
        ret = sym user_return,
    );
}

/// Restores the `Frame` at rsp and returns with iretq. Used after syscalls,
/// after interrupts and exceptions, and as the first entry of new
/// processes. Returning to ring 3 swaps the user's GS base back in.
#[unsafe(naked)]
pub unsafe extern "C" fn user_return() {
    core::arch::naked_asm!(
        "cli",
        "test byte ptr [rsp + {cs}], 3",
        "jz 2f",
        "swapgs",
        "2:",
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rbp",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "add rsp, 16",
        "iretq",
        cs = const crate::interrupts::entry::FRAME_CS_OFFSET,
    );
}

extern "sysv64" fn dispatch(f: &mut Frame) {
    crate::counters::add(|c| &c.syscalls, 1);
    let nr = f.rax;
    let (a0, a1, a2, a3, a4, a5) = (f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9);
    let cwd = AT_FDCWD as u64;
    let result: SysResult = match f.rax {
        0 => sys_file::read(a0, a1, a2),
        1 => sys_file::write(a0, a1, a2),
        2 => sys_file::openat(cwd, a0, a1, a2),
        3 => sys_file::close(a0),
        4 => sys_file::newfstatat(cwd, a0, a1, 0),
        5 => sys_file::fstat(a0, a1),
        6 => sys_file::newfstatat(cwd, a0, a1, 0x100),
        7 => sys_file::poll(a0, a1, sys_file::poll_timeout(a2 as i32 as i64)),
        8 => sys_file::lseek(a0, a1 as i64, a2),
        9 => sys_mem::mmap(a0, a1, a2, a3, a4, a5),
        10 => sys_mem::mprotect(a0, a1, a2),
        11 => sys_mem::munmap(a0, a1),
        12 => sys_mem::brk(a0),
        25 => sys_mem::mremap(a0, a1, a2, a3, a4),
        28 => sys_mem::madvise(a0, a1, a2),
        26 => sys_mem::msync(a0, a1, a2),
        // mlock*: nothing is swapped.
        149 | 150 | 151 | 152 => Ok(0),
        13 => signal::sigaction(a0, a1, a2),
        14 => signal::sigprocmask(a0, a1, a2),
        127 => signal::sigpending(a0, a1),
        130 => signal::sigsuspend(a0, a1),
        15 => signal::sigreturn(f),
        16 => sys_file::ioctl(a0, a1, a2),
        17 => sys_file::pread(a0, a1, a2, a3 as i64),
        18 => sys_file::pwrite(a0, a1, a2, a3 as i64),
        19 => sys_file::readv(a0, a1, a2),
        20 => sys_file::writev(a0, a1, a2),
        21 => sys_file::faccessat(cwd, a0),
        22 => sys_file::pipe2(a0, 0),
        23 => sys_file::timeout(a4, 1_000_000).and_then(|t| sys_file::select(a0, a1, a2, a3, t)),
        24 => {
            super::yield_now();
            Ok(0)
        }
        32 => sys_file::dup(a0),
        33 => sys_file::dup3(a0, a1, 0, true),
        34 => super::pause(),
        35 => sys_time::clock_nanosleep(1, 0, a0, a1),
        230 => sys_time::clock_nanosleep(a0, a1, a2, a3),
        39 => Ok(super::current_pid() as i64),
        186 => Ok(super::current_tid() as i64),
        218 => super::set_tid_address(a0).map(|tid| tid as i64),
        40 => sys_file::sendfile(a0, a1, a2, a3),
        41 => sys_net::socket(a0, a1, a2),
        42 => sys_net::connect(a0, a1, a2),
        43 => sys_net::accept(a0, a1, a2, 0),
        44 => sys_net::sendto(a0, a1, a2, a3, a4, a5),
        45 => sys_net::recvfrom(a0, a1, a2, a3, a4, a5),
        46 => sys_net::sendmsg(a0, a1, a2),
        47 => sys_net::recvmsg(a0, a1, a2),
        48 => sys_net::shutdown(a0, a1),
        49 => sys_net::bind(a0, a1, a2),
        50 => sys_net::listen(a0, a1),
        51 => sys_net::getsockname(a0, a1, a2, false),
        52 => sys_net::getsockname(a0, a1, a2, true),
        53 => Err(EOPNOTSUPP), // socketpair: no AF_UNIX
        54 => sys_net::setsockopt(a0),
        55 => sys_net::getsockopt(a0, a1, a2, a3, a4),
        288 => sys_net::accept(a0, a1, a2, a3),
        99 => super::query::sysinfo(a0),
        125 => super::prctl::capget(a0, a1),
        128 => signal::sigtimedwait(a0, a1, a2, a3),
        126 => super::prctl::capset(a0, a1),
        157 => super::prctl::prctl(a0, a1),
        203 => super::sched_setaffinity(a0, a1, a2),
        204 => super::sched_getaffinity(a0, a1, a2),
        309 => super::getcpu(a0, a1),
        36 => getitimer(a0, a1),
        37 => {
            let (old, _) = super::set_alarm((a0 as u32 as u64).saturating_mul(1_000_000), 0);
            Ok(old.div_ceil(1_000_000) as i64)
        }
        38 => setitimer(a0, a1, a2),
        56 => super::clone(f, a0, a1, a2, a3, a4).map(|tid| tid as i64),
        57 => super::clone::fork(f).map(|pid| pid as i64),
        58 => super::clone::vfork(f).map(|pid| pid as i64),
        59 => execve(f, a0, a1, a2),
        60 => super::exit_thread(((a0 & 0xff) << 8) as i32),
        231 => super::exit_group(((a0 & 0xff) << 8) as i32),
        61 => super::wait4(a0 as i64, a1, a2),
        62 => signal::kill(a0 as i64, a1),
        63 => uname(a0),
        72 => sys_file::fcntl(a0, a1, a2),
        74 | 75 => sys_file::fsync(a0),
        162 => sys_file::sync(),
        76 => sys_file::truncate(a0, a1),
        77 => sys_file::ftruncate(a0, a1),
        79 => sys_file::getcwd(a0, a1),
        80 => sys_file::chdir(a0),
        81 => sys_file::fchdir(a0),
        82 => sys_file::renameat(cwd, a0, cwd, a1),
        83 => sys_file::mkdirat(cwd, a0, a1),
        84 => sys_file::unlinkat(cwd, a0, 0x200),
        87 => sys_file::unlinkat(cwd, a0, 0),
        88 => sys_file::symlinkat(a0, cwd, a1),
        89 => sys_file::readlinkat(cwd, a0, a1, a2),
        90 => sys_file::fchmodat(cwd, a0, a1),
        95 => Ok(0o022), // umask
        98 => sys_time::getrusage(a0, a1),
        102 | 104 | 107 | 108 => Ok(0), // getuid/getgid/geteuid/getegid: everything is root
        105 | 106 => Ok(0), // setuid/setgid
        109 => super::setpgid(a0, a1),
        112 => super::setsid(),
        110 => Ok(super::current_ppid() as i64),
        118 | 120 => getres_ids(a0, a1, a2), // getresuid/getresgid
        111 => super::getpgid(0),
        121 => super::getpgid(a0),
        124 => super::getsid(a0),
        131 => Ok(0), // sigaltstack: handlers always run on the normal stack
        137 => sys_file::statfs(a0, a1),
        138 => sys_file::fstatfs(a0, a1),
        158 => arch_prctl(a0, a1),
        169 => reboot(a0, a1, a2),
        173 => super::ioperm(a0, a1, a2),
        1000 => super::ipc::register(a0, a1, a2),
        1001 => super::ipc::receive(a0, a1, a2, a3 as i64),
        1002 => super::ipc::reply(a0, a1, a2),
        1003 => super::irq::enable(a0),
        1004 => super::dma_map(a0),
        1005 => super::query::proc_query(a0, a1, a2, a3),
        1006 => super::ipc::notify(a0),
        200 => signal::tgkill(None, a0 as i64, a1),
        234 => signal::tgkill(Some(a0 as i64), a1 as i64, a2),
        202 => super::futex::futex(a0, a1, a2, a3, a4, a5),
        217 => sys_file::getdents64(a0, a1, a2),
        228 => sys_time::clock_gettime(a0, a1),
        227 => sys_time::clock_settime(a0, a1),
        229 => sys_time::clock_getres(a0, a1),
        96 => sys_time::gettimeofday(a0, a1),
        164 => sys_time::settimeofday(a0),
        201 => sys_time::time(a0),
        100 => sys_time::times(a0),
        257 => sys_file::openat(a0, a1, a2, a3),
        258 => sys_file::mkdirat(a0, a1, a2),
        262 => sys_file::newfstatat(a0, a1, a2, a3),
        263 => sys_file::unlinkat(a0, a1, a2),
        264 | 316 => sys_file::renameat(a0, a1, a2, a3),
        266 => sys_file::symlinkat(a0, a1, a2),
        267 => sys_file::readlinkat(a0, a1, a2, a3),
        268 => sys_file::fchmodat(a0, a1, a2),
        270 => sys_file::pselect6(a0, a1, a2, a3, a4, a5),
        271 => sys_file::ppoll(a0, a1, a2, a3, a4),
        269 | 439 => sys_file::faccessat(a0, a1),
        235 => sys_file::utimensat(cwd, a0, 0),
        261 => sys_file::utimensat(a0, a1, 0),
        280 => sys_file::utimensat(a0, a1, a3),
        292 => sys_file::dup3(a0, a1, a2, false),
        293 => sys_file::pipe2(a0, a1),
        284 => sys_file::eventfd2(a0, 0),
        213 => epoll::epoll_create(a0),
        291 => epoll::epoll_create1(a0),
        233 => epoll::epoll_ctl(a0, a1, a2, a3),
        232 => epoll::epoll_pwait(a0, a1, a2, a3, 0, 0),
        281 => epoll::epoll_pwait(a0, a1, a2, a3, a4, a5),
        441 => epoll::epoll_pwait2(a0, a1, a2, a3, a4, a5),
        290 => sys_file::eventfd2(a0, a1),
        302 => prlimit(a3),
        318 => getrandom(a0, a1),
        nr => {
            crate::printkln!("[kernel] syscall {} not implemented", nr);
            Err(ENOSYS)
        }
    };
    f.rax = result.unwrap_or_else(|e| -e) as u64;
    super::sched::resched_on_return();
    super::signal::deliver(f, Some(nr));
}

/// reboot(2): power off (QEMU exits) or restart the machine.
fn reboot(magic: u64, magic2: u64, cmd: u64) -> SysResult {
    const MAGIC: u64 = 0xfee1_dead;
    const MAGIC2: [u64; 4] = [0x2812_1969, 0x0512_1996, 0x1604_1998, 0x2011_2000];
    const RESTART: u64 = 0x0123_4567;
    const HALT: u64 = 0xcdef_0123;
    const POWER_OFF: u64 = 0x4321_fedc;
    if magic != MAGIC || !MAGIC2.contains(&magic2) {
        return Err(EINVAL);
    }
    match cmd {
        POWER_OFF | HALT => crate::power_off(0),
        RESTART => crate::restart(),
        _ => Err(EINVAL),
    }
}

/// Everything runs as root: real, effective and saved IDs are all 0.
fn getres_ids(r: u64, e: u64, s: u64) -> SysResult {
    for ptr in [r, e, s] {
        uaccess::write(ptr, 0u32)?;
    }
    Ok(0)
}

fn execve(f: &mut Frame, path: u64, argv: u64, envp: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let args = uaccess::read_cstr_array(argv)?;
    let envs = uaccess::read_cstr_array(envp)?;
    super::exec(f, &path, &args, &envs)?;
    Ok(0)
}

fn arch_prctl(code: u64, addr: u64) -> SysResult {
    const ARCH_SET_FS: u64 = 0x1002;
    match code {
        ARCH_SET_FS if addr < USER_END => {
            FsBase::write(VirtAddr::new(addr));
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

fn uname(buf: u64) -> SysResult {
    let mut uts = [0u8; 6 * 65];
    // oxidenix speaks the Linux ABI but says what it is.
    let fields = ["oxidenix", "oxidenix", env!("CARGO_PKG_VERSION"), "#1 SMP", "x86_64", "(none)"];
    for (i, field) in fields.iter().enumerate() {
        uts[i * 65..i * 65 + field.len()].copy_from_slice(field.as_bytes());
    }
    uaccess::write(buf, uts)?;
    Ok(0)
}

const ITIMER_REAL: u64 = 0;

/// struct itimerval: (interval, value) as two timevals, in microseconds.
fn read_itimerval(addr: u64) -> Result<(u64, u64), i64> {
    let [isec, iusec, vsec, vusec]: [u64; 4] = uaccess::read(addr)?;
    if iusec >= 1_000_000 || vusec >= 1_000_000 || isec > i64::MAX as u64 || vsec > i64::MAX as u64 {
        return Err(EINVAL);
    }
    let us = |sec: u64, usec: u64| sec.saturating_mul(1_000_000).saturating_add(usec);
    Ok((us(isec, iusec), us(vsec, vusec)))
}

fn write_itimerval(addr: u64, (value, interval): (u64, u64)) -> Result<(), i64> {
    if addr == 0 {
        return Ok(());
    }
    uaccess::write(addr, [interval / 1_000_000, interval % 1_000_000, value / 1_000_000, value % 1_000_000])
}

fn setitimer(which: u64, new: u64, old: u64) -> SysResult {
    if which != ITIMER_REAL {
        return Err(EINVAL);
    }
    let (interval, value) = if new == 0 { (0, 0) } else { read_itimerval(new)? };
    write_itimerval(old, super::set_alarm(value, interval))?;
    Ok(0)
}

fn getitimer(which: u64, cur: u64) -> SysResult {
    if which != ITIMER_REAL {
        return Err(EINVAL);
    }
    write_itimerval(cur, super::get_alarm())?;
    Ok(0)
}

fn prlimit(old: u64) -> SysResult {
    if old != 0 {
        uaccess::write(old, [u64::MAX, u64::MAX])?;
    }
    Ok(0)
}

/// Not cryptographically secure: xorshift seeded from the timestamp counter.
fn getrandom(buf: u64, len: u64) -> SysResult {
    let mut x = unsafe { core::arch::x86_64::_rdtsc() } | 1;
    let n = uaccess::read_to_user(buf, len, true, |out, _| {
        for b in out.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        Ok(out.len())
    })?;
    Ok(n as i64)
}
