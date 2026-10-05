use super::address_space::user_range_ok;
use crate::interrupts::gdt;
use x86_64::registers::model_specific::{Efer, EferFlags, FsBase, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

const ENOSYS: i64 = 38;
const EFAULT: i64 = 14;
const EBADF: i64 = 9;
const ENOTTY: i64 = 25;
const EINVAL: i64 = 22;

const STACK_SIZE: usize = 4096 * 4;

#[repr(align(16))]
struct Stack {
    _bytes: [u8; STACK_SIZE],
}

static mut SYSCALL_STACK: Stack = Stack { _bytes: [0; STACK_SIZE] };
#[unsafe(no_mangle)]
static mut SYSCALL_STACK_TOP: u64 = 0;
#[unsafe(no_mangle)]
static mut SYSCALL_USER_RSP: u64 = 0;

/// Registerabbild in der Reihenfolge, in der `syscall_entry` pusht.
#[repr(C)]
pub struct Frame {
    nr: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    rip: u64,
    rflags: u64,
    rsp: u64,
}

pub fn init() {
    unsafe {
        SYSCALL_STACK_TOP = (&raw const SYSCALL_STACK) as u64 + STACK_SIZE as u64;
    }
    let s = gdt::selectors();
    Star::write(s.user_code, s.user_data, s.kernel_code, s.kernel_data)
        .expect("GDT-Layout passt nicht zu sysret");
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
    // IF loeschen: der Stackwechsel unten darf nicht unterbrochen werden.
    SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG | RFlags::TRAP_FLAG);
    unsafe { Efer::update(|f| *f |= EferFlags::SYSTEM_CALL_EXTENSIONS) };
}

#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        "mov [rip + SYSCALL_USER_RSP], rsp",
        "mov rsp, [rip + SYSCALL_STACK_TOP]",
        "push qword ptr [rip + SYSCALL_USER_RSP]",
        "push r11",
        "push rcx",
        "push r9",
        "push r8",
        "push r10",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rax",
        "mov rdi, rsp",
        "call {dispatch}",
        "add rsp, 8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop r10",
        "pop r8",
        "pop r9",
        "pop rcx",
        "pop r11",
        "pop rsp",
        "sysretq",
        dispatch = sym dispatch,
    );
}

extern "sysv64" fn dispatch(f: &mut Frame) -> i64 {
    match f.nr {
        1 => sys_write(f.a0, f.a1, f.a2),
        16 => sys_ioctl(f.a0),
        20 => sys_writev(f.a0, f.a1, f.a2),
        60 | 231 => unsafe { super::return_to_kernel(f.a0 as i32 as i64) },
        158 => sys_arch_prctl(f.a0, f.a1),
        218 => 1,
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

fn sys_ioctl(fd: u64) -> i64 {
    if fd > 2 {
        return -EBADF;
    }
    -ENOTTY
}

fn sys_arch_prctl(code: u64, addr: u64) -> i64 {
    const ARCH_SET_FS: u64 = 0x1002;
    match code {
        ARCH_SET_FS if addr < super::address_space::USER_END => {
            FsBase::write(VirtAddr::new(addr));
            0
        }
        _ => -EINVAL,
    }
}
