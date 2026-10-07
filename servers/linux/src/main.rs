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

extern crate alloc;

mod heap;
mod sync;

use core::sync::atomic::{AtomicU64, Ordering};
use restricted::*;

#[global_allocator]
static HEAP: heap::ServerHeap = heap::ServerHeap::new();

const PAGE: u64 = 4096;
const ENOSYS: i64 = 38;
const PROT_READ: u64 = 1;
const PROT_RW: u64 = 3;

pub(crate) fn syscall(nr: u64, a: [u64; 6]) -> i64 {
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
/// with its `State` (the program's registers) at `state`; so does the
/// instance's pager thread (`role`).
#[unsafe(no_mangle)]
pub extern "C" fn _start(state: *mut State, role: u64) -> ! {
    if role == ROLE_PAGER {
        pager();
    }
    loop {
        if call0(SYS_RESTRICTED_ENTER) as u64 != REASON_SYSCALL {
            continue;
        }
        let s = unsafe { &mut *state };
        match s.rax {
            TEST_MAP..=TEST_LOCKED_ADD => s.rax = test(s.rax, s.rdi) as u64,
            nr if nr >= FIRST_NON_LINUX => s.rax = -ENOSYS as u64,
            _ => {
                call0(SYS_LEGACY_SYSCALL);
            }
        }
    }
}

/// The object of the last TEST_MAP (one per instance, as test calls go).
static TEST_OBJECT: AtomicU64 = AtomicU64::new(0);
/// The paged object of TEST_PAGED, and how many pages the pager supplied.
static TEST_PAGED_OBJECT: AtomicU64 = AtomicU64::new(0);
static SUPPLIED: AtomicU64 = AtomicU64::new(0);
/// The key the test's paged object goes by; +1: never answered; +2:
/// failed once, then answered.
const TEST_KEY: u64 = 0x7e57;
static TEST_FAIL_OBJECT: AtomicU64 = AtomicU64::new(0);
static FAILED_ONCE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The pager thread: supplies the pages threads wait for. (So far the only
/// paged object is the test's; its page n reads "paged n".)
fn pager() -> ! {
    loop {
        let mut request = PagerRequest::default();
        if syscall(SYS_PAGER_WAIT, [&mut request as *mut PagerRequest as u64, 0, 0, 0, 0, 0]) < 0 {
            continue;
        }
        if request.key == TEST_KEY + 2 {
            let handle = TEST_FAIL_OBJECT.load(Ordering::Acquire);
            if !FAILED_ONCE.swap(true, Ordering::Relaxed) {
                syscall(SYS_MO_FAIL, [handle, request.offset, 0, 0, 0, 0]);
            } else {
                let text = b"retry";
                syscall(SYS_MO_SUPPLY, [handle, request.offset, text.as_ptr() as u64, text.len() as u64, 0, 0]);
            }
            continue;
        }
        if request.key != TEST_KEY {
            continue;
        }
        let mut page = [0u8; PAGE as usize];
        let text = *b"paged 0";
        page[..text.len()].copy_from_slice(&text);
        page[text.len() - 1] = b'0' + (request.offset / PAGE) as u8 % 10;
        let handle = TEST_PAGED_OBJECT.load(Ordering::Acquire);
        if syscall(SYS_MO_SUPPLY, [handle, request.offset, page.as_ptr() as u64, PAGE, 0, 0]) == 1 {
            SUPPLIED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// TEST_LOCKED_ADD's counter: one for the instance.
static COUNTER: sync::Mutex<u64> = sync::Mutex::new(0);

/// Allocates `n` blocks of many sizes, fills, checks and frees them (in an
/// order that leaves holes); 0 if every block kept its contents.
fn test_alloc(n: u64) -> i32 {
    use alloc::vec::Vec;
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for i in 0..n as usize {
        let size = 1 + (i * 7919) % 70_000;
        let mut v = Vec::with_capacity(size);
        v.resize(size, (i % 251) as u8);
        blocks.push(v);
        if i % 3 == 2 {
            // Drop every third block early.
            let gone = blocks.swap_remove(i / 3 % blocks.len());
            drop(gone);
        }
    }
    let good = blocks.iter().all(|b| b.iter().all(|&x| x == b[0]));
    if good { 0 } else { -1 }
}

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
        TEST_PAGED => {
            let h = syscall(SYS_MO_CREATE_PAGED, [4, TEST_KEY, 0, 0, 0, 0]);
            if h < 0 {
                return h;
            }
            TEST_PAGED_OBJECT.store(h as u64, Ordering::Release);
            let r = syscall(SYS_MO_MAP, [h as u64, addr, 4 * PAGE, 0, PROT_READ, MO_SHARED]);
            if r < 0 { r } else { 0 }
        }
        TEST_SUPPLIED => SUPPLIED.load(Ordering::Relaxed) as i64,
        TEST_PAGED_FAIL => {
            let h = syscall(SYS_MO_CREATE_PAGED, [1, TEST_KEY + 2, 0, 0, 0, 0]);
            if h < 0 {
                return h;
            }
            TEST_FAIL_OBJECT.store(h as u64, Ordering::Release);
            let r = syscall(SYS_MO_MAP, [h as u64, addr, PAGE, 0, PROT_READ, MO_SHARED]);
            if r < 0 { r } else { 0 }
        }
        TEST_ALLOC => test_alloc(addr) as i64,
        TEST_LOCKED_ADD => {
            for _ in 0..addr {
                let mut count = COUNTER.lock();
                let seen = *count;
                // A pause inside, so that other threads find the lock taken.
                for _ in 0..200 {
                    core::hint::spin_loop();
                }
                *count = seen + 1;
            }
            *COUNTER.lock() as i64
        }
        TEST_PAGED_STUCK => {
            // A key the pager does not answer.
            let h = syscall(SYS_MO_CREATE_PAGED, [1, TEST_KEY + 1, 0, 0, 0, 0]);
            if h < 0 {
                return h;
            }
            let r = syscall(SYS_MO_MAP, [h as u64, addr, PAGE, 0, PROT_READ, MO_SHARED]);
            syscall(SYS_HANDLE_CLOSE, [h as u64, 0, 0, 0, 0, 0]);
            if r < 0 { r } else { 0 }
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
