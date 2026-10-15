//! Time keeping. The clock source is the timestamp counter (TSC), read in
//! a few cycles without leaving the CPU: nanoseconds since boot are
//! `(tsc - boot_tsc) * mult >> 32`. Its frequency is taken where it is
//! stated (ADR 0016): from a hypervisor (kvmclock, or the timing leaf
//! 0x4000_0010), else from the CPU (CPUID leaf 0x15, with the crystal's
//! frequency from leaf 0x15, the CPU model or leaf 0x16); else it is
//! measured against a reference clock (`refclock`: the HPET, the ACPI PM
//! timer, last the PIT). Every busy-wait of the kernel (`delay_us`) and the
//! local APIC timer's calibration run on the TSC, so nothing depends on the
//! PIT, which recent chipsets may no longer clock.
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
//!
//! **The clock page** (`clockpage`, ADR 0014): what a reader needs to compute
//! these clocks itself (the scale, the boot counter, the wall clock's
//! starting point, each CPU's offset) is published in one page that the Linux
//! server maps read-only into its programs (`SYS_CLOCK_PAGE`, under its vDSO),
//! changed under a sequence count whenever it changes (`set_realtime`, a CPU's
//! offset). Each CPU's `IA32_TSC_AUX` holds its index, so RDTSCP and RDPID in
//! user mode tell which CPU a counter value came from. The kernel implements
//! no clock call: it only keeps the page current.

use crate::smp::{self, Cpu};
use crate::sync::IrqSpinLock;
use alloc::sync::Arc;
use clockpage::ClockPage;
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
pub fn rdtsc() -> u64 {
    unsafe {
        _mm_lfence();
        _rdtsc()
    }
}

/// Nanoseconds since boot (CLOCK_MONOTONIC). The kernel does not preempt
/// itself, so the CPU cannot change between the offset and the counter.
/// (The clock page's formula, so the vDSO's readings agree with it.)
pub fn now() -> u64 {
    let tsc = rdtsc().wrapping_add_signed(smp::cpu().tsc_offset.load(Ordering::Relaxed));
    clockpage::mono(tsc, BOOT_TSC.load(Ordering::Relaxed), MULT.load(Ordering::Relaxed))
}

/// Busy-waits at least `us` microseconds (after `init`), for hardware that
/// needs a pause (the start-up of the other CPUs).
pub fn delay_us(us: u64) {
    let end = now() + us * 1000;
    while now() < end {
        core::hint::spin_loop();
    }
}

/// The value this CPU's TSC has at `ns` nanoseconds since boot (for the
/// local APIC's TSC-deadline timer), rounded up so it is never early.
pub fn tsc_at(ns: u64) -> u64 {
    let cycles = ((ns as u128) << 32).div_ceil(MULT.load(Ordering::Relaxed).max(1) as u128) as u64;
    BOOT_TSC.load(Ordering::Relaxed).wrapping_add(cycles).wrapping_sub(smp::cpu().tsc_offset.load(Ordering::Relaxed) as u64)
}

/// Nanoseconds since the epoch (CLOCK_REALTIME).
pub fn realtime() -> u64 {
    clockpage::wall(WALL_AT_BOOT.load(Ordering::Relaxed), now())
}

/// Sets the wall clock (clock_settime): the monotonic clock is unaffected.
/// The clock page has the new starting point before this returns.
pub fn set_realtime(ns: u64) {
    let ns = ns.min(i64::MAX as u64) as i64;
    let _writer = PAGE_WRITER.lock();
    let at_boot = ns.saturating_sub(now() as i64);
    WALL_AT_BOOT.store(at_boot, Ordering::Relaxed);
    if let Some((_, page)) = CLOCK_PAGE.get() {
        page.update(|p| p.wall_at_boot.store(at_boot, Ordering::Relaxed));
    }
}

// ---------------------------------------------------------- the clock page

/// The clock page: its memory object (the handle the Linux server gets) and
/// its contents in the direct map.
static CLOCK_PAGE: spin::Once<(Arc<crate::fs::cache::PageCache>, &'static ClockPage)> = spin::Once::new();
/// Serializes the page's writers (and the wall clock's with them).
static PAGE_WRITER: IrqSpinLock<()> = IrqSpinLock::new(());

const IA32_TSC_AUX: u32 = 0xc000_0103;

/// What the CPUs offer a reader in user mode (`clockpage::HAS_*`): RDTSCP
/// (CPUID 0x8000_0001 EDX bit 27), RDPID (CPUID 7 ECX bit 22).
fn reader_features() -> u32 {
    let mut features = 0;
    if __cpuid(0x8000_0000).eax >= 0x8000_0001 && __cpuid(0x8000_0001).edx & 1 << 27 != 0 {
        features |= clockpage::HAS_RDTSCP;
    }
    if __cpuid(0).eax >= 7 && __cpuid(7).ecx & 1 << 22 != 0 {
        features |= clockpage::HAS_RDPID;
    }
    features
}

/// Puts the calling CPU's index into its TSC_AUX, where RDTSCP and RDPID
/// read it (Linux's encoding: the CPU in bits 0-11, the node, 0, above).
fn set_tsc_aux(cpu: &Cpu) {
    if reader_features() != 0 {
        unsafe { Msr::new(IA32_TSC_AUX).write(cpu.index as u64 & clockpage::AUX_CPU_MASK as u64) };
    }
}

/// How a reader takes the counter, from the CPUs' offsets: plain RDTSC when
/// none is corrected, else RDTSCP with the offset of the CPU it names, or
/// not at all without RDTSCP.
fn reader_mode(page: &ClockPage) -> u32 {
    if page.tsc_offset.iter().all(|o| o.load(Ordering::Relaxed) == 0) {
        clockpage::MODE_TSC
    } else if page.features.load(Ordering::Relaxed) & clockpage::HAS_RDTSCP != 0 {
        clockpage::MODE_TSC_PER_CPU
    } else {
        clockpage::MODE_NONE
    }
}

/// Makes the clock page and publishes the clocks in it (after `init`, with
/// the kernel heap; before the other CPUs start, which add their offsets).
pub fn init_page() {
    const _: () = assert!(smp::MAX_CPUS <= clockpage::CPUS);
    set_tsc_aux(smp::cpu());
    let made = crate::fs::cache::PageCache::anonymous(1).ok().and_then(|cache| {
        // The page's frame stays mapped by this reference for good.
        let frame = cache.map_page(0).ok()?;
        let page = unsafe { &*(crate::memory::phys_to_virt(frame.start_address().as_u64()) as *const ClockPage) };
        Some((cache, page))
    });
    let Some((cache, page)) = made else {
        return crate::printkln!("[time] no memory for the clock page; programs read the clocks by system call");
    };
    let _writer = PAGE_WRITER.lock();
    page.update(|p| {
        p.mult.store(MULT.load(Ordering::Relaxed), Ordering::Relaxed);
        p.boot_tsc.store(BOOT_TSC.load(Ordering::Relaxed), Ordering::Relaxed);
        p.wall_at_boot.store(WALL_AT_BOOT.load(Ordering::Relaxed), Ordering::Relaxed);
        p.features.store(reader_features(), Ordering::Relaxed);
        p.tsc_offset[0].store(smp::cpu().tsc_offset.load(Ordering::Relaxed), Ordering::Relaxed);
        p.mode.store(reader_mode(p), Ordering::Relaxed);
    });
    CLOCK_PAGE.call_once(|| (cache, page));
}

/// The clock page's memory object (None if there is none), for the Linux
/// server to map read-only into its programs.
pub fn clock_page() -> Option<Arc<crate::fs::cache::PageCache>> {
    CLOCK_PAGE.get().map(|(cache, _)| cache.clone())
}

/// Publishes CPU `cpu`'s counter offset (and how a reader takes the
/// counter now).
fn publish_offset(cpu: &Cpu) {
    let Some((_, page)) = CLOCK_PAGE.get() else { return };
    let _writer = PAGE_WRITER.lock();
    page.update(|p| {
        p.tsc_offset[cpu.index].store(cpu.tsc_offset.load(Ordering::Relaxed), Ordering::Relaxed);
        p.mode.store(reader_mode(p), Ordering::Relaxed);
    });
}

/// Wall-clock time of the boot, in seconds since the epoch.
/// The TSC's frequency in Hz (what /proc/cpuinfo's `cpu MHz` shows).
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

pub fn boot_time() -> u64 {
    WALL_AT_BOOT.load(Ordering::Relaxed).max(0) as u64 / NSEC_PER_SEC
}

/// Determines the TSC frequency and starts the clocks. Needs the frame
/// allocator and runs before interrupts are enabled.
pub fn init() {
    let (hz, source) = from_hypervisor()
        .or_else(from_cpuid)
        .or_else(crate::drivers::refclock::measure_tsc)
        .expect("no clock to measure the TSC against (no HPET, ACPI PM timer or PIT)");
    TSC_HZ.store(hz, Ordering::Relaxed);
    MULT.store(((NSEC_PER_SEC as u128) << 32).div_ceil(hz as u128) as u64, Ordering::Relaxed);
    BOOT_TSC.store(rdtsc(), Ordering::Relaxed);
    WALL_AT_BOOT.store((crate::drivers::rtc::read() * NSEC_PER_SEC) as i64, Ordering::Relaxed);
    const INVARIANT_TSC: u32 = 1 << 8;
    let invariant = __cpuid(0x8000_0000).eax >= 0x8000_0007 && __cpuid(0x8000_0007).edx & INVARIANT_TSC != 0;
    crate::printkln!(
        "[time] clock source: TSC at {}.{:03} MHz ({}), {}",
        hz / 1_000_000,
        hz / 1000 % 1000,
        source,
        if invariant { "invariant" } else { "not marked invariant" }
    );
}

/// The frequency a hypervisor states: KVM's paravirtual clock, else the
/// timing leaf 0x4000_0010 (VMware's, which QEMU and others offer too:
/// the TSC's kHz in EAX). Only under a hypervisor (CPUID 1 ECX bit 31).
fn from_hypervisor() -> Option<(u64, &'static str)> {
    const HYPERVISOR: u32 = 1 << 31;
    if __cpuid(1).ecx & HYPERVISOR == 0 {
        return None;
    }
    if let Some(hz) = from_kvmclock() {
        return Some((hz, "kvmclock"));
    }
    const TIMING_LEAF: u32 = 0x4000_0010;
    if __cpuid(0x4000_0000).eax >= TIMING_LEAF {
        let khz = __cpuid(TIMING_LEAF).eax as u64;
        if khz != 0 {
            return Some((khz * 1000, "hypervisor's CPUID leaf 0x40000010"));
        }
    }
    None
}

/// What the CPU states, as Linux's `native_calibrate_tsc` reads it (Intel
/// only): leaf 0x15 gives the TSC's ratio to the core crystal and, mostly,
/// the crystal's frequency; where it does not, the crystal is known from
/// the CPU model, else derived from leaf 0x16's base frequency (which the
/// TSC runs at: crystal = base * denominator / numerator).
fn from_cpuid() -> Option<(u64, &'static str)> {
    let vendor = __cpuid(0);
    let intel = [vendor.ebx, vendor.edx, vendor.ecx] == [0x756e_6547, 0x4965_6e69, 0x6c65_746e];
    if !intel || vendor.eax < 0x15 {
        return None;
    }
    let leaf = __cpuid(0x15);
    let (denominator, numerator) = (leaf.eax as u64, leaf.ebx as u64);
    if denominator == 0 || numerator == 0 {
        return None;
    }
    let (crystal, source) = if leaf.ecx != 0 {
        (leaf.ecx as u64, "CPUID leaf 0x15")
    } else if let Some(hz) = known_crystal() {
        (hz, "CPUID leaf 0x15, crystal by CPU model")
    } else if vendor.eax >= 0x16 && __cpuid(0x16).eax != 0 {
        (__cpuid(0x16).eax as u64 * 1_000_000 * denominator / numerator, "CPUID leaves 0x15 and 0x16")
    } else {
        return None;
    };
    Some((crystal * numerator / denominator, source))
}

/// The core crystal of Intel models whose leaf 0x15 leaves it out (the
/// table of Linux's `native_calibrate_tsc`).
fn known_crystal() -> Option<u64> {
    let signature = __cpuid(1).eax;
    let family = (signature >> 8) & 0xf;
    if family != 6 {
        return None;
    }
    let model = (signature >> 4) & 0xf | (signature >> 12) & 0xf0;
    match model {
        // Skylake and Kaby Lake, mobile and desktop.
        0x4e | 0x5e | 0x8e | 0x9e => Some(24_000_000),
        // Atom Goldmont (Denverton).
        0x5f => Some(25_000_000),
        // Atom Goldmont (Apollo Lake).
        0x5c => Some(19_200_000),
        _ => None,
    }
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
    set_tsc_aux(cpu);
    publish_offset(cpu);
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
