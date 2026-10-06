pub mod apic;
pub mod gdt;
pub mod handlers;

use lazy_static::lazy_static;
use x86_64::structures::idt::InterruptDescriptorTable;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(handlers::breakpoint_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(handlers::double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt.page_fault.set_handler_fn(handlers::page_fault_handler);
        idt.general_protection_fault
            .set_handler_fn(handlers::general_protection_handler);
        idt.invalid_opcode.set_handler_fn(handlers::invalid_opcode_handler);
        unsafe {
            idt[apic::TIMER_VECTOR]
                .set_handler_addr(x86_64::VirtAddr::new(handlers::timer_entry as *const () as u64));
        }
        for (gsi, &handler) in handlers::GSI_HANDLERS.iter().enumerate() {
            idt[apic::IRQ_BASE + gsi as u8].set_handler_fn(handler);
        }
        idt[apic::SPURIOUS_VECTOR].set_handler_fn(handlers::spurious_handler);
        idt
    };
}

/// GDT, TSS and IDT of the bootstrap CPU. The interrupt controllers come
/// later (`init_controllers`), once memory and ACPI are available.
pub fn init() {
    gdt::init();
    IDT.load();
    unsafe { drain_ps2_output() };
}

/// Local and I/O APIC; the keyboard interrupt is routed and unmasked, the
/// other ISA lines are routed but stay masked until a driver enables them.
pub fn init_controllers(rsdp: u64, hz: u64) {
    crate::drivers::acpi::init(rsdp).expect("ACPI");
    apic::init(hz).expect("APIC");
    for irq in 1..16 {
        if irq != 2 {
            apic::route_isa(irq, irq != KEYBOARD_IRQ);
        }
    }
}

pub const KEYBOARD_IRQ: u8 = 1;

// A byte left behind by the BIOS blocks new IRQ1 edges until it is read.
unsafe fn drain_ps2_output() {
    use x86_64::instructions::port::Port;
    let mut status: Port<u8> = Port::new(0x64);
    let mut data: Port<u8> = Port::new(0x60);
    while status.read() & 1 != 0 {
        data.read();
    }
}
