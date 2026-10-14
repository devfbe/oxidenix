//! The clock page: the time data the kernel publishes in one read-only page
//! of every Linux process (the `[vvar]` page of the Linux server's vDSO,
//! ADR 0014), and how a reader in user mode turns it into the kernel's
//! clocks without a system call. Kept apart so the kernel (the writer) and
//! the vDSO (a reader, which includes this file as a module) share one
//! layout and one computation, tested on the host.
//!
//! **The clocks.** The kernel's monotonic clock is nanoseconds since boot,
//! `(tsc - boot_tsc) * mult >> 32` (`mult`: nanoseconds per TSC cycle in
//! 32.32 fixed point), on a counter corrected per CPU by `tsc_offset` where
//! a CPU's TSC is off the bootstrap CPU's (`kernel/src/time.rs`). The wall
//! clock is `wall_at_boot` plus the monotonic clock. `mono` and `wall` are
//! the kernel's own formulas, so a reading here and the kernel's
//! (`SYS_CLOCK_READ`) agree to the nanosecond for the same counter value.
//!
//! **How the counter is read** (`mode`): with every CPU's offset 0
//! (`MODE_TSC`) an ordered RDTSC, as the kernel does; with offsets
//! (`MODE_TSC_PER_CPU`) RDTSCP, which returns the counter together with the
//! CPU's `IA32_TSC_AUX` (its index, which the kernel sets), so the offset is
//! that of the CPU the counter came from even if the thread moves meanwhile;
//! `MODE_NONE` (offsets without RDTSCP, or nothing published yet): no
//! reading, the reader makes the system call.
//!
//! **Consistency** is a sequence lock (Linux's vDSO seqcount): the kernel
//! makes `seq` odd, changes the fields, makes it even again
//! (`ClockPage::update`; its writers are serialized by the kernel); a
//! reader takes a snapshot between two equal even values of `seq`
//! (`ClockPage::read`), so it never combines a wall offset of before a
//! clock_settime with a scale of after a calibration.

#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// CPUs the page has offsets for (the kernel brings up at most this many).
pub const CPUS: usize = 64;

/// `mode`: no reading in user mode (the system call answers).
pub const MODE_NONE: u32 = 0;
/// `mode`: every CPU's counter agrees with the bootstrap CPU's.
pub const MODE_TSC: u32 = 1;
/// `mode`: some CPU's counter is corrected; RDTSCP names the CPU.
pub const MODE_TSC_PER_CPU: u32 = 2;

/// `features`: the CPU has RDTSCP (and the kernel set each CPU's TSC_AUX).
pub const HAS_RDTSCP: u32 = 1;
/// `features`: the CPU has RDPID (TSC_AUX alone, cheaper than RDTSCP).
pub const HAS_RDPID: u32 = 2;

/// The bits of `IA32_TSC_AUX` that are the CPU's index, as Linux encodes it
/// (the NUMA node above them, 0 here).
pub const AUX_CPU_MASK: u32 = 0xfff;

const NSEC_PER_SEC: u64 = 1_000_000_000;

/// The page's contents (the rest of the page is zero). Every field is an
/// atomic: the kernel changes them while programs read.
#[repr(C, align(64))]
pub struct ClockPage {
    /// The sequence count: odd while the kernel changes the page.
    pub seq: AtomicU32,
    /// How the counter is read (`MODE_*`).
    pub mode: AtomicU32,
    /// Nanoseconds per TSC cycle, in 32.32 fixed point.
    pub mult: AtomicU64,
    /// The bootstrap CPU's counter at boot (the monotonic clock's zero).
    pub boot_tsc: AtomicU64,
    /// The wall clock at boot, in nanoseconds since the epoch (moved by
    /// clock_settime).
    pub wall_at_boot: AtomicI64,
    /// What the CPUs offer a reader (`HAS_*`).
    pub features: AtomicU32,
    _reserved: AtomicU32,
    /// Added to CPU n's counter to match the bootstrap CPU's.
    pub tsc_offset: [AtomicI64; CPUS],
}

const _: () = assert!(core::mem::size_of::<ClockPage>() <= 4096);

/// Reads the CPU's counter (the vDSO's instructions; a model in the tests).
pub trait Counter {
    /// The counter, not read before earlier loads (LFENCE; RDTSC).
    fn tsc(&self) -> u64;
    /// The counter and the CPU's TSC_AUX of the same instant (RDTSCP).
    fn tsc_aux(&self) -> (u64, u32);
}

/// One consistent reading of the clocks, in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Since boot (CLOCK_MONOTONIC).
    pub mono: u64,
    /// Since the epoch (CLOCK_REALTIME).
    pub wall: u64,
}

/// The monotonic clock at counter value `tsc` (already corrected for its
/// CPU): the kernel's `time::now`.
#[inline(always)]
pub fn mono(tsc: u64, boot_tsc: u64, mult: u64) -> u64 {
    ((tsc.saturating_sub(boot_tsc) as u128 * mult as u128) >> 32) as u64
}

/// The wall clock at monotonic time `mono`: the kernel's `time::realtime`.
#[inline(always)]
pub fn wall(wall_at_boot: i64, mono: u64) -> u64 {
    wall_at_boot.saturating_add(mono as i64).max(0) as u64
}

/// `ns` as a timespec's seconds and nanoseconds.
#[inline(always)]
pub fn split(ns: u64) -> (u64, u64) {
    (ns / NSEC_PER_SEC, ns % NSEC_PER_SEC)
}

impl ClockPage {
    /// A page with nothing published (`MODE_NONE`).
    pub const fn new() -> ClockPage {
        ClockPage {
            seq: AtomicU32::new(0),
            mode: AtomicU32::new(MODE_NONE),
            mult: AtomicU64::new(0),
            boot_tsc: AtomicU64::new(0),
            wall_at_boot: AtomicI64::new(0),
            features: AtomicU32::new(0),
            _reserved: AtomicU32::new(0),
            tsc_offset: [const { AtomicI64::new(0) }; CPUS],
        }
    }

    /// The clocks now, read with `counter`; None if the page offers no
    /// reading in user mode (`MODE_NONE`, or a CPU beyond `CPUS`).
    #[inline(always)]
    pub fn read<C: Counter>(&self, counter: &C) -> Option<Reading> {
        loop {
            let seq = self.seq.load(Ordering::Acquire);
            if seq & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let tsc = match self.mode.load(Ordering::Relaxed) {
                MODE_TSC => counter.tsc(),
                MODE_TSC_PER_CPU => {
                    let (tsc, aux) = counter.tsc_aux();
                    let offset = self.tsc_offset.get((aux & AUX_CPU_MASK) as usize)?;
                    tsc.wrapping_add_signed(offset.load(Ordering::Relaxed))
                }
                _ => return None,
            };
            let (boot, mult, at_boot) = (self.boot_tsc.load(Ordering::Relaxed), self.mult.load(Ordering::Relaxed), self.wall_at_boot.load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == seq {
                let mono = mono(tsc, boot, mult);
                return Some(Reading { mono, wall: wall(at_boot, mono) });
            }
        }
    }

    /// Changes the page with `change` (stores of its fields) so that no
    /// reader sees part of it. The caller serializes writers.
    pub fn update(&self, change: impl FnOnce(&ClockPage)) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        change(self);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }
}

impl Default for ClockPage {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// A counter at a fixed value on a fixed CPU.
    struct Fixed(u64, u32);

    impl Counter for Fixed {
        fn tsc(&self) -> u64 {
            self.0
        }
        fn tsc_aux(&self) -> (u64, u32) {
            (self.0, self.1)
        }
    }

    fn publish(page: &ClockPage, mode: u32, boot: u64, mult: u64, at_boot: i64) {
        page.update(|p| {
            p.mode.store(mode, Ordering::Relaxed);
            p.boot_tsc.store(boot, Ordering::Relaxed);
            p.mult.store(mult, Ordering::Relaxed);
            p.wall_at_boot.store(at_boot, Ordering::Relaxed);
        });
    }

    #[test]
    fn nothing_published_is_no_reading() {
        assert_eq!(ClockPage::new().read(&Fixed(1, 0)), None);
    }

    #[test]
    fn the_kernels_formulas() {
        let page = ClockPage::new();
        // A 2.5 GHz counter: 0.4 ns per cycle.
        let mult = ((NSEC_PER_SEC as u128) << 32).div_ceil(2_500_000_000) as u64;
        publish(&page, MODE_TSC, 1000, mult, 1_700_000_000 * NSEC_PER_SEC as i64);
        let r = page.read(&Fixed(1000 + 2_500_000_000, 0)).unwrap();
        assert_eq!(r.mono, NSEC_PER_SEC);
        assert_eq!(r.wall, 1_700_000_001 * NSEC_PER_SEC);
        // Before the boot counter: 0, never a wrapped huge value.
        assert_eq!(page.read(&Fixed(10, 0)).unwrap().mono, 0);
        // A wall clock set before the epoch's start is 0.
        publish(&page, MODE_TSC, 0, 1 << 32, -50);
        assert_eq!(page.read(&Fixed(20, 0)).unwrap(), Reading { mono: 20, wall: 0 });
        assert_eq!(split(3 * NSEC_PER_SEC + 7), (3, 7));
    }

    #[test]
    fn per_cpu_offsets_by_tsc_aux() {
        let page = ClockPage::new();
        publish(&page, MODE_TSC_PER_CPU, 0, 1 << 32, 0);
        page.update(|p| p.tsc_offset[3].store(-500, Ordering::Relaxed));
        assert_eq!(page.read(&Fixed(10_000, 0)).unwrap().mono, 10_000);
        assert_eq!(page.read(&Fixed(10_000, 3)).unwrap().mono, 9_500);
        // The node bits above the CPU's are not its index.
        assert_eq!(page.read(&Fixed(10_000, 3 | 1 << 12)).unwrap().mono, 9_500);
        // A CPU the page has no offset for: no reading.
        assert_eq!(page.read(&Fixed(10_000, CPUS as u32)), None);
        // MODE_TSC ignores the offsets (they are all 0 then).
        publish(&page, MODE_TSC, 0, 1 << 32, 0);
        assert_eq!(page.read(&Fixed(10_000, 3)).unwrap().mono, 10_000);
    }

    #[test]
    fn no_reader_sees_a_torn_update() {
        // The writer switches between two states whose readings at counter
        // 1000 are (1000, 1000) and (2000, 1_000_002_000); a reading that
        // mixes their fields is neither.
        let page = Arc::new(ClockPage::new());
        publish(&page, MODE_TSC, 0, 1 << 32, 0);
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let (page, stop) = (page.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut seen = [0u64; 2];
                    while !stop.load(Ordering::Relaxed) {
                        let r = page.read(&Fixed(1000, 0)).unwrap();
                        match r {
                            Reading { mono: 1000, wall: 1000 } => seen[0] += 1,
                            Reading { mono: 2000, wall: 1_000_002_000 } => seen[1] += 1,
                            torn => panic!("torn reading {torn:?}"),
                        }
                    }
                    seen
                })
            })
            .collect();
        for i in 0..2_000_000u64 {
            let (mult, at_boot) = if i % 2 == 0 { (2 << 32, 1_000_000_000) } else { (1 << 32, 0) };
            page.update(|p| {
                p.mult.store(mult, Ordering::Relaxed);
                p.wall_at_boot.store(at_boot, Ordering::Relaxed);
            });
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            let seen = r.join().unwrap();
            assert!(seen[0] + seen[1] > 0);
        }
        assert_eq!(page.seq.load(Ordering::Relaxed) % 2, 0);
    }
}
