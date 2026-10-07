//! The interface between the kernel and the Linux server for restricted
//! mode (docs/design/linux-server.md): the layout of the server's shared
//! region, the per-thread register block, and the kernel calls the server
//! makes from normal mode.

#![no_std]

/// The shared region: PML4 slot 128 (512 GiB), above the 64 TiB the Linux
/// program owns (ADR 0003). Mapped in the normal view only. (Slots 129-255
/// stay free for a larger region.)
pub const SHARED_BASE: u64 = 0x4000_0000_0000;
pub const SHARED_END: u64 = SHARED_BASE + 0x80_0000_0000;
/// Where the server's program is linked.
pub const IMAGE_BASE: u64 = SHARED_BASE;
/// Per-thread areas: a guard page, the server's stack for the thread, and
/// the page with its `State`.
pub const THREADS_BASE: u64 = SHARED_BASE + 0x40_0000_0000;
pub const THREAD_AREA: u64 = 64 * 1024;
pub const THREAD_STACK: u64 = 32 * 1024;
/// Most threads one instance can run at once.
pub const MAX_THREADS: u64 = (SHARED_END - THREADS_BASE) / THREAD_AREA;

/// Stack of thread area `n` (top), and its `State` page.
pub const fn thread_stack_top(n: u64) -> u64 {
    THREADS_BASE + n * THREAD_AREA + 4096 + THREAD_STACK
}

pub const fn thread_state(n: u64) -> u64 {
    thread_stack_top(n)
}

/// The Linux program's registers while the server handles one of its
/// traps: the kernel writes them when the program traps and reads them
/// when the server enters restricted mode again. The server may change
/// them (a system call's result, a signal frame); the kernel accepts only
/// a user-mode instruction pointer and stack below 64 TiB and the flags a
/// program may set.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct State {
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
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

/// Kernel calls of the server (from normal mode only).
///
/// `restricted_enter()`: runs the program with the registers in the
/// thread's `State` until it traps; returns the reason (`REASON_*`).
pub const SYS_RESTRICTED_ENTER: u64 = 1010;
/// `legacy_syscall()`: has the kernel's own Linux implementation carry out
/// the system call in `State` (phase R1's pass-through, which goes away as
/// the server takes the calls over); its result and any signal frame land
/// in `State`.
pub const SYS_LEGACY_SYSCALL: u64 = 1011;

/// The program executed `syscall`; `State::rax` holds its number.
pub const REASON_SYSCALL: u64 = 1;

/// System call numbers at and above this are not Linux's: the server
/// answers them with ENOSYS without asking the kernel (iobench uses one to
/// measure a forwarded call alone).
pub const FIRST_NON_LINUX: u64 = 1000;
