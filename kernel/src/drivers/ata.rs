//! ATA PIO driver for the secondary disk (primary bus, slave drive).
//!
//! Polling only: the controller's interrupt is disabled, which is fine for
//! QEMU's emulated IDE and keeps the driver independent of the scheduler.

use spin::{Mutex, Once};
use x86_64::instructions::port::Port;

pub const SECTOR_SIZE: usize = 512;

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

/// Drive select for the slave with LBA addressing.
const SLAVE_LBA: u8 = 0xf0;

pub struct Disk {
    sectors: u64,
}

static DISK: Once<Option<Disk>> = Once::new();
static BUS: Mutex<()> = Mutex::new(());

#[derive(Debug)]
pub enum Error {
    NoDisk,
    OutOfRange,
    Device,
}

fn status() -> u8 {
    unsafe { Port::<u8>::new(STATUS_COMMAND).read() }
}

fn wait_not_busy() -> Result<u8, Error> {
    for _ in 0..1_000_000 {
        let s = status();
        if s & STATUS_BSY == 0 {
            return Ok(s);
        }
    }
    Err(Error::Device)
}

fn wait_data() -> Result<(), Error> {
    let s = wait_not_busy()?;
    if s & (STATUS_ERR | STATUS_DF) != 0 || s & STATUS_DRQ == 0 {
        return Err(Error::Device);
    }
    Ok(())
}

fn select(lba: u64, count: u8) {
    unsafe {
        Port::<u8>::new(DRIVE).write(SLAVE_LBA | ((lba >> 24) & 0x0f) as u8);
        Port::<u8>::new(SECTOR_COUNT).write(count);
        Port::<u8>::new(LBA_LOW).write(lba as u8);
        Port::<u8>::new(LBA_MID).write((lba >> 8) as u8);
        Port::<u8>::new(LBA_HIGH).write((lba >> 16) as u8);
    }
}

fn command(cmd: u8) {
    unsafe { Port::<u8>::new(STATUS_COMMAND).write(cmd) };
}

/// Probes the slave drive on the primary bus. Returns its size in sectors.
pub fn init() -> Option<u64> {
    DISK.call_once(|| {
        let _bus = BUS.lock();
        unsafe {
            // nIEN: no interrupts from the controller.
            Port::<u8>::new(CONTROL).write(0x02);
            Port::<u8>::new(DRIVE).write(SLAVE_LBA);
        }
        select(0, 0);
        command(CMD_IDENTIFY);
        if status() == 0 {
            return None;
        }
        wait_data().ok()?;
        let mut id = [0u16; 256];
        let mut data = Port::<u16>::new(DATA);
        for w in id.iter_mut() {
            *w = unsafe { data.read() };
        }
        let sectors = id[60] as u64 | (id[61] as u64) << 16;
        (sectors > 0).then_some(Disk { sectors })
    });
    DISK.get().and_then(|d| d.as_ref()).map(|d| d.sectors)
}

fn disk() -> Result<&'static Disk, Error> {
    DISK.get().and_then(|d| d.as_ref()).ok_or(Error::NoDisk)
}

/// Reads whole sectors starting at `lba` into `buf` (a multiple of 512 bytes).
pub fn read(lba: u64, buf: &mut [u8]) -> Result<(), Error> {
    let disk = disk()?;
    let count = buf.len() / SECTOR_SIZE;
    if lba + count as u64 > disk.sectors || lba + count as u64 >= 1 << 28 {
        return Err(Error::OutOfRange);
    }
    let _bus = BUS.lock();
    for (i, sector) in buf.chunks_exact_mut(SECTOR_SIZE).enumerate() {
        wait_not_busy()?;
        select(lba + i as u64, 1);
        command(CMD_READ);
        wait_data()?;
        let mut data = Port::<u16>::new(DATA);
        for pair in sector.chunks_exact_mut(2) {
            pair.copy_from_slice(&unsafe { data.read() }.to_le_bytes());
        }
    }
    Ok(())
}

/// Writes whole sectors and flushes the drive's write cache.
pub fn write(lba: u64, buf: &[u8]) -> Result<(), Error> {
    let disk = disk()?;
    let count = buf.len() / SECTOR_SIZE;
    if lba + count as u64 > disk.sectors || lba + count as u64 >= 1 << 28 {
        return Err(Error::OutOfRange);
    }
    let _bus = BUS.lock();
    for (i, sector) in buf.chunks_exact(SECTOR_SIZE).enumerate() {
        wait_not_busy()?;
        select(lba + i as u64, 1);
        command(CMD_WRITE);
        wait_data()?;
        let mut data = Port::<u16>::new(DATA);
        for pair in sector.chunks_exact(2) {
            unsafe { data.write(u16::from_le_bytes([pair[0], pair[1]])) };
        }
    }
    wait_not_busy()?;
    command(CMD_FLUSH);
    wait_not_busy()?;
    Ok(())
}
