//! Size classes for small allocations: objects of up to 2 KiB come from
//! slabs (4 KiB, aligned to 4 KiB) cut into equal slots, and a class's free
//! slots form an intrusive list, so allocating and freeing one is O(1).
//! A first-fit heap, by contrast, searches its free blocks on every
//! allocation, and the search grows with the heap's fragmentation.
//!
//! The crate holds no lock and allocates nothing itself: the caller keeps
//! one `FreeList` per class behind its own lock (the kernel's interrupt-safe
//! spinlocks, the Linux server's futex mutex) and takes slabs from its
//! backing allocator, which also serves larger objects. Slabs whose slots
//! are all free go back to it with `reclaim` (before it grows), so a
//! workload that once needed many objects of one size does not keep that
//! memory from all others.
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

    /// Gives back the slabs of class `c` whose slots are all free: each is
    /// taken off the list and handed to `give` (for the backing allocator).
    /// Returns how many. The list is sorted by address first (a merge sort
    /// of the links, in place: nothing is allocated), so a slab's free slots
    /// lie together; O(n log n) in the free slots, for when the backing
    /// allocator would otherwise have to grow. What stays is in address
    /// order.
    pub fn reclaim(&mut self, c: usize, mut give: impl FnMut(NonNull<u8>)) -> usize {
        let per = SLAB_SIZE / class_size(c);
        self.head = unsafe { sort(self.head) };
        let slab_of = |n: NonNull<Node>| n.as_ptr() as usize & !(SLAB_SIZE - 1);
        let mut given = 0;
        // `link` is where the run being looked at hangs: the list's head or
        // the last kept node's `next`.
        let mut link: *mut Option<NonNull<Node>> = &mut self.head;
        unsafe {
            while let Some(first) = *link {
                let slab = slab_of(first);
                let mut count = 1;
                let mut last = first;
                while let Some(next) = last.as_ref().next.filter(|&n| slab_of(n) == slab) {
                    count += 1;
                    last = next;
                }
                let after = last.as_ref().next;
                if count == per {
                    *link = after;
                    self.len -= per;
                    give(NonNull::new_unchecked(slab as *mut u8));
                    given += 1;
                } else {
                    link = &mut (*last.as_ptr()).next;
                }
            }
        }
        given
    }
}

/// Sorts a list by address (Simon Tatham's bottom-up merge sort of a
/// singly linked list: no recursion, no allocation).
///
/// # Safety
/// Every node on the list is a free slot of the caller's list.
unsafe fn sort(mut list: Option<NonNull<Node>>) -> Option<NonNull<Node>> {
    let mut width = 1usize;
    loop {
        let mut p = list;
        list = None;
        let mut tail: Option<NonNull<Node>> = None;
        let mut merges = 0;
        while let Some(start) = p {
            merges += 1;
            // Two runs of up to `width` nodes: from `p` and from `q`.
            let mut q = Some(start);
            let mut psize = 0;
            while psize < width {
                psize += 1;
                q = q.and_then(|n| unsafe { n.as_ref().next });
                if q.is_none() {
                    break;
                }
            }
            let mut qsize = width;
            let mut p_at = Some(start);
            while psize > 0 || (qsize > 0 && q.is_some()) {
                let take_p = match (psize > 0, qsize > 0 && q.is_some()) {
                    (true, false) => true,
                    (false, _) => false,
                    (true, true) => p_at.map(|n| n.as_ptr() as usize) <= q.map(|n| n.as_ptr() as usize),
                };
                let e = if take_p {
                    let e = p_at.expect("counted");
                    p_at = unsafe { e.as_ref().next };
                    psize -= 1;
                    e
                } else {
                    let e = q.expect("checked");
                    q = unsafe { e.as_ref().next };
                    qsize -= 1;
                    e
                };
                match tail {
                    Some(t) => unsafe { (*t.as_ptr()).next = Some(e) },
                    None => list = Some(e),
                }
                tail = Some(e);
            }
            p = q;
        }
        if let Some(t) = tail {
            unsafe { (*t.as_ptr()).next = None };
        }
        if merges <= 1 {
            return list;
        }
        width *= 2;
    }
}

impl Default for FreeList {
    fn default() -> Self {
        Self::new()
    }
}
