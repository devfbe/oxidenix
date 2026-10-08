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
