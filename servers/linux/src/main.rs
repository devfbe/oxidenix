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

use restricted::*;

fn syscall0(nr: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") nr => ret, out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

/// Each thread of a Linux program starts here, on its own server stack,
/// with its `State` (the program's registers) at `state`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(state: *mut State) -> ! {
    loop {
        if syscall0(SYS_RESTRICTED_ENTER) != REASON_SYSCALL {
            continue;
        }
        let nr = unsafe { (*state).rax };
        if nr >= FIRST_NON_LINUX {
            unsafe { (*state).rax = -38i64 as u64 }; // ENOSYS
            continue;
        }
        syscall0(SYS_LEGACY_SYSCALL);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // An invalid opcode: the kernel ends the process.
    loop {
        unsafe { core::arch::asm!("ud2") };
    }
}
