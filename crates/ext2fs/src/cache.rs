//! The metadata block cache: inode tables, bitmaps, group descriptors,
//! directories, symlink and indirect blocks. Changes stay in the cache
//! (dirty) until the operation that made them commits, which writes them
//! together and flushes once. Least recently used blocks give way when the
//! cache is full; a dirty one is written first.
//!
//! File data does not pass through here: the kernel's page cache holds it,
//! and reads and writes of whole blocks go straight to the device.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

struct Entry {
    data: Vec<u8>,
    dirty: bool,
    /// When it was last used (the key in `lru`).
    used: u64,
}

pub struct BlockCache {
    blocks: BTreeMap<u32, Entry>,
    /// Blocks by last use, oldest first.
    lru: BTreeMap<u64, u32>,
    clock: u64,
    capacity: usize,
}

impl BlockCache {
    pub fn new(capacity: usize) -> BlockCache {
        BlockCache { blocks: BTreeMap::new(), lru: BTreeMap::new(), clock: 0, capacity: capacity.max(1) }
    }

    /// Block `n`, marked as just used.
    pub fn get(&mut self, n: u32) -> Option<&[u8]> {
        let e = self.blocks.get_mut(&n)?;
        self.lru.remove(&e.used);
        self.clock += 1;
        e.used = self.clock;
        self.lru.insert(self.clock, n);
        Some(&e.data)
    }

    /// Cached blocks in `range`, without marking them used.
    pub fn range(&self, range: core::ops::Range<u32>) -> impl Iterator<Item = (u32, &[u8])> {
        self.blocks.range(range).map(|(&n, e)| (n, &e.data[..]))
    }

    /// Caches block `n` (replacing what was there). Returns a dirty block
    /// that had to go to make room, for the caller to write.
    pub fn insert(&mut self, n: u32, data: Vec<u8>, dirty: bool) -> Option<(u32, Vec<u8>)> {
        let dirty = dirty || self.blocks.get(&n).is_some_and(|e| e.dirty);
        self.remove(n);
        let evicted = if self.blocks.len() >= self.capacity { self.evict() } else { None };
        self.clock += 1;
        self.lru.insert(self.clock, n);
        self.blocks.insert(n, Entry { data, dirty, used: self.clock });
        evicted
    }

    fn evict(&mut self) -> Option<(u32, Vec<u8>)> {
        let (_, n) = self.lru.pop_first()?;
        let e = self.blocks.remove(&n)?;
        e.dirty.then_some((n, e.data))
    }

    /// Forgets block `n` (freed, or overwritten on the device), dirty or not.
    pub fn remove(&mut self, n: u32) {
        if let Some(e) = self.blocks.remove(&n) {
            self.lru.remove(&e.used);
        }
    }

    /// Forgets the blocks in `range`.
    pub fn remove_range(&mut self, range: core::ops::Range<u32>) {
        let found: Vec<u32> = self.blocks.range(range).map(|(&n, _)| n).collect();
        for n in found {
            self.remove(n);
        }
    }

    /// The dirty blocks in ascending order; they count as clean from now on.
    pub fn take_dirty(&mut self) -> Vec<(u32, Vec<u8>)> {
        self.blocks
            .iter_mut()
            .filter(|(_, e)| e.dirty)
            .map(|(&n, e)| {
                e.dirty = false;
                (n, e.data.clone())
            })
            .collect()
    }
}
