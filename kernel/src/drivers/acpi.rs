//! ACPI, as far as SMP needs it: the MADT ("APIC" table) lists the CPUs,
//! the I/O APIC and how ISA interrupts are wired to it.

use crate::memory::{self, Caching};
use alloc::vec::Vec;
use spin::Once;

/// An ISA interrupt rerouted to another GSI or with non-default electrics.
#[derive(Clone, Copy, Debug)]
pub struct Override {
    pub irq: u8,
    pub gsi: u32,
    pub level_triggered: bool,
    pub active_low: bool,
}

#[derive(Debug)]
pub struct Madt {
    pub local_apic: u64,
    /// Local APIC ids of the usable CPUs; the first is not necessarily the
    /// bootstrap CPU.
    pub cpus: Vec<u8>,
    pub io_apic: u64,
    pub io_apic_gsi_base: u32,
    pub overrides: Vec<Override>,
}

static MADT: Once<Madt> = Once::new();

/// Maps `len` bytes of a firmware table.
fn table(phys: u64, len: usize) -> Result<&'static [u8], &'static str> {
    let virt = memory::map_physical(phys, len as u64, Caching::WriteBack)?;
    Ok(unsafe { core::slice::from_raw_parts(virt.as_ptr(), len) })
}

fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// A whole system description table (header included), checksum verified.
fn sdt(phys: u64) -> Result<&'static [u8], &'static str> {
    let header = table(phys, 36)?;
    let len = u32_at(header, 4) as usize;
    if !(36..=1 << 20).contains(&len) {
        return Err("bad ACPI table length");
    }
    let t = table(phys, len)?;
    if !checksum_ok(t) {
        return Err("bad ACPI table checksum");
    }
    Ok(t)
}

/// Parses the MADT, starting from the RSDP the bootloader found.
pub fn init(rsdp_phys: u64) -> Result<&'static Madt, &'static str> {
    let rsdp = table(rsdp_phys, 36)?;
    if &rsdp[0..8] != b"RSD PTR " || !checksum_ok(&rsdp[0..20]) {
        return Err("no valid RSDP");
    }
    // ACPI 2+ has the 64-bit XSDT; fall back to the RSDT.
    let (root, entry_size) = if rsdp[15] >= 2 && u64_at(rsdp, 24) != 0 {
        (sdt(u64_at(rsdp, 24))?, 8)
    } else {
        (sdt(u32_at(rsdp, 16) as u64)?, 4)
    };
    let madt = root[36..]
        .chunks_exact(entry_size)
        .map(|e| if entry_size == 8 { u64_at(e, 0) } else { u32_at(e, 0) as u64 })
        .filter_map(|phys| sdt(phys).ok())
        .find(|t| &t[0..4] == b"APIC")
        .ok_or("no MADT")?;

    let mut m = Madt { local_apic: u32_at(madt, 36) as u64, cpus: Vec::new(), io_apic: 0, io_apic_gsi_base: 0, overrides: Vec::new() };
    let mut rest = &madt[44..];
    while rest.len() >= 2 {
        let (kind, len) = (rest[0], rest[1] as usize);
        if len < 2 || len > rest.len() {
            break;
        }
        let e = &rest[..len];
        match kind {
            // Processor local APIC: enabled, or online-capable.
            0 if len >= 8 && u32_at(e, 4) & 0b11 != 0 => m.cpus.push(e[3]),
            // The first I/O APIC; QEMU has exactly one.
            1 if len >= 12 && m.io_apic == 0 => {
                m.io_apic = u32_at(e, 4) as u64;
                m.io_apic_gsi_base = u32_at(e, 8);
            }
            // Interrupt source override (bus 0 = ISA).
            2 if len >= 10 && e[2] == 0 => {
                let flags = u16::from_le_bytes([e[8], e[9]]);
                m.overrides.push(Override {
                    irq: e[3],
                    gsi: u32_at(e, 4),
                    active_low: flags & 0b11 == 0b11,
                    level_triggered: (flags >> 2) & 0b11 == 0b11,
                });
            }
            // Local APIC address override.
            5 if len >= 12 => m.local_apic = u64_at(e, 4),
            _ => {}
        }
        rest = &rest[len..];
    }
    if m.cpus.is_empty() || m.io_apic == 0 {
        return Err("MADT lacks CPUs or an I/O APIC");
    }
    Ok(MADT.call_once(|| m))
}

pub fn madt() -> &'static Madt {
    MADT.get().expect("acpi::init not called")
}
