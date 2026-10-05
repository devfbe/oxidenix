use x86_64::instructions::port::Port;
use x86_64::structures::idt::{InterruptStackFrame, PageFaultErrorCode};

pub extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::printkln!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

pub extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

pub extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    crate::printkln!("EXCEPTION: PAGE FAULT");
    crate::printkln!("Accessed Address: {:?}", Cr2::read());
    crate::printkln!("Error Code: {:?}", error_code);
    crate::printkln!("{:#?}", stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    crate::drivers::keyboard::push_scancode(scancode);

    unsafe {
        crate::interrupts::PICS
            .lock()
            .notify_end_of_interrupt(crate::interrupts::InterruptIndex::Keyboard as u8);
    }
}
