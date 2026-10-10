//! The server's heap: one allocator for every thread of the instance, in
//! the heap area of the shared region (`pageheap`: page runs and slabs of
//! size classes over an arena whose pages are committed and decommitted
//! with the kernel's `SYS_SHARED_COMMIT` and `SYS_SHARED_DECOMMIT`). Its
//! metadata lies at `HEAP_BASE`, the arena after it.
//!
//! Memory goes back: when the heap holds more free committed memory than it
//! keeps, the timer thread trims it (at most every `TRIM_INTERVAL`, so a
//! workload that frees and allocates in turn does not commit and decommit
//! all the time), and the service thread gives back all of it when the
//! kernel says memory is short (`EVENT_SHRINK`, `shrink`). Its locks are
//! the server's futex mutex (`sync::RawMutex`, in the image: never in
//! memory the heap decommits).

use crate::sync::RawMutex;
use crate::syscall;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use pageheap::{Backing, Heap, CHUNK};
use restricted::{HEAP_BASE, SYS_CLOCK_READ, SYS_SHARED_COMMIT, SYS_SHARED_DECOMMIT, SYS_SHARED_DECOMMIT_RUNS, THREADS_BASE};

/// Most chunks of arena (64 GiB: far beyond any commit limit).
const MAX_CHUNKS: usize = 32 * 1024;
const META: usize = HEAP_BASE as usize;
const ARENA: usize = (META + pageheap::meta_len(MAX_CHUNKS)).next_multiple_of(CHUNK);
const _: () = assert!(ARENA + MAX_CHUNKS * CHUNK <= THREADS_BASE as usize);
/// A trim waits at least this long after the last (nanoseconds).
const TRIM_INTERVAL: u64 = 500_000_000;

/// The kernel's commits of the heap area.
pub struct Kernel;

impl Backing for Kernel {
    fn commit(&self, addr: usize, len: usize) -> bool {
        syscall(SYS_SHARED_COMMIT, [addr as u64, len as u64, 0, 0, 0, 0]) >= 0
    }

    fn decommit(&self, addr: usize, len: usize) {
        syscall(SYS_SHARED_DECOMMIT, [addr as u64, len as u64, 0, 0, 0, 0]);
    }

    fn decommit_runs(&self, runs: &[(usize, usize)]) {
        // (In the image's stack: never memory the heap decommits.)
        let mut list = [0u64; 2 * pageheap::TRIM_RUNS];
        for chunk in runs.chunks(pageheap::TRIM_RUNS) {
            for (i, &(addr, len)) in chunk.iter().enumerate() {
                (list[2 * i], list[2 * i + 1]) = (addr as u64, len as u64);
            }
            syscall(SYS_SHARED_DECOMMIT_RUNS, [list.as_ptr() as u64, chunk.len() as u64, 0, 0, 0, 0]);
        }
    }

    fn trim_wanted(&self) {
        TRIM.store(true, Ordering::Release);
        crate::timer::changed();
    }
}

pub struct ServerHeap(Heap<RawMutex, Kernel>);

impl ServerHeap {
    pub const fn new() -> Self {
        ServerHeap(Heap::new(Kernel, META, ARENA, MAX_CHUNKS))
    }

    pub fn stats(&self) -> pageheap::Stats {
        self.0.stats()
    }
}

unsafe impl GlobalAlloc for ServerHeap {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0.alloc(layout)
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) }
    }
}

/// A trim was asked for (`Kernel::trim_wanted`), and when the last ran.
static TRIM: AtomicBool = AtomicBool::new(false);
static LAST_TRIM: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [restricted::CLOCK_MONO, 0, 0, 0, 0, 0]).max(0) as u64
}

/// When the timer thread should trim (monotonic nanoseconds), 0 if no trim
/// is asked for.
pub fn trim_due() -> u64 {
    if !TRIM.load(Ordering::Acquire) {
        return 0;
    }
    (LAST_TRIM.load(Ordering::Relaxed) + TRIM_INTERVAL).max(1)
}

/// The timer thread: trims the heap if that is asked for and due.
pub fn trim_if_due() {
    let due = trim_due();
    if due == 0 || now() < due {
        return;
    }
    TRIM.store(false, Ordering::Relaxed);
    crate::HEAP.0.trim();
    LAST_TRIM.store(now(), Ordering::Relaxed);
}

/// Pages `EVENT_SHRINK` asked for and the worker has not given yet (the
/// largest request), and how many shrinks the worker made (for the tests).
static SHRINK_ASKED: AtomicU64 = AtomicU64::new(0);
pub static SHRINKS: AtomicU64 = AtomicU64::new(0);

/// `EVENT_SHRINK` (on the service thread, which must not wait for the
/// heap's locks or for diskfs): the worker shrinks.
pub fn ask_shrink(pages: u64) {
    SHRINK_ASKED.fetch_max(pages.max(1), Ordering::AcqRel);
    crate::scm::request();
}

/// The worker: gives back about the pages asked for, of the heap's free
/// memory beyond its floor and of /data's unused clean inodes.
pub fn shrink_if_asked() {
    let pages = SHRINK_ASKED.swap(0, Ordering::AcqRel);
    if pages == 0 {
        return;
    }
    crate::HEAP.0.shrink(pages as usize);
    crate::datafs::shrink(pages);
    LAST_TRIM.store(now(), Ordering::Relaxed);
    SHRINKS.fetch_add(1, Ordering::Relaxed);
}
