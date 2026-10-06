use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::info::{MemoryRegion, MemoryRegionKind};
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

const FRAME_SIZE: u64 = 4096;
/// Frames user memory may not take, so the kernel heap can always grow.
pub const KERNEL_RESERVE_FRAMES: u64 = 4096;

/// Hands out fresh frames from the usable regions first; freed frames go to
/// a free list whose `next` pointer is stored in the frame itself.
///
/// Once `enable_refcounts` ran, every frame carries a reference count so that
/// copy-on-write pages can be shared; `deallocate_frame` drops one reference.
pub struct PhysFrameAllocator {
    regions: &'static [MemoryRegion],
    region_idx: usize,
    next: u64,
    free_head: u64,
    phys_offset: VirtAddr,
    refs: Vec<u32>,
    pub total_frames: u64,
    pub used_frames: u64,
}

impl PhysFrameAllocator {
    pub fn new(regions: &'static [MemoryRegion], phys_offset: VirtAddr) -> Self {
        let total_frames = regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .map(|r| (align_down(r.end) - align_up(r.start)) / FRAME_SIZE)
            .sum();
        PhysFrameAllocator {
            regions,
            region_idx: 0,
            next: 0,
            free_head: 0,
            phys_offset,
            refs: Vec::new(),
            total_frames,
            used_frames: 0,
        }
    }

    fn next_fresh(&mut self) -> Option<u64> {
        while let Some(region) = self.regions.get(self.region_idx) {
            if region.kind == MemoryRegionKind::Usable {
                // Frame 0 stays reserved because 0 marks the end of the free list.
                let start = align_up(region.start).max(FRAME_SIZE);
                let addr = self.next.max(start);
                if addr + FRAME_SIZE <= align_down(region.end) {
                    self.next = addr + FRAME_SIZE;
                    return Some(addr);
                }
            }
            self.region_idx += 1;
        }
        None
    }

    fn slot(&self, frame_addr: u64) -> *mut u64 {
        (self.phys_offset + frame_addr).as_mut_ptr()
    }

    /// Needs the heap, so it runs after the heap has been mapped. Frames
    /// handed out before keep a count of 0 and are never freed.
    pub fn enable_refcounts(&mut self) {
        let max = self
            .regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .map(|r| align_down(r.end))
            .max()
            .unwrap_or(0);
        self.refs = vec![0; (max / FRAME_SIZE) as usize];
    }

    fn index(frame: PhysFrame) -> usize {
        (frame.start_address().as_u64() / FRAME_SIZE) as usize
    }

    /// Adds a reference to an allocated frame (it is now shared).
    pub fn share(&mut self, frame: PhysFrame) {
        if let Some(r) = self.refs.get_mut(Self::index(frame)) {
            *r += 1;
        }
    }

    pub fn free_frames(&self) -> u64 {
        self.total_frames - self.used_frames
    }

    /// Whether `n` more frames may go to user memory (or other memory a
    /// user can demand) without eating into the kernel reserve.
    pub fn user_may_take(&self, n: u64) -> bool {
        self.free_frames() >= KERNEL_RESERVE_FRAMES + n
    }

    /// `n` physically contiguous fresh frames for device DMA, or None. The
    /// frames carry one reference that is never dropped. Frames skipped at
    /// a region boundary stay allocated.
    pub fn allocate_contiguous(&mut self, n: u64) -> Option<u64> {
        if n == 0 || !self.user_may_take(n) {
            return None;
        }
        let mut start = self.next_fresh()?;
        let mut len = 1;
        self.claim(start);
        while len < n {
            let addr = self.next_fresh()?;
            self.claim(addr);
            if addr == start + len * FRAME_SIZE {
                len += 1;
            } else {
                start = addr;
                len = 1;
            }
        }
        Some(start)
    }

    fn claim(&mut self, addr: u64) {
        self.used_frames += 1;
        if let Some(r) = self.refs.get_mut((addr / FRAME_SIZE) as usize) {
            *r = 1;
        }
    }

    pub fn refcount(&self, frame: PhysFrame) -> u32 {
        self.refs.get(Self::index(frame)).copied().unwrap_or(0)
    }
}

unsafe impl FrameAllocator<Size4KiB> for PhysFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let addr = if self.free_head != 0 {
            let addr = self.free_head;
            self.free_head = unsafe { self.slot(addr).read() };
            addr
        } else {
            self.next_fresh()?
        };
        self.used_frames += 1;
        let frame = PhysFrame::containing_address(PhysAddr::new(addr));
        if let Some(r) = self.refs.get_mut(Self::index(frame)) {
            *r = 1;
        }
        Some(frame)
    }
}

impl FrameDeallocator<Size4KiB> for PhysFrameAllocator {
    /// Drops one reference; the frame is freed when the last one goes.
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        if let Some(r) = self.refs.get_mut(Self::index(frame)) {
            if *r > 1 {
                *r -= 1;
                return;
            }
            *r = 0;
        }
        let addr = frame.start_address().as_u64();
        unsafe { self.slot(addr).write(self.free_head) };
        self.free_head = addr;
        self.used_frames -= 1;
    }
}

/// Frame source for user memory: refuses to dip into the kernel reserve.
pub struct UserFrames<'a>(pub &'a mut PhysFrameAllocator);

unsafe impl FrameAllocator<Size4KiB> for UserFrames<'_> {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if self.0.user_may_take(1) {
            self.0.allocate_frame()
        } else {
            None
        }
    }
}

fn align_up(x: u64) -> u64 {
    (x + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)
}

fn align_down(x: u64) -> u64 {
    x & !(FRAME_SIZE - 1)
}
