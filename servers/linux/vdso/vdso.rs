//! The vDSO: the shared object the Linux server maps into every Linux program (ADR 0014),
//! Linux's x86_64 interface (`linux-vdso.so.1`, symbol version `LINUX_2.6`):
//! `__vdso_clock_gettime`, `__vdso_gettimeofday`, `__vdso_time`, `__vdso_clock_getres` and
//! `__vdso_getcpu`, with their unprefixed weak aliases. It answers from the kernel's clock
//! page (`clockpage`, mapped read-only just below it as `[vvar]`) and the CPU's RDTSC,
//! RDTSCP and RDPID, without entering the kernel; what it cannot answer (the CPU-time
//! clocks, the alarm clocks, invalid clocks, a time zone, a page without a reading) is the
//! real system call, which goes to the server as any other, exactly as Linux's vDSO falls
//! back to its system calls.
//!
//! Built by `servers/linux/build.rs` (rustc for the host's Linux target, position-independent,
//! linked by rust-lld with `vdso.lds`) and embedded in the server. It must stay free of
//! relocations (nothing relocates it: the build checks), of writable data and of calls into
//! code it does not contain (`--no-undefined`): every helper inlines, nothing panics. The
//! clock page is reached RIP-relative through `__vdso_clock_page`, which the linker script
//! puts one page below the image.

#![no_std]
// The clock page's file is a crate root of its own (its `no_std` is meant for it there).
#![allow(unused_attributes)]

#[path = "../../../crates/clockpage/src/lib.rs"]
#[allow(dead_code)]
mod clockpage;

use clockpage::{ClockPage, Counter, Reading, AUX_CPU_MASK, HAS_RDPID, HAS_RDTSCP};
use core::arch::{asm, global_asm, x86_64};
use core::sync::atomic::Ordering;

const SYS_GETTIMEOFDAY: u64 = 96;
const SYS_TIME: u64 = 201;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_CLOCK_GETRES: u64 = 229;
const SYS_GETCPU: u64 = 309;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_TAI: i32 = 11;

/// `struct timespec` and `struct timeval`: seconds and the fraction.
#[repr(C)]
pub struct Pair {
    sec: i64,
    frac: i64,
}

/// The clock page, one page below the image.
#[inline(always)]
fn page() -> &'static ClockPage {
    let p: *const ClockPage;
    unsafe { asm!("lea {}, [rip + __vdso_clock_page]", out(reg) p, options(pure, nomem, nostack, preserves_flags)) };
    unsafe { &*p }
}

/// The CPU's counter.
struct Tsc;

impl Counter for Tsc {
    #[inline(always)]
    fn tsc(&self) -> u64 {
        unsafe {
            x86_64::_mm_lfence();
            x86_64::_rdtsc()
        }
    }

    #[inline(always)]
    fn tsc_aux(&self) -> (u64, u32) {
        let mut aux = 0u32;
        let tsc = unsafe { x86_64::__rdtscp(&mut aux) };
        (tsc, aux)
    }
}

#[inline(always)]
unsafe fn syscall(nr: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr as i64 => ret,
            in("rdi") a, in("rsi") b, in("rdx") c,
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// The clocks this object answers, as Linux's vDSO: the high-resolution ones (realtime,
/// monotonic, boot time, TAI), the coarse ones and the raw one. Nothing suspends, the
/// coarse and raw clocks are the precise ones and TAI is the wall clock, as the server
/// has them (`servers/linux/src/time.rs`).
#[inline(always)]
fn reading_of(clock: i32, r: Reading) -> Option<u64> {
    match clock {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_TAI => Some(r.wall),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => Some(r.mono),
        _ => None,
    }
}

#[inline(always)]
fn answers(clock: i32) -> bool {
    matches!(clock, CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_TAI | CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME)
}

/// clock_gettime(clock, ts): 0, or the system call's result.
#[no_mangle]
pub unsafe extern "C" fn __vdso_clock_gettime(clock: i32, ts: *mut Pair) -> i32 {
    if answers(clock) {
        if let Some(ns) = page().read(&Tsc).and_then(|r| reading_of(clock, r)) {
            let (sec, nsec) = clockpage::split(ns);
            unsafe { ts.write(Pair { sec: sec as i64, frac: nsec as i64 }) };
            return 0;
        }
    }
    unsafe { syscall(SYS_CLOCK_GETTIME, clock as i64 as u64, ts as u64, 0) as i32 }
}

/// gettimeofday(tv, tz): the wall clock in microseconds; a time zone is the server's (the
/// system call answers when `tz` is given).
#[no_mangle]
pub unsafe extern "C" fn __vdso_gettimeofday(tv: *mut Pair, tz: *mut u8) -> i32 {
    if tz.is_null() {
        if tv.is_null() {
            return 0;
        }
        if let Some(r) = page().read(&Tsc) {
            let (sec, nsec) = clockpage::split(r.wall);
            unsafe { tv.write(Pair { sec: sec as i64, frac: (nsec / 1000) as i64 }) };
            return 0;
        }
    }
    unsafe { syscall(SYS_GETTIMEOFDAY, tv as u64, tz as u64, 0) as i32 }
}

/// time(t): the wall clock's seconds, also stored at `t` if given.
#[no_mangle]
pub unsafe extern "C" fn __vdso_time(t: *mut i64) -> i64 {
    if let Some(r) = page().read(&Tsc) {
        let sec = clockpage::split(r.wall).0 as i64;
        if !t.is_null() {
            unsafe { t.write(sec) };
        }
        return sec;
    }
    unsafe { syscall(SYS_TIME, t as u64, 0, 0) }
}

/// clock_getres(clock, res): every clock this object answers counts in nanoseconds;
/// another is the system call's.
#[no_mangle]
pub unsafe extern "C" fn __vdso_clock_getres(clock: i32, res: *mut Pair) -> i32 {
    if answers(clock) {
        if !res.is_null() {
            unsafe { res.write(Pair { sec: 0, frac: 1 }) };
        }
        return 0;
    }
    unsafe { syscall(SYS_CLOCK_GETRES, clock as i64 as u64, res as u64, 0) as i32 }
}

/// getcpu(cpu, node, cache): the CPU and node from TSC_AUX (RDPID, else RDTSCP), as the
/// kernel set it for each CPU; without either the system call.
#[no_mangle]
pub unsafe extern "C" fn __vdso_getcpu(cpu: *mut u32, node: *mut u32, cache: *mut u8) -> i64 {
    let features = page().features.load(Ordering::Relaxed);
    let aux = if features & HAS_RDPID != 0 {
        let aux: u64;
        unsafe { asm!("rdpid {}", out(reg) aux, options(nomem, nostack, preserves_flags)) };
        aux as u32
    } else if features & HAS_RDTSCP != 0 {
        Tsc.tsc_aux().1
    } else {
        return unsafe { syscall(SYS_GETCPU, cpu as u64, node as u64, cache as u64) };
    };
    if !cpu.is_null() {
        unsafe { cpu.write(aux & AUX_CPU_MASK) };
    }
    if !node.is_null() {
        unsafe { node.write(aux >> 12) };
    }
    0
}

// The unprefixed names, weak aliases as in Linux's vDSO.
global_asm!(
    ".weak clock_gettime",
    ".type clock_gettime, @function",
    ".set clock_gettime, __vdso_clock_gettime",
    ".weak gettimeofday",
    ".type gettimeofday, @function",
    ".set gettimeofday, __vdso_gettimeofday",
    ".weak time",
    ".type time, @function",
    ".set time, __vdso_time",
    ".weak clock_getres",
    ".type clock_getres, @function",
    ".set clock_getres, __vdso_clock_getres",
    ".weak getcpu",
    ".type getcpu, @function",
    ".set getcpu, __vdso_getcpu",
);
