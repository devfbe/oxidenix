//! ATA PIO driver for the slave drive on the primary bus, running in user
//! space with the I/O ports the kernel granted via ioperm.

use oxrt::port::{inb, inw, outb, outw};

const DATA: u16 = 0x1f0;
const SECTOR_COUNT: u16 = 0x1f2;
const LBA_LOW: u16 = 0x1f3;
const LBA_MID: u16 = 0x1f4;
const LBA_HIGH: u16 = 0x1f5;
const DRIVE: u16 = 0x1f6;
const STATUS_COMMAND: u16 = 0x1f7;
const CONTROL: u16 = 0x3f6;

const STATUS_ERR: u8 = 0x01;
const STATUS_DRQ: u8 = 0x08;
const STATUS_DF: u8 = 0x20;
const STATUS_BSY: u8 = 0x80;

const CMD_READ: u8 = 0x20;
const CMD_WRITE: u8 = 0x30;
const CMD_FLUSH: u8 = 0xe7;
const CMD_IDENTIFY: u8 = 0xec;
const SLAVE_LBA: u8 = 0xf0;
const SECTOR_SIZE: usize = 512;

/// How long a command may keep the drive busy. A cache flush can take
/// seconds when the emulator's host disk is slow.
const TIMEOUT_MS: u64 = 30_000;
/// Status polls before waiting starts to give the CPU away.
const FAST_POLLS: u32 = 10_000;

pub struct Ata {
    sectors: u64,
}

fn status() -> u8 {
    unsafe { inb(STATUS_COMMAND) }
}

fn wait_not_busy() -> Result<u8, ()> {
    for _ in 0..FAST_POLLS {
        let s = status();
        if s & STATUS_BSY == 0 {
            return Ok(s);
        }
    }
    let deadline = oxrt::uptime_ms() + TIMEOUT_MS;
    loop {
        let s = status();
        if s & STATUS_BSY == 0 {
            return Ok(s);
        }
        if oxrt::uptime_ms() > deadline {
            return Err(());
        }
        oxrt::sched_yield();
    }
}

fn wait_data() -> Result<(), ()> {
    let s = wait_not_busy()?;
    if s & (STATUS_ERR | STATUS_DF) != 0 || s & STATUS_DRQ == 0 {
        return Err(());
    }
    Ok(())
}

fn select(lba: u64, count: u8) {
    unsafe {
        outb(DRIVE, SLAVE_LBA | ((lba >> 24) & 0x0f) as u8);
        outb(SECTOR_COUNT, count);
        outb(LBA_LOW, lba as u8);
        outb(LBA_MID, (lba >> 8) as u8);
        outb(LBA_HIGH, (lba >> 16) as u8);
    }
}

impl Ata {
    pub fn probe() -> Option<Ata> {
        unsafe {
            outb(CONTROL, 0x02); // no interrupts from the controller
            outb(DRIVE, SLAVE_LBA);
        }
        select(0, 0);
        unsafe { outb(STATUS_COMMAND, CMD_IDENTIFY) };
        if status() == 0 {
            return None;
        }
        wait_data().ok()?;
        let mut id = [0u16; 256];
        for w in id.iter_mut() {
            *w = unsafe { inw(DATA) };
        }
        let sectors = id[60] as u64 | (id[61] as u64) << 16;
        (sectors > 0).then_some(Ata { sectors })
    }

    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    fn check(&self, lba: u64, len: usize) -> Result<(), ()> {
        let end = lba.checked_add((len / SECTOR_SIZE) as u64).ok_or(())?;
        if end > self.sectors || end >= 1 << 28 || len % SECTOR_SIZE != 0 {
            return Err(());
        }
        Ok(())
    }
}

impl ext2fs::Device for Ata {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.check(lba, buf.len())?;
        for (i, sector) in buf.chunks_exact_mut(SECTOR_SIZE).enumerate() {
            wait_not_busy()?;
            select(lba + i as u64, 1);
            unsafe { outb(STATUS_COMMAND, CMD_READ) };
            wait_data()?;
            for pair in sector.chunks_exact_mut(2) {
                pair.copy_from_slice(&unsafe { inw(DATA) }.to_le_bytes());
            }
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        self.check(lba, buf.len())?;
        for (i, sector) in buf.chunks_exact(SECTOR_SIZE).enumerate() {
            wait_not_busy()?;
            select(lba + i as u64, 1);
            unsafe { outb(STATUS_COMMAND, CMD_WRITE) };
            wait_data()?;
            for pair in sector.chunks_exact(2) {
                unsafe { outw(DATA, u16::from_le_bytes([pair[0], pair[1]])) };
            }
        }
        wait_not_busy()?;
        unsafe { outb(STATUS_COMMAND, CMD_FLUSH) };
        wait_not_busy()?;
        Ok(())
    }

    fn now(&self) -> u32 {
        oxrt::now() as u32
    }
}
