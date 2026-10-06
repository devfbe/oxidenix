pub mod frame;

use bootloader_api::info::MemoryRegion;
use frame::PhysFrameAllocator;
use linked_list_allocator::LockedHeap;
use spin::{Mutex, Once};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame,
    Size4KiB,
};
use x86_64::VirtAddr;

// Upper half; the lower half belongs to processes.
pub const HEAP_START: u64 = 0xffff_c000_0000_0000;
pub const HEAP_SIZE: u64 = 16 * 1024 * 1024;

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

pub static FRAMES: Mutex<Option<PhysFrameAllocator>> = Mutex::new(None);
static PHYS_OFFSET: Once<VirtAddr> = Once::new();
static KERNEL_L4: Once<PhysFrame> = Once::new();

pub fn init(regions: &'static [MemoryRegion], phys_offset: u64) {
    let phys_offset = *PHYS_OFFSET.call_once(|| VirtAddr::new(phys_offset));
    KERNEL_L4.call_once(|| Cr3::read().0);
    let mut frames = PhysFrameAllocator::new(regions, phys_offset);
    let mut mapper = unsafe { active_page_table() };

    let first = Page::<Size4KiB>::containing_address(VirtAddr::new(HEAP_START));
    let last = Page::containing_address(VirtAddr::new(HEAP_START + HEAP_SIZE - 1));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    for page in Page::range_inclusive(first, last) {
        let frame = frames.allocate_frame().expect("no frame for kernel heap");
        unsafe { mapper.map_to(page, frame, flags, &mut frames) }
            .expect("heap page already mapped")
            .flush();
    }
    unsafe { HEAP.lock().init(HEAP_START as *mut u8, HEAP_SIZE as usize) };
    frames.enable_refcounts();
    *FRAMES.lock() = Some(frames);
}

/// SAFETY: the caller must ensure that no second `&mut` to the active
/// level-4 table exists at the same time.
pub unsafe fn active_page_table() -> OffsetPageTable<'static> {
    let offset = *PHYS_OFFSET.get().expect("memory::init not called");
    let (l4_frame, _) = Cr3::read();
    let l4: *mut PageTable = (offset + l4_frame.start_address().as_u64()).as_mut_ptr();
    unsafe { OffsetPageTable::new(&mut *l4, offset) }
}

pub fn phys_offset() -> VirtAddr {
    *PHYS_OFFSET.get().expect("memory::init not called")
}

pub fn kernel_l4() -> PhysFrame {
    *KERNEL_L4.get().expect("memory::init not called")
}

/// Returns a virtual pointer to a physical address.
pub fn phys_to_virt(addr: u64) -> *mut u8 {
    (phys_offset() + addr).as_mut_ptr()
}

pub fn with_frames<R>(f: impl FnOnce(&mut PhysFrameAllocator) -> R) -> R {
    f(FRAMES.lock().as_mut().expect("memory::init not called"))
}

pub struct Stats {
    pub total_frames: u64,
    pub used_frames: u64,
    pub heap_used: usize,
    pub heap_free: usize,
}

pub fn stats() -> Stats {
    let (total_frames, used_frames) = FRAMES
        .lock()
        .as_ref()
        .map_or((0, 0), |f| (f.total_frames, f.used_frames));
    let heap = HEAP.lock();
    Stats {
        total_frames,
        used_frames,
        heap_used: heap.used(),
        heap_free: heap.free(),
    }
}
