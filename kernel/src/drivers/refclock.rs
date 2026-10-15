//! Reference clocks of known frequency that the TSC is measured against
//! when neither the CPU nor a hypervisor states its frequency (`time::init`,
//! ADR 0016): the HPET, then the ACPI PM timer, both found in the ACPI
//! tables, and only as the last resort the PIT, which recent chipsets may
//! have gated off.
//!
//! A measurement counts TSC cycles over about 50 ms of the reference
//! clock. Each end is a pair (TSC, reference) whose TSC is the middle of
//! the shortest of several bracketing reads, so the slow reference read
//! (an I/O port or an uncached register, about a microsecond) costs no
//! precision. The median of three measurements is the result. A clock that
//! does not count is given up on after a bounded number of cycles.

use crate::drivers::acpi;
use crate::memory::{self, Caching};
use crate::time::rdtsc;
use x86_64::instructions::port::Port;

/// How long one measurement runs, in milliseconds.
const MEASURE_MS: u64 = 50;
/// TSC cycles after which a reference clock that has not advanced far
/// enough counts as broken: about a second at 8 GHz, ten at 800 MHz.
const GIVE_UP_CYCLES: u64 = 1 << 33;
/// Plausible TSC frequencies; a result outside means a broken reference.
const PLAUSIBLE_HZ: core::ops::RangeInclusive<u64> = 100_000_000..=20_000_000_000;

/// The TSC's frequency and the clock it was measured against, trying the
/// HPET, the ACPI PM timer and the PIT in this order. None if none works.
pub fn measure_tsc() -> Option<(u64, &'static str)> {
    if let Some(hz) = acpi::hpet().and_then(with_hpet) {
        return Some((hz, "measured against the HPET"));
    }
    if let Some(hz) = acpi::pm_timer().and_then(with_pm_timer) {
        return Some((hz, "measured against the ACPI PM timer"));
    }
    with_pit().map(|hz| (hz, "measured against the PIT"))
}

/// The median of three measurements.
fn median3(mut measure: impl FnMut() -> Option<u64>) -> Option<u64> {
    let mut runs = [measure()?, measure()?, measure()?];
    runs.sort_unstable();
    Some(runs[1]).filter(|hz| PLAUSIBLE_HZ.contains(hz))
}

/// A reading of the reference clock and the TSC at the same moment: the
/// TSC is the middle of the shortest of a few reads around it.
fn sample(read: &impl Fn() -> u64) -> (u64, u64) {
    let mut best = (u64::MAX, 0, 0);
    for _ in 0..5 {
        let before = rdtsc();
        let value = read();
        let after = rdtsc();
        let width = after.wrapping_sub(before);
        if width < best.0 {
            best = (width, before + width / 2, value);
        }
    }
    (best.1, best.2)
}

/// TSC cycles per second over `MEASURE_MS` of a counter that `read`
/// returns, `mask` wide (it wraps), running at `hz`.
fn measure_counter(read: &impl Fn() -> u64, mask: u64, hz: u64) -> Option<u64> {
    let ticks = hz * MEASURE_MS / 1000;
    let (tsc0, start) = sample(read);
    let (mut last, mut elapsed) = (start, 0u64);
    while elapsed < ticks {
        let now = read();
        elapsed += now.wrapping_sub(last) & mask;
        last = now;
        if rdtsc().wrapping_sub(tsc0) > GIVE_UP_CYCLES {
            return None;
        }
    }
    let (tsc1, end) = sample(read);
    elapsed += end.wrapping_sub(last) & mask;
    Some((tsc1.wrapping_sub(tsc0) as u128 * hz as u128 / elapsed as u128) as u64)
}

// ------------------------------------------------------------------ HPET

const HPET_CAPABILITIES: usize = 0x000;
const HPET_CONFIG: usize = 0x010;
const HPET_COUNTER: usize = 0x0f0;
const HPET_ENABLE: u64 = 1;
const HPET_COUNT_64: u64 = 1 << 13;
/// The longest counter period the specification allows: 100 ns.
const HPET_MAX_PERIOD_FS: u64 = 100_000_000;

/// Measures against the HPET's main counter, which it enables for that if
/// the firmware left it off (and stops again afterwards).
fn with_hpet(base: u64) -> Option<u64> {
    let regs = memory::map_physical(base, 1024, Caching::Uncached).ok()?.as_u64() as usize;
    let reg = |offset: usize| (regs + offset) as *mut u64;
    let caps = unsafe { core::ptr::read_volatile(reg(HPET_CAPABILITIES)) };
    let period_fs = caps >> 32;
    if period_fs == 0 || period_fs > HPET_MAX_PERIOD_FS {
        return None;
    }
    let hz = 1_000_000_000_000_000 / period_fs;
    let config = unsafe { core::ptr::read_volatile(reg(HPET_CONFIG)) };
    if config & HPET_ENABLE == 0 {
        unsafe { core::ptr::write_volatile(reg(HPET_CONFIG), config | HPET_ENABLE) };
    }
    // A counter only 32 bits wide is read as such (its upper half need not
    // read as zero).
    let result = if caps & HPET_COUNT_64 != 0 {
        median3(|| measure_counter(&|| unsafe { core::ptr::read_volatile(reg(HPET_COUNTER)) }, u64::MAX, hz))
    } else {
        let low = (regs + HPET_COUNTER) as *const u32;
        median3(|| measure_counter(&|| unsafe { core::ptr::read_volatile(low) } as u64, u32::MAX as u64, hz))
    };
    if config & HPET_ENABLE == 0 {
        unsafe { core::ptr::write_volatile(reg(HPET_CONFIG), config) };
    }
    result
}

// ------------------------------------------------------------ PM timer

const PM_TIMER_HZ: u64 = 3_579_545;

fn with_pm_timer(timer: acpi::PmTimer) -> Option<u64> {
    let mask = if timer.wide { u32::MAX as u64 } else { (1 << 24) - 1 };
    let read = || unsafe { Port::<u32>::new(timer.port).read() } as u64 & mask;
    median3(|| measure_counter(&read, mask, PM_TIMER_HZ))
}

// ------------------------------------------------------------------ PIT

const PIT_HZ: u64 = 1_193_182;

/// Counts TSC cycles over two intervals of PIT channel 2; their difference
/// cancels the fixed cost of starting and polling it.
fn with_pit() -> Option<u64> {
    let cycles = |ticks: u64| -> Option<u64> {
        let mut best = None;
        for _ in 0..3 {
            let start = rdtsc();
            pit_wait(ticks)?;
            let taken = rdtsc() - start;
            best = Some(best.map_or(taken, |b: u64| b.min(taken)));
        }
        best
    };
    let (short, long) = (5 * PIT_HZ / 1000, MEASURE_MS * PIT_HZ / 1000);
    let difference = cycles(long)?.checked_sub(cycles(short)?)?;
    Some(difference * PIT_HZ / (long - short)).filter(|hz| PLAUSIBLE_HZ.contains(hz))
}

/// Busy-waits `ticks` of PIT channel 2 (interrupt on terminal count, read
/// at the gate port's OUT2 bit). None if the channel never ends its count.
fn pit_wait(ticks: u64) -> Option<()> {
    let mut remaining = ticks;
    let start = rdtsc();
    unsafe {
        let mut gate: Port<u8> = Port::new(0x61);
        let mut cmd: Port<u8> = Port::new(0x43);
        let mut ch2: Port<u8> = Port::new(0x42);
        while remaining > 0 {
            let chunk = remaining.min(0xffff) as u16;
            // Gate low, mode 0 (interrupt on terminal count), then gate high.
            let g = gate.read() & 0xfc;
            gate.write(g);
            cmd.write(0b1011_0000);
            ch2.write(chunk as u8);
            ch2.write((chunk >> 8) as u8);
            gate.write(g | 1);
            // OUT2 (bit 5) goes high at terminal count.
            while gate.read() & 0x20 == 0 {
                if rdtsc().wrapping_sub(start) > GIVE_UP_CYCLES {
                    return None;
                }
                core::hint::spin_loop();
            }
            remaining -= chunk as u64;
        }
    }
    Some(())
}
