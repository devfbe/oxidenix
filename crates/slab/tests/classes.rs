//! Size classes and free lists of the slab allocator.

use slab::{class, class_size, FreeList, CLASSES, SLAB_SIZE};
use std::alloc::{alloc, dealloc, Layout};
use std::collections::HashSet;
use std::ptr::NonNull;

fn slab_page() -> NonNull<u8> {
    NonNull::new(unsafe { alloc(Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap()) }).unwrap()
}

#[test]
fn layouts_map_to_the_smallest_class_that_fits_and_aligns() {
    let l = |size, align| Layout::from_size_align(size, align).unwrap();
    assert_eq!(class(&l(1, 1)).map(class_size), Some(16));
    assert_eq!(class(&l(16, 8)).map(class_size), Some(16));
    assert_eq!(class(&l(17, 8)).map(class_size), Some(32));
    assert_eq!(class(&l(24, 64)).map(class_size), Some(64));
    assert_eq!(class(&l(2048, 8)).map(class_size), Some(2048));
    assert_eq!(class(&l(2049, 8)), None);
    assert_eq!(class(&l(16, 4096)), None);
    assert_eq!(class(&l(0, 1)).map(class_size), Some(16));
    assert_eq!(class_size(CLASSES - 1), 2048);
}

#[test]
fn a_slab_gives_distinct_aligned_slots_until_empty() {
    for c in 0..CLASSES {
        let size = class_size(c);
        let page = slab_page();
        let mut list = FreeList::new();
        unsafe { list.add_slab(page, c) };
        assert_eq!(list.len(), SLAB_SIZE / size);
        let mut seen = HashSet::new();
        while let Some(p) = list.pop() {
            let a = p.as_ptr() as usize;
            assert_eq!(a % size, 0, "class {size}: slot aligned to its size");
            assert!(a >= page.as_ptr() as usize && a + size <= page.as_ptr() as usize + SLAB_SIZE);
            assert!(seen.insert(a), "slot handed out twice");
            // The slot is the caller's: writing all of it is fine.
            unsafe { p.as_ptr().write_bytes(0xa5, size) };
        }
        assert_eq!(seen.len(), SLAB_SIZE / size);
        unsafe { dealloc(page.as_ptr(), Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap()) };
    }
}

#[test]
fn freed_slots_come_back_last_in_first_out() {
    let page = slab_page();
    let mut list = FreeList::new();
    unsafe { list.add_slab(page, 2) };
    let a = list.pop().unwrap();
    let b = list.pop().unwrap();
    let before = list.len();
    unsafe {
        list.push(a);
        list.push(b);
    }
    assert_eq!(list.len(), before + 2);
    assert_eq!(list.pop(), Some(b));
    assert_eq!(list.pop(), Some(a));
}

#[test]
fn many_allocations_and_frees_keep_contents() {
    // A model: every live slot holds its own pattern until it is freed.
    let mut lists: Vec<FreeList> = (0..CLASSES).map(|_| FreeList::new()).collect();
    let mut pages = Vec::new();
    let mut live: Vec<(usize, NonNull<u8>, u8)> = Vec::new();
    let mut seed = 0x1234_5678u64;
    let mut rand = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..20_000u32 {
        if live.is_empty() || rand() % 3 != 0 {
            let c = (rand() % CLASSES as u64) as usize;
            let p = match lists[c].pop() {
                Some(p) => p,
                None => {
                    let page = slab_page();
                    pages.push(page);
                    unsafe { lists[c].add_slab(page, c) };
                    lists[c].pop().unwrap()
                }
            };
            let tag = round as u8;
            unsafe { p.as_ptr().write_bytes(tag, class_size(c)) };
            live.push((c, p, tag));
        } else {
            let i = (rand() % live.len() as u64) as usize;
            let (c, p, tag) = live.swap_remove(i);
            let bytes = unsafe { std::slice::from_raw_parts(p.as_ptr(), class_size(c)) };
            assert!(bytes.iter().all(|&b| b == tag), "a live slot was overwritten");
            unsafe { lists[c].push(p) };
        }
    }
    for (c, p, tag) in live {
        let bytes = unsafe { std::slice::from_raw_parts(p.as_ptr(), class_size(c)) };
        assert!(bytes.iter().all(|&b| b == tag));
    }
    for page in pages {
        unsafe { dealloc(page.as_ptr(), Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap()) };
    }
}

/// A workload that once needed many slots of a class must not keep their
/// slabs from everything else for good: `reclaim` gives back exactly the
/// slabs whose slots are all free, whatever order they were freed in, and
/// the list stays usable.
#[test]
fn reclaim_gives_back_slabs_whose_slots_are_all_free() {
    for c in 0..CLASSES {
        let per = SLAB_SIZE / class_size(c);
        let pages: Vec<NonNull<u8>> = (0..6).map(|_| slab_page()).collect();
        let mut list = FreeList::new();
        for &p in &pages {
            unsafe { list.add_slab(p, c) };
        }
        let mut taken: Vec<NonNull<u8>> = std::iter::from_fn(|| list.pop()).collect();
        assert_eq!(taken.len(), 6 * per);
        // Free them in a scrambled order, except one slot of slab 1 and
        // one of slab 4.
        let keep: Vec<usize> = [1usize, 4].iter().map(|&s| pages[s].as_ptr() as usize).collect();
        let mut kept = Vec::new();
        let mut i = 0usize;
        while !taken.is_empty() {
            i = (i * 7919 + 13) % taken.len();
            let p = taken.swap_remove(i);
            let slab = p.as_ptr() as usize & !(SLAB_SIZE - 1);
            if keep.contains(&slab) && !kept.iter().any(|k: &NonNull<u8>| k.as_ptr() as usize & !(SLAB_SIZE - 1) == slab) {
                kept.push(p);
                continue;
            }
            unsafe { list.push(p) };
        }
        let mut given = Vec::new();
        let n = list.reclaim(c, |s| given.push(s.as_ptr() as usize));
        given.sort();
        let mut want: Vec<usize> = [0usize, 2, 3, 5].iter().map(|&s| pages[s].as_ptr() as usize).collect();
        want.sort();
        assert_eq!((n, given), (4, want), "class {}", class_size(c));
        // What is left: the two slabs in use, less their kept slots.
        assert_eq!(list.len(), 2 * per - 2);
        let mut left = HashSet::new();
        while let Some(p) = list.pop() {
            assert!(keep.contains(&(p.as_ptr() as usize & !(SLAB_SIZE - 1))));
            assert!(left.insert(p.as_ptr() as usize));
        }
        assert_eq!(left.len(), 2 * per - 2);
        for p in pages {
            unsafe { dealloc(p.as_ptr(), Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap()) };
        }
    }
}

/// Many slabs, every slot free in a scrambled order: all go back, and an
/// empty list or one without a whole slab gives nothing.
#[test]
fn reclaim_sorts_long_lists() {
    let pages: Vec<NonNull<u8>> = (0..64).map(|_| slab_page()).collect();
    let mut list = FreeList::new();
    assert_eq!(list.reclaim(0, |_| panic!("nothing to give")), 0);
    for &p in &pages {
        unsafe { list.add_slab(p, 0) };
    }
    let mut taken: Vec<NonNull<u8>> = std::iter::from_fn(|| list.pop()).collect();
    let mut i = 0usize;
    let mut last = None;
    while !taken.is_empty() {
        i = (i * 7919 + 13) % taken.len();
        let p = taken.swap_remove(i);
        if last.is_none() {
            // One slot stays in use for the first round.
            last = Some(p);
            continue;
        }
        unsafe { list.push(p) };
    }
    assert_eq!(list.reclaim(0, |_| {}), 63);
    unsafe { list.push(last.unwrap()) };
    assert_eq!(list.reclaim(0, |_| {}), 1);
    assert_eq!(list.len(), 0);
    for p in pages {
        unsafe { dealloc(p.as_ptr(), Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap()) };
    }
}
