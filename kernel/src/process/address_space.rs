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
//! The threads of a process share one address space (`Mm`), behind a lock
//! that may be held while a fault sleeps. Every change that removes,
//! write-protects or moves a mapping is followed by a TLB shootdown
//! (`tlb`), and frames are freed only after it.

use super::tlb::{self, Tlb};
use crate::fs::Inode;
use crate::memory;
use crate::memory::frame::UserFrames;
use crate::sync::{IrqSpinLock, Mutex, MutexGuard};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

pub const USER_END: u64 = 0x0000_8000_0000_0000;
pub const PAGE: u64 = 4096;
/// Marks a read-only mapping of a shared frame that becomes private on the
/// first write (an OS-available page table bit).
pub const COW: PageTableFlags = PageTableFlags::BIT_9;
/// A frame kept for an area that currently denies access (PROT_NONE); the
/// entry is not present.
const PROT_NONE: PageTableFlags = PageTableFlags::BIT_10;

/// Highest address `mmap` hands out; the stack lives above.
pub const MMAP_TOP: u64 = 0x0000_7000_0000_0000;
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

/// Anonymous memory shared between processes (MAP_SHARED|MAP_ANONYMOUS,
/// inherited across fork): every mapping sees the same frames.
pub struct SharedAnon {
    /// Frames by page index; the object holds one reference on each.
    frames: IrqSpinLock<BTreeMap<u64, PhysFrame>>,
    /// Pages committed for the object (released when it goes).
    committed: u64,
}

impl Drop for SharedAnon {
    fn drop(&mut self) {
        let frames = core::mem::take(&mut *self.frames.lock());
        memory::with_frames(|f| {
            for frame in frames.into_values() {
                unsafe { f.deallocate_frame(frame) };
            }
        });
        memory::uncommit(self.committed);
    }
}

/// What backs an area's pages.
#[derive(Clone)]
pub enum Backing {
    /// Zero-filled private memory.
    Anon,
    /// Shared anonymous memory; `index0` is the object page of `start`.
    Shared { object: Arc<SharedAnon>, index0: u64 },
    /// A private copy of a file, read page by page on first access;
    /// `offset` is the file offset of `start`.
    File { inode: Arc<Inode>, offset: u64 },
    /// Device memory, mapped up front (DMA areas).
    Device,
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
        match &mut v.backing {
            Backing::File { offset, .. } => *offset += shift,
            Backing::Shared { index0, .. } => *index0 += shift / PAGE,
            _ => {}
        }
        v
    }

    /// Whether this area would need commit when writable.
    fn private(&self) -> bool {
        matches!(self.backing, Backing::Anon | Backing::File { .. })
    }
}

/// Mapped (resident) and virtual pages, readable by anyone (procfs) while
/// only the owning process changes them.
#[derive(Default)]
pub struct MemStats {
    pub pages: AtomicU64,
    pub virt_pages: AtomicU64,
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
    pub tlb: Arc<Tlb>,
    pub stats: Arc<MemStats>,
    /// Areas by start address; they never overlap.
    vmas: BTreeMap<u64, Vma>,
    /// The heap (brk): from the end of the program to the current break.
    pub brk_start: u64,
    pub brk_end: u64,
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
    pub fn new(space: AddressSpace) -> Option<Arc<Mm>> {
        let (tlb, stats) = (space.tlb.clone(), space.stats.clone());
        Arc::try_new(Mm { tlb, stats, space: Mutex::new(space) }).ok()
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
        Some(AddressSpace { l4, tlb, stats, vmas: BTreeMap::new(), brk_start: 0, brk_end: 0 })
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
        // Coverage check first: no change on failure.
        let mut at = start;
        while at < end {
            let v = self.vma(at).ok_or(Fault::Oom)?;
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
                let shared_area = matches!(self.vma(va).map(|v| &v.backing), Some(Backing::Shared { .. }));
                if flags.contains(PageTableFlags::WRITABLE) && !shared_area && (was_cow || frames.refcount(frame) > 1) {
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
        if old_end > v.end || matches!(v.backing, Backing::Device) {
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
            if let Backing::Shared { index0, .. } = &mut ext.backing {
                *index0 += 1;
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
        tlb::shootdown(&self.tlb, from, from + len);
    }

    // ----------------------------------------------------------- faults

    /// Satisfies an access to `va`: maps a frame on first use, copies a
    /// copy-on-write page on a write, grows a stack. Works on any address
    /// space (the loader fills spaces that are not active yet).
    pub fn fault(&mut self, va: u64, access: Access) -> Result<(), Fault> {
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
        let (frame, writable_ok) = self.new_frame(page, &v)?;
        let mut flags = v.prot.flags();
        if !writable_ok && flags.contains(PageTableFlags::WRITABLE) {
            flags.remove(PageTableFlags::WRITABLE);
            flags.insert(COW);
        }
        self.install(page, frame, flags)
    }

    /// A frame with the page's initial contents, and whether it may be
    /// mapped writable directly.
    fn new_frame(&mut self, page: u64, v: &Vma) -> Result<(PhysFrame, bool), Fault> {
        match &v.backing {
            Backing::Anon => Ok((zeroed_frame()?, true)),
            Backing::Device => Err(Fault::Segv),
            Backing::Shared { object, index0 } => {
                let index = index0 + (page - v.start) / PAGE;
                let mut frames = object.frames.lock();
                let frame = match frames.get(&index) {
                    Some(&f) => f,
                    None => {
                        let f = zeroed_frame()?;
                        frames.insert(index, f);
                        f
                    }
                };
                // The mapping holds its own reference.
                memory::with_frames(|f| f.share(frame));
                Ok((frame, true))
            }
            Backing::File { inode, offset } => {
                let file_off = offset + (page - v.start);
                if file_off >= inode.size() {
                    return Err(Fault::Bus);
                }
                let frame = zeroed_frame()?;
                let buf = unsafe { core::slice::from_raw_parts_mut(memory::phys_to_virt(frame.start_address().as_u64()), PAGE as usize) };
                // Reading may sleep (a remote filesystem): no lock is held.
                if inode.read_at(file_off, buf).is_err() {
                    memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
                    return Err(Fault::Bus);
                }
                Ok((frame, true))
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
        // The entry was not present before, so no TLB holds it.
        count(&self.stats.pages, 1);
        Ok(())
    }

    /// A write to a copy-on-write page: copy it unless this mapping is
    /// the frame's last user.
    fn break_cow(&mut self, page: u64, v: &Vma) -> Result<(), Fault> {
        let e = leaf_entry(self.l4, page).ok_or(Fault::Segv)?;
        let flags = e.flags();
        let writable = ((flags - COW) | PageTableFlags::WRITABLE) & !PROT_NONE;
        let old = PhysFrame::containing_address(e.addr());
        let shared_area = matches!(v.backing, Backing::Shared { .. });
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
    pub fn populate(&mut self, start: u64, len: u64, write: bool) -> Result<(), Fault> {
        for page in user_pages(start, start.saturating_add(len)) {
            self.fault(page.start_address().as_u64(), Access { write, exec: false })?;
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

    /// Writes into the address space (faulting pages in), also when it is
    /// not active (the loader).
    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        let mut done = 0;
        while done < data.len() {
            let va = addr + done as u64;
            self.fault(va, Access { write: false, exec: false })?;
            let e = leaf_entry(self.l4, page_down(va)).ok_or(Fault::Segv)?;
            let phys = e.addr().as_u64() + va % PAGE;
            let chunk = ((PAGE - va % PAGE) as usize).min(data.len() - done);
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), memory::phys_to_virt(phys), chunk) };
            done += chunk;
        }
        Ok(())
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
        new.vmas = self.vmas.clone();
        new.stats.virt_pages.store(self.stats.virt_pages.load(Ordering::Relaxed), Ordering::Relaxed);
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
                            let shared = vmas
                                .range(..=va)
                                .next_back()
                                .is_some_and(|(_, v)| va < v.end && matches!(v.backing, Backing::Shared { .. } | Backing::Device));
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
                            count(&new.stats.pages, 1);
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
        });
        drop(vmas);
    }
}

/// A new shared anonymous memory object of `pages` pages (committed).
pub fn new_shared(pages: u64) -> Result<Arc<SharedAnon>, Fault> {
    if !memory::commit(pages) {
        return Err(Fault::Oom);
    }
    Arc::try_new(SharedAnon { frames: IrqSpinLock::new(BTreeMap::new()), committed: pages }).map_err(|_| {
        memory::uncommit(pages);
        Fault::Oom
    })
}

fn zeroed_frame() -> Result<PhysFrame, Fault> {
    let frame = memory::with_frames(|f| UserFrames(f).allocate_frame()).ok_or(Fault::Oom)?;
    unsafe { core::ptr::write_bytes(memory::phys_to_virt(frame.start_address().as_u64()), 0, PAGE as usize) };
    Ok(frame)
}

/// Recursively frees all lower-half frames, including the table itself.
unsafe fn free_level(frames: &mut memory::frame::PhysFrameAllocator, table_frame: PhysFrame, level: u8) {
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
    let mut space = mm.lock();
    space.fault(va, access)
}
