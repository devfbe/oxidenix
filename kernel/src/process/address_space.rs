use crate::memory;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
    PhysFrame, Size4KiB, Translate,
};
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::VirtAddr;

pub const USER_END: u64 = 0x0000_8000_0000_0000;
const PAGE: u64 = 4096;

/// Eigene Level-4-Tabelle: untere Haelfte gehoert dem Prozess,
/// obere Haelfte (Kernel) wird mit dem Kernel-Adressraum geteilt.
pub struct AddressSpace {
    l4: PhysFrame,
}

impl AddressSpace {
    pub fn new() -> Option<Self> {
        let l4 = memory::with_frames(|f| f.allocate_frame())?;
        let table = table_at(l4);
        let kernel = table_at(memory::kernel_l4());
        for i in 0..256 {
            table[i].set_unused();
        }
        for i in 256..512 {
            table[i] = kernel[i].clone();
        }
        Some(AddressSpace { l4 })
    }

    fn mapper(&self) -> OffsetPageTable<'static> {
        unsafe { OffsetPageTable::new(table_at(self.l4), memory::phys_offset()) }
    }

    /// Mappt [start, start+len) mit genullten Frames. Bereits gemappte Seiten
    /// bekommen die Vereinigung der Flags.
    pub fn map_zeroed(&mut self, start: u64, len: u64, flags: PageTableFlags) -> Result<(), &'static str> {
        if len == 0 {
            return Ok(());
        }
        let end = start.checked_add(len).filter(|&e| e <= USER_END).ok_or("Adresse ausserhalb des Userspace")?;
        let flags = flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let mut mapper = self.mapper();
        let first = Page::<Size4KiB>::containing_address(VirtAddr::new(start));
        let last = Page::containing_address(VirtAddr::new(end - 1));
        memory::with_frames(|frames| {
            for page in Page::range_inclusive(first, last) {
                if mapper.translate_page(page).is_ok() {
                    let merged = merge(leaf_flags(&mapper, page), flags);
                    unsafe { mapper.update_flags(page, merged) }
                        .map_err(|_| "update_flags fehlgeschlagen")?
                        .ignore();
                    continue;
                }
                let frame = frames.allocate_frame().ok_or("kein Speicher frei")?;
                unsafe { core::ptr::write_bytes(memory::phys_to_virt(frame.start_address().as_u64()), 0, PAGE as usize) };
                unsafe { mapper.map_to_with_table_flags(page, frame, flags, parent, frames) }
                    .map_err(|_| "map_to fehlgeschlagen")?
                    .ignore();
            }
            Ok(())
        })
    }

    /// Entfernt alle Mappings in [start, start+len) und gibt die Frames frei.
    pub fn unmap(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len).filter(|&e| e <= USER_END && len > 0) else { return };
        let mut mapper = self.mapper();
        let first = Page::<Size4KiB>::containing_address(VirtAddr::new(start));
        let last = Page::containing_address(VirtAddr::new(end - 1));
        memory::with_frames(|frames| {
            for page in Page::range_inclusive(first, last) {
                if let Ok((frame, flush)) = mapper.unmap(page) {
                    flush.flush();
                    unsafe { frames.deallocate_frame(frame) };
                }
            }
        });
    }

    /// Schreibt in den Adressraum, ohne ihn aktivieren zu muessen.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
        let mapper = self.mapper();
        let mut done = 0;
        while done < data.len() {
            let va = addr + done as u64;
            let phys = mapper.translate_addr(VirtAddr::new(va)).ok_or("Ziel nicht gemappt")?;
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

    /// Tiefe Kopie aller User-Seiten (fuer fork).
    pub fn clone_user(&self) -> Result<AddressSpace, &'static str> {
        let new = AddressSpace::new().ok_or("kein Speicher frei")?;
        let mut mapper = new.mapper();
        let parent = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        // Fehler erst ausserhalb von with_frames zurueckgeben: Drop von `new` braucht den Lock.
        memory::with_frames(|frames| {
            let l4 = table_at(self.l4);
            for i4 in 0..256 {
                for (i3, l3e) in children(&l4[i4]) {
                    for (i2, l2e) in children(l3e) {
                        for (i1, leaf) in children(l2e) {
                            let va = (i4 as u64) << 39 | (i3 as u64) << 30 | (i2 as u64) << 21 | (i1 as u64) << 12;
                            let frame = frames.allocate_frame().ok_or("kein Speicher frei")?;
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    memory::phys_to_virt(leaf.addr().as_u64()),
                                    memory::phys_to_virt(frame.start_address().as_u64()),
                                    PAGE as usize,
                                )
                            };
                            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
                            unsafe { mapper.map_to_with_table_flags(page, frame, leaf.flags(), parent, frames) }
                                .map_err(|_| "map_to fehlgeschlagen")?
                                .ignore();
                        }
                    }
                }
            }
            Ok(())
        })?;
        Ok(new)
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

/// Gibt rekursiv alle Frames der unteren Haelfte frei, inklusive der Tabelle selbst.
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

/// Belegte Eintraege der Tabelle, auf die `entry` zeigt (leer, wenn ungenutzt).
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

/// Prueft im aktiven Adressraum, ob [addr, addr+len) komplett fuer den
/// Userspace gemappt ist.
pub fn user_range_ok(addr: u64, len: u64, write: bool) -> bool {
    let Some(end) = addr.checked_add(len) else { return false };
    if end > USER_END {
        return false;
    }
    if len == 0 {
        return true;
    }
    let mut need = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if write {
        need |= PageTableFlags::WRITABLE;
    }
    let mapper = unsafe { memory::active_page_table() };
    let first = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
    let last = Page::containing_address(VirtAddr::new(end - 1));
    Page::range_inclusive(first, last).all(|p| leaf_flags(&mapper, p).contains(need))
}
