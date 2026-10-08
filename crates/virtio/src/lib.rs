//! The virtio legacy PCI transport (all registers in I/O space) and split
//! virtqueues, for the user-space drivers (netd: network, diskfs: block).
//!
//! Queues and buffers live in the server's DMA area from the kernel
//! (physically contiguous, mapped once), which `Dma` hands out.

#![no_std]

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
/// Device-specific configuration (without MSI-X).
const CONFIG: u16 = 0x14;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;

/// The device writes into the buffer (otherwise it reads it).
pub const DESC_F_WRITE: u16 = 2;
/// The descriptor continues in `next`.
pub const DESC_F_NEXT: u16 = 1;

pub const PAGE: usize = 4096;

fn align(x: usize, to: usize) -> usize {
    (x + to - 1) & !(to - 1)
}

/// Bytes the legacy layout needs for a queue of `size` entries.
fn queue_bytes(size: usize) -> usize {
    align(16 * size + 6 + 2 * size, PAGE) + align(6 + 8 * size, PAGE)
}

/// The DMA area, handed out front to back.
pub struct Dma {
    virt: *mut u8,
    phys: u64,
    len: usize,
    next: usize,
}

impl Dma {
    /// # Safety
    /// `virt` must map `len` bytes of physically contiguous memory at `phys`
    /// that nothing else uses.
    pub unsafe fn new(virt: *mut u8, phys: u64, len: usize) -> Dma {
        Dma { virt, phys, len, next: 0 }
    }

    /// `bytes` zeroed bytes aligned to `to`: (address, physical address).
    pub fn alloc(&mut self, bytes: usize, to: usize) -> Result<(*mut u8, u64), &'static str> {
        let at = align(self.next, to);
        if at + bytes > self.len {
            return Err("DMA area too small");
        }
        self.next = at + bytes;
        // A restarted driver gets the area of its predecessor: clear it.
        let virt = unsafe { self.virt.add(at) };
        unsafe { core::ptr::write_bytes(virt, 0, bytes) };
        Ok((virt, self.phys + at as u64))
    }
}

/// A device on the legacy transport.
pub struct Device {
    io: u16,
}

impl Device {
    /// Resets the device and accepts the features in `wanted` it offers;
    /// fails if it lacks one of `required`. Returns the accepted features.
    ///
    /// # Safety
    /// The I/O ports `io..` must belong to a virtio device (legacy
    /// interface) this process drives alone.
    pub unsafe fn new(io: u16, wanted: u32, required: u32) -> Result<(Device, u32), &'static str> {
        let features = unsafe {
            outb(io + DEVICE_STATUS, 0);
            outb(io + DEVICE_STATUS, STATUS_ACKNOWLEDGE);
            outb(io + DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
            inl(io + DEVICE_FEATURES)
        };
        if features & required != required {
            return Err("the device lacks a required feature");
        }
        let accepted = features & (wanted | required);
        unsafe { outl(io + GUEST_FEATURES, accepted) };
        Ok((Device { io }, accepted))
    }

    /// Sets up queue `index` with at most `max` entries in `dma`.
    pub fn queue(&self, index: u16, max: usize, dma: &mut Dma) -> Result<Queue, &'static str> {
        unsafe { outw(self.io + QUEUE_SELECT, index) };
        let size = unsafe { inw(self.io + QUEUE_SIZE) } as usize;
        if size == 0 {
            return Err("the virtqueue does not exist");
        }
        // The legacy interface fixes the size: the device uses all of it,
        // even if the driver only fills `max` entries.
        let (ring, phys) = dma.alloc(queue_bytes(size), PAGE)?;
        unsafe { outl(self.io + QUEUE_PFN, (phys / PAGE as u64) as u32) };
        let usable = size.min(max);
        let mut queue = Queue {
            size,
            usable,
            desc: ring,
            avail: unsafe { ring.add(16 * size) },
            used: unsafe { ring.add(align(16 * size + 6 + 2 * size, PAGE)) },
            last_used: 0,
            free_head: 0,
            free: usable,
        };
        // The free list for `add_chain`, through the descriptors' `next`.
        for i in 0..usable {
            queue.set_desc(i, 0, 0, 0, (i + 1 < usable).then_some(i as u16 + 1));
        }
        Ok(queue)
    }

    /// Tells the device the driver is ready.
    pub fn driver_ok(&self) {
        unsafe { outb(self.io + DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK) };
    }

    /// Tells the device that queue `index` has new buffers.
    pub fn notify(&self, index: u16) {
        unsafe { outw(self.io + QUEUE_NOTIFY, index) };
    }

    /// Acknowledges an interrupt (reading the ISR clears it).
    pub fn ack_interrupt(&self) -> u8 {
        unsafe { inb(self.io + ISR_STATUS) }
    }

    pub fn config8(&self, offset: u16) -> u8 {
        unsafe { inb(self.io + CONFIG + offset) }
    }

    pub fn config32(&self, offset: u16) -> u32 {
        unsafe { inl(self.io + CONFIG + offset) }
    }

    pub fn config64(&self, offset: u16) -> u64 {
        self.config32(offset) as u64 | (self.config32(offset + 4) as u64) << 32
    }
}

/// One split virtqueue in the legacy memory layout.
///
/// A driver either manages the descriptors itself (`set_desc`, then
/// `push_avail`: netd's fixed buffers) or takes chains from the queue's
/// free list (`add_chain`, and `free_chain` once the device returned
/// them: diskfs, with several requests in flight); not both.
pub struct Queue {
    size: usize,
    /// Entries the driver uses (descriptor indexes below this).
    usable: usize,
    desc: *mut u8,
    avail: *mut u8,
    used: *mut u8,
    /// Next entry of the used ring to look at.
    last_used: u16,
    /// The free descriptors, a list through their `next` fields, and how
    /// many there are.
    free_head: u16,
    free: usize,
}

/// A buffer of a chain (`Queue::add_chain`): `len` bytes at the device
/// address `addr`, which the device writes if `device_writes`.
#[derive(Clone, Copy, Debug)]
pub struct Buffer {
    pub addr: u64,
    pub len: u32,
    pub device_writes: bool,
}

impl Queue {
    /// Descriptors the driver may use.
    pub fn len(&self) -> usize {
        self.usable
    }

    pub fn is_empty(&self) -> bool {
        self.usable == 0
    }

    /// Points descriptor `i` at `len` bytes at physical `addr`; with
    /// `next`, the chain continues there.
    pub fn set_desc(&mut self, i: usize, addr: u64, len: u32, flags: u16, next: Option<u16>) {
        assert!(i < self.usable);
        let d = unsafe { self.desc.add(16 * i) };
        let flags = if next.is_some() { flags | DESC_F_NEXT } else { flags & !DESC_F_NEXT };
        unsafe {
            write_volatile(d as *mut u64, addr);
            write_volatile(d.add(8) as *mut u32, len);
            write_volatile(d.add(12) as *mut u16, flags);
            write_volatile(d.add(14) as *mut u16, next.unwrap_or(0));
        }
    }

    /// Asks the device not to interrupt when it finishes a chain (for a
    /// driver that polls). The device's interrupt line may be shared with
    /// another device, and an interrupt nobody acknowledges would keep it
    /// asserted.
    pub fn disable_interrupts(&mut self) {
        const AVAIL_F_NO_INTERRUPT: u16 = 1;
        unsafe { write_volatile(self.avail as *mut u16, AVAIL_F_NO_INTERRUPT) };
        fence(Ordering::SeqCst);
    }

    /// Hands the chain starting at descriptor `head` to the device (the
    /// caller then notifies it).
    pub fn push_avail(&mut self, head: usize) {
        unsafe {
            let idx = read_volatile(self.avail.add(2) as *const u16);
            write_volatile(self.avail.add(4 + 2 * (idx as usize % self.size)) as *mut u16, head as u16);
            fence(Ordering::SeqCst);
            write_volatile(self.avail.add(2) as *mut u16, idx.wrapping_add(1));
            fence(Ordering::SeqCst);
        }
    }

    /// The next chain the device is done with: (head, bytes written).
    pub fn pop_used(&mut self) -> Option<(usize, usize)> {
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

    /// Free descriptors (for `add_chain`).
    pub fn free_descriptors(&self) -> usize {
        self.free
    }

    fn desc_next(&self, i: usize) -> (u16, u16) {
        let d = unsafe { self.desc.add(16 * i) };
        unsafe { (read_volatile(d.add(12) as *const u16), read_volatile(d.add(14) as *const u16)) }
    }

    /// Takes `bufs.len()` descriptors from the free list, chains them over
    /// `bufs` and hands the chain to the device (the caller then notifies
    /// it); returns its head, which `pop_used` reports when the device is
    /// done. None if too few descriptors are free.
    pub fn add_chain(&mut self, bufs: &[Buffer]) -> Option<usize> {
        if bufs.is_empty() || bufs.len() > self.free {
            return None;
        }
        let head = self.free_head as usize;
        let mut i = head;
        for (n, b) in bufs.iter().enumerate() {
            let (_, next) = self.desc_next(i);
            let last = n + 1 == bufs.len();
            let flags = if b.device_writes { DESC_F_WRITE } else { 0 };
            self.set_desc(i, b.addr, b.len, flags, (!last).then_some(next));
            if last {
                self.free_head = next;
            }
            i = next as usize;
        }
        self.free -= bufs.len();
        self.push_avail(head);
        Some(head)
    }

    /// Returns the chain from `head` (from `pop_used`) to the free list.
    pub fn free_chain(&mut self, head: usize) {
        let mut i = head;
        loop {
            self.free += 1;
            let (flags, next) = self.desc_next(i);
            if flags & DESC_F_NEXT == 0 {
                // The chain's tail points to the old free list.
                self.set_desc(i, 0, 0, 0, Some(self.free_head));
                break;
            }
            i = next as usize;
        }
        self.free_head = head as u16;
    }

    /// Whether the device finished a chain `pop_used` has not returned.
    pub fn has_used(&self) -> bool {
        fence(Ordering::SeqCst);
        unsafe { read_volatile(self.used.add(2) as *const u16) != self.last_used }
    }
}
