//! PCI enumeration through configuration mechanism #1 (ports 0xcf8/0xcfc).
//! The kernel only finds devices and hands their resources (I/O ports,
//! memory BARs, an interrupt: the legacy line or a message-signalled
//! vector) to the server that drives them; configuration space stays the
//! kernel's.

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;

const COMMAND_IO: u16 = 1 << 0;
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;
const COMMAND_INTX_DISABLE: u16 = 1 << 10;
const STATUS_CAPABILITIES: u32 = 1 << 20;

/// Capability ids.
const CAP_MSI: u8 = 0x05;
const CAP_MSIX: u8 = 0x11;

/// MSI message control bits.
const MSI_ENABLE: u16 = 1 << 0;
const MSI_MULTIPLE_ENABLE: u16 = 0b111 << 4;
const MSI_64BIT: u16 = 1 << 7;
const MSI_PER_VECTOR_MASK: u16 = 1 << 8;
/// MSI-X message control bits.
const MSIX_ENABLE: u16 = 1 << 15;
const MSIX_FUNCTION_MASK: u16 = 1 << 14;

/// The local APICs' message address window (Intel SDM 11.11.1): fixed
/// delivery, physical destination in bits 19:12.
const MSI_ADDRESS: u32 = 0xfee0_0000;

#[derive(Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    /// The programming interface (class code bits 15:8).
    pub prog_if: u8,
    /// Legacy (PIC) interrupt line, as set up by the BIOS.
    pub irq: u8,
}

/// A memory base address register: its physical range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBar {
    pub phys: u64,
    pub len: u64,
    pub prefetchable: bool,
}

/// How a device's message-signalled interrupt was set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msi {
    Msi,
    MsiX,
}

fn address(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    1 << 31 | (bus as u32) << 16 | (slot as u32) << 11 | (func as u32) << 8 | (offset as u32 & 0xfc)
}

/// Configuration mechanism #1 is two port accesses: no other CPU may come
/// between them.
static CONFIG: crate::sync::IrqSpinLock<()> = crate::sync::IrqSpinLock::new(());

pub fn read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let _config = CONFIG.lock();
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, slot, func, offset));
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

fn write32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let _config = CONFIG.lock();
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, slot, func, offset));
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}

impl Device {
    pub fn read(&self, offset: u8) -> u32 {
        read32(self.bus, self.slot, self.func, offset)
    }

    pub fn write(&self, offset: u8, value: u32) {
        write32(self.bus, self.slot, self.func, offset, value)
    }

    fn read16(&self, offset: u8) -> u16 {
        (self.read(offset & !3) >> ((offset & 2) * 8)) as u16
    }

    /// A 16-bit write as a read-modify-write of its dword; `keep` masks the
    /// other half's bits that are written back as read (status bits that
    /// clear when written with 1 must be masked out).
    fn write16(&self, offset: u8, value: u16, keep: u32) {
        let shift = (offset & 2) * 8;
        let reg = self.read(offset & !3) & keep & !(0xffff << shift);
        self.write(offset & !3, reg | (value as u32) << shift);
    }

    fn command(&self) -> u16 {
        self.read16(0x04)
    }

    fn set_command(&self, command: u16) {
        // (The status half is all write-1-to-clear bits: written as 0.)
        self.write16(0x04, command, 0);
    }

    /// Base address register `n` if it describes an I/O port range:
    /// (first port, number of ports).
    pub fn io_bar(&self, n: u8) -> Option<(u16, u16)> {
        let offset = 0x10 + 4 * n;
        let bar = self.read(offset);
        if bar & 1 == 0 {
            return None;
        }
        // Size probe: write all ones, read back the mask, restore.
        self.write(offset, u32::MAX);
        let mask = self.read(offset) & !0x3;
        self.write(offset, bar);
        let size = (!mask).wrapping_add(1) & 0xffff;
        (size != 0).then_some(((bar & !0x3) as u16, size as u16))
    }

    /// Base address register `n` (with `n + 1`, for a 64-bit one) if it
    /// describes a memory range. Its decoding is off while it is probed.
    pub fn memory_bar(&self, n: u8) -> Option<MemoryBar> {
        if n > 5 {
            return None;
        }
        let offset = 0x10 + 4 * n;
        let low = self.read(offset);
        if low & 1 != 0 {
            return None;
        }
        let wide = low >> 1 & 0b11 == 0b10;
        if wide && n == 5 {
            return None;
        }
        let command = self.command();
        self.set_command(command & !(COMMAND_MEMORY | COMMAND_IO));
        self.write(offset, u32::MAX);
        let low_mask = self.read(offset) & !0xf;
        self.write(offset, low);
        let (high, high_mask) = if wide {
            let high = self.read(offset + 4);
            self.write(offset + 4, u32::MAX);
            let mask = self.read(offset + 4);
            self.write(offset + 4, high);
            (high, mask)
        } else {
            (0, u32::MAX)
        };
        self.set_command(command);
        let mask = (high_mask as u64) << 32 | low_mask as u64;
        let len = (!mask).wrapping_add(1);
        let phys = (high as u64) << 32 | (low & !0xf) as u64;
        (low_mask != 0 && len != 0 && phys != 0).then_some(MemoryBar { phys, len, prefetchable: low & 0b1000 != 0 })
    }

    /// Lets the device decode its I/O ports and access memory by DMA.
    pub fn enable_io_and_dma(&self) {
        self.set_command(self.command() | COMMAND_IO | COMMAND_BUS_MASTER);
    }

    /// Lets the device decode its memory BARs and access memory by DMA.
    pub fn enable_memory_and_dma(&self) {
        self.set_command(self.command() | COMMAND_MEMORY | COMMAND_BUS_MASTER);
    }

    /// The configuration space offset of capability `id`, if the device
    /// has it. The list is walked at most 48 steps (a looping list from
    /// broken hardware ends).
    pub fn capability(&self, id: u8) -> Option<u8> {
        if self.read(0x04) & STATUS_CAPABILITIES == 0 {
            return None;
        }
        let mut at = self.read(0x34) as u8 & 0xfc;
        for _ in 0..48 {
            if at < 0x40 {
                return None;
            }
            let header = self.read(at);
            if header as u8 == id {
                return Some(at);
            }
            at = (header >> 8) as u8 & 0xfc;
        }
        None
    }

    /// Sends the device's interrupts as messages of `vector` to the local
    /// APIC `apic_id` (fixed delivery, edge): MSI if the device has it,
    /// else entry 0 of its MSI-X table; its legacy line is turned off.
    /// None (nothing changed) if it has neither or the destination is
    /// beyond a message's reach (8 bits without interrupt remapping).
    pub fn enable_msi(&self, vector: u8, apic_id: u32) -> Option<Msi> {
        if apic_id > 0xff {
            return None;
        }
        let address = MSI_ADDRESS | apic_id << 12;
        if let Some(cap) = self.capability(CAP_MSI) {
            let control = self.read16(cap + 2);
            let data_at = if control & MSI_64BIT != 0 {
                self.write(cap + 8, 0);
                cap + 12
            } else {
                cap + 8
            };
            self.write(cap + 4, address);
            // (The data register is 16 bits; the dword's upper half is
            // reserved or the extended data, written as 0.)
            self.write(data_at, vector as u32);
            if control & MSI_PER_VECTOR_MASK != 0 {
                // The mask bits follow the data dword.
                self.write(data_at + 4, 0);
            }
            // One message (multiple message enable 0).
            self.write16(cap + 2, (control & !MSI_MULTIPLE_ENABLE) | MSI_ENABLE, u32::MAX);
            self.set_command(self.command() | COMMAND_INTX_DISABLE);
            return Some(Msi::Msi);
        }
        let cap = self.capability(CAP_MSIX)?;
        let table = self.read(cap + 4);
        let bar = self.memory_bar((table & 0b111) as u8)?;
        let offset = (table & !0b111) as u64;
        // Entry 0: address low, high, data, vector control (bit 0: masked).
        if offset + 16 > bar.len {
            return None;
        }
        let entry = crate::memory::map_physical(bar.phys + offset, 16, crate::memory::Caching::Uncached).ok()?.as_mut_ptr::<u32>();
        let control = self.read16(cap + 2);
        // Masked as a whole while the entry changes, and memory decoding
        // on, so that the table can be written.
        self.write16(cap + 2, control | MSIX_ENABLE | MSIX_FUNCTION_MASK, u32::MAX);
        self.set_command(self.command() | COMMAND_MEMORY);
        unsafe {
            entry.write_volatile(address);
            entry.add(1).write_volatile(0);
            entry.add(2).write_volatile(vector as u32);
            entry.add(3).write_volatile(0);
            // (Read back: the writes reached the device before the unmask.)
            let _ = entry.add(3).read_volatile();
        }
        self.write16(cap + 2, (control | MSIX_ENABLE) & !MSIX_FUNCTION_MASK, u32::MAX);
        self.set_command(self.command() | COMMAND_INTX_DISABLE);
        Some(Msi::MsiX)
    }
}

/// All functions on all buses.
pub fn scan() -> Vec<Device> {
    let mut found = Vec::new();
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            for func in 0..8u8 {
                let id = read32(bus, slot, func, 0);
                if id & 0xffff == 0xffff {
                    if func == 0 {
                        break;
                    }
                    continue;
                }
                let class = read32(bus, slot, func, 0x08);
                let irq = read32(bus, slot, func, 0x3c) as u8;
                found.push(Device {
                    bus,
                    slot,
                    func,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (class >> 24) as u8,
                    subclass: (class >> 16) as u8,
                    prog_if: (class >> 8) as u8,
                    irq,
                });
                let header = read32(bus, slot, func, 0x0c) >> 16 & 0xff;
                if func == 0 && header & 0x80 == 0 {
                    break;
                }
            }
        }
    }
    found
}

/// The first device with this vendor and device id.
pub fn find(vendor: u16, device: u16) -> Option<Device> {
    scan().into_iter().find(|d| d.vendor == vendor && d.device == device)
}

/// Every device of this class, subclass and programming interface.
pub fn find_class(class: u8, subclass: u8, prog_if: u8) -> Vec<Device> {
    scan().into_iter().filter(|d| (d.class, d.subclass, d.prog_if) == (class, subclass, prog_if)).collect()
}
