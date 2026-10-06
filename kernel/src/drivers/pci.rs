//! PCI enumeration through configuration mechanism #1 (ports 0xcf8/0xcfc).
//! The kernel only finds devices and hands their resources (I/O ports,
//! interrupt line) to the server that drives them.

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;

const COMMAND_IO: u16 = 1 << 0;
const COMMAND_BUS_MASTER: u16 = 1 << 2;

#[derive(Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    /// Legacy (PIC) interrupt line, as set up by the BIOS.
    pub irq: u8,
}

fn address(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    1 << 31 | (bus as u32) << 16 | (slot as u32) << 11 | (func as u32) << 8 | (offset as u32 & 0xfc)
}

pub fn read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, slot, func, offset));
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

fn write32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, slot, func, offset));
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}

impl Device {
    fn read(&self, offset: u8) -> u32 {
        read32(self.bus, self.slot, self.func, offset)
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
        write32(self.bus, self.slot, self.func, offset, u32::MAX);
        let mask = self.read(offset) & !0x3;
        write32(self.bus, self.slot, self.func, offset, bar);
        let size = (!mask).wrapping_add(1) & 0xffff;
        (size != 0).then_some(((bar & !0x3) as u16, size as u16))
    }

    /// Lets the device decode its I/O ports and access memory by DMA.
    pub fn enable_io_and_dma(&self) {
        let reg = self.read(0x04);
        let command = (reg as u16) | COMMAND_IO | COMMAND_BUS_MASTER;
        write32(self.bus, self.slot, self.func, 0x04, (reg & 0xffff_0000) | command as u32);
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
