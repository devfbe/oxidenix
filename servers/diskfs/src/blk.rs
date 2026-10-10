//! virtio-blk driver on the shared virtio transport (crates/virtio).
//!
//! Several requests in flight: each is a chain of a request header, data
//! buffers (scatter-gather, any device addresses: the DMA area or granted
//! pages of a ring client) and a status byte, taken from the queue's free
//! list (`submit`); the device finishes them in any order, and `reap`
//! hands back each one's token with its status. The filesystem's own
//! synchronous I/O (ext2fs's `Device`: metadata, and data on the IPC path)
//! goes through a bounce buffer in the DMA area as one more such request,
//! waiting for its own completion and keeping the others' for `reap`.
//!
//! The driver polls for completions with the device's interrupts off: PCI
//! interrupt lines may be shared between devices (under QEMU this disk
//! shares one with the network card), while the kernel gives each line to
//! a single server.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use virtio::{Buffer, Device, Dma, Queue, PAGE};

const FEATURE_SIZE_MAX: u32 = 1 << 1;
const FEATURE_SEG_MAX: u32 = 1 << 2;
const FEATURE_RO: u32 = 1 << 5;
const FEATURE_FLUSH: u32 = 1 << 9;

// Device configuration: capacity in sectors, largest segment, most
// segments per request.
const CONFIG_CAPACITY: u16 = 0;
const CONFIG_SIZE_MAX: u16 = 8;
const CONFIG_SEG_MAX: u16 = 12;

const REQ_IN: u32 = 0;
const REQ_OUT: u32 = 1;
const REQ_FLUSH: u32 = 4;
const STATUS_OK: u8 = 0;

pub const SECTOR_SIZE: usize = 512;
/// The bounce buffer of the synchronous path: its largest transfer.
const MAX_IO: usize = 128 * 1024;
/// Data buffers per request at most (the device may allow fewer).
const MAX_SEGMENTS: usize = 64;
/// Bytes per request header slot: the 16-byte header, the status byte.
const SLOT_BYTES: usize = 32;
const SLOTS: usize = PAGE / SLOT_BYTES;
/// The DMA area the kernel gives diskfs (see start_diskfs).
pub const DMA_BYTES: usize = 64 * PAGE;
/// Pages of the DMA area left to the ring service (`scratch`).
pub const SCRATCH_PAGES: usize = 8;

/// How long a request may take. A flush can take seconds when the
/// emulator's host disk is slow.
const TIMEOUT_MS: u64 = 30_000;
/// Polls of the used ring before waiting starts to give the CPU away.
pub const FAST_POLLS: u32 = 20_000;
/// The token of the synchronous path's request.
const SYNC: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Read,
    Write,
    Flush,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// No room in the queue now: try again after `reap`.
    Busy,
    /// Beyond the disk, not whole sectors, too many segments, or a write
    /// to a read-only disk.
    Invalid,
}

/// A request in flight, by the head of its chain.
#[derive(Clone, Copy)]
struct InFlight {
    token: u64,
    slot: usize,
}

pub struct VirtioBlk {
    device: Device,
    queue: Queue,
    /// Header slots (header and status byte of a request each) and the
    /// free ones.
    slots: (*mut u8, u64),
    free_slots: Vec<usize>,
    in_flight: Vec<Option<InFlight>>,
    /// Completions the synchronous path took from the device: (token, ok).
    done: VecDeque<(u64, bool)>,
    /// The synchronous path's bounce buffer.
    data: (*mut u8, u64),
    /// A page the device may write anything into (the bytes of a sector a
    /// read does not want), and a page of zeros (what a write pads with).
    sink: u64,
    zeros: u64,
    scratch: (*mut u8, u64),
    max_segments: usize,
    max_segment: usize,
    sectors: u64,
    flush: bool,
    read_only: bool,
}

impl VirtioBlk {
    /// Resets and sets up the device at I/O port `io`; `dma`/`phys` is the
    /// DMA area of `DMA_BYTES`.
    pub fn new(io: u16, dma: *mut u8, phys: u64) -> Result<VirtioBlk, &'static str> {
        let wanted = FEATURE_SIZE_MAX | FEATURE_SEG_MAX | FEATURE_RO | FEATURE_FLUSH;
        let (device, features) = unsafe { Device::new(io, wanted, 0) }?;
        let mut dma = unsafe { Dma::new(dma, phys, DMA_BYTES) };
        let mut queue = device.queue(0, usize::MAX, &mut dma)?;
        if queue.len() < 3 {
            return Err("virtqueue too small");
        }
        queue.disable_interrupts();
        let slots = dma.alloc(PAGE, PAGE)?;
        let data = dma.alloc(MAX_IO, PAGE)?;
        let (_, sink) = dma.alloc(PAGE, PAGE)?;
        let (_, zeros) = dma.alloc(PAGE, PAGE)?;
        let scratch = dma.alloc(SCRATCH_PAGES * PAGE, PAGE)?;
        let mut max_segment = MAX_IO;
        if features & FEATURE_SIZE_MAX != 0 {
            let limit = device.config32(CONFIG_SIZE_MAX) as usize / SECTOR_SIZE * SECTOR_SIZE;
            if limit == 0 {
                return Err("the device takes no whole sector per segment");
            }
            max_segment = max_segment.min(limit);
        }
        // Header and status take two descriptors of a chain.
        let mut max_segments = MAX_SEGMENTS.min(queue.len() - 2);
        if features & FEATURE_SEG_MAX != 0 {
            max_segments = max_segments.min(device.config32(CONFIG_SEG_MAX) as usize);
        }
        if max_segments < 3 {
            return Err("the device takes too few segments per request");
        }
        let sectors = device.config64(CONFIG_CAPACITY);
        let size = queue.len();
        device.driver_ok();
        Ok(VirtioBlk {
            device,
            queue,
            slots,
            free_slots: (0..SLOTS).rev().collect(),
            in_flight: vec![None; size],
            done: VecDeque::new(),
            data,
            sink,
            zeros,
            scratch,
            max_segments,
            max_segment,
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

    /// Data buffers one request may have.
    pub fn max_segments(&self) -> usize {
        self.max_segments
    }

    /// Bytes one data buffer may have.
    pub fn max_segment(&self) -> usize {
        self.max_segment
    }

    /// The device address of a page whose contents nobody reads (for the
    /// sector bytes a read does not want).
    pub fn sink(&self) -> u64 {
        self.sink
    }

    /// The device address of a page of zeros (for what a write pads with).
    pub fn zeros(&self) -> u64 {
        self.zeros
    }

    /// DMA memory for the ring service: (address, device address) of
    /// `SCRATCH_PAGES` pages.
    pub fn scratch(&self) -> (*mut u8, u64) {
        self.scratch
    }

    /// Requests in flight (submitted, not yet reaped).
    pub fn in_flight(&self) -> usize {
        SLOTS - self.free_slots.len() + self.done.len()
    }

    /// Starts a request over `bufs` (device addresses and lengths, whole
    /// sectors in all) at sector `lba`; `reap` returns `token` when it is
    /// done. The caller notifies the device (`kick`) after a batch.
    pub fn submit(&mut self, kind: Kind, lba: u64, bufs: &[(u64, u32)], token: u64) -> Result<(), SubmitError> {
        let bytes: u64 = bufs.iter().map(|&(_, len)| len as u64).sum();
        let sectors = bytes / SECTOR_SIZE as u64;
        let fits = lba.checked_add(sectors).is_some_and(|end| end <= self.sectors);
        let shaped = bytes % SECTOR_SIZE as u64 == 0 && bufs.len() <= self.max_segments && bufs.iter().all(|&(_, l)| l > 0 && l as usize <= self.max_segment);
        let allowed = match kind {
            Kind::Flush => bufs.is_empty() && self.flush,
            Kind::Write => !bufs.is_empty() && !self.read_only,
            Kind::Read => !bufs.is_empty(),
        };
        if !fits || !shaped || !allowed {
            return Err(SubmitError::Invalid);
        }
        if self.queue.free_descriptors() < bufs.len() + 2 {
            return Err(SubmitError::Busy);
        }
        let Some(slot) = self.free_slots.pop() else { return Err(SubmitError::Busy) };
        let (header, header_phys) = (unsafe { self.slots.0.add(slot * SLOT_BYTES) }, self.slots.1 + (slot * SLOT_BYTES) as u64);
        let code = match kind {
            Kind::Read => REQ_IN,
            Kind::Write => REQ_OUT,
            Kind::Flush => REQ_FLUSH,
        };
        unsafe {
            core::ptr::write_volatile(header as *mut u32, code);
            core::ptr::write_volatile(header.add(4) as *mut u32, 0);
            core::ptr::write_volatile(header.add(8) as *mut u64, if kind == Kind::Flush { 0 } else { lba });
            core::ptr::write_volatile(header.add(16), 0xff);
        }
        let mut chain = [Buffer { addr: 0, len: 0, device_writes: false }; MAX_SEGMENTS + 2];
        chain[0] = Buffer { addr: header_phys, len: 16, device_writes: false };
        for (i, &(addr, len)) in bufs.iter().enumerate() {
            chain[1 + i] = Buffer { addr, len, device_writes: kind == Kind::Read };
        }
        chain[1 + bufs.len()] = Buffer { addr: header_phys + 16, len: 1, device_writes: true };
        let head = self.queue.add_chain(&chain[..bufs.len() + 2]).expect("descriptors counted above");
        self.in_flight[head] = Some(InFlight { token, slot });
        Ok(())
    }

    /// Tells the device about the requests submitted since the last kick.
    pub fn kick(&self) {
        self.device.notify(0);
    }

    /// A request the device is done with: (token, whether it succeeded).
    pub fn reap(&mut self) -> Option<(u64, bool)> {
        if let Some(done) = self.done.pop_front() {
            return Some(done);
        }
        self.pop()
    }

    /// The next completion from the device itself.
    fn pop(&mut self) -> Option<(u64, bool)> {
        loop {
            let (head, _) = self.queue.pop_used()?;
            // A head the driver did not hand out is the device's bug: skip it.
            let Some(r) = self.in_flight.get_mut(head).and_then(Option::take) else { continue };
            let status = unsafe { core::ptr::read_volatile(self.slots.0.add(r.slot * SLOT_BYTES + 16)) };
            self.queue.free_chain(head);
            self.free_slots.push(r.slot);
            return Some((r.token, status == STATUS_OK));
        }
    }

    /// Waits until the device finished something (or the disk hangs: the
    /// device may still write the buffers, so diskfs must not go on).
    pub fn wait_any(&mut self) {
        let mut polls = 0u32;
        let deadline = oxrt::uptime_ms() + TIMEOUT_MS;
        while !self.queue.has_used() && self.done.is_empty() {
            polls += 1;
            if polls < FAST_POLLS {
                core::hint::spin_loop();
                continue;
            }
            if oxrt::uptime_ms() > deadline {
                oxrt::println!("diskfs: the disk does not answer");
                oxrt::exit(1);
            }
            oxrt::sched_yield();
        }
    }

    /// Runs one request of the synchronous path over the first `len`
    /// bytes of the bounce buffer, keeping other requests' completions.
    fn request(&mut self, kind: Kind, lba: u64, len: usize) -> Result<(), ()> {
        let bufs = [(self.data.1, len as u32)];
        let bufs = if len > 0 { &bufs[..] } else { &[][..] };
        loop {
            match self.submit(kind, lba, bufs, SYNC) {
                Ok(()) => break,
                Err(SubmitError::Invalid) => return Err(()),
                Err(SubmitError::Busy) => {
                    self.wait_any();
                    while let Some(done) = self.pop() {
                        self.done.push_back(done);
                    }
                }
            }
        }
        self.kick();
        loop {
            self.wait_any();
            while let Some((token, ok)) = self.pop() {
                if token == SYNC {
                    return if ok { Ok(()) } else { Err(()) };
                }
                self.done.push_back((token, ok));
            }
        }
    }

    fn data(&mut self, len: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data.0, len) }
    }

    fn chunk(&self) -> usize {
        MAX_IO.min(self.max_segment)
    }
}

impl ext2fs::Device for VirtioBlk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        let mut sector = lba;
        let chunk = self.chunk();
        for part in buf.chunks_mut(chunk) {
            self.request(Kind::Read, sector, part.len())?;
            part.copy_from_slice(self.data(part.len()));
            sector += (part.len() / SECTOR_SIZE) as u64;
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        let mut sector = lba;
        let chunk = self.chunk();
        for part in buf.chunks(chunk) {
            self.data(part.len()).copy_from_slice(part);
            self.request(Kind::Write, sector, part.len())?;
            sector += (part.len() / SECTOR_SIZE) as u64;
        }
        Ok(())
    }

    /// Empties the device's write cache (a device without one writes
    /// through).
    fn flush(&mut self) -> Result<(), ()> {
        if self.flush {
            self.request(Kind::Flush, 0, 0)?;
        }
        Ok(())
    }

    fn now(&self) -> u32 {
        oxrt::now() as u32
    }

    fn random(&mut self) -> u32 {
        let mut b = [0u8; 4];
        oxrt::getrandom(&mut b);
        u32::from_le_bytes(b)
    }
}
