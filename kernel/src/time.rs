//! Time keeping. The clock source is the timestamp counter (TSC), read in
//! a few cycles without leaving the CPU: nanoseconds since boot are
//! `(tsc - boot_tsc) * mult >> 32`. Its frequency comes from the hardware
//! where it states it (CPUID leaf 0x15, or the hypervisor's kvmclock), and
//! is measured against the PIT otherwise.
//!
//! Each CPU has its own counter. When another CPU starts, it measures how
//! far its counter is off the bootstrap CPU's, the way a clock is compared
//! across a network: the bootstrap CPU notes its time before and after
//! asking for the other's, and the other's reading belongs to the middle of
//! that round trip. A difference larger than the round trip is corrected,
//! so the clock does not go back when a thread moves between CPUs.
//!
//! Wall-clock time is the RTC's reading at boot plus the monotonic clock;
//! clock_settime moves it by changing that starting point.

use crate::interrupts::apic::pit_delay_us;
use crate::smp::{self, Cpu};
use core::arch::x86_64::{__cpuid, _mm_lfence, _rdtsc};
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use x86_64::registers::model_specific::Msr;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator};

pub const NSEC_PER_SEC: u64 = 1_000_000_000;

static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds per TSC cycle, in 32.32 fixed point.
static MULT: AtomicU64 = AtomicU64::new(0);
static BOOT_TSC: AtomicU64 = AtomicU64::new(0);
/// Wall-clock time at boot, in nanoseconds since the epoch.
static WALL_AT_BOOT: AtomicI64 = AtomicI64::new(0);

/// The counter, not reordered before earlier loads.
fn rdtsc() -> u64 {
    unsafe {
        _mm_lfence();
        _rdtsc()
    }
}

/// Nanoseconds since boot (CLOCK_MONOTONIC). The kernel does not preempt
/// itself, so the CPU cannot change between the offset and the counter.
pub fn now() -> u64 {
    let tsc = rdtsc().wrapping_add_signed(smp::cpu().tsc_offset.load(Ordering::Relaxed));
    let delta = tsc.saturating_sub(BOOT_TSC.load(Ordering::Relaxed));
    ((delta as u128 * MULT.load(Ordering::Relaxed) as u128) >> 32) as u64
}

/// The value this CPU's TSC has at `ns` nanoseconds since boot (for the
/// local APIC's TSC-deadline timer), rounded up so it is never early.
pub fn tsc_at(ns: u64) -> u64 {
    let cycles = ((ns as u128) << 32).div_ceil(MULT.load(Ordering::Relaxed).max(1) as u128) as u64;
    BOOT_TSC.load(Ordering::Relaxed).wrapping_add(cycles).wrapping_sub(smp::cpu().tsc_offset.load(Ordering::Relaxed) as u64)
}

/// Nanoseconds since the epoch (CLOCK_REALTIME).
pub fn realtime() -> u64 {
    WALL_AT_BOOT.load(Ordering::Relaxed).saturating_add(now() as i64).max(0) as u64
}

/// Sets the wall clock (clock_settime): the monotonic clock is unaffected.
pub fn set_realtime(ns: u64) {
    let ns = ns.min(i64::MAX as u64) as i64;
    WALL_AT_BOOT.store(ns.saturating_sub(now() as i64), Ordering::Relaxed);
}

/// Wall-clock time of the boot, in seconds since the epoch.
pub fn boot_time() -> u64 {
    WALL_AT_BOOT.load(Ordering::Relaxed).max(0) as u64 / NSEC_PER_SEC
}

/// Determines the TSC frequency and starts the clocks. Needs the frame
/// allocator and runs before interrupts are enabled.
pub fn init() {
    let (hz, source) = match from_cpuid().map(|hz| (hz, "CPUID")).or_else(|| from_kvmclock().map(|hz| (hz, "kvmclock"))) {
        Some(found) => found,
        None => (measure_with_pit(), "measured against the PIT"),
    };
    TSC_HZ.store(hz, Ordering::Relaxed);
    MULT.store(((NSEC_PER_SEC as u128) << 32).div_ceil(hz as u128) as u64, Ordering::Relaxed);
    BOOT_TSC.store(rdtsc(), Ordering::Relaxed);
    WALL_AT_BOOT.store((crate::drivers::rtc::read() * NSEC_PER_SEC) as i64, Ordering::Relaxed);
    const INVARIANT_TSC: u32 = 1 << 8;
    let invariant = __cpuid(0x8000_0000).eax >= 0x8000_0007 && __cpuid(0x8000_0007).edx & INVARIANT_TSC != 0;
    crate::printkln!(
        "[time] TSC at {}.{:03} MHz ({}){}",
        hz / 1_000_000,
        hz / 1000 % 1000,
        source,
        if invariant { "" } else { ", not marked invariant" }
    );
}

/// CPUID leaf 0x15: the core crystal's frequency and the TSC's ratio to it.
fn from_cpuid() -> Option<u64> {
    if __cpuid(0).eax < 0x15 {
        return None;
    }
    let r = __cpuid(0x15);
    (r.eax != 0 && r.ebx != 0 && r.ecx != 0).then(|| r.ecx as u64 * r.ebx as u64 / r.eax as u64)
}

/// The hypervisor's TSC scaling, as KVM publishes it for its paravirtual
/// clock: nanoseconds = ((tsc << shift) * mul) >> 32.
fn from_kvmclock() -> Option<u64> {
    const KVM_FEATURE_CLOCKSOURCE2: u32 = 1 << 3;
    const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
    let sig = __cpuid(0x4000_0000);
    let name: [u32; 3] = [sig.ebx, sig.ecx, sig.edx];
    if name != [0x4b4d_564b, 0x564b_4d56, 0x0000_004d] || sig.eax < 0x4000_0001 {
        return None;
    }
    if __cpuid(0x4000_0001).eax & KVM_FEATURE_CLOCKSOURCE2 == 0 {
        return None;
    }
    let frame = crate::memory::with_frames(|f| f.allocate_frame())?;
    let phys = frame.start_address().as_u64();
    let info = crate::memory::phys_to_virt(phys);
    unsafe { core::ptr::write_bytes(info, 0, 4096) };
    // struct pvclock_vcpu_time_info: version at 0, mul at 24, shift at 28.
    let read = |offset: usize| unsafe { core::ptr::read_volatile(info.add(offset) as *const u32) };
    let mut msr = Msr::new(MSR_KVM_SYSTEM_TIME_NEW);
    unsafe { msr.write(phys | 1) };
    let mut scale = None;
    for _ in 0..1_000_000 {
        let version = read(0);
        core::sync::atomic::fence(Ordering::Acquire);
        let (mul, shift) = (read(24), read(28) as u8 as i8);
        core::sync::atomic::fence(Ordering::Acquire);
        if version != 0 && version & 1 == 0 && read(0) == version && mul != 0 {
            scale = Some((mul, shift));
            break;
        }
        core::hint::spin_loop();
    }
    unsafe { msr.write(0) };
    crate::memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
    let (mul, shift) = scale?;
    let hz = ((NSEC_PER_SEC as u128) << 32) / mul as u128;
    let hz = if shift >= 0 { hz >> shift } else { hz << -shift };
    u64::try_from(hz).ok().filter(|&hz| hz > 0)
}

/// Counts TSC cycles over two PIT intervals; their difference cancels the
/// fixed cost of starting and polling the PIT. Best of three for each.
fn measure_with_pit() -> u64 {
    let cycles = |us: u64| {
        (0..3)
            .map(|_| {
                let start = rdtsc();
                pit_delay_us(us);
                rdtsc() - start
            })
            .min()
            .unwrap_or(0)
    };
    let (short, long) = (cycles(5_000), cycles(50_000));
    (long.saturating_sub(short) * 1_000_000 / 45_000).max(1)
}

// ------------------------------------------------- synchronizing CPUs

static SYNC_REQUEST: AtomicU64 = AtomicU64::new(0);
static SYNC_ANSWER: AtomicU64 = AtomicU64::new(0);
static SYNC_TSC: AtomicU64 = AtomicU64::new(0);
static SYNC_OFFSET: AtomicI64 = AtomicI64::new(0);
const SYNC_ROUNDS: u64 = 64;
const SYNC_DONE: u64 = u64::MAX;

/// The bootstrap CPU's side of measuring a CPU that just started and
/// waits in `sync_ap`. Returns the correction (in cycles) it applies.
pub fn sync_bsp() -> i64 {
    // (round trip, offset) of the round with the shortest round trip.
    let mut best = (u64::MAX, 0i64);
    for round in 1..=SYNC_ROUNDS {
        let before = rdtsc();
        SYNC_REQUEST.store(round, Ordering::Release);
        while SYNC_ANSWER.load(Ordering::Acquire) != round {
            core::hint::spin_loop();
        }
        let after = rdtsc();
        let theirs = SYNC_TSC.load(Ordering::Relaxed);
        let trip = after - before;
        if trip < best.0 {
            best = (trip, (before + trip / 2) as i64 - theirs as i64);
        }
    }
    // A difference within the measurement's uncertainty is none.
    let offset = if best.1.unsigned_abs() > best.0 { best.1 } else { 0 };
    SYNC_OFFSET.store(offset, Ordering::Relaxed);
    SYNC_REQUEST.store(SYNC_DONE, Ordering::Release);
    while SYNC_ANSWER.load(Ordering::Acquire) != SYNC_DONE {
        core::hint::spin_loop();
    }
    SYNC_REQUEST.store(0, Ordering::Relaxed);
    SYNC_ANSWER.store(0, Ordering::Relaxed);
    offset
}

/// A starting CPU's side: answers with its counter until told the result.
pub fn sync_ap(cpu: &Cpu) {
    let mut answered = 0;
    loop {
        let round = SYNC_REQUEST.load(Ordering::Acquire);
        if round == SYNC_DONE {
            break;
        }
        if round != 0 && round != answered {
            SYNC_TSC.store(rdtsc(), Ordering::Relaxed);
            SYNC_ANSWER.store(round, Ordering::Release);
            answered = round;
        }
        core::hint::spin_loop();
    }
    cpu.tsc_offset.store(SYNC_OFFSET.load(Ordering::Relaxed), Ordering::Relaxed);
    SYNC_ANSWER.store(SYNC_DONE, Ordering::Release);
}

// ------------------------------------------------------------ CPU time

/// Divides `total` nanoseconds of run time into user and system time in
/// the ratio of the timer ticks that hit each mode (as Linux does: the
/// sum is exact, the split is sampled).
pub fn split(total: u64, user_ticks: u64, system_ticks: u64) -> (u64, u64) {
    if system_ticks == 0 {
        return (total, 0);
    }
    if user_ticks == 0 {
        return (0, total);
    }
    let system = (total as u128 * system_ticks as u128 / (user_ticks + system_ticks) as u128) as u64;
    (total - system, system)
}
