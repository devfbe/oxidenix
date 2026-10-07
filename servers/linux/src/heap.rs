//! The server's heap: one allocator for every thread of the instance, in
//! the shared region, growing by `SYS_SHARED_MAP` when it runs out.

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

unsafe impl GlobalAlloc for ServerHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
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

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.lock().0.deallocate(NonNull::new_unchecked(ptr), layout) };
    }
}
