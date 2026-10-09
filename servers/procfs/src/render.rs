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
    let sig = __cpuid(1).eax;
    let family = (sig >> 8) & 0xf;
    let model = ((sig >> 4) & 0xf) | ((sig >> 12) & 0xf0);
    let n = s.cpus;
    let mut out = String::new();
    for i in 0..n {
        let _ = write!(
            out,
            "processor\t: {i}\nvendor_id\t: {vendor}\ncpu family\t: {family}\nmodel\t\t: {model}\nmodel name\t: {brand}\nphysical id\t: 0\nsiblings\t: {n}\ncore id\t\t: {i}\ncpu cores\t: {n}\napicid\t\t: {i}\nflags\t\t: fpu tsc msr pae cx8 apic sep pge cmov pat clflush mmx fxsr sse sse2 syscall nx lm\n\n"
        );
    }
    out
}
