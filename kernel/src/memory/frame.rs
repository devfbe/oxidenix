//! The physical frame allocator: fresh frames from the bootloader's usable regions, freed ones
//! on an intrusive free list, with reference counts for shared and copy-on-write frames and
//! the memory group each frame is charged to (ADR 0024).

use super::memcg;
use bootloader_api::info::{MemoryRegion, MemoryRegionKind};
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

const FRAME_SIZE: u64 = 4096;
/// Frames user memory may not take, so the kernel heap can always grow.
pub const KERNEL_RESERVE_FRAMES: u64 = 4096;

/// Hands out fresh frames from the usable regions first; freed frames go to
/// a free list whose `next` pointer is stored in the frame itself.
///
/// Every frame carries a reference count so that copy-on-write pages can be
/// shared; `deallocate_frame` drops one reference. It also carries the
/// memory group it is charged to (`memcg`; the root for none): set when it is
/// allocated for a group (`UserFrames`, `charge`), uncharged when its last
/// reference goes.
///
/// Both live in the frame table, physical memory the allocator reserves for
/// itself at boot (as Linux's `mem_map`), not on the kernel heap: it is sized
/// by the highest usable address (6 bytes per frame: 55 MiB on a machine with
/// 32 GiB whose memory reaches up to 37 GiB), more than the initial heap
/// holds, and the heap can grow only once the allocator exists.
pub struct PhysFrameAllocator {
    regions: &'static [MemoryRegion],
    region_idx: usize,
    next: u64,
    free_head: u64,
    phys_offset: VirtAddr,
    /// The frame table's physical range, which fresh frames skip.
    table: core::ops::Range<u64>,
    refs: &'static mut [u32],
    owners: &'static mut [memcg::Id],
    pub total_frames: u64,
    pub used_frames: u64,
}

impl PhysFrameAllocator {
    pub fn new(regions: &'static [MemoryRegion], phys_offset: VirtAddr) -> Self {
        let usable = || regions.iter().filter(|r| r.kind == MemoryRegionKind::Usable);
        let total_frames = usable().map(|r| (align_down(r.end) - align_up(r.start)) / FRAME_SIZE).sum();
        let slots = usable().map(|r| align_down(r.end)).max().unwrap_or(0) / FRAME_SIZE;
        let refs_len = slots * core::mem::size_of::<u32>() as u64;
        let len = align_up(refs_len + slots * core::mem::size_of::<memcg::Id>() as u64);
        // At the end of the highest region it fits in, which leaves low
        // memory (the APs' start-up page below 1 MiB, DMA) alone.
        let table_start = usable()
            .filter(|r| align_down(r.end) >= len && align_down(r.end) - len >= align_up(r.start).max(FRAME_SIZE))
            .map(|r| align_down(r.end) - len)
            .max()
            .unwrap_or_else(|| panic!("no usable memory region holds the frame table ({} bytes)", len));
        // Zeroed: no references, every frame the root's.
        const _: () = assert!(memcg::ROOT == 0);
        let table = (phys_offset + table_start).as_mut_ptr::<u8>();
        unsafe { core::ptr::write_bytes(table, 0, len as usize) };
        // SAFETY: the range is usable RAM that no frame is handed out of
        // (`next_fresh` skips it), mapped at `phys_offset`, and both slices
        // are aligned (the table starts on a page, `refs_len` is a multiple
        // of 4) and disjoint.
        let (refs, owners) = unsafe {
            (
                core::slice::from_raw_parts_mut(table.cast::<u32>(), slots as usize),
                core::slice::from_raw_parts_mut(table.add(refs_len as usize).cast::<memcg::Id>(), slots as usize),
            )
        };
        PhysFrameAllocator {
            regions,
            region_idx: 0,
            next: 0,
            free_head: 0,
            phys_offset,
            table: table_start..table_start + len,
            refs,
            owners,
            total_frames,
            used_frames: len / FRAME_SIZE,
        }
    }

    fn next_fresh(&mut self) -> Option<u64> {
        while let Some(region) = self.regions.get(self.region_idx) {
            if region.kind == MemoryRegionKind::Usable {
                // Frame 0 stays reserved because 0 marks the end of the free list.
                let start = align_up(region.start).max(FRAME_SIZE);
                let mut addr = self.next.max(start);
                if self.table.contains(&addr) {
                    addr = self.table.end;
                }
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

    fn index(frame: PhysFrame) -> usize {
        (frame.start_address().as_u64() / FRAME_SIZE) as usize
    }

    /// The memory group an allocated frame is charged to (`memcg::ROOT`: none).
    pub fn owner(&self, frame: PhysFrame) -> memcg::Id {
        self.owners.get(Self::index(frame)).copied().unwrap_or(memcg::ROOT)
    }

    /// Charges a frame just allocated (not charged yet) to group `id`; the
    /// limit is the caller's to have checked (`memcg::may_charge`).
    pub fn charge(&mut self, frame: PhysFrame, id: memcg::Id) {
        if id == memcg::ROOT {
            return;
        }
        if let Some(owner) = self.owners.get_mut(Self::index(frame)) {
            debug_assert_eq!(*owner, memcg::ROOT, "a frame charged twice");
            *owner = id;
            memcg::charged(id, 1);
        }
    }

    /// How many frames could carry a group (the range `reparent` walks).
    pub fn frame_slots(&self) -> usize {
        self.owners.len()
    }

    /// Moves the frames of [from, to) charged to `id` to the root (a dying
    /// group's: `memcg::collect`).
    pub fn reparent(&mut self, id: memcg::Id, from: usize, to: usize) {
        let to = to.min(self.owners.len());
        let mut moved = 0;
        for owner in self.owners[from.min(to)..to].iter_mut().filter(|o| **o == id) {
            *owner = memcg::ROOT;
            moved += 1;
        }
        if moved > 0 {
            memcg::uncharged(id, moved);
        }
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
        if let Some(owner) = self.owners.get_mut(Self::index(frame)) {
            if *owner != memcg::ROOT {
                memcg::uncharged(core::mem::replace(owner, memcg::ROOT), 1);
            }
        }
        let addr = frame.start_address().as_u64();
        unsafe { self.slot(addr).write(self.free_head) };
        self.free_head = addr;
        self.used_frames -= 1;
    }
}

/// Frame source for user memory: refuses to dip into the kernel reserve, and
/// charges each frame to a memory group, within its limit unless forced
/// (`memcg::Target`).
pub struct UserFrames<'a> {
    frames: &'a mut PhysFrameAllocator,
    target: memcg::Target,
}

impl<'a> UserFrames<'a> {
    /// Frames for the running task, charged to its group (`memcg::Target::current`).
    pub fn new(frames: &'a mut PhysFrameAllocator) -> Self {
        Self::charging(frames, memcg::Target::current())
    }

    /// Frames charged to `target`.
    pub fn charging(frames: &'a mut PhysFrameAllocator, target: memcg::Target) -> Self {
        UserFrames { frames, target }
    }

    /// The allocator itself (to free or share frames meanwhile).
    pub fn allocator(&mut self) -> &mut PhysFrameAllocator {
        self.frames
    }
}

unsafe impl FrameAllocator<Size4KiB> for UserFrames<'_> {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if !self.frames.user_may_take(1) || !memcg::may_charge(self.target, 1) {
            return None;
        }
        let frame = self.frames.allocate_frame()?;
        self.frames.charge(frame, self.target.id());
        Some(frame)
    }
}

fn align_up(x: u64) -> u64 {
    (x + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)
}

fn align_down(x: u64) -> u64 {
    x & !(FRAME_SIZE - 1)
}
