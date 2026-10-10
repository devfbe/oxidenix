//! The page allocator under the heap: runs of pages of the arena, each page
//! allocated or free and committed or not, in bitmaps outside the arena,
//! and a radix tree of summaries over them that finds the lowest run of
//! free pages of a size in a few steps (Go's page allocator: each summary
//! says how long the free runs at the start and the end of its range are,
//! and the longest within).
//!
//! The arena is cut into chunks of `CHUNK_PAGES` pages; each chunk has its
//! bitmaps and summary (`ChunkMeta`), and every `FANOUT` entries of a level
//! one summary on the level above, up to a root that covers the largest
//! arena. Lowest first fit keeps what is in use low in the arena, so free
//! memory gathers at the top, where trimming gives it back first. All
//! metadata lives in its own area, committed as the arena grows and never
//! decommitted: nothing about free pages is kept in them.

use crate::{CHUNK_PAGES, PAGE};

/// Summaries per summary of the level above.
pub(crate) const FANOUT: usize = 16;
/// Most summary levels above the chunks (16^6 chunks: far beyond any arena).
const MAX_UPPER: usize = 6;
const WORDS: usize = CHUNK_PAGES / 64;
/// A run that needs committing commits up to this many pages (the free,
/// uncommitted pages that follow it, reserved with it), so that a growing
/// heap does not make a kernel call per page.
pub(crate) const COMMIT_AHEAD: usize = 16;
/// Free committed pages that make a trim worth it: more than this and more
/// than a quarter of what is committed.
pub(crate) const TRIM_FLOOR: usize = 512;
/// Free committed pages a trim keeps: this many, or an eighth of what is
/// allocated if more (a heap that is busy gets memory back without a
/// kernel call).
pub(crate) const KEEP_FLOOR: usize = 256;

/// Free runs of a range of pages: at its start, the longest, at its end.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Sum {
    pub start: u32,
    pub max: u32,
    pub end: u32,
}

/// A chunk's pages: allocated, committed (a bit each), and its summary.
#[repr(C)]
pub(crate) struct ChunkMeta {
    alloc: [u64; WORDS],
    committed: [u64; WORDS],
    sum: Sum,
}

/// A run handed out (`reserve`): `pages` from `page` for the caller, then
/// `extra` pages reserved along to be committed with them; `commit` if any
/// of them is not committed (the caller commits all, then `finish`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run {
    pub page: usize,
    pub pages: usize,
    pub extra: usize,
    pub commit: bool,
}

/// What a growth commits: the metadata ranges (address, length; empty ones
/// have length 0) and the chunks there are afterwards.
pub(crate) struct Plan {
    pub chunks: usize,
    pub ranges: [(usize, usize); 3],
}

pub(crate) struct Pages {
    arena: usize,
    max_chunks: usize,
    /// Summary levels above the chunks.
    upper: usize,
    /// The metadata: page descriptors (for the slabs), chunk metadata, and
    /// the upper summaries, level by level from the root (`level_at`).
    pub descs: usize,
    metas: usize,
    sums: usize,
    level_at: [usize; MAX_UPPER],
    /// How far each metadata array is committed (an address).
    ends: [usize; 3],
    /// Chunks of the arena in use (their metadata committed).
    pub chunks: usize,
    pub committed: usize,
    pub allocated: usize,
    pub free_committed: usize,
    /// A trim was asked for (`ask_trim`) and has not begun.
    trim_asked: bool,
    /// Where a trim goes on (`take_for_trim`): the chunks above it have no
    /// free committed page left for it (since `trim_begins`).
    trim_cursor: usize,
}

const fn page_up(x: usize) -> usize {
    (x + PAGE - 1) & !(PAGE - 1)
}

const fn div_up(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

/// Bytes of metadata for an arena of `max_chunks` chunks (it starts at a
/// page boundary).
pub(crate) const fn meta_len(max_chunks: usize) -> usize {
    let (upper, at) = upper_levels(max_chunks);
    let _ = at;
    let mut entries = 0;
    let mut l = 0;
    while l < upper {
        entries += div_up(max_chunks, pow(upper - l));
        l += 1;
    }
    page_up(max_chunks * CHUNK_PAGES * 16) + page_up(max_chunks * core::mem::size_of::<ChunkMeta>()) + page_up(entries * core::mem::size_of::<Sum>())
}

const fn pow(e: usize) -> usize {
    let mut x = 1;
    let mut i = 0;
    while i < e {
        x *= FANOUT;
        i += 1;
    }
    x
}

/// The levels above the chunks (the root's entries cover the whole arena)
/// and where each level's entries start in the summary array.
const fn upper_levels(max_chunks: usize) -> (usize, [usize; MAX_UPPER]) {
    let mut upper = 1;
    while pow(upper) < max_chunks {
        upper += 1;
    }
    let mut at = [0; MAX_UPPER];
    let mut l = 1;
    while l < upper {
        at[l] = at[l - 1] + div_up(max_chunks, pow(upper - (l - 1)));
        l += 1;
    }
    (upper, at)
}

/// Bits [from, to) of a word.
fn mask(from: usize, to: usize) -> u64 {
    let hi = if to >= 64 { !0 } else { (1u64 << to) - 1 };
    hi & !((1u64 << from) - 1)
}

/// The longest run of zero bits in `w`.
fn longest_zeros(w: u64) -> u32 {
    let mut x = !w;
    let mut n = 0;
    while x != 0 {
        x &= x << 1;
        n += 1;
    }
    n
}

fn summarize(bits: &[u64; WORDS]) -> Sum {
    let mut start = 0;
    for &w in bits {
        start += w.trailing_zeros().min(64);
        if w != 0 {
            break;
        }
    }
    let mut end = 0;
    for &w in bits.iter().rev() {
        end += w.leading_zeros().min(64);
        if w != 0 {
            break;
        }
    }
    let (mut max, mut run) = (0, 0);
    for &w in bits {
        if w == 0 {
            run += 64;
            continue;
        }
        max = max.max(run + w.trailing_zeros()).max(longest_zeros(w));
        run = w.leading_zeros();
    }
    Sum { start, max: max.max(run), end }
}

/// The summary of entries of `size` pages each, in order.
fn merge(kids: &[Sum], size: usize) -> Sum {
    let full = size as u32;
    let mut start = 0;
    for s in kids {
        start += s.start;
        if s.start != full {
            break;
        }
    }
    let mut end = 0;
    for s in kids.iter().rev() {
        end += s.end;
        if s.start != full {
            break;
        }
    }
    let (mut max, mut run) = (0, 0);
    for s in kids {
        if s.start == full {
            run += full;
        } else {
            max = max.max(run + s.start).max(s.max);
            run = s.end;
        }
    }
    Sum { start, max: max.max(run), end }
}

impl Pages {
    /// The allocator of an arena at `arena` of up to `max_chunks` chunks,
    /// with its metadata at `meta` (`meta_len` bytes, nothing committed).
    pub const fn new(meta: usize, arena: usize, max_chunks: usize) -> Pages {
        let (upper, level_at) = upper_levels(max_chunks);
        let descs = meta;
        let metas = descs + page_up(max_chunks * CHUNK_PAGES * 16);
        let sums = metas + page_up(max_chunks * core::mem::size_of::<ChunkMeta>());
        Pages {
            arena,
            max_chunks,
            upper,
            descs,
            metas,
            sums,
            level_at,
            ends: [descs, metas, sums],
            chunks: 0,
            committed: 0,
            allocated: 0,
            free_committed: 0,
            trim_asked: false,
            trim_cursor: 0,
        }
    }

    pub fn page_of(&self, addr: usize) -> usize {
        (addr - self.arena) / PAGE
    }

    /// Metadata committed, in bytes.
    pub fn meta_bytes(&self) -> usize {
        (self.ends[0] - self.descs) + (self.ends[1] - self.metas) + (self.ends[2] - self.sums)
    }

    fn meta(&self, c: usize) -> &mut ChunkMeta {
        debug_assert!(c < self.chunks);
        unsafe { &mut *((self.metas + c * core::mem::size_of::<ChunkMeta>()) as *mut ChunkMeta) }
    }

    fn upper_sum(&self, level: usize, i: usize) -> &mut Sum {
        unsafe { &mut *((self.sums + (self.level_at[level] + i) * core::mem::size_of::<Sum>()) as *mut Sum) }
    }

    /// Entries of `level` (0 the root, `upper` the chunks) for `chunks` chunks.
    fn entries(&self, level: usize, chunks: usize) -> usize {
        div_up(chunks, pow(self.upper - level))
    }

    /// Pages one entry of `level` covers.
    fn entry_pages(&self, level: usize) -> usize {
        CHUNK_PAGES * pow(self.upper - level)
    }

    fn sum(&self, level: usize, i: usize) -> Sum {
        if level == self.upper {
            self.meta(i).sum
        } else {
            *self.upper_sum(level, i)
        }
    }

    /// The summaries above chunk `c` made again (its own changed).
    fn propagate(&mut self, c: usize) {
        let mut i = c;
        for level in (0..self.upper).rev() {
            let parent = i / FANOUT;
            let first = parent * FANOUT;
            let last = (first + FANOUT).min(self.entries(level + 1, self.chunks));
            let mut kids = [Sum::default(); FANOUT];
            for (k, j) in (first..last).enumerate() {
                kids[k] = self.sum(level + 1, j);
            }
            *self.upper_sum(level, parent) = merge(&kids[..last - first], self.entry_pages(level + 1));
            i = parent;
        }
    }

    /// Sets the bits of pages [page, page + n): allocated and committed as
    /// given (None: unchanged), keeping the counts and summaries.
    fn set(&mut self, page: usize, n: usize, alloc: Option<bool>, committed: Option<bool>) {
        let end = page + n;
        let mut p = page;
        while p < end {
            let c = p / CHUNK_PAGES;
            let chunk_end = ((c + 1) * CHUNK_PAGES).min(end);
            let meta = self.meta(c);
            let (mut da, mut dc, mut dfc) = (0isize, 0isize, 0isize);
            while p < chunk_end {
                let w = (p % CHUNK_PAGES) / 64;
                let from = p % 64;
                let to = (from + (chunk_end - p)).min(64);
                let m = mask(from, to);
                let (oa, oc) = (meta.alloc[w], meta.committed[w]);
                let na = match alloc {
                    Some(true) => oa | m,
                    Some(false) => oa & !m,
                    None => oa,
                };
                let nc = match committed {
                    Some(true) => oc | m,
                    Some(false) => oc & !m,
                    None => oc,
                };
                meta.alloc[w] = na;
                meta.committed[w] = nc;
                da += na.count_ones() as isize - oa.count_ones() as isize;
                dc += nc.count_ones() as isize - oc.count_ones() as isize;
                dfc += (nc & !na).count_ones() as isize - (oc & !oa).count_ones() as isize;
                p += to - from;
            }
            self.allocated = self.allocated.wrapping_add_signed(da);
            self.committed = self.committed.wrapping_add_signed(dc);
            self.free_committed = self.free_committed.wrapping_add_signed(dfc);
            if alloc.is_some() {
                let meta = self.meta(c);
                meta.sum = summarize(&meta.alloc);
                self.propagate(c);
            }
        }
    }

    fn bit(&self, page: usize) -> (bool, bool) {
        let meta = self.meta(page / CHUNK_PAGES);
        let (w, b) = ((page % CHUNK_PAGES) / 64, page % 64);
        (meta.alloc[w] >> b & 1 == 1, meta.committed[w] >> b & 1 == 1)
    }

    /// The lowest run of `n` free pages, if the arena has one.
    pub fn find(&self, n: usize) -> Option<usize> {
        if n == 0 || self.chunks == 0 {
            return None;
        }
        let (mut lo, mut hi) = (0, self.entries(0, self.chunks));
        for level in 0..=self.upper {
            let size = self.entry_pages(level);
            let (mut run, mut base) = (0, 0);
            let mut into = None;
            for i in lo..hi {
                let s = self.sum(level, i);
                if run == 0 {
                    base = i * size;
                }
                if run + s.start as usize >= n {
                    return Some(base);
                }
                if s.max as usize >= n {
                    into = Some(i);
                    break;
                }
                if s.start as usize == size {
                    run += size;
                } else {
                    run = s.end as usize;
                    base = (i + 1) * size - run;
                }
            }
            let i = into?;
            if level == self.upper {
                return self.find_in_chunk(i, n);
            }
            lo = i * FANOUT;
            hi = (lo + FANOUT).min(self.entries(level + 1, self.chunks));
        }
        None
    }

    fn find_in_chunk(&self, c: usize, n: usize) -> Option<usize> {
        let meta = self.meta(c);
        let (mut run, mut start) = (0, 0);
        for (w, &word) in meta.alloc.iter().enumerate() {
            if word == 0 {
                if run == 0 {
                    start = w * 64;
                }
                run += 64;
                if run >= n {
                    return Some(c * CHUNK_PAGES + start);
                }
                continue;
            }
            for b in 0..64 {
                if word >> b & 1 == 1 {
                    run = 0;
                    continue;
                }
                if run == 0 {
                    start = w * 64 + b;
                }
                run += 1;
                if run >= n {
                    return Some(c * CHUNK_PAGES + start);
                }
            }
        }
        debug_assert!(false, "the summary promised a run");
        None
    }

    /// Reserves the lowest run of `n` free pages (None: the arena must grow).
    pub fn reserve(&mut self, n: usize) -> Option<Run> {
        let page = self.find(n)?;
        let commit = (page..page + n).any(|p| !self.bit(p).1);
        let mut extra = 0;
        if commit {
            let top = self.chunks * CHUNK_PAGES;
            while n + extra < COMMIT_AHEAD && page + n + extra < top && self.bit(page + n + extra) == (false, false) {
                extra += 1;
            }
        }
        self.set(page, n + extra, Some(true), None);
        Some(Run { page, pages: n, extra, commit })
    }

    /// The commit of a reserved run is done: with `ok` its pages are
    /// committed and the extra ones free; otherwise the caller decommitted
    /// them all and the run is free again.
    pub fn finish(&mut self, run: &Run, ok: bool) {
        let total = run.pages + run.extra;
        if ok {
            self.set(run.page, total, None, Some(true));
            self.set(run.page + run.pages, run.extra, Some(false), None);
        } else {
            self.set(run.page, total, Some(false), Some(false));
        }
    }

    /// Frees `n` pages from `page`.
    pub fn free(&mut self, page: usize, n: usize) {
        debug_assert!((page..page + n).all(|p| self.bit(p).0), "freeing a free page");
        self.set(page, n, Some(false), None);
    }

    /// Whether free committed memory calls for a trim (true once until the
    /// trim begins).
    pub fn ask_trim(&mut self) -> bool {
        if self.trim_asked || self.free_committed <= TRIM_FLOOR.max(self.committed / 4) {
            return false;
        }
        self.trim_asked = true;
        true
    }

    /// Free committed pages a trim leaves.
    pub fn keep(&self) -> usize {
        KEEP_FLOOR.max(self.allocated / 8)
    }

    pub fn trim_begins(&mut self) {
        self.trim_asked = false;
        self.trim_cursor = self.chunks;
    }

    /// Reserves free committed pages beyond `keep` for a trim, highest
    /// first, as runs (page, pages) into `out`; how many runs.
    pub fn take_for_trim(&mut self, keep: usize, out: &mut [(usize, usize)]) -> usize {
        let mut want = self.free_committed.saturating_sub(keep);
        let mut n = 0;
        let mut c = self.trim_cursor.min(self.chunks);
        while c > 0 && want > 0 && n < out.len() {
            c -= 1;
            let meta = self.meta(c);
            if (0..WORDS).all(|w| meta.committed[w] & !meta.alloc[w] == 0) {
                continue;
            }
            // Runs from the chunk's top down.
            let mut p = (c + 1) * CHUNK_PAGES;
            while p > c * CHUNK_PAGES && want > 0 && n < out.len() {
                if self.bit(p - 1) != (false, true) {
                    p -= 1;
                    continue;
                }
                let end = p;
                while p > c * CHUNK_PAGES && end - p < want && self.bit(p - 1) == (false, true) {
                    p -= 1;
                }
                out[n] = (p, end - p);
                n += 1;
                want -= end - p;
            }
        }
        // On from the chunk it stopped in (it may have more).
        self.trim_cursor = if n == out.len() || want == 0 { (c + 1).min(self.chunks) } else { c };
        for &(page, len) in &out[..n] {
            self.set(page, len, Some(true), None);
        }
        n
    }

    /// The runs `take_for_trim` gave were decommitted: free and uncommitted.
    pub fn trimmed(&mut self, runs: &[(usize, usize)]) {
        for &(page, len) in runs {
            self.set(page, len, Some(false), Some(false));
        }
    }

    /// Whether a run of `n` pages fits now.
    pub fn fits(&self, n: usize) -> bool {
        self.find(n).is_some()
    }

    /// The growth that makes room for a run of `n` pages, or None if the
    /// arena cannot grow so far.
    pub fn plan(&self, n: usize) -> Option<Plan> {
        let chunks = self.chunks + div_up(n, CHUNK_PAGES).max(1);
        if chunks > self.max_chunks {
            return None;
        }
        let descs_end = page_up(self.descs + chunks * CHUNK_PAGES * 16);
        let metas_end = page_up(self.metas + chunks * core::mem::size_of::<ChunkMeta>());
        let mut sums_end = self.sums;
        for level in 0..self.upper {
            sums_end = sums_end.max(self.sums + (self.level_at[level] + self.entries(level, chunks)) * core::mem::size_of::<Sum>());
        }
        let sums_end = page_up(sums_end);
        let range = |from: usize, to: usize| (from, to.saturating_sub(from));
        Some(Plan { chunks, ranges: [range(self.ends[0], descs_end), range(self.ends[1], metas_end), range(self.ends[2], sums_end)] })
    }

    /// Metadata range `i` of a plan is committed (up to `end`).
    pub fn meta_committed(&mut self, i: usize, end: usize) {
        self.ends[i] = self.ends[i].max(end);
    }

    /// Adds the plan's chunks (its metadata committed): free, uncommitted.
    pub fn extend(&mut self, plan: &Plan) {
        let old = self.chunks;
        self.chunks = plan.chunks;
        for c in old..plan.chunks {
            let meta = self.meta(c);
            meta.alloc = [0; WORDS];
            meta.committed = [0; WORDS];
            meta.sum = summarize(&meta.alloc);
        }
        // The summaries above them (upper entries beyond the old chunks'
        // were never written: each is made from its children here).
        for c in old..plan.chunks {
            self.propagate(c);
        }
    }

    /// Checks every count and summary against the bitmaps (tests).
    pub fn verify(&self) {
        let (mut a, mut c, mut fc) = (0, 0, 0);
        for k in 0..self.chunks {
            let meta = self.meta(k);
            for w in 0..WORDS {
                a += meta.alloc[w].count_ones() as usize;
                c += meta.committed[w].count_ones() as usize;
                fc += (meta.committed[w] & !meta.alloc[w]).count_ones() as usize;
            }
            assert_eq!(meta.sum, summarize(&meta.alloc), "chunk {k}'s summary");
        }
        assert_eq!((a, c, fc), (self.allocated, self.committed, self.free_committed), "counts");
        for level in (0..self.upper).rev() {
            for parent in 0..self.entries(level, self.chunks) {
                let first = parent * FANOUT;
                let last = (first + FANOUT).min(self.entries(level + 1, self.chunks));
                let kids: [Sum; FANOUT] = core::array::from_fn(|k| if first + k < last { self.sum(level + 1, first + k) } else { Sum::default() });
                assert_eq!(self.sum(level, parent), merge(&kids[..last - first], self.entry_pages(level + 1)), "level {level} entry {parent}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_of_words() {
        let mut bits = [0u64; WORDS];
        assert_eq!(summarize(&bits), Sum { start: 512, max: 512, end: 512 });
        bits[0] = 1 << 3;
        bits[7] = 1 << 60;
        assert_eq!(summarize(&bits), Sum { start: 3, max: 64 - 4 + 6 * 64 + 60, end: 3 });
        assert_eq!(longest_zeros(0b1000_0001), 56);
        let s = merge(&[Sum { start: 512, max: 512, end: 512 }, Sum { start: 10, max: 100, end: 0 }], 512);
        assert_eq!(s, Sum { start: 522, max: 522, end: 0 });
    }

    #[test]
    fn levels_cover_the_arena() {
        let (upper, at) = upper_levels(32768);
        assert_eq!(upper, 4);
        assert_eq!(&at[..4], &[0, 1, 9, 137]);
        assert_eq!(upper_levels(1).0, 1);
    }
}
