//! Per-CPU data. While a CPU runs kernel code, its GS base points to its
//! `Cpu` block; entries from user mode execute `swapgs` to get there (the
//! user's GS base is always 0 and waits in IA32_KERNEL_GS_BASE meanwhile).

use crate::interrupts::gdt::CpuTables;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};
use x86_64::registers::model_specific::{GsBase, KernelGsBase};
use x86_64::VirtAddr;

/// Most CPUs oxidenix brings up.
pub const MAX_CPUS: usize = 16;

/// One CPU. The first fields are read by the assembly entry paths at fixed
/// offsets (see `syscall_entry`), so their order must not change.
#[repr(C, align(64))]
pub struct Cpu {
    /// gs:0 — this block itself, so `cpu()` is one load.
    this: *const Cpu,
    /// gs:8 — the user stack pointer while a syscall switches stacks.
    pub user_rsp: UnsafeCell<u64>,
    /// gs:16 — top of the running task's kernel stack (syscall entry).
    pub kernel_stack: UnsafeCell<u64>,
    /// Index in `CPUS` (0 = bootstrap CPU).
    pub index: usize,
    pub apic_id: UnsafeCell<u8>,
    tables: UnsafeCell<CpuTables>,
}

/// Offsets for the assembly entry code.
pub const USER_RSP_OFFSET: usize = 8;
pub const KERNEL_STACK_OFFSET: usize = 16;

// The block is shared only by pointer; every mutable field is touched by
// its own CPU alone, with interrupts disabled.
unsafe impl Sync for Cpu {}

impl Cpu {
    const fn new(index: usize) -> Self {
        Cpu {
            this: core::ptr::null(),
            user_rsp: UnsafeCell::new(0),
            kernel_stack: UnsafeCell::new(0),
            index,
            apic_id: UnsafeCell::new(0),
            tables: UnsafeCell::new(CpuTables::new()),
        }
    }

    /// The descriptor tables of this CPU. Only the CPU itself may use them.
    #[allow(clippy::mut_from_ref)]
    pub fn tables(&self) -> &mut CpuTables {
        unsafe { &mut *self.tables.get() }
    }

    /// Kernel stack for the next entry from user mode (syscall or interrupt).
    pub fn set_kernel_stack(&self, top: u64) {
        unsafe { *self.kernel_stack.get() = top };
        self.tables().set_kernel_stack(top);
    }
}

/// The bootstrap CPU's block is static: it is needed before the heap exists.
static mut BSP: Cpu = Cpu::new(0);

/// Blocks of every started CPU, by index.
static CPUS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static ONLINE: AtomicUsize = AtomicUsize::new(0);

/// The calling CPU's block.
#[inline]
pub fn cpu() -> &'static Cpu {
    let p: *const Cpu;
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) p, options(nostack, readonly, preserves_flags)) };
    unsafe { &*p }
}

/// Makes `block` the calling CPU's: loads its GDT and TSS and points GS at it.
fn activate(block: &'static mut Cpu) {
    block.this = block as *const Cpu;
    let ptr = block as *const Cpu as u64;
    unsafe { &mut *block.tables.get() }.load();
    GsBase::write(VirtAddr::new(ptr));
    KernelGsBase::write(VirtAddr::new(0));
    CPUS[block.index].store(ptr as usize, Ordering::Release);
    ONLINE.fetch_add(1, Ordering::AcqRel);
}

/// Sets up the bootstrap CPU; the first thing the kernel does.
pub fn init_bsp() {
    activate(unsafe { &mut *(&raw mut BSP) });
}

/// Records the bootstrap CPU's local APIC id once the APIC is up.
pub fn set_apic_id(id: u8) {
    unsafe { *cpu().apic_id.get() = id };
}

/// Number of CPUs that are running.
pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// The block of CPU `index`, if it runs.
pub fn by_index(index: usize) -> Option<&'static Cpu> {
    let p = CPUS.get(index)?.load(Ordering::Acquire);
    (p != 0).then(|| unsafe { &*(p as *const Cpu) })
}
