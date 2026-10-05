pub mod gdt;
pub mod handlers;

use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::structures::idt::InterruptDescriptorTable;

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = 40;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Keyboard = PIC_1_OFFSET + 1,
}

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

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
        idt[InterruptIndex::Keyboard as u8]
            .set_handler_fn(handlers::keyboard_interrupt_handler);
        idt
    };
}

pub fn init() {
    gdt::init();
    IDT.load();
    unsafe {
        let mut pics = PICS.lock();
        pics.initialize();
        // Nur IRQ1 (Keyboard) zulassen; IRQ0 (Timer) hat keinen Handler.
        pics.write_masks(0b1111_1101, 0b1111_1111);
        drain_ps2_output();
    }
    x86_64::instructions::interrupts::enable();
}

// Ein vom BIOS liegengelassenes Byte blockiert neue IRQ1-Flanken, bis es gelesen wird.
unsafe fn drain_ps2_output() {
    use x86_64::instructions::port::Port;
    let mut status: Port<u8> = Port::new(0x64);
    let mut data: Port<u8> = Port::new(0x60);
    while status.read() & 1 != 0 {
        data.read();
    }
}
