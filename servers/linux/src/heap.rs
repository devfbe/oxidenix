//! The server's heap: one allocator for every thread of the instance, in
//! the shared region, growing by `SYS_SHARED_MAP` when it runs out. Objects
//! of up to 2 KiB come from slabs of size classes (`slab`: O(1), a mutex
//! per class), whose pages the heap supplies; larger ones from the heap
//! itself (first fit).

use crate::sync::Mutex;
use crate::syscall;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{null_mut, NonNull};
use linked_list_allocator::Heap;
use restricted::SYS_SHARED_MAP;

/// The heap grows at least this much at a time.
const GROWTH: usize = 1024 * 1024;

pub struct ServerHeap(Mutex<(Heap, bool)>);

impl ServerHeap {
    pub const fn new() -> Self {
        ServerHeap(Mutex::new((Heap::empty(), false)))
    }
}

/// More memory from the kernel: (start, length), or None.
fn more(at_least: usize) -> Option<(usize, usize)> {
    let len = at_least.max(GROWTH).next_multiple_of(4096);
    let start = syscall(SYS_SHARED_MAP, [len as u64, 0, 0, 0, 0, 0]);
    (start > 0).then_some((start as usize, len))
}

/// Free slots of each size class.
static SLABS: [Mutex<slab::FreeList>; slab::CLASSES] = [const { Mutex::new(slab::FreeList::new()) }; slab::CLASSES];

unsafe impl GlobalAlloc for ServerHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(c) = slab::class(&layout) else { return self.alloc_large(layout) };
        if let Some(p) = SLABS[c].lock().pop() {
            return p.as_ptr();
        }
        let Some(page) = NonNull::new(self.alloc_large(slab::slab_layout())) else { return null_mut() };
        let mut list = SLABS[c].lock();
        unsafe { list.add_slab(page, c) };
        list.pop().map_or(null_mut(), |p| p.as_ptr())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        match slab::class(&layout) {
            Some(c) => unsafe { SLABS[c].lock().push(ptr) },
            None => unsafe { self.0.lock().0.deallocate(ptr, layout) },
        }
    }
}

impl ServerHeap {
    /// From the heap itself, growing it as needed.
    fn alloc_large(&self, layout: Layout) -> *mut u8 {
        let mut guard = self.0.lock();
        let (heap, started) = &mut *guard;
        loop {
            if *started {
                if let Ok(p) = heap.allocate_first_fit(layout) {
                    return p.as_ptr();
                }
            }
            // Room for the block, its alignment and the allocator's bookkeeping.
            let Some((start, len)) = more(layout.size() + layout.align() + 64) else { return null_mut() };
            if *started {
                // The heap grows at its top: the new memory follows on.
                if start == heap.top() as usize {
                    unsafe { heap.extend(len) };
                    continue;
                }
                // Not contiguous (cannot happen while only the heap grows
                // the region): this memory is lost, the request fails.
                return null_mut();
            }
            unsafe { heap.init(start as *mut u8, len) };
            *started = true;
        }
    }

}
