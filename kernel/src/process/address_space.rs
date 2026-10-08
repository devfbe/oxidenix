//! A process's virtual memory: its page tables and the areas (VMAs) that
//! say what each address range is.
//!
//! Memory is demand-paged: `mmap`, `brk` and the stack only create areas;
//! a page gets a frame on its first access (`fault`), which also performs
//! copy-on-write and grows stacks downwards. Faults of the kernel's own
//! accesses to user memory (`uaccess`) are handled the same way.
//!
//! Writable private memory is committed when it is mapped, against a
//! system-wide limit (`memory::commit`), so running out of memory is an
//! ENOMEM from `mmap`, `brk`, `mprotect` or `fork`, not a killed process.
//! `PROT_NONE` and `MAP_NORESERVE` areas commit nothing until made writable.
//!
//! Page table entries carry two software bits: COW (a shared frame that is
//! copied on the first write) and PROT_NONE (a frame kept while its area
//! denies all access).
//!
//! File mappings map the frames of the file's page cache (`fs::cache`):
//! shared ones the cache frame itself, private ones the cache frame
//! copy-on-write until the first write copies it. Anonymous shared memory
//! is an unnamed tmpfs file. Each cache knows the address spaces that map
//! it (`owner`), so truncating a file removes its pages everywhere.
//!
//! The threads of a process share one address space (`Mm`), behind a lock
//! that may be held while a fault sleeps. Every change that removes,
//! write-protects or moves a mapping is followed by a TLB shootdown
//! (`tlb`), and frames are freed only after it.

use super::tlb::{self, Tlb};
use crate::fs::cache::PageCache;
use crate::memory;
use crate::memory::frame::UserFrames;
use crate::sync::{Mutex, MutexGuard};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

/// End of the memory a program may map: 64 TiB, PML4 slots 0-127. Slots
/// 128-255 are the Linux server's shared region (docs/design/linux-server.md,
/// ADR 0003).
pub const USER_END: u64 = 0x0000_4000_0000_0000;
pub const PAGE: u64 = 4096;
/// Marks a read-only mapping of a shared frame that becomes private on the
/// first write (an OS-available page table bit).
pub const COW: PageTableFlags = PageTableFlags::BIT_9;
/// A frame kept for an area that currently denies access (PROT_NONE); the
/// entry is not present.
const PROT_NONE: PageTableFlags = PageTableFlags::BIT_10;

/// Highest address `mmap` hands out; the stack lives above.
pub const MMAP_TOP: u64 = 0x0000_3000_0000_0000;
/// How far a stack may grow below its top (RLIMIT_STACK).
pub const STACK_LIMIT: u64 = 8 * 1024 * 1024;

fn page_down(x: u64) -> u64 {
    x & !(PAGE - 1)
}

/// Pages covering [start, end) in user space. Unlike `Page::range_inclusive`
/// this never steps past the last page, which for the top user page would
/// compute the non-canonical address `USER_END` and panic.
fn user_pages(start: u64, end: u64) -> impl Iterator<Item = Page<Size4KiB>> {
    (page_down(start)..end).step_by(PAGE as usize).map(|a| Page::containing_address(VirtAddr::new(a)))
}

/// Access rights of an area, as mmap's PROT_ bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Prot {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Prot {
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const EXEC: u64 = 4;

    pub fn from_bits(bits: u64) -> Prot {
        Prot { read: bits & Self::READ != 0, write: bits & Self::WRITE != 0, exec: bits & Self::EXEC != 0 }
    }

    pub const RW: Prot = Prot { read: true, write: true, exec: false };

    pub fn none(self) -> bool {
        !self.read && !self.write && !self.exec
    }

    /// Leaf flags for a page of this area (without COW handling).
    fn flags(self) -> PageTableFlags {
        if self.none() {
            return PROT_NONE;
        }
        let mut f = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if self.write {
            f |= PageTableFlags::WRITABLE;
        }
        if !self.exec {
            f |= PageTableFlags::NO_EXECUTE;
        }
        f
    }
}

/// Something kept alive while a mapping or a running program uses a file,
/// and let go when that ends: a kernel file and its write access, the
/// right to run it unwritten, or a record of the Linux server's.
pub type Hold = Arc<dyn Send + Sync>;

/// What backs an area's pages.
#[derive(Clone)]
pub enum Backing {
    /// Zero-filled private memory.
    Anon,
    /// Pages of a file's page cache; `offset` is the file offset of
    /// `start`. Shared: stores reach the file; private: copy-on-write.
    /// `_hold` is kept while it is mapped: the file and the write access of
    /// the descriptor it was mapped through (`MappedFile`), or the Linux
    /// server's notice for its own files (None for anonymous shared
    /// memory); `may_write`: the file was opened for writing, so a shared
    /// mapping may become writable.
    File { cache: Arc<PageCache>, offset: u64, shared: bool, may_write: bool, _hold: Option<Hold> },
    /// Device memory, mapped up front (DMA areas).
    Device,
    /// Pages granted to a channel's service (`channel`), mapped up front
    /// and owned by `grant`, which the kernel finds them by when it takes
    /// them back. Read-only unless `writable`, never executable, never
    /// moved (mremap) and not inherited by fork.
    Granted { grant: Hold, writable: bool },
    /// What a revoked grant leaves in the service (`unmap_grant`): an
    /// inaccessible reservation of its range, so that the service's copies
    /// to the address it knew fault (and fail, `channel::set_copy_fixup`)
    /// instead of reaching whatever else the range could be given to. No
    /// access can be added (mprotect), nothing is mapped over it until the
    /// service unmaps it (munmap), fork does not inherit it.
    Revoked { channel: u64 },
}

impl Backing {
    /// Anonymous shared memory of `pages` pages.
    pub fn shared_anon(pages: u64) -> Result<Backing, Fault> {
        Ok(Backing::File { cache: PageCache::anonymous(pages)?, offset: 0, shared: true, may_write: true, _hold: None })
    }

    /// Shared memory (or device memory): never copied on write or fork.
    fn shared(&self) -> bool {
        matches!(self, Backing::File { shared: true, .. } | Backing::Device | Backing::Granted { .. } | Backing::Revoked { .. })
    }

    /// Whether this is the grant `grant`'s mapping.
    fn is_grant(&self, grant: &Hold) -> bool {
        matches!(self, Backing::Granted { grant: g, .. } if core::ptr::addr_eq(Arc::as_ptr(g), Arc::as_ptr(grant)))
    }

    /// A shared mapping whose stores must mark the page dirty first (a
    /// file on a disk): its pages are mapped writable only after that.
    fn tracks_dirty(&self) -> bool {
        matches!(self, Backing::File { shared: true, cache, .. } if cache.tracks_dirty())
    }
}

#[derive(Clone)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: Prot,
    pub backing: Backing,
    /// Its length is committed (writable private memory).
    pub charged: bool,
    /// A stack: accesses just below it grow it, up to STACK_LIMIT.
    pub grows_down: bool,
}

impl Vma {
    fn pages(&self) -> u64 {
        (self.end - self.start) / PAGE
    }

    /// The part [from, to) of this area (file offsets and object indexes
    /// follow).
    fn slice(&self, from: u64, to: u64) -> Vma {
        let mut v = self.clone();
        let shift = from - self.start;
        v.start = from;
        v.end = to;
        if let Backing::File { offset, .. } = &mut v.backing {
            *offset += shift;
        }
        v
    }

    /// Whether this area would need commit when writable.
    fn private(&self) -> bool {
        matches!(self.backing, Backing::Anon | Backing::File { shared: false, .. })
    }

    /// The cache of a file area and the file page of `start`.
    fn file(&self) -> Option<(&Arc<PageCache>, u64)> {
        match &self.backing {
            Backing::File { cache, offset, .. } => Some((cache, offset / PAGE)),
            _ => None,
        }
    }
}

/// Mapped (resident) and virtual pages, and the most pages ever resident,
/// readable by anyone (procfs) while only the owning process changes them.
#[derive(Default)]
pub struct MemStats {
    pub pages: AtomicU64,
    pub virt_pages: AtomicU64,
    pub peak_pages: AtomicU64,
}

impl MemStats {
    fn add_resident(&self, pages: u64) {
        let now = self.pages.fetch_add(pages, Ordering::Relaxed) + pages;
        self.peak_pages.fetch_max(now, Ordering::Relaxed);
    }
}

fn count(counter: &AtomicU64, delta: i64) {
    if delta >= 0 {
        counter.fetch_add(delta as u64, Ordering::Relaxed);
    } else {
        counter.fetch_sub(delta.unsigned_abs(), Ordering::Relaxed);
    }
}

/// Why an access could not be satisfied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// No area, or the area forbids this access: SIGSEGV.
    Segv,
    /// A file page beyond the end of the file: SIGBUS.
    Bus,
    /// No frame left.
    Oom,
    /// The file does not grant the rights asked for (mprotect: EACCES).
    Access,
    /// The page must come from a pager first (`AddressSpace::awaited`):
    /// the fault is tried again once it is there. Only `handle_fault` and
    /// `populate` ask for this.
    Retry,
}

#[derive(Clone, Copy)]
pub struct Access {
    pub write: bool,
    pub exec: bool,
}

/// Own level-4 table: the lower half belongs to the process, the upper
/// (kernel) half is shared with the kernel address space.
pub struct AddressSpace {
    l4: PhysFrame,
    /// A Linux process's normal view (`linux`): a second top-level table
    /// with this one's program slots (0-127, kept equal) and the Linux
    /// server instance's shared region (slot 128).
    normal: Option<PhysFrame>,
    instance: Option<Arc<super::linux::Instance>>,
    /// A Linux program's space (counted by its instance).
    program: bool,
    pub tlb: Arc<Tlb>,
    pub stats: Arc<MemStats>,
    /// Areas by start address; they never overlap.
    vmas: BTreeMap<u64, Vma>,
    /// The heap (brk): from the end of the program to the current break.
    pub brk_start: u64,
    pub brk_end: u64,
    /// The `Mm` holding this space, once there is one: file caches record
    /// it to reach the space's mappings of their pages.
    owner: Weak<Mm>,
    /// The program file it runs, kept from being written meanwhile.
    pub exe: Option<Hold>,
    /// The page a fault found missing (`Fault::Retry`): the faulting thread
    /// waits for it with the space unlocked.
    awaited: Option<(Arc<PageCache>, u64)>,
}

/// An address space as tasks hold it: shared by the threads of a process
/// (and by a vfork child until it execs or exits).
pub struct Mm {
    /// What a context switch needs, readable without the lock.
    pub tlb: Arc<Tlb>,
    pub stats: Arc<MemStats>,
    space: Mutex<AddressSpace>,
}

impl Mm {
    /// Fails (None) without memory, dropping the space.
    pub fn new(space: AddressSpace) -> Option<Arc<Mm>> {
        let (tlb, stats) = (space.tlb.clone(), space.stats.clone());
        let mm = Arc::try_new(Mm { tlb, stats, space: Mutex::new(space) }).ok()?;
        let mut space = mm.lock();
        space.owner = Arc::downgrade(&mm);
        let files: alloc::vec::Vec<Arc<PageCache>> = space.vmas.values().filter_map(|v| v.file().map(|(c, _)| c.clone())).collect();
        let registered = files.iter().all(|c| c.register(&space.owner).is_ok());
        drop(space);
        registered.then_some(mm)
    }

    /// The address space, for changes and faults. Sleeps while another
    /// thread holds it; never taken in interrupt context or with a spinlock.
    pub fn lock(&self) -> MutexGuard<'_, AddressSpace> {
        self.space.lock()
    }
}

/// Frames taken out of the page tables, released once no TLB can still
/// reach them (after a shootdown of the range they came from).
struct Gather<'a> {
    tlb: &'a Tlb,
    frames: heapless::Vec<PhysFrame, 128>,
    start: u64,
    end: u64,
}

impl<'a> Gather<'a> {
    fn new(tlb: &'a Tlb) -> Self {
        Gather { tlb, frames: heapless::Vec::new(), start: u64::MAX, end: 0 }
    }

    fn add(&mut self, va: u64, frame: PhysFrame) {
        if self.frames.is_full() {
            self.finish();
        }
        let _ = self.frames.push(frame);
        self.start = self.start.min(va);
        self.end = self.end.max(va + PAGE);
    }

    fn finish(&mut self) {
        if self.frames.is_empty() {
            return;
        }
        tlb::shootdown(self.tlb, self.start, self.end);
        let frames = core::mem::take(&mut self.frames);
        memory::with_frames(|f| {
            for frame in frames {
                unsafe { f.deallocate_frame(frame) };
            }
        });
        (self.start, self.end) = (u64::MAX, 0);
    }
}

impl Drop for Gather<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl AddressSpace {
    pub fn new() -> Option<Self> {
        let l4 = memory::with_frames(|f| UserFrames(f).allocate_frame())?;
        let table = table_at(l4);
        let kernel = table_at(memory::kernel_l4());
        for i in 0..256 {
            table[i].set_unused();
        }
        for i in 256..512 {
            table[i] = kernel[i].clone();
        }
        let (Ok(stats), Ok(tlb)) = (Arc::try_new(MemStats::default()), Arc::try_new(Tlb::new(l4))) else {
            memory::with_frames(|f| unsafe { f.deallocate_frame(l4) });
            return None;
        };
        Some(AddressSpace {
            l4,
            normal: None,
            instance: None,
            program: false,
            tlb,
            stats,
            vmas: BTreeMap::new(),
            brk_start: 0,
            brk_end: 0,
            owner: Weak::new(),
            exe: None,
            awaited: None,
        })
    }

    /// Makes this the address space of a Linux process served by
    /// `instance`: adds the normal view. Before anyone runs in it. A
    /// `program`'s space counts for the instance (its pager's has none).
    pub fn attach(&mut self, instance: Arc<super::linux::Instance>, program: bool) -> Result<(), Fault> {
        // Known to the instance before anyone can load the view, so its
        // shootdowns of the region reach this space.
        instance.add_space(&self.tlb)?;
        let normal = memory::with_frames(|f| UserFrames(f).allocate_frame()).ok_or(Fault::Oom)?;
        let (table, mine) = (table_at(normal), table_at(self.l4));
        for i in 0..512 {
            table[i] = mine[i].clone();
        }
        table[super::linux::SHARED_SLOT].set_frame(instance.pdpt(), super::linux::table_flags());
        self.normal = Some(normal);
        self.tlb.set_normal(normal);
        if program {
            instance.program_added();
        }
        self.program = program;
        self.instance = Some(instance);
        Ok(())
    }

    /// The Linux server instance serving this address space, if any.
    pub fn instance(&self) -> Option<&Arc<super::linux::Instance>> {
        self.instance.as_ref()
    }

    /// Copies the program's top-level entries into the normal view; a new
    /// one appears when a mapping needs a new third-level table. Entries
    /// never go away before the address space does.
    fn sync_views(&self, range: core::ops::Range<u64>) {
        let Some(normal) = self.normal else { return };
        let (table, mine) = (table_at(normal), table_at(self.l4));
        let first = (range.start >> 39) as usize & 511;
        let last = (range.end.saturating_sub(1) >> 39) as usize & 511;
        for i in first..=last.min(super::linux::SHARED_SLOT - 1) {
            if table[i].is_unused() && !mine[i].is_unused() {
                table[i] = mine[i].clone();
            }
        }
    }

    fn mapper(&self) -> OffsetPageTable<'static> {
        unsafe { OffsetPageTable::new(table_at(self.l4), memory::phys_offset()) }
    }

    fn active(&self) -> bool {
        Cr3::read().0 == self.l4
    }

    // ------------------------------------------------------------ areas

    /// The area containing `addr`.
    pub fn vma(&self, addr: u64) -> Option<&Vma> {
        self.vmas.range(..=addr).next_back().map(|(_, v)| v).filter(|v| addr < v.end)
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.vmas.range(..end).next_back().is_some_and(|(_, v)| v.end > start)
    }

    fn insert(&mut self, vma: Vma) {
        count(&self.stats.virt_pages, vma.pages() as i64);
        self.vmas.insert(vma.start, vma);
    }

    /// Splits areas so that `at` is a boundary.
    fn split_at(&mut self, at: u64) {
        let Some(v) = self.vma(at).filter(|v| v.start < at).cloned() else { return };
        self.vmas.insert(v.start, v.slice(v.start, at));
        self.vmas.insert(at, v.slice(at, v.end));
    }

    /// Joins `start`'s area with its neighbors where they continue it
    /// (same rights, backing and commit), so brk growth stays one area.
    fn merge_around(&mut self, start: u64) {
        let mergeable = |a: &Vma, b: &Vma| {
            a.end == b.start
                && a.prot == b.prot
                && a.charged == b.charged
                && a.grows_down == b.grows_down
                && matches!((&a.backing, &b.backing), (Backing::Anon, Backing::Anon))
        };
        let Some(cur) = self.vmas.get(&start).cloned() else { return };
        let mut merged = cur.clone();
        if let Some((_, prev)) = self.vmas.range(..start).next_back() {
            if mergeable(prev, &cur) {
                merged.start = prev.start;
                let p = prev.start;
                self.vmas.remove(&p);
            }
        }
        if let Some(next) = self.vmas.get(&cur.end).cloned() {
            if mergeable(&cur, &next) {
                merged.end = next.end;
                self.vmas.remove(&next.start);
            }
        }
        self.vmas.remove(&start);
        self.vmas.insert(merged.start, merged);
    }

    /// A free range of `len` bytes, as high as possible below MMAP_TOP and
    /// above `floor` (the heap).
    pub fn find_free(&self, len: u64, floor: u64) -> Option<u64> {
        let mut top = MMAP_TOP;
        for (_, v) in self.vmas.range(..MMAP_TOP).rev() {
            if v.end <= top && top - v.end >= len {
                break;
            }
            top = top.min(v.start);
        }
        let start = top.checked_sub(len)?;
        (start >= floor && !self.overlaps(start, top)).then_some(start)
    }

    /// Creates an area. Writable private memory is committed unless
    /// `noreserve`; on failure nothing changes (ENOMEM).
    pub fn map(&mut self, start: u64, len: u64, prot: Prot, backing: Backing, noreserve: bool) -> Result<(), Fault> {
        let end = start.checked_add(len).filter(|&e| e <= USER_END && len > 0).ok_or(Fault::Segv)?;
        let mut vma = Vma { start, end, prot, backing, charged: false, grows_down: false };
        if let Some((cache, _)) = vma.file() {
            if self.owner.strong_count() > 0 {
                cache.register(&self.owner)?;
            }
        }
        if prot.write && vma.private() && !noreserve {
            if !memory::commit(vma.pages()) {
                return Err(Fault::Oom);
            }
            vma.charged = true;
        }
        self.unmap(start, len);
        self.insert(vma);
        self.merge_around(start);
        Ok(())
    }

    /// The process stack: an area below `top` that grows down on demand.
    pub fn map_stack(&mut self, top: u64, initial: u64) -> Result<(), Fault> {
        let start = top - initial;
        self.map(start, initial, Prot::RW, Backing::Anon, false)?;
        if let Some(v) = self.vmas.get_mut(&start) {
            v.grows_down = true;
        }
        Ok(())
    }

    /// Removes [start, start+len): areas are cut, pages freed and commit
    /// released.
    pub fn unmap(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len).filter(|&e| e <= USER_END && len > 0) else { return };
        self.split_at(start);
        self.split_at(end);
        let gone: alloc::vec::Vec<u64> = self.vmas.range(start..end).map(|(&s, _)| s).collect();
        for s in gone {
            let v = self.vmas.remove(&s).expect("listed above");
            count(&self.stats.virt_pages, -(v.pages() as i64));
            if v.charged {
                memory::uncommit(v.pages());
            }
        }
        self.clear_pages(start, end);
    }

    /// Frees the frames mapped in [start, end) (the areas stay).
    fn clear_pages(&mut self, start: u64, end: u64) {
        let mut gather = Gather::new(&self.tlb);
        let mut freed = 0i64;
        for_each_leaf(self.l4, start, end, |va, e| {
            let frame = PhysFrame::containing_address(e.addr());
            e.set_unused();
            gather.add(va, frame);
            freed += 1;
        });
        gather.finish();
        count(&self.stats.pages, -freed);
    }

    /// mprotect: new rights for [start, start+len), which must be fully
    /// covered by areas (else ENOMEM, as on Linux). Making private memory
    /// writable commits it.
    pub fn protect(&mut self, start: u64, len: u64, prot: Prot) -> Result<(), Fault> {
        let end = start.checked_add(len).filter(|&e| e <= USER_END).ok_or(Fault::Oom)?;
        // Coverage and rights checks first: no change on failure.
        let mut at = start;
        while at < end {
            let v = self.vma(at).ok_or(Fault::Oom)?;
            if prot.write && matches!(v.backing, Backing::File { shared: true, may_write: false, .. }) {
                return Err(Fault::Access);
            }
            if let Backing::Granted { writable, .. } = v.backing {
                if (prot.write && !writable) || prot.exec {
                    return Err(Fault::Access);
                }
            }
            if matches!(v.backing, Backing::Revoked { .. }) && (prot.read || prot.write || prot.exec) {
                return Err(Fault::Access);
            }
            at = v.end;
        }
        let needed: u64 = self
            .vmas
            .range(..end)
            .filter(|(_, v)| v.end > start && v.private() && !v.charged && prot.write)
            .map(|(_, v)| (v.end.min(end) - v.start.max(start)) / PAGE)
            .sum();
        if needed > 0 && !memory::commit(needed) {
            return Err(Fault::Oom);
        }
        self.split_at(start);
        self.split_at(end);
        let keys: alloc::vec::Vec<u64> = self.vmas.range(start..end).map(|(&s, _)| s).collect();
        for s in keys {
            let v = self.vmas.get_mut(&s).expect("listed above");
            v.prot = prot;
            if prot.write && v.private() && !v.charged {
                v.charged = true;
            }
        }
        self.apply_prot(start, end, prot);
        Ok(())
    }

    /// Rewrites the entries of present (or PROT_NONE-kept) pages for `prot`.
    fn apply_prot(&mut self, start: u64, end: u64, prot: Prot) {
        let l4 = self.l4;
        memory::with_frames(|frames| {
            for_each_leaf(l4, start, end, |va, e| {
                let frame = PhysFrame::containing_address(e.addr());
                let was_cow = e.flags().contains(COW);
                let mut flags = prot.flags();
                // A frame shared with another mapping never becomes writable
                // in place (unless the area is shared memory).
                let shared_area = self.vma(va).is_some_and(|v| v.backing.shared());
                // A page of a disk file stays read-only until a store marks
                // it dirty (unless it is already).
                let tracked = self.vma(va).is_some_and(|v| v.backing.tracks_dirty());
                let was_writable = e.flags().contains(PageTableFlags::WRITABLE);
                let must_fault = if shared_area { tracked && !was_writable } else { was_cow || frames.refcount(frame) > 1 };
                if flags.contains(PageTableFlags::WRITABLE) && must_fault {
                    flags.remove(PageTableFlags::WRITABLE);
                    flags.insert(COW);
                } else if was_cow && !prot.none() {
                    flags.insert(COW);
                }
                e.set_addr(frame.start_address(), flags);
            });
        });
        // Rights may have shrunk: no CPU may keep the old ones.
        tlb::shootdown(&self.tlb, start, end);
    }

    /// madvise(MADV_DONTNEED): private pages are dropped and read as zero
    /// (or from the file) again; shared memory keeps its contents.
    pub fn discard(&mut self, start: u64, len: u64) -> Result<(), Fault> {
        let end = start.checked_add(len).filter(|&e| e <= USER_END).ok_or(Fault::Segv)?;
        let ranges: alloc::vec::Vec<(u64, u64)> = self
            .vmas
            .range(..end)
            .filter(|(_, v)| v.end > start && v.private())
            .map(|(_, v)| (v.start.max(start), v.end.min(end)))
            .collect();
        for (s, e) in ranges {
            self.clear_pages(s, e);
        }
        Ok(())
    }

    /// mremap: resizes the area at [old, old+old_len) to `new_len`, in
    /// place if possible, else (with `may_move`) at a new address, or at
    /// `fixed`. Pages move with their contents. Returns the new address.
    pub fn remap(&mut self, old: u64, old_len: u64, new_len: u64, may_move: bool, fixed: Option<u64>, floor: u64) -> Result<u64, Fault> {
        let v = self.vma(old).cloned().ok_or(Fault::Segv)?;
        let old_end = old.checked_add(old_len).ok_or(Fault::Segv)?;
        if old_end > v.end || matches!(v.backing, Backing::Device | Backing::Granted { .. } | Backing::Revoked { .. }) {
            return Err(Fault::Segv);
        }
        if new_len <= old_len && fixed.is_none() {
            self.unmap(old + new_len, old_len - new_len);
            return Ok(old);
        }
        let grow = new_len - old_len.min(new_len);
        // In place, if the area ends here and the space behind it is free.
        if fixed.is_none() && old_end == v.end && !self.overlaps(old_end, old_end + grow) && old_end + grow <= MMAP_TOP {
            if v.charged && !memory::commit(grow / PAGE) {
                return Err(Fault::Oom);
            }
            let mut ext = v.slice(old_end - PAGE, old_end);
            ext.start = old_end;
            ext.end = old_end + grow;
            if let Backing::File { offset, .. } = &mut ext.backing {
                *offset += PAGE;
            }
            self.insert(ext);
            self.merge_around(old_end);
            return Ok(old);
        }
        if !may_move && fixed.is_none() {
            return Err(Fault::Oom);
        }
        let target = match fixed {
            Some(t) => t,
            None => self.find_free(new_len, floor).ok_or(Fault::Oom)?,
        };
        if v.charged && grow > 0 && !memory::commit(grow / PAGE) {
            return Err(Fault::Oom);
        }
        if fixed.is_some() {
            self.unmap(target, new_len);
        }
        // The new area: the old part, then (if growing) its continuation.
        let mut moved = v.slice(old, old_end);
        moved.start = target;
        moved.end = target + new_len;
        self.split_at(old);
        self.split_at(old_end);
        let old_vma = self.vmas.remove(&old).expect("split above");
        count(&self.stats.virt_pages, -(old_vma.pages() as i64));
        let keep = old_len.min(new_len);
        self.move_pages(old, target, keep);
        if new_len < old_len {
            self.clear_pages(old + new_len, old_end);
            if old_vma.charged {
                memory::uncommit((old_len - new_len) / PAGE);
            }
        }
        self.insert(moved);
        Ok(target)
    }

    /// Moves the page table entries of [from, from+len) to `to`.
    fn move_pages(&mut self, from: u64, to: u64, len: u64) {
        let l4 = self.l4;
        let mut mapper = self.mapper();
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        memory::with_frames(|frames| {
            for_each_leaf(l4, from, from + len, |va, e| {
                let (frame, flags) = (PhysFrame::containing_address(e.addr()), e.flags());
                e.set_unused();
                let page = Page::<Size4KiB>::containing_address(VirtAddr::new(to + (va - from)));
                let mut user = UserFrames(frames);
                // The target range was unmapped, so this cannot collide.
                let _ = unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, &mut user) }.map(|f| f.ignore());
            });
        });
        self.sync_views(to..to + len);
        tlb::shootdown(&self.tlb, from, from + len);
    }

    // ----------------------------------------------------------- faults

    /// Satisfies an access to `va`: maps a frame on first use, copies a
    /// copy-on-write page on a write, grows a stack. Works on any address
    /// space (the loader fills spaces that are not active yet).
    pub fn fault(&mut self, va: u64, access: Access) -> Result<(), Fault> {
        self.fault_or_retry(va, access, true)
    }

    /// `fault`; without `wait`, a page that must come from a pager first is
    /// asked for and `Fault::Retry` returned (the page in `awaited`), so the
    /// caller can wait with the space unlocked: a pager may need to lock
    /// it meanwhile (write-back write-protects every mapping of a file).
    fn fault_or_retry(&mut self, va: u64, access: Access, wait: bool) -> Result<(), Fault> {
        if va >= USER_END {
            return Err(Fault::Segv);
        }
        let page = page_down(va);
        if self.vma(page).is_none() {
            self.grow_stack(page)?;
        }
        let v = self.vma(page).cloned().ok_or(Fault::Segv)?;
        if v.prot.none() || (access.write && !v.prot.write) || (access.exec && !v.prot.exec) {
            return Err(Fault::Segv);
        }
        if let Some(e) = leaf_entry(self.l4, page) {
            let flags = e.flags();
            if flags.contains(PageTableFlags::PRESENT) {
                if access.write && !flags.contains(PageTableFlags::WRITABLE) {
                    return self.break_cow(page, &v);
                }
                // Already satisfied (another path faulted it in).
                return Ok(());
            }
            // A kept PROT_NONE page whose area allows access again (an
            // entry that was not present is in no TLB).
            let frame: PhysFrame = PhysFrame::containing_address(e.addr());
            e.set_addr(frame.start_address(), v.prot.flags());
            return Ok(());
        }
        let (frame, writable_ok) = self.new_frame(page, &v, access, wait)?;
        let mut flags = v.prot.flags();
        if !writable_ok && flags.contains(PageTableFlags::WRITABLE) {
            flags.remove(PageTableFlags::WRITABLE);
            flags.insert(COW);
        }
        self.install(page, frame, flags)
    }

    /// A frame with the page's initial contents (holding a reference for
    /// this mapping), and whether it may be mapped writable directly.
    fn new_frame(&mut self, page: u64, v: &Vma, access: Access, wait: bool) -> Result<(PhysFrame, bool), Fault> {
        match &v.backing {
            Backing::Anon => Ok((zeroed_frame()?, true)),
            Backing::Device | Backing::Granted { .. } | Backing::Revoked { .. } => Err(Fault::Segv),
            Backing::File { cache, offset, shared, .. } => {
                // Reading a missing page may sleep (a remote file): only the
                // address space is locked. A pager's page is waited for
                // without the lock (`fault_or_retry`) unless `wait`.
                let index = (offset + (page - v.start)) / PAGE;
                let Some(frame) = cache.try_map_page(index, wait)? else {
                    self.awaited = Some((cache.clone(), index));
                    return Err(Fault::Retry);
                };
                if *shared && cache.tracks_dirty() {
                    // Writable only once the store marked it dirty.
                    if access.write && !cache.set_dirty(index) {
                        memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
                        return Err(Fault::Bus);
                    }
                    return Ok((frame, access.write));
                }
                if *shared || !access.write {
                    // A private page stays the cache's until written.
                    return Ok((frame, *shared));
                }
                let copy = memory::with_frames(|f| UserFrames(f).allocate_frame());
                memory::with_frames(|f| {
                    if let Some(copy) = copy {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                memory::phys_to_virt(frame.start_address().as_u64()),
                                memory::phys_to_virt(copy.start_address().as_u64()),
                                PAGE as usize,
                            );
                        }
                    }
                    unsafe { f.deallocate_frame(frame) };
                });
                Ok((copy.ok_or(Fault::Oom)?, true))
            }
        }
    }

    fn install(&mut self, page: u64, frame: PhysFrame, flags: PageTableFlags) -> Result<(), Fault> {
        let mut mapper = self.mapper();
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let p = Page::<Size4KiB>::containing_address(VirtAddr::new(page));
        let mapped = memory::with_frames(|frames| {
            let mut user = UserFrames(frames);
            match unsafe { mapper.map_to_with_table_flags(p, frame, flags, parent, &mut user) } {
                Ok(f) => {
                    f.ignore();
                    true
                }
                Err(_) => {
                    unsafe { user.0.deallocate_frame(frame) };
                    false
                }
            }
        });
        if !mapped {
            return Err(Fault::Oom);
        }
        self.sync_views(page..page + PAGE);
        // The entry was not present before, so no TLB holds it.
        self.stats.add_resident(1);
        Ok(())
    }

    /// A write to a copy-on-write page: copy it unless this mapping is
    /// the frame's last user.
    fn break_cow(&mut self, page: u64, v: &Vma) -> Result<(), Fault> {
        let e = leaf_entry(self.l4, page).ok_or(Fault::Segv)?;
        let flags = e.flags();
        let writable = ((flags - COW) | PageTableFlags::WRITABLE) & !PROT_NONE;
        let old = PhysFrame::containing_address(e.addr());
        let shared_area = v.backing.shared();
        if let (true, Backing::File { cache, offset, .. }) = (v.backing.tracks_dirty(), &v.backing) {
            // Gone if the file was truncated meanwhile.
            if !cache.set_dirty((offset + (page - v.start)) / PAGE) {
                return Err(Fault::Bus);
            }
        }
        let copied = memory::with_frames(|frames| {
            if shared_area || frames.refcount(old) <= 1 {
                // Only rights grow: a stale read-only entry elsewhere just
                // faults once more and finds the page writable.
                e.set_flags(writable);
                return Ok(false);
            }
            let new = UserFrames(frames).allocate_frame().ok_or(Fault::Oom)?;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    memory::phys_to_virt(old.start_address().as_u64()),
                    memory::phys_to_virt(new.start_address().as_u64()),
                    PAGE as usize,
                );
            }
            e.set_addr(new.start_address(), writable);
            Ok(true)
        })?;
        if copied {
            // Other threads must stop reading the old frame before this
            // mapping lets go of it.
            let mut gather = Gather::new(&self.tlb);
            gather.add(page, old);
        }
        Ok(())
    }

    /// Gives a page of a read-only private area its own frame if it shares
    /// one (with a fork relative or the file's page cache), keeping its
    /// rights, so a kernel write cannot reach the others.
    fn privatize(&mut self, page: u64) -> Result<(), Fault> {
        let e = leaf_entry(self.l4, page).ok_or(Fault::Segv)?;
        let old = PhysFrame::containing_address(e.addr());
        let copied = memory::with_frames(|frames| {
            if frames.refcount(old) <= 1 {
                return Ok(false);
            }
            let new = UserFrames(frames).allocate_frame().ok_or(Fault::Oom)?;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    memory::phys_to_virt(old.start_address().as_u64()),
                    memory::phys_to_virt(new.start_address().as_u64()),
                    PAGE as usize,
                );
            }
            e.set_addr(new.start_address(), e.flags() - COW);
            Ok(true)
        })?;
        if copied {
            let mut gather = Gather::new(&self.tlb);
            gather.add(page, old);
        }
        Ok(())
    }

    /// An access just below a stack extends it (charging the commit).
    fn grow_stack(&mut self, page: u64) -> Result<(), Fault> {
        let (&start, stack) = self.vmas.range(page..).next().ok_or(Fault::Segv)?;
        if !stack.grows_down || stack.end - page > STACK_LIMIT || self.overlaps(page, start) {
            return Err(Fault::Segv);
        }
        let grow = (start - page) / PAGE;
        if !memory::commit(grow) {
            return Err(Fault::Oom);
        }
        let mut v = self.vmas.remove(&start).expect("found above");
        v.start = page;
        count(&self.stats.virt_pages, grow as i64);
        self.vmas.insert(page, v);
        Ok(())
    }

    /// Faults in every page of [start, start+len) (MAP_POPULATE, loader).
    /// A page that must come from a pager is asked for, not waited for (the
    /// space is locked, and the pager may need it): best effort, as
    /// MAP_POPULATE is on Linux.
    pub fn populate(&mut self, start: u64, len: u64, write: bool) -> Result<(), Fault> {
        for page in user_pages(start, start.saturating_add(len)) {
            match self.fault_or_retry(page.start_address().as_u64(), Access { write, exec: false }, false) {
                Ok(()) => {}
                Err(Fault::Retry) => self.awaited = None,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Maps `pages` existing physical frames starting at `phys` (device DMA
    /// memory) to `start`. Each mapping holds a reference on its frame.
    pub fn map_phys(&mut self, start: u64, phys: u64, pages: u64, prot: Prot) -> Result<(), Fault> {
        let len = pages * PAGE;
        if start.checked_add(len).is_none_or(|e| e > USER_END) {
            return Err(Fault::Segv);
        }
        self.unmap(start, len);
        self.insert(Vma { start, end: start + len, prot, backing: Backing::Device, charged: false, grows_down: false });
        for i in 0..pages {
            let frame = PhysFrame::containing_address(PhysAddr::new(phys + i * PAGE));
            memory::with_frames(|f| f.share(frame));
            self.install(start + i * PAGE, frame, prot.flags())?;
        }
        Ok(())
    }

    /// Maps `frames` (pinned by `grant`) at `start` (see
    /// `Backing::Granted`); each entry holds a reference on its frame.
    /// Nothing is mapped on failure.
    pub fn map_granted(&mut self, start: u64, frames: &[PhysFrame], writable: bool, grant: Hold) -> Result<(), Fault> {
        let len = frames.len() as u64 * PAGE;
        if len == 0 || start % PAGE != 0 || start.checked_add(len).is_none_or(|e| e > USER_END) || self.overlaps(start, start + len) {
            return Err(Fault::Segv);
        }
        let prot = Prot { read: true, write: writable, exec: false };
        self.insert(Vma { start, end: start + len, prot, backing: Backing::Granted { grant, writable }, charged: false, grows_down: false });
        for (i, &frame) in frames.iter().enumerate() {
            memory::with_frames(|f| f.share(frame));
            if let Err(e) = self.install(start + i as u64 * PAGE, frame, prot.flags()) {
                self.unmap(start, len);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Removes every mapping of `grant` (`map_granted`) of channel
    /// `channel`; each range stays reserved and inaccessible
    /// (`Backing::Revoked`) until the service unmaps it or its end of the
    /// channel goes (`unmap_revoked`).
    pub fn unmap_grant(&mut self, grant: &Hold, channel: u64) {
        let ranges: alloc::vec::Vec<(u64, u64)> =
            self.vmas.values().filter(|v| v.backing.is_grant(grant)).map(|v| (v.start, v.end - v.start)).collect();
        for (start, len) in ranges {
            self.unmap(start, len);
            let none = Prot { read: false, write: false, exec: false };
            self.insert(Vma { start, end: start + len, prot: none, backing: Backing::Revoked { channel }, charged: false, grows_down: false });
        }
    }

    /// Frees what the revoked grants of `channel` left (`unmap_grant`):
    /// the service's end of the channel is gone, and with it what it knew.
    pub fn unmap_revoked(&mut self, channel: u64) {
        let ranges: alloc::vec::Vec<(u64, u64)> = self
            .vmas
            .values()
            .filter(|v| matches!(v.backing, Backing::Revoked { channel: c } if c == channel))
            .map(|v| (v.start, v.end - v.start))
            .collect();
        for (start, len) in ranges {
            self.unmap(start, len);
        }
    }

    /// Removes every mapping of the memory object `cache`.
    pub fn unmap_object(&mut self, cache: &PageCache) {
        let ranges: alloc::vec::Vec<(u64, u64)> = self
            .vmas
            .values()
            .filter(|v| v.file().is_some_and(|(c, _)| core::ptr::eq(Arc::as_ptr(c), cache)))
            .map(|v| (v.start, v.end - v.start))
            .collect();
        for (start, len) in ranges {
            self.unmap(start, len);
        }
    }

    /// Writes into the address space (faulting pages in), also when it is
    /// not active, regardless of the areas' rights (the loader clears the
    /// bss part of a segment's last file page). A frame shared with others
    /// (copy-on-write, or a file's page cache) is copied first, unless the
    /// area is shared memory.
    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        self.write_as(addr, data, false)
    }

    /// Writes into the address space as a user write would (the area must
    /// be writable), also when it is not active (a fork child's copy).
    pub fn write_user(&mut self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        self.write_as(addr, data, true)
    }

    fn write_as(&mut self, addr: u64, data: &[u8], user: bool) -> Result<(), Fault> {
        let mut done = 0;
        while done < data.len() {
            let va = addr + done as u64;
            self.fault(va, Access { write: user, exec: false })?;
            let page = page_down(va);
            let v = self.vma(page).cloned().ok_or(Fault::Segv)?;
            if v.prot.write {
                if leaf_entry(self.l4, page).is_some_and(|e| e.flags().contains(COW)) {
                    self.break_cow(page, &v)?;
                }
            } else if !v.backing.shared() {
                self.privatize(page)?;
            }
            let e = leaf_entry(self.l4, page).ok_or(Fault::Segv)?;
            let phys = e.addr().as_u64() + va % PAGE;
            let chunk = ((PAGE - va % PAGE) as usize).min(data.len() - done);
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), memory::phys_to_virt(phys), chunk) };
            done += chunk;
        }
        Ok(())
    }

    /// Removes this space's mappings of `cache`'s pages from `index` on
    /// (a truncated file), private copies included.
    pub fn unmap_file(&mut self, cache: &PageCache, index: u64) {
        let ranges: alloc::vec::Vec<(u64, u64)> = self
            .vmas
            .values()
            .filter_map(|v| {
                let (c, first) = v.file()?;
                if !core::ptr::eq(Arc::as_ptr(c), cache) || first + v.pages() <= index {
                    return None;
                }
                Some((v.start + index.saturating_sub(first) * PAGE, v.end))
            })
            .collect();
        for (start, end) in ranges {
            self.clear_pages(start, end);
        }
    }

    /// The shared file mappings in [start, end) with the file pages they
    /// cover; None if part of the range is not mapped (msync: ENOMEM).
    pub fn file_ranges(&self, start: u64, end: u64) -> Option<alloc::vec::Vec<(Arc<PageCache>, core::ops::Range<u64>)>> {
        let mut out = alloc::vec::Vec::new();
        let mut at = start;
        while at < end {
            let v = self.vma(at)?;
            if let (true, Some((cache, first))) = (v.backing.shared(), v.file()) {
                let from = first + (at - v.start) / PAGE;
                let to = first + (v.end.min(end) - v.start) / PAGE;
                out.push((cache.clone(), from..to));
            }
            at = v.end;
        }
        Some(out)
    }

    /// Write-protects `pages` (file page indices) of `cache` in this
    /// space's shared mappings of it, so the next store faults and marks
    /// the page dirty again (write-back).
    pub fn write_protect_file(&mut self, cache: &PageCache, pages: &[u64]) {
        let ranges: alloc::vec::Vec<(u64, u64, u64)> = self
            .vmas
            .values()
            .filter(|v| v.backing.shared())
            .filter_map(|v| {
                let (c, first) = v.file()?;
                core::ptr::eq(Arc::as_ptr(c), cache).then_some((v.start, v.end, first))
            })
            .collect();
        for (start, end, first) in ranges {
            let (mut lo, mut hi) = (u64::MAX, 0);
            for &index in pages.iter().filter(|&&i| i >= first && i - first < (end - start) / PAGE) {
                let va = start + (index - first) * PAGE;
                if let Some(e) = leaf_entry(self.l4, va) {
                    let flags = e.flags();
                    if flags.contains(PageTableFlags::WRITABLE) {
                        e.set_flags((flags - PageTableFlags::WRITABLE) | COW);
                        (lo, hi) = (lo.min(va), hi.max(va + PAGE));
                    }
                }
            }
            if lo < hi {
                tlb::shootdown(&self.tlb, lo, hi);
            }
        }
    }

    // -------------------------------------------------------------- fork

    /// Copy-on-write clone for fork: both spaces share every private frame
    /// (writable pages become read-only COW pages in both); shared memory
    /// stays shared. The child's writable private memory is committed again
    /// (it may diverge), so fork can fail with ENOMEM.
    pub fn clone_user(&self) -> Result<AddressSpace, Fault> {
        let charged: u64 = self.vmas.values().filter(|v| v.charged).map(|v| v.pages()).sum();
        if !memory::commit(charged) {
            return Err(Fault::Oom);
        }
        let Some(mut new) = AddressSpace::new() else {
            memory::uncommit(charged);
            return Err(Fault::Oom);
        };
        // Granted pages are the service's alone: a child does not get them.
        new.vmas = self.vmas.iter().filter(|(_, v)| !matches!(v.backing, Backing::Granted { .. } | Backing::Revoked { .. })).map(|(&k, v)| (k, v.clone())).collect();
        new.stats.virt_pages.store(new.vmas.values().map(|v| v.pages()).sum(), Ordering::Relaxed);
        let mut mapper = new.mapper();
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let vmas = &self.vmas;
        // Return errors only outside of with_frames: dropping `new` needs the lock.
        let result = memory::with_frames(|frames| {
            let mut frames = UserFrames(frames);
            let l4 = table_at(self.l4);
            for i4 in 0..256 {
                for (i3, l3e) in children(&l4[i4]) {
                    for (i2, l2e) in children(l3e) {
                        let l1 = table_at(PhysFrame::containing_address(l2e.addr()));
                        for (i1, leaf) in l1.iter_mut().enumerate().filter(|(_, e)| !e.is_unused()) {
                            let va = (i4 as u64) << 39 | (i3 as u64) << 30 | (i2 as u64) << 21 | (i1 as u64) << 12;
                            let area = vmas.range(..=va).next_back().map(|(_, v)| v).filter(|v| va < v.end);
                            if area.is_some_and(|v| matches!(v.backing, Backing::Granted { .. })) {
                                continue;
                            }
                            let shared = area.is_some_and(|v| v.backing.shared());
                            let mut flags = leaf.flags();
                            if !shared && flags.contains(PageTableFlags::WRITABLE) {
                                flags.remove(PageTableFlags::WRITABLE);
                                flags.insert(COW);
                                leaf.set_flags(flags);
                            }
                            let frame = PhysFrame::containing_address(leaf.addr());
                            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
                            unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, &mut frames) }
                                .map_err(|_| Fault::Oom)?
                                .ignore();
                            // Only a successful mapping owns a reference.
                            frames.0.share(frame);
                            new.stats.add_resident(1);
                        }
                    }
                }
            }
            Ok(())
        });
        // The parent's pages just lost their write permission, also for
        // its other threads.
        tlb::shootdown(&self.tlb, 0, USER_END);
        new.brk_start = self.brk_start;
        new.brk_end = self.brk_end;
        new.exe = self.exe.clone();
        if let Some(instance) = &self.instance {
            new.attach(instance.clone(), self.program)?;
        }
        // (The child registers with the file caches when it gets its Mm.)
        result.map(|_| new)
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Every task left it (see `tlb::switch`) before the last reference went.
        debug_assert!(!self.active(), "dropping the loaded address space");
        let charged: u64 = self.vmas.values().filter(|v| v.charged).map(|v| v.pages()).sum();
        memory::uncommit(charged);
        // Shared objects drop with the areas, after the frames' mappings.
        let vmas = core::mem::take(&mut self.vmas);
        memory::with_frames(|frames| unsafe {
            free_level(frames, self.l4, 4);
            // The normal view's lower tables are the program's (freed
            // above) and the instance's (freed with it).
            if let Some(normal) = self.normal {
                frames.deallocate_frame(normal);
            }
        });
        drop(vmas);
        if self.program {
            if let Some(instance) = &self.instance {
                instance.program_gone();
            }
        }
    }
}

fn zeroed_frame() -> Result<PhysFrame, Fault> {
    let frame = memory::with_frames(|f| UserFrames(f).allocate_frame()).ok_or(Fault::Oom)?;
    unsafe { core::ptr::write_bytes(memory::phys_to_virt(frame.start_address().as_u64()), 0, PAGE as usize) };
    Ok(frame)
}

/// Recursively frees all lower-half frames, including the table itself.
pub(super) unsafe fn free_level(frames: &mut memory::frame::PhysFrameAllocator, table_frame: PhysFrame, level: u8) {
    let table = table_at(table_frame);
    let entries = if level == 4 { 0..256 } else { 0..512 };
    for i in entries {
        let entry = &table[i];
        if entry.is_unused() {
            continue;
        }
        let frame = PhysFrame::containing_address(entry.addr());
        if level == 1 {
            unsafe { frames.deallocate_frame(frame) };
        } else {
            unsafe { free_level(frames, frame, level - 1) };
        }
    }
    unsafe { frames.deallocate_frame(table_frame) };
}

/// Used entries of the table `entry` points to (empty if unused).
fn children(entry: &PageTableEntry) -> impl Iterator<Item = (usize, &'static PageTableEntry)> {
    let table: Option<&'static PageTable> = (!entry.is_unused()).then(|| &*table_at(PhysFrame::containing_address(entry.addr())));
    table.into_iter().flat_map(|t| t.iter().enumerate()).filter(|(_, e)| !e.is_unused())
}

fn table_at(frame: PhysFrame) -> &'static mut PageTable {
    unsafe { &mut *(memory::phys_to_virt(frame.start_address().as_u64()) as *mut PageTable) }
}

/// The level-1 entry for `va` if it holds a frame (present or kept for
/// PROT_NONE), with all upper levels present.
fn leaf_entry(l4: PhysFrame, va: u64) -> Option<&'static mut PageTableEntry> {
    let v = VirtAddr::new(va);
    let mut table = table_at(l4);
    for index in [v.p4_index(), v.p3_index(), v.p2_index()] {
        let e = &table[index];
        if !e.flags().contains(PageTableFlags::PRESENT) || e.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }
        table = table_at(PhysFrame::containing_address(e.addr()));
    }
    let e = &mut table[v.p1_index()];
    (!e.is_unused()).then_some(e)
}

/// Calls `f` for every used level-1 entry (present or kept for PROT_NONE)
/// in [start, end), skipping whole tables that are not there, so sparse
/// huge ranges (reservations of gigabytes) cost what is mapped in them.
fn for_each_leaf(l4: PhysFrame, start: u64, end: u64, mut f: impl FnMut(u64, &'static mut PageTableEntry)) {
    let mut va = page_down(start);
    'outer: while va < end {
        let v = VirtAddr::new(va);
        let mut frame = l4;
        for (index, shift) in [(v.p4_index(), 39), (v.p3_index(), 30), (v.p2_index(), 21)] {
            let e = &table_at(frame)[index];
            if !e.flags().contains(PageTableFlags::PRESENT) || e.flags().contains(PageTableFlags::HUGE_PAGE) {
                va = ((va >> shift) + 1) << shift;
                continue 'outer;
            }
            frame = PhysFrame::containing_address(e.addr());
        }
        let stop = end.min(((va >> 21) + 1) << 21);
        while va < stop {
            let e = &mut table_at(frame)[((va >> 12) & 511) as usize];
            if !e.is_unused() {
                f(va, e);
            }
            va += PAGE;
        }
    }
}

/// The page fault handler's part: satisfies a fault of the running task
/// at `va` in its address space. May sleep (the address space is locked,
/// a file page may be read), so interrupts must be enabled.
pub fn handle_fault(va: u64, access: Access) -> Result<(), Fault> {
    let mm = super::current_mm().ok_or(Fault::Segv)?;
    let result = loop {
        let (result, awaited) = {
            let mut space = mm.lock();
            let result = space.fault_or_retry(va, access, false);
            (result, space.awaited.take())
        };
        match (result, awaited) {
            // The page comes from a pager: waited for with the space
            // unlocked, then the fault is tried again (the mapping may have
            // changed meanwhile).
            (Err(Fault::Retry), Some((cache, index))) => {
                if cache.wait_page(index).is_err() {
                    break Err(Fault::Bus);
                }
            }
            (Err(Fault::Retry), None) => break Err(Fault::Bus),
            (result, _) => break result,
        }
    };
    if access.write && result.is_ok() {
        // The store may have made a page dirty: too many, and this writer
        // writes back (with no lock held).
        crate::fs::cache::balance_dirty();
    }
    result
}
