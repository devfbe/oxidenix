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
    Timer = PIC_1_OFFSET,
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
        unsafe {
            idt[InterruptIndex::Timer as u8]
                .set_handler_addr(x86_64::VirtAddr::new(handlers::timer_entry as *const () as u64));
        }
        idt[InterruptIndex::Keyboard as u8]
            .set_handler_fn(handlers::keyboard_interrupt_handler);
        for &(line, handler) in handlers::DEVICE_IRQS {
            idt[PIC_1_OFFSET + line].set_handler_fn(handler);
        }
        idt
    };
}

pub fn init() {
    gdt::init();
    IDT.load();
    unsafe {
        let mut pics = PICS.lock();
        pics.initialize();
        // Only allow IRQ0 (timer) and IRQ1 (keyboard).
        pics.write_masks(0b1111_1100, 0b1111_1111);
        init_pit(100);
        drain_ps2_output();
    }
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

/// Programs PIT channel 0 as a periodic timer.
unsafe fn init_pit(hz: u32) {
    use x86_64::instructions::port::Port;
    let divisor = (1_193_182 / hz) as u16;
    let mut cmd: Port<u8> = Port::new(0x43);
    let mut ch0: Port<u8> = Port::new(0x40);
    cmd.write(0x36);
    ch0.write(divisor as u8);
    ch0.write((divisor >> 8) as u8);
}
