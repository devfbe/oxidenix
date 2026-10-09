//! The formats of procfs's own making: /proc/cpuinfo (from CPUID, an
//! unprivileged instruction) and /proc/version. The others are
//! `procproto::render`'s, shared with the Linux server.

use alloc::string::String;
use core::fmt::Write;
use procproto::System;

pub fn version() -> String {
    String::from(concat!("oxidenix version ", env!("CARGO_PKG_VERSION"), " (built with Rust) #1 SMP\n"))
}

/// /proc/cpuinfo: one block per CPU.
pub fn cpuinfo(s: &System) -> String {
    use core::arch::x86_64::__cpuid;
    let words = |order: [u32; 4]| -> alloc::vec::Vec<u8> { order.iter().flat_map(|w| w.to_le_bytes()).collect() };
    let v = __cpuid(0);
    let vendor_bytes = words([v.ebx, v.edx, v.ecx, 0]);
    let vendor = String::from_utf8_lossy(&vendor_bytes[..12]).into_owned();
    let mut brand = alloc::vec::Vec::new();
    if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
        for leaf in 0x8000_0002..=0x8000_0004u32 {
            let r = __cpuid(leaf);
            brand.extend(words([r.eax, r.ebx, r.ecx, r.edx]));
        }
    }
    let brand: String = String::from_utf8_lossy(&brand).trim_matches(|c: char| c == '\0' || c == ' ').into();
    let (family, model, stepping) = signature(__cpuid(1).eax);
    // The TSC's rate, in MHz with three decimals (Linux's tsc_khz).
    let khz = s.tsc_hz / 1000;
    let n = s.cpus;
    let mut out = String::new();
    for i in 0..n {
        let _ = write!(
            out,
            "processor\t: {i}\nvendor_id\t: {vendor}\ncpu family\t: {family}\nmodel\t\t: {model}\nmodel name\t: {brand}\nstepping\t: {stepping}\ncpu MHz\t\t: {}.{:03}\nphysical id\t: 0\nsiblings\t: {n}\ncore id\t\t: {i}\ncpu cores\t: {n}\napicid\t\t: {i}\nflags\t\t: fpu tsc msr pae cx8 apic sep pge cmov pat clflush mmx fxsr sse sse2 syscall nx lm\n\n",
            khz / 1000,
            khz % 1000
        );
    }
    out
}

/// (family, model, stepping) from CPUID leaf 1's signature, as x86's rules
/// (and Linux's `x86_family`/`x86_model`) say: the extended family counts
/// only for family 15, the extended model only for families 6 and 15.
fn signature(eax: u32) -> (u32, u32, u32) {
    let base_family = (eax >> 8) & 0xf;
    let family = if base_family == 0xf { base_family + ((eax >> 20) & 0xff) } else { base_family };
    let model = if base_family == 6 || base_family == 0xf { ((eax >> 4) & 0xf) | ((eax >> 12) & 0xf0) } else { (eax >> 4) & 0xf };
    (family, model, eax & 0xf)
}
