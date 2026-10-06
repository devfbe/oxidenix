use bootloader_api::info::{MemoryRegion, MemoryRegionKind};
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

const FRAME_SIZE: u64 = 4096;

/// Hands out fresh frames from the usable regions first; freed frames go to
/// a free list whose `next` pointer is stored in the frame itself.
pub struct PhysFrameAllocator {
    regions: &'static [MemoryRegion],
    region_idx: usize,
    next: u64,
    free_head: u64,
    phys_offset: VirtAddr,
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
        Some(PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

impl FrameDeallocator<Size4KiB> for PhysFrameAllocator {
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        let addr = frame.start_address().as_u64();
        unsafe { self.slot(addr).write(self.free_head) };
        self.free_head = addr;
        self.used_frames -= 1;
    }
}

fn align_up(x: u64) -> u64 {
    (x + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)
}

fn align_down(x: u64) -> u64 {
    x & !(FRAME_SIZE - 1)
}
