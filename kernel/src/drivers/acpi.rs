//! ACPI's static tables, as far as the kernel needs them: the MADT ("APIC")
//! lists the CPUs (local APIC and x2APIC entries), the I/O APIC and how ISA
//! interrupts are wired to it; the HPET table and the FADT ("FACP") say where
//! the reference clocks are that the TSC is calibrated against (`refclock`).

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
    /// APIC ids of the usable CPUs, from the local APIC entries (8-bit ids)
    /// and the local x2APIC entries (32-bit ids), each once, in table order;
    /// the first is not necessarily the bootstrap CPU.
    pub cpus: Vec<u32>,
    pub io_apic: u64,
    pub io_apic_gsi_base: u32,
    pub overrides: Vec<Override>,
    /// The machine has the two 8259 PICs as well (PCAT_COMPAT), which must
    /// be masked before the APICs take over.
    pub has_pics: bool,
}

/// The ACPI power-management timer: a 3.579545 MHz counter in an I/O port,
/// 24 bits wide or, with `wide`, 32.
#[derive(Clone, Copy, Debug)]
pub struct PmTimer {
    pub port: u16,
    pub wide: bool,
}

struct Tables {
    madt: Madt,
    /// The HPET's register block (physical), if the machine has one.
    hpet: Option<u64>,
    pm_timer: Option<PmTimer>,
}

static TABLES: Once<Tables> = Once::new();

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

/// Finds the tables the kernel uses, starting from the RSDP the bootloader
/// found. Only the MADT is required.
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
    let find = |signature: &[u8; 4]| {
        root[36..]
            .chunks_exact(entry_size)
            .map(|e| if entry_size == 8 { u64_at(e, 0) } else { u32_at(e, 0) as u64 })
            .filter_map(|phys| sdt(phys).ok())
            .find(|t| &t[0..4] == signature)
    };
    let madt = parse_madt(find(b"APIC").ok_or("no MADT")?)?;
    let hpet = find(b"HPET").and_then(parse_hpet);
    let pm_timer = find(b"FACP").and_then(parse_fadt);
    Ok(&TABLES.call_once(|| Tables { madt, hpet, pm_timer }).madt)
}

fn parse_madt(madt: &[u8]) -> Result<Madt, &'static str> {
    const PCAT_COMPAT: u32 = 1;
    let mut m = Madt {
        local_apic: u32_at(madt, 36) as u64,
        cpus: Vec::new(),
        io_apic: 0,
        io_apic_gsi_base: 0,
        overrides: Vec::new(),
        has_pics: u32_at(madt, 40) & PCAT_COMPAT != 0,
    };
    // A CPU usable now (enabled) or later (online-capable), listed once:
    // firmware may describe a CPU by both kinds of entry.
    let add_cpu = |cpus: &mut Vec<u32>, id: u32, flags: u32| {
        if flags & 0b11 != 0 && !cpus.contains(&id) {
            cpus.push(id);
        }
    };
    let mut rest = &madt[44..];
    while rest.len() >= 2 {
        let (kind, len) = (rest[0], rest[1] as usize);
        if len < 2 || len > rest.len() {
            break;
        }
        let e = &rest[..len];
        match kind {
            // Processor local APIC: its 8-bit id.
            0 if len >= 8 => add_cpu(&mut m.cpus, e[3] as u32, u32_at(e, 4)),
            // The first I/O APIC.
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
            // Processor local x2APIC: its 32-bit id (firmware uses these for
            // ids from 255 on, some for every CPU).
            9 if len >= 16 => add_cpu(&mut m.cpus, u32_at(e, 4), u32_at(e, 8)),
            _ => {}
        }
        rest = &rest[len..];
    }
    if m.cpus.is_empty() || m.io_apic == 0 {
        return Err("MADT lacks CPUs or an I/O APIC");
    }
    Ok(m)
}

/// The HPET table: the register block's address (a generic address in
/// system memory).
fn parse_hpet(t: &[u8]) -> Option<u64> {
    const SYSTEM_MEMORY: u8 = 0;
    (t.len() >= 52 && t[40] == SYSTEM_MEMORY).then(|| u64_at(t, 44)).filter(|&base| base != 0)
}

/// The FADT's PM timer: the 64-bit generic address (ACPI 2+) if it names
/// an I/O port, else the 32-bit port field. Hardware-reduced ACPI has none.
fn parse_fadt(t: &[u8]) -> Option<PmTimer> {
    const TMR_VAL_EXT: u32 = 1 << 8;
    const HW_REDUCED_ACPI: u32 = 1 << 20;
    const SYSTEM_IO: u8 = 1;
    if t.len() < 116 {
        return None;
    }
    let flags = u32_at(t, 112);
    if flags & HW_REDUCED_ACPI != 0 {
        return None;
    }
    let extended = (t.len() >= 220 && t[208] == SYSTEM_IO).then(|| u64_at(t, 212)).filter(|&port| port != 0);
    let port = extended.unwrap_or(u32_at(t, 76) as u64);
    let port = u16::try_from(port).ok().filter(|&p| p != 0)?;
    Some(PmTimer { port, wide: flags & TMR_VAL_EXT != 0 })
}

fn tables() -> &'static Tables {
    TABLES.get().expect("acpi::init not called")
}

pub fn madt() -> &'static Madt {
    &tables().madt
}

/// The HPET's register block (physical address), if the firmware lists one.
pub fn hpet() -> Option<u64> {
    tables().hpet
}

/// The ACPI PM timer, if the firmware lists one.
pub fn pm_timer() -> Option<PmTimer> {
    tables().pm_timer
}
