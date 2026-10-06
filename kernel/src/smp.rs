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
    pub sched: crate::process::sched::CpuSched,
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
            sched: crate::process::sched::CpuSched::new(),
        }
    }

    /// The descriptor tables of this CPU. Only the CPU itself may use them.
    #[allow(clippy::mut_from_ref)]
    pub fn tables(&self) -> &mut CpuTables {
        unsafe { &mut *self.tables.get() }
    }

    pub fn apic_id(&self) -> u8 {
        unsafe { *self.apic_id.get() }
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

// ------------------------------------------------------------ start-up

// Real-mode start-up code for the other CPUs, copied to a page below 1 MiB.
// A STARTUP IPI starts a CPU at that page with CS = page >> 4, IP = 0. It
// goes straight to long mode (PAE, the kernel's CR3, EFER.LME and NXE,
// CR0.PG and PE), far-jumps into 64-bit code in the same page (which is
// identity-mapped) and from there to `ap_entry` on its own stack. The
// fields at the end are filled in for each CPU.
core::arch::global_asm!(
    ".pushsection .text.ap_trampoline, \"ax\"",
    ".balign 4096",
    ".code16",
    ".global ap_trampoline_start",
    "ap_trampoline_start:",
    "    cli",
    "    cld",
    "    mov %cs, %ax",
    "    mov %ax, %ds",
    "    lgdtl (ap_gdtr - ap_trampoline_start)",
    "    mov %cr4, %eax",
    "    or $0x20, %eax",
    "    mov %eax, %cr4",
    "    movl (ap_cr3 - ap_trampoline_start), %eax",
    "    mov %eax, %cr3",
    "    mov $0xc0000080, %ecx",
    "    rdmsr",
    "    or $0x900, %eax",
    "    wrmsr",
    "    mov %cr0, %eax",
    "    or $0x80000001, %eax",
    "    mov %eax, %cr0",
    // ljmpl $0x08, $<physical address of ap_long_mode>
    "    .byte 0x66, 0xea",
    ".global ap_ljmp_target",
    "ap_ljmp_target:",
    "    .long 0",
    "    .word 0x08",
    ".code64",
    ".global ap_long_mode",
    "ap_long_mode:",
    "    mov $0x10, %ax",
    "    mov %ax, %ds",
    "    mov %ax, %es",
    "    mov %ax, %ss",
    "    xor %ax, %ax",
    "    mov %ax, %fs",
    "    mov %ax, %gs",
    "    mov ap_stack(%rip), %rsp",
    "    mov ap_arg(%rip), %rdi",
    "    mov ap_entry_addr(%rip), %rax",
    "    jmp *%rax",
    ".balign 8",
    ".global ap_gdt",
    "ap_gdt:",
    "    .quad 0",
    "    .quad 0x00af9a000000ffff",
    "    .quad 0x00cf92000000ffff",
    ".global ap_gdtr",
    "ap_gdtr:",
    "    .word 23",
    "    .long 0",
    ".balign 8",
    ".global ap_cr3",
    "ap_cr3: .quad 0",
    ".global ap_stack",
    "ap_stack: .quad 0",
    ".global ap_entry_addr",
    "ap_entry_addr: .quad 0",
    ".global ap_arg",
    "ap_arg: .quad 0",
    ".global ap_trampoline_end",
    "ap_trampoline_end:",
    ".popsection",
    options(att_syntax)
);

unsafe extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static ap_ljmp_target: u8;
    static ap_long_mode: u8;
    static ap_gdt: u8;
    static ap_gdtr: u8;
    static ap_cr3: u8;
    static ap_stack: u8;
    static ap_entry_addr: u8;
    static ap_arg: u8;
}

/// Offset of a trampoline symbol from its start.
fn tramp_offset(sym: *const u8) -> usize {
    sym as usize - (&raw const ap_trampoline_start) as usize
}

/// A zeroed, heap-allocated `Cpu` block, initialized in place (it is too
/// large for a temporary on the stack) and never freed.
fn new_block(index: usize) -> &'static mut Cpu {
    use alloc::alloc::{alloc_zeroed, Layout};
    let layout = Layout::new::<Cpu>();
    let p = unsafe { alloc_zeroed(layout) } as *mut Cpu;
    assert!(!p.is_null(), "out of memory for a CPU block");
    unsafe {
        core::ptr::addr_of_mut!((*p).index).write(index);
        CpuTables::init_in_place(UnsafeCell::raw_get(core::ptr::addr_of_mut!((*p).tables)));
        core::ptr::addr_of_mut!((*p).sched).write(crate::process::sched::CpuSched::new());
        &mut *p
    }
}

/// Starts every other CPU the MADT lists, one after another.
pub fn start_aps() {
    start_all();
    crate::printkln!("[smp] {} of {} CPUs online", online(), crate::drivers::acpi::madt().cpus.len());
}

fn start_all() {
    let Some(low) = crate::memory::low_frame() else {
        return crate::printkln!("[smp] no page below 1 MiB for the start-up code; using one CPU");
    };
    if let Err(e) = crate::memory::identity_map(low) {
        return crate::printkln!("[smp] {}; using one CPU", e);
    }
    let cr3 = crate::memory::kernel_l4().start_address().as_u64();
    if cr3 >= 1 << 32 {
        return crate::printkln!("[smp] kernel page table above 4 GiB; using one CPU");
    }
    let start = &raw const ap_trampoline_start;
    let len = (&raw const ap_trampoline_end) as usize - start as usize;
    let page = crate::memory::phys_to_virt(low);
    let field = |sym: *const u8| unsafe { page.add(tramp_offset(sym)) };
    let bsp_id = cpu().apic_id();
    let mut index = 1;
    for &apic_id in &crate::drivers::acpi::madt().cpus {
        if apic_id == bsp_id {
            continue;
        }
        if index >= MAX_CPUS {
            crate::printkln!("[smp] more than {} CPUs; the rest stay off", MAX_CPUS);
            break;
        }
        let block = new_block(index);
        let stack = unsafe { alloc::boxed::Box::<crate::process::task::KernelStack>::new_zeroed().assume_init() };
        let stack_top = stack.0.as_ptr() as u64 + stack.0.len() as u64;
        // The stack becomes the CPU's idle task's stack for good.
        core::mem::forget(stack);
        unsafe {
            core::ptr::copy_nonoverlapping(start, page, len);
            (field(&raw const ap_ljmp_target) as *mut u32).write_unaligned((low + tramp_offset(&raw const ap_long_mode) as u64) as u32);
            (field(&raw const ap_gdtr).add(2) as *mut u32).write_unaligned((low + tramp_offset(&raw const ap_gdt) as u64) as u32);
            (field(&raw const ap_cr3) as *mut u64).write(cr3);
            (field(&raw const ap_stack) as *mut u64).write(stack_top);
            (field(&raw const ap_entry_addr) as *mut u64).write(ap_entry as *const () as u64);
            (field(&raw const ap_arg) as *mut u64).write(block as *mut Cpu as u64);
        }
        let before = online();
        use crate::interrupts::apic::{ipi, pit_delay_us};
        ipi::send_init(apic_id);
        pit_delay_us(10_000);
        for _ in 0..2 {
            ipi::send_startup(apic_id, (low >> 12) as u8);
            pit_delay_us(200);
            if online() > before {
                break;
            }
        }
        // Up to 100 ms for the CPU to come up.
        for _ in 0..100 {
            if online() > before {
                break;
            }
            pit_delay_us(1_000);
        }
        if online() > before {
            index += 1;
        } else {
            crate::printkln!("[smp] CPU with APIC id {} did not start", apic_id);
        }
    }
}

/// First Rust code of another CPU, on its own stack, with the kernel's
/// page table but no GDT, IDT or GS of its own yet.
extern "C" fn ap_entry(block: *mut Cpu) -> ! {
    let block = unsafe { &mut *block };
    let index = block.index;
    unsafe {
        use x86_64::registers::control::{Cr0, Cr0Flags};
        Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT));
    }
    activate(block);
    crate::interrupts::init_ap();
    crate::process::enable_sse();
    crate::process::syscall::init();
    crate::interrupts::apic::init_local();
    set_apic_id(crate::interrupts::apic::id());
    let idle = alloc::sync::Arc::new(crate::process::task::Task::idle_task(index, None, 0));
    crate::process::sched::set_initial(idle.clone(), idle);
    crate::process::sched::idle_loop()
}
