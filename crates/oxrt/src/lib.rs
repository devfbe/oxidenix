//! Minimal runtime for oxidenix servers: entry point, raw system calls
//! (Linux ABI plus the oxidenix IPC calls), a heap, printing and port I/O.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use core::fmt;
use linked_list_allocator::LockedHeap;

pub mod sys {
    pub const WRITE: u64 = 1;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const FUTEX: u64 = 202;
    pub const SCHED_YIELD: u64 = 24;
    pub const GETPID: u64 = 39;
    pub const EXIT_GROUP: u64 = 231;
    pub const IOPERM: u64 = 173;
    pub const CLOCK_GETTIME: u64 = 228;
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
    /// (token): wakes whoever waits on the server's object `token`
    pub const IPC_NOTIFY: u64 = 1006;

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
    /// (channel, grant, &info) -> address: maps a grant of the channel's
    /// client (read-only unless granted writable; never executable, not
    /// inherited by fork, gone when revoked); stores (pages, writable) as
    /// two u64s at `info`. ENOENT for an unknown or revoked grant.
    pub const GRANT_MAP: u64 = 1070;
    /// (channel, grant, offset) -> device address of the byte at `offset`
    /// of the grant (valid to the end of its page); EINVAL beyond the
    /// grant. A revoked grant stays pinned until `GRANT_DMA_UNMAP`.
    pub const GRANT_DMA: u64 = 1071;
    /// (channel, grant): the service's devices are done with the grant.
    pub const GRANT_DMA_UNMAP: u64 = 1072;
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

pub fn write(fd: u64, buf: &[u8]) -> i64 {
    syscall(sys::WRITE, [fd, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0])
}

pub fn exit(code: i32) -> ! {
    syscall(sys::EXIT_GROUP, [code as u64, 0, 0, 0, 0, 0]);
    unreachable!("exit_group returned");
}

pub fn getpid() -> i64 {
    syscall(sys::GETPID, [0; 6])
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    let mut ts = [0u64; 2];
    syscall(sys::CLOCK_GETTIME, [0, ts.as_mut_ptr() as u64, 0, 0, 0, 0]);
    ts[0]
}

/// Milliseconds since boot (monotonic).
pub fn uptime_ms() -> u64 {
    const CLOCK_MONOTONIC: u64 = 1;
    let mut ts = [0u64; 2];
    syscall(sys::CLOCK_GETTIME, [CLOCK_MONOTONIC, ts.as_mut_ptr() as u64, 0, 0, 0, 0]);
    ts[0] * 1000 + ts[1] / 1_000_000
}

/// Lets other processes run before this one continues.
pub fn sched_yield() {
    syscall(sys::SCHED_YIELD, [0; 6]);
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
    /// The timeout ran out.
    Timeout,
}

pub const ETIMEDOUT: i64 = 110;

/// Waits for the next request or device interrupt, at most `timeout_ms`
/// milliseconds (None: forever).
pub fn ipc_receive(buf: &mut [u8], timeout_ms: Option<u64>) -> Result<Event, i64> {
    let mut id = 0u64;
    let timeout = timeout_ms.map_or(u64::MAX, |t| t.min(i64::MAX as u64));
    let r = syscall(sys::IPC_RECEIVE, [buf.as_mut_ptr() as u64, buf.len() as u64, &mut id as *mut u64 as u64, timeout, 0, 0]);
    match r {
        r if r == -ETIMEDOUT => Ok(Event::Timeout),
        r if r < 0 => Err(r),
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

/// Tells the kernel that the object `token` of the calling server changed
/// (netd: the readiness of the socket with that handle).
pub fn ipc_notify(token: u64) -> Result<(), i64> {
    match syscall(sys::IPC_NOTIFY, [token, 0, 0, 0, 0, 0]) {
        0 => Ok(()),
        e => Err(e),
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
    let mut info = [0u64; 2];
    let addr = result(syscall(sys::GRANT_MAP, [channel, grant as u64, info.as_mut_ptr() as u64, 0, 0, 0]))?;
    Ok((addr as *mut u8, info[0], info[1] != 0))
}

/// The device address of byte `offset` of grant `grant`.
pub fn grant_dma(channel: u64, grant: u32, offset: u64) -> Result<u64, i64> {
    result(syscall(sys::GRANT_DMA, [channel, grant as u64, offset, 0, 0, 0]))
}

pub fn grant_dma_unmap(channel: u64, grant: u32) -> Result<(), i64> {
    result(syscall(sys::GRANT_DMA_UNMAP, [channel, grant as u64, 0, 0, 0, 0])).map(|_| ())
}

/// mprotect(2).
pub fn mprotect(addr: *const u8, len: usize, prot: u64) -> Result<(), i64> {
    result(syscall(sys::MPROTECT, [addr as u64, len as u64, prot, 0, 0, 0])).map(|_| ())
}

/// futex(2) FUTEX_WAIT on a shared word (not private: the word may be in
/// memory shared with another process, a channel): sleeps while `*word ==
/// value`, at most `timeout_ms` (None: no limit). EPIPE once the word's
/// object was hung up (a channel whose peer is gone).
pub fn futex_wait(word: &core::sync::atomic::AtomicU32, value: u32, timeout_ms: Option<u64>) -> Result<(), i64> {
    let ts = timeout_ms.map(|ms| [ms / 1000, (ms % 1000) * 1_000_000]);
    let ts_ptr = ts.as_ref().map_or(0, |t| t.as_ptr() as u64);
    result(syscall(sys::FUTEX, [word as *const _ as u64, 0, value as u64, ts_ptr, 0, 0])).map(|_| ())
}

/// futex(2) FUTEX_WAKE on a shared word: wakes up to `n` waiters.
pub fn futex_wake(word: &core::sync::atomic::AtomicU32, n: u32) -> Result<u64, i64> {
    result(syscall(sys::FUTEX, [word as *const _ as u64, 1, n as u64, 0, 0, 0]))
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
        write(1, s.as_bytes());
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
const HEAP_SIZE: usize = 4 * 1024 * 1024;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("panic: {}", info);
    exit(101)
}

/// Called by `entry!`: sets up the heap, collects argv and runs `main`.
///
/// # Safety
/// `sp` must be the initial stack pointer handed over by the kernel.
pub unsafe fn start(sp: *const u64, main: fn(Vec<&'static str>) -> i32) -> ! {
    const PROT_RW: u64 = 3;
    const MAP_PRIVATE_ANON: u64 = 0x22;
    let heap = syscall(sys::MMAP, [0, HEAP_SIZE as u64, PROT_RW, MAP_PRIVATE_ANON, u64::MAX, 0]);
    if heap < 0 {
        exit(102);
    }
    unsafe { HEAP.lock().init(heap as *mut u8, HEAP_SIZE) };
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

/// Defines the program entry point `_start`, which calls `main(args)`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        unsafe extern "C" fn _start() -> ! {
            core::arch::naked_asm!("mov rdi, rsp", "and rsp, -16", "call {start}", start = sym __oxrt_start);
        }

        unsafe extern "C" fn __oxrt_start(sp: *const u64) -> ! {
            unsafe { $crate::start(sp, $main) }
        }
    };
}
