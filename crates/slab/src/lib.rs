//! Size classes for small allocations: objects of up to 2 KiB come from
//! slabs (4 KiB, aligned to 4 KiB) cut into equal slots, and a class's free
//! slots form an intrusive list, so allocating and freeing one is O(1).
//! A first-fit heap, by contrast, searches its free blocks on every
//! allocation, and the search grows with the heap's fragmentation.
//!
//! The crate holds no lock and allocates nothing itself: the caller keeps
//! one `FreeList` per class behind its own lock (the kernel's interrupt-safe
//! spinlocks, the Linux server's futex mutex), takes slabs from its backing
//! allocator, which also serves larger objects, and never gives slabs back
//! (a slab's slots stay its class's).
//!
//! Slots are aligned to their size (a power of two dividing the slab), so a
//! class serves every layout whose size and alignment fit it.

#![no_std]

use core::alloc::Layout;
use core::ptr::NonNull;

/// The size and alignment of a slab.
pub const SLAB_SIZE: usize = 4096;
/// The number of classes: 16, 32, ..., 2048 bytes.
pub const CLASSES: usize = 8;
const SMALLEST: usize = 16;

/// The class that serves `layout`, or None for one too large (for the
/// backing allocator).
pub fn class(layout: &Layout) -> Option<usize> {
    let need = layout.size().max(layout.align()).max(SMALLEST).checked_next_power_of_two()?;
    let c = need.trailing_zeros() as usize - SMALLEST.trailing_zeros() as usize;
    (c < CLASSES).then_some(c)
}

/// The slot size of class `c`.
pub const fn class_size(c: usize) -> usize {
    SMALLEST << c
}

/// The layout of a slab, for the backing allocator.
pub fn slab_layout() -> Layout {
    // SLAB_SIZE is a power of two: always valid.
    Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap_or(Layout::new::<u8>())
}

/// A free slot holds the link to the next one.
struct Node {
    next: Option<NonNull<Node>>,
}

/// The free slots of one class.
pub struct FreeList {
    head: Option<NonNull<Node>>,
    len: usize,
}

// The list only links memory it was given; whoever owns it serializes use.
unsafe impl Send for FreeList {}

impl FreeList {
    pub const fn new() -> Self {
        FreeList { head: None, len: 0 }
    }

    /// Free slots on the list.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// A free slot, if there is one.
    pub fn pop(&mut self) -> Option<NonNull<u8>> {
        let node = self.head?;
        self.head = unsafe { node.as_ref().next };
        self.len -= 1;
        Some(node.cast())
    }

    /// Returns a slot.
    ///
    /// # Safety
    /// `slot` came from `pop` of a list of the same class and is not used
    /// any more.
    pub unsafe fn push(&mut self, slot: NonNull<u8>) {
        let node = slot.cast::<Node>();
        unsafe { node.as_ptr().write(Node { next: self.head }) };
        self.head = Some(node);
        self.len += 1;
    }

    /// Cuts a new slab into slots of class `c` and adds them.
    ///
    /// # Safety
    /// `slab` is `SLAB_SIZE` bytes, aligned to `SLAB_SIZE`, owned by the
    /// caller and given to this list for good.
    pub unsafe fn add_slab(&mut self, slab: NonNull<u8>, c: usize) {
        let size = class_size(c);
        // Highest first, so the slab is handed out from its start.
        for i in (0..SLAB_SIZE / size).rev() {
            unsafe { self.push(NonNull::new_unchecked(slab.as_ptr().add(i * size))) };
        }
    }
}

impl Default for FreeList {
    fn default() -> Self {
        Self::new()
    }
}
