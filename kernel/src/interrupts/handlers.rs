use x86_64::instructions::port::Port;
use crate::process::syscall::Frame;
use x86_64::PrivilegeLevel;
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
    let write_to_present = PageFaultErrorCode::CAUSED_BY_WRITE | PageFaultErrorCode::PROTECTION_VIOLATION;
    if error_code.contains(write_to_present) && crate::process::address_space::resolve_cow(Cr2::read_raw()) {
        return;
    }
    if from_user(&stack_frame) {
        crate::printkln!(
            "[kernel] segmentation fault at {:#x} (rip {:#x}), process killed",
            Cr2::read_raw(),
            stack_frame.instruction_pointer.as_u64()
        );
        crate::process::exit(11);
    }
    crate::printkln!("EXCEPTION: PAGE FAULT");
    crate::printkln!("Accessed Address: {:?}", Cr2::read());
    crate::printkln!("Error Code: {:?}", error_code);
    crate::printkln!("{:#?}", stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}

/// Timer entry: completes the CPU's interrupt frame to a full `Frame`, so
/// that signal delivery can redirect a preempted user program.
#[unsafe(naked)]
pub unsafe extern "C" fn timer_entry() {
    core::arch::naked_asm!(
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rbp",
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",
        "mov rdi, rsp",
        "call {handler}",
        "jmp {ret}",
        handler = sym timer_interrupt,
        ret = sym crate::process::syscall::user_return,
    );
}

extern "sysv64" fn timer_interrupt(frame: &mut Frame) {
    unsafe {
        crate::interrupts::PICS
            .lock()
            .notify_end_of_interrupt(crate::interrupts::InterruptIndex::Timer as u8);
    }
    crate::process::tick();
    // The kernel is not preemptive: only user-space code is interrupted.
    if frame.from_user() {
        crate::process::schedule();
        crate::process::signal::deliver(frame, None);
    }
}

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    crate::drivers::keyboard::handle_scancode(scancode);

    unsafe {
        crate::interrupts::PICS
            .lock()
            .notify_end_of_interrupt(crate::interrupts::InterruptIndex::Keyboard as u8);
    }
}

pub extern "x86-interrupt" fn general_protection_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    if from_user(&stack_frame) {
        crate::printkln!(
            "[kernel] general protection fault (rip {:#x}), process killed",
            stack_frame.instruction_pointer.as_u64()
        );
        crate::process::exit(11);
    }
    panic!("EXCEPTION: GENERAL PROTECTION ({:#x})\n{:#?}", error_code, stack_frame);
}

pub extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    if from_user(&stack_frame) {
        crate::printkln!(
            "[kernel] invalid opcode (rip {:#x}), process killed",
            stack_frame.instruction_pointer.as_u64()
        );
        crate::process::exit(4);
    }
    panic!("EXCEPTION: INVALID OPCODE\n{:#?}", stack_frame);
}

fn from_user(frame: &InterruptStackFrame) -> bool {
    frame.code_segment.rpl() == PrivilegeLevel::Ring3
}
