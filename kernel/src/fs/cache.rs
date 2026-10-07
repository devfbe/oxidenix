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
//! Lock order: address space (sleeping) → `io` (sleeping) → `state` →
//! frames. `io` serializes changes of contents and size (`write`,
//! `truncate`); it is never held while an address space is locked.

use crate::memory;
use crate::memory::frame::UserFrames;
use crate::process::address_space::{Fault, Mm, PAGE};
use crate::process::errno::*;
use crate::sync::{IrqSpinLock, Mutex};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame};

/// Largest file size (as `off_t` allows).
pub const MAX_SIZE: u64 = i64::MAX as u64;

/// Pages of tmpfs file contents, and their limit: half of what may be
/// committed, as Linux's default tmpfs size.
static TMPFS_PAGES: AtomicU64 = AtomicU64::new(0);
static TMPFS_LIMIT: AtomicU64 = AtomicU64::new(0);

/// Sets the tmpfs limit from the commit limit (after memory::init).
pub fn init() {
    TMPFS_LIMIT.store(memory::commit_stats().1 / 2, Ordering::Relaxed);
}

/// (pages used, page limit) of tmpfs.
pub fn tmpfs_usage() -> (u64, u64) {
    (TMPFS_PAGES.load(Ordering::Relaxed), TMPFS_LIMIT.load(Ordering::Relaxed))
}

/// Where pages come from.
enum Store {
    /// The cache is the only copy. `image` is the file's initramfs data;
    /// pages below `prepaid` were committed when the cache was created
    /// (anonymous shared memory) and are not charged again.
    Memory { image: &'static [u8], prepaid: u64 },
}

struct State {
    pages: BTreeMap<u64, PhysFrame>,
    size: u64,
    /// Bytes of `image` still valid as file contents (truncation cuts it).
    image_len: u64,
    /// Pages charged to commit and tmpfs (memory store, beyond `prepaid`).
    charged: u64,
}

pub struct PageCache {
    state: IrqSpinLock<State>,
    io: Mutex<()>,
    store: Store,
    /// Address spaces that map this file (see `register`).
    mappers: IrqSpinLock<Vec<Weak<Mm>>>,
}

/// A page charged to commit and tmpfs, released on drop unless kept.
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
            uncharge(1);
        }
    }
}

fn uncharge(pages: u64) {
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

impl PageCache {
    fn new(store: Store, size: u64, image_len: u64) -> Result<Arc<PageCache>, i64> {
        Arc::try_new(PageCache {
            state: IrqSpinLock::new(State { pages: BTreeMap::new(), size, image_len, charged: 0 }),
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
        }
    }

    fn image(&self) -> &'static [u8] {
        match self.store {
            Store::Memory { image, .. } => image,
        }
    }

    /// The initial contents of a missing page into `out` (a page).
    fn fill(&self, st: &State, index: u64, out: &mut [u8]) {
        let start = index * PAGE;
        let valid = (st.image_len.min(st.size) as usize).min(self.image().len());
        let from = (start as usize).min(valid);
        let to = ((start + PAGE) as usize).min(valid);
        out[..to - from].copy_from_slice(&self.image()[from..to]);
        out[to - from..].fill(0);
    }

    /// Creates page `index` if it is missing.
    fn ensure(&self, index: u64) -> Result<(), i64> {
        if self.state.lock().pages.contains_key(&index) {
            return Ok(());
        }
        // Charge first: committing may not happen under the state lock.
        let charge = if index >= self.prepaid() { Some(Charge::take()?) } else { None };
        let mut st = self.state.lock();
        if st.pages.contains_key(&index) {
            return Ok(());
        }
        let frame = memory::with_frames(|f| UserFrames(f).allocate_frame()).ok_or(ENOMEM)?;
        self.fill(&st, index, frame_bytes(frame));
        st.pages.insert(index, frame);
        if let Some(c) = charge {
            c.keep();
            st.charged += 1;
        }
        Ok(())
    }

    /// Reads up to `buf.len()` bytes at `off` (fewer at the end of the
    /// file). Never sleeps.
    pub fn read(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let st = self.state.lock();
        let end = off.saturating_add(buf.len() as u64).min(st.size);
        let mut pos = off;
        while pos < end {
            let (index, in_page) = (page_of(pos), (pos % PAGE) as usize);
            let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
            let out = &mut buf[(pos - off) as usize..][..n];
            match st.pages.get(&index) {
                Some(&frame) => out.copy_from_slice(&frame_bytes(frame)[in_page..in_page + n]),
                None => {
                    let mut page = [0u8; PAGE as usize];
                    self.fill(&st, index, &mut page);
                    out.copy_from_slice(&page[in_page..in_page + n]);
                }
            }
            pos += n as u64;
        }
        Ok(end.saturating_sub(off) as usize)
    }

    /// Writes `data` at `off`, growing the file.
    pub fn write(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        let end = off.checked_add(data.len() as u64).filter(|&e| e <= MAX_SIZE).ok_or(EFBIG)?;
        let _io = self.io.lock();
        let mut pos = off;
        while pos < end {
            let (index, in_page) = (page_of(pos), (pos % PAGE) as usize);
            let n = ((PAGE as usize) - in_page).min((end - pos) as usize);
            if let Err(e) = self.ensure(index) {
                // A short write if some data went in.
                return if pos == off { Err(e) } else { Ok((pos - off) as usize) };
            }
            let mut st = self.state.lock();
            let frame = *st.pages.get(&index).expect("created above, kept by io");
            if pos > st.size {
                Self::grow(&mut st, pos);
            }
            frame_bytes(frame)[in_page..in_page + n].copy_from_slice(&data[(pos - off) as usize..][..n]);
            st.size = st.size.max(pos + n as u64);
            pos += n as u64;
        }
        Ok(data.len())
    }

    /// Extends the file to `len` (> size): bytes past the old end in its
    /// last page (stores through a mapping there) become zero.
    fn grow(st: &mut State, len: u64) {
        let tail = st.size % PAGE;
        if tail != 0 {
            if let Some(&frame) = st.pages.get(&page_of(st.size)) {
                let stop = (PAGE).min(tail + (len - st.size)) as usize;
                frame_bytes(frame)[tail as usize..stop].fill(0);
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
            let mut st = self.state.lock();
            if len >= st.size {
                Self::grow(&mut st, len);
                return Ok(());
            }
            let first_gone = page_of(len + PAGE - 1);
            if len % PAGE != 0 {
                if let Some(&frame) = st.pages.get(&page_of(len)) {
                    frame_bytes(frame)[(len % PAGE) as usize..].fill(0);
                }
            }
            let gone = st.pages.split_off(&first_gone);
            let prepaid = self.prepaid();
            let charged = gone.keys().filter(|&&i| i >= prepaid).count() as u64;
            st.charged -= charged;
            st.size = len;
            st.image_len = st.image_len.min(len);
            drop(st);
            memory::with_frames(|f| {
                for frame in gone.into_values() {
                    unsafe { f.deallocate_frame(frame) };
                }
            });
            uncharge(charged);
            first_gone
        };
        self.unmap_from(first_gone);
        Ok(())
    }

    /// For a fault at page `index` of the file: its frame, with a new
    /// reference for the mapping (SIGBUS beyond the end of the file).
    pub fn map_page(&self, index: u64) -> Result<PhysFrame, Fault> {
        loop {
            if index >= page_of(self.size().saturating_add(PAGE - 1)) {
                return Err(Fault::Bus);
            }
            match self.ensure(index) {
                Ok(()) => {}
                Err(ENOMEM) => return Err(Fault::Oom),
                // tmpfs is full: no page to map, as on Linux.
                Err(_) => return Err(Fault::Bus),
            }
            let st = self.state.lock();
            // Gone again if a truncation came in between: check the size.
            if let Some(&frame) = st.pages.get(&index) {
                memory::with_frames(|f| f.share(frame));
                return Ok(frame);
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

impl Drop for PageCache {
    fn drop(&mut self) {
        let (pages, charged) = {
            let mut st = self.state.lock();
            (core::mem::take(&mut st.pages), st.charged)
        };
        memory::with_frames(|f| {
            for frame in pages.into_values() {
                unsafe { f.deallocate_frame(frame) };
            }
        });
        uncharge(charged);
        let prepaid = self.prepaid();
        if prepaid > 0 {
            memory::uncommit(prepaid);
        }
    }
}
