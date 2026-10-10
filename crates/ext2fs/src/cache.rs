//! The metadata block cache: inode tables, bitmaps, group descriptors,
//! directories, symlink and indirect blocks. Changes stay in the cache
//! (dirty) until the operation that made them commits, which writes them
//! together and flushes (after what they point to, see the crate's
//! "Ordering"); a block stays dirty until it was written, so
//! a failed commit is retried by the next one. Least recently used clean
//! blocks give way when the cache is full; dirty ones stay (the journal's
//! rule: a transaction's blocks reach their places only after its commit).
//!
//! File data does not pass through here: the kernel's page cache holds it,
//! and reads and writes of whole blocks go straight to the device (or, on
//! diskfs's ring path, between the device and the client's pages).

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

    /// Whether block `n` is cached.
    pub fn contains(&self, n: u32) -> bool {
        self.blocks.contains_key(&n)
    }

    /// Whether block `n` is cached and changed since it was last written.
    pub fn is_dirty(&self, n: u32) -> bool {
        self.blocks.get(&n).is_some_and(|e| e.dirty)
    }

    /// Whether any block is dirty.
    pub fn has_dirty(&self) -> bool {
        self.blocks.values().any(|e| e.dirty)
    }

    /// Cached blocks in `range`, without marking them used.
    pub fn range(&self, range: core::ops::Range<u32>) -> impl Iterator<Item = (u32, &[u8])> {
        self.blocks.range(range).map(|(&n, e)| (n, &e.data[..]))
    }

    /// The block to evict before another one can be cached, if the cache
    /// is full: the least recently used clean one. Dirty blocks are the running
    /// transaction's and stay until it commits (the journal's rule: nothing of it reaches
    /// its place before its commit); with only dirty blocks the cache grows. (The second
    /// value, contents to write first, is never given any more.)
    pub fn victim(&self, incoming: u32) -> Option<(u32, Option<&[u8]>)> {
        if self.blocks.len() < self.capacity || self.blocks.contains_key(&incoming) {
            return None;
        }
        let n = self.lru.values().copied().find(|n| !self.blocks[n].dirty)?;
        Some((n, None))
    }

    /// Caches block `n` (replacing what was there). The caller made room
    /// (see `victim`).
    pub fn insert(&mut self, n: u32, data: Vec<u8>, dirty: bool) {
        let dirty = dirty || self.blocks.get(&n).is_some_and(|e| e.dirty);
        self.remove(n);
        self.clock += 1;
        self.lru.insert(self.clock, n);
        self.blocks.insert(n, Entry { data, dirty, used: self.clock });
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

    /// The dirty blocks in ascending order (they stay dirty until
    /// `mark_clean`).
    pub fn dirty(&self) -> Vec<(u32, Vec<u8>)> {
        self.blocks.iter().filter(|(_, e)| e.dirty).map(|(&n, e)| (n, e.data.clone())).collect()
    }

    /// Block `n` reached the device.
    pub fn mark_clean(&mut self, n: u32) {
        if let Some(e) = self.blocks.get_mut(&n) {
            e.dirty = false;
        }
    }
}
