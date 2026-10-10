//! The heap over a model of the kernel's commits: objects keep their
//! contents, runs are the lowest that fit, free memory goes back when
//! trimmed, a decommitted page is never written (its poison is checked when
//! it is committed again) and a failed commit leaves the heap usable.

use pageheap::{Backing, Heap, RawLock, Stats, CHUNK, CHUNK_PAGES, KEEP_FLOOR, PAGE};
use std::alloc::Layout;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

const POISON: u8 = 0xa5;

struct Spin(AtomicBool);

unsafe impl RawLock for Spin {
    const NEW: Self = Spin(AtomicBool::new(false));
    fn lock(&self) {
        while self.0.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
    }
    unsafe fn unlock(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The kernel as the heap sees it: committed pages (zeroed when committed),
/// decommitted ones poisoned (and checked when committed again), commits
/// that fail on demand.
struct Model {
    committed: Mutex<BTreeSet<usize>>,
    decommitted: Mutex<BTreeSet<usize>>,
    /// Commits left before they fail (usize::MAX: never).
    budget: AtomicUsize,
    trims_wanted: AtomicUsize,
}

impl Model {
    fn new() -> Model {
        Model {
            committed: Mutex::new(BTreeSet::new()),
            decommitted: Mutex::new(BTreeSet::new()),
            budget: AtomicUsize::new(usize::MAX),
            trims_wanted: AtomicUsize::new(0),
        }
    }

    fn pages(&self) -> usize {
        self.committed.lock().unwrap().len()
    }
}

impl Backing for Model {
    fn commit(&self, addr: usize, len: usize) -> bool {
        assert!(addr % PAGE == 0 && len % PAGE == 0 && len > 0);
        let mut committed = self.committed.lock().unwrap();
        let mut decommitted = self.decommitted.lock().unwrap();
        for page in (addr..addr + len).step_by(PAGE) {
            if committed.contains(&page) {
                continue;
            }
            let left = self.budget.load(Ordering::Relaxed);
            if left == 0 {
                return false;
            }
            if left != usize::MAX {
                self.budget.store(left - 1, Ordering::Relaxed);
            }
            let mem = unsafe { std::slice::from_raw_parts_mut(page as *mut u8, PAGE) };
            if decommitted.remove(&page) {
                assert!(mem.iter().all(|&b| b == POISON), "decommitted page {page:#x} was written");
            }
            mem.fill(0);
            committed.insert(page);
        }
        true
    }

    fn decommit(&self, addr: usize, len: usize) {
        let mut committed = self.committed.lock().unwrap();
        let mut decommitted = self.decommitted.lock().unwrap();
        for page in (addr..addr + len).step_by(PAGE) {
            if committed.remove(&page) {
                unsafe { std::slice::from_raw_parts_mut(page as *mut u8, PAGE) }.fill(POISON);
                decommitted.insert(page);
            }
        }
    }

    fn trim_wanted(&self) {
        self.trims_wanted.fetch_add(1, Ordering::Relaxed);
    }
}

/// A heap over memory of the host: `chunks` chunks of arena and their
/// metadata (leaked: the tests are short).
fn heap(chunks: usize) -> &'static Heap<Spin, Model> {
    let meta_len = pageheap::meta_len(chunks);
    let meta = unsafe { std::alloc::alloc(Layout::from_size_align(meta_len, PAGE).unwrap()) } as usize;
    let arena = unsafe { std::alloc::alloc(Layout::from_size_align(chunks * CHUNK, PAGE).unwrap()) } as usize;
    assert!(meta != 0 && arena != 0);
    Box::leak(Box::new(Heap::new(Model::new(), meta, arena, chunks)))
}

/// Whether every byte of the arena the heap uses is committed (its
/// committed count matches the model's).
fn agrees(h: &Heap<Spin, Model>) -> Stats {
    let s = h.stats();
    assert_eq!(s.committed, h.backing().pages() * PAGE, "{s:?}");
    h.verify();
    s
}

fn fill(p: *mut u8, len: usize, tag: u8) {
    unsafe { std::ptr::write_bytes(p, tag, len) };
}

fn check(p: *mut u8, len: usize, tag: u8) {
    assert!(unsafe { std::slice::from_raw_parts(p, len) }.iter().all(|&b| b == tag), "block at {p:?} changed");
}

#[test]
fn objects_keep_their_contents() {
    let h = heap(64);
    let mut blocks = Vec::new();
    for i in 0..5000usize {
        let size = 1 + (i * 7919) % 20_000;
        let layout = Layout::from_size_align(size, 8).unwrap();
        let p = h.alloc(layout);
        assert!(!p.is_null());
        fill(p, size, i as u8);
        blocks.push((p, layout, i as u8));
        if i % 3 == 2 {
            let (p, layout, tag) = blocks.swap_remove(i / 3 % blocks.len());
            check(p, layout.size(), tag);
            unsafe { h.dealloc(p, layout) };
        }
    }
    agrees(h);
    for (p, layout, tag) in blocks {
        check(p, layout.size(), tag);
        unsafe { h.dealloc(p, layout) };
    }
    let s = agrees(h);
    assert_eq!(s.in_use, 0);
}

#[test]
fn freed_memory_goes_back_when_trimmed() {
    let h = heap(32);
    let small = Layout::from_size_align(64, 8).unwrap();
    let large = Layout::from_size_align(5 * PAGE, 8).unwrap();
    let mut blocks = Vec::new();
    // 16 MiB of small objects and 8 MiB of large ones.
    for i in 0..(16 << 20) / 64 {
        let p = h.alloc(small);
        fill(p, 64, i as u8);
        blocks.push((p, small, i as u8));
    }
    for i in 0..(8 << 20) / (5 * PAGE) {
        let p = h.alloc(large);
        fill(p, large.size(), i as u8);
        blocks.push((p, large, i as u8));
    }
    let full = agrees(h);
    assert!(full.committed >= 24 << 20);
    // A few survivors, spread over the arena.
    let keep: Vec<_> = blocks.iter().copied().step_by(9973).collect();
    let wanted = h.backing().trims_wanted.load(Ordering::Relaxed);
    for (i, &(p, layout, tag)) in blocks.iter().enumerate() {
        if i % 9973 != 0 {
            check(p, layout.size(), tag);
            unsafe { h.dealloc(p, layout) };
        }
    }
    assert!(h.backing().trims_wanted.load(Ordering::Relaxed) > wanted, "no trim asked for");
    h.trim();
    let trimmed = agrees(h);
    // What is kept: the survivors' pages, the free pages kept (1 MiB), the
    // metadata, a few slabs.
    // (Each survivor keeps its page: a slab, or a large block's five.)
    let survivors: usize = keep.iter().map(|&(_, layout, _)| layout.size().div_ceil(PAGE)).sum();
    assert!(trimmed.committed < full.committed / 4, "{trimmed:?}");
    assert!(trimmed.committed - trimmed.meta <= (survivors + 8) * PAGE + (1 << 20), "{trimmed:?}");
    // A shrink gives back about what it is asked for, never below the
    // floor of free pages it keeps.
    let big = Layout::from_size_align(600 * PAGE, 8).unwrap();
    let p = h.alloc(big);
    unsafe { h.dealloc(p, big) };
    let before = agrees(h);
    h.shrink(100);
    let after = agrees(h);
    assert_eq!(before.committed - after.committed, 100 * PAGE, "{before:?} {after:?}");
    h.shrink(usize::MAX);
    let shrunk = agrees(h);
    assert!(shrunk.committed - shrunk.meta <= (survivors + 8 + KEEP_FLOOR) * PAGE, "{shrunk:?}");
    assert!(shrunk.committed - shrunk.meta >= KEEP_FLOOR * PAGE, "{shrunk:?}");
    for &(p, layout, tag) in &keep {
        check(p, layout.size(), tag);
    }
    // And it serves again (committing what it gave back).
    for i in 0..(4 << 20) / 64 {
        let p = h.alloc(small);
        check(p, 0, 0);
        fill(p, 64, i as u8);
    }
    agrees(h);
    for &(p, layout, tag) in &keep {
        check(p, layout.size(), tag);
        unsafe { h.dealloc(p, layout) };
    }
    agrees(h);
}

#[test]
fn runs_are_the_lowest_that_fit() {
    let h = heap(8);
    // Large objects only (no slabs): the model is a bitmap of the pages.
    let mut used = vec![false; 8 * CHUNK_PAGES];
    let mut chunks = 0;
    let mut live: Vec<(usize, usize)> = Vec::new();
    let base = {
        let p = h.alloc(Layout::from_size_align(PAGE + 1, 8).unwrap());
        unsafe { h.dealloc(p, Layout::from_size_align(PAGE + 1, 8).unwrap()) };
        p as usize
    };
    let lowest = |used: &[bool], chunks: usize, n: usize| -> Option<usize> {
        let top = chunks * CHUNK_PAGES;
        let mut run = 0;
        for p in 0..top {
            run = if used[p] { 0 } else { run + 1 };
            if run == n {
                return Some(p + 1 - n);
            }
        }
        None
    };
    let mut seed = 12345u64;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..4000 {
        if live.is_empty() || rand() % 3 != 0 {
            let n = match rand() % 10 {
                0 => 1 + (rand() % 1200) as usize,
                1..=3 => 1 + (rand() % 64) as usize,
                _ => 1 + (rand() % 4) as usize,
            };
            let want = lowest(&used, chunks, n).or_else(|| {
                let grown = chunks + n.div_ceil(CHUNK_PAGES).max(1);
                if grown > 8 {
                    return None;
                }
                chunks = grown;
                lowest(&used, chunks, n)
            });
            let p = h.alloc(Layout::from_size_align(n * PAGE, 8).unwrap());
            match want {
                Some(page) => {
                    assert_eq!(p as usize, base + page * PAGE, "a run of {n}");
                    used[page..page + n].iter_mut().for_each(|u| *u = true);
                    live.push((page, n));
                }
                None => assert!(p.is_null()),
            }
        } else {
            let (page, n) = live.swap_remove(rand() as usize % live.len());
            used[page..page + n].iter_mut().for_each(|u| *u = false);
            unsafe { h.dealloc((base + page * PAGE) as *mut u8, Layout::from_size_align(n * PAGE, 8).unwrap()) };
        }
        if rand() % 500 == 0 {
            if rand() % 2 == 0 {
                h.trim();
            } else {
                h.shrink(rand() as usize % 1000);
            }
        }
    }
    agrees(h);
}

#[test]
fn a_failed_commit_is_out_of_memory_and_nothing_else() {
    let h = heap(4);
    let small = Layout::from_size_align(32, 8).unwrap();
    let mut blocks = Vec::new();
    for _ in 0..1000 {
        blocks.push(h.alloc(small));
    }
    h.backing().budget.store(0, Ordering::Relaxed);
    let mut failed = 0;
    for _ in 0..2000 {
        let p = h.alloc(small);
        if p.is_null() {
            failed += 1;
        } else {
            blocks.push(p);
        }
    }
    assert!(failed > 0);
    assert!(h.alloc(Layout::from_size_align(64 * PAGE, 8).unwrap()).is_null());
    agrees(h);
    h.backing().budget.store(usize::MAX, Ordering::Relaxed);
    let p = h.alloc(Layout::from_size_align(64 * PAGE, 8).unwrap());
    assert!(!p.is_null());
    fill(p, 64 * PAGE, 7);
    unsafe { h.dealloc(p, Layout::from_size_align(64 * PAGE, 8).unwrap()) };
    for p in blocks {
        unsafe { h.dealloc(p, small) };
    }
    assert_eq!(agrees(h).in_use, 0);
}

#[test]
fn over_aligned_blocks() {
    let h = heap(8);
    let mut blocks = Vec::new();
    for shift in 12..20 {
        let layout = Layout::from_size_align(3 * PAGE + 5, 1 << shift).unwrap();
        let p = h.alloc(layout);
        assert_eq!(p as usize % (1 << shift), 0);
        fill(p, layout.size(), shift as u8);
        blocks.push((p, layout));
    }
    for (p, layout) in blocks {
        check(p, layout.size(), layout.align().trailing_zeros() as u8);
        unsafe { h.dealloc(p, layout) };
    }
    assert_eq!(agrees(h).in_use, 0);
}

#[test]
fn threads_and_a_trimmer() {
    let h = heap(64);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                h.trim();
                h.shrink(usize::MAX);
                std::thread::yield_now();
            }
        });
        let workers: Vec<_> = (0..4u64)
            .map(|t| {
                s.spawn(move || {
                    let mut seed = 0x9e37_79b9_7f4a_7c15u64 ^ t;
                    let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
                    for i in 0..60_000usize {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        if live.len() < 3000 && seed % 5 != 0 {
                            let size = match seed % 7 {
                                0 => 3000 + (seed >> 8) as usize % 40_000,
                                _ => 1 + (seed >> 8) as usize % 2048,
                            };
                            let layout = Layout::from_size_align(size, 8).unwrap();
                            let p = h.alloc(layout);
                            assert!(!p.is_null());
                            fill(p, size, i as u8);
                            live.push((p, layout, i as u8));
                        } else if !live.is_empty() {
                            let (p, layout, tag) = live.swap_remove((seed >> 16) as usize % live.len());
                            check(p, layout.size(), tag);
                            unsafe { h.dealloc(p, layout) };
                        }
                    }
                    for (p, layout, tag) in live {
                        check(p, layout.size(), tag);
                        unsafe { h.dealloc(p, layout) };
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
    });
    h.shrink(usize::MAX);
    let s = agrees(h);
    assert_eq!(s.in_use, 0);
    assert!(s.free <= (KEEP_FLOOR + 8) * PAGE, "{s:?}");
}
