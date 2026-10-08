//! Interrupt setup: the IDT, the interrupt controllers (local and I/O APIC from ACPI, the 8259
//! PICs masked) and the per-CPU setup of the other CPUs.

pub mod apic;
pub mod entry;
pub mod gdt;
pub mod handlers;

use lazy_static::lazy_static;
use x86_64::structures::idt::InterruptDescriptorTable;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        unsafe {
            idt.double_fault
                .set_handler_fn(handlers::double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt.non_maskable_interrupt.set_handler_fn(handlers::nmi_handler);
        for &(vector, stub) in entry::stubs() {
            let addr = x86_64::VirtAddr::new(stub as usize as u64);
            // The crate only exposes exceptions through named fields.
            let options = unsafe {
                match vector {
                    0 => idt.divide_error.set_handler_addr(addr),
                    1 => idt.debug.set_handler_addr(addr),
                    3 => idt.breakpoint.set_handler_addr(addr),
                    4 => idt.overflow.set_handler_addr(addr),
                    5 => idt.bound_range_exceeded.set_handler_addr(addr),
                    6 => idt.invalid_opcode.set_handler_addr(addr),
                    7 => idt.device_not_available.set_handler_addr(addr),
                    10 => idt.invalid_tss.set_handler_addr(addr),
                    11 => idt.segment_not_present.set_handler_addr(addr),
                    12 => idt.stack_segment_fault.set_handler_addr(addr),
                    13 => idt.general_protection_fault.set_handler_addr(addr),
                    14 => idt.page_fault.set_handler_addr(addr),
                    16 => idt.x87_floating_point.set_handler_addr(addr),
                    17 => idt.alignment_check.set_handler_addr(addr),
                    18 => idt.machine_check.set_handler_addr(addr),
                    19 => idt.simd_floating_point.set_handler_addr(addr),
                    20 => idt.virtualization.set_handler_addr(addr),
                    21 => idt.cp_protection_exception.set_handler_addr(addr),
                    v => idt[v].set_handler_addr(addr),
                }
            };
            // int3 from user space (debuggers, abort()) must be allowed.
            if vector == 3 {
                options.set_privilege_level(x86_64::PrivilegeLevel::Ring3);
            }
        }
        idt
    };
}

/// Loads the (shared) IDT on the calling CPU. The interrupt controllers come
/// later (`init_controllers`), once memory and ACPI are available.
pub fn init() {
    IDT.load();
    unsafe { drain_ps2_output() };
}

/// Local and I/O APIC; the keyboard interrupt is routed and unmasked, the
/// other ISA lines are routed but stay masked until a driver enables them.
pub fn init_controllers(rsdp: u64) {
    crate::drivers::acpi::init(rsdp).expect("ACPI");
    apic::init().expect("APIC");
    for irq in 1..16 {
        if irq != 2 {
            apic::route_isa(irq, irq != KEYBOARD_IRQ);
        }
    }
}

pub const KEYBOARD_IRQ: u8 = 1;

/// Loads the shared IDT on another CPU.
pub fn init_ap() {
    IDT.load();
}

// A byte left behind by the BIOS blocks new IRQ1 edges until it is read.
unsafe fn drain_ps2_output() {
    use x86_64::instructions::port::Port;
    let mut status: Port<u8> = Port::new(0x64);
    let mut data: Port<u8> = Port::new(0x60);
    while status.read() & 1 != 0 {
        data.read();
    }
}
