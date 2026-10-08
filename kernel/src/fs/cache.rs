//! The page cache: the pages of a regular file in physical frames, shared
//! by `read`, `write` and every mapping of the file (see
//! docs/design/page-cache.md).
//!
//! A cache holds one reference on each of its frames; a page table entry
//! that maps a frame holds another, so a page dropped from the cache stays
//! valid for its mappings until they are removed.
//!
//! The memory store (tmpfs files, anonymous shared memory) has no other
//! copy of the data: its pages are committed memory and are never dropped
//! while the file has them. A page missing within the file is read from the
//! initramfs image (while that part of the file was never cut off) or is
//! zero; `read` takes those bytes without creating the page.
//!
//! The paged store belongs to a pager in user space (the Linux server): a
//! missing page is requested from it and the thread that needs the page
//! sleeps until the pager supplies it (`supply`), whether the thread runs a
//! program or the kernel (copying from a mapping). Supplied pages are
//! committed and stay until the object goes.
//!
//! The cached store is a file the Linux server caches (its page cache of
//! a disk file, `restricted::SYS_MO_CREATE_CACHED`): the file's size, its
//! pages and which of them are dirty are kept here, the data comes from
//! and goes to the disk through the server. A missing page is requested
//! from the pager (or, for the server's own reads and writes, reported
//! missing: `Fill::No`); the pager makes it *pending* (`pin_fill`: a zeroed
//! frame, pinned for a grant, which no reader sees), has the disk server
//! read into it by DMA and then declares it filled or failed (`filled`).
//! Stores (writes, shared mappings) mark pages dirty, the object's first
//! one tells the pager (`Pager::dirty`). A shared mapping maps a page
//! read-only until the first store, which marks it dirty (`set_dirty`).
//! Write-back takes runs of dirty pages (`pin_dirty`: their marks cleared,
//! write-protected in every mapping, pinned for a read-only grant): a
//! store in between faults, marks the page dirty again and is written next
//! time, so none is lost; `redirty` puts back what a failed write took.
//! Its pages are counted as cached memory (`memory::cache_charge`), which
//! commits and new cache pages reclaim: clean pages that nothing pins or
//! maps, a page used since the last look getting a second chance. Dirty
//! ones the reclaim cannot drop make it ask the pagers to write back
//! (`Pager::writeback`), and so do too many dirty pages (`balance_dirty`).
//!
//! Lock order: address space (sleeping) → `io` (sleeping) → `state` →
//! frames. `io` serializes what changes contents or size (`write`,
//! `truncate`); it is never held while an address space is locked. Cache
//! hits only take `state`.

use crate::memory;
use crate::memory::frame::UserFrames;
use crate::process::address_space::{Fault, Mm, PAGE};
use crate::process::errno::*;
use crate::sync::{IrqSpinLock, Mutex};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame};

/// Largest file size (as `off_t` allows).
pub const MAX_SIZE: u64 = i64::MAX as u64;
/// Pages reclaim looks at per hold of a cache's lock.
const RECLAIM_BATCH: usize = 256;

/// Dirty pages of all caches.
static DIRTY: AtomicU64 = AtomicU64::new(0);

/// Dirty pages (for /proc/meminfo).
pub fn dirty_pages() -> u64 {
    DIRTY.load(Ordering::Relaxed)
}

/// Pages of tmpfs file contents, and their limit: half of what may be
/// committed, as Linux's default tmpfs size.
static TMPFS_PAGES: AtomicU64 = AtomicU64::new(0);
static TMPFS_LIMIT: AtomicU64 = AtomicU64::new(0);

/// Caches with a cached store (reclaimable pages), for reclaim; the next
/// one to look at.
static CACHES: IrqSpinLock<Vec<Weak<PageCache>>> = IrqSpinLock::new(Vec::new());
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Sets the tmpfs limit from the commit limit (after memory::init) and
/// lets commits reclaim cached pages.
pub fn init() {
    TMPFS_LIMIT.store(memory::commit_stats().1 / 2, Ordering::Relaxed);
    memory::set_reclaim(reclaim);
}

/// (pages used, page limit) of tmpfs.
pub fn tmpfs_usage() -> (u64, u64) {
    (TMPFS_PAGES.load(Ordering::Relaxed), TMPFS_LIMIT.load(Ordering::Relaxed))
}

/// Supplies the pages of a paged memory object.
pub trait Pager: Send + Sync {
    /// Page `index` of the object with `key` is wanted. False if the pager
    /// is gone (the page will never come).
    fn request(&self, key: u64, index: u64) -> bool;
    /// Where threads waiting for its pages sleep: its answers (pages,
    /// failures) and its end wake them.
    fn wait_chan(&self) -> usize;
    /// Whether the pager can still answer (its thread lives).
    fn alive(&self) -> bool;
    /// The cached object `key` got its first dirty page.
    fn dirty(&self, key: u64);
    /// Too many pages are dirty (memory runs short of clean ones): the
    /// pager should write back about `pages` of them.
    fn writeback(&self, pages: u64);
}

/// Where pages come from.
enum Store {
    /// The cache is the only copy. `image` is the file's initramfs data;
    /// pages below `prepaid` were committed when the cache was created
    /// (anonymous shared memory) and are not charged again.
    Memory { image: &'static [u8], prepaid: u64 },
    /// A pager in user space, which knows the object by `key`.
    Paged { pager: Weak<dyn Pager>, key: u64 },
    /// A file the pager caches (see the module comment), known to it as
    /// `key`, which may grow to `limit` bytes.
    Cached { pager: Weak<dyn Pager>, key: u64, limit: u64 },
}

struct Page {
    frame: PhysFrame,
    /// Used since reclaim last looked at it.
    referenced: bool,
    /// Stored to since it was last written back (cached store).
    dirty: bool,
    /// Grants pinning it (`pin`): while any, the page stays this object's.
    pins: u32,
    /// Being filled (cached store, `pin_fill`): not yet the file's data,
    /// so nobody reads, maps or writes it until `filled`.
    pending: bool,
}

impl Page {
    fn new(frame: PhysFrame) -> Page {
        Page { frame, referenced: true, dirty: false, pins: 0, pending: false }
    }
}

/// Whether a read or write of a cached object fetches the missing pages it
/// meets (through the pager) or stops at the first one (the server, which
/// fills them itself).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Yes,
    No,
}

/// Most pages one `pin_fill` or `pin_dirty` takes (a run of a file's pages
/// for one read or write of the disk server: `fsring::MAX_TRANSFER`).
pub const MAX_RUN: u64 = 256;

/// The dirty pages of every cache, in pages of the commit limit: above
/// `1 / BACKGROUND` the pagers are asked to write back, above `1 / HARD` a
/// thread that stores waits for them (as Linux's dirty ratios).
const BACKGROUND: u64 = 10;
const HARD: u64 = 5;
/// The longest a storing thread waits for write-back at once.
const THROTTLE: u64 = 1_000_000_000;

/// Where threads throttled by `balance_dirty` wait.
fn dirty_chan() -> usize {
    &DIRTY as *const AtomicU64 as usize
}

/// `pages` dirty pages were written back or dropped: throttled writers may
/// go on.
fn undirty(pages: u64) {
    if pages == 0 {
        return;
    }
    let before = DIRTY.fetch_sub(pages, Ordering::Relaxed);
    let hard = memory::commit_stats().1 / HARD;
    if before > hard && before - pages <= hard {
        crate::process::sched::wakeup(dirty_chan());
    }
}

/// Threads waiting at one page of a paged or cached object (`State::waits`).
struct Waiters {
    index: u64,
    count: u32,
    /// Failed answers for the page (counted while anyone waits).
    failures: u32,
}

/// Most present pages one `pin_fill` or `pin_dirty` looks at (with
/// interrupts off): beyond, it says where to go on (`Scan::Resume`).
const MAX_SCAN: usize = 1024;

/// Why a run could not be pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scan {
    Errno(i64),
    /// The scan stopped (`MAX_SCAN`) before a run: ask again from this page.
    Resume(u64),
}

impl State {
    /// Counts a waiter of page `index`; the failures it has seen.
    fn enter_wait(&mut self, index: u64) -> Result<u32, i64> {
        if let Some(w) = self.waits.iter_mut().find(|w| w.index == index) {
            w.count += 1;
            return Ok(w.failures);
        }
        self.waits.try_reserve(1).map_err(|_| ENOMEM)?;
        self.waits.push(Waiters { index, count: 1, failures: 0 });
        Ok(0)
    }

    fn leave_wait(&mut self, index: u64) {
        if let Some(i) = self.waits.iter().position(|w| w.index == index) {
            self.waits[i].count -= 1;
            if self.waits[i].count == 0 {
                self.waits.swap_remove(i);
            }
        }
    }

    fn failures(&self, index: u64) -> u32 {
        self.waits.iter().find(|w| w.index == index).map_or(0, |w| w.failures)
    }

    /// The pager could not supply pages `first..end`: whoever waits fails.
    fn fail_waiters(&mut self, first: u64, end: u64) {
        for w in self.waits.iter_mut().filter(|w| (first..end).contains(&w.index)) {
            w.failures = w.failures.wrapping_add(1);
        }
    }
}

struct State {
    pages: BTreeMap<u64, Page>,
    size: u64,
    /// Bytes of `image` still valid as file contents (truncation cuts it).
    image_len: u64,
    /// Pages charged: to commit and tmpfs (memory store, beyond
    /// `prepaid`), to commit (paged store) or as cached memory (cached
    /// store).
    charged: u64,
    /// The page index reclaim continues at.
    cursor: u64,
    /// Pages of a paged or cached object threads wait for, with the
    /// failed answers since: a waiter fails if one came while it waited (a
    /// later access asks again). Bounded by the threads waiting: a pager's
    /// failure is recorded only where someone waits.
    waits: Vec<Waiters>,
    /// Dirty pages (cached store).
    dirty: u64,
}

pub struct PageCache {
    state: IrqSpinLock<State>,
    io: Mutex<()>,
    store: Store,
    /// Address spaces that map this file (see `register`).
    mappers: IrqSpinLock<Vec<Weak<Mm>>>,
    /// Set once by `hang_up`: futex waits on this object fail (EPIPE).
    hung_up: core::sync::atomic::AtomicBool,
}

/// A tmpfs page charged to commit and tmpfs, released on drop unless kept.
struct Charge(bool);

impl Charge {
    fn take() -> Result<Charge, i64> {
        let limit = TMPFS_LIMIT.load(Ordering::Relaxed);
        TMPFS_PAGES
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < limit).then_some(n + 1))
            .map_err(|_| ENOSPC)?;
        if !memory::commit(1) {
            TMPFS_PAGES.fetch_sub(1, Ordering::Relaxed);
            return Err(ENOSPC);
        }
        Ok(Charge(true))
    }

    fn keep(mut self) {
        self.0 = false;
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.0 {
            uncharge_tmpfs(1);
        }
    }
}

fn uncharge_tmpfs(pages: u64) {
    if pages > 0 {
        TMPFS_PAGES.fetch_sub(pages, Ordering::Relaxed);
        memory::uncommit(pages);
    }
}

fn frame_bytes(frame: PhysFrame) -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(memory::phys_to_virt(frame.start_address().as_u64()), PAGE as usize) }
}

fn page_of(off: u64) -> u64 {
    off / PAGE
}

fn new_frame() -> Option<PhysFrame> {
    memory::with_frames(|f| UserFrames(f).allocate_frame())
}

fn free_frames(frames: impl IntoIterator<Item = PhysFrame>) {
    memory::with_frames(|f| {
        for frame in frames {
            unsafe { f.deallocate_frame(frame) };
        }
    });
}

impl PageCache {
    fn new(store: Store, size: u64, image_len: u64) -> Result<Arc<PageCache>, i64> {
        Arc::try_new(PageCache {
            state: IrqSpinLock::new(State {
                pages: BTreeMap::new(),
                size,
                image_len,
                charged: 0,
                cursor: 0,
                waits: Vec::new(),
                dirty: 0,
            }),
            io: Mutex::new(()),
            store,
            mappers: IrqSpinLock::new(Vec::new()),
            hung_up: core::sync::atomic::AtomicBool::new(false),
        })
        .map_err(|_| ENOMEM)
    }

    /// A tmpfs file whose contents start as `image` (initramfs data, or
    /// empty).
    pub fn memory(image: &'static [u8]) -> Result<Arc<PageCache>, i64> {
        Self::new(Store::Memory { image, prepaid: 0 }, image.len() as u64, image.len() as u64)
    }

    /// Anonymous shared memory of `pages` zeroed pages, committed now.
    pub fn anonymous(pages: u64) -> Result<Arc<PageCache>, Fault> {
        if !memory::commit(pages) {
            return Err(Fault::Oom);
        }
        Self::new(Store::Memory { image: &[], prepaid: pages }, pages * PAGE, 0).map_err(|_| {
            memory::uncommit(pages);
            Fault::Oom
        })
    }

    /// Enters a cache whose pages reclaim may drop into its list.
    fn reclaimable(cache: &Arc<PageCache>) -> Result<(), i64> {
        let mut list = CACHES.lock();
        list.retain(|c| c.strong_count() > 0);
        list.try_reserve(1).map_err(|_| ENOMEM)?;
        list.push(Arc::downgrade(cache));
        Ok(())
    }

    /// A file of `size` bytes (at most `limit`) that `pager` caches and
    /// knows as `key` (see the module comment).
    pub fn cached(size: u64, limit: u64, pager: Weak<dyn Pager>, key: u64) -> Result<Arc<PageCache>, i64> {
        let limit = limit.min(MAX_SIZE);
        if size > limit {
            return Err(EFBIG);
        }
        let cache = Self::new(Store::Cached { pager, key, limit }, size, 0)?;
        Self::reclaimable(&cache)?;
        Ok(cache)
    }

    /// A cached object's key.
    pub fn cached_key(&self) -> Option<u64> {
        match self.store {
            Store::Cached { key, .. } => Some(key),
            _ => None,
        }
    }

    /// Whether this is a cached store's object.
    pub fn is_cached(&self) -> bool {
        matches!(self.store, Store::Cached { .. })
    }

    /// The pager of a paged or cached object, and its key.
    fn pager(&self) -> Option<(&Weak<dyn Pager>, u64)> {
        match &self.store {
            Store::Paged { pager, key } | Store::Cached { pager, key, .. } => Some((pager, *key)),
            Store::Memory { .. } => None,
        }
    }

    /// A memory object of `pages` pages whose contents `pager` supplies
    /// on demand; it knows the object as `key`.
    pub fn paged(pages: u64, pager: Weak<dyn Pager>, key: u64) -> Result<Arc<PageCache>, i64> {
        let size = pages.checked_mul(PAGE).filter(|&s| s <= MAX_SIZE).ok_or(EINVAL)?;
        Self::new(Store::Paged { pager, key }, size, 0)
    }

    /// The key of a paged object.
    pub fn paged_key(&self) -> Option<u64> {
        match self.store {
            Store::Paged { key, .. } => Some(key),
            _ => None,
        }
    }

    /// Waits until the pager supplied page `index` (EIO if the pager is
    /// gone, EINTR if the thread is dying: a pager that never answers must
    /// not leave it unkillable).
    fn wait_paged(&self, index: u64) -> Result<(), i64> {
        let Some((pager, key)) = self.pager() else { return Ok(()) };
        let seen = self.state.lock().enter_wait(index)?;
        let result = loop {
            let done = x86_64::instructions::interrupts::without_interrupts(|| {
                let Some(pager) = pager.upgrade() else { return Err(EIO) };
                // Registered before looking, so an answer cannot slip by.
                let wait = crate::process::sched::prepare_to_wait(pager.wait_chan());
                let pending = {
                    let st = self.state.lock();
                    if st.failures(index) != seen {
                        return Err(EIO);
                    }
                    match st.pages.get(&index).map(|p| p.pending) {
                        Some(false) => return Ok(true),
                        // Being filled: its answer comes without asking,
                        // unless the pager is gone meanwhile.
                        Some(true) if !pager.alive() => return Err(EIO),
                        Some(true) => true,
                        // Cut off meanwhile: the caller looks again.
                        None if index >= page_of(st.size.saturating_add(PAGE - 1)) => return Ok(true),
                        None => false,
                    }
                };
                if crate::process::signal::dying() {
                    return Err(EINTR);
                }
                if !pending && !pager.request(key, index) {
                    return Err(EIO);
                }
                drop(pager);
                wait.sleep();
                Ok(false)
            });
            match done {
                Ok(false) => {}
                Ok(true) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.state.lock().leave_wait(index);
        result
    }

    /// The pager's answer: page `index` with `data` (the rest zero), if it
    /// is still missing; wakes who waits for it. Whether it was inserted.
    pub fn supply(&self, index: u64, data: &[u8]) -> Result<bool, i64> {
        let Store::Paged { pager, .. } = &self.store else { return Err(EINVAL) };
        if data.len() > PAGE as usize {
            return Err(EINVAL);
        }
        if index >= page_of(self.size().saturating_add(PAGE - 1)) {
            return Err(EINVAL);
        }
        if self.state.lock().pages.contains_key(&index) {
            return Ok(false);
        }
        if !memory::commit(1) {
            return Err(ENOMEM);
        }
        let Some(frame) = new_frame() else {
            memory::uncommit(1);
            return Err(ENOMEM);
        };
        let page = frame_bytes(frame);
        page[..data.len()].copy_from_slice(data);
        page[data.len()..].fill(0);
        let inserted = {
            let mut st = self.state.lock();
            if st.pages.contains_key(&index) {
                false
            } else {
                st.pages.insert(index, Page::new(frame));
                st.charged += 1;
                true
            }
        };
        if !inserted {
            free_frames([frame]);
            memory::uncommit(1);
        }
        if let Some(pager) = pager.upgrade() {
            crate::process::sched::wakeup(pager.wait_chan());
        }
        Ok(inserted)
    }

    /// The pager's answer that page `index` cannot be had: the threads
    /// waiting for it get an error (a later access asks again).
    pub fn fail(&self, index: u64) -> Result<(), i64> {
        let Store::Paged { pager, .. } = &self.store else { return Err(EINVAL) };
        if index >= page_of(self.size().saturating_add(PAGE - 1)) {
            return Err(EINVAL);
        }
        {
            let mut st = self.state.lock();
            if !st.pages.contains_key(&index) {
                st.fail_waiters(index, index + 1);
            }
        }
        if let Some(pager) = pager.upgrade() {
            crate::process::sched::wakeup(pager.wait_chan());
        }
        Ok(())
    }

    /// A private in-memory copy of a file's current contents (a server's
    /// program, kept unchanged whatever happens to the file later).
    pub fn copy_of(file: &super::Inode) -> Result<Arc<PageCache>, i64> {
        let size = file.size();
        let copy = Self::anonymous(size.div_ceil(PAGE)).map_err(|_| ENOMEM)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(64 * 1024).map_err(|_| ENOMEM)?;
        buf.resize(64 * 1024, 0);
        let mut off = 0;
        while off < size {
            let n = file.read_at(off, &mut buf)?;
            if n == 0 {
                break;
            }
            copy.write(off, &buf[..n])?;
            off += n as u64;
        }
        copy.truncate(off)?;
        Ok(copy)
    }

    pub fn size(&self) -> u64 {
        self.state.lock().size
    }

    fn prepaid(&self) -> u64 {
        match self.store {
            Store::Memory { prepaid, .. } => prepaid,
            Store::Paged { .. } | Store::Cached { .. } => 0,
        }
    }

    /// The initial contents of a missing memory-store page into `out`.
    fn fill_memory(&self, st: &State, index: u64, out: &mut [u8]) {
        let image = match self.store {
            Store::Memory { image, .. } => image,
            Store::Paged { .. } | Store::Cached { .. } => &[],
        };
        let start = index * PAGE;
        let valid = (st.image_len.min(st.size) as usize).min(image.len());
        let from = (start as usize).min(valid);
        let to = ((start + PAGE) as usize).min(valid);
        out[..to - from].copy_from_slice(&image[from..to]);
        out[to - from..].fill(0);
    }

    /// Creates page `index` of a memory store if it is missing.
    fn create(&self, index: u64) -> Result<(), i64> {
        if self.state.lock().pages.contains_key(&index) {
            return Ok(());
        }
        // Charge first: committing may not happen under the state lock.
        let charge = if index >= self.prepaid() { Some(Charge::take()?) } else { None };
        let mut st = self.state.lock();
        if st.pages.contains_key(&index) {
            return Ok(());
        }
        let frame = new_frame().ok_or(ENOMEM)?;
        self.fill_memory(&st, index, frame_bytes(frame));
        st.pages.insert(index, Page::new(frame));
        if let Some(c) = charge {
            c.keep();
            st.charged += 1;
        }
        Ok(())
    }

    /// Reads up to `buf.len()` bytes at `off` (fewer at the end of the
    /// file). Sleeps only to wait for the pages of a paged or cached
    /// object.
    pub fn read(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        self.read_with(off, buf, Fill::Yes)
    }

    /// `read`, which with `Fill::No` stops at the first missing page of a
    /// cached object (EAGAIN if that is the first one) instead of asking
    /// its pager; it still waits for a page being filled.
    pub fn read_with(&self, off: u64, buf: &mut [u8], fill: Fill) -> Result<usize, i64> {
        let mut pos = off;
        loop {
            let missing = {
                let mut st = self.state.lock();
                let end = off.saturating_add(buf.len() as u64).min(st.size);
                let mut missing = None;
                while pos < end {
                    let (index, in_page) = (page_of(pos), (pos % PAGE) as usize);
                    let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
                    let out = &mut buf[(pos - off) as usize..][..n];
                    match st.pages.get_mut(&index) {
                        Some(page) if !page.pending => {
                            page.referenced = true;
                            out.copy_from_slice(&frame_bytes(page.frame)[in_page..in_page + n]);
                        }
                        None if matches!(self.store, Store::Memory { .. }) => {
                            let mut page = [0u8; PAGE as usize];
                            self.fill_memory(&st, index, &mut page);
                            out.copy_from_slice(&page[in_page..in_page + n]);
                        }
                        page => {
                            missing = Some((index, page.is_some()));
                            break;
                        }
                    }
                    pos += n as u64;
                }
                missing
            };
            let Some((index, pending)) = missing else {
                return Ok(pos.saturating_sub(off) as usize);
            };
            if fill == Fill::No && !pending && self.is_cached() {
                return if pos == off { Err(EAGAIN) } else { Ok((pos - off) as usize) };
            }
            // (Only a paged or cached object misses pages.)
            match self.wait_paged(index) {
                Ok(()) => continue,
                Err(e) if pos == off => return Err(e),
                Err(_) => return Ok((pos - off) as usize),
            }
        }
    }

    /// Writes `data` at `off`, growing the file.
    pub fn write(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        self.write_with(off, data, Fill::Yes)
    }

    /// `write`; with `Fill::No` a write into a missing page of a cached
    /// object that needs the page's data first stops there (EAGAIN if
    /// nothing was written), as `read_with`.
    pub fn write_with(&self, off: u64, data: &[u8], fill: Fill) -> Result<usize, i64> {
        off.checked_add(data.len() as u64).filter(|&e| e <= MAX_SIZE).ok_or(EFBIG)?;
        // A paged object's pages come from its pager alone (`supply`).
        if let Store::Paged { .. } = self.store {
            return Err(EINVAL);
        }
        if let Store::Cached { limit, .. } = self.store {
            off.checked_add(data.len() as u64).filter(|&e| e <= limit).ok_or(EFBIG)?;
            return self.write_cached(off, data, fill);
        }
        let _io = self.io.lock();
        let end = off + data.len() as u64;
        let mut pos = off;
        while pos < end {
            let (index, in_page) = (page_of(pos), (pos % PAGE) as usize);
            let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
            let chunk = &data[(pos - off) as usize..][..n];
            if let Err(e) = self.create(index) {
                // A short write if some data went in.
                return if pos == off { Err(e) } else { Ok((pos - off) as usize) };
            }
            let mut st = self.state.lock();
            if pos > st.size {
                Self::grow(&mut st, pos);
            }
            if let Some(page) = st.pages.get(&index) {
                frame_bytes(page.frame)[in_page..in_page + n].copy_from_slice(chunk);
            }
            st.size = st.size.max(pos + n as u64);
            pos += n as u64;
        }
        Ok(data.len())
    }

    /// A write into a cached object: each page is made present (created
    /// zeroed where the write covers all of its data, or filled by the
    /// pager), written and marked dirty.
    fn write_cached(&self, off: u64, data: &[u8], fill: Fill) -> Result<usize, i64> {
        let _io = self.io.lock();
        let end = off + data.len() as u64;
        let mut pos = off;
        let mut first_dirty = false;
        let result = loop {
            if pos >= end {
                break Ok(());
            }
            let (index, in_page) = (page_of(pos), (pos % PAGE) as usize);
            let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
            let chunk = &data[(pos - off) as usize..][..n];
            // What the page needs before it can be written.
            enum Need {
                Nothing,
                Wait,
                Fill,
                Create,
            }
            let need = {
                let mut st = self.state.lock();
                let size = st.size;
                match st.pages.get_mut(&index) {
                    Some(page) if !page.pending => {
                        frame_bytes(page.frame)[in_page..in_page + n].copy_from_slice(chunk);
                        page.referenced = true;
                        if !page.dirty {
                            page.dirty = true;
                            st.dirty += 1;
                            first_dirty |= st.dirty == 1;
                            DIRTY.fetch_add(1, Ordering::Relaxed);
                        }
                        if pos > size {
                            Self::grow(&mut st, pos);
                        }
                        st.size = st.size.max(pos + n as u64);
                        Need::Nothing
                    }
                    Some(_) => Need::Wait,
                    None => {
                        // The page's bytes that are file data: kept unless the
                        // write starts at the page and covers them all.
                        let data_len = size.saturating_sub(index * PAGE).min(PAGE);
                        if data_len == 0 || (in_page == 0 && n as u64 >= data_len) { Need::Create } else { Need::Fill }
                    }
                }
            };
            let made = match need {
                Need::Nothing => {
                    pos += n as u64;
                    continue;
                }
                Need::Wait => self.wait_paged(index),
                Need::Fill if fill == Fill::No => Err(EAGAIN),
                Need::Fill => self.wait_paged(index),
                Need::Create => self.create_cached(index),
            };
            if let Err(e) = made {
                break Err(e);
            }
        };
        if first_dirty {
            self.notify_dirty();
        }
        let done = (pos - off) as usize;
        match result {
            Ok(()) => Ok(done),
            Err(e) if done == 0 => Err(e),
            Err(_) => Ok(done),
        }
    }

    /// Creates page `index` of a cached object, zeroed, unless it exists by
    /// now (the caller writes it next).
    fn create_cached(&self, index: u64) -> Result<(), i64> {
        if !memory::cache_charge(1) {
            return Err(ENOMEM);
        }
        let Some(frame) = new_frame() else {
            memory::cache_uncharge(1);
            return Err(ENOMEM);
        };
        frame_bytes(frame).fill(0);
        let mut st = self.state.lock();
        if st.pages.contains_key(&index) {
            drop(st);
            free_frames([frame]);
            memory::cache_uncharge(1);
            return Ok(());
        }
        st.pages.insert(index, Page::new(frame));
        st.charged += 1;
        Ok(())
    }

    /// Tells the pager that this object has a dirty page now (its first).
    fn notify_dirty(&self) {
        if let Store::Cached { pager, key, .. } = &self.store {
            if let Some(pager) = pager.upgrade() {
                pager.dirty(*key);
            }
        }
    }

    /// Extends the file to `len` (> size): bytes past the old end in its
    /// last page (stores through a mapping there) become zero.
    fn grow(st: &mut State, len: u64) {
        let tail = st.size % PAGE;
        if tail != 0 {
            if let Some(page) = st.pages.get(&page_of(st.size)) {
                let stop = (PAGE).min(tail + (len - st.size)) as usize;
                frame_bytes(page.frame)[tail as usize..stop].fill(0);
            }
        }
        st.size = len;
    }

    /// Sets the size to `len`. Shrinking drops the pages beyond and
    /// removes them from every mapping, private copies included, so later
    /// accesses there raise SIGBUS.
    pub fn truncate(&self, len: u64) -> Result<(), i64> {
        if len > MAX_SIZE {
            return Err(EFBIG);
        }
        let first_gone = {
            let _io = self.io.lock();
            if let Store::Cached { limit, .. } = self.store {
                if len > limit {
                    return Err(EFBIG);
                }
            }
            let mut st = self.state.lock();
            if len >= st.size {
                Self::grow(&mut st, len);
                return Ok(());
            }
            let first_gone = page_of(len + PAGE - 1);
            // A granted page stays the object's until it is revoked, and a
            // page being filled gets its data only then (its cut-off tail
            // could not stay zero).
            let partial_pending = len % PAGE != 0 && st.pages.get(&page_of(len)).is_some_and(|p| p.pending);
            if partial_pending || st.pages.range(first_gone..).any(|(_, p)| p.pins > 0) {
                return Err(EBUSY);
            }
            if len % PAGE != 0 {
                if let Some(page) = st.pages.get(&page_of(len)) {
                    frame_bytes(page.frame)[(len % PAGE) as usize..].fill(0);
                }
            }
            let gone = st.pages.split_off(&first_gone);
            let dirty = gone.values().filter(|p| p.dirty).count() as u64;
            st.dirty -= dirty.min(st.dirty);
            let prepaid = self.prepaid();
            let charged = gone.keys().filter(|&&i| i >= prepaid).count() as u64;
            st.charged -= charged;
            st.size = len;
            st.image_len = st.image_len.min(len);
            drop(st);
            undirty(dirty);
            free_frames(gone.into_values().map(|p| p.frame));
            self.uncharge(charged);
            first_gone
        };
        self.unmap_from(first_gone);
        // Waiters for pages beyond the end look again (and fail there).
        if let Some((pager, _)) = self.pager() {
            if let Some(pager) = pager.upgrade() {
                crate::process::sched::wakeup(pager.wait_chan());
            }
        }
        Ok(())
    }

    /// Releases the accounting of `pages` dropped pages.
    fn uncharge(&self, pages: u64) {
        match self.store {
            Store::Memory { .. } => uncharge_tmpfs(pages),
            Store::Cached { .. } if pages > 0 => memory::cache_uncharge(pages),
            Store::Cached { .. } => {}
            Store::Paged { .. } => memory::uncommit(pages),
        }
    }

    /// For a fault at page `index` of the file: its frame, with a new
    /// reference for the mapping (SIGBUS beyond the end of the file or if
    /// the page cannot be read).
    pub fn map_page(&self, index: u64) -> Result<PhysFrame, Fault> {
        loop {
            if let Some(frame) = self.try_map_page(index, true)? {
                return Ok(frame);
            }
        }
    }

    /// `map_page`, which without `wait` does not wait for the page of a
    /// paged or cached object that is missing (it asks the pager for it)
    /// or being filled: None, and the caller waits with `wait_page` once
    /// it holds no lock the pager may need (the address space's), then
    /// tries again.
    pub fn try_map_page(&self, index: u64, wait: bool) -> Result<Option<PhysFrame>, Fault> {
        if index >= page_of(self.size().saturating_add(PAGE - 1)) {
            return Err(Fault::Bus);
        }
        let made = match self.store {
            Store::Memory { .. } => self.create(index),
            Store::Paged { .. } | Store::Cached { .. } if wait => self.wait_paged(index),
            Store::Paged { .. } | Store::Cached { .. } => self.request_page(index),
        };
        match made {
            Ok(()) => {}
            Err(ENOMEM) => return Err(Fault::Oom),
            // tmpfs is full or the server failed: no page to map, as on
            // Linux.
            Err(_) => return Err(Fault::Bus),
        }
        let mut st = self.state.lock();
        // Missing again if a truncation or reclaim came in between (or not
        // there yet: the caller waits).
        match st.pages.get_mut(&index) {
            Some(page) if !page.pending => {
                page.referenced = true;
                memory::with_frames(|f| f.share(page.frame));
                Ok(Some(page.frame))
            }
            _ => Ok(None),
        }
    }

    /// Asks the pager for page `index` unless it is there or being filled
    /// (EIO if the pager is gone). A failure of the fill is seen by the
    /// wait that follows (`wait_page`), which asks again if it came before
    /// the wait began.
    fn request_page(&self, index: u64) -> Result<(), i64> {
        let Some((pager, key)) = self.pager() else { return Ok(()) };
        {
            if self.state.lock().pages.contains_key(&index) {
                return Ok(());
            }
        }
        match pager.upgrade() {
            Some(pager) if pager.request(key, index) => Ok(()),
            _ => Err(EIO),
        }
    }

    /// Waits until page `index` of a paged or cached object is there (see
    /// `try_map_page`) and returns its frame with a reference (so reclaim
    /// cannot take it before the fault is tried again; None if it is gone
    /// already, truncated): EIO if it cannot be had, EINTR if the thread
    /// dies.
    pub fn wait_page(&self, index: u64) -> Result<Option<PhysFrame>, i64> {
        self.wait_paged(index)?;
        let st = self.state.lock();
        Ok(st.pages.get(&index).filter(|p| !p.pending).map(|p| {
            memory::with_frames(|f| f.share(p.frame));
            p.frame
        }))
    }

    /// Lets go of the reference `wait_page` gave.
    pub fn put_frame(frame: PhysFrame) {
        free_frames([frame]);
    }

    /// Pins page `index` for a grant (`process::channel`): makes it present
    /// (a memory store creates it; a paged object's page must have been
    /// supplied: ENODATA otherwise, since its pager may be the caller) and
    /// returns its frame with a reference for the grant. While pinned, the
    /// page stays this object's: truncation fails with EBUSY, reclaim
    /// skips it (its frame is shared), so the frame a service or device
    /// reaches is always the object's page. A cached object's pages are
    /// granted only to be filled or written back (`pin_fill`, `pin_dirty`:
    /// EINVAL here).
    pub fn pin(&self, index: u64) -> Result<PhysFrame, i64> {
        loop {
            if index >= page_of(self.size().saturating_add(PAGE - 1)) {
                return Err(EINVAL);
            }
            match self.store {
                Store::Memory { .. } => self.create(index)?,
                Store::Paged { .. } => {}
                Store::Cached { .. } => return Err(EINVAL),
            }
            let mut st = self.state.lock();
            // Truncated between the check above and the creation: the
            // page made past the end goes again (no page lives there).
            if index >= page_of(st.size.saturating_add(PAGE - 1)) {
                let past = st.pages.remove(&index).filter(|p| p.pins == 0);
                if let Some(page) = past {
                    let charged = (index >= self.prepaid()) as u64;
                    st.charged -= charged;
                    drop(st);
                    free_frames([page.frame]);
                    self.uncharge(charged);
                }
                return Err(EINVAL);
            }
            match st.pages.get_mut(&index) {
                Some(page) => {
                    let frame = page.frame;
                    memory::with_frames(|f| f.share(frame));
                    page.pins += 1;
                    return Ok(frame);
                }
                None if matches!(self.store, Store::Paged { .. }) => return Err(ENODATA),
                // Truncated again in between: try once more.
                None => {}
            }
        }
    }

    /// Ends a pin of `pin`, `pin_fill` or `pin_dirty` (with the frame it
    /// returned). A failed fill's page may be gone already (`filled`): then
    /// only the pin's reference goes.
    pub fn unpin(&self, index: u64, frame: PhysFrame) {
        {
            let mut st = self.state.lock();
            match st.pages.get_mut(&index) {
                Some(page) if page.frame == frame && page.pins > 0 => page.pins -= 1,
                _ => debug_assert!(self.is_cached(), "unpinning a page that is not pinned"),
            }
        }
        free_frames([frame]);
    }

    /// For a grant to fill a cached object: the first run of missing pages
    /// in the `window` pages from `first` (within the file; the run at most
    /// `MAX_RUN` long), made pending (zeroed frames nobody reads until `filled`)
    /// and pinned. Returns the run's first page, its frames (each with a
    /// reference for the grant) and the file's size; ENOENT if no page of
    /// the window is missing, ENOMEM if not even one page can be cached.
    pub fn pin_fill(&self, first: u64, window: u64) -> Result<(u64, Vec<PhysFrame>, u64), Scan> {
        if !self.is_cached() {
            return Err(Scan::Errno(EINVAL));
        }
        // The first gap among the present pages of the window (walking
        // them, at most `MAX_SCAN`), and the run of missing pages there.
        let find = |st: &State| {
            let end = first.saturating_add(window).min(page_of(st.size.saturating_add(PAGE - 1)));
            let mut start = first;
            for (scanned, (&index, _)) in st.pages.range(first..end).enumerate() {
                if index != start {
                    break;
                }
                if scanned >= MAX_SCAN {
                    return Err(Scan::Resume(start));
                }
                start += 1;
            }
            if start >= end {
                return Err(Scan::Errno(ENOENT));
            }
            let stop = end.min(start + MAX_RUN);
            let stop = st.pages.range(start..stop).next().map_or(stop, |(&i, _)| i);
            Ok((start, stop - start))
        };
        let (start, count) = find(&self.state.lock())?;
        // Charged first (reclaiming may take cache locks); one page will do
        // if more do not fit.
        let count = if memory::cache_charge(count) {
            count
        } else if count > 1 && memory::cache_charge(1) {
            1
        } else {
            return Err(Scan::Errno(ENOMEM));
        };
        let mut frames = Vec::new();
        if frames.try_reserve_exact(count as usize).is_err() {
            memory::cache_uncharge(count);
            return Err(Scan::Errno(ENOMEM));
        }
        for _ in 0..count {
            let Some(frame) = new_frame() else { break };
            frame_bytes(frame).fill(0);
            frames.push(frame);
        }
        // The run as it is now: a page may have come or the file shrunk.
        let mut st = self.state.lock();
        let size = st.size;
        let last = page_of(st.size.saturating_add(PAGE - 1));
        let mut taken = 0;
        for (i, &frame) in frames.iter().enumerate() {
            let index = start + i as u64;
            if index >= last || st.pages.contains_key(&index) {
                break;
            }
            st.pages.insert(index, Page { frame, referenced: true, dirty: false, pins: 1, pending: true });
            taken += 1;
        }
        st.charged += taken as u64;
        drop(st);
        let no_frame = frames.is_empty();
        let spare = frames.split_off(taken);
        let unused = count - taken as u64;
        free_frames(spare);
        if unused > 0 {
            memory::cache_uncharge(unused);
        }
        if taken == 0 {
            // No frame to be had, or the pages came meanwhile.
            return Err(Scan::Errno(if no_frame { ENOMEM } else { ENOENT }));
        }
        // The grant's references.
        memory::with_frames(|f| frames.iter().for_each(|&frame| f.share(frame)));
        Ok((start, frames, size))
    }

    /// The pager's answer for `count` pages from `first` (at most
    /// `MAX_RUN`) it filled (`pin_fill`): with `ok` they hold the file's
    /// data now, else the pending ones go and the missing ones fail too
    /// (whoever waits for them gets EIO, a later access asks again; so a
    /// pager that cannot even start a fill answers). Present pages are
    /// left alone. Wakes the waiters.
    pub fn filled(&self, first: u64, count: u64, ok: bool) -> Result<(), i64> {
        let Store::Cached { pager, .. } = &self.store else { return Err(EINVAL) };
        if count > MAX_RUN {
            return Err(EINVAL);
        }
        let end = first.checked_add(count).ok_or(EINVAL)?;
        let mut gone = Vec::new();
        {
            let mut st = self.state.lock();
            let st = &mut *st;
            // Only pages of the file: none lives beyond, nobody waits there.
            let end = end.min(page_of(st.size.saturating_add(PAGE - 1)));
            let pending: Vec<u64> = st.pages.range(first..end.max(first)).filter(|(_, p)| p.pending).map(|(&i, _)| i).collect();
            for index in pending {
                if ok {
                    if let Some(page) = st.pages.get_mut(&index) {
                        page.pending = false;
                    }
                } else if let Some(page) = st.pages.remove(&index) {
                    // A pin keeps its own reference (`unpin`).
                    gone.push(page.frame);
                }
            }
            if !ok {
                // Missing pages fail only for whoever waits for them now.
                st.fail_waiters(first, end);
            }
            st.charged -= gone.len() as u64;
        }
        self.uncharge(gone.len() as u64);
        free_frames(gone);
        if let Some(pager) = pager.upgrade() {
            crate::process::sched::wakeup(pager.wait_chan());
        }
        Ok(())
    }

    /// For write-back of a cached object: the first run of dirty pages in
    /// the `window` pages from `first` (at most `MAX_RUN` long), clean now,
    /// write-protected in every mapping (a store there marks them dirty
    /// again) and pinned. Returns the run's first page, its frames with a
    /// reference each for the grant, and the file's size then (their data
    /// ends there: a write extends the size as it dirties a page, so a
    /// size read before the run could cut off data); ENOENT if none is
    /// dirty.
    pub fn pin_dirty(&self, first: u64, window: u64) -> Result<(u64, Vec<PhysFrame>, u64), Scan> {
        if !self.is_cached() {
            return Err(Scan::Errno(EINVAL));
        }
        let end = first.saturating_add(window);
        let mut frames = Vec::new();
        frames.try_reserve_exact(window.min(MAX_RUN) as usize).map_err(|_| Scan::Errno(ENOMEM))?;
        let (start, size) = {
            let mut st = self.state.lock();
            if st.dirty == 0 {
                return Err(Scan::Errno(ENOENT));
            }
            let size = st.size;
            let mut start = None;
            for (scanned, (&index, page)) in st.pages.range_mut(first..end).enumerate() {
                match start {
                    None if scanned >= MAX_SCAN => return Err(Scan::Resume(index)),
                    None if !page.dirty => continue,
                    None => start = Some(index),
                    Some(s) if !page.dirty || index != s + frames.len() as u64 || frames.len() as u64 == MAX_RUN => break,
                    Some(_) => {}
                }
                page.dirty = false;
                page.pins += 1;
                frames.push(page.frame);
            }
            let Some(start) = start else { return Err(Scan::Errno(ENOENT)) };
            st.dirty -= (frames.len() as u64).min(st.dirty);
            (start, size)
        };
        undirty(frames.len() as u64);
        memory::with_frames(|f| frames.iter().for_each(|&frame| f.share(frame)));
        let pages: Vec<u64> = (start..start + frames.len() as u64).collect();
        self.write_protect(&pages);
        Ok((start, frames, size))
    }

    /// The pager is gone (its thread ended): its pending pages will never
    /// be filled. They go, and whoever waits for them fails.
    fn abandon_pending(&self) {
        let mut gone = Vec::new();
        {
            let mut st = self.state.lock();
            let pending: Vec<u64> = st.pages.iter().filter(|(_, p)| p.pending).map(|(&i, _)| i).collect();
            for index in pending {
                if let Some(page) = st.pages.remove(&index) {
                    gone.push(page.frame);
                }
                st.fail_waiters(index, index + 1);
            }
            st.charged -= gone.len() as u64;
        }
        self.uncharge(gone.len() as u64);
        free_frames(gone);
    }

    /// Marks the present pages among `count` from `first` dirty again (a
    /// write-back that failed).
    pub fn redirty(&self, first: u64, count: u64) -> Result<(), i64> {
        if !self.is_cached() {
            return Err(EINVAL);
        }
        first.checked_add(count).ok_or(EINVAL)?;
        let first_dirty = {
            let mut st = self.state.lock();
            let mut marked = 0;
            for (_, page) in st.pages.range_mut(first..first + count) {
                if !page.dirty && !page.pending {
                    page.dirty = true;
                    marked += 1;
                }
            }
            let was = st.dirty;
            st.dirty += marked;
            DIRTY.fetch_add(marked, Ordering::Relaxed);
            was == 0 && marked > 0
        };
        if first_dirty {
            self.notify_dirty();
        }
        Ok(())
    }

    /// Dirty pages of this object (cached store).
    pub fn dirty_pages(&self) -> u64 {
        self.state.lock().dirty
    }

    /// Ends futex waits on this object for good: waiters wake (the caller
    /// wakes them, `futex::wake_object`) and new waits fail with EPIPE. For
    /// a channel whose peer is gone.
    pub fn hang_up(&self) {
        self.hung_up.store(true, Ordering::SeqCst);
    }

    pub fn is_hung_up(&self) -> bool {
        self.hung_up.load(Ordering::SeqCst)
    }

    /// Drops up to `want` clean pages that only the cache uses, giving a
    /// page used since the last look a second chance. Returns how many.
    fn shrink(&self, want: u64) -> u64 {
        let mut freed = 0;
        let mut seen = 0;
        loop {
            let mut st = self.state.lock();
            // Two turns around the file: the first may only clear marks.
            if freed >= want || seen >= 2 * st.pages.len() {
                break;
            }
            let start = st.cursor;
            let (mut gone, mut frames) = (Vec::new(), Vec::new());
            if gone.try_reserve(RECLAIM_BATCH).is_err() || frames.try_reserve(RECLAIM_BATCH).is_err() {
                break;
            }
            let mut next = 0;
            let mut looked = 0;
            memory::with_frames(|frames| {
                for (&index, page) in st.pages.range_mut(start..).take(RECLAIM_BATCH) {
                    looked += 1;
                    next = index + 1;
                    if page.referenced {
                        page.referenced = false;
                    } else if !page.dirty && !page.pending && frames.refcount(page.frame) == 1 && freed + (gone.len() as u64) < want {
                        gone.push(index);
                    }
                }
            });
            seen += looked.max(1);
            st.cursor = if looked < RECLAIM_BATCH { 0 } else { next };
            frames.extend(gone.iter().filter_map(|i| st.pages.remove(i)).map(|p| p.frame));
            st.charged -= frames.len() as u64;
            drop(st);
            freed += frames.len() as u64;
            self.uncharge(frames.len() as u64);
            free_frames(frames);
        }
        freed
    }

    /// Whether a store through a shared mapping must first mark the page
    /// dirty (a cached object: the page must be written back).
    pub fn tracks_dirty(&self) -> bool {
        self.is_cached()
    }

    /// Marks page `index` dirty before a shared mapping may store to it;
    /// false if the page is gone (truncated meanwhile).
    pub fn set_dirty(&self, index: u64) -> bool {
        let first = {
            let mut st = self.state.lock();
            let Some(page) = st.pages.get_mut(&index).filter(|p| !p.pending) else { return false };
            if page.dirty {
                return true;
            }
            page.dirty = true;
            DIRTY.fetch_add(1, Ordering::Relaxed);
            st.dirty += 1;
            st.dirty == 1
        };
        if first {
            self.notify_dirty();
        }
        true
    }

    /// Write-protects `pages` in every shared mapping of this file.
    fn write_protect(&self, pages: &[u64]) {
        let mut i = self.mappers.lock().len();
        while i > 0 {
            i -= 1;
            let mm = self.mappers.lock().get(i).and_then(Weak::upgrade);
            if let Some(mm) = mm {
                mm.lock().write_protect_file(self, pages);
            }
        }
    }

    /// Records that `mm` maps this file, so truncation can reach its page
    /// table entries. Kept until the address space is gone.
    pub fn register(&self, mm: &Weak<Mm>) -> Result<(), Fault> {
        let mut mappers = self.mappers.lock();
        if mappers.iter().any(|m| m.ptr_eq(mm)) {
            return Ok(());
        }
        mappers.retain(|m| m.strong_count() > 0);
        mappers.try_reserve(1).map_err(|_| Fault::Oom)?;
        mappers.push(mm.clone());
        Ok(())
    }

    /// Removes the pages from `index` on from every mapping of this file.
    /// Walks the list downwards without copying it: `register` only
    /// appends and drops dead entries, which moves live ones down, so an
    /// entry may be visited twice (harmless) but none is skipped.
    fn unmap_from(&self, index: u64) {
        let mut i = self.mappers.lock().len();
        while i > 0 {
            i -= 1;
            let mm = self.mappers.lock().get(i).and_then(Weak::upgrade);
            if let Some(mm) = mm {
                mm.lock().unmap_file(self, index);
            }
        }
    }
}

/// Called after a store made a page dirty, with no lock held, so dirty
/// pages (which reclaim cannot drop) never crowd out memory: above a tenth
/// of the commit limit the pagers of the cached objects are asked to
/// write back; above a fifth the storing thread waits for them (up to
/// `THROTTLE` at a time; never a pager's own thread, which does the
/// writing).
pub fn balance_dirty() {
    let limit = memory::commit_stats().1;
    let (background, hard) = (limit / BACKGROUND, limit / HARD);
    if dirty_pages() <= background {
        return;
    }
    ask_pagers(dirty_pages().saturating_sub(background));
    if dirty_pages() <= hard || crate::process::linux::is_pager() {
        return;
    }
    let deadline = crate::time::now() + THROTTLE;
    loop {
        let wait = crate::process::sched::prepare_to_wait(dirty_chan());
        if dirty_pages() <= hard || crate::time::now() >= deadline || crate::process::signal::dying() {
            break;
        }
        wait.sleep_until(deadline);
    }
}

/// The pager at `pager` (its data pointer) is gone: the pending pages of
/// its cached objects will never be filled (their waiters fail).
pub fn pager_gone(pager: *const ()) {
    let mut i = CACHES.lock().len();
    while i > 0 {
        i -= 1;
        let cache = CACHES.lock().get(i).and_then(Weak::upgrade);
        if let Some(cache) = cache {
            if let Store::Cached { pager: p, .. } = &cache.store {
                if p.as_ptr() as *const () == pager {
                    cache.abandon_pending();
                }
            }
        }
    }
}

/// Asks the pagers whose cached objects have dirty pages to write back
/// about `pages` of them (each pager queues one request at a time).
fn ask_pagers(pages: u64) {
    let mut i = CACHES.lock().len();
    while i > 0 {
        i -= 1;
        let cache = CACHES.lock().get(i).and_then(Weak::upgrade);
        if let Some(cache) = cache {
            if let (Store::Cached { pager, .. }, true) = (&cache.store, cache.dirty_pages() > 0) {
                if let Some(pager) = pager.upgrade() {
                    pager.writeback(pages);
                }
            }
        }
    }
}

/// Drops up to `want` reclaimable pages of cached objects, visiting the
/// caches in turn; returns how many it dropped. Called by `memory` when a
/// commit or a new cache page needs room, never with a cache lock held.
fn reclaim(want: u64) -> u64 {
    let mut freed = 0;
    let caches = CACHES.lock().len();
    for _ in 0..caches {
        let cache = {
            let list = CACHES.lock();
            if list.is_empty() {
                break;
            }
            list[NEXT.fetch_add(1, Ordering::Relaxed) % list.len()].upgrade()
        };
        // (The last reference may go here, outside the list's lock.)
        if let Some(cache) = cache {
            freed += cache.shrink(want - freed);
        }
        if freed >= want {
            break;
        }
    }
    // Dirty pages stand in the way: the pagers write them back (they can
    // be dropped then; this request fails or makes do with less).
    if freed < want && dirty_pages() > 0 {
        ask_pagers(want - freed);
    }
    freed
}

impl Drop for PageCache {
    fn drop(&mut self) {
        let (pages, charged) = {
            let mut st = self.state.lock();
            (core::mem::take(&mut st.pages), st.charged)
        };
        // Dirty pages go with the object: the Linux server writes a file
        // back before it lets go of its object (`datafs::evict`, at the
        // instance's end), or does not want the data (an unlinked file);
        // what an instance that died without writing back left is lost, as
        // on a crash.
        undirty(pages.values().filter(|p| p.dirty).count() as u64);
        free_frames(pages.into_values().map(|p| p.frame));
        self.uncharge(charged);
        let prepaid = self.prepaid();
        if prepaid > 0 {
            memory::uncommit(prepaid);
        }
    }
}
