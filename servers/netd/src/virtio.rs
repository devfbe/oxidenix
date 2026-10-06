//! virtio-net driver (legacy PCI interface, all registers in I/O space).
//!
//! The DMA area from the kernel holds both virtqueues and fixed packet
//! buffers; every descriptor points at its own buffer, so a descriptor
//! index identifies a buffer.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use oxrt::port::{inb, inl, inw, outb, outl, outw};

// Legacy register offsets in the I/O BAR.
const DEVICE_FEATURES: u16 = 0x00;
const GUEST_FEATURES: u16 = 0x04;
const QUEUE_PFN: u16 = 0x08;
const QUEUE_SIZE: u16 = 0x0c;
const QUEUE_SELECT: u16 = 0x0e;
const QUEUE_NOTIFY: u16 = 0x10;
const DEVICE_STATUS: u16 = 0x12;
const ISR_STATUS: u16 = 0x13;
const MAC: u16 = 0x14;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const FEATURE_MAC: u32 = 1 << 5;

const DESC_F_WRITE: u16 = 2;

const RX: u16 = 0;
const TX: u16 = 1;

const PAGE: usize = 4096;
/// Buffers per queue (the device's queue may be larger; the rest stays unused).
const BUFFERS: usize = 64;
const BUFFER_SIZE: usize = 2048;
/// The legacy `virtio_net_hdr` in front of every frame (no mergeable buffers).
const NET_HDR: usize = 10;
pub const MTU: usize = 1514;

/// One split virtqueue in the legacy memory layout.
struct Queue {
    size: usize,
    desc: *mut u8,
    avail: *mut u8,
    used: *mut u8,
    /// Next entry of the used ring to look at.
    last_used: u16,
    buffers: *mut u8,
    buffers_phys: u64,
}

fn align(x: usize) -> usize {
    (x + PAGE - 1) & !(PAGE - 1)
}

/// Bytes the legacy layout needs for a queue of `size` entries.
fn queue_bytes(size: usize) -> usize {
    align(16 * size + 6 + 2 * size) + align(6 + 8 * size)
}

impl Queue {
    unsafe fn set_desc(&mut self, i: usize, len: u32, flags: u16) {
        let d = unsafe { self.desc.add(16 * i) };
        let addr = self.buffers_phys + (i * BUFFER_SIZE) as u64;
        unsafe {
            write_volatile(d as *mut u64, addr);
            write_volatile(d.add(8) as *mut u32, len);
            write_volatile(d.add(12) as *mut u16, flags);
            write_volatile(d.add(14) as *mut u16, 0);
        }
    }

    /// Hands descriptor `i` to the device.
    unsafe fn push_avail(&mut self, i: usize) {
        unsafe {
            let idx = read_volatile(self.avail.add(2) as *const u16);
            write_volatile(self.avail.add(4 + 2 * (idx as usize % self.size)) as *mut u16, i as u16);
            fence(Ordering::SeqCst);
            write_volatile(self.avail.add(2) as *mut u16, idx.wrapping_add(1));
            fence(Ordering::SeqCst);
        }
    }

    /// The next descriptor the device is done with: (index, length written).
    unsafe fn pop_used(&mut self) -> Option<(usize, usize)> {
        unsafe {
            fence(Ordering::SeqCst);
            let idx = read_volatile(self.used.add(2) as *const u16);
            if idx == self.last_used {
                return None;
            }
            let e = self.used.add(4 + 8 * (self.last_used as usize % self.size));
            self.last_used = self.last_used.wrapping_add(1);
            let id = read_volatile(e as *const u32) as usize;
            let len = read_volatile(e.add(4) as *const u32) as usize;
            Some((id, len))
        }
    }

    fn buffer(&mut self, i: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.buffers.add(i * BUFFER_SIZE), BUFFER_SIZE) }
    }
}

pub struct VirtioNet {
    io: u16,
    rx: Queue,
    tx: Queue,
    /// Transmit descriptors not in use by the device.
    tx_free: alloc::vec::Vec<usize>,
    pub mac: [u8; 6],
}

impl VirtioNet {
    /// Resets and sets up the device; `dma`/`phys` is the DMA area.
    pub fn new(io: u16, dma: *mut u8, phys: u64, dma_len: usize) -> Result<VirtioNet, &'static str> {
        unsafe {
            outb(io + DEVICE_STATUS, 0);
            outb(io + DEVICE_STATUS, STATUS_ACKNOWLEDGE);
            outb(io + DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
            let features = inl(io + DEVICE_FEATURES);
            if features & FEATURE_MAC == 0 {
                return Err("the device has no MAC address");
            }
            outl(io + GUEST_FEATURES, FEATURE_MAC);
        }
        let mut offset = 0;
        let mut queue = |index: u16| -> Result<Queue, &'static str> {
            unsafe { outw(io + QUEUE_SELECT, index) };
            let size = unsafe { inw(io + QUEUE_SIZE) } as usize;
            if size < BUFFERS {
                return Err("virtqueue too small");
            }
            let ring = offset;
            offset += queue_bytes(size);
            let buffers = offset;
            offset += BUFFERS * BUFFER_SIZE;
            if offset > dma_len {
                return Err("DMA area too small");
            }
            // A restarted netd gets the DMA area of its predecessor; the
            // reset device starts with empty rings, so must the memory.
            unsafe { core::ptr::write_bytes(dma.add(ring), 0, queue_bytes(size)) };
            unsafe { outl(io + QUEUE_PFN, ((phys + ring as u64) / PAGE as u64) as u32) };
            let desc = unsafe { dma.add(ring) };
            Ok(Queue {
                size,
                desc,
                avail: unsafe { desc.add(16 * size) },
                used: unsafe { desc.add(align(16 * size + 6 + 2 * size)) },
                last_used: 0,
                buffers: unsafe { dma.add(buffers) },
                buffers_phys: phys + buffers as u64,
            })
        };
        let mut rx = queue(RX)?;
        let mut tx = queue(TX)?;
        // Every receive buffer goes to the device right away.
        for i in 0..BUFFERS {
            unsafe {
                rx.set_desc(i, BUFFER_SIZE as u32, DESC_F_WRITE);
                rx.push_avail(i);
            }
        }
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = unsafe { inb(io + MAC + i as u16) };
        }
        unsafe {
            outb(io + DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
            outw(io + QUEUE_NOTIFY, RX);
        }
        for i in 0..BUFFERS {
            unsafe { tx.set_desc(i, 0, 0) };
        }
        Ok(VirtioNet { io, rx, tx, tx_free: (0..BUFFERS).collect(), mac })
    }

    /// Acknowledges an interrupt (reading the ISR clears it).
    pub fn ack_interrupt(&mut self) {
        unsafe { inb(self.io + ISR_STATUS) };
    }

    /// Passes the next received frame to `f`, then returns the buffer to
    /// the device.
    pub fn receive<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let (i, len) = unsafe { self.rx.pop_used()? };
        let len = len.clamp(NET_HDR, BUFFER_SIZE);
        let result = f(&self.rx.buffer(i)[NET_HDR..len]);
        unsafe {
            self.rx.push_avail(i);
            outw(self.io + QUEUE_NOTIFY, RX);
        }
        Some(result)
    }

    pub fn has_received(&mut self) -> bool {
        fence(Ordering::SeqCst);
        unsafe { read_volatile(self.rx.used.add(2) as *const u16) != self.rx.last_used }
    }

    /// Takes back transmit buffers the device has sent.
    fn reclaim(&mut self) {
        while let Some((i, _)) = unsafe { self.tx.pop_used() } {
            self.tx_free.push(i);
        }
    }

    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        !self.tx_free.is_empty()
    }

    /// Sends a frame of `len` bytes that `f` fills in.
    pub fn send<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        self.reclaim();
        let i = self.tx_free.pop().expect("can_send checked before");
        let len = len.min(MTU);
        let buf = self.tx.buffer(i);
        buf[..NET_HDR].fill(0);
        let result = f(&mut buf[NET_HDR..NET_HDR + len]);
        unsafe {
            self.tx.set_desc(i, (NET_HDR + len) as u32, 0);
            self.tx.push_avail(i);
            outw(self.io + QUEUE_NOTIFY, TX);
        }
        result
    }
}
