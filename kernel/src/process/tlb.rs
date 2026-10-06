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

use crate::smp;
use core::sync::atomic::{fence, AtomicU64, Ordering};
use x86_64::instructions::tlb;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::PhysFrame;
use x86_64::VirtAddr;

/// Up to this many pages are flushed one by one; more flush everything.
const MAX_SINGLE: u64 = 32;

/// An address space as the TLB sees it: its top-level table and the CPUs
/// that have it loaded.
pub struct Tlb {
    pub l4: PhysFrame,
    cpus: AtomicU64,
}

impl Tlb {
    pub fn new(l4: PhysFrame) -> Tlb {
        Tlb { l4, cpus: AtomicU64::new(0) }
    }

    fn active_here(&self) -> bool {
        Cr3::read().0 == self.l4
    }
}

fn my_bit() -> u64 {
    1 << smp::cpu().index
}

/// Moves this CPU from address space `from` to `to` (None: the kernel's
/// own tables). Called with interrupts off.
pub fn switch(from: Option<&Tlb>, to: Option<&Tlb>) {
    if let (Some(a), Some(b)) = (from, to) {
        if a.l4 == b.l4 {
            return;
        }
    }
    let bit = my_bit();
    match to {
        Some(t) => {
            t.cpus.fetch_or(bit, Ordering::SeqCst);
            unsafe { Cr3::write(t.l4, Cr3Flags::empty()) };
        }
        None => unsafe { Cr3::write(crate::memory::kernel_l4(), Cr3Flags::empty()) },
    }
    if let Some(f) = from {
        f.cpus.fetch_and(!bit, Ordering::SeqCst);
    }
}

fn flush_local(start: u64, end: u64) {
    let pages = (end - start) / 4096;
    if pages > MAX_SINGLE {
        tlb::flush_all();
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
    start: AtomicU64,
    end: AtomicU64,
    pending: AtomicU64,
}

static REQUEST: Request = Request { l4: AtomicU64::new(0), start: AtomicU64::new(0), end: AtomicU64::new(0), pending: AtomicU64::new(0) };
static SHOOTER: spin::Mutex<()> = spin::Mutex::new(());

/// Flushes for the current request if it targets this CPU (from the IPI
/// handler, and from every wait of the protocol).
pub fn serve() {
    let bit = my_bit();
    if REQUEST.pending.load(Ordering::Acquire) & bit == 0 {
        return;
    }
    if Cr3::read().0.start_address().as_u64() == REQUEST.l4.load(Ordering::Relaxed) {
        flush_local(REQUEST.start.load(Ordering::Relaxed), REQUEST.end.load(Ordering::Relaxed));
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
    if space.active_here() {
        flush_local(start, end);
    }
    // Pairs with the fetch_or in `switch`: either that CPU is in the set,
    // or it loads CR3 after the page table changes above.
    fence(Ordering::SeqCst);
    let others = space.cpus.load(Ordering::SeqCst) & !my_bit();
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
    REQUEST.l4.store(space.l4.start_address().as_u64(), Ordering::Relaxed);
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
