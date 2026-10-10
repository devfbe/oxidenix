//! A heap that gives memory back: the Linux server's (docs/design/linux-server.md,
//! "The server's heap"). Its arena is address space whose pages are memory
//! only while committed (`Backing`: the kernel's `SYS_SHARED_COMMIT` and
//! `SYS_SHARED_DECOMMIT` in the server, a model in the host tests).
//!
//! - **Pages** (`pages`): runs of pages, lowest first fit over bitmaps of
//!   allocated and committed pages with a radix tree of free-run summaries.
//!   A run with uncommitted pages is committed before it is handed out
//!   (failing: null, the caller's ENOMEM), with up to `COMMIT_AHEAD` free
//!   pages after it; the kernel never commits a page by itself.
//! - **Slabs** (`slabs`): objects of up to 2 KiB come from one-page slabs
//!   of the size classes (`slab::class`), O(1) under the class's lock; a
//!   slab whose last object goes returns to the pages (but one per class).
//!   Larger objects are runs of whole pages.
//! - **Trimming**: when the free committed pages exceed `TRIM_FLOOR` (2 MiB)
//!   and a quarter of all committed, the heap asks for a trim
//!   (`Backing::trim_wanted`); the trim (`trim`, on a thread that holds no
//!   lock) decommits free pages from the arena's top down until
//!   `KEEP_FLOOR` (1 MiB) or an eighth of what is allocated is left;
//!   `trim(true)` (memory is short) keeps nothing and takes the classes'
//!   empty slabs too.
//!
//! Nothing about free memory is kept in it: bitmaps, summaries and slab
//! descriptors are in their own area, committed as the arena grows and
//! never decommitted, so a decommitted page is never read. No lock is held
//! across a kernel call: a run is reserved (marked allocated) under the
//! pages' lock, committed or decommitted with it released, and finished
//! under it again. Lock order: a class's lock is never held while the
//! pages' is taken; `grow` (serializing growth of the metadata) is taken
//! before the pages' lock.
//!
//! The crate holds locks of the caller's kind (`RawLock`: the server's
//! futex mutex, a spin lock in the host tests) and allocates nothing.

#![no_std]

mod pages;
mod slabs;

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ptr::null_mut;
use pages::Pages;
use slabs::{Class, Place};

pub const PAGE: usize = 4096;
/// Pages of a chunk: one bitmap word each per 64, one summary.
pub const CHUNK_PAGES: usize = 512;
pub const CHUNK: usize = CHUNK_PAGES * PAGE;

/// Where the heap's memory comes from.
pub trait Backing {
    /// Commits [addr, addr + len) (pages): true if all are memory now. On
    /// false some may be; the heap decommits the range.
    fn commit(&self, addr: usize, len: usize) -> bool;
    /// Gives [addr, addr + len) back; its contents are lost.
    fn decommit(&self, addr: usize, len: usize);
    /// The heap has more free committed memory than it keeps: `trim` it
    /// soon, from where no lock is held. Called without the heap's locks.
    fn trim_wanted(&self);
}

/// A lock of the caller's kind.
///
/// # Safety
/// `lock` gives mutual exclusion until the matching `unlock`.
pub unsafe trait RawLock {
    const NEW: Self;
    fn lock(&self);
    /// # Safety
    /// The calling thread holds the lock.
    unsafe fn unlock(&self);
}

struct Locked<L, T> {
    lock: L,
    data: UnsafeCell<T>,
}

struct Guard<'a, L: RawLock, T> {
    locked: &'a Locked<L, T>,
}

impl<L: RawLock, T> Locked<L, T> {
    const fn new(data: T) -> Self {
        Locked { lock: L::NEW, data: UnsafeCell::new(data) }
    }

    fn lock(&self) -> Guard<'_, L, T> {
        self.lock.lock();
        Guard { locked: self }
    }
}

impl<L: RawLock, T> core::ops::Deref for Guard<'_, L, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.locked.data.get() }
    }
}

impl<L: RawLock, T> core::ops::DerefMut for Guard<'_, L, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.locked.data.get() }
    }
}

impl<L: RawLock, T> Drop for Guard<'_, L, T> {
    fn drop(&mut self) {
        unsafe { self.locked.lock.unlock() };
    }
}

/// The heap's numbers, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Committed: the arena's committed pages and the metadata.
    pub committed: usize,
    /// In use by objects: slots of the slabs, and large objects' pages.
    pub in_use: usize,
    /// Committed and not in use.
    pub free: usize,
    /// Of the arena grown so far, not committed.
    pub decommitted: usize,
    /// The metadata (part of `committed`).
    pub meta: usize,
}

pub struct Heap<L: RawLock, B: Backing> {
    backing: B,
    place: Place,
    pages: Locked<L, Pages>,
    grow: Locked<L, ()>,
    classes: [Locked<L, Class>; slab::CLASSES],
}

// The locks serialize every access to the state.
unsafe impl<L: RawLock + Sync, B: Backing + Sync> Sync for Heap<L, B> {}

/// Bytes of metadata an arena of `max_chunks` chunks needs before it
/// (`Heap::new`'s `meta`).
pub const fn meta_len(max_chunks: usize) -> usize {
    pages::meta_len(max_chunks)
}

impl<L: RawLock, B: Backing> Heap<L, B> {
    /// A heap over an arena at `arena` (page-aligned) of up to `max_chunks`
    /// chunks, with its metadata at `meta` (`meta_len(max_chunks)` bytes,
    /// page-aligned, apart from the arena). Nothing is committed yet.
    pub const fn new(backing: B, meta: usize, arena: usize, max_chunks: usize) -> Self {
        let pages = Pages::new(meta, arena, max_chunks);
        let place = Place { descs: pages.descs, arena };
        Heap {
            backing,
            place,
            pages: Locked::new(pages),
            grow: Locked::new(()),
            classes: [const { Locked::new(Class::new()) }; slab::CLASSES],
        }
    }

    pub fn backing(&self) -> &B {
        &self.backing
    }

    /// Memory for `layout`, or null (out of memory).
    pub fn alloc(&self, layout: Layout) -> *mut u8 {
        match slab::class(&layout) {
            Some(c) => self.alloc_small(c),
            None => self.alloc_large(layout),
        }
    }

    /// # Safety
    /// `ptr` came from `alloc` with this `layout` and is not used any more.
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        match slab::class(&layout) {
            Some(c) => {
                let page = unsafe { self.classes[c].lock().put(self.place, ptr as usize, c) };
                if let Some(page) = page {
                    self.free_pages(page, 1);
                }
            }
            None => self.free_pages(ptr as usize, layout.size().div_ceil(PAGE)),
        }
    }

    fn alloc_small(&self, c: usize) -> *mut u8 {
        loop {
            if let Some(p) = self.classes[c].lock().take(self.place, c) {
                return p as *mut u8;
            }
            // A new slab, with the class's lock let go meanwhile (the pages
            // may need a kernel call).
            let page = self.alloc_pages(1);
            if page.is_null() {
                return null_mut();
            }
            self.classes[c].lock().add(self.place, page as usize, c);
        }
    }

    fn alloc_large(&self, layout: Layout) -> *mut u8 {
        let n = layout.size().div_ceil(PAGE);
        let align = layout.align() / PAGE;
        if align <= 1 {
            return self.alloc_pages(n);
        }
        // Over-aligned: a longer run, its ends given back.
        let Some(total) = n.checked_add(align - 1) else { return null_mut() };
        let p = self.alloc_pages(total);
        if p.is_null() {
            return p;
        }
        let start = (p as usize).next_multiple_of(layout.align());
        let head = (start - p as usize) / PAGE;
        if head > 0 {
            self.free_pages(p as usize, head);
        }
        if total - head > n {
            self.free_pages(start + n * PAGE, total - head - n);
        }
        start as *mut u8
    }

    /// A run of `n` pages, committed; null if none can be had.
    fn alloc_pages(&self, n: usize) -> *mut u8 {
        loop {
            let run = self.pages.lock().reserve(n);
            let Some(run) = run else {
                if self.grow(n) {
                    continue;
                }
                return null_mut();
            };
            let addr = self.place.arena + run.page * PAGE;
            if run.commit {
                let len = (run.pages + run.extra) * PAGE;
                let ok = self.backing.commit(addr, len);
                if !ok {
                    self.backing.decommit(addr, len);
                }
                let ask = {
                    let mut pages = self.pages.lock();
                    pages.finish(&run, ok);
                    pages.ask_trim()
                };
                if ask {
                    self.backing.trim_wanted();
                }
                if !ok {
                    return null_mut();
                }
            }
            return addr as *mut u8;
        }
    }

    fn free_pages(&self, addr: usize, n: usize) {
        let ask = {
            let mut pages = self.pages.lock();
            let page = pages.page_of(addr);
            pages.free(page, n);
            pages.ask_trim()
        };
        if ask {
            self.backing.trim_wanted();
        }
    }

    /// Grows the arena so that a run of `n` pages fits (its metadata
    /// committed first, with only the growth's lock held); whether it does.
    fn grow(&self, n: usize) -> bool {
        let _grow = self.grow.lock();
        let plan = {
            let pages = self.pages.lock();
            if pages.fits(n) {
                return true;
            }
            pages.plan(n)
        };
        let Some(plan) = plan else { return false };
        for (i, &(addr, len)) in plan.ranges.iter().enumerate() {
            if len == 0 {
                continue;
            }
            if !self.backing.commit(addr, len) {
                // (Only the new part: what was committed before stays.)
                self.backing.decommit(addr, len);
                return false;
            }
            self.pages.lock().meta_committed(i, addr + len);
        }
        self.pages.lock().extend(&plan);
        true
    }

    /// Gives free committed memory back: down to what the heap keeps, or
    /// (`everything`: memory is short) all of it, the classes' empty slabs
    /// too. From a thread that holds none of the heap's locks.
    pub fn trim(&self, everything: bool) {
        self.pages.lock().trim_begins();
        if everything {
            for (c, class) in self.classes.iter().enumerate() {
                let _ = c;
                let mut out = [0usize; slabs::EMPTY_KEEP as usize];
                let n = class.lock().drain(self.place, &mut out);
                for &page in &out[..n] {
                    self.free_pages(page, 1);
                }
            }
        }
        loop {
            let mut runs = [(0usize, 0usize); 16];
            let n = {
                let mut pages = self.pages.lock();
                let keep = if everything { 0 } else { pages.keep() };
                pages.take_for_trim(keep, &mut runs)
            };
            if n == 0 {
                break;
            }
            for &(page, len) in &runs[..n] {
                self.backing.decommit(self.place.arena + page * PAGE, len * PAGE);
            }
            self.pages.lock().trimmed(&runs[..n]);
        }
    }

    pub fn stats(&self) -> Stats {
        let (mut slab_pages, mut slab_used) = (0, 0);
        for (c, class) in self.classes.iter().enumerate() {
            let class = class.lock();
            slab_pages += class.slabs;
            slab_used += class.used * slab::class_size(c);
        }
        let pages = self.pages.lock();
        let meta = pages.meta_bytes();
        let committed = pages.committed * PAGE + meta;
        // (Classes and pages read one after the other: a slab added in
        // between is counted as a large object for a moment.)
        let in_use = (pages.allocated.saturating_sub(slab_pages)) * PAGE + slab_used;
        Stats {
            committed,
            in_use,
            free: committed.saturating_sub(in_use + meta),
            decommitted: (pages.chunks * CHUNK_PAGES - pages.committed) * PAGE,
            meta,
        }
    }

    /// Checks the page allocator's counts and summaries (tests).
    pub fn verify(&self) {
        self.pages.lock().verify();
    }
}
