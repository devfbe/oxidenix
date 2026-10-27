//! The xHCI controller's registers (xHCI 1.2, chapter 5): the capability,
//! operational, runtime and doorbell registers and the extended
//! capabilities, in the BAR the kernel mapped uncached (`oxrt::mmio_map`).
//! Every access is a volatile 32-bit one; 64-bit registers are written
//! low half first (as the specification allows for all of them).

/// A window of registers.
#[derive(Clone, Copy)]
pub struct Mmio {
    base: *mut u8,
    len: usize,
}

impl Mmio {
    pub fn new(base: *mut u8, len: usize) -> Mmio {
        Mmio { base, len }
    }

    /// The window from byte `offset` on (None beyond the BAR).
    pub fn at(&self, offset: usize) -> Option<Mmio> {
        (offset < self.len).then(|| Mmio { base: unsafe { self.base.add(offset) }, len: self.len - offset })
    }

    pub fn read32(&self, offset: usize) -> u32 {
        assert!(offset + 4 <= self.len && offset % 4 == 0, "register {offset:#x} outside the BAR");
        unsafe { (self.base.add(offset) as *const u32).read_volatile() }
    }

    pub fn write32(&self, offset: usize, value: u32) {
        assert!(offset + 4 <= self.len && offset % 4 == 0, "register {offset:#x} outside the BAR");
        unsafe { (self.base.add(offset) as *mut u32).write_volatile(value) }
    }

    pub fn write64(&self, offset: usize, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

// Capability registers.
pub const CAPLENGTH: usize = 0x00;
pub const HCSPARAMS1: usize = 0x04;
pub const HCSPARAMS2: usize = 0x08;
pub const HCCPARAMS1: usize = 0x10;
pub const DBOFF: usize = 0x14;
pub const RTSOFF: usize = 0x18;

/// HCCPARAMS1 bits.
pub const HCC_AC64: u32 = 1 << 0;
pub const HCC_CSZ: u32 = 1 << 2;
pub const HCC_PPC: u32 = 1 << 3;

// Operational registers.
pub const USBCMD: usize = 0x00;
pub const USBSTS: usize = 0x04;
pub const PAGESIZE: usize = 0x08;
pub const CRCR: usize = 0x18;
pub const DCBAAP: usize = 0x30;
pub const CONFIG: usize = 0x38;
/// The first port's register set; each is `PORT_STRIDE` bytes.
pub const PORTS: usize = 0x400;
pub const PORT_STRIDE: usize = 0x10;

pub const CMD_RUN: u32 = 1 << 0;
pub const CMD_RESET: u32 = 1 << 1;
pub const CMD_INTE: u32 = 1 << 2;
pub const CMD_HSEE: u32 = 1 << 3;

pub const STS_HALTED: u32 = 1 << 0;
pub const STS_HSE: u32 = 1 << 2;
pub const STS_EINT: u32 = 1 << 3;
pub const STS_CNR: u32 = 1 << 11;
pub const STS_HCE: u32 = 1 << 12;

pub const CRCR_RCS: u64 = 1 << 0;

// PORTSC bits (xHCI 1.2, 5.4.8).
pub const PORT_CCS: u32 = 1 << 0;
pub const PORT_PED: u32 = 1 << 1;
pub const PORT_PR: u32 = 1 << 4;
pub const PORT_PLS_SHIFT: u32 = 5;
pub const PORT_PLS_MASK: u32 = 0xf << 5;
pub const PORT_PP: u32 = 1 << 9;
pub const PORT_SPEED_SHIFT: u32 = 10;
pub const PORT_SPEED_MASK: u32 = 0xf << 10;
pub const PORT_CSC: u32 = 1 << 17;
pub const PORT_PEC: u32 = 1 << 18;
pub const PORT_WRC: u32 = 1 << 19;
pub const PORT_OCC: u32 = 1 << 20;
pub const PORT_PRC: u32 = 1 << 21;
pub const PORT_PLC: u32 = 1 << 22;
pub const PORT_CEC: u32 = 1 << 23;
pub const PORT_WPR: u32 = 1 << 31;
/// Every change bit (write 1 to clear).
pub const PORT_CHANGES: u32 = PORT_CSC | PORT_PEC | PORT_WRC | PORT_OCC | PORT_PRC | PORT_PLC | PORT_CEC;
/// The bits a write must give back as read to leave them as they are
/// (Linux's `xhci_port_state_to_neutral`): the read-only ones (CCS, OCA,
/// the speed, DR) and the read-write ones (PLS, PP, PIC, the wake
/// enables). Everything else written as 0 changes nothing: PED and the
/// change bits clear only when written with 1, PR and WPR act only then.
pub const PORT_NEUTRAL: u32 = (1 << 0) | (1 << 3) | (0xf << 10) | (1 << 30) | (0xf << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);

/// Port link states (PLS) a warm reset gets a USB 3 port out of.
pub const PLS_INACTIVE: u32 = 6;
pub const PLS_COMPLIANCE: u32 = 10;

// Interrupter 0's registers (runtime base + 0x20).
pub const IR0: usize = 0x20;
pub const IMAN: usize = 0x00;
pub const IMOD: usize = 0x04;
pub const ERSTSZ: usize = 0x08;
pub const ERSTBA: usize = 0x10;
pub const ERDP: usize = 0x18;

pub const IMAN_IP: u32 = 1 << 0;
pub const IMAN_IE: u32 = 1 << 1;
/// ERDP's Event Handler Busy (write 1 to clear).
pub const ERDP_EHB: u64 = 1 << 3;

// Extended capabilities (xHCI 1.2, 7).
pub const EXT_LEGACY: u8 = 1;
pub const EXT_PROTOCOL: u8 = 2;
/// USBLEGSUP's semaphores, and USBLEGCTLSTS's SMI enables and status.
pub const LEGACY_BIOS_OWNED: u32 = 1 << 16;
pub const LEGACY_OS_OWNED: u32 = 1 << 24;
/// SMI enables to clear (USB, host system error, OS ownership, PCI
/// command, BAR) and the status bits to clear (write 1).
pub const LEGACY_SMI_ENABLES: u32 = (1 << 0) | (1 << 4) | (0x7 << 13);
pub const LEGACY_SMI_STATUS: u32 = 0x7 << 29;
