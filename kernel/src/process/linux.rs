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
use alloc::sync::Arc;
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
    /// A top-level table with only the shared slot, to map into the region.
    view: PhysFrame,
    /// The region's third-level table.
    pdpt: PhysFrame,
    /// The server's entry point for a new thread.
    entry: u64,
    slots: spin::Mutex<Slots>,
    /// The kernel objects the server holds, by handle.
    handles: spin::Mutex<Handles>,
    /// Pages of paged objects that threads wait for, for the pager thread.
    pager: spin::Mutex<PagerQueue>,
    /// Address spaces of the tree's programs: when the last goes, so does
    /// the pager's process.
    programs: core::sync::atomic::AtomicUsize,
    /// The top of the server's heap, and the pages committed for it.
    heap_end: spin::Mutex<u64>,
    heap_pages: core::sync::atomic::AtomicU64,
}

/// Pages wanted from the pager. A request is queued once until the pager
/// takes it; after that, a thread that still waits (the page did not come,
/// or failed) asks again.
struct PagerQueue {
    requests: alloc::collections::VecDeque<(u64, u64)>,
    queued: alloc::collections::BTreeSet<(u64, u64)>,
    /// The tree has no program left: the pager's process ends.
    closing: bool,
    /// The pager's process is gone: no page will come any more.
    dead: bool,
}

/// A kernel object the Linux server refers to by handle.
#[derive(Clone)]
enum Object {
    /// A memory object (pages that mappings and reads and writes share).
    Memory(Arc<PageCache>),
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
        let mut instance = Instance {
            view,
            pdpt,
            entry: 0,
            slots: spin::Mutex::new(Slots { next: 0, free: Vec::new(), states: BTreeMap::new() }),
            handles: spin::Mutex::new(Handles { next: 1, objects: BTreeMap::new() }),
            pager: spin::Mutex::new(PagerQueue {
                requests: alloc::collections::VecDeque::new(),
                queued: alloc::collections::BTreeSet::new(),
                closing: false,
                dead: false,
            }),
            programs: core::sync::atomic::AtomicUsize::new(0),
            heap_end: spin::Mutex::new(HEAP_BASE),
            heap_pages: core::sync::atomic::AtomicU64::new(0),
        };
        instance.entry = instance.load(image)?;
        Arc::try_new(instance).map_err(|_| ENOMEM)
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
                if ph.vaddr < IMAGE_BASE || end > THREADS_BASE || file_end > size || ph.filesz > ph.memsz {
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
        if elf.entry < IMAGE_BASE || elf.entry >= THREADS_BASE {
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

    /// The word at `addr` of the server's memory (mapped, 4-aligned).
    fn word(&self, addr: u64) -> Result<&core::sync::atomic::AtomicU32, i64> {
        if addr % 4 != 0 || !(SHARED_BASE..SHARED_END).contains(&addr) {
            return Err(EINVAL);
        }
        let mapper = unsafe { OffsetPageTable::new(table_at(self.view), memory::phys_offset()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        let frame = mapper.translate_page(page).map_err(|_| EFAULT)?;
        let phys = frame.start_address().as_u64() + addr % PAGE;
        // Server memory stays mapped as long as the instance lives.
        Ok(unsafe { &*(memory::phys_to_virt(phys) as *const core::sync::atomic::AtomicU32) })
    }

    /// A new handle for `object`.
    fn insert(&self, object: Object) -> Result<u64, i64> {
        let mut h = self.handles.lock();
        if h.objects.len() >= MAX_HANDLES {
            return Err(EMFILE);
        }
        let handle = h.next;
        h.next += 1;
        h.objects.insert(handle, object);
        Ok(handle)
    }

    fn object(&self, handle: u64) -> Result<Object, i64> {
        self.handles.lock().objects.get(&handle).cloned().ok_or(EBADF)
    }

    fn memory(&self, handle: u64) -> Result<Arc<PageCache>, i64> {
        match self.object(handle)? {
            Object::Memory(m) => Ok(m),
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

    /// The pager's process ended: what is waited for will not come.
    fn pager_gone(&self) {
        let mut q = self.pager.lock();
        q.dead = true;
        q.requests.clear();
        q.queued.clear();
        drop(q);
        super::wakeup(self.answer_chan());
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
            q.requests.push_back((key, index));
            drop(q);
            super::wakeup(self.pager_chan());
        }
        true
    }

    fn wait_chan(&self) -> usize {
        self.answer_chan()
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
    }
}

/// A thread of a Linux program as restricted mode sees it.
pub struct LinuxThread {
    instance: Arc<Instance>,
    slot: u64,
    state: PhysFrame,
    /// Running the program (true) or the server.
    pub restricted: bool,
    /// The instance's pager thread, which serves no program.
    pager: bool,
    /// The server waits in `legacy_syscall`: the kernel runs the call in
    /// the program's view.
    in_legacy: bool,
    /// The server's registers while the program runs.
    normal: Frame,
}

impl LinuxThread {
    /// A thread of `instance` whose program starts with the registers of
    /// `program`; returns it and the frame the thread starts with (the
    /// server's entry, on the thread's server stack).
    pub fn new(instance: Arc<Instance>, program: &Frame) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state) = instance.thread()?;
        let thread = LinuxThread { instance, slot, state, restricted: false, pager: false, in_legacy: false, normal: Frame::default() };
        save(program, thread.state());
        let start = thread.start(ROLE_PROGRAM);
        Ok((thread, start))
    }

    /// The pager thread of `instance`, and the frame it starts with.
    pub fn pager(instance: Arc<Instance>) -> Result<(LinuxThread, Frame), i64> {
        let (slot, state) = instance.thread()?;
        let thread = LinuxThread { instance, slot, state, restricted: false, pager: true, in_legacy: false, normal: Frame::default() };
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
        if self.pager {
            self.instance.pager_gone();
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
            instance.handles.lock().objects.remove(&a[0]).map(|_| 0).ok_or(EBADF)
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
            let (handle, addr, offset, flags) = (a[0], a[1], a[3], a[5]);
            let len = range(addr, a[2])?;
            let prot = prot(a[4])?;
            if !page_aligned(offset) || flags & !MO_SHARED != 0 {
                return Err(EINVAL);
            }
            let cache = instance.memory(handle)?;
            offset.checked_add(len).filter(|&e| e <= cache.size()).ok_or(EINVAL)?;
            let shared = flags & MO_SHARED != 0;
            let backing = Backing::File { cache, offset, shared, may_write: true, _file: None };
            let mm = mm()?;
            mm.lock().map(addr, len, prot, backing, false).map_err(|_| ENOMEM)?;
            Ok(addr as i64)
        }
        SYS_MO_CREATE_PAGED => {
            let pages = a[0];
            if pages == 0 || pages > USER_END / PAGE {
                return Err(EINVAL);
            }
            let pager: alloc::sync::Weak<dyn crate::fs::cache::Pager> = Arc::downgrade(&instance) as _;
            let object = PageCache::paged(pages, pager, a[1])?;
            Ok(instance.insert(Object::Memory(object))? as i64)
        }
        SYS_PAGER_WAIT => pager_wait(&instance, a[0]),
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
        SYS_SERVER_FUTEX_WAIT => {
            let (addr, val, deadline, flags) = (a[0], a[1] as u32, a[2], a[3]);
            if flags & !FUTEX_INTERRUPTIBLE != 0 {
                return Err(EINVAL);
            }
            let word = instance.word(addr)?;
            let id = Arc::as_ptr(&instance) as usize;
            super::futex::server_wait(id, addr, word, val, (deadline != 0).then_some(deadline), flags & FUTEX_INTERRUPTIBLE != 0)
        }
        SYS_SERVER_FUTEX_WAKE => {
            instance.word(a[0])?;
            Ok(super::futex::server_wake(Arc::as_ptr(&instance) as usize, a[0], a[1]))
        }
        SYS_MO_UNMAP => {
            let len = range(a[0], a[1])?;
            mm()?.lock().unmap(a[0], len);
            Ok(0)
        }
        SYS_MO_PROTECT => {
            let len = range(a[0], a[1])?;
            let prot = prot(a[2])?;
            mm()?.lock().protect(a[0], len, prot).map_err(|_| ENOMEM)?;
            Ok(0)
        }
        SYS_MO_READ | SYS_MO_WRITE => {
            let (handle, offset, buf, len) = (a[0], a[1], a[2], a[3]);
            let cache = instance.memory(handle)?;
            // Within the object, a page-sized piece at a time.
            let end = offset.checked_add(len).filter(|&e| e <= cache.size()).ok_or(EINVAL)?;
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
                    cache.write(offset + done, &chunk[..n])?;
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
            save(f, l.state());
            *f = l.normal;
            f.rax = reason;
            l.restricted = false;
        });
        switch_view(true);
    });
}

/// restricted_enter() from the server (`f`): runs the program.
pub fn enter(f: &mut Frame) -> Result<(), i64> {
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
            Ok::<_, i64>(())
        })?;
        switch_view(false);
        Ok(())
    })
}

/// legacy_syscall() from the server: the kernel's Linux implementation
/// carries out the system call in `State`, which then holds the result (or
/// the new program after execve, or a signal frame).
///
/// The call runs in the program's view, as it would without the server:
/// the server's memory is then out of reach of the kernel's Linux code
/// altogether, not only because every user pointer is checked against
/// 64 TiB (`uaccess`).
pub fn legacy() -> Result<(), i64> {
    let state = with_current(|p| p.linux.as_ref().filter(|l| !l.pager).map(|l| *l.state())).ok_or(EPERM)?;
    let mut program = Frame::default();
    load(&state, &mut program)?;
    set_legacy(true);
    super::syscall::dispatch_linux(&mut program);
    set_legacy(false);
    with_current(|p| {
        if let Some(l) = p.linux.as_ref() {
            save(&program, l.state());
        }
    });
    Ok(())
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

/// pager_wait(request): the next page a thread waits for, written to the
/// server's memory at `out`. The pager's process ends here when the tree
/// has no program left.
fn pager_wait(instance: &Arc<Instance>, out: u64) -> SysResult {
    if !with_current(|p| p.linux.as_ref().is_some_and(|l| l.pager)) {
        return Err(EPERM);
    }
    loop {
        let next = x86_64::instructions::interrupts::without_interrupts(|| {
            let wait = super::sched::prepare_to_wait(instance.pager_chan());
            let mut q = instance.pager.lock();
            if let Some(r) = q.requests.pop_front() {
                q.queued.remove(&r);
                return Some(Some(r));
            }
            if q.closing {
                return Some(None);
            }
            drop(q);
            wait.sleep();
            None
        });
        match next {
            Some(Some((key, index))) => {
                let request = PagerRequest { key, offset: index * PAGE };
                let bytes = unsafe {
                    core::slice::from_raw_parts(&request as *const PagerRequest as *const u8, core::mem::size_of::<PagerRequest>())
                };
                if let Err(e) = super::uaccess::copy_to_server(out, bytes) {
                    // Still wanted: next time.
                    let mut q = instance.pager.lock();
                    if q.queued.insert((key, index)) {
                        q.requests.push_front((key, index));
                    }
                    return Err(e);
                }
                return Ok(0);
            }
            Some(None) => super::exit_group(0),
            None => {}
        }
    }
}
