pub mod frame;
pub mod kstack;

use bootloader_api::info::MemoryRegion;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{null_mut, NonNull};
use frame::PhysFrameAllocator;
use linked_list_allocator::Heap;
use crate::sync::IrqSpinLock;
use spin::Once;
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
struct GrowingHeap(IrqSpinLock<Heap>);

#[global_allocator]
static HEAP: GrowingHeap = GrowingHeap(IrqSpinLock::new(Heap::empty()));

unsafe impl GlobalAlloc for GrowingHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        crate::counters::HEAP_ALLOCS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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
        {
            // Lock order FRAMES -> HEAP, taken only here. Nothing allocates
            // from the heap while holding FRAMES (the frame allocator keeps
            // its free list inside the free frames), so this cannot deadlock;
            // try_lock would fail spuriously whenever another CPU holds FRAMES.
            let mut guard = FRAMES.lock();
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
            // The heap never gives memory back: these frames are no longer
            // there for programs, so less can be promised to them.
            let _ = COMMIT_LIMIT.try_update(core::sync::atomic::Ordering::Relaxed, core::sync::atomic::Ordering::Relaxed, |l| {
                Some(l.saturating_sub(want / PAGE))
            });
            true
        }
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

/// Physical frames. Invariant: no heap allocation while this lock is held
/// (the heap grows by taking it).
pub static FRAMES: IrqSpinLock<Option<PhysFrameAllocator>> = IrqSpinLock::new(None);
static PHYS_OFFSET: Once<VirtAddr> = Once::new();
/// A page below 1 MiB, reserved at boot for the other CPUs' start-up code.
static LOW_FRAME: Once<u64> = Once::new();

pub fn low_frame() -> Option<u64> {
    LOW_FRAME.get().copied()
}

/// Identity-maps the page at `phys` (below 4 GiB) in the kernel's page
/// table, so code there keeps running when a CPU turns paging on.
pub fn identity_map(phys: u64) -> Result<(), &'static str> {
    let mut guard = FRAMES.lock();
    let frames = guard.as_mut().ok_or("memory::init not called")?;
    let offset = phys_offset();
    let l4: *mut PageTable = (offset + kernel_l4().start_address().as_u64()).as_mut_ptr();
    let mut mapper = unsafe { OffsetPageTable::new(&mut *l4, offset) };
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(phys));
    let frame = PhysFrame::containing_address(x86_64::PhysAddr::new(phys));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    use x86_64::structures::paging::{mapper::MapToError, Translate};
    match unsafe { mapper.map_to(page, frame, flags, frames) } {
        Ok(flush) => {
            flush.flush();
            Ok(())
        }
        Err(MapToError::PageAlreadyMapped(f)) if f == frame => Ok(()),
        Err(_) => match mapper.translate_addr(VirtAddr::new(phys)) {
            Some(p) if p.as_u64() == phys => Ok(()),
            _ => Err("cannot identity-map the start-up page"),
        },
    }
}
static KERNEL_L4: Once<PhysFrame> = Once::new();

pub fn init(regions: &'static [MemoryRegion], phys_offset: u64) {
    let phys_offset = *PHYS_OFFSET.call_once(|| VirtAddr::new(phys_offset));
    KERNEL_L4.call_once(|| Cr3::read().0);
    let mut frames = PhysFrameAllocator::new(regions, phys_offset);
    // The first fresh frame is the lowest usable one. Other CPUs start in
    // real mode, so their start-up code needs a page below 1 MiB.
    if let Some(low) = frames.allocate_frame().map(|f| f.start_address().as_u64()).filter(|&a| a < 0x10_0000) {
        LOW_FRAME.call_once(|| low);
    }
    map_heap(HEAP_START, HEAP_INITIAL, &mut frames).expect("cannot map the initial kernel heap");
    kstack::init(Cr3::read().0, &mut frames);
    unsafe { HEAP.0.lock().init(HEAP_START as *mut u8, HEAP_INITIAL as usize) };
    frames.enable_refcounts();
    COMMIT_LIMIT.store(
        frames.free_frames().saturating_sub(frame::KERNEL_RESERVE_FRAMES),
        core::sync::atomic::Ordering::Relaxed,
    );
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

/// Virtual window for device registers and firmware tables, outside the
/// physical-memory mapping (which only covers RAM and is cached).
const MMIO_START: u64 = 0xffff_d000_0000_0000;
const MMIO_SIZE: u64 = 1 << 30;
static MMIO_NEXT: IrqSpinLock<u64> = IrqSpinLock::new(MMIO_START);

/// How a physical range is mapped by `map_physical`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Caching {
    /// Device registers: uncached, so every access reaches the device.
    Uncached,
    /// Firmware tables in RAM.
    WriteBack,
}

/// Maps `len` bytes of physical memory at `phys` into the kernel half of
/// every address space and returns the virtual address of `phys`. Done at
/// boot, before processes exist, so their page tables share the mapping.
pub fn map_physical(phys: u64, len: u64, caching: Caching) -> Result<VirtAddr, &'static str> {
    let first = phys & !(PAGE - 1);
    let end = phys.checked_add(len).and_then(|e| e.checked_next_multiple_of(PAGE)).ok_or("range overflows")?;
    let size = end - first;
    let virt = {
        let mut next = MMIO_NEXT.lock();
        let virt = *next;
        if virt + size > MMIO_START + MMIO_SIZE {
            return Err("MMIO window exhausted");
        }
        *next += size;
        virt
    };
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    if caching == Caching::Uncached {
        flags |= PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH;
    }
    let mut guard = FRAMES.lock();
    let frames = guard.as_mut().ok_or("memory::init not called")?;
    let offset = phys_offset();
    let l4: *mut PageTable = (offset + kernel_l4().start_address().as_u64()).as_mut_ptr();
    let mut mapper = unsafe { OffsetPageTable::new(&mut *l4, offset) };
    for i in 0..size / PAGE {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt + i * PAGE));
        let frame = PhysFrame::containing_address(x86_64::PhysAddr::new(first + i * PAGE));
        unsafe { mapper.map_to(page, frame, flags, frames) }.map_err(|_| "map_to failed")?.flush();
    }
    Ok(VirtAddr::new(virt + (phys - first)))
}

/// Returns a virtual pointer to a physical address.
pub fn phys_to_virt(addr: u64) -> *mut u8 {
    (phys_offset() + addr).as_mut_ptr()
}

/// Pages of memory promised to processes (writable private mappings, see
/// process::address_space), pages of the file cache that can be dropped
/// again (fs::cache), and the most both together may be: all usable
/// frames except the kernel's reserve. Committing up front makes running
/// out of memory an ENOMEM at mmap/brk/fork time instead of a fault;
/// cached pages are reclaimed to make room for a commit.
struct Account {
    committed: u64,
    cached: u64,
}

static ACCOUNT: IrqSpinLock<Account> = IrqSpinLock::new(Account { committed: 0, cached: 0 });
static COMMIT_LIMIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Drops up to the given number of reclaimable cache pages; returns how
/// many it dropped (set by the file cache).
static RECLAIM: Once<fn(u64) -> u64> = Once::new();

pub fn set_reclaim(f: fn(u64) -> u64) {
    RECLAIM.call_once(|| f);
}

/// Adds `pages` to the committed or (`cache`) the cached pages if both
/// together stay within the limit, reclaiming cache pages for room. Never
/// called with a page cache lock held (reclaiming takes them).
fn charge(pages: u64, cache: bool) -> bool {
    loop {
        let over = {
            let mut a = ACCOUNT.lock();
            let limit = COMMIT_LIMIT.load(core::sync::atomic::Ordering::Relaxed);
            let total = a.committed.saturating_add(a.cached).saturating_add(pages);
            if total <= limit {
                *(if cache { &mut a.cached } else { &mut a.committed }) += pages;
                return true;
            }
            // Reclaiming can at most empty the cache.
            if a.committed.saturating_add(pages) > limit || a.cached == 0 {
                return false;
            }
            total - limit
        };
        if RECLAIM.get().map_or(0, |reclaim| reclaim(over)) == 0 {
            return false;
        }
    }
}

/// Promises `pages` pages; false (nothing promised) if over the limit.
pub fn commit(pages: u64) -> bool {
    charge(pages, false)
}

pub fn uncommit(pages: u64) {
    ACCOUNT.lock().committed -= pages;
}

/// Accounts `pages` new pages of the file cache; false if they would not
/// fit even after reclaiming others.
pub fn cache_charge(pages: u64) -> bool {
    charge(pages, true)
}

pub fn cache_uncharge(pages: u64) {
    ACCOUNT.lock().cached -= pages;
}

/// (committed, limit) in pages.
pub fn commit_stats() -> (u64, u64) {
    (ACCOUNT.lock().committed, COMMIT_LIMIT.load(core::sync::atomic::Ordering::Relaxed))
}

/// Reclaimable pages of the file cache.
pub fn cached_pages() -> u64 {
    ACCOUNT.lock().cached
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
