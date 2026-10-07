//! virtio-blk driver on the shared virtio transport (crates/virtio).
//!
//! One request at a time: a chain of three descriptors (request header,
//! data, status byte) over fixed buffers in the DMA area, so a transfer of
//! up to `MAX_IO` bytes is one request the device moves by DMA. The driver
//! polls for completion with the device's interrupts off: diskfs serves one
//! request at a time anyway, and PCI interrupt lines may be shared between
//! devices (under QEMU this disk shares one with the network card), while
//! the kernel gives each line to a single server.

use virtio::{Device, Dma, Queue, DESC_F_WRITE, PAGE};

const FEATURE_SIZE_MAX: u32 = 1 << 1;
const FEATURE_RO: u32 = 1 << 5;
const FEATURE_FLUSH: u32 = 1 << 9;

// Device configuration: capacity in sectors, largest segment.
const CONFIG_CAPACITY: u16 = 0;
const CONFIG_SIZE_MAX: u16 = 8;

const REQ_IN: u32 = 0;
const REQ_OUT: u32 = 1;
const REQ_FLUSH: u32 = 4;
const STATUS_OK: u8 = 0;

const SECTOR_SIZE: usize = 512;
/// The largest transfer per request.
const MAX_IO: usize = 128 * 1024;
/// The DMA area the kernel gives diskfs (see start_diskfs).
pub const DMA_BYTES: usize = 64 * PAGE;

/// How long a request may take. A flush can take seconds when the
/// emulator's host disk is slow.
const TIMEOUT_MS: u64 = 30_000;
/// Polls of the used ring before waiting starts to give the CPU away.
const FAST_POLLS: u32 = 20_000;

pub struct VirtioBlk {
    device: Device,
    queue: Queue,
    /// Header (16 bytes) and status byte, and the data buffer.
    header: (*mut u8, u64),
    data: (*mut u8, u64),
    max_io: usize,
    sectors: u64,
    flush: bool,
    read_only: bool,
}

impl VirtioBlk {
    /// Resets and sets up the device at I/O port `io`; `dma`/`phys` is the
    /// DMA area of `DMA_BYTES`.
    pub fn new(io: u16, dma: *mut u8, phys: u64) -> Result<VirtioBlk, &'static str> {
        let (device, features) = unsafe { Device::new(io, FEATURE_SIZE_MAX | FEATURE_RO | FEATURE_FLUSH, 0) }?;
        let mut dma = unsafe { Dma::new(dma, phys, DMA_BYTES) };
        let mut queue = device.queue(0, 3, &mut dma)?;
        if queue.len() < 3 {
            return Err("virtqueue too small");
        }
        queue.disable_interrupts();
        let header = dma.alloc(PAGE, PAGE)?;
        let data = dma.alloc(MAX_IO, PAGE)?;
        let mut max_io = MAX_IO;
        if features & FEATURE_SIZE_MAX != 0 {
            let limit = device.config32(CONFIG_SIZE_MAX) as usize / SECTOR_SIZE * SECTOR_SIZE;
            if limit == 0 {
                return Err("the device takes no whole sector per request");
            }
            max_io = max_io.min(limit);
        }
        let sectors = device.config64(CONFIG_CAPACITY);
        device.driver_ok();
        Ok(VirtioBlk {
            device,
            queue,
            header,
            data,
            max_io,
            sectors,
            flush: features & FEATURE_FLUSH != 0,
            read_only: features & FEATURE_RO != 0,
        })
    }

    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Runs one request over the first `len` bytes of the data buffer.
    fn request(&mut self, kind: u32, lba: u64, len: usize) -> Result<(), ()> {
        let (header, header_phys) = self.header;
        let status = unsafe { header.add(16) };
        unsafe {
            core::ptr::write_volatile(header as *mut u32, kind);
            core::ptr::write_volatile(header.add(4) as *mut u32, 0);
            core::ptr::write_volatile(header.add(8) as *mut u64, lba);
            core::ptr::write_volatile(status, 0xff);
        }
        let status_desc = if len > 0 { 2 } else { 1 };
        self.queue.set_desc(0, header_phys, 16, 0, Some(if len > 0 { 1 } else { status_desc }));
        if len > 0 {
            let flags = if kind == REQ_IN { DESC_F_WRITE } else { 0 };
            self.queue.set_desc(1, self.data.1, len as u32, flags, Some(2));
        }
        self.queue.set_desc(status_desc as usize, header_phys + 16, 1, DESC_F_WRITE, None);
        self.queue.push_avail(0);
        self.device.notify(0);
        self.wait()?;
        match unsafe { core::ptr::read_volatile(status) } {
            STATUS_OK => Ok(()),
            _ => Err(()),
        }
    }

    /// Waits for the device to finish the request in flight.
    fn wait(&mut self) -> Result<(), ()> {
        for _ in 0..FAST_POLLS {
            if self.queue.pop_used().is_some() {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        let deadline = oxrt::uptime_ms() + TIMEOUT_MS;
        loop {
            if self.queue.pop_used().is_some() {
                return Ok(());
            }
            if oxrt::uptime_ms() > deadline {
                // The device may still write the buffers: never reuse them.
                oxrt::println!("diskfs: the disk does not answer");
                oxrt::exit(1);
            }
            oxrt::sched_yield();
        }
    }

    fn check(&self, lba: u64, len: usize) -> Result<(), ()> {
        let end = lba.checked_add((len / SECTOR_SIZE) as u64).ok_or(())?;
        if end > self.sectors || len % SECTOR_SIZE != 0 {
            return Err(());
        }
        Ok(())
    }

    fn data(&mut self, len: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data.0, len) }
    }
}

impl ext2fs::Device for VirtioBlk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.check(lba, buf.len())?;
        let mut sector = lba;
        for chunk in buf.chunks_mut(self.max_io) {
            self.request(REQ_IN, sector, chunk.len())?;
            chunk.copy_from_slice(self.data(chunk.len()));
            sector += (chunk.len() / SECTOR_SIZE) as u64;
        }
        Ok(())
    }

    /// Writes and makes the data durable (a flush after the write).
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        self.check(lba, buf.len())?;
        if self.read_only {
            return Err(());
        }
        let mut sector = lba;
        for chunk in buf.chunks(self.max_io) {
            self.data(chunk.len()).copy_from_slice(chunk);
            self.request(REQ_OUT, sector, chunk.len())?;
            sector += (chunk.len() / SECTOR_SIZE) as u64;
        }
        if self.flush {
            self.request(REQ_FLUSH, 0, 0)?;
        }
        Ok(())
    }

    fn now(&self) -> u32 {
        oxrt::now() as u32
    }
}
