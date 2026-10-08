//! A set of block numbers kept as ranges, for sets that follow a file's
//! blocks: freeing a 2 GiB file frees two million mostly contiguous blocks,
//! which as single entries would take more memory than diskfs has, and as
//! ranges take a few.

use alloc::collections::BTreeMap;

/// Block numbers as disjoint, non-adjacent ranges start -> end (exclusive).
#[derive(Default)]
pub struct BlockSet {
    ranges: BTreeMap<u32, u32>,
}

impl BlockSet {
    pub const fn new() -> BlockSet {
        BlockSet { ranges: BTreeMap::new() }
    }

    pub fn contains(&self, block: u32) -> bool {
        self.ranges.range(..=block).next_back().is_some_and(|(_, &end)| block < end)
    }

    /// Adds `block`, joining the ranges it touches.
    pub fn insert(&mut self, block: u32) {
        if self.contains(block) {
            return;
        }
        let mut start = block;
        let mut end = block + 1;
        if let Some((&s, &e)) = self.ranges.range(..block).next_back() {
            if e == block {
                start = s;
            }
        }
        if let Some(e) = self.ranges.remove(&end) {
            end = e;
        }
        self.ranges.insert(start, end);
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn clear(&mut self) {
        self.ranges.clear();
    }

    /// How many ranges hold the set (its memory; for the tests).
    #[cfg(test)]
    pub fn ranges(&self) -> usize {
        self.ranges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::BlockSet;

    #[test]
    fn contiguous_blocks_make_one_range() {
        let mut s = BlockSet::new();
        // Every other block (backwards), then the gaps.
        for b in (1000..3000u32).filter(|b| b % 2 == 0).rev().chain((1000..3000).filter(|b| b % 2 == 1)) {
            s.insert(b);
        }
        assert_eq!(s.ranges(), 1);
        assert!(s.contains(1000) && s.contains(2999) && !s.contains(999) && !s.contains(3000));
    }

    #[test]
    fn gaps_stay_and_fill() {
        let mut s = BlockSet::new();
        for b in [5, 7, 9, 5] {
            s.insert(b);
        }
        assert_eq!(s.ranges(), 3);
        assert!(!s.contains(6) && !s.contains(8));
        s.insert(6);
        s.insert(8);
        assert_eq!(s.ranges(), 1);
        assert!((5..10).all(|b| s.contains(b)) && !s.contains(4) && !s.contains(10));
        s.insert(u32::MAX - 1);
        assert!(s.contains(u32::MAX - 1) && !s.contains(u32::MAX));
        s.clear();
        assert!(s.is_empty() && !s.contains(5));
    }
}
