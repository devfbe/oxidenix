pub mod frame;

use bootloader_api::info::MemoryRegion;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{null_mut, NonNull};
use frame::PhysFrameAllocator;
use linked_list_allocator::Heap;
use spin::{Mutex, Once};
use x86_64::instructions::interrupts::without_interrupts;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame,
    Size4KiB,
};
use x86_64::VirtAddr;

// Upper half; the lower half belongs to processes.
pub const HEAP_START: u64 = 0xffff_c000_0000_0000;
const HEAP_INITIAL: u64 = 8 * 1024 * 1024;
/// Virtual space reserved for the heap; physical frames are added on demand.
const HEAP_MAX: u64 = 1024 * 1024 * 1024;
const HEAP_GROW_STEP: u64 = 1024 * 1024;
const PAGE: u64 = 4096;

/// Kernel heap that grows by mapping more frames when an allocation fails,
/// so it is bounded by physical memory instead of a fixed size.
struct GrowingHeap(Mutex<Heap>);

#[global_allocator]
static HEAP: GrowingHeap = GrowingHeap(Mutex::new(Heap::empty()));

unsafe impl GlobalAlloc for GrowingHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        loop {
            if let Ok(p) = self.0.lock().allocate_first_fit(layout) {
                return p.as_ptr();
            }
            if !self.grow(layout) {
                return null_mut();
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.lock().deallocate(NonNull::new_unchecked(ptr), layout) };
    }
}

impl GrowingHeap {
    fn grow(&self, layout: Layout) -> bool {
        let Some(want) = (layout.size() as u64)
            .checked_add(layout.align() as u64)
            .map(|w| w.max(HEAP_GROW_STEP))
            .and_then(|w| w.checked_next_multiple_of(PAGE))
        else {
            return false;
        };
        without_interrupts(|| {
            // try_lock: an allocation made while the frame lock is held must
            // fail instead of deadlocking.
            let Some(mut guard) = FRAMES.try_lock() else { return false };
            let Some(frames) = guard.as_mut() else { return false };
            let mut heap = self.0.lock();
            let top = heap.top() as u64;
            let fits = top.checked_add(want).is_some_and(|end| end <= HEAP_START + HEAP_MAX);
            if !fits || frames.free_frames() < want / PAGE + 8 {
                return false;
            }
            if map_heap(top, want, frames).is_err() {
                return false;
            }
            unsafe { heap.extend(want as usize) };
            true
        })
    }
}

fn map_heap(start: u64, len: u64, frames: &mut PhysFrameAllocator) -> Result<(), ()> {
    let mut mapper = unsafe { active_page_table() };
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    let first = Page::<Size4KiB>::containing_address(VirtAddr::new(start));
    let last = Page::containing_address(VirtAddr::new(start + len - 1));
    for page in Page::range_inclusive(first, last) {
        let frame = frames.allocate_frame().ok_or(())?;
        // The heap's upper-half tables are shared by every address space.
        match unsafe { mapper.map_to(page, frame, flags, frames) } {
            Ok(flush) => flush.flush(),
            Err(_) => {
                unsafe { frames.deallocate_frame(frame) };
                return Err(());
            }
        }
    }
    Ok(())
}

pub static FRAMES: Mutex<Option<PhysFrameAllocator>> = Mutex::new(None);
static PHYS_OFFSET: Once<VirtAddr> = Once::new();
static KERNEL_L4: Once<PhysFrame> = Once::new();

pub fn init(regions: &'static [MemoryRegion], phys_offset: u64) {
    let phys_offset = *PHYS_OFFSET.call_once(|| VirtAddr::new(phys_offset));
    KERNEL_L4.call_once(|| Cr3::read().0);
    let mut frames = PhysFrameAllocator::new(regions, phys_offset);
    map_heap(HEAP_START, HEAP_INITIAL, &mut frames).expect("cannot map the initial kernel heap");
    unsafe { HEAP.0.lock().init(HEAP_START as *mut u8, HEAP_INITIAL as usize) };
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
    let heap = HEAP.0.lock();
    Stats {
        total_frames,
        used_frames,
        heap_used: heap.used(),
        heap_free: heap.free(),
    }
}
