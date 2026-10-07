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

// Kernel objects, by handle (one table per server instance, so an object
// can be used from any thread of the process tree).

/// `handle_close(handle)`.
pub const SYS_HANDLE_CLOSE: u64 = 1012;
/// `mo_create(pages) -> handle`: a zero-filled memory object of `pages`
/// pages (committed memory).
pub const SYS_MO_CREATE: u64 = 1013;
/// `mo_map(handle, addr, len, offset, prot, flags) -> addr`: maps `len`
/// bytes of the object from `offset` at `addr` in the calling thread's
/// program view (replacing what was there), shared (`MO_SHARED`: stores
/// reach the object) or private (copy-on-write). `prot`: mmap's PROT_ bits.
pub const SYS_MO_MAP: u64 = 1014;
/// `mo_unmap(addr, len)` in the calling thread's program view.
pub const SYS_MO_UNMAP: u64 = 1015;
/// `mo_protect(addr, len, prot)` in the calling thread's program view.
pub const SYS_MO_PROTECT: u64 = 1016;
/// `mo_read(handle, offset, buf, len) -> bytes`: from the object into the
/// server's memory.
pub const SYS_MO_READ: u64 = 1017;
/// `mo_write(handle, offset, buf, len) -> bytes`: from the server's memory
/// into the object (within its size).
pub const SYS_MO_WRITE: u64 = 1018;

pub const MO_SHARED: u64 = 1;

/// Test calls a program can make to its server (lxtest): they exercise the
/// kernel interface above on the calling process. Each returns 0 or a
/// negative errno.
/// `(addr)`: a 3-page object with "linux server" at its second page,
/// mapped shared and writable at `addr`.
pub const TEST_MAP: u64 = 1500;
/// `()`: the first byte of that object.
pub const TEST_READ: u64 = 1501;
/// `(addr)`: the mapping made read-only.
pub const TEST_PROTECT: u64 = 1502;
/// `(addr)`: the mapping removed and the object's handle closed.
pub const TEST_UNMAP: u64 = 1503;
/// `(addr)`: maps a fresh object at `addr` and returns the kernel's answer.
pub const TEST_MAP_AT: u64 = 1504;

// Paged memory objects: the server supplies their pages on demand, from a
// thread of its own (the pager thread), while the thread that needs a page
// sleeps in the kernel (it may be the kernel itself, copying from a
// mapping, so the request cannot go to that thread's server).

/// `mo_create_paged(pages, key) -> handle`: a memory object whose pages
/// the server supplies; requests name it by `key`.
pub const SYS_MO_CREATE_PAGED: u64 = 1019;
/// `pager_wait(request) -> 0`: the pager thread waits for the next page
/// someone needs and gets it as a `PagerRequest`. When the instance's last
/// program is gone, the pager's process ends here.
pub const SYS_PAGER_WAIT: u64 = 1020;
/// `mo_supply(handle, offset, buf, len)`: the page at `offset` of a paged
/// object, from `len` bytes at `buf` (the rest zero), unless it is there
/// already; wakes whoever waits for it.
pub const SYS_MO_SUPPLY: u64 = 1021;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PagerRequest {
    pub key: u64,
    /// Byte offset of the page in the object.
    pub offset: u64,
}

/// A server thread starts with its role in `rsi` (and its `State` in
/// `rdi`): it serves a program's thread, or it is the instance's pager.
pub const ROLE_PROGRAM: u64 = 0;
pub const ROLE_PAGER: u64 = 1;

/// `(addr)`: a 4-page paged object mapped shared and readable at `addr`;
/// page n reads "paged n" (supplied by the pager thread when touched).
pub const TEST_PAGED: u64 = 1505;
/// `()`: how many pages the pager supplied so far.
pub const TEST_SUPPLIED: u64 = 1506;
