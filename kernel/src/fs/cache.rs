//! The page cache: the pages of a regular file in physical frames, shared
//! by `read`, `write` and every mapping of the file (see
//! docs/design/page-cache.md).
//!
//! A cache holds one reference on each of its frames; a page table entry
//! that maps a frame holds another, so a page dropped from the cache stays
//! valid for its mappings until they are removed.
//!
//! The memory store (the Linux server's tmpfs files, anonymous shared
//! memory, the programs the kernel starts) has no other copy of the data:
//! its pages are committed memory and are never dropped while the file has
//! them. A page missing within the file is read from the boot image (while
//! that part of the file was never cut off) or is zero; `read` takes those
//! bytes without creating the page.
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
//! A store needs the page's disk space secured up to the file's end
//! (`Page::backed`, delayed allocation): the server's writes check it
//! (`Backing`), a mapping's store asks the pager (`Pager::mkwrite`) and
//! waits for its answer (`backed`), and a file that grows write-protects
//! the page that held its end (`extended`) so its next store asks too.
//! Write-back takes runs of dirty pages (`pin_dirty`: their marks cleared,
//! write-protected in every mapping, pinned for a read-only grant): a
//! store in between faults, marks the page dirty again and is written next
//! time, so none is lost; `redirty` puts back what a failed write took.
//! Its pages are counted as cached memory (`memory::cache_charge`), not
//! committed: they use the frames commitments have not claimed yet, and
//! allocations for user memory reclaim them when free frames run low
//! (`reclaim`): clean pages that nothing pins, a page used since the last
//! look getting a second chance; a page programs map after its mappings
//! that did not use it since the last look are removed (`shrink`, through
//! `mappers`). Dirty ones the reclaim cannot drop make it ask the pagers
//! to write back (`Pager::writeback`), and so do too many dirty pages
//! (`balance_dirty`).
//!
//! Lock order: address space (sleeping) → `io` (sleeping) → `state` →
//! frames. `io` serializes what changes contents or size (`write`,
//! `truncate`); it is never held while an address space is locked. Cache
//! hits only take `state`.

use crate::memory;
use crate::process::address_space::{Fault, Mm, PAGE};
use crate::process::errno::*;
use crate::sync::{IrqSpinLock, Mutex};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use x86_64::structures::paging::{FrameDeallocator, PhysFrame};

/// Largest file size (as `off_t` allows).
pub const MAX_SIZE: u64 = i64::MAX as u64;
/// Pages one reclaim looks at at least (more for larger requests).
const RECLAIM_SCAN: usize = 4096;
/// Pages reclaim looks at per hold of a cache's lock.
const RECLAIM_BATCH: usize = 256;

/// Dirty pages of all caches.
static DIRTY: AtomicU64 = AtomicU64::new(0);

/// Dirty pages (for /proc/meminfo).
pub fn dirty_pages() -> u64 {
    DIRTY.load(Ordering::Relaxed)
}

/// Pages of cached objects that are pinned: being filled, written back or
/// granted otherwise.
static PINNED: AtomicU64 = AtomicU64::new(0);

/// The dirty and pinned pages of one pager's (one Linux server
/// instance's) cached objects: no instance may hold more than its share
/// of what reclaim cannot drop, so one cannot starve the others.
#[derive(Default)]
pub struct CacheCounts {
    dirty: AtomicU64,
    pinned: AtomicU64,
}

impl CacheCounts {
    /// The dirty pages above which an instance's storing threads wait for
    /// its write-back (half the global hard ratio: Linux bounds each
    /// device's share of the dirty pages the same way).
    fn dirty_limit() -> u64 {
        memory::commit_stats().1 / HARD / 2
    }

    /// The most pages an instance may have pinned (filled or written back
    /// at once): a quarter of the commit limit.
    fn pinned_limit() -> u64 {
        memory::commit_stats().1 / 4
    }
}

/// Pinned pages of cached objects (`Writeback:` in /proc/meminfo).
pub fn pinned_pages() -> u64 {
    PINNED.load(Ordering::Relaxed)
}

/// Cache pages that reclaim cannot drop now, dirty or pinned: they are
/// not there for committed memory until write-back or the grant ends, so
/// commit counts them as taken (`memory::commit`), and a store waits
/// rather than let them crowd out committed memory (`balance_dirty`).
pub fn unavailable_pages() -> u64 {
    DIRTY.load(Ordering::Relaxed) + PINNED.load(Ordering::Relaxed)
}

/// Dirty pages written back or dropped so far (a shutdown waits longer
/// while this moves).
static CLEANED: AtomicU64 = AtomicU64::new(0);

pub fn cleaned_pages() -> u64 {
    CLEANED.load(Ordering::Relaxed)
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
    /// A store wants page `index` of cached object `key`, which is not
    /// backed (`Page::backed`): the pager should secure the disk space for
    /// it and answer (`PageCache::backed`). False if the pager is gone.
    fn mkwrite(&self, key: u64, index: u64) -> bool;
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
    Cached { pager: Weak<dyn Pager>, key: u64, limit: u64, counts: Arc<CacheCounts> },
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
    /// Cached store: the bytes from the page's start whose disk space is
    /// secured (the pager promised its blocks, or they exist): the page
    /// may hold file data there that write-back will surely find room
    /// for. A store needs it up to the file's end (`Backing`).
    backed: u16,
    /// The pager was asked to back it (`Pager::mkwrite`), no answer yet.
    mkwrite: bool,
    /// Taken by a reclaim that walks its mappings (`shrink`, as Linux's
    /// isolated pages): other reclaims leave it alone meanwhile (their own
    /// reference would keep each other from dropping it).
    isolated: bool,
}

impl Page {
    fn new(frame: PhysFrame) -> Page {
        Page { frame, referenced: true, dirty: false, pins: 0, pending: false, backed: 0, mkwrite: false, isolated: false }
    }
}

/// The bytes of page `index` a store needs backed when the file is `size`
/// long: those below the end (a store there makes them file data).
fn backing_need(size: u64, index: u64) -> u64 {
    size.saturating_sub(index * PAGE).min(PAGE)
}

/// What a write into a cached object requires of the disk space behind
/// its pages (`Page::backed`, delayed allocation's reservation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backing {
    /// Nothing (writes the server checks otherwise).
    Ignore,
    /// Each page must be backed up to the file's end (as the write leaves
    /// it): the write stops before a page that is not (ENOSPC if nothing
    /// was written), and the pager secures the space first.
    Check,
    /// As `Check`, after marking the pages it writes backed up to this
    /// byte (the pager secured the space up to there).
    Vouched(u64),
}

/// What `set_dirty` found.
pub enum Dirtied {
    Yes,
    /// The page is gone (truncated meanwhile), or its pager.
    Gone,
    /// No memory to record the store among the page's waiters.
    Oom,
    /// It is not backed: the pager was asked (`Pager::mkwrite`) with the
    /// store among the page's waiters already; it waits with this (for the
    /// backing) and tries again.
    Unbacked(PageWait),
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
/// How often a throttled store looks again (and asks the pagers again),
/// and the longest it waits while dirty pages crowd out committed memory
/// or its instance is over its share.
const CROWDED_RECHECK: u64 = 100_000_000;
const CROWDED_WAIT: u64 = 30_000_000_000;

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
    CLEANED.fetch_add(pages, Ordering::Relaxed);
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
    /// Failed answers for the page (counted while anyone waits): fills
    /// (`fail`, `filled`), and backings (`backed`), which only the threads
    /// that wait for the backing too see.
    failures: Seen,
    /// Why the last fill failed: EIO (the page could not be read: SIGBUS
    /// for a mapping) or ENOMEM (no memory for it: the toucher is killed,
    /// as by Linux's OOM killer, not sent SIGBUS).
    fill_errno: i64,
    /// Changes of the page that may have answered or overtaken a request
    /// for it (it came, a fill of it began, it was cut off): a request for
    /// the page made at an earlier count may be done with, one made at the
    /// current count is still outstanding (`PageWait::asked`).
    changes: u32,
}

/// Failed answers for a page that a waiter has seen (`Waiters::failures`).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Seen {
    fill: u32,
    backing: u32,
}

/// What `PageCache::try_map_page` found.
pub enum Lookup {
    /// The page's frame, with a new reference for the mapping.
    Frame(PhysFrame),
    /// The page must come from the pager first: the caller waits with this
    /// once it holds no lock the pager may need, then tries again.
    Missing(PageWait),
}

/// A thread's place among the waiters of page `index` of a paged or
/// cached object (`State::waits`), taken before the pager is asked for the
/// page (`try_map_page`) or its backing (`set_dirty`) and kept across the
/// address space's unlock until the thread waited (`wait`) or gave up
/// (dropped). A pager's failure is recorded only where someone waits, so
/// without it a failure that came before the wait began would be lost,
/// and the wait would ask again: the access that made a failed request
/// must fail (SIGBUS, EFAULT), as on Linux.
pub struct PageWait {
    cache: Arc<PageCache>,
    index: u64,
    /// The page must also be backed (`Pager::mkwrite`).
    backed: bool,
    /// The failures for the page it had seen when it entered.
    seen: Seen,
    /// The page's `Waiters::changes` when this thread asked the pager for
    /// it: while they stay, the request is outstanding and the wait does
    /// not ask again.
    asked: Option<u32>,
}

impl PageWait {
    /// Waits until the page is there (and backed, with `backed`), counting
    /// every failure since it entered: its frame with a reference (so
    /// reclaim cannot take it before the fault is tried again; None if it
    /// is gone already, truncated), EIO if it cannot be had (ENOMEM if the
    /// pager had no memory for it), EINTR if the thread dies.
    pub fn wait(self) -> Result<Option<PhysFrame>, i64> {
        self.cache.wait_registered(self.index, self.backed, self.seen, self.asked)?;
        Ok(self.cache.waited_frame(self.index))
    }
}

impl Drop for PageWait {
    fn drop(&mut self) {
        self.cache.state.lock().leave_wait(self.index);
    }
}

/// The fault for an error making or getting a page: tmpfs is full or the
/// server failed, no page to map (Bus, as on Linux), or no memory (Oom:
/// the toucher is killed).
pub fn page_fault(e: i64) -> Fault {
    match e {
        ENOMEM => Fault::Oom,
        // (A tmpfs page's commit waits for write-back, `Mm::retrying`.)
        EAGAIN => Fault::CommitWait,
        _ => Fault::Bus,
    }
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
    fn enter_wait(&mut self, index: u64) -> Result<Seen, i64> {
        if let Some(w) = self.waits.iter_mut().find(|w| w.index == index) {
            w.count += 1;
            return Ok(w.failures);
        }
        self.waits.try_reserve(1).map_err(|_| ENOMEM)?;
        self.waits.push(Waiters { index, count: 1, failures: Seen::default(), fill_errno: EIO, changes: 0 });
        Ok(Seen::default())
    }

    fn leave_wait(&mut self, index: u64) {
        if let Some(i) = self.waits.iter().position(|w| w.index == index) {
            self.waits[i].count -= 1;
            if self.waits[i].count == 0 {
                self.waits.swap_remove(i);
            }
        }
    }

    /// The failures and changes of page `index` (none where nobody waits).
    fn waited(&self, index: u64) -> (Seen, u32) {
        self.waits.iter().find(|w| w.index == index).map_or((Seen::default(), 0), |w| (w.failures, w.changes))
    }

    /// The pager could not supply pages `first..end`: whoever waits for
    /// one that is still missing fails (a page that is there, or being
    /// filled by another fill, is not this failure's: a store waiting for
    /// its backing does not fail for a neighbour's read error), with
    /// `errno` (`Waiters::fill_errno`).
    fn fail_waiters(&mut self, first: u64, end: u64, errno: i64) -> Wake {
        let pages = &self.pages;
        let mut wake = Wake(false);
        for w in self.waits.iter_mut().filter(|w| (first..end).contains(&w.index) && !pages.contains_key(&w.index)) {
            w.failures.fill = w.failures.fill.wrapping_add(1);
            w.fill_errno = errno;
            w.changes = w.changes.wrapping_add(1);
            wake.0 = true;
        }
        wake
    }

    /// The pager could not back pages `first..end`: whoever waits for
    /// their backing fails.
    fn fail_backing(&mut self, first: u64, end: u64) -> Wake {
        let mut wake = Wake(false);
        for w in self.waits.iter_mut().filter(|w| (first..end).contains(&w.index)) {
            w.failures.backing = w.failures.backing.wrapping_add(1);
            wake.0 = true;
        }
        wake
    }

    /// Pages `first..end` came, began to be filled or were cut off: a
    /// request for one that was made before may be done with, so its
    /// waiters look again (and ask again if it is missing).
    fn changed(&mut self, first: u64, end: u64) -> Wake {
        let mut wake = Wake(false);
        for w in self.waits.iter_mut().filter(|w| (first..end).contains(&w.index)) {
            w.changes = w.changes.wrapping_add(1);
            wake.0 = true;
        }
        wake
    }
}

/// What became of pages being filled (`PageCache::filled`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filled {
    /// They hold the file's data.
    Ok,
    /// They could not be read (EIO) or had (ENOMEM): whoever waits for them
    /// gets the error.
    Failed(i64),
    /// They were not read for a reason that says nothing about the file (the
    /// filesystem's server died): missing again, their waiters ask again.
    Again,
}

/// Whether a change of a cache's state (`State::fail_waiters`,
/// `fail_backing`, `changed`) concerns threads waiting for its pages:
/// they must be woken (`PageCache::wake`) once the state is unlocked, or a
/// waiter that does not ask again for a request it took as outstanding
/// would sleep on.
#[must_use = "the waiters must be woken (PageCache::wake) once the state is unlocked"]
struct Wake(bool);

impl Wake {
    /// Both changes' waiters.
    fn and(self, other: Wake) -> Wake {
        Wake(self.0 || other.0)
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
    /// later access asks again), and the changes that tell whether its own
    /// request is still outstanding. Bounded by the threads waiting: a
    /// pager's failure is recorded only where someone waits.
    waits: Vec<Waiters>,
    /// Dirty pages (cached store).
    dirty: u64,
    /// The pager let go of its last handle (`orphan`): it cannot answer
    /// any more, so what is missing or unbacked never comes.
    orphaned: bool,
}

/// The address spaces that map an object, in the order they registered
/// (`PageCache::register`), for the walks that must reach every mapping of
/// a page (`PageCache::for_each_mapper`).
struct Mappers {
    /// By sequence number, ascending: new entries go at the end, and
    /// removing dead ones keeps the order. Dead entries are removed by
    /// every registration and at the end of every walk, so the list holds
    /// at most the live mappers and those gone since.
    list: Vec<(u64, Weak<Mm>)>,
    /// The next registration's sequence number.
    next: u64,
}

pub struct PageCache {
    state: IrqSpinLock<State>,
    io: Mutex<()>,
    store: Store,
    /// Address spaces that map this file (see `register`).
    mappers: IrqSpinLock<Mappers>,
    /// Set once by `hang_up`: futex waits on this object fail (EPIPE).
    hung_up: core::sync::atomic::AtomicBool,
    /// The pager's handles that name this object (`handle_opened`): at
    /// none left, it is orphaned (`orphan`).
    handles: AtomicUsize,
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
            // Only dirty or pinned disk-file pages in the way: EAGAIN, the
            // caller waits for their write-back where it holds no lock
            // (`create_waiting`; a fault in `Mm::retrying`).
            return Err(if memory::commit_blocked_by_cache(1) { EAGAIN } else { ENOSPC });
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

/// A frame for a page, reclaiming other cache pages for it if none is
/// free (`memory::user_frame`; never with a cache's state locked).
fn new_frame() -> Option<PhysFrame> {
    memory::user_frame()
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
                orphaned: false,
            }),
            io: Mutex::new(()),
            store,
            mappers: IrqSpinLock::new(Mappers { list: Vec::new(), next: 0 }),
            hung_up: core::sync::atomic::AtomicBool::new(false),
            handles: AtomicUsize::new(0),
        })
        .map_err(|_| ENOMEM)
    }

    /// A tmpfs file whose contents start as `image` (initramfs data, or
    /// empty).
    pub fn memory(image: &'static [u8]) -> Result<Arc<PageCache>, i64> {
        Self::new(Store::Memory { image, prepaid: 0 }, image.len() as u64, image.len() as u64)
    }

    /// A program the kernel starts (a native server, the Linux server),
    /// over its bytes in the boot image: every page committed now, so
    /// that a page made at a fault never fails, whatever memory is left
    /// then (a server must not die of a full tmpfs or commit limit when it
    /// first runs a part of its code).
    pub fn program(image: &'static [u8]) -> Result<Arc<PageCache>, i64> {
        let pages = (image.len() as u64).div_ceil(PAGE);
        if !memory::commit(pages) {
            return Err(ENOMEM);
        }
        Self::new(Store::Memory { image, prepaid: pages }, image.len() as u64, image.len() as u64).inspect_err(|_| memory::uncommit(pages))
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
    pub fn cached(size: u64, limit: u64, pager: Weak<dyn Pager>, key: u64, counts: Arc<CacheCounts>) -> Result<Arc<PageCache>, i64> {
        let limit = limit.min(MAX_SIZE);
        if size > limit {
            return Err(EFBIG);
        }
        let cache = Self::new(Store::Cached { pager, key, limit, counts }, size, 0)?;
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

    /// Wakes the threads waiting for this object's pages if `wake` says a
    /// change concerned them (call with the state unlocked).
    fn wake(&self, wake: Wake) {
        if wake.0 {
            self.wake_waiters();
        }
    }

    /// Wakes the threads waiting for this object's pages (they sleep on
    /// the pager's channel, `Pager::wait_chan`).
    fn wake_waiters(&self) {
        if let Some(pager) = self.pager().and_then(|(p, _)| p.upgrade()) {
            crate::process::sched::wakeup(pager.wait_chan());
        }
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

    /// A handle of the pager's names this object (the Linux server's
    /// handle table holds it).
    pub fn handle_opened(&self) {
        self.handles.fetch_add(1, Ordering::Relaxed);
    }

    /// A handle of the pager's that named this object went: at the last,
    /// a paged or cached object is orphaned (`orphan`).
    pub fn handle_closed(&self) {
        if self.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.orphan();
        }
    }

    /// The key of a paged object.
    pub fn paged_key(&self) -> Option<u64> {
        match self.store {
            Store::Paged { key, .. } => Some(key),
            _ => None,
        }
    }

    /// Waits until the pager supplied page `index` and, with `backed`, also
    /// backed it up to the file's end (`Pager::mkwrite`): EIO if it cannot
    /// (it failed, or is gone; ENOMEM if the fill found no memory), EINTR
    /// if the thread is dying (a pager that
    /// never answers must not leave it unkillable).
    fn wait_paged(&self, index: u64, backed: bool) -> Result<(), i64> {
        if self.pager().is_none() {
            return Ok(());
        }
        let seen = self.state.lock().enter_wait(index)?;
        let result = self.wait_registered(index, backed, seen, None);
        self.state.lock().leave_wait(index);
        result
    }

    /// `wait_paged` for a thread already among the page's waiters
    /// (`enter_wait`, which said it had seen `seen` failures) that asked
    /// for the page at its changes `asked`, if it did: it stays one, the
    /// caller lets go. A page that is there (and backed, with `backed`)
    /// ends the wait whatever failed meanwhile; else a failure since it
    /// entered does (a fill's, or a backing's for a wait for the backing
    /// too, never another's: a reader does not fail for a store's full
    /// disk). The pager is asked for the page only if no request of this
    /// thread is outstanding (one was answered or overtaken since).
    fn wait_registered(&self, index: u64, backed: bool, seen: Seen, mut asked: Option<u32>) -> Result<(), i64> {
        let Some((pager, key)) = self.pager() else { return Ok(()) };
        // What the pager is to be asked for.
        enum Ask {
            Nothing,
            Page,
            Backing,
        }
        loop {
            let done = x86_64::instructions::interrupts::without_interrupts(|| {
                let Some(pager) = pager.upgrade() else { return Err(EIO) };
                // Registered before looking, so an answer cannot slip by.
                let wait = crate::process::sched::prepare_to_wait(pager.wait_chan());
                let ask = {
                    let mut st = self.state.lock();
                    let need = backing_need(st.size, index);
                    let beyond = index >= page_of(st.size.saturating_add(PAGE - 1));
                    let (failures, changes) = st.waited(index);
                    let present = st.pages.get(&index).map(|p| (p.pending, p.backed as u64 >= need));
                    match present {
                        Some((false, enough)) if !backed || enough => return Ok(true),
                        // Cut off meanwhile: the caller looks again.
                        None if beyond => return Ok(true),
                        _ => {}
                    }
                    // (Orphaned: no answer can come.)
                    if st.orphaned || (backed && failures.backing != seen.backing) {
                        return Err(EIO);
                    }
                    if failures.fill != seen.fill {
                        return Err(st.waits.iter().find(|w| w.index == index).map_or(EIO, |w| w.fill_errno));
                    }
                    match st.pages.get_mut(&index) {
                        // Being filled or backed: its answer comes without
                        // asking, unless the pager is gone meanwhile.
                        Some(p) if (p.pending || p.mkwrite) && !pager.alive() => return Err(EIO),
                        Some(p) if p.pending || p.mkwrite => Ask::Nothing,
                        Some(p) => {
                            p.mkwrite = true;
                            Ask::Backing
                        }
                        // Its own request is still outstanding: the answer
                        // comes without asking again.
                        None if asked == Some(changes) && !pager.alive() => return Err(EIO),
                        None if asked == Some(changes) => Ask::Nothing,
                        None => {
                            asked = Some(changes);
                            Ask::Page
                        }
                    }
                };
                if crate::process::kill::dying() {
                    if let Ask::Backing = ask {
                        if let Some(p) = self.state.lock().pages.get_mut(&index) {
                            p.mkwrite = false;
                        }
                    }
                    return Err(EINTR);
                }
                let asked = match ask {
                    Ask::Nothing => true,
                    Ask::Page => pager.request(key, index),
                    Ask::Backing => pager.mkwrite(key, index),
                };
                if !asked {
                    return Err(EIO);
                }
                drop(pager);
                wait.sleep();
                Ok(false)
            });
            match done {
                Ok(false) => {}
                Ok(true) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    /// The pager's answer: page `index` with `data` (the rest zero), if it
    /// is still missing; wakes who waits for it. Whether it was inserted.
    pub fn supply(&self, index: u64, data: &[u8]) -> Result<bool, i64> {
        let Store::Paged { .. } = &self.store else { return Err(EINVAL) };
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
        let wake = {
            let mut st = self.state.lock();
            if st.pages.contains_key(&index) {
                None
            } else {
                st.pages.insert(index, Page::new(frame));
                st.charged += 1;
                Some(st.changed(index, index + 1))
            }
        };
        let inserted = wake.is_some();
        match wake {
            Some(wake) => self.wake(wake),
            None => {
                free_frames([frame]);
                memory::uncommit(1);
            }
        }
        Ok(inserted)
    }

    /// The pager's answer that page `index` cannot be had: the threads
    /// waiting for it get an error (a later access asks again).
    pub fn fail(&self, index: u64) -> Result<(), i64> {
        let Store::Paged { .. } = &self.store else { return Err(EINVAL) };
        if index >= page_of(self.size().saturating_add(PAGE - 1)) {
            return Err(EINVAL);
        }
        let wake = self.state.lock().fail_waiters(index, index + 1, EIO);
        self.wake(wake);
        Ok(())
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

    /// `create`, waiting for write-back when only dirty or pinned cache
    /// pages stand in the way of the page's commit (EAGAIN): for callers
    /// that hold no address space (a write, a grant), with one `deadline`
    /// for all the pages of their call.
    fn create_waiting(&self, index: u64, deadline: &mut Option<u64>) -> Result<(), i64> {
        loop {
            match self.create(index) {
                Err(EAGAIN) if memory::wait_for_cache(1, deadline) => {}
                Err(EAGAIN) => return Err(ENOSPC),
                r => return r,
            }
        }
    }

    /// Makes the first `pages` pages of a memory store (a channel's, mapped
    /// into the Linux server's region next), waiting for write-back if
    /// their commit needs it. Nothing to do for other stores.
    pub fn make_pages(&self, pages: u64) -> Result<(), i64> {
        if let Store::Memory { .. } = self.store {
            let mut deadline = None;
            for index in 0..pages.min(page_of(self.size().saturating_add(PAGE - 1))) {
                self.create_waiting(index, &mut deadline)?;
            }
        }
        Ok(())
    }

    /// Creates page `index` of a memory store if it is missing.
    fn create(&self, index: u64) -> Result<(), i64> {
        if self.state.lock().pages.contains_key(&index) {
            return Ok(());
        }
        // Charge and frame first: neither may be had under the state lock
        // (a frame may come from reclaim, which takes cache locks).
        let charge = if index >= self.prepaid() { Some(Charge::take()?) } else { None };
        let frame = new_frame().ok_or(ENOMEM)?;
        let mut st = self.state.lock();
        if st.pages.contains_key(&index) {
            drop(st);
            free_frames([frame]);
            return Ok(());
        }
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
            // (Only a paged or cached object misses pages.) The page waited
            // for is copied from right away, with a reference of the wait's
            // own: reclaim, which may take a page as soon as it came under
            // pressure, cannot take it before this read got its bytes, so a
            // read always makes progress.
            let frame = match self.wait_frame(index) {
                Ok(frame) => frame,
                Err(e) if pos == off => return Err(e),
                Err(_) => return Ok((pos - off) as usize),
            };
            let Some(frame) = frame else { continue };
            let size = self.size();
            let in_page = (pos % PAGE) as usize;
            let end = off.saturating_add(buf.len() as u64).min(size);
            if page_of(pos) == index && pos < end {
                let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
                buf[(pos - off) as usize..][..n].copy_from_slice(&frame_bytes(frame)[in_page..in_page + n]);
                pos += n as u64;
            }
            Self::put_frame(frame);
        }
    }

    /// `wait_paged` for page `index`, which returns its frame with a
    /// reference (None if it was cut off meanwhile).
    fn wait_frame(&self, index: u64) -> Result<Option<PhysFrame>, i64> {
        if self.pager().is_none() {
            return Ok(None);
        }
        let seen = self.state.lock().enter_wait(index)?;
        let result = self.wait_registered(index, false, seen, None).map(|()| self.waited_frame(index));
        self.state.lock().leave_wait(index);
        result
    }

    /// Writes `data` at `off`, growing the file.
    pub fn write(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        self.write_with(off, data, Fill::Yes, Backing::Ignore)
    }

    /// `write`; with `Fill::No` a write into a missing page of a cached
    /// object that needs the page's data first stops there (EAGAIN if
    /// nothing was written), as `read_with`; `backing` as `Backing` says
    /// (cached objects only).
    pub fn write_with(&self, off: u64, data: &[u8], fill: Fill, backing: Backing) -> Result<usize, i64> {
        off.checked_add(data.len() as u64).filter(|&e| e <= MAX_SIZE).ok_or(EFBIG)?;
        // A paged object's pages come from its pager alone (`supply`).
        if let Store::Paged { .. } = self.store {
            return Err(EINVAL);
        }
        if let Store::Cached { limit, .. } = self.store {
            off.checked_add(data.len() as u64).filter(|&e| e <= limit).ok_or(EFBIG)?;
            return self.write_cached(off, data, fill, backing);
        }
        let _io = self.io.lock();
        let end = off + data.len() as u64;
        let mut pos = off;
        let mut deadline = None;
        while pos < end {
            let (index, in_page) = (page_of(pos), (pos % PAGE) as usize);
            let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
            let chunk = &data[(pos - off) as usize..][..n];
            if let Err(e) = self.create_waiting(index, &mut deadline) {
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
    /// pager), checked for its backing, written and marked dirty.
    fn write_cached(&self, off: u64, data: &[u8], fill: Fill, backing: Backing) -> Result<usize, i64> {
        let io = self.io.lock();
        let end = off + data.len() as u64;
        let mut pos = off;
        let mut first_dirty = false;
        let old_size = self.state.lock().size;
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
                Backing,
            }
            let need = {
                let mut st = self.state.lock();
                let size = st.size;
                // The page's bytes that are file data once written.
                let wanted = backing_need(size.max(pos + n as u64), index);
                if let (Backing::Vouched(upto), Some(page)) = (backing, st.pages.get_mut(&index)) {
                    if !page.pending {
                        page.backed = page.backed.max(upto.saturating_sub(index * PAGE).min(PAGE) as u16);
                    }
                }
                match st.pages.get_mut(&index) {
                    Some(page) if !page.pending && backing != Backing::Ignore && (page.backed as u64) < wanted => Need::Backing,
                    Some(page) if !page.pending => {
                        frame_bytes(page.frame)[in_page..in_page + n].copy_from_slice(chunk);
                        page.referenced = true;
                        if !page.dirty {
                            page.dirty = true;
                            st.dirty += 1;
                            first_dirty |= st.dirty == 1;
                            self.dirty_added(1);
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
                        let whole = data_len == 0 || (in_page == 0 && n as u64 >= data_len);
                        let vouched = match backing {
                            Backing::Ignore => true,
                            Backing::Check => false,
                            Backing::Vouched(upto) => upto.saturating_sub(index * PAGE).min(PAGE) >= wanted,
                        };
                        // (Not made for nothing when it could not be written.)
                        if whole && !vouched {
                            Need::Backing
                        } else if whole {
                            Need::Create
                        } else {
                            Need::Fill
                        }
                    }
                }
            };
            let made = match need {
                Need::Nothing => {
                    pos += n as u64;
                    continue;
                }
                Need::Wait => self.wait_paged(index, false),
                Need::Fill if fill == Fill::No => Err(EAGAIN),
                Need::Fill => self.wait_paged(index, false),
                Need::Create => self.create_cached(index),
                Need::Backing => Err(ENOSPC),
            };
            if let Err(e) = made {
                break Err(e);
            }
        };
        drop(io);
        if first_dirty {
            self.notify_dirty();
        }
        self.extended(old_size);
        let done = (pos - off) as usize;
        match result {
            Ok(()) => Ok(done),
            Err(e) if done == 0 => Err(e),
            Err(_) => Ok(done),
        }
    }

    /// The file grew from `old_size`: the page that held its end may now
    /// hold file data beyond its backing, which a writable mapping could
    /// store to unnoticed. It is write-protected, so the next store asks
    /// for its backing (as Linux's `pagecache_isize_extended`).
    fn extended(&self, old_size: u64) {
        if old_size % PAGE == 0 || !self.is_cached() {
            return;
        }
        let index = page_of(old_size);
        let short = {
            let st = self.state.lock();
            st.size > old_size && st.pages.get(&index).is_some_and(|p| (p.backed as u64) < backing_need(st.size, index))
        };
        if short {
            self.write_protect(&[index]);
        }
    }

    /// The pager's answer to `Pager::mkwrite` for the pages from byte
    /// `first` (page-aligned) to `end` (at most `MAX_RUN` pages): backed up
    /// to `end` (`ok`), or the space could not be had (whoever waits for
    /// their backing fails: SIGBUS for a store through a mapping, as
    /// Linux's ENOSPC in `page_mkwrite`). Wakes the waiters.
    pub fn backed(&self, first: u64, end: u64, ok: bool) -> Result<(), i64> {
        let Store::Cached { .. } = &self.store else { return Err(EINVAL) };
        if first % PAGE != 0 || end < first || end - first > MAX_RUN * PAGE {
            return Err(EINVAL);
        }
        // (A failure names at least the page at `first`.)
        let from = page_of(first);
        let to = page_of(end.saturating_add(PAGE - 1)).max(from + 1);
        let failed = {
            let mut st = self.state.lock();
            for (&index, page) in st.pages.range_mut(from..to) {
                page.mkwrite = false;
                if ok {
                    page.backed = page.backed.max((end - index * PAGE).min(PAGE) as u16);
                }
            }
            if ok { Wake(false) } else { st.fail_backing(from, to) }
        };
        // Answered either way: whoever waits for the backing looks again.
        self.wake(Wake(true).and(failed));
        Ok(())
    }

    /// After the disk server lost the pager's promises (it restarted):
    /// clears the backing of the pages from `from` on and returns the
    /// first run of dirty pages among them (at most `MAX_RUN`), whose space
    /// the pager secures again; None when no page is left. Looks at
    /// `MAX_SCAN` pages at most (`Scan::Resume`).
    pub fn unback(&self, from: u64) -> Result<Option<(u64, u64)>, Scan> {
        if !self.is_cached() {
            return Err(Scan::Errno(EINVAL));
        }
        let mut st = self.state.lock();
        let mut run: Option<(u64, u64)> = None;
        for (scanned, (&index, page)) in st.pages.range_mut(from..).enumerate() {
            match run {
                None if scanned >= MAX_SCAN => return Err(Scan::Resume(index)),
                // The next call starts here.
                Some((s, e)) if !page.dirty || index != e || e - s == MAX_RUN => break,
                _ => {}
            }
            page.backed = 0;
            if page.dirty {
                run = Some(run.map_or((index, index + 1), |(s, _)| (s, index + 1)));
            }
        }
        Ok(run)
    }

    /// Creates page `index` of a cached object, zeroed, unless it exists by
    /// now (the caller writes it next).
    fn create_cached(&self, index: u64) -> Result<(), i64> {
        let frame = new_frame().ok_or(ENOMEM)?;
        memory::cache_charge(1);
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
        let wake = st.changed(index, index + 1);
        drop(st);
        self.wake(wake);
        Ok(())
    }

    /// The instance's counts of a cached object.
    fn counts(&self) -> Option<&CacheCounts> {
        match &self.store {
            Store::Cached { counts, .. } => Some(counts),
            _ => None,
        }
    }

    fn dirty_added(&self, pages: u64) {
        DIRTY.fetch_add(pages, Ordering::Relaxed);
        if let Some(c) = self.counts() {
            c.dirty.fetch_add(pages, Ordering::Relaxed);
        }
    }

    fn dirty_removed(&self, pages: u64) {
        if let Some(c) = self.counts() {
            c.dirty.fetch_sub(pages, Ordering::Relaxed);
        }
        undirty(pages);
    }

    /// `pages` pages became pinned under a reservation (`reserve_pins`),
    /// which already counts them for the instance.
    fn pinned_added(&self, pages: u64) {
        PINNED.fetch_add(pages, Ordering::Relaxed);
    }

    /// Returns the part of a reservation that was not pinned.
    fn unreserve_pins(&self, pages: u64) {
        if let Some(c) = self.counts() {
            c.pinned.fetch_sub(pages, Ordering::Relaxed);
        }
    }

    fn pinned_removed(&self, pages: u64) {
        PINNED.fetch_sub(pages, Ordering::Relaxed);
        if let Some(c) = self.counts() {
            c.pinned.fetch_sub(pages, Ordering::Relaxed);
        }
    }

    /// Reserves up to `want` pages of this object's instance's pins
    /// (`pin_fill`, `pin_dirty`) in one atomic step, so concurrent fills
    /// cannot together pass the limit; the caller returns what it did not
    /// pin (`unreserve_pins`). EBUSY if none are left: never a wait here
    /// (the caller may hold pins of its own in flight, which only it can
    /// let go of); the server waits for its transfers and asks again.
    fn reserve_pins(&self, want: u64) -> Result<u64, Scan> {
        let Some(counts) = self.counts() else { return Ok(want) };
        let limit = CacheCounts::pinned_limit();
        let mut room = 0;
        let _ = counts.pinned.try_update(Ordering::AcqRel, Ordering::Acquire, |pinned| {
            room = want.min(limit.saturating_sub(pinned));
            (room > 0).then_some(pinned + room)
        });
        if room == 0 {
            return Err(Scan::Errno(EBUSY));
        }
        Ok(room)
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
        let (first_gone, wake) = {
            let _io = self.io.lock();
            if let Store::Cached { limit, .. } = self.store {
                if len > limit {
                    return Err(EFBIG);
                }
            }
            let mut st = self.state.lock();
            if len >= st.size {
                let old_size = st.size;
                Self::grow(&mut st, len);
                drop(st);
                drop(_io);
                self.extended(old_size);
                return Ok(());
            }
            let first_gone = page_of(len + PAGE - 1);
            // A granted page stays the object's until it is revoked (a
            // pinned page is never taken from the cache: DMA or a copy may
            // be in flight, and its frame and pinned accounting go only at
            // the last unpin), and a page being filled gets its data only
            // then (its cut-off tail could not stay zero).
            let partial_pending = len % PAGE != 0 && st.pages.get(&page_of(len)).is_some_and(|p| p.pending);
            if partial_pending || st.pages.range(first_gone..).any(|(_, p)| p.pins > 0) {
                return Err(EBUSY);
            }
            if len % PAGE != 0 {
                if let Some(page) = st.pages.get_mut(&page_of(len)) {
                    frame_bytes(page.frame)[(len % PAGE) as usize..].fill(0);
                    // The blocks beyond the end go with the cut.
                    page.backed = page.backed.min((len % PAGE) as u16);
                }
            }
            let gone = st.pages.split_off(&first_gone);
            debug_assert!(gone.values().all(|p| p.pins == 0), "truncation took a pinned page");
            let dirty = gone.values().filter(|p| p.dirty).count() as u64;
            st.dirty -= dirty.min(st.dirty);
            let prepaid = self.prepaid();
            let charged = gone.keys().filter(|&&i| i >= prepaid).count() as u64;
            st.charged -= charged;
            st.size = len;
            st.image_len = st.image_len.min(len);
            let wake = st.changed(first_gone, u64::MAX);
            drop(st);
            self.dirty_removed(dirty);
            free_frames(gone.into_values().map(|p| p.frame));
            self.uncharge(charged);
            (first_gone, wake)
        };
        self.unmap_from(first_gone);
        // Waiters for pages beyond the end look again (and fail there).
        self.wake(wake);
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
            if index >= page_of(self.size().saturating_add(PAGE - 1)) {
                return Err(Fault::Bus);
            }
            match self.store {
                Store::Memory { .. } => self.create(index),
                Store::Paged { .. } | Store::Cached { .. } => self.wait_paged(index, false),
            }
            .map_err(page_fault)?;
            // Missing again if a truncation or reclaim came in between.
            if let Some(frame) = self.present(index) {
                return Ok(frame);
            }
        }
    }

    /// `map_page`, which without `wait` does not wait for the page of a
    /// paged or cached object that is missing (it asks the pager for it)
    /// or being filled: `Lookup::Missing`, and the caller waits with it
    /// once it holds no lock the pager may need (the address space's),
    /// then tries again.
    pub fn try_map_page(self: &Arc<Self>, index: u64, wait: bool) -> Result<Lookup, Fault> {
        if wait || self.pager().is_none() {
            return self.map_page(index).map(Lookup::Frame);
        }
        if index >= page_of(self.size().saturating_add(PAGE - 1)) {
            return Err(Fault::Bus);
        }
        if let Some(frame) = self.present(index) {
            return Ok(Lookup::Frame(frame));
        }
        // Among the page's waiters before the pager is asked: its answer,
        // a failure too, cannot come before anyone waits for it.
        // Not asked if it is being filled (its answer comes without).
        let (seen, asked) = {
            let mut st = self.state.lock();
            let seen = st.enter_wait(index).map_err(page_fault)?;
            (seen, (!st.pages.contains_key(&index)).then(|| st.waited(index).1))
        };
        let wait = PageWait { cache: self.clone(), index, backed: false, seen, asked };
        if wait.asked.is_some() {
            self.request_page(index).map_err(page_fault)?;
        }
        Ok(match self.present(index) {
            Some(frame) => Lookup::Frame(frame),
            None => Lookup::Missing(wait),
        })
    }

    /// Page `index`'s frame with a new reference for a mapping, if it is
    /// there and not being filled.
    fn present(&self, index: u64) -> Option<PhysFrame> {
        let mut st = self.state.lock();
        let page = st.pages.get_mut(&index).filter(|p| !p.pending)?;
        page.referenced = true;
        memory::with_frames(|f| f.share(page.frame));
        Some(page.frame)
    }

    /// Asks the pager for page `index` (EIO if the pager is gone). Whoever
    /// asks is among the page's waiters already (`PageWait`), so a failed
    /// fill fails its wait.
    fn request_page(&self, index: u64) -> Result<(), i64> {
        let Some((pager, key)) = self.pager() else { return Ok(()) };
        match pager.upgrade() {
            Some(pager) if pager.request(key, index) => Ok(()),
            _ => Err(EIO),
        }
    }

    /// The frame of page `index` with a reference for whoever waited for
    /// it (`PageWait::wait`).
    fn waited_frame(&self, index: u64) -> Option<PhysFrame> {
        let st = self.state.lock();
        st.pages.get(&index).filter(|p| !p.pending).map(|p| {
            memory::with_frames(|f| f.share(p.frame));
            p.frame
        })
    }

    /// Lets go of the reference `PageWait::wait` gave.
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
    /// EINVAL here). A memory-store page made here may wait for write-back
    /// for its commit, until `deadline` (one for all the pages of a grant).
    pub fn pin(&self, index: u64, deadline: &mut Option<u64>) -> Result<PhysFrame, i64> {
        loop {
            if index >= page_of(self.size().saturating_add(PAGE - 1)) {
                return Err(EINVAL);
            }
            match self.store {
                Store::Memory { .. } => self.create_waiting(index, deadline)?,
                Store::Paged { .. } => {}
                Store::Cached { .. } => return Err(EINVAL),
            }
            let mut st = self.state.lock();
            // Truncated between the check above and the creation: the
            // page made past the end goes again (no page lives there).
            if index >= page_of(st.size.saturating_add(PAGE - 1)) {
                // (Only if nobody pinned it meanwhile: a pinned page is
                // never taken from the cache, its frame and accounting
                // stay until the last unpin.)
                let unpinned = st.pages.get(&index).is_some_and(|p| p.pins == 0);
                let past = if unpinned { st.pages.remove(&index) } else { None };
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
                Some(page) if page.frame == frame && page.pins > 0 => {
                    page.pins -= 1;
                    if page.pins == 0 && self.is_cached() {
                        self.pinned_removed(1);
                    }
                }
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
        let mut frames = Vec::new();
        frames.try_reserve_exact(count as usize).map_err(|_| Scan::Errno(ENOMEM))?;
        // Frames from free memory or reclaimed cache pages (no cache lock
        // held), before the pins are reserved (a fill waiting for memory
        // holds no room others could use); a shorter run will do. Without
        // any, the fill waits for reclaim to make progress (write-back,
        // address spaces busy, mapped pages used since the last look,
        // programs ending), except on the pager's own thread while dirty
        // pages stand in the way: after one forced reclaim it is told
        // (ENOMEM) and writes them back itself first.
        let mut tries = 0;
        let pager_must_write = || dirty_pages() > 0 && crate::process::linux::is_pager();
        while frames.len() < count as usize {
            match new_frame() {
                Some(frame) => {
                    frame_bytes(frame).fill(0);
                    frames.push(frame);
                }
                None if frames.is_empty() && !pager_must_write() && memory::reclaim_retry(1, &mut tries) => {}
                None if frames.is_empty() && pager_must_write() && tries == 0 => {
                    tries = 1;
                    memory::reclaim_forced(1);
                }
                None => break,
            }
        }
        // The pins for what it got; frames beyond the instance's room go.
        let reserved = match self.reserve_pins(frames.len() as u64) {
            // (No frame: ENOMEM below, no pins.)
            _ if frames.is_empty() => 0,
            Ok(reserved) => reserved,
            Err(e) => {
                free_frames(frames);
                return Err(e);
            }
        };
        free_frames(frames.split_off(reserved as usize));
        memory::cache_charge(frames.len() as u64);
        let count = frames.len() as u64;
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
            st.pages.insert(index, Page { pins: 1, pending: true, ..Page::new(frame) });
            taken += 1;
        }
        self.pinned_added(taken as u64);
        self.unreserve_pins(reserved - taken as u64);
        st.charged += taken as u64;
        let wake = st.changed(start, start + taken as u64);
        drop(st);
        // Being filled now: their waiters wait without asking.
        self.wake(wake);
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
    /// `MAX_RUN`) it filled (`pin_fill`): with `Ok` they hold the file's
    /// data now, else the pending ones go and the missing ones fail too
    /// (whoever waits for them gets the error, EIO or ENOMEM, a later
    /// access asks again; so a pager that cannot even start a fill
    /// answers), or, `Again`, go as missing (their waiters ask again).
    /// Present pages are left alone. Wakes the waiters.
    pub fn filled(&self, first: u64, count: u64, outcome: Filled) -> Result<(), i64> {
        let ok = outcome == Filled::Ok;
        let Store::Cached { .. } = &self.store else { return Err(EINVAL) };
        if count > MAX_RUN {
            return Err(EINVAL);
        }
        let end = first.checked_add(count).ok_or(EINVAL)?;
        let mut gone = Vec::new();
        let failed = {
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
                    // A pin keeps its own reference (`unpin`, which finds
                    // the page gone: it is no longer pinned memory).
                    if page.pins > 0 {
                        self.pinned_removed(1);
                    }
                    gone.push(page.frame);
                }
            }
            st.charged -= gone.len() as u64;
            // Missing pages fail only for whoever waits for them now; pages to be read
            // again are only missing: their waiters ask again.
            match outcome {
                Filled::Ok => Wake(false),
                Filled::Failed(e) => st.fail_waiters(first, end, e),
                Filled::Again => st.changed(first, end),
            }
        };
        self.uncharge(gone.len() as u64);
        free_frames(gone);
        // Filled: whoever waits for the pages takes them.
        self.wake(failed.and(Wake(ok)));
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
        let room = self.reserve_pins(window.min(MAX_RUN))?;
        let mut frames = Vec::new();
        if frames.try_reserve_exact(room as usize).is_err() {
            self.unreserve_pins(room);
            return Err(Scan::Errno(ENOMEM));
        }
        let mut newly = 0;
        let run = (|| {
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
                    Some(s) if !page.dirty || index != s + frames.len() as u64 || frames.len() as u64 == room => break,
                    Some(_) => {}
                }
                page.dirty = false;
                if page.pins == 0 {
                    newly += 1;
                }
                page.pins += 1;
                frames.push(page.frame);
            }
            let Some(start) = start else { return Err(Scan::Errno(ENOENT)) };
            st.dirty -= (frames.len() as u64).min(st.dirty);
            Ok((start, size))
        })();
        // Pinned now: the pages that were not before (the reservation
        // counted them for the instance); the rest of it goes back.
        self.pinned_added(newly);
        self.unreserve_pins(room - newly);
        let (start, size) = run?;
        self.dirty_removed(frames.len() as u64);
        memory::with_frames(|f| frames.iter().for_each(|&frame| f.share(frame)));
        let pages: Vec<u64> = (start..start + frames.len() as u64).collect();
        self.write_protect(&pages);
        Ok((start, frames, size))
    }

    /// The pager is gone (its thread ended): its pending pages will never
    /// be filled. They go, and whoever waits for them fails.
    fn abandon_pending(&self) {
        let mut gone = Vec::new();
        let mut wake = Wake(false);
        {
            let mut st = self.state.lock();
            let pending: Vec<u64> = st.pages.iter().filter(|(_, p)| p.pending).map(|(&i, _)| i).collect();
            for index in pending {
                if let Some(page) = st.pages.remove(&index) {
                    if page.pins > 0 {
                        self.pinned_removed(1);
                    }
                    gone.push(page.frame);
                }
                wake = wake.and(st.fail_waiters(index, index + 1, EIO));
            }
            st.charged -= gone.len() as u64;
        }
        self.uncharge(gone.len() as u64);
        free_frames(gone);
        self.wake(wake);
    }

    /// The pager let go of its last handle to this paged or cached object
    /// (the Linux server dropped the file's inode, which nothing maps or
    /// holds any more): no answer can come for it, since every answer
    /// names a handle. A thread still waiting for one of its pages or
    /// their backing (a fault whose mapping went meanwhile) gets EIO
    /// instead of sleeping on; so do later waits, and pending fills go.
    pub fn orphan(&self) {
        if self.pager().is_none() {
            return;
        }
        let wake = {
            let mut st = self.state.lock();
            // (Pages' `mkwrite` marks stay: a wait looks at `orphaned`
            // first.)
            st.orphaned = true;
            Wake(!st.waits.is_empty())
        };
        self.abandon_pending();
        self.wake(wake);
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
            self.dirty_added(marked);
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

    /// Drops up to `want` clean pages, giving a page used since the last
    /// look a second chance: those only the cache uses directly, and, with
    /// `unmap`, also those programs map, after taking them out of every
    /// mapping that has not used them since the last look (as Linux's
    /// reclaim unmaps file pages through the reverse map: the spaces that
    /// map this file, `mappers`, each taken only if free, so reclaim never
    /// waits for a space whose holder may be waiting for memory). Returns
    /// how many it dropped.
    /// Looks at no more pages than `budget` allows (and takes them off).
    fn shrink(&self, want: u64, unmap: bool, force: bool, budget: &mut usize) -> u64 {
        let mut freed = 0;
        let mut seen = 0;
        loop {
            let mut st = self.state.lock();
            // Three turns around the file: the first may only clear the
            // marks, the second the accessed bits of the mapped pages.
            let turns = if unmap { 3 } else { 2 };
            if freed >= want || seen >= turns * st.pages.len() || *budget == 0 {
                break;
            }
            let start = st.cursor;
            let take = RECLAIM_BATCH.min(*budget);
            let (mut gone, mut frames, mut mapped) = (Vec::new(), Vec::new(), Vec::new());
            if gone.try_reserve(RECLAIM_BATCH).is_err()
                || frames.try_reserve(RECLAIM_BATCH).is_err()
                || (unmap && mapped.try_reserve(RECLAIM_BATCH).is_err())
            {
                break;
            }
            let mut next = 0;
            let mut looked = 0;
            memory::with_frames(|frames| {
                for (&index, page) in st.pages.range_mut(start..).take(take) {
                    looked += 1;
                    next = index + 1;
                    if page.isolated {
                        continue;
                    }
                    if page.referenced && !force {
                        page.referenced = false;
                        continue;
                    }
                    if page.dirty || page.pending || page.pins > 0 || freed + ((gone.len() + mapped.len()) as u64) >= want {
                        continue;
                    }
                    if frames.refcount(page.frame) == 1 {
                        gone.push(index);
                    } else if unmap {
                        // A reference of reclaim's own while it walks the
                        // mappings without the state lock: the frame
                        // cannot be freed and reused meanwhile (by another
                        // reclaim dropping the page, say, and a private
                        // copy of this very file page), so an entry that
                        // holds it can only be a mapping of this page.
                        frames.share(page.frame);
                        page.isolated = true;
                        mapped.push((index, page.frame));
                    }
                }
            });
            seen += looked.max(1);
            *budget = budget.saturating_sub(looked.max(1));
            // (Fewer than it could take: the end of the file.)
            st.cursor = if looked < take { 0 } else { next };
            frames.extend(gone.iter().filter_map(|i| st.pages.remove(i)).map(|p| p.frame));
            st.charged -= frames.len() as u64;
            drop(st);
            if !mapped.is_empty() {
                let mut young = Vec::new();
                if young.try_reserve_exact(mapped.len()).is_ok() {
                    young.resize(mapped.len(), false);
                    // (`mapped` is in ascending order of index, as
                    // `reclaim_file_pages` wants it.)
                    // A space that is busy keeps its entries: their pages
                    // stay (still shared) and the next look tries again.
                    // (A walk cut short, no slot left for a reference:
                    // the spaces not visited keep their entries, so
                    // their pages stay, shared.)
                    let _ = self.for_each_mapper_reclaim(|mm| {
                        if let Some(mut space) = mm.try_lock() {
                            space.reclaim_file_pages(self, &mapped, &mut young, force);
                        }
                    });
                    // Unmapped everywhere and still the same clean page
                    // (the frame, which reclaim's reference kept from
                    // reuse, names it): only the cache and reclaim hold
                    // its frame now. A fault mapping it again shares the
                    // frame under the state lock first (`present`), a fork
                    // copies an entry still there: either shows here.
                    let mut st = self.state.lock();
                    // Whether only the cache holds each frame now (`young`
                    // reused: no allocation while the frames are locked).
                    memory::with_frames(|f| {
                        for (&(_, frame), young) in mapped.iter().zip(young.iter_mut()) {
                            if !*young && f.refcount(frame) > 2 {
                                *young = true;
                            }
                        }
                    });
                    let before = frames.len();
                    for (&(index, frame), &kept) in mapped.iter().zip(&young) {
                        let Some(page) = st.pages.get_mut(&index).filter(|p| p.frame == frame) else { continue };
                        page.isolated = false;
                        if kept {
                            page.referenced = true;
                        } else if !page.dirty && !page.pending && page.pins == 0 {
                            frames.push(frame);
                            st.pages.remove(&index);
                        }
                    }
                    st.charged -= (frames.len() - before) as u64;
                } else {
                    // (No memory to walk them: they go back as they were.)
                    let mut st = self.state.lock();
                    for &(index, frame) in &mapped {
                        if let Some(page) = st.pages.get_mut(&index).filter(|p| p.frame == frame) {
                            page.isolated = false;
                        }
                    }
                }
                // Reclaim's own references go (a page dropped above goes
                // with the cache's, freed below).
                free_frames(mapped.iter().map(|&(_, frame)| frame));
            }
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

    /// Marks page `index` dirty before a shared mapping may store to it.
    /// With `backed`, the page must be backed up to the file's end first
    /// (else the pager is asked: `Dirtied::Unbacked`, and the store waits
    /// for it, among the page's waiters from before the pager is asked, so
    /// that a failed backing cannot pass it by); without, only the dirty
    /// mark is set (for a page whose store was backed before:
    /// `AddressSpace::store_to_file`).
    pub fn set_dirty(self: &Arc<Self>, index: u64, backed: bool) -> Dirtied {
        let Store::Cached { pager, key, .. } = &self.store else { return Dirtied::Gone };
        enum Step {
            Dirty { first: bool },
            /// Not backed; whether the pager must be asked (it was not
            /// yet), and the failures the store has seen as a waiter.
            Unbacked { ask: bool, seen: Seen },
        }
        let step = {
            let mut st = self.state.lock();
            let need = backing_need(st.size, index);
            let Some(page) = st.pages.get(&index).filter(|p| !p.pending) else { return Dirtied::Gone };
            if backed && (page.backed as u64) < need {
                // Among the waiters in the same hold that finds it
                // unbacked: a `backed` answer cannot come in between.
                let Ok(seen) = st.enter_wait(index) else { return Dirtied::Oom };
                let page = st.pages.get_mut(&index).expect("found above");
                let ask = !page.mkwrite;
                page.mkwrite = true;
                Step::Unbacked { ask, seen }
            } else if page.dirty {
                Step::Dirty { first: false }
            } else {
                st.pages.get_mut(&index).expect("found above").dirty = true;
                self.dirty_added(1);
                st.dirty += 1;
                Step::Dirty { first: st.dirty == 1 }
            }
        };
        match step {
            Step::Dirty { first } => {
                if first {
                    self.notify_dirty();
                }
                Dirtied::Yes
            }
            Step::Unbacked { ask, seen } => {
                // Made with the state unlocked: dropping it locks it.
                let wait = PageWait { cache: self.clone(), index, backed: true, seen, asked: None };
                if !ask || pager.upgrade().is_some_and(|p| p.mkwrite(*key, index)) {
                    return Dirtied::Unbacked(wait);
                }
                if let Some(page) = self.state.lock().pages.get_mut(&index) {
                    page.mkwrite = false;
                }
                Dirtied::Gone
            }
        }
    }

    /// Write-protects `pages` in every shared mapping of this file.
    fn write_protect(&self, pages: &[u64]) {
        self.for_each_mapper(|mm| mm.lock().write_protect_file(self, pages));
    }

    /// Records that `mm` maps this file, so truncation and write-back can
    /// reach its page table entries; at the end of the list, with the next
    /// sequence number, so a walk in progress reaches it too
    /// (`for_each_mapper`). Kept until the address space is gone: then
    /// skipped, and removed by the next registration or the end of the
    /// next walk (`Mappers::compact`).
    ///
    /// A fork child registers with the caches of the areas it inherits
    /// under its parent's lock, before it gets copies of the parent's
    /// entries (`Mm::fork`): after the parent, so every walk that could
    /// leave the child a stale copy reaches the child after the parent.
    pub fn register(&self, mm: &Weak<Mm>) -> Result<(), Fault> {
        let mut mappers = self.mappers.lock();
        mappers.compact();
        if mappers.list.iter().any(|(_, m)| m.ptr_eq(mm)) {
            return Ok(());
        }
        mappers.list.try_reserve(1).map_err(|_| Fault::Oom)?;
        let seq = mappers.next;
        mappers.next += 1;
        mappers.list.push((seq, mm.clone()));
        Ok(())
    }

    /// Removes the pages from `index` on from every mapping of this file.
    fn unmap_from(&self, index: u64) {
        self.for_each_mapper(|mm| mm.lock().unmap_file(self, index));
    }

    /// `for_each_mapper` for reclaim: the references it takes go to the
    /// background reclaimer (`defer_drop`), never dropped here; when no
    /// slot is left for one, the walk ends (false: not every mapper was
    /// visited).
    fn for_each_mapper_reclaim(&self, mut f: impl FnMut(&Mm)) -> bool {
        let mut last = None;
        loop {
            if !defer_slot() {
                return false;
            }
            let entry = {
                let mappers = self.mappers.lock();
                let i = last.map_or(0, |seq| mappers.list.partition_point(|&(s, _)| s <= seq));
                mappers.list.get(i).map(|(seq, mm)| (*seq, mm.upgrade()))
            };
            let Some((seq, mm)) = entry else {
                defer_unslot();
                break;
            };
            last = Some(seq);
            match mm {
                Some(mm) => {
                    f(&mm);
                    defer_drop(Deferred::Space(mm));
                }
                None => defer_unslot(),
            }
        }
        true
    }

    /// Calls `f` for each address space that maps this file, one at a time
    /// with no lock of the cache held (`f` locks the space), in the order
    /// they registered, those that register meanwhile included.
    ///
    /// That is what makes a change to the cache (a truncation, a write-back
    /// cleaning pages) reach a fork child's copy of its parent's entries:
    /// the walk starts after the change and visits the parent under the
    /// parent's lock. If the fork copied the entries before that visit, it
    /// registered the child before (`Mm::fork`, under the parent's lock), so
    /// the walk visits the child after the parent, once the copy is done
    /// (the child's lock is held for it); if it copied them after the
    /// visit, they are as current as the parent's.
    ///
    /// A walk ends once it caught up with the registrations, so forks in a
    /// tight loop prolong it (by design: each new child may hold a copy
    /// the walk must reach; it ends when the forks pause or the walk
    /// overtakes them). It goes on after the sequence number it visited
    /// last, not at an index, so dead entries may be removed meanwhile
    /// (by registrations, by other walks) without making it skip one.
    fn for_each_mapper(&self, mut f: impl FnMut(&Mm)) {
        let mut last = None;
        loop {
            let entry = {
                let mappers = self.mappers.lock();
                let i = last.map_or(0, |seq| mappers.list.partition_point(|&(s, _)| s <= seq));
                mappers.list.get(i).map(|(seq, mm)| (*seq, mm.upgrade()))
            };
            let Some((seq, mm)) = entry else { break };
            last = Some(seq);
            // (The space, if this was its last reference, goes here,
            // outside the lock.)
            if let Some(mm) = mm {
                f(&mm);
            }
        }
        self.mappers.lock().compact();
    }
}

impl Mappers {
    /// Drops the entries of address spaces that are gone. Keeps the order
    /// (and with it, parent before child).
    fn compact(&mut self) {
        self.list.retain(|(_, m)| m.strong_count() > 0);
    }
}

/// Called after a store made a page of `dirtied` dirty, with no lock held,
/// so dirty pages (which reclaim cannot drop) never crowd out memory: above
/// a tenth of the commit limit the pagers of the cached objects are asked
/// to write back; above a fifth the storing thread waits for them (up to
/// `THROTTLE` at a time; never a pager's own thread, which does the
/// writing).
///
/// Only a store that dirtied a page of a disk file is throttled, as Linux
/// calls `balance_dirty_pages` only where a page cache page was dirtied: a
/// store to anonymous or tmpfs memory adds nothing write-back could take
/// away, and the thread that stores may be the one write-back waits for (a
/// disk server faulting in its own heap or stack while it writes the
/// pages back: throttled, it would wait for itself).
pub fn balance_dirty(dirtied: &PageCache) {
    // (Nothing dirty or pinned: nothing to balance.)
    if unavailable_pages() == 0 {
        return;
    }
    let limit = memory::commit_stats().1;
    let (background, hard) = (limit / BACKGROUND, limit / HARD);
    // The share of the instance that owns the file the store dirtied (it
    // was charged for the page, whoever stored).
    let own = dirtied.counts();
    let own_over = || own.is_some_and(|c| c.dirty.load(Ordering::Relaxed) > CacheCounts::dirty_limit());
    // Committed memory and the cache pages reclaim cannot drop together
    // beyond the limit: the frames promised could not all be had.
    let crowding = || {
        let (committed, limit) = memory::commit_stats();
        committed + unavailable_pages() > limit
    };
    if dirty_pages() <= background && !crowding() && !own_over() {
        return;
    }
    ask_pagers(dirty_pages().saturating_sub(background).max(1));
    if crate::process::linux::is_pager() || (dirty_pages() <= hard && !crowding() && !own_over()) {
        return;
    }
    // Above the hard ratio, above the instance's share or crowding out
    // committed memory: the storing thread waits for write-back, killably,
    // up to `THROTTLE` (`CROWDED_WAIT` while it crowds out committed
    // memory or its instance is over its share), looking again (and asking
    // the pagers again) every `CROWDED_RECHECK`. A write-back that makes
    // no progress for that long (a stuck disk) does not hold the store
    // for ever: committed memory then has its own end (`memory::reclaim_retry`).
    let start = crate::time::now();
    loop {
        let wait = crate::process::sched::prepare_to_wait(dirty_chan());
        let (crowded, over) = (crowding(), own_over());
        let now = crate::time::now();
        let limit = if crowded || over { CROWDED_WAIT } else { THROTTLE };
        if crate::process::kill::dying() || (!crowded && !over && dirty_pages() <= hard) || now >= start + limit {
            break;
        }
        wait.sleep_until((now + CROWDED_RECHECK).min(start + limit));
        ask_pagers(dirty_pages().max(1));
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

/// Asks the pagers to write back about `pages` dirty pages (for a commit
/// that the cache's dirty pages stand in the way of).
pub fn ask_writeback(pages: u64) {
    if dirty_pages() > 0 {
        ask_pagers(pages.max(1));
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

/// A reference reclaim took: dropping it may be the last drop, an address
/// space's or a cache's teardown (`AddressSpace::drop` tells the Linux
/// server's instance and futexes, `PageCache::drop` frees frames), which
/// must not run within an allocation that reclaimed (its caller may hold
/// any lock). So reclaim never drops one: it hands each to the background
/// reclaimer, which drops it holding no lock (the `Arc` itself, so the
/// object stays where it is until then). Held only to be dropped.
#[allow(dead_code)]
enum Deferred {
    Space(Arc<Mm>),
    Cache(Arc<PageCache>),
}

/// How many references may wait for the background reclaimer.
const DEFERRED_MAX: usize = 256;

/// The references left for the background reclaimer (`drop_deferred`), in
/// a fixed array (no allocation), and the slots taken (`defer_slot`): a
/// slot is taken before the reference, so handing it over cannot fail.
static DEFERRED: IrqSpinLock<heapless::Vec<Deferred, DEFERRED_MAX>> = IrqSpinLock::new(heapless::Vec::new());
static DEFERRED_SLOTS: AtomicUsize = AtomicUsize::new(0);

/// Takes a slot for a reference reclaim is about to take; false if all are
/// taken (the reclaim stops its walk for this round: the background
/// reclaimer, woken, frees them).
fn defer_slot() -> bool {
    let taken = DEFERRED_SLOTS.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < DEFERRED_MAX).then_some(n + 1)).is_ok();
    if !taken {
        memory::wake_reclaimer();
    }
    taken
}

/// Gives back a slot taken for a reference that never came (the object
/// was gone).
fn defer_unslot() {
    DEFERRED_SLOTS.fetch_sub(1, Ordering::AcqRel);
}

/// Hands reclaim's reference `d` (its slot taken) to the background
/// reclaimer.
fn defer_drop(d: Deferred) {
    let pushed = DEFERRED.lock().push(d);
    debug_assert!(pushed.is_ok(), "a deferred reference without its slot");
    memory::wake_reclaimer();
}

/// Drops the references reclaim left (the background reclaimer, which
/// holds no lock).
pub fn drop_deferred() {
    loop {
        let Some(d) = DEFERRED.lock().pop() else { return };
        drop(d);
        DEFERRED_SLOTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Drops up to `want` reclaimable pages of cached objects, visiting the
/// caches in turn; returns how many it dropped. Called by `memory` when a
/// commit or a new cache page needs room, never with a cache lock held.
/// The pages only the caches use go first; then, if that was not enough,
/// those that programs map too (unmapped where unused, `shrink`), unless
/// interrupts are off: unmapping needs TLB shootdowns, which must not be
/// waited for there. With `force`, pages used lately go too.
fn reclaim(want: u64, force: bool) -> u64 {
    let mut freed = 0;
    let may_unmap = x86_64::instructions::interrupts::are_enabled();
    for unmap in [false, true] {
        if freed >= want || (unmap && !may_unmap) {
            break;
        }
        // The work of one pass is bounded: it looks at no more pages than
        // this (a large cache with little to drop is not walked whole each
        // time; the cursors go on where it stopped).
        let mut budget = RECLAIM_SCAN.max(want as usize * 16);
        let caches = CACHES.lock().len();
        for _ in 0..caches {
            // (A slot for the reference first: `defer_drop`.)
            if !defer_slot() {
                return freed;
            }
            let cache = {
                let list = CACHES.lock();
                if list.is_empty() {
                    defer_unslot();
                    break;
                }
                list[NEXT.fetch_add(1, Ordering::Relaxed) % list.len()].upgrade()
            };
            if cache.is_none() {
                defer_unslot();
            }
            if let Some(cache) = cache {
                freed += cache.shrink(want - freed, unmap, force, &mut budget);
                defer_drop(Deferred::Cache(cache));
            }
            if freed >= want {
                break;
            }
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
        self.dirty_removed(pages.values().filter(|p| p.dirty).count() as u64);
        if self.is_cached() {
            self.pinned_removed(pages.values().filter(|p| p.pins > 0).count() as u64);
        }
        free_frames(pages.into_values().map(|p| p.frame));
        self.uncharge(charged);
        let prepaid = self.prepaid();
        if prepaid > 0 {
            memory::uncommit(prepaid);
        }
    }
}
