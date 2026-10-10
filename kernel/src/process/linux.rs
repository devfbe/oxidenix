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
//! The server implements every Linux system call (R9: the pass-through
//! of phase R1, `legacy_syscall`, and the kernel's Linux code are gone);
//! the kernel's part is the mechanism this module's calls offer
//! (`server_call`). Faults the kernel cannot resolve and exceptions of the
//! program go back to the server as `REASON_EXCEPTION`.

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
use x86_64::structures::paging::{FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

/// The top-level slot of the shared region.
pub const SHARED_SLOT: usize = (SHARED_BASE >> 39) as usize;

/// Flags of the tables leading to user pages.
pub fn table_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE
}

/// rflags bits a program may set: CF PF AF ZF SF TF DF OF (as sigreturn).
const USER_FLAGS: u64 = 0xcd5;
/// The server's program in the boot image.
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

/// The instances alive, but `except` (none if there is no memory for the
/// list). The list is copied out under its lock into room reserved before,
/// and references are dropped only after it (a drop may be the instance's
/// teardown).
fn instances(except: Option<&Instance>) -> Vec<Arc<Instance>> {
    let mut out: Vec<Arc<Instance>> = Vec::new();
    loop {
        let want = INSTANCES.lock().len();
        if out.try_reserve_exact(want).is_err() {
            return out;
        }
        let all = INSTANCES.lock();
        if all.len() <= out.capacity() {
            out.extend(all.iter().filter_map(|w| w.upgrade()));
            break;
        }
    }
    out.retain(|i| !except.is_some_and(|e| core::ptr::eq(Arc::as_ptr(i), e)));
    out
}

/// When `post_shrink` last went through the instances (`time::now`), and
/// how often it may; how often an instance gets `EVENT_SHRINK` at most
/// (one asked for sooner waits, `event_wait`).
static LAST_SHRINK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
const SHRINK_POST_INTERVAL: u64 = 100_000_000;
const SHRINK_INTERVAL: u64 = 1_000_000_000;

/// Memory is short (a commit was refused at the limit, or reclaim found no
/// clean cache page to drop): every instance's service thread gets
/// `EVENT_SHRINK` for `pages` pages (requests coalesce into the largest;
/// each instance gets one at most once a second). From a thread that holds
/// no lock (the reclaimer). False if it went through them less than 100 ms
/// ago and did nothing (the caller asks again later).
pub fn post_shrink(pages: u64) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let now = crate::time::now();
    let last = LAST_SHRINK.load(Relaxed);
    if last != 0 && now < last + SHRINK_POST_INTERVAL || LAST_SHRINK.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
        return false;
    }
    for instance in instances(None) {
        instance.shrink.fetch_max(pages, core::sync::atomic::Ordering::AcqRel);
        super::wakeup(instance.pager_chan());
    }
    true
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
/// answered (or its pager is gone), at most until `deadline` (0: none), or,
/// if `killable`, until the caller is being killed. Whether all did.
pub fn sync_wait(ticket: u64, except: Option<&Instance>, deadline: u64, killable: bool) -> bool {
    let asked = instances(except);
    loop {
        let wait = super::sched::prepare_to_wait(sync_chan());
        if asked.iter().all(|i| i.synced(ticket)) {
            return true;
        }
        let dying = super::kill::dying();
        if (killable && dying) || (deadline != 0 && crate::time::now() >= deadline) {
            return false;
        }
        match deadline {
            // A dying caller that must wait on (`power`) sleeps a tick at a
            // time: whatever ends its sleeps does not make it spin.
            d if dying => wait.sleep_until((crate::time::now() + crate::timer::TICK_NS).min(d)),
            0 => wait.sleep(),
            d => wait.sleep_until(d),
        }
    }
}

/// `SYS_POWER`: every instance writes its caches back and flushes them
/// (the caller's too: its pager is another thread), at most `SYNC_WAIT`
/// (each request to diskfs is bounded on its own), then the machine powers
/// off or restarts. A caller killed meanwhile still waits: the machine
/// goes off either way, and its data with it unless the write-back is done.
fn power(how: u64) -> ! {
    const SYNC_WAIT: u64 = 60 * crate::time::NSEC_PER_SEC;
    let ticket = sync_start(None);
    if !sync_wait(ticket, None, crate::time::now() + SYNC_WAIT, false) {
        crate::printkln!("[kernel] power: a Linux server did not write its caches back");
    }
    match how {
        POWER_RESTART => crate::restart(),
        _ => crate::power_off(0),
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
    match crate::fs::program(SERVER_PATH).ok_or(ENOENT).and_then(PageCache::program) {
        Ok(image) => {
            IMAGE.call_once(|| image);
        }
        Err(e) => crate::printkln!("[linux] cannot read {} (errno {}); Linux programs cannot start", SERVER_PATH, e),
    }
}

fn table_at(frame: PhysFrame) -> &'static mut PageTable {
    unsafe { &mut *(memory::phys_to_virt(frame.start_address().as_u64()) as *mut PageTable) }
}

/// A zeroed frame, reclaiming cache pages for it if none is free.
fn zeroed_frame() -> Result<PhysFrame, i64> {
    let frame = memory::user_frame().ok_or(ENOMEM)?;
    unsafe { core::ptr::write_bytes(memory::phys_to_virt(frame.start_address().as_u64()), 0, 4096) };
    Ok(frame)
}

/// Pages of commitment an instance holds for its heap beyond what is
/// mapped there (2 MiB): a server cannot fail an allocation but by breaking
/// its instance, and a heap that gives memory back has to commit again
/// later, maybe when its programs have taken all there is or dirty pages
/// crowd the limit (the server is what writes them back). The reserve is
/// the instance's own (no other tree can take it) and counted as committed
/// for everyone (the commit guarantee holds); commits beyond it are
/// ordinary ones. It is had when there is room (never at the cost of a
/// tree's start or of another commit) and all instances' together are at
/// most `HEAP_POOL_SHARE` of the commit limit (`pageheap::charge`, whose
/// arithmetic is host-tested).
const HEAP_RESERVE: u64 = 512;
const HEAP_POOL_SHARE: u64 = 16;
static HEAP_POOL: pageheap::charge::Pool = pageheap::charge::Pool::new();

/// The heap area's commitment (`Instance::heap`).
pub struct HeapArea {
    /// Its mapped pages and its reserve: what it holds of the commit limit.
    charge: pageheap::charge::Charge,
    /// A refused commit or a broken account was logged (once per instance).
    refused_told: bool,
}

impl HeapArea {
    /// Tops the reserve up as far as the pool and the commit limit allow
    /// now (failing nothing).
    fn grow_reserve(&mut self) {
        HEAP_POOL.set_max(memory::commit_stats().1 / HEAP_POOL_SHARE);
        let want = HEAP_POOL.take(self.charge.deficit(HEAP_RESERVE));
        if want == 0 {
            return;
        }
        // As much as there is room for now; a top-up that finds none is
        // no sign of pressure (it asks nobody to shrink).
        let got = memory::commit_some(want);
        HEAP_POOL.give(want - got);
        self.charge.grow_reserve(got, HEAP_RESERVE);
    }
}

/// Runs one `SYS_SHARED_DECOMMIT_RUNS` takes at most.
const DECOMMIT_RUNS_MAX: u64 = 16;

/// Pages `Instance::unmap_pages` takes off at a time (their frames on the
/// stack until the shootdown).
const UNMAP_BATCH: usize = 64;

/// A zeroed frame for the server's heap (`SYS_SHARED_COMMIT`): a free one,
/// else one clean cache pages give back (those used lately too); never a
/// wait (for write-back, for an address space): the server may be the one
/// to write back, and its heap fails fast (ENOMEM) rather than wait for
/// itself.
fn heap_frame() -> Result<PhysFrame, i64> {
    if let Ok(frame) = zeroed_frame() {
        return Ok(frame);
    }
    memory::reclaim_forced(1);
    zeroed_frame()
}

/// Pages `Instance::commit_heap` takes frames for at a time (on the stack,
/// gotten before the heap area's lock).
const COMMIT_BATCH: u64 = 64;

/// The end of [addr, addr + len) if it is a page-aligned range of the
/// heap area (`HEAP_BASE..THREADS_BASE`), else EINVAL.
fn heap_range(addr: u64, len: u64) -> Result<u64, i64> {
    let end = addr.checked_add(len).ok_or(EINVAL)?;
    if addr % PAGE != 0 || len % PAGE != 0 || len == 0 || addr < HEAP_BASE || end > THREADS_BASE {
        return Err(EINVAL);
    }
    Ok(end)
}

fn heap_page(addr: u64) -> Page<Size4KiB> {
    Page::containing_address(VirtAddr::new(addr))
}

/// A word of the server's memory a wait reads (`Instance::word`), with the
/// reference on its frame it holds if the page may be decommitted.
struct ServerWord {
    word: *const core::sync::atomic::AtomicU32,
    frame: Option<PhysFrame>,
}

impl ServerWord {
    fn get(&self) -> &core::sync::atomic::AtomicU32 {
        // The frame is mapped in the physical map for good, and either the
        // instance's (image, thread areas) or held by this reference.
        unsafe { &*self.word }
    }
}

impl Drop for ServerWord {
    fn drop(&mut self) {
        if let Some(frame) = self.frame {
            memory::with_frames(|f| unsafe { f.deallocate_frame(frame) });
        }
    }
}

/// One instance of the Linux server: the page tables of its shared region
/// and its threads' areas.
pub struct Instance {
    /// Unique among every instance the kernel ever made (process groups
    /// name theirs by it: `ThreadGroup::instance`).
    pub id: u64,
    /// The host grant: the tree may change what is the machine's, not its
    /// own (the wall clock, `SYS_CLOCK_SET`; power, `SYS_POWER`). Given by
    /// whoever starts the tree (`host_grant`: the monitor's `run` and
    /// autorun do); without it those calls are EPERM, and the server acts
    /// as Linux in a pid namespace that is not the initial one (ADR 0011).
    host: core::sync::atomic::AtomicBool,
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
    /// Pages of paged objects that threads wait for, for the pager thread.
    pager: spin::Mutex<PagerQueue>,
    /// Address spaces of the tree's programs: when the last goes, so does
    /// the pager's process.
    programs: core::sync::atomic::AtomicUsize,
    /// Held while the entries of the server's heap area
    /// (`HEAP_BASE..THREADS_BASE`) change, with its commitment (mapped pages
    /// and reserve, `SYS_SHARED_COMMIT`); never across a wait for memory.
    /// (A wait on a word there takes no part of it: `word` relies on the
    /// frames' lock, under which a decommit frees.)
    heap: crate::sync::Mutex<HeapArea>,
    /// Memory is short: the service thread gets `EVENT_SHRINK` for this
    /// many pages (0: none asked; `post_shrink`), at most once per
    /// `SHRINK_INTERVAL` (when it got the last).
    shrink: core::sync::atomic::AtomicU64,
    shrunk_at: core::sync::atomic::AtomicU64,
    /// The dirty and pinned pages of its cached objects (bounded per
    /// instance, `fs::cache`).
    cache_counts: Arc<crate::fs::cache::CacheCounts>,
    /// Memory objects the kernel mapped into the region (channels), by
    /// address; held while their entries change and shootdowns run.
    maps: crate::sync::Mutex<BTreeMap<u64, RegionMap>>,
    /// The address spaces showing the region (their normal view), for the
    /// shootdowns of `unmap_object`.
    spaces: spin::Mutex<Vec<Weak<super::tlb::Tlb>>>,
    /// Channels the instance holds (at most `MAX_CHANNELS`).
    channels: core::sync::atomic::AtomicUsize,
    /// The tasks of the programs' threads by thread area, with the area's
    /// generation: what a key (`slot | gen << 32`) names.
    tasks: spin::Mutex<BTreeMap<u64, (u32, Weak<super::task::Task>)>>,
    /// The command line the tree's first process runs (`SYS_INIT_ARGS`).
    init_args: spin::Mutex<Vec<u8>>,
    /// A thread of the server failed (`break_instance`):
    /// the server's state is lost, its lock waits end (`FUTEX_LOCK`).
    broken: core::sync::atomic::AtomicBool,
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
/// wanted from it, holds released, write-back. A page request is queued
/// once until the pager takes it; a thread that still waits asks again only
/// once its request was answered or overtaken (the page came and went, or
/// was cut off: `PageWait`).
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
    /// Keys of the programs' threads that are gone, for `EVENT_THREAD_EXIT`.
    /// Its capacity covers every thread announced and not yet reported
    /// (`announced`), reserved before a thread starts, so a push never
    /// allocates and no exit is lost.
    exits: alloc::collections::VecDeque<u64>,
    announced: usize,
}

/// A kernel object the Linux server refers to by handle.
#[derive(Clone)]
pub(super) enum Object {
    /// A memory object (pages that mappings and reads and writes share).
    Memory(Arc<PageCache>),
    /// The contents of a file of the server's (a tmpfs file object), with
    /// the hold this handle carries (`SYS_MO_HOLD`).
    File(Arc<PageCache>, Option<Arc<Record>>),
    /// The initramfs of the boot image, read-only (`SYS_INITRAMFS`).
    Image(&'static [u8]),
    /// The client's end of a channel to a device server (`SYS_CHAN_CREATE`).
    Channel(Arc<super::channel::ClientEnd>),
    /// A process of the instance (`SYS_PROC_SELF`, `SYS_PROC_CREATE`).
    Process(Arc<Container>),
}

/// A process as the Linux server holds it: the kernel's thread group, and
/// until its first thread is made (`SYS_THREAD_CREATE`), what that thread
/// gets. The handle keeps the group (its CPU time and memory counts) after
/// the process ended, until the server reaped it.
pub struct Container {
    pub group: Arc<super::task::ThreadGroup>,
    fresh: spin::Mutex<Option<Fresh>>,
}

/// The address space of a process without a thread yet, and the id its
/// first thread takes.
struct Fresh {
    mm: Arc<super::address_space::Mm>,
    pid: super::sched::PidReservation,
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
    /// The `State` page of each area, for the kernel to reach directly, and
    /// the area's generation (one more for each thread it serves).
    states: BTreeMap<u64, (PhysFrame, u32)>,
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
        let Ok(cache_counts) = Arc::try_new(crate::fs::cache::CacheCounts::default()) else {
            memory::with_frames(|f| unsafe {
                f.deallocate_frame(view);
                f.deallocate_frame(pdpt);
            });
            return Err(ENOMEM);
        };
        table_at(view)[SHARED_SLOT].set_frame(pdpt, table_flags());
        // From here on, dropping the instance frees what was mapped.
        static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        let mut instance = Instance {
            id: NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
            host: core::sync::atomic::AtomicBool::new(false),
            view,
            pdpt,
            entry: 0,
            slots: spin::Mutex::new(Slots { next: 0, free: Vec::new(), states: BTreeMap::new() }),
            handles: spin::Mutex::new(Handles { next: 1, objects: BTreeMap::new() }),
            usercopy: spin::Once::new(),
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
                exits: alloc::collections::VecDeque::new(),
                announced: 0,
            }),
            programs: core::sync::atomic::AtomicUsize::new(0),
            heap: crate::sync::Mutex::new(HeapArea { charge: pageheap::charge::Charge::new(), refused_told: false }),
            shrink: core::sync::atomic::AtomicU64::new(0),
            shrunk_at: core::sync::atomic::AtomicU64::new(0),
            cache_counts,
            maps: crate::sync::Mutex::new(BTreeMap::new()),
            spaces: spin::Mutex::new(Vec::new()),
            channels: core::sync::atomic::AtomicUsize::new(0),
            tasks: spin::Mutex::new(BTreeMap::new()),
            init_args: spin::Mutex::new(Vec::new()),
            broken: core::sync::atomic::AtomicBool::new(false),
        };
        instance.entry = instance.load(image)?;
        // The heap's reserve as far as there is room (none: the tree starts
        // anyway, the reserve comes with later commits). Dropping the
        // instance returns it.
        instance.heap.get_mut().grow_reserve();
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
        // Frames for the tables it may need, reclaimed if none are free.
        memory::ensure_user_frames(3);
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

    /// A thread area: (its number, its `State` page, its key: the number
    /// and the area's new generation).
    fn thread(&self) -> Result<(u64, PhysFrame, u64), i64> {
        let mut slots = self.slots.lock();
        while let Some(n) = slots.free.pop() {
            let (state, gen) = slots.states.get_mut(&n).expect("a free area was mapped");
            // (31 bits: a key is a positive return value.) An area whose generations are
            // used up is retired, so that no key is ever named twice.
            if *gen == 0x7fff_ffff {
                continue;
            }
            *gen += 1;
            return Ok((n, *state, n | (*gen as u64) << 32));
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
        slots.states.insert(n, (state, 1));
        Ok((n, state, n | 1 << 32))
    }

    /// The live task of the thread `key` (of a program of this instance).
    fn task_of(&self, key: u64) -> Option<Arc<super::task::Task>> {
        // The weak reference only, under the lock: a reference made from it
        // may be the task's last once it was upgraded, and dropping that
        // drops the thread's `LinuxThread`, whose `thread_gone` takes this
        // lock (a dead thread found here deadlocked its CPU).
        let weak = {
            let tasks = self.tasks.lock();
            let (gen, task) = tasks.get(&(key & 0xffff_ffff))?;
            if *gen as u64 != key >> 32 {
                return None;
            }
            task.clone()
        };
        weak.upgrade().filter(|t| t.state() != super::task::State::Dead)
    }

    /// Makes room for the exit of one more program thread, which then counts
    /// as announced (its `LinuxThread` reports its end).
    fn announce(&self) -> Result<(), i64> {
        let mut q = self.pager.lock();
        let want = q.announced + 1;
        if q.exits.capacity() < want {
            let more = want - q.exits.len();
            q.exits.try_reserve(more).map_err(|_| ENOMEM)?;
        }
        q.announced = want;
        Ok(())
    }

    /// A program's thread `key` is gone: `EVENT_THREAD_EXIT` for the
    /// service thread (room was reserved by `announce`).
    fn thread_gone(&self, key: u64) {
        {
            let mut tasks = self.tasks.lock();
            if tasks.get(&(key & 0xffff_ffff)).is_some_and(|(gen, _)| *gen as u64 == key >> 32) {
                tasks.remove(&(key & 0xffff_ffff));
            }
        }
        let mut q = self.pager.lock();
        if q.dead || q.closing {
            q.announced -= 1;
            return;
        }
        // (Within the capacity `announce` reserved: no allocation.)
        q.exits.push_back(key);
        drop(q);
        super::wakeup(self.pager_chan());
    }

    /// Keeps the command line of the tree's first process (`SYS_INIT_ARGS`).
    pub fn set_init_args(&self, path: &str, args: &[alloc::string::String], envs: &[alloc::string::String]) -> Result<(), i64> {
        let len = path.len() + 1 + args.iter().chain(envs).map(|s| s.len() + 1).sum::<usize>() + 2;
        let mut out = Vec::new();
        out.try_reserve_exact(len).map_err(|_| ENOMEM)?;
        out.extend_from_slice(path.as_bytes());
        out.push(0);
        for list in [args, envs] {
            for s in list {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            out.push(0);
        }
        *self.init_args.lock() = out;
        Ok(())
    }

    /// The frame of the data page at `addr`, mapped (zeroed) if missing.
    fn ensure(&self, addr: u64) -> Result<PhysFrame, i64> {
        // (The heap area is mapped only by `commit_heap`, which counts it.)
        assert!(!(HEAP_BASE..THREADS_BASE).contains(&addr), "ensure in the heap area");
        let mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        if let Ok(frame) = mapper.translate_page(page) {
            return Ok(frame);
        }
        let frame = zeroed_frame()?;
        self.map(addr, frame, PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE)?;
        Ok(frame)
    }

    /// `SYS_SHARED_COMMIT`: commits and maps (zeroed) the pages of the
    /// heap area's [addr, addr + len) that are not; returns how many. In
    /// batches of `COMMIT_BATCH` pages, each one's frames gotten first with
    /// no lock held, then committed and mapped under the area's lock (never
    /// a wait for memory under it: a decommit, which gives memory back,
    /// never waits behind a commit that waits for memory).
    fn commit_heap(&self, addr: u64, len: u64) -> Result<u64, i64> {
        let end = heap_range(addr, len)?;
        let mut total = 0;
        let mut at = addr;
        while at < end {
            let to = (at + COMMIT_BATCH * PAGE).min(end);
            total += self.commit_batch(at, to)?;
            at = to;
        }
        Ok(total)
    }

    fn commit_batch(&self, from: u64, to: u64) -> Result<u64, i64> {
        let mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let missing = || (from..to).step_by(PAGE as usize).filter(|&at| mapper.translate_page(heap_page(at)).is_err()).count();
        let want = missing();
        if want == 0 {
            return Ok(0);
        }
        let mut frames = [None::<PhysFrame>; COMMIT_BATCH as usize];
        let free = |frames: &mut [Option<PhysFrame>]| {
            memory::with_frames(|f| {
                for frame in frames.iter_mut().filter_map(Option::take) {
                    unsafe { f.deallocate_frame(frame) };
                }
            })
        };
        for slot in frames.iter_mut().take(want) {
            match heap_frame() {
                Ok(frame) => *slot = Some(frame),
                Err(e) => {
                    free(&mut frames);
                    return Err(e);
                }
            }
        }
        let mut area = self.heap.lock();
        // (Counted again: only the server's own concurrent calls on the
        // same pages could change it, and they get no more frames.)
        let missing = missing() as u64;
        if missing > want as u64 {
            drop(area);
            free(&mut frames);
            return Err(ENOMEM);
        }
        // From the instance's own reserve first (committed already), the
        // rest as any commitment.
        let plan = area.charge.begin(missing, &HEAP_POOL);
        if plan.fresh > 0 && !memory::commit(plan.fresh) {
            memory::uncommit(area.charge.abort(plan, &HEAP_POOL, HEAP_RESERVE));
            // Said once per instance (a tree can make it happen at will).
            let tell = !core::mem::replace(&mut area.refused_told, true);
            drop(area);
            free(&mut frames);
            if tell {
                let (committed, limit) = memory::commit_stats();
                crate::printkln!(
                    "[linux] instance {}: heap commit of {} pages refused beyond its reserve (committed {}, dirty or pinned {}, limit {})",
                    self.id,
                    missing,
                    committed,
                    crate::fs::cache::unavailable_pages(),
                    limit
                );
            }
            return Err(ENOMEM);
        }
        // Each page committed exactly while it is mapped: what is mapped
        // when a table cannot be had stays (the caller decommits the
        // range), the rest of the commitment goes back.
        let (mut done, mut next) = (0, 0);
        let mut result = Ok(missing);
        for at in (from..to).step_by(PAGE as usize) {
            if mapper.translate_page(heap_page(at)).is_ok() {
                continue;
            }
            let frame = frames[next].take().expect("a frame for each missing page");
            next += 1;
            if let Err(e) = self.map(at, frame, PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE) {
                result = Err(e);
                break;
            }
            done += 1;
        }
        memory::uncommit(area.charge.end(plan, done, &HEAP_POOL, HEAP_RESERVE));
        if result.is_ok() {
            area.grow_reserve();
        }
        drop(area);
        free(&mut frames);
        result
    }

    /// `SYS_SHARED_DECOMMIT`: unmaps the committed pages of the heap area's
    /// [addr, addr + len) from every view, frees them and returns their
    /// commitment; how many there were.
    fn decommit_heap(&self, addr: u64, len: u64) -> Result<u64, i64> {
        let end = heap_range(addr, len)?;
        self.decommit_runs(&[(addr, (end - addr) / PAGE)])
    }

    /// `SYS_SHARED_DECOMMIT_RUNS`: `decommit_heap` for up to
    /// `DECOMMIT_RUNS_MAX` (address, length) pairs at `list` in the server's
    /// memory, with one shootdown per batch of pages.
    fn decommit_list(&self, list: u64, n: u64) -> Result<u64, i64> {
        if n == 0 || n > DECOMMIT_RUNS_MAX {
            return Err(EINVAL);
        }
        let mut raw = [0u8; 16 * DECOMMIT_RUNS_MAX as usize];
        super::uaccess::copy_from_server(list, &mut raw[..16 * n as usize])?;
        let mut runs = [(0u64, 0u64); DECOMMIT_RUNS_MAX as usize];
        for (run, pair) in runs.iter_mut().zip(raw[..16 * n as usize].chunks_exact(16)) {
            let addr = u64::from_le_bytes(pair[..8].try_into().unwrap_or_default());
            let len = u64::from_le_bytes(pair[8..].try_into().unwrap_or_default());
            *run = (addr, (heap_range(addr, len)? - addr) / PAGE);
        }
        self.decommit_runs(&runs[..n as usize])
    }

    /// Unmaps the runs (start, pages) of the heap area and returns their
    /// commitment; how many pages were committed.
    fn decommit_runs(&self, runs: &[(u64, u64)]) -> Result<u64, i64> {
        let mut area = self.heap.lock();
        let gone = self.unmap_runs(runs);
        // The heap area is mapped only by `commit_heap`, which counts each
        // page: more cannot go than it counted. Were the account broken,
        // the commitment stays held (never refunded twice).
        match area.charge.unmapped(gone, &HEAP_POOL, HEAP_RESERVE) {
            Ok(back) => memory::uncommit(back),
            Err(_) => {
                if !core::mem::replace(&mut area.refused_told, true) {
                    crate::printkln!("[linux] instance {}: heap account broken ({} pages unmapped)", self.id, gone);
                }
            }
        }
        Ok(gone)
    }

    /// The word at `addr` of the server's own memory (mapped, 4-aligned),
    /// for a wait: not in the range of the objects mapped into the region
    /// (`object_word`). A word of the image or a thread area stays mapped
    /// as long as the instance lives; one in the heap area holds a
    /// reference on its frame (a decommit of the page meanwhile unmaps it,
    /// the frame stays until the wait lets go of it).
    fn word(&self, addr: u64) -> Result<ServerWord, i64> {
        if (MAPS_BASE..HEAP_BASE).contains(&addr) {
            return Err(EFAULT);
        }
        let heap = (HEAP_BASE..THREADS_BASE).contains(&addr);
        // Translated and shared with the frames locked: a decommit frees
        // the frame (with them locked) only after it cleared the entry, so
        // an entry found here still holds its frame until it is shared.
        memory::with_frames(|f| {
            let (frame, word) = self.translate_word(addr)?;
            if heap {
                f.share(frame);
            }
            Ok(ServerWord { word, frame: heap.then_some(frame) })
        })
    }

    /// Whether a word at `addr` of the server's own memory is mapped (for a
    /// wake, which reads nothing there).
    fn word_mapped(&self, addr: u64) -> Result<(), i64> {
        if (MAPS_BASE..HEAP_BASE).contains(&addr) {
            return Err(EFAULT);
        }
        self.translate_word(addr).map(|_| ())
    }

    /// The word at `addr` of the region and its frame, mapped or not; the
    /// caller keeps what is mapped there.
    fn translate_word(&self, addr: u64) -> Result<(PhysFrame, *const core::sync::atomic::AtomicU32), i64> {
        if addr % 4 != 0 || !(SHARED_BASE..SHARED_END).contains(&addr) {
            return Err(EINVAL);
        }
        let mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        let frame = mapper.translate_page(page).map_err(|_| EFAULT)?;
        let phys = frame.start_address().as_u64() + addr % PAGE;
        Ok((frame, memory::phys_to_virt(phys) as *const core::sync::atomic::AtomicU32))
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
        // The pages made first, before the region's lock (a tmpfs page's
        // commit may wait for write-back, which the pager does: it may
        // need that lock to map objects of its own).
        object.make_pages(pages)?;
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

    /// Removes the entries of `pages` pages at `start` (those mapped), drops
    /// them from the TLBs of every address space showing the region, then
    /// lets go of the frames; how many there were.
    fn unmap_pages(&self, start: u64, pages: u64) -> u64 {
        self.unmap_runs(&[(start, pages)])
    }

    /// `unmap_pages` for several runs (start, pages) at once: in batches of
    /// `UNMAP_BATCH` frames, held on the stack (nothing is allocated), each
    /// with one shootdown over the span its pages lie in.
    fn unmap_runs(&self, runs: &[(u64, u64)]) -> u64 {
        let mut mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let mut frames = [None::<PhysFrame>; UNMAP_BATCH];
        let (mut n, mut lo, mut hi, mut gone) = (0, u64::MAX, 0, 0);
        let flush = |frames: &mut [Option<PhysFrame>], n: &mut usize, lo: &mut u64, hi: &mut u64| {
            if *n > 0 {
                self.shootdown(*lo, *hi);
                memory::with_frames(|f| {
                    for frame in frames[..*n].iter_mut().filter_map(Option::take) {
                        unsafe { f.deallocate_frame(frame) };
                    }
                });
            }
            (*n, *lo, *hi) = (0, u64::MAX, 0);
        };
        for &(start, pages) in runs {
            for at in (start..start + pages * PAGE).step_by(PAGE as usize) {
                if let Ok((frame, tlb)) = mapper.unmap(Page::<Size4KiB>::containing_address(VirtAddr::new(at))) {
                    tlb.ignore();
                    frames[n] = Some(frame);
                    n += 1;
                    gone += 1;
                    (lo, hi) = (lo.min(at), hi.max(at + PAGE));
                    if n == UNMAP_BATCH {
                        flush(&mut frames, &mut n, &mut lo, &mut hi);
                    }
                }
            }
        }
        flush(&mut frames, &mut n, &mut lo, &mut hi);
        gone
    }

    /// Drops [start, end) of the region from the TLBs of every address
    /// space showing it. Their list is copied out under its lock into room
    /// reserved before (nothing allocated under a spinlock); without room,
    /// every CPU flushes the range and every address space's entries (a
    /// flush of the kernel's mappings reaches them all).
    fn shootdown(&self, start: u64, end: u64) {
        let mut spaces: Vec<Arc<super::tlb::Tlb>> = Vec::new();
        loop {
            let want = self.spaces.lock().len();
            if spaces.try_reserve_exact(want).is_err() {
                super::tlb::shootdown_kernel(start, end);
                return;
            }
            let list = self.spaces.lock();
            if list.len() <= spaces.capacity() {
                spaces.extend(list.iter().filter_map(Weak::upgrade));
                break;
            }
        }
        // One request to the CPUs of them all (the region is the same in
        // every one).
        super::tlb::shootdown_many(&spaces, start, end);
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
        let (_, word) = self.translate_word(addr).ok()?;
        Some((m.object.clone(), addr - start, unsafe { &*word }))
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
        if let Object::Memory(cache) | Object::File(cache, _) = &object {
            cache.handle_opened();
        }
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

impl Instance {
    /// Queues an event for the service thread (none once it is gone or
    /// going: nobody would take it).
    pub(super) fn queue_event(&self, event: Event) {
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

/// A hold of the server's (`SYS_MO_HOLD`) kept by kernel objects (mappings,
/// a running program): the server learns when the last holder is gone
/// (`EVENT_RELEASE`).
pub struct Record {
    instance: Weak<Instance>,
    word: u64,
}

impl Drop for Record {
    fn drop(&mut self) {
        if let Some(instance) = self.instance.upgrade() {
            instance.queue_event(Event { kind: EVENT_RELEASE, a: self.word, b: 0 });
        }
    }
}

/// Gives `instance` the host grant (see `Instance::host`), or takes it.
pub fn host_grant(instance: &Instance, on: bool) {
    instance.host.store(on, core::sync::atomic::Ordering::Release);
}

/// Grants the console device to `instance` (None: the kernel's monitor, which
/// takes it back when the tree it started has ended its first process); the
/// instance that held it gets `EVENT_CONSOLE_LOST` (ADR 0007).
pub fn console_grant(instance: Option<&Arc<Instance>>) {
    let (id, chan) = instance.map_or((0, 0), |i| (i.id, i.pager_chan()));
    let old = crate::drivers::console_device::set_holder(id, chan);
    if old != 0 && old != id {
        if let Some(previous) = instances(None).into_iter().find(|i| i.id == old) {
            previous.queue_event(Event { kind: EVENT_CONSOLE_LOST, a: 0, b: 0 });
        }
    }
}

impl Drop for Instance {
    /// No address space shows the region any more: its frames go.
    fn drop(&mut self) {
        crate::drivers::console_device::release(self.id);
        memory::uncommit(self.heap.get_mut().charge.close(&HEAP_POOL));
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
    /// The thread's key (`slot | generation << 32`).
    key: u64,
    /// A program's thread whose end the service thread learns
    /// (`EVENT_THREAD_EXIT`): set once it was listed and started.
    announced: bool,
    /// The dying thread was told so (`REASON_EXIT`): it exits at its next
    /// `restricted_enter` if its server did not end it.
    exit_told: bool,
    /// The server's registers while the program runs.
    normal: Frame,
}

impl LinuxThread {
    /// A thread of `instance` whose program starts with the registers of
    /// `program`; returns it and the frame the thread starts with (the
    /// server's entry, on the thread's server stack, with `cookie` and the
    /// thread's key). `role`: `ROLE_PROGRAM`, or `ROLE_INIT` for the first
    /// thread of a new tree.
    pub fn new(instance: Arc<Instance>, program: &Frame, role: u64, cookie: u64) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state, key) = instance.thread()?;
        // A slot's count of server locks starts at 0 (a thread that died
        // holding some left it).
        unsafe { *((memory::phys_to_virt(state.start_address().as_u64()) as u64 + SERVER_LOCKS_OFFSET) as *mut u32) = 0 };
        let thread = LinuxThread::bare(instance, slot, state, key, false, false);
        save(program, thread.state());
        let mut start = thread.start(role);
        start.rdx = cookie;
        start.rcx = key;
        Ok((thread, start))
    }

    fn bare(instance: Arc<Instance>, slot: u64, state: PhysFrame, key: u64, pager: bool, events: bool) -> LinuxThread {
        LinuxThread {
            instance,
            slot,
            state,
            restricted: false,
            pager,
            events,
            key,
            announced: false,
            exit_told: false,
            normal: Frame::default(),
        }
    }

    /// The thread's key.
    pub fn key(&self) -> u64 {
        self.key
    }

    /// The thread is listed and about to start: the service thread will
    /// learn its end (room for the event reserved now).
    pub fn announce(&mut self, task: &Arc<super::task::Task>) -> Result<(), i64> {
        self.instance.announce()?;
        self.instance.tasks.lock().insert(self.slot, ((self.key >> 32) as u32, Arc::downgrade(task)));
        self.announced = true;
        Ok(())
    }

    /// A further service thread of `instance` in the pager's process, that
    /// serves no program and no page (`role`: `ROLE_WORKER`, where the
    /// server's collector of sockets in flight runs, or `ROLE_NET`, where
    /// its internet sockets' readiness and closing are handled), and its
    /// frame.
    pub fn service(instance: Arc<Instance>, role: u64) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state, key) = instance.thread()?;
        // A slot's count of server locks starts at 0 (a thread that died
        // holding some left it).
        unsafe { *((memory::phys_to_virt(state.start_address().as_u64()) as u64 + SERVER_LOCKS_OFFSET) as *mut u32) = 0 };
        let thread = LinuxThread::bare(instance, slot, state, key, true, false);
        let start = thread.start(role);
        Ok((thread, start))
    }

    /// The pager thread of `instance`, and the frame it starts with.
    pub fn pager(instance: Arc<Instance>) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state, key) = instance.thread()?;
        // A slot's count of server locks starts at 0 (a thread that died
        // holding some left it).
        unsafe { *((memory::phys_to_virt(state.start_address().as_u64()) as u64 + SERVER_LOCKS_OFFSET) as *mut u32) = 0 };
        let thread = LinuxThread::bare(instance, slot, state, key, true, true);
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
        !self.restricted
    }

    /// The kernel's address of the server's count of locks this thread
    /// holds (in its State page; see `restricted::SERVER_LOCKS_OFFSET`).
    pub fn locks_word(&self) -> u64 {
        memory::phys_to_virt(self.state.start_address().as_u64()) as u64 + SERVER_LOCKS_OFFSET
    }

    #[allow(clippy::mut_from_ref)]
    fn state(&self) -> &mut State {
        unsafe { &mut *(memory::phys_to_virt(self.state.start_address().as_u64()) as *mut State) }
    }
}

impl Drop for LinuxThread {
    fn drop(&mut self) {
        if self.events {
            self.instance.pager_gone();
        }
        // The slot goes back only now, with the task that owns this
        // thread (`Task::server_locks` points into its State page); the
        // service thread learns the end after (the task's reference to the
        // address space went when it exited).
        self.instance.release(self.slot);
        if self.announced {
            self.instance.thread_gone(self.key);
        }
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
        trap_vector: 0,
        trap_error: 0,
        trap_addr: 0,
        trap_kind: 0,
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

/// Whether the calling thread runs its Linux server (the normal view
/// loaded).
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
    use super::address_space::Backing;
    if nr == SYS_THREAD_EXIT {
        // Before anything of this call is on the stack: it does not return.
        if is_pager() {
            // A service thread ends only with its instance, or when the
            // instance broke and a lock it waits for is lost (`break_instance`):
            // then the service threads' process ends, and the instance with it.
            if instance_broken() {
                super::exit_group(super::kill::SIGKILL as i32);
            }
            return Err(EPERM);
        }
        let status = a[0] as i32;
        match a[1] {
            0 => super::exit_thread(status),
            EXIT_GROUP => super::exit_group(status),
            _ => return Err(EINVAL),
        }
    }
    let instance = instance()?;
    let page_aligned = |x: u64| x % PAGE == 0;
    let mm = || super::current_mm().ok_or(EINVAL);
    match nr {
        SYS_HANDLE_CLOSE => {
            let object = instance.handles.lock().objects.remove(&a[0]).ok_or(EBADF)?;
            // At a paged or cached object's last handle the server can no
            // longer answer for it (`PageCache::orphan`).
            if let Object::Memory(cache) | Object::File(cache, _) = &object {
                cache.handle_closed();
            }
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
            use super::vm::{anon_backing, place_and_map, Placement};
            let (handle, addr, offset, flags) = (a[0], a[1], a[3], a[5]);
            let prot = super::vm::prot(a[4])?;
            let all = MO_SHARED | MO_FIXED | MO_NOREPLACE | MO_NORESERVE | MO_POPULATE | MO_READONLY | MO_GROWSDOWN;
            let may_write = flags & MO_READONLY == 0;
            if !page_aligned(addr) || !page_aligned(offset) || flags & !all != 0 {
                return Err(EINVAL);
            }
            let len = a[2].checked_add(PAGE - 1).ok_or(EINVAL)? & !(PAGE - 1);
            if len == 0 || len >= USER_END || offset.checked_add(len).is_none() {
                return Err(EINVAL);
            }
            if flags & MO_GROWSDOWN != 0 {
                // A stack: anonymous, private, readable and writable, where
                // the server puts it, growing as far as `offset` says.
                if handle != 0 || flags != MO_GROWSDOWN | MO_FIXED || a[4] != 3 {
                    return Err(EINVAL);
                }
                let top = addr.checked_add(len).filter(|&e| e <= USER_END && addr != 0).ok_or(EINVAL)?;
                mm()?.lock().map_stack(top, len, offset).map_err(|_| ENOMEM)?;
                return Ok(addr as i64);
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
                    Object::Image(_) | Object::Channel(_) | Object::Process(_) => return Err(EINVAL),
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
        SYS_VM_REMAP => super::vm::remap(a[0], a[1], a[2], a[3], a[4]),
        SYS_VM_DISCARD => super::vm::discard(a[0], a[1]),
        SYS_VM_SYNC => super::vm::sync(a[0], a[1], a[2], a[3], a[4]),
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
            sync_wait(a[0], Some(&instance), 0, true);
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
        SYS_SERVER_WAIT => {
            let (list, n, deadline, flags) = (a[0], a[1], a[2], a[3]);
            if n == 0 || n > WAIT_MAX || flags & !FUTEX_INTERRUPTIBLE != 0 {
                return Err(EINVAL);
            }
            let mut raw = [0u8; 16 * WAIT_MAX as usize];
            super::uaccess::copy_from_server(list, &mut raw[..16 * n as usize])?;
            let pairs = || {
                raw[..16 * n as usize].chunks_exact(16).map(|pair| {
                    let addr = u64::from_le_bytes(pair[..8].try_into().unwrap_or_default());
                    (addr, u64::from_le_bytes(pair[8..].try_into().unwrap_or_default()) as u32)
                })
            };
            // The server's own words first, each held for the wait (`word`).
            let mut held = Vec::new();
            held.try_reserve_exact(n as usize).map_err(|_| ENOMEM)?;
            for (addr, _) in pairs() {
                if !(MAPS_BASE..HEAP_BASE).contains(&addr) {
                    held.push(instance.word(addr)?);
                }
            }
            let mut words = Vec::new();
            words.try_reserve_exact(n as usize).map_err(|_| ENOMEM)?;
            let mut own = held.iter();
            for (addr, val) in pairs() {
                // A word of an object mapped into the region has the object's key.
                words.push(if (MAPS_BASE..HEAP_BASE).contains(&addr) {
                    let (object, offset, word) = instance.object_word(addr).ok_or(EFAULT)?;
                    super::futex::WaitWord::object(object, offset, word, val)
                } else {
                    super::futex::WaitWord::server(Arc::as_ptr(&instance) as usize, addr, own.next().expect("held above").get(), val)
                });
            }
            let deadline = (deadline != 0).then_some(deadline);
            let ends = if flags & FUTEX_INTERRUPTIBLE != 0 { super::futex::Ends::Interrupted } else { super::futex::Ends::Dying };
            super::futex::server_waitv(&words, deadline, ends)
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
        SYS_SHARED_COMMIT => Ok(instance.commit_heap(a[0], a[1])? as i64),
        SYS_SHARED_DECOMMIT => Ok(instance.decommit_heap(a[0], a[1])? as i64),
        SYS_SHARED_DECOMMIT_RUNS => Ok(instance.decommit_list(a[0], a[1])? as i64),
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
                // The server's tmpfs files' reads and writes.
                SYS_MO_FILE_READ => {
                    let n = super::uaccess::read_to_user(buf, len, true, |chunk, done| cache.read_with(offset + done, chunk, fill))?;
                    Ok(n as i64)
                }
                SYS_MO_FILE_WRITE => {
                    offset.checked_add(len).ok_or(EFBIG)?;
                    // Too many dirty pages (of all, or of the file's owner's
                    // share): this writer waits for write-back, before each
                    // chunk after the first and after the last.
                    let n = super::uaccess::write_from_user(buf, len, |chunk, done| {
                        if done > 0 && cache.is_cached() {
                            crate::fs::cache::balance_dirty(&cache);
                        }
                        cache.write_with(offset + done, chunk, fill, backing)
                    });
                    if cache.is_cached() {
                        crate::fs::cache::balance_dirty(&cache);
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
        SYS_PROC_SELF..=SYS_RANDOM => process_call(&instance, nr, a),
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
        SYS_CLOCK_READ => super::clock::read(a[0]).map(|ns| ns as i64),
        // On the program's memory (in this view, below 64 TiB).
        SYS_FUTEX_WAIT | SYS_FUTEX_WAKE | SYS_FUTEX_REQUEUE => {
            if is_pager() {
                return Err(EPERM);
            }
            super::native::futex_call(nr, a)
        }
        SYS_THREAD_FS => {
            use x86_64::registers::model_specific::FsBase;
            if !serves_program() {
                return Err(EPERM);
            }
            let old = FsBase::read().as_u64();
            match a[0] {
                0 => {}
                1 if a[1] < USER_END => FsBase::write(x86_64::VirtAddr::new(a[1])),
                _ => return Err(EINVAL),
            }
            Ok(old as i64)
        }
        SYS_CLOCK_SET => {
            if !instance.host.load(core::sync::atomic::Ordering::Acquire) {
                return Err(EPERM);
            }
            crate::time::set_realtime(a[0]);
            Ok(0)
        }
        SYS_POWER => {
            if a[0] != POWER_OFF && a[0] != POWER_RESTART {
                return Err(EINVAL);
            }
            if !instance.host.load(core::sync::atomic::Ordering::Acquire) {
                return Err(EPERM);
            }
            // Nothing of this call may stay referenced: it does not return.
            drop(instance);
            power(a[0])
        }
        SYS_STACK_LIMIT => {
            let group = if a[0] == 0 { super::sched::current().group.clone() } else { container(&instance, a[0])?.group.clone() };
            group.stack_soft.store(a[1], core::sync::atomic::Ordering::Relaxed);
            Ok(0)
        }
        SYS_HOST_GRANTED => match instance.host.load(core::sync::atomic::Ordering::Acquire) {
            true => Ok(0),
            false => Err(EPERM),
        },
        SYS_FILE_PAGES => {
            let (used, limit) = crate::fs::cache::tmpfs_usage();
            let mut out = [0u8; 16];
            out[..8].copy_from_slice(&used.to_le_bytes());
            out[8..].copy_from_slice(&limit.to_le_bytes());
            super::uaccess::copy_to_server(a[0], &out)?;
            Ok(0)
        }
        SYS_SLEEP_UNTIL => match a[1] {
            0 => super::sleep_until(a[0]).map(|_| 0),
            SLEEP_NAP => {
                let now = crate::time::now();
                if a[0] > now.saturating_add(NAP_MAX) {
                    return Err(EINVAL);
                }
                super::sched::prepare_to_sleep().sleep_until(a[0]);
                Ok(0)
            }
            _ => Err(EINVAL),
        },
        SYS_YIELD => {
            super::yield_now();
            Ok(0)
        }
        SYS_SERVER_FUTEX_WAIT => {
            use super::futex::Ends;
            let (addr, val, deadline, flags) = (a[0], a[1] as u32, a[2], a[3]);
            let ends = match flags {
                0 => Ends::Dying,
                FUTEX_INTERRUPTIBLE => Ends::Interrupted,
                FUTEX_LOCK => Ends::Lock,
                FUTEX_SLEEPLOCK => Ends::SleepLock,
                _ => return Err(EINVAL),
            };
            let deadline = (deadline != 0).then_some(deadline);
            // A word of an object mapped into the region has the object's key.
            // (A lock's wait only on the server's own memory: a word another
            // party writes must never hold a dying thread.)
            if let Some((object, offset, word)) = instance.object_word(addr) {
                // (Nor a sleeping lock's: a broken instance's shake reaches only waits on the
                // server's own memory.)
                if matches!(ends, Ends::Lock | Ends::SleepLock) {
                    return Err(EINVAL);
                }
                return super::futex::object_wait(&object, offset, word, val, deadline, ends);
            }
            let word = instance.word(addr)?;
            let id = Arc::as_ptr(&instance) as usize;
            super::futex::server_wait(id, addr, word.get(), val, deadline, ends)
        }
        SYS_SERVER_FUTEX_WAKE => {
            if let Some((object, offset, _)) = instance.object_word(a[0]) {
                return Ok(super::futex::object_wake(&object, offset, a[1]));
            }
            instance.word_mapped(a[0])?;
            Ok(super::futex::server_wake(Arc::as_ptr(&instance) as usize, a[0], a[1]))
        }
        SYS_CHAN_CREATE => {
            instance.channel_added()?;
            let mapped = super::channel::Channel::new(a[0], a[2]).and_then(|c| instance.map_object(c.memory(), c.pages(), 1).map(|addr| (c, addr)));
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
            end.channel.connect(name, super::current_pid(), instance.id)?;
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
                        let _ = if mode == GRANT_FILL { object.filled(first, count, crate::fs::cache::Filled::Failed(EIO)) } else { object.redirty(first, count) };
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
            let cache = PageCache::cached(size, limit, pager, key, instance.cache_counts.clone())?;
            Ok(instance.insert(Object::File(cache, None))? as i64)
        }
        SYS_MO_FILLED | SYS_MO_REDIRTY => {
            let Object::File(cache, _) = instance.object(a[0])? else { return Err(EINVAL) };
            if !page_aligned(a[1]) {
                return Err(EINVAL);
            }
            let (first, count) = (a[1] / PAGE, a[2]);
            if nr == SYS_MO_FILLED {
                let outcome = match a[3] {
                    FILL_OK => crate::fs::cache::Filled::Ok,
                    FILL_FAILED => crate::fs::cache::Filled::Failed(EIO),
                    FILL_NOMEM => crate::fs::cache::Filled::Failed(ENOMEM),
                    FILL_AGAIN => crate::fs::cache::Filled::Again,
                    _ => return Err(EINVAL),
                };
                cache.filled(first, count, outcome)?;
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
        SYS_THREAD_NICE => {
            let t = thread_by_key(&instance, a[0])?;
            let old = t.nice.load(core::sync::atomic::Ordering::Relaxed);
            if a[1] != 0 {
                t.nice.store((a[2] as i64).clamp(-20, 19) as i8, core::sync::atomic::Ordering::Relaxed);
            }
            Ok(old as i64 + 20)
        }
        SYS_CONSOLE_READ => {
            use crate::drivers::console_device;
            if console_device::holder() != instance.id {
                return Err(EIO);
            }
            let mut buf = [0u8; 512];
            let n = console_device::read(&mut buf[..(a[1] as usize).min(512)]);
            // (A bad buffer of the server's loses the bytes.)
            super::uaccess::copy_to_server(a[0], &buf[..n])?;
            Ok(n as i64)
        }
        SYS_CONSOLE_WRITE => {
            use crate::drivers::console_device;
            if a[2] & !CONSOLE_ECHO != 0 {
                return Err(EINVAL);
            }
            // Not the holder: nothing to queue for, no ticket to take.
            if console_device::holder() != instance.id {
                return Err(EIO);
            }
            // The bytes first, into the kernel's memory, with no lock or turn
            // held: no copy (whatever it might wait for) ever happens in the turn,
            // which is only ever held within this call.
            let max = if a[2] & CONSOLE_ECHO != 0 { CONSOLE_ECHO_MAX } else { CONSOLE_WRITE_MAX };
            let n = a[1].min(max) as usize;
            let mut buf = Vec::new();
            buf.try_reserve_exact(n).map_err(|_| ENOMEM)?;
            buf.resize(n, 0);
            super::uaccess::copy_from_server(a[0], &mut buf)?;
            if a[2] & CONSOLE_ECHO != 0 {
                // Never waits (the service thread's echoes).
                return console_device::echo(instance.id, &buf).map(|n| n as i64);
            }
            console_device::write_all(instance.id, &buf).map(|_| n as i64)
        }
        SYS_CONSOLE_INFO => {
            if crate::drivers::console_device::holder() != instance.id {
                return Err(EIO);
            }
            let (cols, rows) = crate::drivers::console::size();
            let mut out = [0u8; 16];
            out[..8].copy_from_slice(&(cols as u64).to_le_bytes());
            out[8..].copy_from_slice(&(rows as u64).to_le_bytes());
            super::uaccess::copy_to_server(a[0], &out)?;
            Ok(0)
        }
        SYS_SYSTEM_INFO => {
            // The system's records only: the processes are the server's (R8).
            if a[0] != procproto::QUERY_SYSTEM {
                return Err(EINVAL);
            }
            super::query::server_query(a[0], a[2], a[3])
        }
        TEST_HOST => {
            if !crate::TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(ENOSYS);
            }
            let old = instance.host.swap(a[0] != 0, core::sync::atomic::Ordering::AcqRel);
            Ok(old as i64)
        }
        TEST_FUTEX_WATCH => {
            if !crate::TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(ENOSYS);
            }
            super::futex::test_watch(a[0], a[1] != 0)
        }
        TEST_SERVER_TICKS => {
            if !crate::TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(ENOSYS);
            }
            // (A longer name names no server.)
            if a[1] > 64 {
                return Err(ENAMETOOLONG);
            }
            let len = a[1] as usize;
            let mut name = [0u8; 64];
            super::uaccess::copy_from_server(a[0], &mut name[..len])?;
            let name = core::str::from_utf8(&name[..len]).map_err(|_| EINVAL)?;
            let pid = super::server_named(name).and_then(|s| s.pid()).ok_or(ESRCH)?;
            let group = super::group(pid).ok_or(ESRCH)?;
            let (user, system) = group.info.lock().cputime();
            Ok(((user + system) / 1_000_000) as i64)
        }
        TEST_KILL_SERVER => {
            if !crate::TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(ENOSYS);
            }
            // (A longer name names no server.)
            if a[1] > 64 {
                return Err(ENAMETOOLONG);
            }
            let len = a[1] as usize;
            let mut name = [0u8; 64];
            super::uaccess::copy_from_server(a[0], &mut name[..len])?;
            let name = core::str::from_utf8(&name[..len]).map_err(|_| EINVAL)?;
            let pid = super::server_named(name).and_then(|s| s.pid()).ok_or(ESRCH)?;
            super::kill::kill_pid(pid).map(|_| 0)
        }
        SYS_TEST_MODE => Ok(crate::TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) as i64),
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
        SYS_MO_UNMAP => super::vm::unmap(a[0], a[1]),
        SYS_MO_PROTECT => super::vm::protect(a[0], a[1], a[2]),
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
                        crate::fs::cache::balance_dirty(&cache);
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

/// The thread `key` of `instance`'s programs (0: the caller).
fn thread_by_key(instance: &Instance, key: u64) -> Result<Arc<super::task::Task>, i64> {
    if key == 0 {
        return Ok(super::sched::current_arc());
    }
    instance.task_of(key).ok_or(ESRCH)
}

/// The process behind the handle `handle`.
fn container(instance: &Instance, handle: u64) -> Result<Arc<Container>, i64> {
    match instance.object(handle)? {
        Object::Process(c) => Ok(c),
        _ => Err(EINVAL),
    }
}

/// Kicks a Linux program's thread (`SYS_THREAD_KICK`): its waits end, its
/// program stops at once (see `restricted::SYS_THREAD_KICK`).
pub fn kick_task(t: &Arc<super::task::Task>) {
    t.kicked.store(true, core::sync::atomic::Ordering::SeqCst);
    super::kill::kick(t);
}

/// Whether the calling thread's instance is broken (`break_instance`).
pub fn instance_broken() -> bool {
    with_current(|p| p.linux.as_ref().is_some_and(|l| l.instance.broken.load(core::sync::atomic::Ordering::Acquire)))
}

/// The calling thread's server failed (an exception in its own code): what
/// it held (its locks, sleeping ones too, references) is never let go of,
/// and the server's state is lost. The instance ends: every program thread is killed, and every wait
/// for one of the server's locks (`FUTEX_LOCK`, which nothing else ends)
/// ends with EINTR, on which the server ends the waiting thread. So no
/// thread of the instance waits for good on a lock a dead holder kept.
pub fn break_instance() {
    let Some(instance) = with_current(|p| p.linux.as_ref().map(|l| l.instance.clone())) else { return };
    if instance.broken.swap(true, core::sync::atomic::Ordering::AcqRel) {
        return;
    }
    crate::printkln!("[linux] the server failed: instance {} ends", instance.id);
    let tasks: Vec<Arc<super::task::Task>> = instance.tasks.lock().values().filter_map(|(_, t)| t.upgrade()).collect();
    for t in &tasks {
        kill_task(t);
    }
    super::futex::shake_server(Arc::as_ptr(&instance) as usize);
}

/// Kills a Linux program's thread: marked dying and kicked, it exits at its
/// next `restricted_enter`.
pub fn kill_task(t: &Arc<super::task::Task>) {
    t.killed.store(true, core::sync::atomic::Ordering::SeqCst);
    kick_task(t);
}

/// Whether the calling thread of a Linux program was kicked or must die:
/// its interruptible waits end (`kill::interrupted`).
pub fn kicked() -> bool {
    let me = super::sched::current();
    me.kicked.load(core::sync::atomic::Ordering::SeqCst) || dying()
}

/// Whether the calling thread of a Linux program must die: it was killed,
/// or its process is ending (`exit_group`, the kernel's kill).
pub fn dying() -> bool {
    let me = super::sched::current();
    me.killed.load(core::sync::atomic::Ordering::SeqCst) || *me.group.exit.lock() != super::kill::GroupExit::None
}

/// Whether the calling thread serves a program (not a service thread, not a
/// native task): its signals are its server's.
pub fn serves_program() -> bool {
    with_current(|p| p.linux.as_ref().is_some_and(|l| !l.pager))
}

/// The calls on processes and threads (phase R8, `SYS_PROC_SELF` to
/// `SYS_VM_FLOOR`; `SYS_THREAD_EXIT` is `server_call`'s).
fn process_call(instance: &Arc<Instance>, nr: u64, a: [u64; 6]) -> SysResult {
    use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
    // Service threads serve no program: they may only look at threads and
    // processes, and kick or kill them.
    let service = matches!(nr, SYS_THREAD_KICK | SYS_THREAD_KILL | SYS_PROC_INFO | SYS_THREAD_INFO | SYS_THREAD_AFFINITY | SYS_THREAD_NAME);
    if is_pager() && !service {
        return Err(EPERM);
    }
    match nr {
        SYS_PROC_SELF => {
            let group = super::sched::current().group.clone();
            let c = Arc::try_new(Container { group, fresh: spin::Mutex::new(None) }).map_err(|_| ENOMEM)?;
            Ok(instance.insert(Object::Process(c))? as i64)
        }
        SYS_PROC_CREATE => {
            use super::task::{Info, ThreadGroup};
            let flags = a[0];
            let vm = flags & (PROC_FORK | PROC_SHARE_VM);
            if flags & !(PROC_FORK | PROC_SHARE_VM) != 0 || (vm != PROC_FORK && vm != PROC_SHARE_VM) {
                return Err(EINVAL);
            }
            let pid = super::sched::reserve_pid()?;
            let mm = with_current(|p| p.mm.clone()).ok_or(EINVAL)?;
            let mm = if vm == PROC_SHARE_VM { mm } else { super::address_space::Mm::fork(&mm.lock()).map_err(|_| ENOMEM)? };
            let name = super::sched::current().group.info.lock().name.clone();
            let mut info = Info::new(name);
            info.mem = Some(mm.stats.clone());
            let group = ThreadGroup::new(pid.pid, info).ok_or(ENOMEM)?;
            // (A default: the server sets the new process's own, from its copy of the limits.)
            group.stack_soft.store(super::sched::current().group.stack_soft.load(Relaxed), Relaxed);
            group.instance.store(instance.id, Release);
            group.server_reaps.store(true, Relaxed);
            let c = Arc::try_new(Container { group, fresh: spin::Mutex::new(Some(Fresh { mm, pid })) }).map_err(|_| ENOMEM)?;
            Ok(instance.insert(Object::Process(c))? as i64)
        }
        SYS_THREAD_CREATE => thread_create(instance, a),
        SYS_THREAD_KICK => {
            kick_task(&thread_by_key(instance, a[0])?);
            Ok(0)
        }
        SYS_THREAD_KILL => {
            if a[0] == 0 {
                return Err(EINVAL);
            }
            kill_task(&thread_by_key(instance, a[0])?);
            Ok(0)
        }
        SYS_EXEC_SPACE => exec_space(instance, a[0], a[1], a[2]),
        SYS_PROC_INFO => {
            let group = if a[0] == 0 { super::sched::current().group.clone() } else { container(instance, a[0])?.group.clone() };
            let mut out = ProcInfo { start_ticks: group.start_ticks, kernel_pid: group.tgid, ..Default::default() };
            {
                let info = group.info.lock();
                (out.user_ns, out.system_ns) = info.cputime();
                if let Some(mem) = &info.mem {
                    out.pages = mem.pages.load(Relaxed);
                    out.virt_pages = mem.virt_pages.load(Relaxed);
                    out.peak_pages = mem.peak_pages.load(Relaxed);
                }
                out.peak_pages = out.peak_pages.max(info.peak_pages);
                out.threads = info.threads.len() as u64;
                out.running = info.threads.iter().filter(|t| matches!(t.state(), super::task::State::Running | super::task::State::Runnable)).count() as u64;
            }
            out.killed = group.killed_by_kernel.load(Acquire);
            super::uaccess::copy_to_server(a[1], as_bytes(&out))?;
            Ok(0)
        }
        SYS_THREAD_INFO => {
            let t = thread_by_key(instance, a[0])?;
            let (user_ns, system_ns) = t.cputime();
            let out = ThreadInfo {
                user_ns,
                system_ns,
                run_ns: t.runtime(),
                nice: t.nice.load(Relaxed) as i64,
                cpu: t.last_cpu.load(Relaxed) as u64,
                running: matches!(t.state(), super::task::State::Running | super::task::State::Runnable) as u64,
                kernel_tid: t.tid(),
            };
            super::uaccess::copy_to_server(a[1], as_bytes(&out))?;
            Ok(0)
        }
        SYS_THREAD_AFFINITY => {
            let t = thread_by_key(instance, a[0])?;
            let online = super::online_mask();
            let old = t.affinity.load(Relaxed) & online;
            if a[1] != 0 {
                let wanted = a[2] & online;
                if wanted == 0 {
                    return Err(EINVAL);
                }
                t.affinity.store(wanted, Relaxed);
                if core::ptr::eq(&*t, super::sched::current()) && !t.may_run_on(crate::smp::cpu().index) {
                    super::schedule();
                }
            }
            Ok(old as i64)
        }
        SYS_THREAD_CLEARTID => {
            if a[0] >= USER_END {
                return Err(EFAULT);
            }
            with_current(|p| p.clear_child_tid = a[0]);
            Ok(0)
        }
        SYS_THREAD_NAME => {
            let t = thread_by_key(instance, a[0])?;
            let mut buf = [0u8; 15];
            let n = (a[2] as usize).min(buf.len());
            super::uaccess::copy_from_server(a[1], &mut buf[..n])?;
            let name = alloc::string::String::from_utf8_lossy(&buf[..n]).into_owned();
            *t.comm.lock() = name;
            Ok(0)
        }
        SYS_INIT_ARGS => {
            let args = instance.init_args.lock().clone();
            if args.len() as u64 > a[1] {
                return Err(ERANGE);
            }
            super::uaccess::copy_to_server(a[0], &args)?;
            Ok(args.len() as i64)
        }
        SYS_VM_FLOOR => {
            if a[0] > USER_END {
                return Err(EINVAL);
            }
            super::current_mm().ok_or(EINVAL)?.lock().brk_end = a[0];
            Ok(0)
        }
        SYS_RANDOM => {
            let mut bytes = [0u8; 256];
            let n = (a[1] as usize).min(bytes.len());
            crate::random::fill(&mut bytes[..n]);
            super::uaccess::copy_to_server(a[0], &bytes[..n])?;
            Ok(n as i64)
        }
        _ => Err(ENOSYS),
    }
}

/// The bytes of a plain record.
fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// `SYS_THREAD_CREATE`: a thread in a new process (its first) or in the
/// caller's.
fn thread_create(instance: &Arc<Instance>, a: [u64; 6]) -> SysResult {
    use core::sync::atomic::Ordering::Relaxed;
    use x86_64::registers::model_specific::FsBase;
    let (process, state, flags, tls, ctid, cookie) = (a[0], a[1], a[2], a[3], a[4], a[5]);
    if flags & !THREAD_SETTLS != 0 {
        return Err(EINVAL);
    }
    if flags & THREAD_SETTLS != 0 && tls >= USER_END {
        return Err(EPERM);
    }
    if ctid >= USER_END {
        return Err(EFAULT);
    }
    // A dying caller makes no thread (Linux's copy_process with a fatal
    // signal pending): it would escape the kill that ends the caller's
    // process or the exec that ends its other threads.
    if dying() {
        return Err(EINTR);
    }
    let mut regs = State::default();
    let bytes = unsafe { core::slice::from_raw_parts_mut(&mut regs as *mut State as *mut u8, core::mem::size_of::<State>()) };
    super::uaccess::copy_from_server(state, bytes)?;
    let mut program = Frame::default();
    load(&regs, &mut program)?;
    let me = super::sched::current();
    let (group, mm, pid) = if process == 0 {
        let mm = with_current(|p| p.mm.clone());
        (me.group.clone(), mm.ok_or(EINVAL)?, super::sched::reserve_pid()?)
    } else {
        let c = container(instance, process)?;
        let fresh = c.fresh.lock().take().ok_or(EBUSY)?;
        (c.group.clone(), fresh.mm, fresh.pid)
    };
    let (linux, start) = LinuxThread::new(instance.clone(), &program, ROLE_PROGRAM, cookie)?;
    let key = linux.key();
    let own = super::Process {
        mm: Some(mm),
        io_bitmap: None,
        server: None,
        clear_child_tid: ctid,
        linux: Some(linux),
        copy_fixup: None,
    };
    let comm = me.comm.lock().clone();
    let child = super::new_task(pid.pid, group, comm, own, start)?;
    // The program resumes with the caller's FPU registers (the server
    // touches none) and TLS pointer, or the new one.
    unsafe {
        let cpu = child.cpu_state();
        cpu.fs_base = if flags & THREAD_SETTLS != 0 { tls } else { FsBase::read().as_u64() };
        core::arch::asm!("fxsave64 [{}]", in(reg) cpu.fpu.0.as_mut_ptr(), options(nostack));
    }
    child.affinity.store(me.affinity.load(Relaxed), Relaxed);
    child.nice.store(me.nice.load(Relaxed), Relaxed);
    // Before it can run: its end will be reported. (Should listing it fail,
    // its end is reported all the same, for a key the server never saw.)
    unsafe { child.own() }.linux.as_mut().expect("set above").announce(&child)?;
    pid.insert(child.clone())?;
    super::sched::start(child);
    Ok(key as i64)
}

/// `SYS_EXEC_SPACE`: execve's point of no return (see
/// `restricted::SYS_EXEC_SPACE`).
fn exec_space(instance: &Arc<Instance>, exe: u64, name: u64, len: u64) -> SysResult {
    use super::address_space::{AddressSpace, Hold, Mm};
    use core::sync::atomic::Ordering::Relaxed;
    use x86_64::registers::model_specific::FsBase;
    let me = super::sched::current();
    if me.group.info.lock().threads.len() != 1 {
        return Err(EBUSY);
    }
    let hold: Option<Hold> = match exe {
        0 => None,
        h => match instance.object(h)? {
            Object::File(_, hold) => hold.map(|h| h as Hold),
            _ => return Err(EINVAL),
        },
    };
    let mut buf = [0u8; 15];
    let n = (len as usize).min(buf.len());
    super::uaccess::copy_from_server(name, &mut buf[..n])?;
    let name = alloc::string::String::from_utf8_lossy(&buf[..n]).into_owned();
    let mut space = AddressSpace::new().ok_or(ENOMEM)?;
    space.exe = hold;
    space.attach(instance.clone(), true).map_err(|_| ENOMEM)?;
    let mm = Mm::new(space).ok_or(ENOMEM)?;
    let comm = name.clone();
    // The point of no return: from here on the old program is gone. Its
    // CLONE_CHILD_CLEARTID word is cleared and woken as at an exit (Linux's
    // exec_mm_release).
    let ctid = with_current(|p| core::mem::take(&mut p.clear_child_tid));
    if ctid != 0 && super::uaccess::write(ctid, 0u32).is_ok() {
        let _ = super::futex::wake_one(ctid);
    }
    {
        let mut info = me.group.info.lock();
        let old_name = core::mem::replace(&mut info.name, name);
        if let Some(old) = info.mem.replace(mm.stats.clone()) {
            info.peak_pages = info.peak_pages.max(old.peak_pages.load(Relaxed));
        }
        drop(info);
        drop(old_name);
    }
    let old_comm = core::mem::replace(&mut *me.comm.lock(), comm);
    drop(old_comm);
    let old_mm = with_current(|p| {
        // The server runs (the normal view): the new space's normal view
        // shows the same region.
        super::tlb::switch(p.mm.as_ref().map(|m| &*m.tlb), Some(&mm.tlb), true);
        p.copy_fixup = None;
        p.mm.replace(mm)
    });
    // The old address space goes here (unless a vfork parent shares it).
    drop(old_mm);
    FsBase::write(x86_64::VirtAddr::new(0));
    let initial = super::task::FpuState::initial();
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) initial.0.as_ptr(), options(nostack)) };
    Ok(0)
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
    trap_with(f, reason, [0; 4]);
}

/// The program raised an exception the kernel does not resolve: back to
/// the server with `REASON_EXCEPTION` and what happened (vector, error
/// code, address, `FAULT_*` of a page fault).
pub fn trap_exception(f: &mut Frame, vector: u64, error: u64, addr: u64, kind: u64) {
    trap_with(f, REASON_EXCEPTION, [vector, error, addr, kind]);
}

fn trap_with(f: &mut Frame, reason: u64, detail: [u64; 4]) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        with_current(|p| {
            let l = p.linux.as_mut().expect("a Linux thread");
            let state = l.state();
            save(f, state);
            [state.trap_vector, state.trap_error, state.trap_addr, state.trap_kind] = detail;
            *f = l.normal;
            f.rax = reason;
            l.restricted = false;
        });
        switch_view(true);
    });
}

/// Whether the calling thread is a Linux program's whose program runs (an
/// interrupt from it) and that was kicked or must die: it goes back to its
/// server (`REASON_KICK`) on the way out of the interrupt.
pub fn kick_pending() -> bool {
    mode() == Some(true) && kicked()
}

/// restricted_enter() from the server (`f`): runs the program, unless the
/// thread must die (`REASON_EXIT` once; then it exits here, where its
/// server holds no lock) or was kicked (`REASON_KICK` at once, the flag
/// cleared).
pub fn enter(f: &mut Frame) -> Result<(), i64> {
    if with_current(|p| p.linux.as_ref().is_some_and(|l| l.pager)) {
        return Err(EPERM);
    }
    if dying() {
        // Once to the server, which lets go of what the thread holds and
        // exits it on a clean stack; a second time (a server that did not)
        // the kernel ends the thread itself.
        let told = with_current(|p| p.linux.as_mut().map(|l| core::mem::replace(&mut l.exit_told, true)));
        if told == Some(false) {
            f.rax = REASON_EXIT;
            return Ok(());
        }
        super::exit_thread(super::kill::SIGKILL as i32);
    }
    let me = super::sched::current();
    if me.kicked.swap(false, core::sync::atomic::Ordering::SeqCst) {
        f.rax = REASON_KICK;
        return Ok(());
    }
    x86_64::instructions::interrupts::without_interrupts(|| {
        with_current(|p| {
            let l = p.linux.as_mut().expect("a Linux thread");
            let mut program = Frame::default();
            load(l.state(), &mut program)?;
            l.normal = *f;
            *f = program;
            l.restricted = true;
            Ok::<_, i64>(())
        })
        .inspect(|_| switch_view(false))
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
            if let Some(key) = q.exits.pop_front() {
                q.announced -= 1;
                return Some(Some(Event { kind: EVENT_THREAD_EXIT, a: key, b: 0 }));
            }
            if let Some(mut e) = q.requests.pop_front() {
                match e.kind {
                    EVENT_PAGE => {
                        q.queued.remove(&(e.a, e.b / PAGE));
                    }
                    EVENT_WRITEBACK => q.writeback_queued = false,
                    EVENT_SYNC => {
                        // Answers every ticket asked so far.
                        q.sync_queued = false;
                        e.a = q.sync_wanted;
                    }
                    _ => {}
                }
                return Some(Some(e));
            }
            // Console input the keyboard's interrupt announced (by a flag:
            // it takes no lock of the instance).
            if crate::drivers::console_device::take_event(instance.id) {
                return Some(Some(Event { kind: EVENT_CONSOLE, a: 0, b: 0 }));
            }
            // Memory is short (`post_shrink`, by a flag likewise), once
            // `SHRINK_INTERVAL` passed since the last time.
            let mut shrink_due = 0;
            if instance.shrink.load(core::sync::atomic::Ordering::Acquire) != 0 {
                let now = crate::time::now();
                let at = instance.shrunk_at.load(core::sync::atomic::Ordering::Relaxed);
                shrink_due = if at == 0 { now } else { at + SHRINK_INTERVAL };
                if now >= shrink_due {
                    let pages = instance.shrink.swap(0, core::sync::atomic::Ordering::AcqRel);
                    if pages != 0 {
                        instance.shrunk_at.store(now, core::sync::atomic::Ordering::Relaxed);
                        return Some(Some(Event { kind: EVENT_SHRINK, a: pages, b: 0 }));
                    }
                }
            }
            if q.closing {
                if q.closing_told {
                    return Some(None);
                }
                q.closing_told = true;
                return Some(Some(Event { kind: EVENT_CLOSING, a: 0, b: 0 }));
            }
            drop(q);
            if deadline != 0 && crate::time::now() >= deadline {
                return Some(Some(Event { kind: EVENT_TIMER, a: 0, b: 0 }));
            }
            // Until the deadline or a shrink's turn, whichever is first.
            match (deadline, shrink_due) {
                (0, 0) => wait.sleep(),
                (0, until) | (until, 0) => wait.sleep_until(until),
                (a, b) => wait.sleep_until(a.min(b)),
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
                        EVENT_THREAD_EXIT => {
                            // (Into the room it came from.)
                            q.announced += 1;
                            q.exits.push_front(event.a);
                        }
                        EVENT_PAGE if !q.queued.insert((event.a, event.b / PAGE)) => {}
                        EVENT_TIMER => {}
                        EVENT_SHRINK => {
                            instance.shrink.fetch_max(event.a, core::sync::atomic::Ordering::AcqRel);
                        }
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
