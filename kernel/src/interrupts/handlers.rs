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
    super::apic::eoi();
    crate::process::tick();
    // The kernel is not preemptive: only user-space code is interrupted.
    if frame.from_user() {
        crate::process::schedule();
        crate::process::signal::deliver(frame, None);
    }
}

fn keyboard_interrupt() {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    crate::drivers::keyboard::handle_scancode(scancode);
    super::apic::eoi();
}

/// Spurious interrupts from the local APIC need no EOI.
pub extern "x86-interrupt" fn spurious_handler(_stack_frame: InterruptStackFrame) {}

/// An I/O APIC interrupt: the keyboard, or a line owned by a user-space
/// driver (see process::irq).
fn gsi_interrupt(gsi: u32) {
    let Some(irq) = (0..16u8).find(|&irq| super::apic::gsi_of(irq) == gsi) else {
        return super::apic::eoi();
    };
    if irq == super::KEYBOARD_IRQ {
        keyboard_interrupt();
    } else {
        crate::process::irq::fire(irq);
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

/// One handler per I/O APIC pin (GSI 0-23).
macro_rules! gsi_handlers {
    ($($name:ident = $gsi:literal),*) => {
        $(extern "x86-interrupt" fn $name(_stack_frame: InterruptStackFrame) {
            gsi_interrupt($gsi);
        })*
        pub const GSI_HANDLERS: &[extern "x86-interrupt" fn(InterruptStackFrame)] = &[$($name),*];
    };
}

gsi_handlers!(
    gsi0 = 0, gsi1 = 1, gsi2 = 2, gsi3 = 3, gsi4 = 4, gsi5 = 5, gsi6 = 6, gsi7 = 7, gsi8 = 8, gsi9 = 9,
    gsi10 = 10, gsi11 = 11, gsi12 = 12, gsi13 = 13, gsi14 = 14, gsi15 = 15, gsi16 = 16, gsi17 = 17,
    gsi18 = 18, gsi19 = 19, gsi20 = 20, gsi21 = 21, gsi22 = 22, gsi23 = 23
);
