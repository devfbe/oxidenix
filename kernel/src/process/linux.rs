//! Restricted mode: a Linux program and the Linux server on one thread
//! (docs/design/linux-server.md; the interface is `crates/restricted`).
//!
//! Every process tree started from the kernel (`/init`, the monitor's
//! `run`) gets an **instance** of the Linux server (ADR 0002): a shared
//! region above the program's 64 TiB holding the server's program, and per
//! thread a stack and the page with the program's registers (`State`).
//! The instance's third-level table is slot 128 of the normal view of every
//! address space it serves (`AddressSpace::attach`); the program's view
//! lacks it, so the program cannot reach the server.
//!
//! A thread starts in the server (normal mode). `restricted_enter` saves
//! the server's registers here, loads the program's from `State`, switches
//! to the program's view and returns to the program. The program's
//! `syscall` comes back here (`trap`): its registers go to `State`, the
//! server's come back with the reason, and the normal view is loaded. With
//! PCIDs neither switch flushes the TLB, and the server touches neither the
//! FPU nor the FS/GS bases, so only general registers move.
//!
//! Phase R1: the server hands every system call back with `legacy_syscall`,
//! which runs the kernel's own Linux implementation on the registers in
//! `State`. Faults and exceptions of the program are still handled by the
//! kernel directly, as before.

use super::address_space::{free_level, USER_END};
use super::errno::*;
use super::address_space::PAGE;
use super::syscall::Frame;
use super::with_current;
use super::errno::SysResult;
use crate::fs::cache::PageCache;
use crate::memory::{self, frame::UserFrames};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use restricted::*;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

/// The top-level slot of the shared region.
pub const SHARED_SLOT: usize = (SHARED_BASE >> 39) as usize;

/// Flags of the tables leading to user pages.
pub fn table_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE
}

/// rflags bits a program may set: CF PF AF ZF SF TF DF OF (as sigreturn).
const USER_FLAGS: u64 = 0xcd5;
/// The server's program file, read once at boot: a restart of a process
/// tree must not run whatever was written to /sbin/linux since.
const SERVER_PATH: &str = "/sbin/linux";
/// The largest server program accepted.
const MAX_IMAGE: u64 = 16 * 1024 * 1024;

static IMAGE: spin::Once<Arc<PageCache>> = spin::Once::new();

/// Instances alive (their memory and objects not yet gone), for `settle`.
static LIVE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn live_chan() -> usize {
    &LIVE as *const _ as usize
}

/// Instances alive (a shutdown waits for them to end).
pub fn live() -> usize {
    LIVE.load(core::sync::atomic::Ordering::Acquire)
}

/// Every instance made (gone ones are pruned when a new one comes), for
/// syncs across instances.
static INSTANCES: spin::Mutex<Vec<alloc::sync::Weak<Instance>>> = spin::Mutex::new(Vec::new());
/// The last sync ticket handed out (`sync_start`).
static SYNC_TICKET: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Where `sync_wait` sleeps: woken by `sync_done` and by a pager's end.
fn sync_chan() -> usize {
    &SYNC_TICKET as *const _ as usize
}

/// The instances alive, but `except`.
fn instances(except: Option<&Instance>) -> Vec<Arc<Instance>> {
    let all = INSTANCES.lock();
    all.iter()
        .filter_map(|w| w.upgrade())
        .filter(|i| !except.is_some_and(|e| core::ptr::eq(Arc::as_ptr(i), e)))
        .collect()
}

/// Asks every instance's pager but `except`'s to write its caches back
/// and flush (`EVENT_SYNC`); the ticket to wait for (`sync_wait`).
pub fn sync_start(except: Option<&Instance>) -> u64 {
    let ticket = SYNC_TICKET.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1;
    for instance in instances(except) {
        instance.ask_sync(ticket);
    }
    ticket
}

/// Waits until every instance but `except` that `ticket` asked has
/// answered (or its pager is gone), at most until `deadline` (0: none), or
/// until the caller is being killed. Whether all did.
pub fn sync_wait(ticket: u64, except: Option<&Instance>, deadline: u64) -> bool {
    let asked = instances(except);
    loop {
        let wait = super::sched::prepare_to_wait(sync_chan());
        if asked.iter().all(|i| i.synced(ticket)) {
            return true;
        }
        if super::signal::dying() || (deadline != 0 && crate::time::now() >= deadline) {
            return false;
        }
        if deadline != 0 {
            wait.sleep_until(deadline);
        } else {
            wait.sleep();
        }
    }
}

/// Waits until every instance of the Linux server is gone (each wrote its
/// caches back and closed its channels: its pager handled `EVENT_CLOSING`
/// last), at most until `deadline`: for a shutdown, after the last program
/// ended. Whether they are.
pub fn settle(deadline: u64) -> bool {
    loop {
        let wait = super::sched::prepare_to_wait(live_chan());
        if LIVE.load(core::sync::atomic::Ordering::Acquire) == 0 {
            return true;
        }
        if crate::time::now() >= deadline {
            return false;
        }
        wait.sleep_until(deadline);
    }
}

/// Reads the Linux server's program (at boot, once the root filesystem is
/// there).
pub fn init() {
    match crate::fs::resolve("/", SERVER_PATH, true).and_then(|inode| PageCache::copy_of(&inode)) {
        Ok(image) => {
            IMAGE.call_once(|| image);
        }
        Err(e) => crate::printkln!("[linux] cannot read {} (errno {}); Linux programs cannot start", SERVER_PATH, e),
    }
}

fn table_at(frame: PhysFrame) -> &'static mut PageTable {
    unsafe { &mut *(memory::phys_to_virt(frame.start_address().as_u64()) as *mut PageTable) }
}

fn zeroed_frame() -> Result<PhysFrame, i64> {
    let frame = memory::with_frames(|f| UserFrames(f).allocate_frame()).ok_or(ENOMEM)?;
    unsafe { core::ptr::write_bytes(memory::phys_to_virt(frame.start_address().as_u64()), 0, 4096) };
    Ok(frame)
}

/// One instance of the Linux server: the page tables of its shared region
/// and its threads' areas.
pub struct Instance {
    /// Unique among every instance the kernel ever made (process groups
    /// name theirs by it: `ThreadGroup::instance`).
    pub id: u64,
    /// A top-level table with only the shared slot, to map into the region.
    view: PhysFrame,
    /// The region's third-level table.
    pdpt: PhysFrame,
    /// The server's entry point for a new thread.
    entry: u64,
    slots: spin::Mutex<Slots>,
    /// The kernel objects the server holds, by handle.
    handles: spin::Mutex<Handles>,
    /// The server's copy instruction and where its fault resumes (see
    /// `SYS_SET_USERCOPY`), once set.
    usercopy: spin::Once<(u64, u64)>,
    /// The placeholders of the server's files, by id, for readiness reports.
    files: spin::Mutex<BTreeMap<u64, alloc::sync::Weak<crate::fs::file::ServerFile>>>,
    /// Pages of paged objects that threads wait for, for the pager thread.
    pager: spin::Mutex<PagerQueue>,
    /// Address spaces of the tree's programs: when the last goes, so does
    /// the pager's process.
    programs: core::sync::atomic::AtomicUsize,
    /// The top of the server's heap, and the pages committed for it.
    heap_end: spin::Mutex<u64>,
    heap_pages: core::sync::atomic::AtomicU64,
    /// Memory objects the kernel mapped into the region (channels), by
    /// address; held while their entries change and shootdowns run.
    maps: crate::sync::Mutex<BTreeMap<u64, RegionMap>>,
    /// The address spaces showing the region (their normal view), for the
    /// shootdowns of `unmap_object`.
    spaces: spin::Mutex<Vec<Weak<super::tlb::Tlb>>>,
    /// Channels the instance holds (at most `MAX_CHANNELS`).
    channels: core::sync::atomic::AtomicUsize,
}

/// A memory object mapped into the region (`Instance::map_object`).
struct RegionMap {
    pages: u64,
    object: Arc<PageCache>,
    /// Mapped at the server's request (`SYS_MO_MAP_SERVER`), which may
    /// unmap it; the others (channels) go with their kernel object.
    server: bool,
}

/// Most channels one instance may hold.
const MAX_CHANNELS: usize = 64;

/// Events for the instance's service thread (the pager thread): pages
/// wanted from it, and server files whose last descriptor went. A page
/// request is queued once until the pager takes it; after that, a thread
/// that still waits (the page did not come, or failed) asks again.
struct PagerQueue {
    requests: alloc::collections::VecDeque<Event>,
    queued: alloc::collections::BTreeSet<(u64, u64)>,
    /// `EVENT_RELEASE` events queued so far (`SYS_EVENT_RELEASES`).
    releases: u64,
    /// The tree has no program left: the pager gets `EVENT_CLOSING`, then
    /// its process ends.
    closing: bool,
    /// `EVENT_CLOSING` was delivered.
    closing_told: bool,
    /// An `EVENT_WRITEBACK` is queued (one at a time).
    writeback_queued: bool,
    /// An `EVENT_SYNC` is queued (one at a time), for tickets up to
    /// `sync_wanted`; the pager answered those up to `sync_done`.
    sync_queued: bool,
    sync_wanted: u64,
    sync_done: u64,
    /// The pager's process is gone: no page will come any more.
    dead: bool,
    /// An `EVENT_INFLIGHT` is queued (one at a time).
    inflight_queued: bool,
}

/// A kernel object the Linux server refers to by handle.
#[derive(Clone)]
pub(super) enum Object {
    /// A memory object (pages that mappings and reads and writes share).
    Memory(Arc<PageCache>),
    /// An open file of the kernel's descriptor table, to map.
    KernelFile(Arc<crate::fs::file::OpenFile>),
    /// An open file description in flight (`KFILE_INFLIGHT`): counted in
    /// its `in_flight` while the handle lives.
    InFlight(Arc<InFlight>),
    /// An inode of the kernel's tree (`super::linux_inode`).
    Inode(Arc<crate::fs::Inode>),
    /// The contents of a file of the server's (a tmpfs file object), with
    /// the hold this handle carries (`SYS_MO_HOLD`).
    File(Arc<PageCache>, Option<Arc<Record>>),
    /// The initramfs of the boot image, read-only (`SYS_INITRAMFS`).
    Image(&'static [u8]),
    /// The client's end of a channel to a device server (`SYS_CHAN_CREATE`).
    Channel(Arc<super::channel::ClientEnd>),
}

/// A reference to an open file description that is a descriptor in
/// flight.
pub(super) struct InFlight(Arc<crate::fs::file::OpenFile>);

impl InFlight {
    fn new(file: Arc<crate::fs::file::OpenFile>) -> InFlight {
        file.in_flight.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        InFlight(file)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
    }
}

/// Most handles one instance may hold.
const MAX_HANDLES: usize = 64 * 1024;

struct Handles {
    next: u64,
    objects: BTreeMap<u64, Object>,
}

/// Thread areas: mapped once, reused after their thread ended.
struct Slots {
    next: u64,
    free: Vec<u64>,
    /// The `State` page of each area, for the kernel to reach directly.
    states: BTreeMap<u64, PhysFrame>,
}

impl Instance {
    /// A new instance with the server's program loaded.
    pub fn new() -> Result<Arc<Instance>, i64> {
        let image = IMAGE.get().ok_or(ENOEXEC)?;
        let view = zeroed_frame()?;
        let pdpt = match zeroed_frame() {
            Ok(f) => f,
            Err(e) => {
                memory::with_frames(|f| unsafe { f.deallocate_frame(view) });
                return Err(e);
            }
        };
        table_at(view)[SHARED_SLOT].set_frame(pdpt, table_flags());
        // From here on, dropping the instance frees what was mapped.
        static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        let mut instance = Instance {
            id: NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
            view,
            pdpt,
            entry: 0,
            slots: spin::Mutex::new(Slots { next: 0, free: Vec::new(), states: BTreeMap::new() }),
            handles: spin::Mutex::new(Handles { next: 1, objects: BTreeMap::new() }),
            usercopy: spin::Once::new(),
            files: spin::Mutex::new(BTreeMap::new()),
            pager: spin::Mutex::new(PagerQueue {
                requests: alloc::collections::VecDeque::new(),
                queued: alloc::collections::BTreeSet::new(),
                releases: 0,
                closing: false,
                closing_told: false,
                writeback_queued: false,
                sync_queued: false,
                sync_wanted: 0,
                sync_done: 0,
                dead: false,
                inflight_queued: false,
            }),
            programs: core::sync::atomic::AtomicUsize::new(0),
            heap_end: spin::Mutex::new(HEAP_BASE),
            heap_pages: core::sync::atomic::AtomicU64::new(0),
            maps: crate::sync::Mutex::new(BTreeMap::new()),
            spaces: spin::Mutex::new(Vec::new()),
            channels: core::sync::atomic::AtomicUsize::new(0),
        };
        instance.entry = instance.load(image)?;
        // Counted from here on (its drop uncounts it).
        LIVE.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        let instance = Arc::try_new(instance).map_err(|_| ENOMEM)?;
        let mut all = INSTANCES.lock();
        all.retain(|w| w.strong_count() > 0);
        all.try_reserve(1).map_err(|_| ENOMEM)?;
        all.push(Arc::downgrade(&instance));
        drop(all);
        Ok(instance)
    }

    /// Asks the pager for `EVENT_SYNC` (ticket `ticket`).
    fn ask_sync(&self, ticket: u64) {
        {
            let mut q = self.pager.lock();
            if q.dead {
                return;
            }
            q.sync_wanted = q.sync_wanted.max(ticket);
            if q.sync_queued {
                return;
            }
            q.sync_queued = true;
            q.requests.push_back(Event { kind: EVENT_SYNC, a: 0, b: 0 });
        }
        super::wakeup(self.pager_chan());
    }

    /// Whether the pager answered `ticket` (or was never asked for it, or
    /// is gone).
    fn synced(&self, ticket: u64) -> bool {
        let q = self.pager.lock();
        q.dead || q.sync_done >= ticket || q.sync_wanted < ticket
    }

    pub fn pdpt(&self) -> PhysFrame {
        self.pdpt
    }

    fn map(&self, addr: u64, frame: PhysFrame, flags: PageTableFlags) -> Result<(), i64> {
        // Only the shared slot is part of the views.
        assert!((SHARED_BASE..SHARED_END).contains(&addr), "outside the shared region");
        let mut mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        let flags = flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let mapped = memory::with_frames(|f| {
            let mut user = UserFrames(f);
            unsafe { mapper.map_to_with_table_flags(page, frame, flags, table_flags(), &mut user) }.map(|m| m.ignore())
        });
        mapped.map_err(|_| {
            memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
            ENOMEM
        })
    }

    /// Maps the server's program into the region; returns its entry point.
    fn load(&self, image: &PageCache) -> Result<u64, i64> {
        let size = image.size();
        if size > MAX_IMAGE {
            return Err(ENOEXEC);
        }
        let mut data = Vec::new();
        data.try_reserve_exact(size as usize).map_err(|_| ENOMEM)?;
        data.resize(size as usize, 0);
        if image.read(0, &mut data)? != data.len() {
            return Err(EIO);
        }
        let elf = super::elf::Elf::parse(&data).map_err(|_| ENOEXEC)?;
        // Pages by address: the frame, and whether any segment touching
        // the page is writable or executable.
        let mut pages: BTreeMap<u64, (PhysFrame, bool, bool)> = BTreeMap::new();
        let result = (|| {
            for ph in elf.program_headers().filter(|p| p.kind == super::elf::PT_LOAD) {
                let end = ph.vaddr.checked_add(ph.memsz).ok_or(ENOEXEC)?;
                let file_end = ph.offset.checked_add(ph.filesz).ok_or(ENOEXEC)?;
                if ph.vaddr < IMAGE_BASE || end > MAPS_BASE || file_end > size || ph.filesz > ph.memsz {
                    return Err(ENOEXEC);
                }
                let (w, x) = (ph.flags & super::elf::PF_W != 0, ph.flags & super::elf::PF_X != 0);
                let mut page = ph.vaddr & !0xfff;
                while page < end {
                    let (frame, pw, px) = match pages.get(&page) {
                        Some(&e) => e,
                        None => (zeroed_frame()?, false, false),
                    };
                    pages.insert(page, (frame, pw || w, px || x));
                    // The file's bytes within this page.
                    let from = page.max(ph.vaddr);
                    let to = (page + 4096).min(ph.vaddr + ph.filesz);
                    if from < to {
                        let src = (ph.offset + (from - ph.vaddr)) as usize;
                        let dst = memory::phys_to_virt(frame.start_address().as_u64() + (from - page));
                        unsafe { core::ptr::copy_nonoverlapping(data[src..].as_ptr(), dst, (to - from) as usize) };
                    }
                    page += 4096;
                }
            }
            Ok(())
        })();
        let mapped: Result<(), i64> = result.and_then(|_| {
            let mut all = core::mem::take(&mut pages).into_iter();
            for (page, (frame, w, x)) in all.by_ref() {
                let mut flags = PageTableFlags::empty();
                if w {
                    flags |= PageTableFlags::WRITABLE;
                }
                if !x {
                    flags |= PageTableFlags::NO_EXECUTE;
                }
                if let Err(e) = self.map(page, frame, flags) {
                    // The rest were never mapped: free them here.
                    for (_, (frame, _, _)) in all {
                        memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
                    }
                    return Err(e);
                }
            }
            Ok(())
        });
        for (_, (frame, _, _)) in pages {
            memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
        }
        mapped?;
        if elf.entry < IMAGE_BASE || elf.entry >= MAPS_BASE {
            return Err(ENOEXEC);
        }
        Ok(elf.entry)
    }

    /// A thread area: (its number, its `State` page).
    fn thread(&self) -> Result<(u64, PhysFrame), i64> {
        let mut slots = self.slots.lock();
        if let Some(n) = slots.free.pop() {
            return Ok((n, slots.states[&n]));
        }
        let n = slots.next;
        if n >= MAX_THREADS {
            return Err(EAGAIN);
        }
        // Pages mapped by an attempt that failed half-way are kept.
        let stack = thread_stack_top(n) - THREAD_STACK;
        for page in (stack..thread_stack_top(n)).step_by(4096) {
            self.ensure(page)?;
        }
        let state = self.ensure(thread_state(n))?;
        slots.next = n + 1;
        slots.states.insert(n, state);
        Ok((n, state))
    }

    /// The frame of the data page at `addr`, mapped (zeroed) if missing.
    fn ensure(&self, addr: u64) -> Result<PhysFrame, i64> {
        let mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        if let Ok(frame) = mapper.translate_page(page) {
            return Ok(frame);
        }
        let frame = zeroed_frame()?;
        self.map(addr, frame, PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE)?;
        Ok(frame)
    }

    /// Grows the server's heap by `len` bytes (rounded up to pages):
    /// zeroed, committed memory. Returns where it starts.
    fn grow_heap(&self, len: u64) -> Result<u64, i64> {
        let len = len.checked_add(PAGE - 1).ok_or(EINVAL)? & !(PAGE - 1);
        let mut end = self.heap_end.lock();
        let start = *end;
        if len == 0 || start.checked_add(len).is_none_or(|e| e > THREADS_BASE) {
            return Err(ENOMEM);
        }
        let pages = len / PAGE;
        if !memory::commit(pages) {
            return Err(ENOMEM);
        }
        self.heap_pages.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
        for addr in (start..start + len).step_by(PAGE as usize) {
            // Mapped pages stay (the instance frees them); the rest of the
            // commit is given back below with the failure.
            if let Err(e) = self.ensure(addr) {
                *end = addr;
                let left = (start + len - addr) / PAGE;
                self.heap_pages.fetch_sub(left, core::sync::atomic::Ordering::Relaxed);
                memory::uncommit(left);
                return Err(e);
            }
        }
        *end = start + len;
        Ok(start)
    }

    /// The word at `addr` of the server's own memory (mapped, 4-aligned),
    /// which stays mapped as long as the instance lives. Not in the range
    /// of the objects mapped into the region (`object_word`): those come
    /// and go.
    fn word(&self, addr: u64) -> Result<&core::sync::atomic::AtomicU32, i64> {
        if (MAPS_BASE..HEAP_BASE).contains(&addr) {
            return Err(EFAULT);
        }
        self.translate_word(addr)
    }

    /// The word at `addr` of the region, mapped or not; the caller keeps
    /// what is mapped there.
    fn translate_word(&self, addr: u64) -> Result<&core::sync::atomic::AtomicU32, i64> {
        if addr % 4 != 0 || !(SHARED_BASE..SHARED_END).contains(&addr) {
            return Err(EINVAL);
        }
        let mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        let frame = mapper.translate_page(page).map_err(|_| EFAULT)?;
        let phys = frame.start_address().as_u64() + addr % PAGE;
        Ok(unsafe { &*(memory::phys_to_virt(phys) as *const core::sync::atomic::AtomicU32) })
    }

    /// An address space whose normal view shows the region appeared.
    pub fn add_space(&self, tlb: &Arc<super::tlb::Tlb>) -> Result<(), super::address_space::Fault> {
        let mut spaces = self.spaces.lock();
        spaces.retain(|t| t.strong_count() > 0);
        spaces.try_reserve(1).map_err(|_| super::address_space::Fault::Oom)?;
        spaces.push(Arc::downgrade(tlb));
        Ok(())
    }

    /// Maps the first `pages` pages of `object` into the region
    /// (`MAPS_BASE..HEAP_BASE`), the first `read_only` of them read-only,
    /// the rest writable; returns where. Each entry holds a reference on
    /// its frame, the region's entry the object.
    pub(super) fn map_object(&self, object: &Arc<PageCache>, pages: u64, read_only: u64) -> Result<u64, i64> {
        self.map_region(object, pages, read_only, false)
    }

    fn map_region(&self, object: &Arc<PageCache>, pages: u64, read_only: u64, server: bool) -> Result<u64, i64> {
        let len = pages.checked_mul(PAGE).filter(|&l| l > 0).ok_or(EINVAL)?;
        let mut maps = self.maps.lock();
        // First fit.
        let mut start = MAPS_BASE;
        for (&at, m) in maps.iter() {
            if at - start >= len {
                break;
            }
            start = at + m.pages * PAGE;
        }
        if start.checked_add(len).is_none_or(|e| e > HEAP_BASE) {
            return Err(ENOMEM);
        }
        maps.insert(start, RegionMap { pages, object: object.clone(), server });
        for i in 0..pages {
            let flags = if i < read_only { PageTableFlags::NO_EXECUTE } else { PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE };
            let mapped = object.map_page(i).map_err(|_| ENOMEM).and_then(|frame| self.map(start + i * PAGE, frame, flags));
            if let Err(e) = mapped {
                self.unmap_pages(start, i);
                maps.remove(&start);
                return Err(e);
            }
        }
        Ok(start)
    }

    /// Removes the object mapped at `addr` by `map_object`.
    pub(super) fn unmap_object(&self, addr: u64) {
        let mut maps = self.maps.lock();
        if let Some(m) = maps.remove(&addr) {
            // Under the lock: the range is not handed out again before
            // every TLB dropped it.
            self.unmap_pages(addr, m.pages);
        }
    }

    /// `SYS_MO_UNMAP_SERVER`: removes an object the server mapped itself
    /// (EINVAL for anything else at `addr`).
    fn unmap_server_object(&self, addr: u64) -> Result<(), i64> {
        let mut maps = self.maps.lock();
        if !maps.get(&addr).is_some_and(|m| m.server) {
            return Err(EINVAL);
        }
        let m = maps.remove(&addr).expect("checked");
        self.unmap_pages(addr, m.pages);
        Ok(())
    }

    /// Removes the entries of `pages` pages at `start`, drops them from the
    /// TLBs of every address space showing the region, then lets go of the
    /// frames.
    fn unmap_pages(&self, start: u64, pages: u64) {
        let mut mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let mut frames = Vec::new();
        // Without room to collect the frames, one page at a time.
        let batch = frames.try_reserve_exact(pages as usize).is_ok();
        for i in 0..pages {
            let at = start + i * PAGE;
            if let Ok((frame, flush)) = mapper.unmap(Page::<Size4KiB>::containing_address(VirtAddr::new(at))) {
                flush.ignore();
                if batch {
                    frames.push(frame);
                } else {
                    self.shootdown(at, at + PAGE);
                    memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
                }
            }
        }
        if batch {
            self.shootdown(start, start + pages * PAGE);
        }
        memory::with_frames(|f| {
            for frame in frames {
                unsafe { f.deallocate_frame(frame) };
            }
        });
    }

    fn shootdown(&self, start: u64, end: u64) {
        let spaces: Vec<Arc<super::tlb::Tlb>> = self.spaces.lock().iter().filter_map(Weak::upgrade).collect();
        for tlb in spaces {
            super::tlb::shootdown(&tlb, start, end);
        }
    }

    /// The object mapped by `map_object` at `addr`, the offset of `addr` in
    /// it, and the word there (valid while the object is kept).
    fn object_word(&self, addr: u64) -> Option<(Arc<PageCache>, u64, &core::sync::atomic::AtomicU32)> {
        if !(MAPS_BASE..HEAP_BASE).contains(&addr) {
            return None;
        }
        let maps = self.maps.lock();
        let (&start, m) = maps.range(..=addr).next_back()?;
        if addr >= start + m.pages * PAGE {
            return None;
        }
        // Translated under the lock: the entry is still the object's page,
        // which the object (returned with it) keeps.
        let word = self.translate_word(addr).ok()?;
        Some((m.object.clone(), addr - start, word))
    }

    /// Counts a new channel of the instance (EMFILE beyond `MAX_CHANNELS`).
    pub(super) fn channel_added(&self) -> Result<(), i64> {
        use core::sync::atomic::Ordering::Relaxed;
        self.channels.try_update(Relaxed, Relaxed, |n| (n < MAX_CHANNELS).then_some(n + 1)).map(|_| ()).map_err(|_| EMFILE)
    }

    pub(super) fn channel_gone(&self) {
        self.channels.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// A new handle for `object`.
    pub(super) fn insert(&self, object: Object) -> Result<u64, i64> {
        let mut h = self.handles.lock();
        if h.objects.len() >= MAX_HANDLES {
            return Err(EMFILE);
        }
        let handle = h.next;
        h.next += 1;
        h.objects.insert(handle, object);
        Ok(handle)
    }

    pub(super) fn object(&self, handle: u64) -> Result<Object, i64> {
        self.handles.lock().objects.get(&handle).cloned().ok_or(EBADF)
    }

    fn channel(&self, handle: u64) -> Result<Arc<super::channel::ClientEnd>, i64> {
        match self.object(handle)? {
            Object::Channel(c) => Ok(c),
            _ => Err(EINVAL),
        }
    }

    fn memory(&self, handle: u64) -> Result<Arc<PageCache>, i64> {
        match self.object(handle)? {
            Object::Memory(m) | Object::File(m, _) => Ok(m),
            _ => Err(EINVAL),
        }
    }

    /// Where the pager thread waits (odd: never a pointer channel).
    fn pager_chan(&self) -> usize {
        (self as *const Instance as usize) | 1
    }

    /// Where threads waiting for the pager's pages sleep.
    fn answer_chan(&self) -> usize {
        (self as *const Instance as usize) | 3
    }

    /// The pager's process ended: what is waited for will not come (its
    /// pending pages go, so nobody waits for them either).
    fn pager_gone(&self) {
        let mut q = self.pager.lock();
        q.dead = true;
        q.requests.clear();
        q.queued.clear();
        drop(q);
        crate::fs::cache::pager_gone(self as *const Instance as *const ());
        super::wakeup(self.answer_chan());
        super::wakeup(sync_chan());
    }

    /// An address space of one of the tree's programs appeared.
    pub fn program_added(&self) {
        self.programs.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// An address space of a program went; with the last one, the pager's
    /// process ends.
    pub fn program_gone(&self) {
        if self.programs.fetch_sub(1, core::sync::atomic::Ordering::AcqRel) == 1 {
            self.close();
        }
    }

    /// Ends the pager's process (no program will ask it for pages).
    pub fn close(&self) {
        self.pager.lock().closing = true;
        super::wakeup(self.pager_chan());
    }

    fn release(&self, n: u64) {
        self.slots.lock().free.push(n);
    }
}

impl crate::fs::cache::Pager for Instance {
    fn request(&self, key: u64, index: u64) -> bool {
        let mut q = self.pager.lock();
        if q.dead {
            return false;
        }
        if q.queued.insert((key, index)) {
            q.requests.push_back(Event { kind: EVENT_PAGE, a: key, b: index * PAGE });
            drop(q);
            super::wakeup(self.pager_chan());
        }
        true
    }

    fn wait_chan(&self) -> usize {
        self.answer_chan()
    }

    fn alive(&self) -> bool {
        !self.pager.lock().dead
    }

    fn dirty(&self, key: u64) {
        self.queue_event(Event { kind: EVENT_DIRTY, a: key, b: 0 });
    }

    fn mkwrite(&self, key: u64, index: u64) -> bool {
        {
            let mut q = self.pager.lock();
            if q.dead {
                return false;
            }
            // (Once per page until answered: `Page::mkwrite`.)
            q.requests.push_back(Event { kind: EVENT_MKWRITE, a: key, b: index * PAGE });
        }
        super::wakeup(self.pager_chan());
        true
    }

    fn writeback(&self, pages: u64) {
        {
            let mut q = self.pager.lock();
            if q.dead || q.closing || q.writeback_queued {
                return;
            }
            q.writeback_queued = true;
            q.requests.push_back(Event { kind: EVENT_WRITEBACK, a: pages, b: 0 });
        }
        super::wakeup(self.pager_chan());
    }
}

impl crate::fs::file::ServerFiles for Instance {
    fn closed(&self, id: u64) {
        self.files.lock().remove(&id);
        // Closed by this instance's thread in a pass-through call: the
        // server learns it when the call returns (close is then complete
        // for the other end, as with the kernel's own pipes).
        let mine = with_current(|p| match p.linux.as_mut() {
            Some(l) if l.in_legacy && core::ptr::eq(Arc::as_ptr(&l.instance), self) => {
                l.closed_now.push(id);
                true
            }
            _ => false,
        });
        if !mine {
            self.queue_closed(id);
        }
    }

    fn in_flight_reference_gone(&self) {
        let mut q = self.pager.lock();
        if q.dead || q.closing || q.inflight_queued {
            return;
        }
        q.inflight_queued = true;
        q.requests.push_back(Event { kind: EVENT_INFLIGHT, a: 0, b: 0 });
        drop(q);
        super::wakeup(self.pager_chan());
    }
}

impl Instance {
    /// Tells the service thread that the server file `id` lost its last
    /// descriptor.
    fn queue_closed(&self, id: u64) {
        self.queue_event(Event { kind: EVENT_CLOSED, a: id, b: 0 });
    }

    /// Queues an event for the service thread (none once it is gone or
    /// going: nobody would take it).
    fn queue_event(&self, event: Event) {
        let mut q = self.pager.lock();
        if q.dead || q.closing {
            return;
        }
        if event.kind == EVENT_RELEASE {
            q.releases += 1;
        }
        q.requests.push_back(event);
        drop(q);
        super::wakeup(self.pager_chan());
    }
}

/// A program the server resolved for an execve.
pub enum ExecTarget {
    /// An inode of the kernel's tree.
    Inode(Arc<crate::fs::Inode>),
    /// A file object of the server's and the hold the program keeps.
    File(Arc<PageCache>, Option<Arc<Record>>),
}

/// A record of the server's (`SYS_FS_RECORD`) held by a kernel object: the
/// server learns when the last holder is gone (`EVENT_RELEASE`).
pub struct Record {
    instance: Weak<Instance>,
    word: u64,
}

impl Record {
    pub fn word(&self) -> u64 {
        self.word
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        if let Some(instance) = self.instance.upgrade() {
            instance.queue_event(Event { kind: EVENT_RELEASE, a: self.word, b: 0 });
        }
    }
}

impl Drop for Instance {
    /// No address space shows the region any more: its frames go.
    fn drop(&mut self) {
        memory::uncommit(self.heap_pages.load(core::sync::atomic::Ordering::Relaxed));
        memory::with_frames(|frames| unsafe {
            free_level(frames, self.pdpt, 3);
            frames.deallocate_frame(self.view);
        });
        if self.entry != 0 {
            LIVE.fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
            super::wakeup(live_chan());
        }
    }
}

/// A thread of a Linux program as restricted mode sees it.
pub struct LinuxThread {
    instance: Arc<Instance>,
    slot: u64,
    state: PhysFrame,
    /// Running the program (true) or the server.
    pub restricted: bool,
    /// A service thread of the instance (the pager or the worker), which
    /// serves no program.
    pager: bool,
    /// The pager, which takes the instance's events.
    events: bool,
    /// The server waits in `legacy_syscall`: the kernel runs the call in
    /// the program's view.
    in_legacy: bool,
    /// The system call the program trapped with, while the server handles
    /// it itself: signal delivery after it restarts it as the kernel's
    /// own handling would (taken by a pass-through, which delivers itself).
    trap_nr: Option<u64>,
    /// Server files whose last descriptor this thread's pass-through call
    /// closed: handed to the server when the call returns.
    closed_now: Vec<u64>,
    /// The server's files its `kfd_lookup`s found during the system call
    /// it handles: kept until it enters the program again (Linux's fdget),
    /// so that another thread's close cannot take a file from under a call
    /// that uses it.
    pinned: Vec<Arc<crate::fs::file::OpenFile>>,
    /// The record for the working-directory context the thread's next
    /// pass-through clone creates (`FS_CHILD`).
    pub fs_child: Option<Record>,
    /// The program the thread's next pass-through execve runs, as the
    /// server resolved it, with its absolute path (`SYS_EXEC_TARGET`).
    pub exec_target: Option<(ExecTarget, alloc::string::String)>,
    /// The server's registers while the program runs.
    normal: Frame,
}

impl LinuxThread {
    /// A thread of `instance` whose program starts with the registers of
    /// `program`; returns it and the frame the thread starts with (the
    /// server's entry, on the thread's server stack).
    pub fn new(instance: Arc<Instance>, program: &Frame) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state) = instance.thread()?;
        let thread =
            LinuxThread { instance, slot, state, restricted: false, pager: false, events: false, in_legacy: false, trap_nr: None, closed_now: Vec::new(), pinned: Vec::new(), fs_child: None, exec_target: None, normal: Frame::default() };
        save(program, thread.state());
        let start = thread.start(ROLE_PROGRAM);
        Ok((thread, start))
    }

    /// The worker thread of `instance` (a second service thread, in the
    /// pager's process, that serves no program and no page: the server's
    /// collector of sockets in flight runs there), and its frame.
    pub fn worker(instance: Arc<Instance>) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state) = instance.thread()?;
        let thread =
            LinuxThread { instance, slot, state, restricted: false, pager: true, events: false, in_legacy: false, trap_nr: None, closed_now: Vec::new(), pinned: Vec::new(), fs_child: None, exec_target: None, normal: Frame::default() };
        let start = thread.start(ROLE_WORKER);
        Ok((thread, start))
    }

    /// The pager thread of `instance`, and the frame it starts with.
    pub fn pager(instance: Arc<Instance>) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state) = instance.thread()?;
        let thread =
            LinuxThread { instance, slot, state, restricted: false, pager: true, events: true, in_legacy: false, trap_nr: None, closed_now: Vec::new(), pinned: Vec::new(), fs_child: None, exec_target: None, normal: Frame::default() };
        let start = thread.start(ROLE_PAGER);
        Ok((thread, start))
    }

    /// The server's entry for this thread: on its stack, with its `State`
    /// and its role.
    fn start(&self, role: u64) -> Frame {
        // At a function's entry the stack is 8 bytes off 16-byte alignment.
        let mut start = Frame::user_start(self.instance.entry, thread_stack_top(self.slot) - 8);
        start.rdi = thread_state(self.slot);
        start.rsi = role;
        start
    }

    /// Whether the thread's CPU shows the normal view (the server runs).
    pub fn normal_view(&self) -> bool {
        !self.restricted && !self.in_legacy
    }

    #[allow(clippy::mut_from_ref)]
    fn state(&self) -> &mut State {
        unsafe { &mut *(memory::phys_to_virt(self.state.start_address().as_u64()) as *mut State) }
    }
}

impl Drop for LinuxThread {
    fn drop(&mut self) {
        // Closed in a call that never returned (the thread exited in it).
        for id in core::mem::take(&mut self.closed_now) {
            self.instance.queue_closed(id);
        }
        if self.events {
            self.instance.pager_gone();
        }
        // Pins of a call that never returned.
        for file in core::mem::take(&mut self.pinned) {
            crate::fs::file::release(file);
        }
        self.instance.release(self.slot);
    }
}

fn save(f: &Frame, s: &mut State) {
    *s = State {
        rax: f.rax,
        rbx: f.rbx,
        rcx: f.rcx,
        rdx: f.rdx,
        rsi: f.rsi,
        rdi: f.rdi,
        rbp: f.rbp,
        r8: f.r8,
        r9: f.r9,
        r10: f.r10,
        r11: f.r11,
        r12: f.r12,
        r13: f.r13,
        r14: f.r14,
        r15: f.r15,
        rip: f.rip,
        rflags: f.rflags,
        rsp: f.rsp,
    };
}

/// The program's registers from `State`, which the server may have
/// changed: a return to user mode below 64 TiB with harmless flags.
fn load(s: &State, f: &mut Frame) -> Result<(), i64> {
    if s.rip >= USER_END || s.rsp >= USER_END {
        return Err(EINVAL);
    }
    let mut frame = Frame::user_start(s.rip, s.rsp);
    frame.rax = s.rax;
    frame.rbx = s.rbx;
    frame.rcx = s.rcx;
    frame.rdx = s.rdx;
    frame.rsi = s.rsi;
    frame.rdi = s.rdi;
    frame.rbp = s.rbp;
    frame.r8 = s.r8;
    frame.r9 = s.r9;
    frame.r10 = s.r10;
    frame.r11 = s.r11;
    frame.r12 = s.r12;
    frame.r13 = s.r13;
    frame.r14 = s.r14;
    frame.r15 = s.r15;
    frame.rflags = (s.rflags & USER_FLAGS) | 0x202;
    *f = frame;
    Ok(())
}

/// The calling thread's place: None if it is not a Linux thread, else
/// whether it runs the program (true) or the server.
pub fn mode() -> Option<bool> {
    with_current(|p| p.linux.as_ref().map(|l| l.restricted))
}

/// Whether the calling thread runs its Linux server, with the normal view
/// loaded (not in a legacy call).
pub fn in_server() -> bool {
    with_current(|p| p.linux.as_ref().is_some_and(|l| l.normal_view()))
}

/// Whether the calling thread is a Linux server instance's pager thread.
pub fn is_pager() -> bool {
    with_current(|p| p.linux.as_ref().is_some_and(|l| l.pager))
}

/// Whether the caller is the instance's pager, which takes its events.
fn owns_events() -> bool {
    with_current(|p| p.linux.as_ref().is_some_and(|l| l.events))
}

fn instance() -> Result<Arc<Instance>, i64> {
    with_current(|p| p.linux.as_ref().map(|l| l.instance.clone())).ok_or(EPERM)
}

/// The kernel calls of the Linux server beyond entering restricted mode
/// (see `restricted::SYS_*`). Mappings go into the calling thread's
/// program view.
pub fn server_call(nr: u64, a: [u64; 6]) -> SysResult {
    use super::address_space::{Backing, Prot};
    let instance = instance()?;
    let page_aligned = |x: u64| x % PAGE == 0;
    // A range of the program's memory: page-aligned, below 64 TiB.
    let range = |addr: u64, len: u64| -> Result<u64, i64> {
        let len = len.checked_add(PAGE - 1).ok_or(EINVAL)? & !(PAGE - 1);
        if !page_aligned(addr) || len == 0 || addr.checked_add(len).is_none_or(|e| e > USER_END) {
            return Err(EINVAL);
        }
        Ok(len)
    };
    let prot = |bits: u64| if bits & !7 != 0 { Err(EINVAL) } else { Ok(Prot::from_bits(bits)) };
    let mm = || super::current_mm().ok_or(EINVAL);
    match nr {
        SYS_HANDLE_CLOSE => {
            let object = instance.handles.lock().objects.remove(&a[0]).ok_or(EBADF)?;
            // Released outside the lock. A channel's end goes now, unless a
            // call of another thread still uses it (then with that call).
            if let Object::Channel(end) = object {
                if let Ok(end) = Arc::try_unwrap(end) {
                    end.close();
                }
            }
            Ok(0)
        }
        SYS_MO_CREATE => {
            let pages = a[0];
            if pages == 0 || pages > USER_END / PAGE {
                return Err(EINVAL);
            }
            let object = PageCache::anonymous(pages).map_err(|_| ENOMEM)?;
            Ok(instance.insert(Object::Memory(object))? as i64)
        }
        SYS_MO_MAP => {
            use super::sys_mem::{anon_backing, file_backing, place_and_map, Placement};
            let (handle, addr, offset, flags) = (a[0], a[1], a[3], a[5]);
            let prot = prot(a[4])?;
            let all = MO_SHARED | MO_FIXED | MO_NOREPLACE | MO_NORESERVE | MO_POPULATE | MO_READONLY;
            let may_write = flags & MO_READONLY == 0;
            if !page_aligned(addr) || !page_aligned(offset) || flags & !all != 0 {
                return Err(EINVAL);
            }
            let len = a[2].checked_add(PAGE - 1).ok_or(EINVAL)? & !(PAGE - 1);
            if len == 0 || len >= USER_END || offset.checked_add(len).is_none() {
                return Err(EINVAL);
            }
            let shared = flags & MO_SHARED != 0;
            let backing = match handle {
                0 if shared => return Err(EINVAL),
                0 => anon_backing(false, len)?,
                h => match instance.object(h)? {
                    Object::Memory(cache) => {
                        offset.checked_add(len).filter(|&e| e <= cache.size()).ok_or(EINVAL)?;
                        Backing::File { cache, offset, shared, may_write, _hold: None }
                    }
                    Object::KernelFile(f) => file_backing(&f, shared, prot, len, offset)?,
                    Object::Inode(_) | Object::Image(_) | Object::Channel(_) | Object::InFlight(_) => return Err(EINVAL),
                    // As a file's mapping: it may reach beyond the end (SIGBUS
                    // there), and keeps the handle's hold while it exists.
                    Object::File(cache, hold) => {
                        let hold: Option<super::address_space::Hold> = hold.map(|h| h as _);
                        Backing::File { cache, offset, shared, may_write, _hold: hold }
                    }
                },
            };
            let placement = Placement {
                fixed: flags & (MO_FIXED | MO_NOREPLACE) != 0,
                no_replace: flags & MO_NOREPLACE != 0,
                no_reserve: flags & MO_NORESERVE != 0,
                populate: flags & MO_POPULATE != 0,
            };
            place_and_map(addr, len, prot, backing, placement)
        }
        SYS_KFILE_OBJECT => {
            if a[1] & !KFILE_INFLIGHT != 0 {
                return Err(EINVAL);
            }
            let f = with_current(|p| p.file(a[0]))?;
            let object = if a[1] & KFILE_INFLIGHT != 0 { Object::InFlight(Arc::new(InFlight::new(f))) } else { Object::KernelFile(f) };
            Ok(instance.insert(object)? as i64)
        }
        SYS_VM_REMAP => super::sys_mem::mremap(a[0], a[1], a[2], a[3], a[4]),
        SYS_VM_DISCARD => super::sys_mem::madvise(a[0], a[1], 4),
        SYS_VM_SYNC => super::sys_mem::msync_server(a[0], a[1], a[2], a[3], a[4]),
        SYS_MO_CREATE_PAGED => {
            let pages = a[0];
            if pages == 0 || pages > USER_END / PAGE {
                return Err(EINVAL);
            }
            let pager: alloc::sync::Weak<dyn crate::fs::cache::Pager> = Arc::downgrade(&instance) as _;
            let object = PageCache::paged(pages, pager, a[1])?;
            Ok(instance.insert(Object::Memory(object))? as i64)
        }
        SYS_EVENT_WAIT => match event_wait(&instance, a[0], a[1]) {
            Some(result) => result,
            None => {
                // The tree has no program left: the pager's process ends.
                // No reference of this call may stay behind on its stack
                // (exit_group does not return): the instance goes with its
                // last holder.
                drop(instance);
                super::exit_group(0)
            }
        },
        SYS_EVENT_RELEASES => Ok(instance.pager.lock().releases as i64),
        SYS_SYNC_OTHERS => {
            if is_pager() {
                return Err(EPERM);
            }
            if a[0] == 0 {
                return Ok(sync_start(Some(&instance)) as i64);
            }
            sync_wait(a[0], Some(&instance), 0);
            Ok(0)
        }
        SYS_SYNC_DONE => {
            if !owns_events() {
                return Err(EPERM);
            }
            let mut q = instance.pager.lock();
            q.sync_done = q.sync_done.max(a[0].min(q.sync_wanted));
            drop(q);
            super::wakeup(sync_chan());
            Ok(0)
        }
        SYS_KFD_INSTALL => {
            use crate::fs::file::{OpenFile, ServerFile, Kind, O_ACCMODE, O_APPEND, O_CLOEXEC, O_NONBLOCK};
            let (id, flags, ready, kind) = (a[0], a[1] as u32, a[2] as i16, a[3]);
            if id == 0 || flags & !(O_ACCMODE | O_NONBLOCK | O_APPEND | O_CLOEXEC) != 0 || kind & !KFD_ALWAYS_READY != 0 {
                return Err(EINVAL);
            }
            let owner: alloc::sync::Weak<dyn crate::fs::file::ServerFiles> = Arc::downgrade(&instance) as _;
            let placeholder = ServerFile::new(id, owner, ready, kind == KFD_ALWAYS_READY);
            instance.files.lock().insert(id, Arc::downgrade(&placeholder));
            let file = OpenFile::new(Kind::Server(placeholder), flags, None);
            with_current(|p| p.alloc_fd(file, flags & O_CLOEXEC != 0, 0))
        }
        SYS_KFD_LOOKUP => {
            let file = with_current(|p| p.file(a[0]))?;
            let crate::fs::file::Kind::Server(s) = &file.kind else { return Ok(0) };
            // Another instance's file (a descriptor that crossed trees)
            // must never be taken for one of this instance's ids.
            if !s.owned_by(Arc::as_ptr(&instance) as *const ()) {
                return Err(EBADF);
            }
            if a[1] != 0 {
                let flags = file.flags.load(core::sync::atomic::Ordering::Relaxed);
                super::uaccess::copy_to_server(a[1], &flags.to_le_bytes())?;
            }
            let id = s.id as i64;
            // Pinned for the rest of the call (not a service thread's,
            // which serves no call). A pin that cannot be kept fails the
            // lookup: an unpinned file could go under the call.
            with_current(|p| match p.linux.as_mut() {
                Some(l) if !l.pager => {
                    l.pinned.try_reserve(1).map_err(|_| ENOMEM)?;
                    l.pinned.push(file.clone());
                    Ok::<(), i64>(())
                }
                _ => Ok(()),
            })?;
            Ok(id)
        }
        SYS_KFD_READY => {
            let file = instance.files.lock().get(&a[0]).and_then(|w| w.upgrade()).ok_or(ENOENT)?;
            file.set_ready(a[1] as i16);
            Ok(0)
        }
        SYS_KFD_READ | SYS_KFD_WRITE => {
            let file = with_current(|p| p.file(a[0]))?;
            let (buf, len) = (a[1], a[2].min(64 * 1024) as usize);
            let mut data = alloc::vec![0u8; len];
            if nr == SYS_KFD_READ {
                let n = file.read(&mut data)?;
                super::uaccess::copy_to_server(buf, &data[..n])?;
                Ok(n as i64)
            } else {
                super::uaccess::copy_from_server(buf, &mut data)?;
                Ok(file.write(&data)? as i64)
            }
        }
        SYS_KFD_CLOSE => {
            let gone = super::current_files()?.take(a[0]).ok_or(EBADF)?;
            drop(gone);
            Ok(0)
        }
        SYS_MO_SUPPLY => {
            let (handle, offset, buf, len) = (a[0], a[1], a[2], a[3]);
            let cache = instance.memory(handle)?;
            if cache.paged_key().is_none() || !page_aligned(offset) || len > PAGE {
                return Err(EINVAL);
            }
            let mut data = [0u8; PAGE as usize];
            super::uaccess::copy_from_server(buf, &mut data[..len as usize])?;
            Ok(cache.supply(offset / PAGE, &data[..len as usize])? as i64)
        }
        SYS_MO_FAIL => {
            let (handle, offset) = (a[0], a[1]);
            let cache = instance.memory(handle)?;
            if cache.paged_key().is_none() || !page_aligned(offset) {
                return Err(EINVAL);
            }
            cache.fail(offset / PAGE)?;
            Ok(0)
        }
        SYS_SHARED_MAP => Ok(instance.grow_heap(a[0])? as i64),
        SYS_INODE_ROOT..=SYS_EXEC_TARGET => super::linux_inode::call(&instance, nr, a),
        SYS_INITRAMFS => {
            let image = crate::fs::initramfs().ok_or(ENOENT)?;
            super::uaccess::copy_to_server(a[0], &(image.len() as u64).to_le_bytes())?;
            Ok(instance.insert(Object::Image(image))? as i64)
        }
        SYS_MO_FROM_IMAGE => {
            let Object::Image(image) = instance.object(a[0])? else { return Err(EINVAL) };
            let end = a[1].checked_add(a[2]).filter(|&e| e <= image.len() as u64).ok_or(EINVAL)?;
            let cache = PageCache::memory(&image[a[1] as usize..end as usize])?;
            Ok(instance.insert(Object::File(cache, None))? as i64)
        }
        SYS_MO_CREATE_FILE => {
            let cache = PageCache::memory(&[])?;
            Ok(instance.insert(Object::File(cache, None))? as i64)
        }
        SYS_MO_HOLD => {
            let cache = match instance.object(a[0])? {
                Object::File(cache, _) => cache,
                _ => return Err(EINVAL),
            };
            let record = match a[1] {
                0 => None,
                word => Some(Arc::try_new(Record { instance: Arc::downgrade(&instance), word }).map_err(|_| ENOMEM)?),
            };
            match instance.insert(Object::File(cache, record.clone())) {
                Ok(h) => Ok(h as i64),
                Err(e) => {
                    // Refused: nothing was handed over, so no release.
                    if let Some(mut r) = record.and_then(|r| Arc::try_unwrap(r).ok()) {
                        r.instance = Weak::new();
                    }
                    Err(e)
                }
            }
        }
        SYS_MO_FILE_READ | SYS_MO_FILE_WRITE | SYS_MO_FILE_SIZE | SYS_MO_TRUNCATE => {
            use crate::fs::cache::Fill;
            let cache = match instance.object(a[0])? {
                Object::File(cache, _) => cache,
                _ => return Err(EINVAL),
            };
            let (offset, buf, len) = (a[1], a[2], a[3]);
            let flags = if matches!(nr, SYS_MO_FILE_READ | SYS_MO_FILE_WRITE) { a[4] } else { 0 };
            let allowed = if nr == SYS_MO_FILE_WRITE { MO_NOFILL | MO_CHECK_BACKED | MO_BACKED } else { MO_NOFILL };
            if flags & !allowed != 0 || flags & (MO_CHECK_BACKED | MO_BACKED) == MO_CHECK_BACKED | MO_BACKED {
                return Err(EINVAL);
            }
            let fill = if flags & MO_NOFILL != 0 { Fill::No } else { Fill::Yes };
            let backing = match flags {
                f if f & MO_CHECK_BACKED != 0 => crate::fs::cache::Backing::Check,
                f if f & MO_BACKED != 0 => crate::fs::cache::Backing::Vouched(a[5]),
                _ => crate::fs::cache::Backing::Ignore,
            };
            match nr {
                // As the kernel's tmpfs files are read and written.
                SYS_MO_FILE_READ => {
                    let n = super::uaccess::read_to_user(buf, len, true, |chunk, done| cache.read_with(offset + done, chunk, fill))?;
                    Ok(n as i64)
                }
                SYS_MO_FILE_WRITE => {
                    offset.checked_add(len).ok_or(EFBIG)?;
                    let n = super::uaccess::write_from_user(buf, len, |chunk, done| cache.write_with(offset + done, chunk, fill, backing));
                    if cache.is_cached() {
                        // Too many dirty pages: this writer waits a little.
                        crate::fs::cache::balance_dirty();
                    }
                    Ok(n? as i64)
                }
                SYS_MO_FILE_SIZE => Ok(cache.size() as i64),
                _ => {
                    cache.truncate(offset)?;
                    Ok(0)
                }
            }
        }
        SYS_FS_RECORD => {
            let (op, word) = (a[0], a[1]);
            let record = || (word != 0).then(|| Record { instance: Arc::downgrade(&instance), word });
            match op {
                FS_GET => Ok(with_current(|p| p.fs.as_ref().map_or(0, |f| f.record_word())) as i64),
                FS_SET => {
                    if word == 0 {
                        return Err(EINVAL);
                    }
                    let fs = with_current(|p| p.fs.clone()).ok_or(EINVAL)?;
                    // A refused record was never handed over: no release.
                    let mut refused = fs.set_record(Record { instance: Arc::downgrade(&instance), word }).err();
                    if let Some(r) = refused.as_mut() {
                        r.instance = Weak::new();
                    }
                    if refused.is_some() { Err(EEXIST) } else { Ok(0) }
                }
                FS_CHILD => {
                    let old = with_current(|p| p.linux.as_mut().and_then(|l| core::mem::replace(&mut l.fs_child, record())));
                    // Released outside the task's lock.
                    drop(old);
                    Ok(0)
                }
                _ => Err(EINVAL),
            }
        }
        SYS_SET_USERCOPY => {
            let (insn, fixup) = (a[0], a[1]);
            let code = IMAGE_BASE..THREADS_BASE;
            if !code.contains(&insn) || !code.contains(&fixup) {
                return Err(EINVAL);
            }
            let mut set = false;
            instance.usercopy.call_once(|| {
                set = true;
                (insn, fixup)
            });
            if set { Ok(0) } else { Err(EBUSY) }
        }
        SYS_CLOCK_READ => super::sys_time::read_clock(a[0]).map(|ns| ns as i64),
        SYS_SLEEP_UNTIL => super::sleep_until(a[0]).map(|_| 0),
        SYS_YIELD => {
            super::yield_now();
            Ok(0)
        }
        SYS_SERVER_FUTEX_WAIT => {
            let (addr, val, deadline, flags) = (a[0], a[1] as u32, a[2], a[3]);
            if flags & !FUTEX_INTERRUPTIBLE != 0 {
                return Err(EINVAL);
            }
            let (deadline, interruptible) = ((deadline != 0).then_some(deadline), flags & FUTEX_INTERRUPTIBLE != 0);
            // A word of an object mapped into the region has the object's key.
            if let Some((object, offset, word)) = instance.object_word(addr) {
                return super::futex::object_wait(&object, offset, word, val, deadline, interruptible);
            }
            let word = instance.word(addr)?;
            let id = Arc::as_ptr(&instance) as usize;
            super::futex::server_wait(id, addr, word, val, deadline, interruptible)
        }
        SYS_SERVER_FUTEX_WAKE => {
            if let Some((object, offset, _)) = instance.object_word(a[0]) {
                return Ok(super::futex::object_wake(&object, offset, a[1]));
            }
            instance.word(a[0])?;
            Ok(super::futex::server_wake(Arc::as_ptr(&instance) as usize, a[0], a[1]))
        }
        SYS_CHAN_CREATE => {
            instance.channel_added()?;
            let mapped = super::channel::Channel::new(a[0]).and_then(|c| instance.map_object(c.memory(), c.pages(), 1).map(|addr| (c, addr)));
            let (channel, addr) = mapped.inspect_err(|_| instance.channel_gone())?;
            // (Undone already if the end cannot be made.)
            let end = super::channel::ClientEnd::new(channel, Arc::downgrade(&instance), addr)?;
            // From here on, dropping the end undoes it all.
            let end = Arc::try_new(end).map_err(|_| {
                super::channel::run_deferred();
                ENOMEM
            })?;
            let handle = instance.insert(Object::Channel(end)).inspect_err(|_| super::channel::run_deferred())?;
            if let Err(e) = super::uaccess::copy_to_server(a[1], &addr.to_le_bytes()) {
                let end = instance.handles.lock().objects.remove(&handle);
                drop(end);
                super::channel::run_deferred();
                return Err(e);
            }
            Ok(handle as i64)
        }
        SYS_CHAN_CONNECT => {
            let end = instance.channel(a[0])?;
            if a[2] > 64 {
                return Err(EINVAL);
            }
            let mut name = [0u8; 64];
            super::uaccess::copy_from_server(a[1], &mut name[..a[2] as usize])?;
            let name = core::str::from_utf8(&name[..a[2] as usize]).map_err(|_| EINVAL)?;
            end.channel.connect(name, super::current_pid())?;
            Ok(0)
        }
        SYS_GRANT => {
            let end = instance.channel(a[0])?;
            let object = instance.memory(a[1])?;
            let (flags, writable) = (a[4], a[4] & GRANT_WRITE != 0);
            match flags & !GRANT_WRITE {
                0 => Ok(end.channel.grant(&object, a[2], a[3], writable)? as i64),
                // A cached object's pages: to fill (into a writable grant)
                // or to write back (read-only).
                mode @ (GRANT_FILL | GRANT_DIRTY) if (mode == GRANT_FILL) == writable => {
                    let (id, first, count, size) = match end.channel.grant_run(&object, a[2], a[3], mode == GRANT_FILL, writable) {
                        Ok(run) => run,
                        Err(crate::fs::cache::Scan::Errno(e)) => return Err(e),
                        Err(crate::fs::cache::Scan::Resume(at)) => {
                            // Not found yet: the caller asks again from `at`.
                            let mut info = [0u8; 24];
                            info[..8].copy_from_slice(&at.to_le_bytes());
                            super::uaccess::copy_to_server(a[5], &info)?;
                            return Err(EAGAIN);
                        }
                    };
                    let mut info = [0u8; 24];
                    info[..8].copy_from_slice(&first.to_le_bytes());
                    info[8..16].copy_from_slice(&count.to_le_bytes());
                    info[16..].copy_from_slice(&size.to_le_bytes());
                    if let Err(e) = super::uaccess::copy_to_server(a[5], &info) {
                        // The caller cannot know the run: it goes again.
                        let _ = end.channel.revoke(id);
                        let _ = if mode == GRANT_FILL { object.filled(first, count, false) } else { object.redirty(first, count) };
                        return Err(e);
                    }
                    Ok(id as i64)
                }
                _ => Err(EINVAL),
            }
        }
        SYS_MO_CREATE_CACHED => {
            let (size, key, limit) = (a[0], a[1], a[2]);
            let pager: alloc::sync::Weak<dyn crate::fs::cache::Pager> = Arc::downgrade(&instance) as _;
            let cache = PageCache::cached(size, limit, pager, key)?;
            Ok(instance.insert(Object::File(cache, None))? as i64)
        }
        SYS_MO_FILLED | SYS_MO_REDIRTY => {
            let Object::File(cache, _) = instance.object(a[0])? else { return Err(EINVAL) };
            if !page_aligned(a[1]) {
                return Err(EINVAL);
            }
            let (first, count) = (a[1] / PAGE, a[2]);
            if nr == SYS_MO_FILLED {
                cache.filled(first, count, a[3] != 0)?;
            } else {
                cache.redirty(first, count)?;
            }
            Ok(0)
        }
        SYS_MO_BACKED => {
            let Object::File(cache, _) = instance.object(a[0])? else { return Err(EINVAL) };
            cache.backed(a[1], a[2], a[3] != 0)?;
            Ok(0)
        }
        SYS_MO_UNBACK => {
            let Object::File(cache, _) = instance.object(a[0])? else { return Err(EINVAL) };
            let (found, run) = match cache.unback(a[1]) {
                Ok(Some((first, end))) => (1, [first, end]),
                Ok(None) => (0, [0, 0]),
                Err(crate::fs::cache::Scan::Errno(e)) => return Err(e),
                Err(crate::fs::cache::Scan::Resume(at)) => {
                    let mut info = [0u8; 16];
                    info[..8].copy_from_slice(&at.to_le_bytes());
                    super::uaccess::copy_to_server(a[2], &info)?;
                    return Err(EAGAIN);
                }
            };
            let mut info = [0u8; 16];
            info[..8].copy_from_slice(&run[0].to_le_bytes());
            info[8..].copy_from_slice(&run[1].to_le_bytes());
            super::uaccess::copy_to_server(a[2], &info)?;
            Ok(found)
        }
        SYS_THREAD_EXISTS => {
            if a[1] & !THREAD_IN_INSTANCE != 0 {
                return Err(EINVAL);
            }
            if a[1] & THREAD_IN_INSTANCE != 0 {
                // A process of this instance's tree.
                let ok = a[0] != 0 && super::group(a[0]).is_some_and(|g| g.instance.load(core::sync::atomic::Ordering::Acquire) == instance.id);
                return if ok { Ok(0) } else { Err(ESRCH) };
            }
            if a[0] == 0 || super::task(a[0]).is_none() {
                return Err(ESRCH);
            }
            Ok(0)
        }
        SYS_KFD_INSTALL_FILE => {
            use crate::fs::file::O_CLOEXEC;
            let flags = a[1] as u32;
            if is_pager() {
                return Err(EPERM);
            }
            if a[1] > u32::MAX as u64 || flags & !O_CLOEXEC != 0 {
                return Err(EINVAL);
            }
            let file = match instance.object(a[0])? {
                Object::KernelFile(f) => f,
                Object::InFlight(f) => f.0.clone(),
                _ => return Err(EINVAL),
            };
            with_current(|p| p.alloc_fd(file, flags & O_CLOEXEC != 0, 0))
        }
        SYS_KFILE_INFO => {
            let (file, extra) = match instance.object(a[0])? {
                // Less the clone `object` just made.
                Object::KernelFile(f) => (f, 1),
                // The handle's reference is the `InFlight`'s; this clone is
                // the extra one.
                Object::InFlight(f) => (f.0.clone(), 1),
                _ => return Err(EINVAL),
            };
            let refs = Arc::strong_count(&file) as u64 - extra;
            let id = match &file.kind {
                crate::fs::file::Kind::Server(s) if s.owned_by(Arc::as_ptr(&instance) as *const ()) => s.id,
                _ => 0,
            };
            drop(file);
            let mut out = [0u8; 16];
            out[..8].copy_from_slice(&refs.to_le_bytes());
            out[8..].copy_from_slice(&id.to_le_bytes());
            super::uaccess::copy_to_server(a[1], &out)?;
            Ok(0)
        }
        SYS_SIGNAL_THREAD => {
            if is_pager() {
                return Err(EPERM);
            }
            if a[0] == 0 || a[0] > 64 {
                return Err(EINVAL);
            }
            super::signal::tgkill(Some(super::current_pid() as i64), super::current_tid() as i64, a[0])
        }
        SYS_THREAD_IDS => {
            let mut ids = [0u8; 32];
            ids[..8].copy_from_slice(&(super::current_pid() as u64).to_le_bytes());
            ids[8..16].copy_from_slice(&(super::current_tid() as u64).to_le_bytes());
            // uid and gid: everyone is root.
            super::uaccess::copy_to_server(a[0], &ids)?;
            Ok(0)
        }
        SYS_SERVER_LOG => {
            let len = a[1].min(SERVER_LOG_MAX);
            let mut text = [0u8; SERVER_LOG_MAX as usize];
            super::uaccess::copy_from_server(a[0], &mut text[..len as usize])?;
            let text = core::str::from_utf8(&text[..len as usize]).map_err(|_| EINVAL)?;
            crate::printkln!("[linux] {}", text.trim_end());
            Ok(0)
        }
        SYS_MO_MAP_SERVER => {
            // Plain memory only (its pages are made present now).
            let Object::Memory(object) = instance.object(a[0])? else { return Err(EINVAL) };
            if object.paged_key().is_some() || a[1] == 0 || a[1].checked_mul(PAGE).is_none_or(|l| l > object.size()) {
                return Err(EINVAL);
            }
            Ok(instance.map_region(&object, a[1], 0, true)? as i64)
        }
        SYS_MO_UNMAP_SERVER => instance.unmap_server_object(a[0]).map(|_| 0),
        SYS_REVOKE => {
            let end = instance.channel(a[0])?;
            end.channel.revoke(u32::try_from(a[1]).map_err(|_| EINVAL)?)
        }
        SYS_MO_UNMAP => {
            let len = range(a[0], a[1])?;
            mm()?.lock().unmap(a[0], len);
            Ok(0)
        }
        SYS_MO_PROTECT => {
            let len = range(a[0], a[1])?;
            let prot = prot(a[2])?;
            mm()?.lock().protect(a[0], len, prot).map_err(|e| if e == super::address_space::Fault::Access { EACCES } else { ENOMEM })?;
            Ok(0)
        }
        SYS_MO_READ | SYS_MO_WRITE => {
            let (handle, offset, buf, len) = (a[0], a[1], a[2], a[3]);
            let object = instance.object(handle)?;
            if let Object::Image(image) = object {
                if nr == SYS_MO_WRITE {
                    return Err(EACCES);
                }
                let end = offset.checked_add(len).filter(|&e| e <= image.len() as u64).ok_or(EINVAL)?;
                super::uaccess::copy_to_server(buf, &image[offset as usize..end as usize])?;
                return Ok(len as i64);
            }
            let cache = instance.memory(handle)?;
            // Within the object (a file object: a read up to its end, a
            // write as far as it goes), a page-sized piece at a time.
            let end = match object {
                Object::File(..) if nr == SYS_MO_READ => offset.checked_add(len).ok_or(EINVAL)?.min(cache.size().max(offset)),
                Object::File(..) => offset.checked_add(len).ok_or(EFBIG)?,
                _ => offset.checked_add(len).filter(|&e| e <= cache.size()).ok_or(EINVAL)?,
            };
            let mut chunk = [0u8; PAGE as usize];
            let mut done = 0;
            while offset + done < end {
                let n = ((end - offset - done) as usize).min(chunk.len());
                if nr == SYS_MO_READ {
                    let got = cache.read(offset + done, &mut chunk[..n])?;
                    super::uaccess::copy_to_server(buf + done, &chunk[..got])?;
                    if got < n {
                        return Ok((done + got as u64) as i64);
                    }
                } else {
                    super::uaccess::copy_from_server(buf + done, &mut chunk[..n])?;
                    // A file object that cannot grow further (ENOSPC):
                    // what was written counts.
                    if cache.is_cached() && done > 0 && done % (64 * PAGE) == 0 {
                        crate::fs::cache::balance_dirty();
                    }
                    match cache.write(offset + done, &chunk[..n]) {
                        Ok(w) if w < n => return Ok((done + w as u64) as i64),
                        Ok(_) => {}
                        Err(e) if done == 0 => return Err(e),
                        Err(_) => return Ok(done as i64),
                    }
                }
                done += n as u64;
            }
            Ok(done as i64)
        }
        _ => Err(ENOSYS),
    }
}

/// Loads the view of the current address space for the server or the
/// program.
fn switch_view(normal: bool) {
    with_current(|p| {
        if let Some(mm) = &p.mm {
            super::tlb::switch(Some(&mm.tlb), Some(&mm.tlb), normal);
        }
    });
}

/// The program trapped (`f` holds its registers): back to the server, with
/// `reason`.
pub fn trap(f: &mut Frame, reason: u64) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        with_current(|p| {
            let l = p.linux.as_mut().expect("a Linux thread");
            l.trap_nr = (reason == REASON_SYSCALL).then_some(f.rax);
            save(f, l.state());
            *f = l.normal;
            f.rax = reason;
            l.restricted = false;
        });
        switch_view(true);
    });
}

/// restricted_enter() from the server (`f`): runs the program. Returns
/// the system call the server just handled itself, if any, for the signal
/// delivery that follows.
pub fn enter(f: &mut Frame) -> Result<Option<u64>, i64> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        with_current(|p| {
            let l = p.linux.as_mut().expect("a Linux thread");
            if l.pager {
                return Err(EPERM);
            }
            let mut program = Frame::default();
            load(l.state(), &mut program)?;
            l.normal = *f;
            *f = program;
            l.restricted = true;
            Ok::<_, i64>((l.trap_nr.take(), core::mem::take(&mut l.pinned)))
        })
        .inspect(|_| switch_view(false))
    })
    .map(|(handled, pinned)| {
        // The call is done with its files.
        for file in pinned {
            crate::fs::file::release(file);
        }
        handled
    })
}

/// A page fault of the server at `rip` on program memory at `addr`: the
/// program's own fault handling resolves it (true), else, if `rip` is the
/// server's copy instruction, the server resumes at its fixup (`Some`).
pub fn server_fault(rip: u64) -> Option<u64> {
    let instance = with_current(|p| p.linux.as_ref().map(|l| l.instance.clone()))?;
    let &(insn, fixup) = instance.usercopy.get()?;
    (rip == insn).then_some(fixup)
}

/// legacy_syscall(closed, cap) from the server: the kernel's Linux
/// implementation carries out the system call in `State`, which then holds
/// the result (or the new program after execve, or a signal frame); the
/// server files the call closed for good go to `closed` (see
/// `SYS_LEGACY_SYSCALL`). Returns how many.
///
/// The call runs in the program's view, as it would without the server:
/// the server's memory is then out of reach of the kernel's Linux code
/// altogether, not only because every user pointer is checked against
/// 64 TiB (`uaccess`).
pub fn legacy(closed: u64, cap: u64) -> Result<u64, i64> {
    let state = with_current(|p| {
        let l = p.linux.as_mut().filter(|l| !l.pager)?;
        // The pass-through delivers signals itself.
        l.trap_nr = None;
        Some(*l.state())
    })
    .ok_or(EPERM)?;
    let mut program = Frame::default();
    load(&state, &mut program)?;
    crate::counters::add(|c| &c.legacy_calls, 1);
    super::sched::current().group.legacy_calls.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    set_legacy(true);
    super::syscall::dispatch_linux(&mut program);
    set_legacy(false);
    let (ids, instance, unused) = with_current(|p| match p.linux.as_mut() {
        Some(l) => {
            save(&program, l.state());
            l.exec_target = None;
            (core::mem::take(&mut l.closed_now), Some(l.instance.clone()), l.fs_child.take())
        }
        None => (Vec::new(), None, None),
    });
    // A child's record no clone took goes back to the server.
    drop(unused);
    let mut told = 0;
    for id in ids {
        let fits = told < cap && super::uaccess::copy_to_server(closed + told * 8, &id.to_le_bytes()).is_ok();
        if fits {
            told += 1;
        } else if let Some(i) = &instance {
            i.queue_closed(id);
        }
    }
    Ok(told)
}

/// Enters or leaves a legacy call: the view follows.
fn set_legacy(on: bool) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        with_current(|p| {
            if let Some(l) = p.linux.as_mut() {
                l.in_legacy = on;
            }
        });
        switch_view(!on);
    });
}

/// event_wait(event, deadline): the instance's next event (a page a
/// thread waits for, a server file closed, ...), written to the server's
/// memory at `out`; `EVENT_TIMER` once `deadline` (0: none) passed. When
/// the tree has no program left: `EVENT_CLOSING` once, then None (the
/// service thread's process ends).
fn event_wait(instance: &Arc<Instance>, out: u64, deadline: u64) -> Option<SysResult> {
    if !owns_events() {
        return Some(Err(EPERM));
    }
    loop {
        let next = x86_64::instructions::interrupts::without_interrupts(|| {
            let wait = super::sched::prepare_to_wait(instance.pager_chan());
            let mut q = instance.pager.lock();
            if let Some(mut e) = q.requests.pop_front() {
                match e.kind {
                    EVENT_PAGE => {
                        q.queued.remove(&(e.a, e.b / PAGE));
                    }
                    EVENT_WRITEBACK => q.writeback_queued = false,
                    EVENT_INFLIGHT => q.inflight_queued = false,
                    EVENT_SYNC => {
                        // Answers every ticket asked so far.
                        q.sync_queued = false;
                        e.a = q.sync_wanted;
                    }
                    _ => {}
                }
                return Some(Some(e));
            }
            if q.closing {
                if q.closing_told {
                    return Some(None);
                }
                q.closing_told = true;
                return Some(Some(Event { kind: EVENT_CLOSING, a: 0, b: 0 }));
            }
            drop(q);
            if deadline != 0 {
                if crate::time::now() >= deadline {
                    return Some(Some(Event { kind: EVENT_TIMER, a: 0, b: 0 }));
                }
                wait.sleep_until(deadline);
            } else {
                wait.sleep();
            }
            None
        });
        match next {
            Some(Some(event)) => {
                let bytes =
                    unsafe { core::slice::from_raw_parts(&event as *const Event as *const u8, core::mem::size_of::<Event>()) };
                if let Err(e) = super::uaccess::copy_to_server(out, bytes) {
                    // Still due: next time.
                    let mut q = instance.pager.lock();
                    match event.kind {
                        EVENT_PAGE if !q.queued.insert((event.a, event.b / PAGE)) => {}
                        EVENT_TIMER => {}
                        EVENT_CLOSING => q.closing_told = false,
                        EVENT_WRITEBACK if q.writeback_queued => {}
                        EVENT_SYNC if q.sync_queued => {}
                        _ => {
                            match event.kind {
                                EVENT_WRITEBACK => q.writeback_queued = true,
                                EVENT_SYNC => q.sync_queued = true,
                                _ => {}
                            }
                            q.requests.push_front(event)
                        }
                    }
                    return Some(Err(e));
                }
                return Some(Ok(0));
            }
            Some(None) => return None,
            None => {}
        }
    }
}
