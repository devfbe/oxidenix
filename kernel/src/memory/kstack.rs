//! Kernel stacks.
//!
//! Every task's kernel stack lives in its own slot of a dedicated virtual
//! region, mapped page by page from physical frames, with unmapped guard
//! pages below it: an overflow faults (and ends in the double-fault
//! handler) instead of silently overwriting the neighbor. The frames go
//! back to the frame allocator when the task is gone, rather than staying
//! in the kernel heap, which never shrinks. Stacks of user tasks are
//! charged to the commit limit, so creating threads and processes fails
//! with ENOMEM before memory promised to programs runs out.
//!
//! A freed slot's address range may still be cached in TLBs of CPUs that
//! ran the task. Nothing touches it there, but a new stack in the same slot
//! must not be reached through those stale entries: freed slots are reused
//! only after one global flush of the region, which collects many of them.

use super::{phys_offset, phys_to_virt, FRAMES};
use crate::sync::IrqSpinLock;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

pub const STACK_SIZE: u64 = 64 * 1024;
const PAGES: u64 = STACK_SIZE / 4096;
/// The region (one level-4 entry, created at boot so every address space
/// shares it) and its slots: a stack on top of an equally large guard.
pub const REGION: u64 = 0xffff_e000_0000_0000;
const SLOT: u64 = 2 * STACK_SIZE;
const SLOTS: usize = 4096;
const WORDS: usize = SLOTS / 64;

struct Slots {
    used: [u64; WORDS],
    /// Freed since the last flush of the region: not reusable yet.
    dirty: [u64; WORDS],
}

/// Also serializes all changes to the region's page tables.
static SLOTS_: IrqSpinLock<Slots> = IrqSpinLock::new(Slots { used: [0; WORDS], dirty: [0; WORDS] });

/// Creates the region's level-3 table in the kernel's level-4 table, so
/// address spaces created later share everything below it.
pub fn init(kernel_l4: PhysFrame, frames: &mut impl FrameAllocator<Size4KiB>) {
    let l3 = frames.allocate_frame().expect("no frame for the kernel stack region");
    unsafe { core::ptr::write_bytes(phys_to_virt(l3.start_address().as_u64()), 0, 4096) };
    let l4 = unsafe { &mut *(phys_to_virt(kernel_l4.start_address().as_u64()) as *mut PageTable) };
    let index = VirtAddr::new(REGION).p4_index();
    assert!(l4[index].is_unused(), "the kernel stack region is taken by the bootloader");
    l4[index].set_frame(l3, PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
}

fn mapper() -> OffsetPageTable<'static> {
    let l4 = unsafe { &mut *(phys_to_virt(super::kernel_l4().start_address().as_u64()) as *mut PageTable) };
    unsafe { OffsetPageTable::new(l4, phys_offset()) }
}

fn stack_bottom(slot: usize) -> u64 {
    REGION + slot as u64 * SLOT + (SLOT - STACK_SIZE)
}

/// A mapped, zeroed kernel stack; unmapped and freed on drop.
pub struct KernelStack {
    slot: usize,
    charged: bool,
}

impl KernelStack {
    /// A new stack; `charge` counts it against the commit limit (stacks
    /// of user tasks). None if memory or slots run out.
    pub fn new(charge: bool) -> Option<KernelStack> {
        if charge && !super::commit(PAGES) {
            return None;
        }
        // (Mapped with the frames locked: free frames made first, by
        // reclaim if need be.)
        if charge {
            super::ensure_user_frames(PAGES + 3);
        }
        let stack = Self::map(charge);
        if stack.is_none() && charge {
            super::uncommit(PAGES);
        }
        stack
    }

    fn map(charged: bool) -> Option<KernelStack> {
        let mut flushed = false;
        loop {
            let mut slots = SLOTS_.lock();
            let free = (0..SLOTS).find(|&i| (slots.used[i / 64] | slots.dirty[i / 64]) & (1 << (i % 64)) == 0);
            let Some(slot) = free else {
                if flushed || slots.dirty.iter().all(|&w| w == 0) {
                    return None;
                }
                // Make the freed slots reusable: one flush on every CPU,
                // without the lock (a shootdown must not wait for a CPU
                // spinning on it).
                let dirty = slots.dirty;
                drop(slots);
                super::super::process::tlb::shootdown_kernel(REGION, REGION + SLOTS as u64 * SLOT);
                let mut slots = SLOTS_.lock();
                for (d, f) in slots.dirty.iter_mut().zip(dirty) {
                    *d &= !f;
                }
                flushed = true;
                continue;
            };
            let mut mapper = mapper();
            let mut guard = FRAMES.lock();
            let frames = guard.as_mut()?;
            if !frames.user_may_take(PAGES) {
                return None;
            }
            let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
            for i in 0..PAGES {
                let page = Page::<Size4KiB>::containing_address(VirtAddr::new(stack_bottom(slot) + i * 4096));
                let mapped = frames.allocate_frame().and_then(|frame| {
                    unsafe { core::ptr::write_bytes(phys_to_virt(frame.start_address().as_u64()), 0, 4096) };
                    match unsafe { mapper.map_to(page, frame, flags, frames) } {
                        Ok(flush) => {
                            flush.flush();
                            Some(())
                        }
                        Err(_) => {
                            unsafe { frames.deallocate_frame(frame) };
                            None
                        }
                    }
                });
                if mapped.is_none() {
                    unmap(&mut mapper, frames, slot, i);
                    return None;
                }
            }
            slots.used[slot / 64] |= 1 << (slot % 64);
            return Some(KernelStack { slot, charged });
        }
    }

    /// The address just above the stack (the initial stack pointer).
    pub fn top(&self) -> u64 {
        stack_bottom(self.slot) + STACK_SIZE
    }
}

/// Unmaps the first `pages` pages of `slot` and frees their frames.
fn unmap(mapper: &mut OffsetPageTable<'static>, frames: &mut super::frame::PhysFrameAllocator, slot: usize, pages: u64) {
    for i in 0..pages {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(stack_bottom(slot) + i * 4096));
        if let Ok((frame, flush)) = mapper.unmap(page) {
            flush.flush();
            unsafe { frames.deallocate_frame(frame) };
        }
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let mut slots = SLOTS_.lock();
        {
            let mut mapper = mapper();
            let mut guard = FRAMES.lock();
            let frames = guard.as_mut().expect("memory is set up");
            unmap(&mut mapper, frames, self.slot, PAGES);
        }
        slots.used[self.slot / 64] &= !(1 << (self.slot % 64));
        slots.dirty[self.slot / 64] |= 1 << (self.slot % 64);
        drop(slots);
        if self.charged {
            super::uncommit(PAGES);
        }
    }
}
