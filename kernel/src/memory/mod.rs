//! Kernel memory: the frame allocator, the kernel heap (slab size classes over a first-fit heap),
//! mappings of physical memory, the commit and page-cache accounting with the overcommit
//! policy, and the OOM killer.

pub mod frame;
pub mod kstack;
pub mod oom;

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
/// so it is bounded by physical memory instead of a fixed size. Objects of
/// up to 2 KiB come from slabs of size classes (`slab`: O(1), a lock per
/// class), whose pages the heap supplies; larger ones from the heap itself
/// (first fit, one lock).
struct GrowingHeap(IrqSpinLock<Heap>);

#[global_allocator]
static HEAP: GrowingHeap = GrowingHeap(IrqSpinLock::new(Heap::empty()));

/// Free slots of each size class.
static SLABS: [IrqSpinLock<slab::FreeList>; slab::CLASSES] = [const { IrqSpinLock::new(slab::FreeList::new()) }; slab::CLASSES];

unsafe impl GlobalAlloc for GrowingHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        crate::counters::HEAP_ALLOCS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let Some(c) = slab::class(&layout) else { return self.alloc_large(layout) };
        if let Some(p) = SLABS[c].lock().pop() {
            return p.as_ptr();
        }
        // A new slab for the class (no lock held meanwhile: the heap may
        // grow, and another CPU may fill the list first; both are fine).
        let Some(page) = NonNull::new(self.alloc_large(slab::slab_layout())) else { return null_mut() };
        let mut list = SLABS[c].lock();
        unsafe { list.add_slab(page, c) };
        list.pop().map_or(null_mut(), |p| p.as_ptr())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        match slab::class(&layout) {
            Some(c) => unsafe { SLABS[c].lock().push(ptr) },
            None => unsafe { self.0.lock().deallocate(ptr, layout) },
        }
    }
}

impl GrowingHeap {
    /// From the heap itself, growing it as needed.
    fn alloc_large(&self, layout: Layout) -> *mut u8 {
        loop {
            if let Ok(p) = self.0.lock().allocate_first_fit(layout) {
                return p.as_ptr();
            }
            // Slabs whose slots are all free first, then more memory.
            if self.reclaim() == 0 && !self.grow(layout) {
                return null_mut();
            }
        }
    }

    /// Gives the heap back the slabs whose slots are all free (lock order
    /// SLABS -> HEAP, taken only here; nothing takes HEAP and then SLABS).
    fn reclaim(&self) -> usize {
        let mut given = 0;
        for (c, slabs) in SLABS.iter().enumerate() {
            given += slabs.lock().reclaim(c, |slab| unsafe { self.0.lock().deallocate(slab, slab::slab_layout()) });
        }
        given
    }

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
            let _ = USER_PAGES.try_update(core::sync::atomic::Ordering::Relaxed, core::sync::atomic::Ordering::Relaxed, |l| {
                Some(l.saturating_sub(want / PAGE))
            });
            // The heap took from the free frames (the kernel's reserve if
            // need be): below the low watermark, the background reclaimer
            // makes up for it. Not woken from here (the frames and the
            // heap are locked, and any lock may be held around an
            // allocation): the next timer tick does (`reclaim_tick`).
            if !frames.user_may_take(low_watermark(frames)) {
                RECLAIM_KICK.store(true, core::sync::atomic::Ordering::Release);
            }
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
    USER_PAGES.store(
        frames.free_frames().saturating_sub(frame::KERNEL_RESERVE_FRAMES),
        core::sync::atomic::Ordering::Relaxed,
    );
    TOTAL_PAGES.store(frames.total_frames, core::sync::atomic::Ordering::Relaxed);
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
/// process::address_space; tmpfs pages, the Linux servers' heaps, paged
/// objects' pages: Linux's Committed_AS), counted in every overcommit mode
/// (`Overcommit`). In the strict mode, the default, they may be at most
/// the commit limit (`commit_limit`: all usable frames except the kernel's
/// reserve, Linux's `overcommit_memory=2` with a ratio of 100%), so running
/// out of memory is an ENOMEM at mmap/brk/fork time instead of a fault; in
/// the others a fault that finds no frame after reclaim calls the OOM
/// killer (`oom`), which ends the process its badness picks.
///
/// Pages of the file cache (`cached`) are counted, not committed, as on
/// Linux, where they are no part of Committed_AS: they live in the frames
/// that commitments have not claimed yet (memory promised but not touched,
/// such as the stacks of threads, is most of what is promised), and when a
/// commitment claims one and none is free, clean cache pages are reclaimed
/// for it (`user_frame`), those that programs map included. What cannot be
/// dropped at once, dirty and pinned cache pages
/// (`fs::cache::unavailable_pages`), counts against the limit as taken
/// (strict mode): a commit fails while committed and those together would
/// exceed it, and a store that makes them exceed it waits for write-back
/// (`fs::cache::balance_dirty`). So every committed page has a frame: free,
/// or a clean cache page's, which reclaim can always take (the fault that
/// needs it waits for write-back meanwhile, `reclaim_retry`).
struct Account {
    committed: u64,
    cached: u64,
}

static ACCOUNT: IrqSpinLock<Account> = IrqSpinLock::new(Account { committed: 0, cached: 0 });
/// The frames user memory may have: all usable ones but the kernel's
/// reserve, less what the kernel heap took since (it never gives frames
/// back). The base of the commit limit and of what is scaled by memory
/// (the dirty ratios, tmpfs's size, the instances' heap reserves).
static USER_PAGES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The overcommit policy, as Linux's `vm.overcommit_memory`,
/// `vm.overcommit_ratio` and `vm.overcommit_kbytes` (the machine's state:
/// set through `/proc/sys/vm` by a tree with the host grant only, ADR 0013).
pub mod overcommit {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// Heuristic: a commit fails only if it alone is more than all memory
    /// (Linux's `OVERCOMMIT_GUESS`, its default).
    pub const GUESS: u64 = 0;
    /// Every commit succeeds (`OVERCOMMIT_ALWAYS`).
    pub const ALWAYS: u64 = 1;
    /// Strict: committed memory stays within the commit limit
    /// (`OVERCOMMIT_NEVER`), oxidenix's default (ADR 0013).
    pub const NEVER: u64 = 2;

    static MODE: AtomicU64 = AtomicU64::new(NEVER);
    /// The commit limit in percent of user memory (Linux's default is 50;
    /// without swap that would leave half of memory unpromised: 100).
    static RATIO: AtomicU64 = AtomicU64::new(100);
    /// The commit limit in KiB instead (0: the ratio applies).
    static KBYTES: AtomicU64 = AtomicU64::new(0);

    pub fn mode() -> u64 {
        MODE.load(Relaxed)
    }

    pub fn ratio() -> u64 {
        RATIO.load(Relaxed)
    }

    pub fn kbytes() -> u64 {
        KBYTES.load(Relaxed)
    }

    /// Whether the strict mode holds: then every committed page has a
    /// frame, and what serves that promise (counting dirty and pinned
    /// cache pages as taken, waiting for their write-back) applies.
    pub fn strict() -> bool {
        mode() == NEVER
    }

    /// Sets the mode (0..=2, else EINVAL); the old one.
    pub fn set_mode(mode: u64) -> Result<u64, i64> {
        if mode > NEVER {
            return Err(crate::process::errno::EINVAL);
        }
        Ok(MODE.swap(mode, Relaxed))
    }

    /// Sets the ratio (at most `i32::MAX`, Linux's int, else EINVAL), which
    /// then applies: the KiB go to 0, as Linux's handlers do. The old ratio.
    pub fn set_ratio(ratio: u64) -> Result<u64, i64> {
        if ratio > i32::MAX as u64 {
            return Err(crate::process::errno::EINVAL);
        }
        KBYTES.store(0, Relaxed);
        Ok(RATIO.swap(ratio, Relaxed))
    }

    /// Sets the limit in KiB, which then applies (the ratio goes to 0);
    /// the old value. (0 leaves both at 0: a limit of nothing, as on Linux.)
    pub fn set_kbytes(kbytes: u64) -> Result<u64, i64> {
        RATIO.store(0, Relaxed);
        Ok(KBYTES.swap(kbytes, Relaxed))
    }
}

/// The commit limit (Linux's CommitLimit, which it shows in every mode):
/// `overcommit_kbytes` if set, else `overcommit_ratio` percent of user
/// memory (there is no swap).
pub fn commit_limit() -> u64 {
    let kbytes = overcommit::kbytes();
    if kbytes != 0 {
        return kbytes / (PAGE / 1024);
    }
    let base = USER_PAGES.load(core::sync::atomic::Ordering::Relaxed);
    (base as u128 * overcommit::ratio() as u128 / 100).min(u64::MAX as u128) as u64
}

/// The frames user memory may have (`USER_PAGES`): what the dirty
/// ratios, tmpfs's size and other shares of memory are taken of.
pub fn user_pages() -> u64 {
    USER_PAGES.load(core::sync::atomic::Ordering::Relaxed)
}

/// All usable frames (Linux's totalram_pages: what the heuristic mode
/// and the OOM killer's badness are measured against).
static TOTAL_PAGES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn total_pages() -> u64 {
    TOTAL_PAGES.load(core::sync::atomic::Ordering::Relaxed)
}
/// Drops up to the given number of reclaimable cache pages, with `true`
/// also those used lately (no second chance); returns how many it dropped
/// (set by the file cache).
static RECLAIM: Once<fn(u64, bool) -> u64> = Once::new();

pub fn set_reclaim(f: fn(u64, bool) -> u64) {
    RECLAIM.call_once(|| f);
}

/// Promises `pages` pages; false (nothing promised) if the overcommit
/// mode refuses them: strict, over the limit; heuristic, more than all
/// memory at once (Linux's `__vm_enough_memory`, without swap); never when
/// overcommitting always.
pub fn commit(pages: u64) -> bool {
    let refused = match overcommit::mode() {
        overcommit::ALWAYS => false,
        overcommit::GUESS => pages > total_pages(),
        _ => {
            // Cache pages reclaim cannot drop now (dirty, pinned) are not
            // there for the commitment: they count as taken until
            // write-back or the grant ends (stores wait meanwhile rather
            // than crowd out what was promised, `fs::cache::balance_dirty`).
            let unavailable = crate::fs::cache::unavailable_pages();
            let mut a = ACCOUNT.lock();
            if a.committed.saturating_add(pages).saturating_add(unavailable) <= commit_limit() {
                a.committed += pages;
                return true;
            }
            true
        }
    };
    if refused {
        // The Linux servers give back what they keep committed and free
        // (as much as this commit lacked, at least).
        SHRINK_PAGES.fetch_max(pages.max(1), core::sync::atomic::Ordering::Relaxed);
        wake_reclaimer();
        return false;
    }
    ACCOUNT.lock().committed += pages;
    true
}

/// Promises as many as `max` pages as fit now (none: no sign of pressure,
/// nobody is asked to shrink); how many. For speculative reserves (all of
/// them unless strict: what fits then is no promise of frames anyway).
pub fn commit_some(max: u64) -> u64 {
    if !overcommit::strict() {
        ACCOUNT.lock().committed += max;
        return max;
    }
    let unavailable = crate::fs::cache::unavailable_pages();
    let mut a = ACCOUNT.lock();
    let n = commit_limit().saturating_sub(a.committed.saturating_add(unavailable)).min(max);
    a.committed += n;
    n
}

/// A commit was refused at the limit: the background reclaimer asks the
/// Linux servers to shrink by (at least) this many pages
/// (`linux::post_shrink`); 0 if none was.
static SHRINK_PAGES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Whether a commit of `pages` failed only for the cache pages that are
/// dirty or pinned now (write-back will make room), not for memory
/// promised. (Only the strict mode counts those pages.)
pub fn commit_blocked_by_cache(pages: u64) -> bool {
    let unavailable = crate::fs::cache::unavailable_pages();
    let (committed, limit) = commit_stats();
    overcommit::strict() && unavailable > 0 && committed.saturating_add(pages) <= limit
}

/// Whether a commit of `pages` would fit now (the cache's dirty and
/// pinned pages counted as taken, as `commit` counts them).
fn commit_fits(pages: u64) -> bool {
    let (committed, limit) = commit_stats();
    committed.saturating_add(pages).saturating_add(crate::fs::cache::unavailable_pages()) <= limit
}

/// How long a commit waits for write-back at most (all its waits
/// together), and how often it looks again.
const CACHE_WAIT: u64 = 30_000_000_000;
const CACHE_RECHECK: u64 = 100_000_000;

/// For a commit of `pages` that failed only for dirty or pinned cache
/// pages, with no lock held that write-back may need (an address space's):
/// asks the pagers to write back and waits until the commit would fit,
/// killably, until `deadline` (set at the first wait, `CACHE_WAIT` on).
/// Whether to try the commit again.
///
/// Only where no lock is held (an address space unlocked by
/// `Mm::retrying` or `Mm::committing`); never from within an allocation.
pub fn wait_for_cache(pages: u64, deadline: &mut Option<u64>) -> bool {
    debug_assert!(x86_64::instructions::interrupts::are_enabled(), "waiting for write-back with a spinlock held");
    let end = *deadline.get_or_insert_with(|| crate::time::now() + CACHE_WAIT);
    loop {
        // Room as soon as write-back made enough (not only once nothing is
        // dirty or pinned any more), none ever if memory promised is in
        // the way.
        if commit_fits(pages) {
            return true;
        }
        if !commit_blocked_by_cache(pages) {
            return false;
        }
        let now = crate::time::now();
        if now >= end || crate::process::kill::dying() {
            return false;
        }
        crate::fs::cache::ask_writeback(pages);
        crate::process::sched::prepare_to_wait(reclaim_chan()).sleep_until((now + CACHE_RECHECK).min(end));
    }
}

pub fn uncommit(pages: u64) {
    ACCOUNT.lock().committed -= pages;
}

/// Counts `pages` new pages of the file cache.
pub fn cache_charge(pages: u64) {
    ACCOUNT.lock().cached += pages;
}

pub fn cache_uncharge(pages: u64) {
    ACCOUNT.lock().cached -= pages;
}

/// (committed, limit) in pages.
pub fn commit_stats() -> (u64, u64) {
    (ACCOUNT.lock().committed, commit_limit())
}

/// Pages of the file cache (disk files').
pub fn cached_pages() -> u64 {
    ACCOUNT.lock().cached
}

/// Cache pages one reclaim for a frame drops at least (Linux's
/// SWAP_CLUSTER_MAX), so a run of allocations does not reclaim page by page.
const RECLAIM_BATCH: u64 = 32;

/// Reclaims cache pages for `n` frames (at least `RECLAIM_BATCH`); how
/// many it dropped. Only with interrupts on: then no spinlock is held that
/// reclaim takes, and it may unmap pages (TLB shootdowns). The caller holds
/// no page cache lock. With `force`, pages used lately go too.
fn reclaim_for(n: u64, force: bool) -> u64 {
    use core::sync::atomic::Ordering::Relaxed;
    if !x86_64::instructions::interrupts::are_enabled() {
        return 0;
    }
    // After a reclaim that found nothing, the allocations that would
    // reclaim again at once take from the watermark instead for a moment:
    // a large cache with nothing to drop (dirty, in use) is not scanned
    // whole for every frame.
    let now = crate::time::now();
    if !force && now < FRUITLESS_UNTIL.load(Relaxed) {
        return 0;
    }
    let freed = RECLAIM.get().map_or(0, |reclaim| reclaim(n.max(RECLAIM_BATCH), force));
    if freed == 0 {
        FRUITLESS_UNTIL.store(now + FRUITLESS_PAUSE, Relaxed);
    }
    freed
}

/// One reclaim for `n` frames that takes pages used lately too (with no
/// lock held: the pager's thread before it is told to write back).
pub fn reclaim_forced(n: u64) -> u64 {
    reclaim_for(n, true)
}

/// Until when (`time::now`) a reclaim that found nothing makes the next
/// ones skip, and for how long.
static FRUITLESS_UNTIL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
const FRUITLESS_PAUSE: u64 = 10_000_000;

/// Free frames above the kernel's reserve that allocations for user
/// memory leave for those that cannot reclaim (page tables made with the
/// frames locked, a fork's copies, allocations with interrupts off): below,
/// they reclaim cache pages first (Linux's low watermark; about 1/128 of
/// memory, at least 1 MiB).
fn low_watermark(f: &PhysFrameAllocator) -> u64 {
    (f.total_frames / 128).max(256)
}

/// A frame for user memory (or memory a user can demand: page tables,
/// cache pages): a free one above the kernel's reserve and the low
/// watermark, else one that reclaiming cache pages gives back, else one
/// of the watermark's. None if there is none at all and reclaim found
/// nothing to drop now. Never called with the frames locked.
pub fn user_frame() -> Option<PhysFrame> {
    loop {
        let (frame, low) = with_frames(|f| {
            let frame = if f.user_may_take(1 + low_watermark(f)) { frame::UserFrames(f).allocate_frame() } else { None };
            (frame, !f.user_may_take(low_watermark(f)))
        });
        if low {
            wake_reclaimer();
        }
        if frame.is_some() {
            return frame;
        }
        if reclaim_for(1, false) == 0 {
            // (An OOM victim's own faults may go into the reserve, so it
            // gets to its end: Linux's TIF_MEMDIE.)
            return with_frames(|f| frame::UserFrames(f).allocate_frame())
                .or_else(|| crate::process::sched::current().group.oom_victim.load(core::sync::atomic::Ordering::Acquire).then(emergency_frame).flatten());
        }
    }
}

/// A frame of the kernel's reserve for an allocation that must not fail
/// when user memory is exhausted (Linux's memory reserves): the Linux
/// server's allocations that cannot fail (its heap's commits from the
/// instance's reserve, `linux::heap_frame`) and an OOM victim's faults.
/// Only the reserve's upper half: the lower is the kernel heap's alone.
/// The background reclaimer calls the OOM killer when user memory stays
/// exhausted (`reclaimer`), which refills it.
pub fn emergency_frame() -> Option<PhysFrame> {
    with_frames(|f| if f.free_frames() > frame::KERNEL_RESERVE_FRAMES / 2 { f.allocate_frame() } else { None })
}

/// Wakes the allocations throttled in `reclaim_retry` (memory came back:
/// an OOM victim's).
pub fn wake_throttled() {
    crate::process::sched::wakeup(reclaim_chan());
}

/// The background reclaimer (Linux's kswapd): woken when the free frames
/// above the kernel's reserve come below the low watermark, it reclaims
/// cache pages until they are above the high one (twice the low), so the
/// allocations that cannot reclaim (page tables made with the frames
/// locked, a fork's copies, kernel stacks, anything with interrupts off)
/// find frames without depending on direct reclaim, which a fruitless
/// reclaim pauses for a moment. It holds no lock while it reclaims, so it
/// can unmap pages from every address space that is not busy. When user
/// memory stays exhausted with nothing to reclaim, it calls the OOM killer
/// for those allocations (`oom`).
static RECLAIMER_WANTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn reclaimer_chan() -> usize {
    &RECLAIMER_WANTED as *const _ as usize
}

/// A wakeup of the background reclaimer asked for where it could not be
/// made (the kernel heap's growth): the timer tick makes it.
static RECLAIM_KICK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Called by the timer tick (interrupt context): makes a wakeup asked for
/// by the kernel heap's growth.
pub fn reclaim_tick() {
    if RECLAIM_KICK.load(core::sync::atomic::Ordering::Relaxed) && RECLAIM_KICK.swap(false, core::sync::atomic::Ordering::AcqRel) {
        wake_reclaimer();
    }
}

/// Wakes the background reclaimer (cheap when it is awake already).
pub fn wake_reclaimer() {
    if !RECLAIMER_WANTED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        crate::process::sched::wakeup(reclaimer_chan());
    }
}

/// Starts the background reclaimer's kernel thread.
pub fn start_reclaimer() -> Result<(), i64> {
    crate::process::sched::spawn_kernel_thread("reclaim", reclaimer)
}

fn reclaimer() -> ! {
    loop {
        // A kick is consumed before the work it asks for, so one that comes
        // while it works (and finds the flag clear: it wakes nobody) makes
        // the next round run instead of being lost; registered as a sleeper
        // before looking, so a kick between the look and the sleep wakes it.
        let wait = crate::process::sched::prepare_to_wait(reclaimer_chan());
        if !RECLAIMER_WANTED.swap(false, core::sync::atomic::Ordering::AcqRel) {
            wait.sleep();
            continue;
        }
        drop(wait);
        // Up to the high watermark, giving pages used lately their second
        // chance. Only below the low watermark, and only after a whole
        // sweep of the cache without it dropped anything, it takes those
        // too (`force`); when even that drops nothing (write-back, busy
        // address spaces, nothing reclaimable), it rests, longer each time
        // up to a second (Linux's kswapd_failures), and looks again while
        // still low. A kick (a deferred reference, a new shortage) ends a
        // rest at once.
        let (mut force, mut fruitless, mut failures) = (false, 0u64, 0u32);
        // Since when user memory is exhausted with nothing to reclaim.
        let mut starved_since: Option<u64> = None;
        loop {
            crate::fs::cache::drop_deferred();
            let (short, low) = with_frames(|f| {
                let high = 2 * low_watermark(f);
                (!f.user_may_take(high), !f.user_may_take(low_watermark(f)))
            });
            // After a commit was refused at the limit, the Linux servers give
            // back what they can do without (their shrinkers, `EVENT_SHRINK`,
            // scaled by what was refused; each server at most once a
            // second). Kept for later if it cannot be posted now.
            let asked = SHRINK_PAGES.swap(0, core::sync::atomic::Ordering::Relaxed);
            if asked > 0 && !crate::process::linux::post_shrink(asked) {
                SHRINK_PAGES.fetch_max(asked, core::sync::atomic::Ordering::Relaxed);
            }
            if !short {
                break;
            }
            let freed = RECLAIM.get().map_or(0, |reclaim| reclaim(RECLAIM_BATCH * 4, force));
            if freed > 0 {
                (force, fruitless, failures) = (false, 0, 0);
                continue;
            }
            if !low {
                // Between the watermarks with nothing to drop unforced:
                // done for now.
                break;
            }
            fruitless += 1;
            // Rounds that look at the whole cache (each at most
            // `fs::cache`'s scan budget, three turns for the second chances).
            let sweep = 3 * (cached_pages() / RECLAIM_SWEEP_PAGES + 1);
            if !force && fruitless < sweep {
                continue;
            }
            if !force {
                force = true;
                continue;
            }
            crate::fs::cache::ask_writeback(RECLAIM_BATCH);
            // Clean cache, used lately or not, gave nothing: the servers are
            // asked for what is missing up to the high watermark (not
            // before: their caches are worth more than clean file pages).
            let lacking = with_frames(|f| (2 * low_watermark(f) + frame::KERNEL_RESERVE_FRAMES).saturating_sub(f.free_frames()));
            crate::process::linux::post_shrink(lacking.max(1));
            // User memory exhausted (no frame above the kernel's reserve)
            // for as long as an allocation retries reclaim before it calls
            // the OOM killer (`reclaim_retry`): the allocations that cannot
            // wait for memory (page tables made with the frames locked, the
            // kernel heap, the Linux servers' own, which take from the
            // reserve) have nobody else to call it.
            let now = crate::time::now();
            if with_frames(|f| f.user_may_take(1)) {
                starved_since = None;
            } else {
                let since = *starved_since.get_or_insert(now);
                let retries = if crate::fs::cache::unavailable_pages() > 0 { RECLAIM_RETRIES_IO } else { RECLAIM_RETRIES };
                if now - since >= retries as u64 * RECLAIM_THROTTLE {
                    oom::out_of_memory(1);
                    starved_since = None;
                }
            }
            let rest = (RECLAIM_THROTTLE << failures.min(4)).min(RECLAIMER_REST_MAX);
            failures = failures.saturating_add(1);
            let wait = crate::process::sched::prepare_to_wait(reclaimer_chan());
            if !RECLAIMER_WANTED.swap(false, core::sync::atomic::Ordering::AcqRel) {
                wait.sleep_until(crate::time::now() + rest);
            }
            (force, fruitless) = (false, 0);
        }
    }
}

/// The pages one reclaim pass looks at (`fs::cache`'s scan budget), for
/// the background reclaimer's sweep; the longest it rests when nothing
/// can be dropped.
const RECLAIM_SWEEP_PAGES: u64 = 4096;
const RECLAIMER_REST_MAX: u64 = 1_000_000_000;

/// Makes `n` frames free for user memory above the low watermark,
/// reclaiming cache pages if they are not (for allocations made with the
/// frames locked: page tables, a copy). Whether `n` are free (if need be,
/// within the watermark); another CPU may take them before the caller.
pub fn ensure_user_frames(n: u64) -> bool {
    loop {
        if with_frames(|f| f.user_may_take(n + low_watermark(f))) {
            return true;
        }
        wake_reclaimer();
        if reclaim_for(n, false) == 0 {
            return with_frames(|f| f.user_may_take(n));
        }
    }
}

/// How often an allocation that found no frame tries again after reclaim
/// dropped nothing (Linux's MAX_RECLAIM_RETRIES), and how long it waits
/// before each try (for write-back to clean dirty pages, for a busy
/// address space, for programs to end): about 1.6 s without any progress
/// before the OOM killer is called (`oom::out_of_memory`).
pub const RECLAIM_RETRIES: u32 = 16;
/// The same while dirty or pinned cache pages may still become droppable
/// (write-back in progress): about 30 s.
const RECLAIM_RETRIES_IO: u32 = 300;
const RECLAIM_THROTTLE: u64 = 100_000_000;

/// For an allocation that failed for want of a frame, with no lock held:
/// reclaims cache pages for `n` frames, waiting a little for write-back
/// or busy address spaces if nothing could be dropped now (as Linux's
/// reclaim throttling). Once `tries` rounds made no progress, the OOM
/// killer ends a process (`oom::out_of_memory`) and the rounds start anew.
/// Whether the allocation should be tried again: false if no process could
/// be killed, or the thread is dying (also by the OOM killer's choice).
/// After a round without progress, pages used lately go too (as Linux's
/// reclaim raises its priority): a committed page needs its frame more
/// than a cached page that is in use (which is read again when used).
pub fn reclaim_retry(n: u64, tries: &mut u32) -> bool {
    debug_assert!(x86_64::instructions::interrupts::are_enabled(), "waiting for reclaim with a spinlock held");
    let mut force = false;
    loop {
        // Dirty or pinned cache pages become droppable once written back
        // or ungranted: while there are some, more tries are allowed
        // (commit counted them as taken, so what was promised is there
        // once they are), but not for ever (a stuck disk), and not for a
        // pager's thread, which may be the one to write them.
        let waiting_for_io = crate::fs::cache::unavailable_pages() > 0 && !crate::process::linux::is_pager();
        let most = if waiting_for_io { RECLAIM_RETRIES_IO } else { RECLAIM_RETRIES };
        if crate::process::kill::dying() {
            return false;
        }
        if *tries >= most {
            // Reclaim gave up: a process's end must make room (Linux's
            // `__alloc_pages_may_oom`), after which reclaim goes on.
            if !oom::out_of_memory(n) {
                return false;
            }
            *tries = 0;
        }
        if with_frames(|f| f.user_may_take(n)) || reclaim_for(n, force) > 0 {
            return true;
        }
        force = true;
        *tries += 1;
        let deadline = crate::time::now() + RECLAIM_THROTTLE;
        crate::process::sched::prepare_to_wait(reclaim_chan()).sleep_until(deadline);
    }
}

/// Where throttled allocations wait (only for time to pass: nothing
/// wakes it).
fn reclaim_chan() -> usize {
    &RECLAIM as *const _ as usize
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
    // Free slab slots are free memory, though the heap counts their slabs.
    let slack: usize = (0..slab::CLASSES).map(|c| SLABS[c].lock().len() * slab::class_size(c)).sum();
    let heap = HEAP.0.lock();
    Stats {
        total_frames,
        used_frames,
        heap_used: heap.used() - slack,
        heap_free: heap.free() + slack,
    }
}
