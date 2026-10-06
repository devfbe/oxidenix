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
    pub const SCHED_YIELD: u64 = 24;
    pub const GETPID: u64 = 39;
    pub const EXIT_GROUP: u64 = 231;
    pub const IOPERM: u64 = 173;
    pub const CLOCK_GETTIME: u64 = 228;
    /// (name, name length, argument) -> service id
    pub const IPC_REGISTER: u64 = 1000;
    /// (buffer, buffer length, &request id) -> message length
    pub const IPC_RECEIVE: u64 = 1001;
    /// (request id, buffer, length)
    pub const IPC_REPLY: u64 = 1002;
}

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

/// Milliseconds since boot (monotonic, timer resolution).
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
    match syscall(sys::IPC_REGISTER, [name.as_ptr() as u64, name.len() as u64, arg, 0, 0, 0]) {
        e if e < 0 => Err(e),
        id => Ok(id as u64),
    }
}

/// Waits for the next request; returns (request id, message length).
pub fn ipc_receive(buf: &mut [u8]) -> Result<(u64, usize), i64> {
    let mut id = 0u64;
    let r = syscall(sys::IPC_RECEIVE, [buf.as_mut_ptr() as u64, buf.len() as u64, &mut id as *mut u64 as u64, 0, 0, 0]);
    if r < 0 { Err(r) } else { Ok((id, r as usize)) }
}

pub fn ipc_reply(id: u64, msg: &[u8]) -> Result<(), i64> {
    match syscall(sys::IPC_REPLY, [id, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0]) {
        0 => Ok(()),
        e => Err(e),
    }
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
