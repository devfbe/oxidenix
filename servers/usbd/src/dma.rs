//! The DMA area the kernel granted usbd (`oxrt::dma_map`: physically
//! contiguous, `USBD_DMA_PAGES` pages, the same across restarts), handed
//! out in page runs: the controller's tables and rings, the devices'
//! contexts and transfer rings, the transfers' buffers. A run is aligned
//! to its size rounded up to a power of two, so no buffer crosses a 64 KiB
//! boundary (which one xHCI transfer descriptor's buffer must not) and
//! every structure meets its alignment (64 bytes for contexts and rings,
//! a page for the scratchpad).

use core::sync::atomic::{fence, Ordering};

pub const PAGE: usize = 4096;
/// Must match the kernel's USBD_DMA_PAGES.
pub const PAGES: usize = 1024;

/// A run of the area: where usbd reaches it and where devices do.
#[derive(Debug)]
pub struct Buf {
    virt: *mut u8,
    phys: u64,
    pages: usize,
}

impl Buf {
    pub fn phys(&self) -> u64 {
        self.phys
    }

    pub fn len(&self) -> usize {
        self.pages * PAGE
    }

    /// The run's bytes. Devices write them behind the compiler's back: a
    /// caller reads them only after the device said it is done (an
    /// event), with `acquire` first.
    pub fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.virt, self.len()) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.virt, self.len()) }
    }

    /// The 32-bit word at byte `offset` (volatile: the device's).
    pub fn read32(&self, offset: usize) -> u32 {
        assert!(offset + 4 <= self.len() && offset % 4 == 0);
        unsafe { (self.virt.add(offset) as *const u32).read_volatile() }
    }

    pub fn write32(&mut self, offset: usize, value: u32) {
        assert!(offset + 4 <= self.len() && offset % 4 == 0);
        unsafe { (self.virt.add(offset) as *mut u32).write_volatile(value) }
    }

    pub fn write64(&mut self, offset: usize, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }

    pub fn zero(&mut self) {
        unsafe { core::ptr::write_bytes(self.virt, 0, self.len()) };
        release();
    }
}

/// Orders usbd's stores to DMA memory before what follows (a doorbell, a
/// cycle bit the controller polls).
pub fn release() {
    fence(Ordering::SeqCst);
}

/// Orders what follows after the device's stores that an event announced.
pub fn acquire() {
    fence(Ordering::SeqCst);
}

pub struct Dma {
    base: *mut u8,
    phys: u64,
    /// One bit per page: taken.
    used: [u64; PAGES / 64],
}

impl Dma {
    /// The area at `base` (physical `phys`).
    pub fn new(base: *mut u8, phys: u64) -> Dma {
        Dma { base, phys, used: [0; PAGES / 64] }
    }

    /// A zeroed run of `pages` pages, aligned to its size rounded up to a
    /// power of two; None if no such run is free.
    pub fn alloc(&mut self, pages: usize) -> Option<Buf> {
        if pages == 0 || pages > PAGES {
            return None;
        }
        let align = pages.next_power_of_two();
        let first = (0..PAGES).step_by(align).find(|&p| p + pages <= PAGES && (p..p + pages).all(|q| !self.taken(q)))?;
        for q in first..first + pages {
            self.used[q / 64] |= 1 << (q % 64);
        }
        let mut buf = Buf { virt: unsafe { self.base.add(first * PAGE) }, phys: self.phys + (first * PAGE) as u64, pages };
        buf.zero();
        Some(buf)
    }

    /// A run of at least `bytes` bytes.
    pub fn alloc_bytes(&mut self, bytes: usize) -> Option<Buf> {
        self.alloc(bytes.div_ceil(PAGE).max(1))
    }

    /// Gives a run back (no device may use it any more).
    pub fn free(&mut self, buf: Buf) {
        let first = (buf.phys - self.phys) as usize / PAGE;
        for q in first..first + buf.pages {
            debug_assert!(self.taken(q));
            self.used[q / 64] &= !(1 << (q % 64));
        }
    }

    fn taken(&self, page: usize) -> bool {
        self.used[page / 64] & 1 << (page % 64) != 0
    }

    /// The area's highest device address (for controllers that only reach
    /// 4 GiB).
    pub fn end(&self) -> u64 {
        self.phys + (PAGES * PAGE) as u64
    }
}
