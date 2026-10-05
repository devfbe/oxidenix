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
    Timer = PIC_1_OFFSET,        // 32
    Keyboard = PIC_1_OFFSET + 1, // 33
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
        idt[InterruptIndex::Keyboard as u8]
            .set_handler_fn(handlers::keyboard_interrupt_handler);
        idt
    };
}

pub fn init() {
    gdt::init();
    IDT.load();
    unsafe {
        PICS.lock().initialize();
    }
    x86_64::instructions::interrupts::enable();
}
