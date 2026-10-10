//! The Linux server (docs/design/linux-server.md). It runs on the threads
//! of the Linux programs it serves, in their address spaces' normal view,
//! and handles what the programs trap into: their system calls (every one:
//! the kernel implements none, R9; one the server does not implement is
//! ENOSYS), their exceptions (as signals), and kicks (a signal or a stop
//! for the thread). Before a thread's program runs again its signals are
//! delivered (`signal::deliver`).
//!
//! Invariants the kernel relies on: no FPU/SSE (the target has none) and
//! no FS or GS base, so the program's FPU registers and TLS pointer stay in
//! the CPU while the server runs (signal frames save and restore them with
//! fxsave and fxrstor); no lock of the server is held when a thread enters
//! its program (a killed thread exits there).

#![no_std]
#![no_main]
#![feature(allocator_ext)]

extern crate alloc;

mod chantest;
mod console;
mod datafile;
mod datafs;
mod devices;
mod disktest;
mod epoll;
mod eventfd;
mod exec;
mod fdtable;
mod files;
mod fsclient;
mod futex;
mod heap;
mod ids;
mod inet;
mod inetcalls;
mod initramfs;
mod inotify;
mod local;
mod mm;
mod namespace;
mod netclient;
mod netdev;
mod netlink;
mod pathfile;
mod paths;
mod pipe;
mod process;
mod poll;
mod procfile;
mod procfs;
mod pty;
mod records;
mod ringclient;
mod sched;
mod scm;
mod signal;
mod sockcalls;
mod sync;
mod system;
mod time;
mod timer;
mod tmpfile;
mod tmpfs;
mod tty;
mod unix;
mod usercopy;

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};
use restricted::*;

#[global_allocator]
static HEAP: heap::ServerHeap = heap::ServerHeap::new();

const PAGE: u64 = 4096;
const ENOSPC: i64 = 28;
const ENOSYS: i64 = 38;
const PROT_READ: u64 = 1;
const PROT_RW: u64 = 3;
const SYS_IO_URING_SETUP: u64 = 425;
const SYS_IO_URING_ENTER: u64 = 426;
const SYS_IO_URING_REGISTER: u64 = 427;

/// Whether the kernel runs the self-tests (`SYS_TEST_MODE`), asked once:
/// 0 not yet known, 1 no, 2 yes.
static TEST_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Whether the test hooks (`TEST_*`) answer: in test mode only.
fn test_mode() -> bool {
    match TEST_MODE.load(Ordering::Relaxed) {
        0 => {
            let on = syscall(SYS_TEST_MODE, [0; 6]) == 1;
            TEST_MODE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        known => known == 2,
    }
}

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

/// A kernel call that reads and writes the thread's `State` (restricted_enter):
/// the pointer goes into the asm, so the compiler writes back what the
/// server set in `state` before and reads it again after (a `&mut` alone would let it
/// keep fields in registers across the call).
pub(crate) fn state_call(state: &mut State, nr: u64, a: [u64; 6]) -> i64 {
    let ret: i64;
    let ptr = state as *mut State;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr as i64 => ret,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2], in("r10") a[3], in("r8") a[4], in("r9") a[5],
            in("r12") ptr,
            out("rcx") _, out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Each thread of a Linux program starts here, on its own server stack,
/// with its `State` (the program's registers) at `state`; so do the
/// instance's service threads (`role`). A program's thread made by clone
/// gets its creator's `cookie` (`process::Birth`) and its key.
#[unsafe(no_mangle)]
pub extern "C" fn _start(state: *mut State, role: u64, cookie: u64, key: u64) -> ! {
    local::start(role);
    usercopy::register();
    match role {
        ROLE_PAGER => pager(),
        ROLE_WORKER => scm::worker(),
        ROLE_NET => netclient::thread(),
        ROLE_TIMER => timer::thread(),
        _ => {}
    }
    let s = unsafe { &mut *state };
    if role == ROLE_INIT {
        // The tree's first thread: pid 1, its standard descriptors on the
        // console, then the program the kernel started the tree with.
        process::register_init(key);
        console::setup_stdio();
        exec::init(s);
    } else {
        process::start_thread(cookie, key);
    }
    if let Some(status) = local::pending_exit() {
        process::die(status);
    }
    // Its signals before its first instruction (a stop, a kill of its
    // process, a signal sent while it was made).
    signal::deliver(s, None);
    serve(s)
}

/// The loop of a program's thread: the program runs until it traps, the
/// server handles the trap, delivers the thread's signals, and the program
/// runs again.
fn serve(s: &mut State) -> ! {
    loop {
        let reason = state_call(s, SYS_RESTRICTED_ENTER, [0; 6]);
        if reason < 0 {
            // The kernel refuses the registers (they never come from the
            // program unchecked): the process cannot go on.
            process::die(signal::SIGSEGV as i32);
        }
        match reason as u64 {
            REASON_SYSCALL => {
                let nr = s.rax;
                let result = dispatch(s);
                s.rax = result as u64;
                // /data inodes the call let go of go now, before it returns
                // (an unlink's blocks are free when it returns).
                datafs::reap();
                // A failure past an execve's point of no return ends the process here,
                // where nothing of the call is left on the stack.
                if let Some(status) = local::pending_exit() {
                    process::die(status);
                }
                // The fast path: the program runs on, unless the call was
                // interrupted or changed the thread's mask (a kick makes
                // restricted_enter come back at once).
                if signal::interrupted(result) || nr == signal::SYS_RT_SIGRETURN || local::get().flags.load(Ordering::Relaxed) & local::RESTORE_MASK != 0 {
                    signal::deliver(s, Some((nr, result)));
                }
            }
            REASON_KICK => signal::deliver(s, None),
            // Killed (by the process model or the kernel): the thread ends here, on a clean
            // stack, letting go of what it holds first.
            REASON_EXIT => process::exit_killed(),
            REASON_EXCEPTION => {
                signal::exception(s);
                signal::deliver(s, None);
            }
            _ => {}
        }
    }
}

/// Handles the program's system call in `s`: its result.
fn dispatch(s: &mut State) -> i64 {
    if let Some(result) = mm::handle(s)
        .or_else(|| process::handle(s))
        .or_else(|| signal::handle(s))
        .or_else(|| exec::handle(s))
        .or_else(|| timer::handle(s))
        .or_else(|| time::handle(s))
        .or_else(|| fdtable::handle(s))
        .or_else(|| files::handle(s))
        .or_else(|| poll::handle(s))
        .or_else(|| epoll::handle(s))
        .or_else(|| paths::handle(s))
        .or_else(|| sched::handle(s))
        .or_else(|| ids::handle(s))
        .or_else(|| futex::handle(s))
        .or_else(|| system::handle(s))
        .or_else(|| sockcalls::handle(s))
    {
        return result;
    }
    match s.rax {
        // The test hooks reach beyond the caller (the instance's test
        // objects, the test service, /data files, the server's heap and
        // locks): only for the self-tests.
        TEST_MAP..=TEST_HEAP_STATS if !test_mode() => -ENOSYS,
        TEST_MAP..=TEST_CACHED => test(s.rax, s.rdi),
        TEST_MKWRITE_FAIL => datafs::fail_next_mkwrite(s.rdi),
        TEST_FILL_GONE => datafs::fail_next_fill(s.rdi),
        TEST_SLEEP_LOCKED => match TEST_SLEEP_LOCK.lock() {
            Ok(_held) => {
                let until = (syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64).saturating_add(s.rdi);
                syscall(SYS_SLEEP_UNTIL, [until, 0, 0, 0, 0, 0]);
                0
            }
            Err(e) => -e,
        },
        TEST_SERVER_FAIL => {
            let _plain = COUNTER.lock();
            let _sleeping = TEST_SLEEP_LOCK.lock();
            // The server's own code fails with both held.
            unsafe { core::arch::asm!("ud2", options(nomem, nostack)) };
            0
        }
        // The kernel's to answer, with the name in the server's memory.
        TEST_KILL_SERVER => match usercopy::read_cstr(s.rdi) {
            Ok(name) => syscall(TEST_KILL_SERVER, [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0]),
            Err(e) => -e,
        },
        TEST_SERVER_TICKS => match usercopy::read_cstr(s.rdi) {
            Ok(name) => syscall(TEST_SERVER_TICKS, [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0]),
            Err(e) => -e,
        },
        TEST_HOST => syscall(TEST_HOST, [s.rdi, 0, 0, 0, 0, 0]),
        TEST_FUTEX_WATCH => syscall(TEST_FUTEX_WATCH, [s.rdi, s.rsi, 0, 0, 0, 0]),
        TEST_HEAP_STATS => test(s.rax, s.rdi),
        // Not offered, as by a Linux built without io_uring: libuv (and
        // so Node.js) probes io_uring_setup at start and uses epoll
        // instead. Answered here, so the kernel does not log them as
        // unknown calls.
        SYS_IO_URING_SETUP | SYS_IO_URING_ENTER | SYS_IO_URING_REGISTER => -ENOSYS,
        nr if nr >= FIRST_NON_LINUX => -ENOSYS,
        // Nothing passes through (R9): a Linux call the server does not
        // implement is ENOSYS, said on the console (scripted runs read
        // which calls a program misses from there).
        nr => {
            // Once per number and instance: a program that probes a call in
            // a loop does not flood the console.
            static SAID: [AtomicU64; (FIRST_NON_LINUX as usize).div_ceil(64)] = [const { AtomicU64::new(0) }; (FIRST_NON_LINUX as usize).div_ceil(64)];
            let bit = 1u64 << (nr % 64);
            if SAID[(nr / 64) as usize].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                let text = alloc::format!("syscall {} not implemented", nr);
                syscall(SYS_SERVER_LOG, [text.as_ptr() as u64, text.len() as u64, 0, 0, 0, 0]);
            }
            -ENOSYS
        }
    }
}

/// A page request of an `EVENT_PAGE`.
struct PagerRequest {
    key: u64,
    offset: u64,
}

/// The sleeping lock of `TEST_SLEEP_LOCKED` and `TEST_SERVER_FAIL`.
static TEST_SLEEP_LOCK: sync::SleepLock = sync::SleepLock::new(());

/// The object of the last TEST_MAP (one per instance, as test calls go).
static TEST_OBJECT: AtomicU64 = AtomicU64::new(0);
/// The paged object of TEST_PAGED, and how many pages the pager supplied.
static TEST_PAGED_OBJECT: AtomicU64 = AtomicU64::new(0);
/// Held by the pager across a supply and its count (bounded work: the
/// supply copies from the pager's own memory and waits for nothing), so a
/// thread that saw a page come and then asks (TEST_SUPPLIED) finds it
/// counted: the supply wakes the page's waiters, which may run and ask
/// before the pager is back from it.
static SUPPLIED: sync::Mutex<u64> = sync::Mutex::new(0);
/// TEST_PAGED_STUCK's object (its handle, the latest run's).
static TEST_STUCK_OBJECT: AtomicU64 = AtomicU64::new(0);
/// The key the test's paged object goes by; +1: never answered.
const TEST_KEY: u64 = 0x7e57;
/// The keys of TEST_PAGED_FAIL's objects, one of its own each, from here
/// up (below `datafs::KEY_BASE`).
const FAIL_KEY_BASE: u64 = 1 << 31;
static NEXT_FAIL_KEY: AtomicU64 = AtomicU64::new(FAIL_KEY_BASE);
/// TEST_PAGED_FAIL's objects by key: the handle, and whether the pager
/// failed the object's first request yet. Each object fails once (every
/// run of the test, not only the instance's first), and a request is
/// answered from its own object. They stay for the instance (a page each,
/// and only the tests make them), so no handle is ever closed and reused
/// under a request in flight.
static FAIL_OBJECTS: sync::Mutex<BTreeMap<u64, (u64, bool)>> = sync::Mutex::new(BTreeMap::new());

/// The instance's service thread (the pager thread): supplies the pages
/// threads wait for (/data's from the disk, `datafs`; the tests' paged
/// objects: page n of the first reads "paged n"), writes /data's dirty
/// files back (when they have been dirty a while, when the kernel asks,
/// when the instance ends), drops the server's files whose last
/// descriptor went, takes back the holds the kernel released, and ends
/// the threads and processes whose end the kernel reports (`process`).
fn pager() -> ! {
    loop {
        datafs::reap();
        let mut event = Event::default();
        let deadline = datafs::next_deadline();
        if syscall(SYS_EVENT_WAIT, [&mut event as *mut Event as u64, deadline, 0, 0, 0, 0]) < 0 {
            continue;
        }
        match event.kind {
            EVENT_RELEASE => {
                // Bit 0 tells a tmpfs file's hold, bit 1 a /data file's.
                if event.a & 1 == 1 {
                    tmpfs::released(event.a);
                } else if event.a & datafs::HOLD_TAG != 0 {
                    datafs::released(event.a);
                }
                tmpfs::release_handled();
                continue;
            }
            EVENT_THREAD_EXIT => {
                // A program's thread is gone: its process may have ended.
                process::thread_ended(event.a);
                continue;
            }
            EVENT_DIRTY => {
                datafs::dirtied(event.a);
                continue;
            }
            EVENT_TIMER => {
                datafs::write_dirty(false);
                continue;
            }
            EVENT_WRITEBACK => {
                // Memory is short of clean pages: everything dirty goes.
                datafs::write_dirty(true);
                continue;
            }
            EVENT_CLOSING => {
                // What the instance's sockets still had to send reaches
                // netd (their closes hand it over) before the instance goes:
                // the worker has closed the ended processes' tables first.
                fdtable::settle();
                netclient::settle();
                datafs::ending();
                continue;
            }
            EVENT_MKWRITE => {
                datafs::mkwrite(event.a, event.b);
                continue;
            }
            EVENT_CONSOLE => {
                // Typed on the keyboard: through the console's line
                // discipline (echo, signals).
                console::input();
                continue;
            }
            EVENT_CONSOLE_LOST => {
                console::lost();
                continue;
            }
            EVENT_SERVICE_GONE => {
                // diskfs keeps our unlinked open files for us until we name them again.
                // (On the worker: the reconnection waits for locks, the pager must not.)
                datafs::reconnect_later();
                continue;
            }
            EVENT_SYNC => {
                // Another instance's sync(2), or a reboot.
                datafs::sync_event(event.a);
                continue;
            }
            _ => {}
        }
        let request = PagerRequest { key: event.a, offset: event.b };
        if request.key >= datafs::KEY_BASE {
            // A /data file's page.
            datafs::page(request.key, request.offset);
            continue;
        }
        if (FAIL_KEY_BASE..datafs::KEY_BASE).contains(&request.key) {
            let Some((handle, failed)) = FAIL_OBJECTS.lock().get_mut(&request.key).map(|o| (o.0, core::mem::replace(&mut o.1, true))) else {
                continue;
            };
            if !failed {
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
        let mut supplied = SUPPLIED.lock();
        if syscall(SYS_MO_SUPPLY, [handle, request.offset, page.as_ptr() as u64, PAGE, 0, 0]) == 1 {
            *supplied += 1;
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
            let mapped = syscall(SYS_MO_MAP, [h as u64, addr, len, 0, PROT_RW, MO_SHARED | MO_FIXED]);
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
            let r = syscall(SYS_MO_MAP, [h as u64, addr, PAGE, 0, PROT_RW, MO_SHARED | MO_FIXED]);
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
            let r = syscall(SYS_MO_MAP, [h as u64, addr, 4 * PAGE, 0, PROT_READ, MO_SHARED | MO_FIXED]);
            if r < 0 { r } else { 0 }
        }
        TEST_SUPPLIED => *SUPPLIED.lock() as i64,
        TEST_PAGED_FAIL => {
            let key = NEXT_FAIL_KEY.fetch_add(1, Ordering::Relaxed);
            if key >= datafs::KEY_BASE {
                return -ENOSPC;
            }
            let h = syscall(SYS_MO_CREATE_PAGED, [1, key, 0, 0, 0, 0]);
            if h < 0 {
                return h;
            }
            // Known before it is mapped, so before any request for it.
            FAIL_OBJECTS.lock().insert(key, (h as u64, false));
            let r = syscall(SYS_MO_MAP, [h as u64, addr, PAGE, 0, PROT_READ, MO_SHARED | MO_FIXED]);
            if r < 0 { r } else { 0 }
        }
        TEST_ALLOC => test_alloc(addr) as i64,
        TEST_HEAP_STATS => {
            let stats = HEAP.stats();
            let mut bytes = [0u8; 32];
            for (i, v) in [stats.committed, stats.in_use, stats.free, stats.decommitted].into_iter().enumerate() {
                bytes[i * 8..i * 8 + 8].copy_from_slice(&(v as u64).to_le_bytes());
            }
            match usercopy::to_program(addr, &bytes) {
                Ok(()) => 0,
                Err(e) => -e,
            }
        }
        TEST_CHANNEL => chantest::run(addr),
        TEST_DISKRING => disktest::run(addr),
        TEST_CACHED => datafs::test(addr),
        TEST_FS_VALUE => records::test_value(addr),
        TEST_FS_RECORDS => records::test_records(addr != 0),
        TEST_USERCOPY => match usercopy::to_program(addr, b"usercopy") {
            Ok(()) => 0,
            Err(e) => -e,
        },
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
        TEST_PAGED_STUCK if addr == 0 => {
            // Let go of: the kernel ends the waits for its page.
            match TEST_STUCK_OBJECT.swap(0, Ordering::Relaxed) {
                0 => -namespace::ENOENT,
                h => syscall(SYS_HANDLE_CLOSE, [h, 0, 0, 0, 0, 0]),
            }
        }
        TEST_PAGED_STUCK => {
            // A key the pager does not answer.
            let h = syscall(SYS_MO_CREATE_PAGED, [1, TEST_KEY + 1, 0, 0, 0, 0]);
            if h < 0 {
                return h;
            }
            let r = syscall(SYS_MO_MAP, [h as u64, addr, PAGE, 0, PROT_READ, MO_SHARED | MO_FIXED]);
            // Kept (the previous run's goes): at its last handle the
            // kernel would end the waits for it (no answer could come).
            let old = TEST_STUCK_OBJECT.swap(h as u64, Ordering::Relaxed);
            if old != 0 {
                syscall(SYS_HANDLE_CLOSE, [old, 0, 0, 0, 0, 0]);
            }
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
