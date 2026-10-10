//! TLB coherence: which CPUs use an address space, and shootdowns.
//!
//! Threads of one process run on several CPUs at once, each with the same
//! page tables loaded and their entries cached in its TLB. When a mapping
//! is removed, write-protected or moved, every such CPU must drop the stale
//! entries before the change counts as done: until then a thread could
//! still write through a stale entry into a frame that is shared (fork) or
//! already freed. Frames are therefore released only after the shootdown.
//!
//! Each address space keeps the set of CPUs that have it loaded. A CPU
//! joins before it loads CR3 and leaves after it loaded another one, and a
//! shooter reads the set only after changing the page tables (with a full
//! fence in between), so a CPU it misses loads the new entries anyway.
//!
//! One shootdown runs at a time. Its targets acknowledge from the IPI
//! handler; a CPU waiting with interrupts off (for the shooter lock, or for
//! acknowledgements) serves a request addressed to it itself, so two
//! shooters can never wait for each other.
//!
//! **PCIDs.** Where the CPU has them, a switch of address spaces keeps the
//! TLB: entries are tagged with a process-context id, and each CPU keeps
//! its last `ASIDS` address spaces under ids 1..=ASIDS (id 0 is the
//! kernel's own tables, loaded with a flush). An address space is known by
//! a unique `Tlb::id`, never by its page table's frame, which may be reused
//! by another address space. A CPU that does not have an address space
//! loaded still holds its entries under its id, so a shootdown cannot reach
//! them by IPI: instead every shootdown first advances the address space's
//! `generation`, and a CPU loads an id without a flush only if it last
//! flushed it at the current generation. Ordering: the shooter changes the
//! page tables, advances the generation, then reads the CPU set; a
//! switching CPU joins the set, then reads the generation (both SeqCst). So
//! either the shooter sees the CPU (and its IPI arrives once the CPU has
//! loaded the tables, since switches run with interrupts off), or the CPU
//! sees the new generation and flushes.

use crate::smp;
use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
use x86_64::instructions::tlb;
use x86_64::registers::control::{Cr3, Cr4, Cr4Flags};
use x86_64::structures::paging::PhysFrame;
use x86_64::VirtAddr;

/// Up to this many pages are flushed one by one; more flush everything.
const MAX_SINGLE: u64 = 32;

/// Address spaces a CPU keeps tagged in its TLB (PCIDs 1..=ASIDS).
const ASIDS: usize = 6;

/// Set at boot if the CPUs have PCIDs (and CR4.PCIDE is on).
static PCID: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// An address space as the TLB sees it: its top-level table, its unique
/// id, the generation of its last shootdown and the CPUs that have it
/// loaded.
///
/// A Linux process's address space has a second top-level table, the
/// normal view (its tables plus the Linux server's shared region, see
/// `linux`). Both views map the program's memory through the same tables,
/// so they are one address space here: one generation, one CPU set (a CPU
/// with either view loaded), and a shootdown flushes whichever is loaded.
/// Each view has its own PCID slot.
pub struct Tlb {
    pub l4: PhysFrame,
    normal: spin::Once<PhysFrame>,
    id: u64,
    generation: AtomicU64,
    cpus: AtomicU64,
}

impl Tlb {
    pub fn new(l4: PhysFrame) -> Tlb {
        Tlb {
            l4,
            normal: spin::Once::new(),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            generation: AtomicU64::new(0),
            cpus: AtomicU64::new(0),
        }
    }

    /// Sets the normal view's top-level table (once, before anyone runs in
    /// the address space).
    pub fn set_normal(&self, l4: PhysFrame) {
        self.normal.call_once(|| l4);
    }

    /// The table of the normal view (`normal`) or of the program's.
    fn root(&self, normal: bool) -> PhysFrame {
        match self.normal.get() {
            Some(&n) if normal => n,
            _ => self.l4,
        }
    }

    /// (program view, normal view) top-level tables, for diagnostics.
    pub fn roots(&self) -> (u64, u64) {
        (self.l4.start_address().as_u64(), self.normal.get().map_or(0, |n| n.start_address().as_u64()))
    }

    fn active_here(&self) -> bool {
        let cr3 = Cr3::read().0;
        cr3 == self.l4 || self.normal.get() == Some(&cr3)
    }
}

/// What a PCID slot holds: one view of one address space. Never 0 (an
/// empty slot), since `Tlb::id` starts at 1.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ViewKey(u64);

impl ViewKey {
    fn new(t: &Tlb, normal_view: bool) -> ViewKey {
        ViewKey(t.id * 2 + normal_view as u64)
    }

    const EMPTY: ViewKey = ViewKey(0);
}

/// A CPU's PCID slots: which view of which address space each holds, and
/// the generation it was flushed at. Only the CPU itself touches it,
/// and only with interrupts off (the shootdown IPI changes it too). A slot
/// is marked current only by a load of CR3 that flushes its PCID.
pub struct AsidCache {
    slots: UnsafeCell<[(ViewKey, u64); ASIDS]>,
    next: UnsafeCell<usize>,
}

impl AsidCache {
    pub const fn new() -> Self {
        AsidCache { slots: UnsafeCell::new([(ViewKey::EMPTY, 0); ASIDS]), next: UnsafeCell::new(0) }
    }

    #[allow(clippy::mut_from_ref)]
    fn slots(&self) -> &mut [(ViewKey, u64); ASIDS] {
        debug_assert!(!x86_64::instructions::interrupts::are_enabled());
        unsafe { &mut *self.slots.get() }
    }

    /// The PCID for view `key` at `generation`, and whether its entries are
    /// current (loadable without a flush).
    fn assign(&self, key: ViewKey, generation: u64) -> (u16, bool) {
        let slots = self.slots();
        if let Some(i) = slots.iter().position(|s| s.0 == key) {
            let current = slots[i].1 == generation;
            slots[i].1 = generation;
            return (i as u16 + 1, current);
        }
        let next = unsafe { &mut *self.next.get() };
        let i = *next;
        *next = (i + 1) % ASIDS;
        slots[i] = (key, generation);
        (i as u16 + 1, false)
    }

    /// Every slot's entries must be flushed before they are used again
    /// (kernel mappings changed; they are cached under every PCID).
    fn invalidate_all(&self) {
        for s in self.slots().iter_mut() {
            *s = (ViewKey::EMPTY, 0);
        }
    }
}

// Each CPU's cache is used by that CPU alone.
unsafe impl Sync for AsidCache {}

/// Turns PCIDs on for the calling CPU if it has them (CPUID.1:ECX bit 17).
/// Every CPU calls it at start-up, while its CR3 has PCID 0 (as CR4.PCIDE
/// requires); the bootstrap CPU decides for all.
pub fn init_cpu(bootstrap: bool) {
    if bootstrap {
        let ecx = core::arch::x86_64::__cpuid(1).ecx;
        PCID.store(ecx & 1 << 17 != 0, Ordering::Relaxed);
        let mode = if ecx & 1 << 17 != 0 { "tagged by PCID" } else { "flushed on every switch (no PCIDs)" };
        crate::printkln!("[tlb] address spaces {}", mode);
    }
    if PCID.load(Ordering::Relaxed) {
        unsafe { Cr4::update(|f| f.insert(Cr4Flags::PCID)) };
    }
}

pub fn pcids() -> bool {
    PCID.load(Ordering::Relaxed)
}

/// Loads CR3: `l4` with `pcid`, keeping that PCID's entries if `keep`.
fn load_cr3(l4: PhysFrame, pcid: u16, keep: bool) {
    let value = l4.start_address().as_u64() | pcid as u64 | if keep { 1 << 63 } else { 0 };
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) value, options(nostack, preserves_flags)) };
}

fn my_bit() -> u64 {
    1 << smp::cpu().index
}

/// Moves this CPU from address space `from` to `to` (None: the kernel's
/// own tables), to its normal view if `normal` (and it has one). Called
/// with interrupts off.
pub fn switch(from: Option<&Tlb>, to: Option<&Tlb>, normal: bool) {
    if let (Some(a), Some(b)) = (from, to) {
        if a.id == b.id {
            // Threads of one process: only the view may differ.
            if Cr3::read().0 != b.root(normal) {
                load(b, normal);
            }
            return;
        }
    }
    let bit = my_bit();
    match to {
        Some(t) => {
            t.cpus.fetch_or(bit, Ordering::SeqCst);
            load(t, normal);
        }
        None => {
            crate::counters::add(|c| &c.address_space_switches, 1);
            load_cr3(crate::memory::kernel_l4(), 0, false)
        }
    }
    if let Some(f) = from {
        f.cpus.fetch_and(!bit, Ordering::SeqCst);
    }
}

/// Loads a view of `t`, which this CPU has joined (`cpus`).
fn load(t: &Tlb, normal: bool) {
    crate::counters::add(|c| &c.address_space_switches, 1);
    let root = t.root(normal);
    if pcids() {
        let generation = t.generation.load(Ordering::SeqCst);
        let (pcid, current) = smp::cpu().asids.assign(ViewKey::new(t, root != t.l4), generation);
        load_cr3(root, pcid, current);
    } else {
        load_cr3(root, 0, false);
    }
}

/// Flushes the TLB entries of the current PCID (`tlb::flush_all` would
/// reload CR3 with PCID 0 and leave them).
fn flush_current() {
    let cr3: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov cr3, {}", in(reg) cr3 & !(1 << 63), options(nostack, preserves_flags));
    }
}

fn flush_local(start: u64, end: u64) {
    let pages = (end - start) / 4096;
    if pages > MAX_SINGLE {
        flush_current();
    } else {
        for i in 0..pages {
            tlb::flush(VirtAddr::new(start + i * 4096));
        }
    }
}

/// The shootdown in progress: whose entries, which range, and the CPUs
/// that still have to flush.
struct Request {
    l4: AtomicU64,
    /// The normal view's table, if the address space has one (else 0).
    normal: AtomicU64,
    start: AtomicU64,
    end: AtomicU64,
    pending: AtomicU64,
}

static REQUEST: Request =
    Request { l4: AtomicU64::new(0), normal: AtomicU64::new(0), start: AtomicU64::new(0), end: AtomicU64::new(0), pending: AtomicU64::new(0) };
static SHOOTER: spin::Mutex<()> = spin::Mutex::new(());

/// `Request::l4` of a flush of kernel mappings, which every CPU does.
const KERNEL: u64 = 0;
/// `Request::l4` of a flush of several address spaces' range
/// (`shootdown_many`): each target flushes it whatever it has loaded (no
/// table is at address 1).
const ANY: u64 = 1;

/// Flushes for the current request if it targets this CPU (from the IPI
/// handler, and from every wait of the protocol).
pub fn serve() {
    x86_64::instructions::interrupts::without_interrupts(serve_here);
}

fn serve_here() {
    let bit = my_bit();
    if REQUEST.pending.load(Ordering::Acquire) & bit == 0 {
        return;
    }
    let l4 = REQUEST.l4.load(Ordering::Relaxed);
    let cr3 = Cr3::read().0.start_address().as_u64();
    if l4 == KERNEL || l4 == ANY || cr3 == l4 || cr3 == REQUEST.normal.load(Ordering::Relaxed) {
        flush_local(REQUEST.start.load(Ordering::Relaxed), REQUEST.end.load(Ordering::Relaxed));
    }
    // Kernel mappings are cached under every PCID.
    if l4 == KERNEL {
        smp::cpu().asids.invalidate_all();
    }
    REQUEST.pending.fetch_and(!bit, Ordering::AcqRel);
}

/// Drops the entries for [start, end) of `space` from every TLB. Called
/// after the page tables changed and before the frames they mapped are
/// reused; never with a spinlock held that another CPU could spin on with
/// interrupts off.
pub fn shootdown(space: &Tlb, start: u64, end: u64) {
    if start >= end {
        return;
    }
    // CPUs that do not have it loaded flush its PCID when they load it.
    // This CPU too: its slot stays at the old generation even though it
    // flushes the range now, because the slot can only be marked current
    // safely by a load that flushes everything (a concurrent shooter may
    // have advanced the generation for a range this CPU never flushed).
    space.generation.fetch_add(1, Ordering::SeqCst);
    if space.active_here() {
        flush_local(start, end);
    }
    // Pairs with the fetch_or in `switch`: either that CPU is in the set,
    // or it reads the new generation after it joined.
    fence(Ordering::SeqCst);
    let others = space.cpus.load(Ordering::SeqCst) & !my_bit();
    let normal = space.normal.get().map_or(0, |n| n.start_address().as_u64());
    request(space.l4.start_address().as_u64(), normal, others, start, end);
}

/// `shootdown` of [start, end) in several address spaces at once (the
/// Linux server's region, which every address space of an instance shows):
/// one request to the union of the CPUs that have any of them, each of
/// which flushes the range in what it has loaded (harmless in another
/// space); entries under other PCIDs go with each space's new generation.
pub fn shootdown_many(spaces: &[alloc::sync::Arc<Tlb>], start: u64, end: u64) {
    if start >= end || spaces.is_empty() {
        return;
    }
    let mut here = false;
    for space in spaces {
        space.generation.fetch_add(1, Ordering::SeqCst);
        here |= space.active_here();
    }
    if here {
        flush_local(start, end);
    }
    // (Pairs with the fetch_or in `switch`, as in `shootdown`.)
    fence(Ordering::SeqCst);
    let others = spaces.iter().fold(0, |m, s| m | s.cpus.load(Ordering::SeqCst)) & !my_bit();
    request(ANY, ANY, others, start, end);
}

/// Drops the entries for [start, end) of the kernel's own mappings (shared
/// by every address space) from every CPU's TLB. The kernel does not mark
/// them global, so a full flush reaches them too.
pub fn shootdown_kernel(start: u64, end: u64) {
    flush_local(start, end);
    x86_64::instructions::interrupts::without_interrupts(|| smp::cpu().asids.invalidate_all());
    fence(Ordering::SeqCst);
    let all = (0..smp::MAX_CPUS).filter(|&i| smp::by_index(i).is_some()).fold(0u64, |m, i| m | 1 << i);
    request(KERNEL, KERNEL, all & !my_bit(), start, end);
}

/// Has the CPUs in `targets` flush [start, end) of the address space
/// with top-level table `l4` (and normal view `normal`, or 0) and waits.
fn request(l4: u64, normal: u64, targets: u64, start: u64, end: u64) {
    let others = targets;
    if others == 0 {
        return;
    }
    let _shooter = loop {
        if let Some(g) = SHOOTER.try_lock() {
            break g;
        }
        serve();
        core::hint::spin_loop();
    };
    REQUEST.l4.store(l4, Ordering::Relaxed);
    REQUEST.normal.store(normal, Ordering::Relaxed);
    REQUEST.start.store(start, Ordering::Relaxed);
    REQUEST.end.store(end, Ordering::Relaxed);
    REQUEST.pending.store(others, Ordering::Release);
    for i in 0..smp::MAX_CPUS {
        if others & (1 << i) != 0 {
            if let Some(cpu) = smp::by_index(i) {
                crate::interrupts::apic::ipi::send_vector(cpu.apic_id(), crate::interrupts::apic::ipi::TLB_VECTOR);
            } else {
                REQUEST.pending.fetch_and(!(1 << i), Ordering::AcqRel);
            }
        }
    }
    while REQUEST.pending.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }
}
