//! Slabs of the size classes (`slab::class`): a slab is one page of the
//! arena cut into slots of its class's size. Each slab has a descriptor
//! outside the arena (`Desc`, by page), with its own list of free slots
//! (linked through the free slots themselves, as indices) and a bump index
//! for slots never used, so a new slab is not written whole. A class keeps
//! the slabs that have free slots on a list (`partial`), forgets the full
//! ones, and keeps at most `EMPTY_KEEP` empty ones (`empty`): a slab whose
//! last object goes beyond those goes back to the page allocator at once.
//! Allocating and freeing a slot are O(1), under the class's lock.

use crate::PAGE;

/// Empty slabs a class keeps (so that one object allocated and freed in
/// turn does not move a page back and forth).
pub(crate) const EMPTY_KEEP: u32 = 1;

const NONE: u32 = u32::MAX;
const FULL: u8 = 0;
const PARTIAL: u8 = 1;
const EMPTY: u8 = 2;
const LOOSE: u8 = 3;

/// A slab's descriptor (16 bytes per page of the arena).
#[repr(C)]
struct Desc {
    next: u32,
    prev: u32,
    /// The first free slot + 1 (0: none); each free slot holds the next
    /// one's index + 1 in its first two bytes.
    free: u16,
    /// Slots below have been handed out at some time.
    bump: u16,
    used: u16,
    class: u8,
    list: u8,
}

/// Where the descriptors and the arena are.
#[derive(Clone, Copy)]
pub(crate) struct Place {
    pub descs: usize,
    pub arena: usize,
}

impl Place {
    fn desc(&self, p: u32) -> &mut Desc {
        unsafe { &mut *((self.descs + p as usize * core::mem::size_of::<Desc>()) as *mut Desc) }
    }

    fn page(&self, p: u32) -> usize {
        self.arena + p as usize * PAGE
    }
}

/// A class's slabs.
pub(crate) struct Class {
    partial: u32,
    empty: u32,
    empties: u32,
    /// Slabs it has (partial, full and empty), and slots in use.
    pub slabs: usize,
    pub used: usize,
}

impl Class {
    pub const fn new() -> Class {
        Class { partial: NONE, empty: NONE, empties: 0, slabs: 0, used: 0 }
    }

    fn push(head: &mut u32, at: Place, p: u32) {
        let d = at.desc(p);
        d.prev = NONE;
        d.next = *head;
        if *head != NONE {
            at.desc(*head).prev = p;
        }
        *head = p;
    }

    fn unlink(head: &mut u32, at: Place, p: u32) {
        let (next, prev) = {
            let d = at.desc(p);
            (d.next, d.prev)
        };
        if prev != NONE {
            at.desc(prev).next = next;
        } else {
            *head = next;
        }
        if next != NONE {
            at.desc(next).prev = prev;
        }
    }

    /// A free slot of class `c`, if a slab has one.
    pub fn take(&mut self, at: Place, c: usize) -> Option<usize> {
        let p = if self.partial != NONE {
            self.partial
        } else if self.empty != NONE {
            let p = self.empty;
            Self::unlink(&mut self.empty, at, p);
            self.empties -= 1;
            Self::push(&mut self.partial, at, p);
            at.desc(p).list = PARTIAL;
            p
        } else {
            return None;
        };
        let size = slab::class_size(c);
        let base = at.page(p);
        let d = at.desc(p);
        debug_assert_eq!(d.class as usize, c);
        let i = if d.free != 0 {
            let i = d.free - 1;
            d.free = unsafe { *((base + i as usize * size) as *const u16) };
            i
        } else {
            d.bump += 1;
            d.bump - 1
        };
        d.used += 1;
        self.used += 1;
        if d.used as usize == PAGE / size {
            d.list = FULL;
            Self::unlink(&mut self.partial, at, p);
        }
        Some(base + i as usize * size)
    }

    /// Adds the page at `page` as a new slab of class `c`.
    pub fn add(&mut self, at: Place, page: usize, c: usize) {
        let p = ((page - at.arena) / PAGE) as u32;
        let d = at.desc(p);
        *d = Desc { next: NONE, prev: NONE, free: 0, bump: 0, used: 0, class: c as u8, list: PARTIAL };
        Self::push(&mut self.partial, at, p);
        self.slabs += 1;
    }

    /// Returns the slot at `ptr` (class `c`); the page of a slab that is
    /// empty now and not kept, for the page allocator.
    ///
    /// # Safety
    /// `ptr` came from `take` of this class and is not used any more.
    pub unsafe fn put(&mut self, at: Place, ptr: usize, c: usize) -> Option<usize> {
        let size = slab::class_size(c);
        let p = ((ptr - at.arena) / PAGE) as u32;
        let base = at.page(p);
        let d = at.desc(p);
        debug_assert_eq!(d.class as usize, c);
        debug_assert!(d.used > 0 && (ptr - base) % size == 0);
        unsafe { *(ptr as *mut u16) = d.free };
        d.free = ((ptr - base) / size) as u16 + 1;
        let was_full = d.list == FULL;
        d.used -= 1;
        self.used -= 1;
        if d.used == 0 {
            if !was_full {
                Self::unlink(&mut self.partial, at, p);
            }
            if self.empties < EMPTY_KEEP {
                Self::push(&mut self.empty, at, p);
                at.desc(p).list = EMPTY;
                self.empties += 1;
                return None;
            }
            at.desc(p).list = LOOSE;
            self.slabs -= 1;
            return Some(base);
        }
        if was_full {
            d.list = PARTIAL;
            Self::push(&mut self.partial, at, p);
        }
        None
    }

    /// Takes the empty slabs off (their pages into `out`); how many.
    pub fn drain(&mut self, at: Place, out: &mut [usize]) -> usize {
        let mut n = 0;
        while self.empty != NONE && n < out.len() {
            let p = self.empty;
            Self::unlink(&mut self.empty, at, p);
            at.desc(p).list = LOOSE;
            self.empties -= 1;
            self.slabs -= 1;
            out[n] = at.page(p);
            n += 1;
        }
        n
    }
}
