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
//! The remote store caches the pages of a file served by a filesystem
//! server. A miss reads up to 64 KiB of missing pages; `write` goes to the
//! server first (write-through) and then into the cached pages, which it
//! also creates for whole pages it covers. The size is kept here: every
//! change goes through the kernel. Its pages are counted as cached memory
//! (`memory::cache_charge`), which commits and new cache pages reclaim:
//! clean pages that no mapping uses, a page used since the last look
//! getting a second chance.
//!
//! Shared mappings of a remote file map its pages read-only until the
//! first store, which marks the page dirty (see `set_dirty`). Write-back
//! clears the dirty mark, write-protects the page in every mapping, then
//! writes it: a store in between faults, marks it dirty again and is
//! written next time, so none is lost. `msync`, `fsync` and `sync` write
//! back, and so does the flusher every few seconds and a writer that finds
//! too many dirty pages.
//!
//! The paged store belongs to a pager in user space (the Linux server): a
//! missing page is requested from it and the thread that needs the page
//! sleeps until the pager supplies it (`supply`), whether the thread runs a
//! program or the kernel (copying from a mapping). Supplied pages are
//! committed and stay until the object goes.
//!
//! Lock order: address space (sleeping) → `io` (sleeping) → `state` →
//! frames. `io` serializes what changes contents or size and talks to the
//! server (filling pages, `write`, `truncate`); it is never held while an
//! address space is locked. Cache hits only take `state`.

use super::remote::RemoteFs;
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
/// Pages a miss reads from a server at once.
const READAHEAD: u64 = 16;
/// Pages reclaim looks at per hold of a cache's lock.
const RECLAIM_BATCH: usize = 256;
/// Dirty pages written back per round (and at most per message group).
const WRITEBACK_BATCH: usize = 16;
/// How often the flusher writes dirty pages back (nanoseconds).
const FLUSH_INTERVAL: u64 = 5_000_000_000;

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

/// Caches with a remote store, for reclaim; the next one to look at.
static REMOTE: IrqSpinLock<Vec<Weak<PageCache>>> = IrqSpinLock::new(Vec::new());
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
}

/// Where pages come from.
enum Store {
    /// The cache is the only copy. `image` is the file's initramfs data;
    /// pages below `prepaid` were committed when the cache was created
    /// (anonymous shared memory) and are not charged again.
    Memory { image: &'static [u8], prepaid: u64 },
    /// Inode `ino` of a filesystem server.
    Remote { fs: Arc<RemoteFs>, ino: u32 },
    /// A pager in user space, which knows the object by `key`.
    Paged { pager: Weak<dyn Pager>, key: u64 },
}

struct Page {
    frame: PhysFrame,
    /// Used since reclaim last looked at it.
    referenced: bool,
    /// Stored to through a shared mapping since it was last written back
    /// (remote store only).
    dirty: bool,
}

impl Page {
    fn new(frame: PhysFrame) -> Page {
        Page { frame, referenced: true, dirty: false }
    }
}

struct State {
    pages: BTreeMap<u64, Page>,
    size: u64,
    /// Bytes of `image` still valid as file contents (truncation cuts it).
    image_len: u64,
    /// Pages charged: to commit and tmpfs (memory store, beyond
    /// `prepaid`), or as cached memory (remote store).
    charged: u64,
    /// The page index reclaim continues at.
    cursor: u64,
    /// Pages of a paged object its pager failed to supply: the waiting
    /// threads get an error once, a later access asks again.
    failed: alloc::collections::BTreeSet<u64>,
}

pub struct PageCache {
    state: IrqSpinLock<State>,
    io: Mutex<()>,
    store: Store,
    /// Address spaces that map this file (see `register`).
    mappers: IrqSpinLock<Vec<Weak<Mm>>>,
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
                failed: alloc::collections::BTreeSet::new(),
            }),
            io: Mutex::new(()),
            store,
            mappers: IrqSpinLock::new(Vec::new()),
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

    /// The cache of inode `ino` of a filesystem server, `size` bytes long.
    pub fn remote(fs: Arc<RemoteFs>, ino: u32, size: u64) -> Result<Arc<PageCache>, i64> {
        let cache = Self::new(Store::Remote { fs, ino }, size, 0)?;
        let mut list = REMOTE.lock();
        list.retain(|c| c.strong_count() > 0);
        list.try_reserve(1).map_err(|_| ENOMEM)?;
        list.push(Arc::downgrade(&cache));
        Ok(cache)
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
        let Store::Paged { pager, key } = &self.store else { return Ok(()) };
        loop {
            let done = x86_64::instructions::interrupts::without_interrupts(|| {
                let Some(pager) = pager.upgrade() else { return Err(EIO) };
                // Registered before looking, so an answer cannot slip by.
                let wait = crate::process::sched::prepare_to_wait(pager.wait_chan());
                {
                    let mut st = self.state.lock();
                    if st.pages.contains_key(&index) {
                        return Ok(true);
                    }
                    if st.failed.remove(&index) {
                        return Err(EIO);
                    }
                }
                if crate::process::signal::dying() {
                    return Err(EINTR);
                }
                if !pager.request(*key, index) {
                    return Err(EIO);
                }
                drop(pager);
                wait.sleep();
                Ok(false)
            })?;
            if done {
                return Ok(());
            }
        }
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
                st.failed.insert(index);
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

    /// Whether no page is cached.
    pub fn is_empty(&self) -> bool {
        self.state.lock().pages.is_empty()
    }

    pub fn size(&self) -> u64 {
        self.state.lock().size
    }

    fn remote_store(&self) -> Option<(&Arc<RemoteFs>, u32)> {
        match &self.store {
            Store::Remote { fs, ino } => Some((fs, *ino)),
            Store::Memory { .. } | Store::Paged { .. } => None,
        }
    }

    fn prepaid(&self) -> u64 {
        match self.store {
            Store::Memory { prepaid, .. } => prepaid,
            Store::Remote { .. } | Store::Paged { .. } => 0,
        }
    }

    /// The initial contents of a missing memory-store page into `out`.
    fn fill_memory(&self, st: &State, index: u64, out: &mut [u8]) {
        let image = match self.store {
            Store::Memory { image, .. } => image,
            Store::Remote { .. } | Store::Paged { .. } => &[],
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

    /// Reads page `index` of a remote store (and missing pages after it)
    /// from the server, unless it is there by now. ENOMEM if no page could
    /// be cached.
    fn fetch(&self, index: u64) -> Result<(), i64> {
        let (fs, ino) = self.remote_store().expect("remote store");
        let _io = self.io.lock();
        let (count, size) = {
            let st = self.state.lock();
            let last = page_of(st.size.saturating_add(PAGE - 1));
            let count = (index..last.min(index + READAHEAD)).take_while(|i| !st.pages.contains_key(i)).count() as u64;
            (count, st.size)
        };
        if count == 0 {
            return Ok(());
        }
        // One page will do if more do not fit.
        let count = if memory::cache_charge(count) {
            count
        } else if count > 1 && memory::cache_charge(1) {
            1
        } else {
            return Err(ENOMEM);
        };
        let start = index * PAGE;
        let bytes = (count * PAGE).min(size - start) as usize;
        let mut buf = Vec::new();
        if buf.try_reserve_exact(bytes).is_err() {
            memory::cache_uncharge(count);
            return Err(ENOMEM);
        }
        buf.resize(bytes, 0);
        let n = match fs.read(ino, start, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                memory::cache_uncharge(count);
                return Err(e);
            }
        };
        let mut st = self.state.lock();
        let mut got = 0;
        for i in 0..count {
            let Some(frame) = new_frame() else { break };
            let page = frame_bytes(frame);
            let from = ((i * PAGE) as usize).min(n);
            let to = (((i + 1) * PAGE) as usize).min(n);
            page[..to - from].copy_from_slice(&buf[from..to]);
            page[to - from..].fill(0);
            // Nothing else inserts remote pages without `io`.
            st.pages.insert(index + i, Page::new(frame));
            got += 1;
        }
        st.charged += got;
        drop(st);
        memory::cache_uncharge(count - got);
        if got == 0 {
            return Err(ENOMEM);
        }
        Ok(())
    }

    /// Reads up to `buf.len()` bytes at `off` (fewer at the end of the
    /// file). Sleeps only to read missing pages of a remote store.
    pub fn read(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
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
                        Some(page) => {
                            page.referenced = true;
                            out.copy_from_slice(&frame_bytes(page.frame)[in_page..in_page + n]);
                        }
                        None if matches!(self.store, Store::Memory { .. }) => {
                            let mut page = [0u8; PAGE as usize];
                            self.fill_memory(&st, index, &mut page);
                            out.copy_from_slice(&page[in_page..in_page + n]);
                        }
                        None => {
                            missing = Some((index, end));
                            break;
                        }
                    }
                    pos += n as u64;
                }
                missing
            };
            let Some((index, end)) = missing else {
                return Ok(pos.saturating_sub(off) as usize);
            };
            if let Store::Paged { .. } = self.store {
                match self.wait_paged(index) {
                    Ok(()) => continue,
                    Err(e) if pos == off => return Err(e),
                    Err(_) => return Ok((pos - off) as usize),
                }
            }
            match self.fetch(index) {
                Ok(()) => {}
                // No room to cache it: read this page past the cache.
                Err(ENOMEM) => {
                    let (fs, ino) = self.remote_store().expect("remote store");
                    let stop = ((index + 1) * PAGE).min(end);
                    let n = fs.read(ino, pos, &mut buf[(pos - off) as usize..(stop - off) as usize])?;
                    pos += n as u64;
                    if pos < stop {
                        return Ok((pos - off) as usize);
                    }
                }
                Err(e) if pos == off => return Err(e),
                Err(_) => return Ok((pos - off) as usize),
            }
        }
    }

    /// Writes `data` at `off`, growing the file.
    pub fn write(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        off.checked_add(data.len() as u64).filter(|&e| e <= MAX_SIZE).ok_or(EFBIG)?;
        // A paged object's pages come from its pager alone (`supply`).
        if let Store::Paged { .. } = self.store {
            return Err(EINVAL);
        }
        let _io = self.io.lock();
        let written = match self.remote_store() {
            // Write-through: the server has the data before the cache.
            Some((fs, ino)) => fs.write(ino, off, data)?,
            None => data.len(),
        };
        let end = off + written as u64;
        let mut pos = off;
        while pos < end {
            let (index, in_page) = (page_of(pos), (pos % PAGE) as usize);
            let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
            let chunk = &data[(pos - off) as usize..][..n];
            let stored = match self.remote_store() {
                None => self.create(index).map(|_| true),
                Some(_) => Ok(self.cache_written(index, in_page, n)),
            };
            if let Err(e) = stored {
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
        Ok(written)
    }

    /// For a write of `n` bytes at `in_page` into page `index` of a remote
    /// store: creates the page (zeroed) if the write covers all of it that
    /// has data (or it lies past the end), so written data is cached; a
    /// page that would need reading first is not created. Whether the page
    /// is cached now.
    fn cache_written(&self, index: u64, in_page: usize, n: usize) -> bool {
        let size = {
            let st = self.state.lock();
            if st.pages.contains_key(&index) {
                return true;
            }
            st.size
        };
        let start = index * PAGE;
        let old_data_end = size.saturating_sub(start).min(PAGE) as usize;
        // A page wholly past the old end held only zeros.
        let covers = start >= size || (in_page == 0 && n >= old_data_end);
        if !covers || !memory::cache_charge(1) {
            return false;
        }
        let Some(frame) = new_frame() else {
            memory::cache_uncharge(1);
            return false;
        };
        frame_bytes(frame).fill(0);
        let mut st = self.state.lock();
        st.pages.insert(index, Page::new(frame));
        st.charged += 1;
        true
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
            if let Some((fs, ino)) = self.remote_store() {
                fs.truncate(ino, len)?;
            }
            let mut st = self.state.lock();
            if len >= st.size {
                Self::grow(&mut st, len);
                return Ok(());
            }
            let first_gone = page_of(len + PAGE - 1);
            if len % PAGE != 0 {
                if let Some(page) = st.pages.get(&page_of(len)) {
                    frame_bytes(page.frame)[(len % PAGE) as usize..].fill(0);
                }
            }
            let gone = st.pages.split_off(&first_gone);
            DIRTY.fetch_sub(gone.values().filter(|p| p.dirty).count() as u64, Ordering::Relaxed);
            let prepaid = self.prepaid();
            let charged = gone.keys().filter(|&&i| i >= prepaid).count() as u64;
            st.charged -= charged;
            st.size = len;
            st.image_len = st.image_len.min(len);
            drop(st);
            free_frames(gone.into_values().map(|p| p.frame));
            self.uncharge(charged);
            first_gone
        };
        self.unmap_from(first_gone);
        Ok(())
    }

    /// Releases the accounting of `pages` dropped pages.
    fn uncharge(&self, pages: u64) {
        match self.store {
            Store::Memory { .. } => uncharge_tmpfs(pages),
            Store::Remote { .. } if pages > 0 => memory::cache_uncharge(pages),
            Store::Remote { .. } => {}
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
            let made = match self.store {
                Store::Memory { .. } => self.create(index),
                Store::Remote { .. } => self.fetch(index),
                Store::Paged { .. } => self.wait_paged(index),
            };
            match made {
                Ok(()) => {}
                Err(ENOMEM) => return Err(Fault::Oom),
                // tmpfs is full or the server failed: no page to map, as
                // on Linux.
                Err(_) => return Err(Fault::Bus),
            }
            let mut st = self.state.lock();
            // Gone again if a truncation or reclaim came in between.
            if let Some(page) = st.pages.get_mut(&index) {
                page.referenced = true;
                memory::with_frames(|f| f.share(page.frame));
                return Ok(page.frame);
            }
        }
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
                    } else if !page.dirty && frames.refcount(page.frame) == 1 && freed + (gone.len() as u64) < want {
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
    /// dirty (a remote store: the page must be written back).
    pub fn tracks_dirty(&self) -> bool {
        self.remote_store().is_some()
    }

    /// Marks page `index` dirty before a shared mapping may store to it;
    /// false if the page is gone (truncated meanwhile).
    pub fn set_dirty(&self, index: u64) -> bool {
        let mut st = self.state.lock();
        let Some(page) = st.pages.get_mut(&index) else { return false };
        if !page.dirty {
            page.dirty = true;
            DIRTY.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// Writes the dirty pages among `pages` (page indices) back to the
    /// server. A memory store has nothing to write.
    pub fn writeback(&self, pages: core::ops::Range<u64>) -> Result<(), i64> {
        let Some((fs, ino)) = self.remote_store() else { return Ok(()) };
        let mut from = pages.start;
        loop {
            // 1. Take the dirty marks of a batch.
            let mut batch: heapless::Vec<u64, WRITEBACK_BATCH> = heapless::Vec::new();
            {
                let mut st = self.state.lock();
                for (&index, page) in st.pages.range_mut(from..pages.end) {
                    if page.dirty {
                        page.dirty = false;
                        let _ = batch.push(index);
                        if batch.is_full() {
                            break;
                        }
                    }
                }
            }
            let Some(&last) = batch.last() else { return Ok(()) };
            DIRTY.fetch_sub(batch.len() as u64, Ordering::Relaxed);
            from = last + 1;
            // 2. Make every mapping fault (and mark them again) on a store.
            self.write_protect(&batch);
            // 3. Write them, in runs of consecutive pages.
            let _io = self.io.lock();
            let mut buf = Vec::new();
            if buf.try_reserve_exact(WRITEBACK_BATCH * PAGE as usize).is_err() {
                self.redirty(&batch);
                return Err(ENOMEM);
            }
            let mut i = 0;
            while i < batch.len() {
                let mut j = i + 1;
                while j < batch.len() && batch[j] == batch[j - 1] + 1 {
                    j += 1;
                }
                let start = batch[i] * PAGE;
                buf.clear();
                {
                    let st = self.state.lock();
                    for index in &batch[i..j] {
                        // Gone or cut by a truncation meanwhile: write only
                        // what is still part of the file.
                        let Some(page) = st.pages.get(index) else { break };
                        let len = st.size.saturating_sub(index * PAGE).min(PAGE) as usize;
                        buf.extend_from_slice(&frame_bytes(page.frame)[..len]);
                        if len < PAGE as usize {
                            break;
                        }
                    }
                }
                if !buf.is_empty() && fs.write(ino, start, &buf).is_err() {
                    self.redirty(&batch[i..]);
                    return Err(EIO);
                }
                i = j;
            }
        }
    }

    /// An O_DIRECT read: what is on the server, after writing back the
    /// dirty pages of the range (as Linux does).
    pub fn read_direct(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let Some((fs, ino)) = self.remote_store() else { return self.read(off, buf) };
        let end = off.saturating_add(buf.len() as u64);
        self.writeback(page_of(off)..page_of(end.saturating_add(PAGE - 1)))?;
        fs.read(ino, off, buf)
    }

    /// Marks pages dirty again whose write-back failed.
    fn redirty(&self, pages: &[u64]) {
        for &index in pages {
            self.set_dirty(index);
        }
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

/// Writes back the dirty pages of every file (sync, the flusher).
pub fn flush_all() {
    let mut i = REMOTE.lock().len();
    while i > 0 {
        i -= 1;
        let cache = REMOTE.lock().get(i).and_then(Weak::upgrade);
        if let Some(cache) = cache {
            // A failed write keeps its pages dirty for the next round.
            let _ = cache.writeback(0..u64::MAX);
        }
    }
}

/// The flusher: a kernel thread writing dirty pages back every few
/// seconds, so stores through shared mappings reach the disk on their own.
pub fn flusher() -> ! {
    loop {
        crate::process::sched::prepare_to_sleep().sleep_until(crate::time::now() + FLUSH_INTERVAL);
        if dirty_pages() > 0 {
            flush_all();
        }
    }
}

/// Called after a store made a page dirty, with no lock held: a writer
/// that finds a fifth of the commit limit dirty writes back itself, so
/// dirty pages (which reclaim cannot drop) never crowd out memory.
pub fn balance_dirty() {
    if dirty_pages() > memory::commit_stats().1 / 5 {
        flush_all();
    }
}

/// Drops up to `want` reclaimable pages of remote caches, visiting the
/// caches in turn; returns how many it dropped. Called by `memory` when a
/// commit or a new cache page needs room, never with a cache lock held.
fn reclaim(want: u64) -> u64 {
    let mut freed = 0;
    let caches = REMOTE.lock().len();
    for _ in 0..caches {
        let cache = {
            let list = REMOTE.lock();
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
    freed
}

impl Drop for PageCache {
    fn drop(&mut self) {
        let (pages, charged) = {
            let mut st = self.state.lock();
            (core::mem::take(&mut st.pages), st.charged)
        };
        // (Only a released file or an empty cache goes: no data is lost.)
        DIRTY.fetch_sub(pages.values().filter(|p| p.dirty).count() as u64, Ordering::Relaxed);
        free_frames(pages.into_values().map(|p| p.frame));
        self.uncharge(charged);
        let prepaid = self.prepaid();
        if prepaid > 0 {
            memory::uncommit(prepaid);
        }
    }
}
