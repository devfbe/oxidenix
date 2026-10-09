//! Minimal runtime for oxidenix's native servers: entry point, raw system
//! calls (the kernel's own interface: no Linux call, the kernel implements
//! none), a heap, printing and port I/O.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use core::fmt;
use linked_list_allocator::LockedHeap;

/// The kernel's calls for native servers (`kernel/src/process/native.rs`).
/// Where a call is a mechanism the Linux server uses too, it has the number
/// and contract of `restricted`'s (documented there), on the caller's own
/// memory.
pub mod sys {
    /// (handle 0, addr, len, 0, prot, flags) -> address: anonymous private
    /// memory (`restricted::MO_FIXED`, `MO_NOREPLACE`, `MO_NORESERVE`,
    /// `MO_POPULATE`).
    pub const MO_MAP: u64 = restricted::SYS_MO_MAP;
    /// (addr, len).
    pub const MO_UNMAP: u64 = restricted::SYS_MO_UNMAP;
    /// (addr, len, prot): EACCES for memory that may not get `prot` (a
    /// read-only grant), ENOMEM for a range not all mapped.
    pub const MO_PROTECT: u64 = restricted::SYS_MO_PROTECT;
    /// (addr, val, deadline, bitset, flags), (addr, n, bitset, flags),
    /// (addr, n_wake, n_move, addr2, val, flags): futexes on this program's
    /// memory (shared words keyed by their object, so a channel's words meet
    /// the client's).
    pub const FUTEX_WAIT: u64 = restricted::SYS_FUTEX_WAIT;
    pub const FUTEX_WAKE: u64 = restricted::SYS_FUTEX_WAKE;
    pub const FUTEX_REQUEUE: u64 = restricted::SYS_FUTEX_REQUEUE;
    /// (clock) -> nanoseconds (`restricted::CLOCK_*`).
    pub const CLOCK_READ: u64 = restricted::SYS_CLOCK_READ;
    pub const YIELD: u64 = restricted::SYS_YIELD;
    /// (buf, len) -> n: at most 256 bytes of the kernel's generator.
    pub const RANDOM: u64 = restricted::SYS_RANDOM;
    /// (wait status, `restricted::EXIT_GROUP`): the process ends.
    pub const EXIT: u64 = restricted::SYS_THREAD_EXIT;
    /// (name, name length, argument, flags) -> service id; flags:
    /// `IPC_CHANNELS` (the service accepts channel offers)
    pub const IPC_REGISTER: u64 = 1000;
    /// (buffer, buffer length, &request id) -> message length
    pub const IPC_RECEIVE: u64 = 1001;
    /// (request id, buffer, length)
    pub const IPC_REPLY: u64 = 1002;
    /// (interrupt line): unmasks the server's device interrupt
    pub const IRQ_ENABLE: u64 = 1003;
    /// (&physical address) -> address of the server's DMA area
    pub const DMA_MAP: u64 = 1004;
    /// (op, argument, buffer, length) -> bytes of process/system information
    pub const PROC_QUERY: u64 = 1005;
    /// (from, count, on): access to the I/O ports the kernel assigned the
    /// server (1006 was `ipc_notify` until R7b).
    pub const IOPERM: u64 = 1006;
    /// (argv): runs the server's program again with the NULL-terminated
    /// arguments `argv`; the process is no server any more (the kernel
    /// counts the incarnation's end).
    pub const EXEC: u64 = 1007;
    /// (buf, len) -> n: text on the kernel's console, at most 4096 bytes.
    pub const LOG: u64 = 1008;

    // The service's end of a channel (docs/design/io-rings.md; the client's
    // calls are `restricted::SYS_CHAN_CREATE` and the following).

    /// (channel) -> address: maps a channel offered to this service (an
    /// `Offer` in a control request) into its memory, read and write; ENOENT
    /// if it was not offered to it, EPIPE if the client is gone.
    pub const CHAN_ATTACH: u64 = 1068;
    /// (channel): lets go of the channel: the mappings of it and of its
    /// grants go, the client sees `SERVICE_GONE`; vouches that no device
    /// uses its grants any more.
    pub const CHAN_DETACH: u64 = 1069;
    /// (channel, grant, &info, max_pages) -> address: maps a grant of the
    /// channel's client (read-only unless granted writable; never
    /// executable, not inherited by fork, gone when revoked); stores
    /// (pages, writable) as two u64s at `info`. ENOENT for an unknown or
    /// revoked grant; E2BIG, nothing mapped, for one of more than
    /// `max_pages` pages (0: any).
    pub const GRANT_MAP: u64 = 1070;
    /// (channel, grant, offset) -> device address of the byte at `offset`
    /// of the grant (valid to the end of its page); EINVAL beyond the
    /// grant. A revoked grant stays pinned until `GRANT_DMA_UNMAP`.
    pub const GRANT_DMA: u64 = 1071;
    /// (channel, grant): the service's devices are done with the grant.
    pub const GRANT_DMA_UNMAP: u64 = 1072;
    /// (channel, value): arms the service's doorbell watch on the
    /// channel's submission ring: if its `tail` still holds `value`, the
    /// client's next doorbell (or its end going) makes `ipc_receive`
    /// return `Event::Doorbell`. One-shot; EAGAIN if the tail moved, EPIPE
    /// once the client is gone. (The service announced the sleep in the
    /// ring first: `ring::Consumer::prepare_sleep`.)
    pub const CHAN_WATCH: u64 = 1073;
    /// (insn, fixup): a fault of this program's instruction at `insn` (its
    /// copy routine on granted memory, `copy::copy`) that cannot be
    /// resolved resumes at `fixup` instead of killing it. Once per program.
    pub const SET_COPY_FIXUP: u64 = 1074;
    /// (channel, grant, first page, count, &addresses): the device
    /// addresses of `count` (at most 512) pages of a grant, stored as u64s;
    /// `GRANT_DMA` for a range.
    pub const GRANT_DMA_PAGES: u64 = 1075;
}

/// `ipc_register` flag: the service accepts channel offers.
pub const IPC_CHANNELS: u64 = 1;
/// Set in the id of a control request (from the kernel itself).
pub const IPC_CONTROL: u64 = 1 << 63;

/// Raw system call; returns the kernel's result (negative errno on error).
pub fn syscall(nr: u64, a: [u64; 6]) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr as i64 => ret,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
            in("r10") a[3], in("r8") a[4], in("r9") a[5],
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Writes `text` on the kernel's console.
pub fn log(text: &[u8]) {
    for chunk in text.chunks(4096) {
        syscall(sys::LOG, [chunk.as_ptr() as u64, chunk.len() as u64, 0, 0, 0, 0]);
    }
}

/// Ends the process with exit code `code`.
pub fn exit(code: i32) -> ! {
    syscall(sys::EXIT, [((code as u64) & 0xff) << 8, restricted::EXIT_GROUP, 0, 0, 0, 0]);
    unreachable!("exit returned");
}

/// Fills `buf` from the kernel's random generator (ChaCha20, seeded from
/// the CPU's entropy source and timing jitter: fit for secrets).
pub fn getrandom(buf: &mut [u8]) {
    let mut done = 0;
    while done < buf.len() {
        let n = syscall(sys::RANDOM, [buf[done..].as_mut_ptr() as u64, (buf.len() - done) as u64, 0, 0, 0, 0]);
        if n <= 0 {
            // Only a bad buffer fails, and this one is ours.
            exit(103);
        }
        done += n as usize;
    }
}

/// The kernel's clock `id` (`restricted::CLOCK_*`) in nanoseconds.
pub fn clock(id: u64) -> u64 {
    syscall(sys::CLOCK_READ, [id, 0, 0, 0, 0, 0]).max(0) as u64
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    clock(restricted::CLOCK_WALL) / 1_000_000_000
}

/// Milliseconds since boot (monotonic).
pub fn uptime_ms() -> u64 {
    clock(restricted::CLOCK_MONO) / 1_000_000
}

/// Lets other processes run before this one continues.
pub fn sched_yield() {
    syscall(sys::YIELD, [0; 6]);
}

/// Asks the kernel for access to I/O ports [from, from + count).
pub fn ioperm(from: u16, count: u16) -> Result<(), i64> {
    match syscall(sys::IOPERM, [from as u64, count as u64, 1, 0, 0, 0]) {
        0 => Ok(()),
        e => Err(e),
    }
}

/// Registers this process as the server behind `name`; `arg` is passed to
/// the kernel (for filesystems: the root inode).
pub fn ipc_register(name: &str, arg: u64) -> Result<u64, i64> {
    ipc_register_with(name, arg, 0)
}

/// `ipc_register` with flags (`IPC_CHANNELS`).
pub fn ipc_register_with(name: &str, arg: u64, flags: u64) -> Result<u64, i64> {
    match syscall(sys::IPC_REGISTER, [name.as_ptr() as u64, name.len() as u64, arg, flags, 0, 0]) {
        e if e < 0 => Err(e),
        id => Ok(id as u64),
    }
}

/// What `ipc_receive` returns.
pub enum Event {
    /// A request: (request id, message length).
    Request(u64, usize),
    /// A control request of the kernel's (a channel offer, see
    /// `ring::channel::Offer`), only to services registered with
    /// `IPC_CHANNELS`: (request id, message length).
    Control(u64, usize),
    /// Device interrupts fired; the bit mask of the lines.
    Interrupt(u16),
    /// A doorbell the service watches rang (`chan_watch`): which one is for
    /// the service to find out (it polls its rings).
    Doorbell,
    /// The timeout ran out.
    Timeout,
}

pub const ETIMEDOUT: i64 = 110;
pub const EAGAIN: i64 = 11;
/// `ipc_receive`'s result (with request id 0) for a doorbell.
const DOORBELL: i64 = 1 << 16;

/// Waits for the next request or device interrupt, at most `timeout_ms`
/// milliseconds (None: forever).
pub fn ipc_receive(buf: &mut [u8], timeout_ms: Option<u64>) -> Result<Event, i64> {
    let mut id = 0u64;
    let timeout = timeout_ms.map_or(u64::MAX, |t| t.min(i64::MAX as u64));
    let r = syscall(sys::IPC_RECEIVE, [buf.as_mut_ptr() as u64, buf.len() as u64, &mut id as *mut u64 as u64, timeout, 0, 0]);
    match r {
        r if r == -ETIMEDOUT => Ok(Event::Timeout),
        r if r < 0 => Err(r),
        DOORBELL if id == 0 => Ok(Event::Doorbell),
        r if id == 0 => Ok(Event::Interrupt(r as u16)),
        r if id & IPC_CONTROL != 0 => Ok(Event::Control(id, r as usize)),
        r => Ok(Event::Request(id, r as usize)),
    }
}

/// Unmasks the device interrupt line assigned to this server.
pub fn irq_enable(line: u8) -> Result<(), i64> {
    match syscall(sys::IRQ_ENABLE, [line as u64, 0, 0, 0, 0, 0]) {
        0 => Ok(()),
        e => Err(e),
    }
}

/// Asks the kernel for process or system information (see `procproto`);
/// returns the number of bytes written to `buf`.
pub fn proc_query(op: u64, arg: u64, buf: &mut [u8]) -> Result<usize, i64> {
    match syscall(sys::PROC_QUERY, [op, arg, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0]) {
        e if e < 0 => Err(e),
        n => Ok(n as usize),
    }
}

/// Maps the server's DMA area: (address, physical address).
pub fn dma_map() -> Result<(*mut u8, u64), i64> {
    let mut phys = 0u64;
    match syscall(sys::DMA_MAP, [&mut phys as *mut u64 as u64, 0, 0, 0, 0, 0]) {
        e if e < 0 => Err(e),
        addr => Ok((addr as *mut u8, phys)),
    }
}

pub fn ipc_reply(id: u64, msg: &[u8]) -> Result<(), i64> {
    match syscall(sys::IPC_REPLY, [id, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0]) {
        0 => Ok(()),
        e => Err(e),
    }
}

fn result(r: i64) -> Result<u64, i64> {
    if r < 0 { Err(r) } else { Ok(r as u64) }
}

/// Maps the offered channel `channel`; returns its address.
pub fn chan_attach(channel: u64) -> Result<*mut u8, i64> {
    result(syscall(sys::CHAN_ATTACH, [channel, 0, 0, 0, 0, 0])).map(|a| a as *mut u8)
}

pub fn chan_detach(channel: u64) -> Result<(), i64> {
    result(syscall(sys::CHAN_DETACH, [channel, 0, 0, 0, 0, 0])).map(|_| ())
}

/// Maps grant `grant` of `channel`: (address, pages, writable).
pub fn grant_map(channel: u64, grant: u32) -> Result<(*mut u8, u64, bool), i64> {
    grant_map_max(channel, grant, 0).map_err(|(e, _)| e)
}

/// `grant_map` of a grant of at most `max_pages` pages: the error -E2BIG
/// (nothing mapped) for a larger one, with its size in pages (0 for an
/// unknown grant).
pub fn grant_map_max(channel: u64, grant: u32, max_pages: u64) -> Result<(*mut u8, u64, bool), (i64, u64)> {
    let mut info = [0u64; 2];
    let addr = result(syscall(sys::GRANT_MAP, [channel, grant as u64, info.as_mut_ptr() as u64, max_pages, 0, 0])).map_err(|e| (e, info[0]))?;
    Ok((addr as *mut u8, info[0], info[1] != 0))
}

/// The device address of byte `offset` of grant `grant`.
pub fn grant_dma(channel: u64, grant: u32, offset: u64) -> Result<u64, i64> {
    result(syscall(sys::GRANT_DMA, [channel, grant as u64, offset, 0, 0, 0]))
}

pub fn grant_dma_unmap(channel: u64, grant: u32) -> Result<(), i64> {
    result(syscall(sys::GRANT_DMA_UNMAP, [channel, grant as u64, 0, 0, 0, 0])).map(|_| ())
}

/// The device addresses of the pages `first..first + out.len()` of grant
/// `grant` (at most 512 at once).
pub fn grant_dma_pages(channel: u64, grant: u32, first: u64, out: &mut [u64]) -> Result<(), i64> {
    result(syscall(sys::GRANT_DMA_PAGES, [channel, grant as u64, first, out.len() as u64, out.as_mut_ptr() as u64, 0])).map(|_| ())
}

/// Arms the doorbell watch of `channel` if its submission `tail` still
/// holds `value`: Ok(true) when armed, Ok(false) when the tail moved (do
/// not sleep: there is work).
pub fn chan_watch(channel: u64, value: u32) -> Result<bool, i64> {
    match syscall(sys::CHAN_WATCH, [channel, value as u64, 0, 0, 0, 0]) {
        0 => Ok(true),
        e if e == -EAGAIN => Ok(false),
        e => Err(e),
    }
}

/// Copies to and from granted memory that a revoke may take away at any
/// moment (docs/design/io-rings.md, "The service's contract for grant
/// memory"): a fault of the copy is resolved by the kernel resuming at the
/// fixup (`sys::SET_COPY_FIXUP`), and the copy fails instead of the
/// service.
pub mod copy {
    use super::{syscall, sys};

    unsafe extern "C" {
        static oxrt_copy_insn: u8;
        static oxrt_copy_fixup: u8;
    }

    /// Copies `len` bytes; returns how many were not copied (0: all).
    #[unsafe(naked)]
    unsafe extern "C" fn copy_bytes(dst: *mut u8, src: *const u8, len: usize) -> usize {
        core::arch::naked_asm!(
            "mov rcx, rdx",
            ".global oxrt_copy_insn",
            "oxrt_copy_insn:",
            "rep movsb",
            "xor eax, eax",
            "ret",
            ".global oxrt_copy_fixup",
            "oxrt_copy_fixup:",
            "mov rax, rcx",
            "ret",
        );
    }

    /// Tells the kernel where the copy may fault (once, at start).
    pub fn register() -> Result<(), i64> {
        let (insn, fixup) = (&raw const oxrt_copy_insn as u64, &raw const oxrt_copy_fixup as u64);
        match syscall(sys::SET_COPY_FIXUP, [insn, fixup, 0, 0, 0, 0]) {
            0 => Ok(()),
            e => Err(e),
        }
    }

    /// Copies `len` bytes from `src` to `dst`, either of which may be
    /// granted memory; false if it faulted (part may have been copied).
    ///
    /// # Safety
    /// Whatever of the two ranges is not granted memory must be valid for
    /// the access; granted ranges must lie within the grant's mapping.
    pub unsafe fn copy(dst: *mut u8, src: *const u8, len: usize) -> bool {
        unsafe { copy_bytes(dst, src, len) == 0 }
    }
}

/// Anonymous private memory of `len` bytes, readable and writable,
/// where the kernel finds room.
pub fn alloc_pages(len: usize) -> Result<*mut u8, i64> {
    const PROT_RW: u64 = 3;
    result(syscall(sys::MO_MAP, [0, 0, len as u64, 0, PROT_RW, 0])).map(|a| a as *mut u8)
}

/// Unmaps [addr, addr + len).
pub fn munmap(addr: *const u8, len: usize) -> Result<(), i64> {
    result(syscall(sys::MO_UNMAP, [addr as u64, len as u64, 0, 0, 0, 0])).map(|_| ())
}

/// Changes the protection of [addr, addr + len) (PROT_ bits).
pub fn mprotect(addr: *const u8, len: usize, prot: u64) -> Result<(), i64> {
    result(syscall(sys::MO_PROTECT, [addr as u64, len as u64, prot, 0, 0, 0])).map(|_| ())
}

/// Every bit of a futex's bitset.
const ANY: u64 = u32::MAX as u64;

/// Waits on a shared word (the word may be in memory shared with another
/// process, a channel): sleeps while `*word == value`, at most
/// `timeout_ms` (None: no limit). EPIPE once the word's object was hung up
/// (a channel whose peer is gone), ETIMEDOUT, EAGAIN if it changed.
pub fn futex_wait(word: &core::sync::atomic::AtomicU32, value: u32, timeout_ms: Option<u64>) -> Result<(), i64> {
    let deadline = timeout_ms.map_or(0, |ms| clock(restricted::CLOCK_MONO).saturating_add(ms.saturating_mul(1_000_000)).max(1));
    result(syscall(sys::FUTEX_WAIT, [word as *const _ as u64, value as u64, deadline, ANY, 0, 0])).map(|_| ())
}

/// Wakes up to `n` waiters on a shared word.
pub fn futex_wake(word: &core::sync::atomic::AtomicU32, n: u32) -> Result<u64, i64> {
    result(syscall(sys::FUTEX_WAKE, [word as *const _ as u64, n as u64, ANY, 0, 0, 0]))
}

/// Wakes up to `n_wake` waiters on `word` and moves up to `n_move` more to
/// wait on `other`: how many were woken and moved.
pub fn futex_requeue(word: &core::sync::atomic::AtomicU32, n_wake: u32, n_move: u32, other: &core::sync::atomic::AtomicU32) -> Result<u64, i64> {
    let (from, to) = (word as *const _ as u64, other as *const _ as u64);
    result(syscall(sys::FUTEX_REQUEUE, [from, n_wake as u64, n_move as u64, to, 0, 0]))
}

pub mod port {
    use core::arch::asm;

    pub unsafe fn inb(port: u16) -> u8 {
        let v: u8;
        unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)) };
        v
    }
    pub unsafe fn outb(port: u16, v: u8) {
        unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) };
    }
    pub unsafe fn inw(port: u16) -> u16 {
        let v: u16;
        unsafe { asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack)) };
        v
    }
    pub unsafe fn outw(port: u16, v: u16) {
        unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack)) };
    }
    pub unsafe fn inl(port: u16) -> u32 {
        let v: u32;
        unsafe { asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack)) };
        v
    }
    pub unsafe fn outl(port: u16, v: u32) {
        unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack)) };
    }
}

pub struct Stdout;

impl fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        log(s.as_bytes());
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ({ let _ = core::fmt::Write::write_fmt(&mut $crate::Stdout, format_args!($($arg)*)); });
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();
/// The heap a server gets unless its `entry!` asks for another size.
pub const HEAP_SIZE: usize = 4 * 1024 * 1024;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("panic: {}", info);
    exit(101)
}

/// Called by `entry!`: sets up a heap of `heap_size` bytes (committed
/// memory), collects argv and runs `main`.
///
/// # Safety
/// `sp` must be the initial stack pointer handed over by the kernel.
pub unsafe fn start(sp: *const u64, main: fn(Vec<&'static str>) -> i32, heap_size: usize) -> ! {
    let Ok(heap) = alloc_pages(heap_size) else { exit(102) };
    unsafe { HEAP.lock().init(heap, heap_size) };
    let argc = unsafe { *sp } as usize;
    let mut args = Vec::new();
    for i in 0..argc {
        let p = unsafe { *sp.add(1 + i) } as *const u8;
        let mut len = 0;
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        let bytes = unsafe { core::slice::from_raw_parts(p, len) };
        args.push(core::str::from_utf8(bytes).unwrap_or(""));
    }
    exit(main(args))
}

/// Defines the program entry point `_start`, which calls `main(args)`;
/// `heap = bytes` sizes the heap (default `HEAP_SIZE`).
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        $crate::entry!($main, heap = $crate::HEAP_SIZE);
    };
    ($main:path, heap = $heap:expr) => {
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        unsafe extern "C" fn _start() -> ! {
            core::arch::naked_asm!("mov rdi, rsp", "and rsp, -16", "call {start}", start = sym __oxrt_start);
        }

        unsafe extern "C" fn __oxrt_start(sp: *const u64) -> ! {
            unsafe { $crate::start(sp, $main, $heap) }
        }
    };
}
