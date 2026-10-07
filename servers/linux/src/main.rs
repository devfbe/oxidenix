//! The Linux server (docs/design/linux-server.md). It runs on the threads
//! of the Linux programs it serves, in their address spaces' normal view,
//! and handles what the programs trap into: in phase R1 it hands every
//! system call back to the kernel's own Linux implementation.
//!
//! Invariants the kernel relies on: no FPU/SSE (the target has none) and
//! no FS or GS base, so the program's FPU registers and TLS pointer stay in
//! the CPU while the server runs.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};
use restricted::*;

const PAGE: u64 = 4096;
const ENOSYS: i64 = 38;
const PROT_READ: u64 = 1;
const PROT_RW: u64 = 3;

fn syscall(nr: u64, a: [u64; 6]) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr as i64 => ret,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2], in("r10") a[3], in("r8") a[4], in("r9") a[5],
            out("rcx") _, out("r11") _,
            options(nostack),
        );
    }
    ret
}

fn call0(nr: u64) -> i64 {
    syscall(nr, [0; 6])
}

/// Each thread of a Linux program starts here, on its own server stack,
/// with its `State` (the program's registers) at `state`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(state: *mut State) -> ! {
    loop {
        if call0(SYS_RESTRICTED_ENTER) as u64 != REASON_SYSCALL {
            continue;
        }
        let s = unsafe { &mut *state };
        match s.rax {
            TEST_MAP..=TEST_MAP_AT => s.rax = test(s.rax, s.rdi) as u64,
            nr if nr >= FIRST_NON_LINUX => s.rax = -ENOSYS as u64,
            _ => {
                call0(SYS_LEGACY_SYSCALL);
            }
        }
    }
}

/// The object of the last TEST_MAP (one per instance, as test calls go).
static TEST_OBJECT: AtomicU64 = AtomicU64::new(0);

/// The test calls (see `restricted::TEST_*`).
fn test(nr: u64, addr: u64) -> i64 {
    let len = 3 * PAGE;
    match nr {
        TEST_MAP => {
            let h = syscall(SYS_MO_CREATE, [3, 0, 0, 0, 0, 0]);
            if h < 0 {
                return h;
            }
            let text = b"linux server";
            let wrote = syscall(SYS_MO_WRITE, [h as u64, PAGE, text.as_ptr() as u64, text.len() as u64, 0, 0]);
            if wrote != text.len() as i64 {
                return if wrote < 0 { wrote } else { -5 };
            }
            let mapped = syscall(SYS_MO_MAP, [h as u64, addr, len, 0, PROT_RW, MO_SHARED]);
            if mapped < 0 {
                return mapped;
            }
            TEST_OBJECT.store(h as u64, Ordering::Relaxed);
            0
        }
        TEST_READ => {
            let mut byte = [0u8; 1];
            let r = syscall(SYS_MO_READ, [TEST_OBJECT.load(Ordering::Relaxed), 0, byte.as_mut_ptr() as u64, 1, 0, 0]);
            if r < 0 { r } else { byte[0] as i64 }
        }
        TEST_PROTECT => syscall(SYS_MO_PROTECT, [addr, len, PROT_READ, 0, 0, 0]),
        TEST_UNMAP => {
            let r = syscall(SYS_MO_UNMAP, [addr, len, 0, 0, 0, 0]);
            syscall(SYS_HANDLE_CLOSE, [TEST_OBJECT.swap(0, Ordering::Relaxed), 0, 0, 0, 0, 0]);
            r
        }
        TEST_MAP_AT => {
            let h = syscall(SYS_MO_CREATE, [1, 0, 0, 0, 0, 0]);
            if h < 0 {
                return h;
            }
            let r = syscall(SYS_MO_MAP, [h as u64, addr, PAGE, 0, PROT_RW, MO_SHARED]);
            syscall(SYS_HANDLE_CLOSE, [h as u64, 0, 0, 0, 0, 0]);
            if r >= 0 {
                syscall(SYS_MO_UNMAP, [addr, PAGE, 0, 0, 0, 0]);
                return 0;
            }
            r
        }
        _ => -ENOSYS,
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // An invalid opcode: the kernel ends the process.
    loop {
        unsafe { core::arch::asm!("ud2") };
    }
}
