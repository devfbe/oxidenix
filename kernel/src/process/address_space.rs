use crate::memory;
use crate::memory::frame::UserFrames;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
    PhysFrame, Size4KiB, Translate,
};
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::VirtAddr;

pub const USER_END: u64 = 0x0000_8000_0000_0000;
const PAGE: u64 = 4096;
/// Marks a read-only mapping of a shared frame that becomes private on the
/// first write (an OS-available page table bit).
pub const COW: PageTableFlags = PageTableFlags::BIT_9;

/// Pages covering [start, end) in user space. Unlike `Page::range_inclusive`
/// this never steps past the last page, which for the top user page would
/// compute the non-canonical address `USER_END` and panic.
fn user_pages(start: u64, end: u64) -> impl Iterator<Item = Page<Size4KiB>> {
    (start & !(PAGE - 1)..end)
        .step_by(PAGE as usize)
        .map(|a| Page::containing_address(VirtAddr::new(a)))
}

/// Pages mapped in an address space, readable by anyone (procfs) while
/// only the owning process changes the mappings.
#[derive(Default)]
pub struct MemStats {
    pub pages: core::sync::atomic::AtomicU64,
}

/// Own level-4 table: the lower half belongs to the process, the upper
/// (kernel) half is shared with the kernel address space.
pub struct AddressSpace {
    l4: PhysFrame,
    pub stats: alloc::sync::Arc<MemStats>,
}

fn count(stats: &MemStats, delta: i64) {
    use core::sync::atomic::Ordering;
    if delta >= 0 {
        stats.pages.fetch_add(delta as u64, Ordering::Relaxed);
    } else {
        stats.pages.fetch_sub(delta.unsigned_abs(), Ordering::Relaxed);
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
        let stats = alloc::sync::Arc::try_new(MemStats::default()).ok()?;
        Some(AddressSpace { l4, stats })
    }

    fn mapper(&self) -> OffsetPageTable<'static> {
        unsafe { OffsetPageTable::new(table_at(self.l4), memory::phys_offset()) }
    }

    /// Maps [start, start+len) with zeroed frames. Pages that are already
    /// mapped get the union of the flags.
    pub fn map_zeroed(&mut self, start: u64, len: u64, flags: PageTableFlags) -> Result<(), &'static str> {
        if len == 0 {
            return Ok(());
        }
        let end = start.checked_add(len).filter(|&e| e <= USER_END).ok_or("address outside of user space")?;
        let flags = flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let mut mapper = self.mapper();
        memory::with_frames(|frames| {
            let mut frames = UserFrames(frames);
            for page in user_pages(start, end) {
                if let Ok(frame) = mapper.translate_page(page) {
                    let old = leaf_flags(&mapper, page);
                    let mut merged = merge(old, flags);
                    // A frame shared with another address space must never become
                    // writable in place; it gets copy-on-write semantics instead.
                    let shared = old.contains(COW) || frames.0.refcount(frame) > 1;
                    if shared && merged.contains(PageTableFlags::WRITABLE) {
                        merged.remove(PageTableFlags::WRITABLE);
                        merged.insert(COW);
                    }
                    unsafe { mapper.update_flags(page, merged) }
                        .map_err(|_| "update_flags failed")?
                        .ignore();
                    continue;
                }
                let frame = frames.allocate_frame().ok_or("out of memory")?;
                count(&self.stats, 1);
                unsafe { core::ptr::write_bytes(memory::phys_to_virt(frame.start_address().as_u64()), 0, PAGE as usize) };
                match unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, &mut frames) } {
                    Ok(flush) => flush.ignore(),
                    Err(_) => {
                        // Not mapped, so nothing else will ever free it.
                        unsafe { frames.0.deallocate_frame(frame) };
                        count(&self.stats, -1);
                        return Err("map_to failed");
                    }
                }
            }
            Ok(())
        })
    }

    /// Maps `pages` existing physical frames starting at `phys` (device DMA
    /// memory) to `start`. Each mapping holds a reference on its frame.
    pub fn map_phys(&mut self, start: u64, phys: u64, pages: u64, flags: PageTableFlags) -> Result<(), &'static str> {
        let end = start.checked_add(pages * PAGE).filter(|&e| e <= USER_END).ok_or("address outside of user space")?;
        let flags = flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let mut mapper = self.mapper();
        memory::with_frames(|frames| {
            for (i, page) in user_pages(start, end).enumerate() {
                let frame = PhysFrame::containing_address(x86_64::PhysAddr::new(phys + i as u64 * PAGE));
                let mut user = UserFrames(frames);
                unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, &mut user) }
                    .map_err(|_| "map_to failed")?
                    .ignore();
                frames.share(frame);
                count(&self.stats, 1);
            }
            Ok(())
        })
    }

    /// Removes all mappings in [start, start+len) and frees their frames.
    pub fn unmap(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len).filter(|&e| e <= USER_END && len > 0) else { return };
        let mut mapper = self.mapper();
        memory::with_frames(|frames| {
            for page in user_pages(start, end) {
                if let Ok((frame, flush)) = mapper.unmap(page) {
                    flush.flush();
                    unsafe { frames.deallocate_frame(frame) };
                    count(&self.stats, -1);
                }
            }
        });
    }

    /// Writes into the address space without activating it.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
        let mapper = self.mapper();
        let mut done = 0;
        while done < data.len() {
            let va = addr + done as u64;
            let phys = mapper.translate_addr(VirtAddr::new(va)).ok_or("target not mapped")?;
            let chunk = ((PAGE - va % PAGE) as usize).min(data.len() - done);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data[done..].as_ptr(),
                    memory::phys_to_virt(phys.as_u64()),
                    chunk,
                )
            };
            done += chunk;
        }
        Ok(())
    }

    /// Copy-on-write clone for fork: both spaces share every user frame;
    /// writable pages become read-only COW pages in both of them.
    pub fn clone_user(&self) -> Result<AddressSpace, &'static str> {
        let new = AddressSpace::new().ok_or("out of memory")?;
        let mut mapper = new.mapper();
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
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
                            let mut flags = leaf.flags();
                            if flags.contains(PageTableFlags::WRITABLE) {
                                flags.remove(PageTableFlags::WRITABLE);
                                flags.insert(COW);
                                leaf.set_flags(flags);
                            }
                            let frame = PhysFrame::containing_address(leaf.addr());
                            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
                            unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, &mut frames) }
                                .map_err(|_| "map_to failed")?
                                .ignore();
                            // Only a successful mapping owns a reference.
                            frames.0.share(frame);
                            count(&new.stats, 1);
                        }
                    }
                }
            }
            Ok(())
        });
        // The parent's pages just lost their write permission.
        if Cr3::read().0 == self.l4 {
            x86_64::instructions::tlb::flush_all();
        }
        result.map(|_| new)
    }

    pub fn activate(&self) {
        unsafe { Cr3::write(self.l4, Cr3Flags::empty()) };
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        if Cr3::read().0 == self.l4 {
            unsafe { Cr3::write(memory::kernel_l4(), Cr3Flags::empty()) };
        }
        memory::with_frames(|frames| unsafe {
            free_level(frames, self.l4, 4);
        });
    }
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
    let table: Option<&'static PageTable> = (!entry.is_unused())
        .then(|| &*table_at(PhysFrame::containing_address(entry.addr())));
    table
        .into_iter()
        .flat_map(|t| t.iter().enumerate())
        .filter(|(_, e)| !e.is_unused())
}

fn table_at(frame: PhysFrame) -> &'static mut PageTable {
    unsafe { &mut *(memory::phys_to_virt(frame.start_address().as_u64()) as *mut PageTable) }
}

fn leaf_flags(mapper: &OffsetPageTable, page: Page) -> PageTableFlags {
    use x86_64::structures::paging::mapper::TranslateResult;
    match mapper.translate(page.start_address()) {
        TranslateResult::Mapped { flags, .. } => flags,
        _ => PageTableFlags::empty(),
    }
}

fn merge(a: PageTableFlags, b: PageTableFlags) -> PageTableFlags {
    let nx = a.contains(PageTableFlags::NO_EXECUTE) && b.contains(PageTableFlags::NO_EXECUTE);
    let mut f = (a | b) - PageTableFlags::NO_EXECUTE;
    if nx {
        f |= PageTableFlags::NO_EXECUTE;
    }
    f
}

/// The level-1 entry for `va`, if all upper levels are present.
fn leaf_entry(l4: PhysFrame, va: u64) -> Option<&'static mut PageTableEntry> {
    let v = VirtAddr::new(va);
    let mut table = table_at(l4);
    for index in [v.p4_index(), v.p3_index(), v.p2_index()] {
        let e = &table[index];
        if e.is_unused() || e.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }
        table = table_at(PhysFrame::containing_address(e.addr()));
    }
    let e = &mut table[v.p1_index()];
    (!e.is_unused()).then_some(e)
}

/// Makes the copy-on-write page at `va` in the active address space
/// privately writable. Returns false if it is not a COW page or memory ran out.
pub fn resolve_cow(va: u64) -> bool {
    if va >= USER_END {
        return false;
    }
    let Some(entry) = leaf_entry(Cr3::read().0, va) else { return false };
    let flags = entry.flags();
    if !flags.contains(COW) {
        return false;
    }
    let writable = (flags - COW) | PageTableFlags::WRITABLE;
    let old = PhysFrame::containing_address(entry.addr());
    let done = memory::with_frames(|frames| {
        if frames.refcount(old) <= 1 {
            entry.set_flags(writable);
            return true;
        }
        let Some(new) = UserFrames(frames).allocate_frame() else { return false };
        unsafe {
            core::ptr::copy_nonoverlapping(
                memory::phys_to_virt(old.start_address().as_u64()),
                memory::phys_to_virt(new.start_address().as_u64()),
                PAGE as usize,
            );
            entry.set_addr(new.start_address(), writable);
            frames.deallocate_frame(old);
        }
        true
    });
    if done {
        x86_64::instructions::tlb::flush(VirtAddr::new(va));
    }
    done
}

/// Checks in the active address space whether [addr, addr+len) is fully
/// mapped for user space. For writes, copy-on-write pages are made
/// private first, since kernel writes would not fault on them.
pub fn user_range_ok(addr: u64, len: u64, write: bool) -> bool {
    let Some(end) = addr.checked_add(len) else { return false };
    if end > USER_END {
        return false;
    }
    if len == 0 {
        return true;
    }
    let need = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    let mapper = unsafe { memory::active_page_table() };
    user_pages(addr, end).all(|p| {
        let flags = leaf_flags(&mapper, p);
        if !flags.contains(need) {
            return false;
        }
        !write || flags.contains(PageTableFlags::WRITABLE) || resolve_cow(p.start_address().as_u64())
    })
}
