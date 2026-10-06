use super::address_space::USER_END;
use super::errno::*;
use super::sys_file::{self, AT_FDCWD};
use super::{sys_mem, uaccess};
use crate::interrupts::gdt;
use x86_64::registers::model_specific::{Efer, EferFlags, FsBase, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

#[unsafe(no_mangle)]
static mut SYSCALL_STACK_TOP: u64 = 0;
#[unsafe(no_mangle)]
static mut SYSCALL_USER_RSP: u64 = 0;

/// Vollstaendiges User-Registerabbild, in der Reihenfolge, in der
/// `syscall_entry` pusht (niedrigste Adresse zuerst).
#[repr(C)]
#[derive(Clone, Default)]
pub struct Frame {
    pub rax: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub r10: u64,
    pub r8: u64,
    pub r9: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

impl Frame {
    pub fn user_start(entry: u64, sp: u64) -> Self {
        Frame {
            rip: entry,
            rsp: sp,
            rflags: 0x202,
            ..Default::default()
        }
    }
}

pub fn init() {
    let s = gdt::selectors();
    Star::write(s.user_code, s.user_data, s.kernel_code, s.kernel_data)
        .expect("GDT-Layout passt nicht zu sysret");
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
    // IF loeschen: der Kernel ist nicht praeemptiv, Syscalls laufen ohne IRQs.
    SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG | RFlags::TRAP_FLAG);
    unsafe { Efer::update(|f| *f |= EferFlags::SYSTEM_CALL_EXTENSIONS) };
}

pub fn set_kernel_stack(top: u64) {
    unsafe { SYSCALL_STACK_TOP = top };
}

#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        "mov [rip + SYSCALL_USER_RSP], rsp",
        "mov rsp, [rip + SYSCALL_STACK_TOP]",
        "push qword ptr [rip + SYSCALL_USER_RSP]",
        "push r11",
        "push rcx",
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push rbp",
        "push rbx",
        "push r9",
        "push r8",
        "push r10",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rax",
        "mov rdi, rsp",
        "call {dispatch}",
        "mov [rsp], rax",
        "jmp {ret}",
        dispatch = sym dispatch,
        ret = sym syscall_return,
    );
}

/// Stellt das `Frame` ab rsp wieder her und kehrt per sysret in den Ring 3
/// zurueck. Neue Prozesse starten ueber diesen Pfad.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_return() {
    core::arch::naked_asm!(
        "pop rax",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop r10",
        "pop r8",
        "pop r9",
        "pop rbx",
        "pop rbp",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "pop rcx",
        "pop r11",
        "pop rsp",
        "sysretq",
    );
}

extern "sysv64" fn dispatch(f: &mut Frame) -> i64 {
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
        8 => sys_file::lseek(a0, a1 as i64, a2),
        9 => sys_mem::mmap(a0, a1, a2, a3, a4, a5),
        10 => Ok(0), // mprotect: Seiten bleiben, wie sie gemappt wurden
        11 => sys_mem::munmap(a0, a1),
        12 => sys_mem::brk(a0),
        13 => sigaction(a2, a3),
        14 => sigprocmask(a2, a3),
        16 => sys_file::ioctl(a0, a1, a2),
        19 => sys_file::readv(a0, a1, a2),
        20 => sys_file::writev(a0, a1, a2),
        21 => sys_file::faccessat(cwd, a0),
        22 => sys_file::pipe2(a0, 0),
        24 => {
            super::yield_now();
            Ok(0)
        }
        32 => sys_file::dup(a0),
        33 => sys_file::dup3(a0, a1, 0, true),
        35 => nanosleep(a0),
        39 | 186 | 218 => Ok(super::current_pid() as i64),
        40 => sys_file::sendfile(a0, a1, a2, a3),
        57 => super::fork(f).map(|pid| pid as i64),
        59 => execve(f, a0, a1, a2),
        60 | 231 => super::exit(((a0 & 0xff) << 8) as i32),
        61 => super::wait4(a0 as i64, a1, a2),
        63 => uname(a0),
        72 => sys_file::fcntl(a0, a1, a2),
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
        102 | 104 | 107 | 108 => Ok(0), // getuid/getgid/geteuid/getegid: alles root
        105 | 106 | 109 | 112 => Ok(0), // setuid/setgid/setpgid/setsid
        110 => Ok(super::current_ppid() as i64),
        111 | 121 | 124 => Ok(super::current_pid() as i64), // getpgrp/getpgid/getsid
        158 => arch_prctl(a0, a1),
        217 => sys_file::getdents64(a0, a1, a2),
        228 => clock_gettime(a1),
        257 => sys_file::openat(a0, a1, a2, a3),
        258 => sys_file::mkdirat(a0, a1, a2),
        262 => sys_file::newfstatat(a0, a1, a2, a3),
        263 => sys_file::unlinkat(a0, a1, a2),
        264 | 316 => sys_file::renameat(a0, a1, a2, a3),
        266 => sys_file::symlinkat(a0, a1, a2),
        267 => sys_file::readlinkat(a0, a1, a2, a3),
        268 => sys_file::fchmodat(a0, a1, a2),
        269 | 439 => sys_file::faccessat(a0, a1),
        235 => sys_file::utimensat(cwd, a0, 0),
        261 => sys_file::utimensat(a0, a1, 0),
        280 => sys_file::utimensat(a0, a1, a3),
        292 => sys_file::dup3(a0, a1, a2, false),
        293 => sys_file::pipe2(a0, a1),
        302 => prlimit(a3),
        318 => getrandom(a0, a1),
        nr => {
            crate::printkln!("[kernel] syscall {} nicht implementiert", nr);
            Err(ENOSYS)
        }
    };
    result.unwrap_or_else(|e| -e)
}

fn execve(f: &mut Frame, path: u64, argv: u64, envp: u64) -> SysResult {
    let path = uaccess::read_cstr(path)?;
    let args = uaccess::read_cstr_array(argv)?;
    let envs = uaccess::read_cstr_array(envp)?;
    super::exec(f, &path, &args, &envs)?;
    Ok(0)
}

/// Signale gibt es noch nicht: Handler werden angenommen, aber nie ausgeloest.
fn sigaction(oldact: u64, size: u64) -> SysResult {
    if oldact != 0 {
        uaccess::slice_mut(oldact, 24 + size)?.fill(0);
    }
    Ok(0)
}

/// Signale gibt es noch nicht; die alte Maske ist immer leer.
fn sigprocmask(oldset: u64, size: u64) -> SysResult {
    if oldset != 0 {
        uaccess::slice_mut(oldset, size)?.fill(0);
    }
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
    for (i, field) in ["Linux", "rust-kernel", "6.0.0-rust", "#1", "x86_64", "(none)"].iter().enumerate() {
        uts[i * 65..i * 65 + field.len()].copy_from_slice(field.as_bytes());
    }
    uaccess::write(buf, uts)?;
    Ok(0)
}

fn clock_gettime(ts: u64) -> SysResult {
    let ticks = super::ticks();
    let hz = super::TIMER_HZ;
    uaccess::write(ts, [ticks / hz, (ticks % hz) * (1_000_000_000 / hz)])?;
    Ok(0)
}

fn nanosleep(req: u64) -> SysResult {
    let [sec, nsec]: [u64; 2] = uaccess::read(req)?;
    let tick_ns = 1_000_000_000 / super::TIMER_HZ;
    super::sleep_ticks(sec * super::TIMER_HZ + nsec.div_ceil(tick_ns));
    Ok(0)
}

fn prlimit(old: u64) -> SysResult {
    if old != 0 {
        uaccess::write(old, [u64::MAX, u64::MAX])?;
    }
    Ok(0)
}

/// Kein kryptografischer Zufall: xorshift ueber dem Zeitstempelzaehler.
fn getrandom(buf: u64, len: u64) -> SysResult {
    let out = uaccess::slice_mut(buf, len)?;
    let mut x = unsafe { core::arch::x86_64::_rdtsc() } | 1;
    for b in out.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    Ok(len as i64)
}
