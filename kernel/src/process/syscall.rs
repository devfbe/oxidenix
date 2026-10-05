use super::address_space::{user_range_ok, USER_END};
use crate::interrupts::gdt;
use alloc::string::String;
use alloc::vec::Vec;
use x86_64::registers::model_specific::{Efer, EferFlags, FsBase, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

pub const ENOENT: i64 = 2;
pub const E2BIG: i64 = 7;
pub const EBADF: i64 = 9;
pub const ECHILD: i64 = 10;
pub const ENOMEM: i64 = 12;
pub const EFAULT: i64 = 14;
pub const EINVAL: i64 = 22;
pub const ENOTTY: i64 = 25;
pub const ENOSYS: i64 = 38;

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
    let (a0, a1, a2) = (f.rdi, f.rsi, f.rdx);
    match f.rax {
        1 => sys_write(a0, a1, a2),
        14 => sys_rt_sigprocmask(a2, f.r10),
        16 => sys_ioctl(a0, a1, a2),
        20 => sys_writev(a0, a1, a2),
        24 => {
            super::yield_now();
            0
        }
        39 | 186 => super::current_pid() as i64,
        57 => super::fork(f).map_or_else(|e| -e, |pid| pid as i64),
        59 => sys_execve(f, a0, a1),
        60 | 231 => super::exit(((a0 & 0xff) << 8) as i32),
        61 => super::wait4(a0 as i64, a1, a2),
        110 => super::current_ppid() as i64,
        158 => sys_arch_prctl(a0, a1),
        218 => super::current_pid() as i64,
        nr => {
            crate::printkln!("[kernel] syscall {} nicht implementiert", nr);
            -ENOSYS
        }
    }
}

fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -EBADF;
    }
    if !user_range_ok(buf, len, false) {
        return -EFAULT;
    }
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
    crate::drivers::console::write_bytes(bytes);
    len as i64
}

fn sys_writev(fd: u64, iov: u64, count: u64) -> i64 {
    if count > 1024 || !user_range_ok(iov, count * 16, false) {
        return -EFAULT;
    }
    let mut total = 0;
    for i in 0..count {
        let entry = iov + i * 16;
        let (base, len) = unsafe { (*(entry as *const u64), *((entry + 8) as *const u64)) };
        let n = sys_write(fd, base, len);
        if n < 0 {
            return n;
        }
        total += n;
    }
    total
}

fn sys_ioctl(fd: u64, request: u64, arg: u64) -> i64 {
    const TIOCGWINSZ: u64 = 0x5413;
    if fd > 2 {
        return -EBADF;
    }
    if request != TIOCGWINSZ {
        return -ENOTTY;
    }
    if !user_range_ok(arg, 8, true) {
        return -EFAULT;
    }
    let (cols, rows) = crate::drivers::console::size();
    let ws = [rows as u16, cols as u16, 0, 0];
    unsafe { (arg as *mut [u16; 4]).write_unaligned(ws) };
    0
}

/// Signale gibt es noch nicht; die alte Maske ist immer leer.
fn sys_rt_sigprocmask(oldset: u64, size: u64) -> i64 {
    if oldset != 0 {
        if !user_range_ok(oldset, size, true) {
            return -EFAULT;
        }
        unsafe { core::ptr::write_bytes(oldset as *mut u8, 0, size as usize) };
    }
    0
}

fn sys_arch_prctl(code: u64, addr: u64) -> i64 {
    const ARCH_SET_FS: u64 = 0x1002;
    match code {
        ARCH_SET_FS if addr < USER_END => {
            FsBase::write(VirtAddr::new(addr));
            0
        }
        _ => -EINVAL,
    }
}

fn sys_execve(f: &mut Frame, path: u64, argv: u64) -> i64 {
    let Some(path) = read_user_cstr(path) else { return -EFAULT };
    let mut args = Vec::new();
    if argv != 0 {
        for i in 0.. {
            if i >= 64 {
                return -E2BIG;
            }
            let slot = argv + i * 8;
            if !user_range_ok(slot, 8, false) {
                return -EFAULT;
            }
            let ptr = unsafe { *(slot as *const u64) };
            if ptr == 0 {
                break;
            }
            let Some(arg) = read_user_cstr(ptr) else { return -EFAULT };
            args.push(arg);
        }
    }
    match super::exec(f, &path, &args) {
        Ok(()) => 0,
        Err(e) => -e,
    }
}

fn read_user_cstr(addr: u64) -> Option<String> {
    let mut bytes = Vec::new();
    for i in 0..4096 {
        let a = addr.checked_add(i)?;
        if (i == 0 || a % 4096 == 0) && !user_range_ok(a, 1, false) {
            return None;
        }
        let b = unsafe { *(a as *const u8) };
        if b == 0 {
            return String::from_utf8(bytes).ok();
        }
        bytes.push(b);
    }
    None
}
