use super::apic;
use crate::process::signal;
use crate::process::syscall::Frame;
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::InterruptStackFrame;

/// Every vector that goes through `entry::common_entry` ends up here.
pub extern "sysv64" fn trap(frame: &mut Frame) {
    let vector = frame.vector as u8;
    match vector {
        0..=31 => exception(frame),
        apic::TIMER_VECTOR => {
            apic::eoi();
            crate::process::tick(frame.from_user());
            // The kernel is not preemptive: only user code is interrupted.
            if frame.from_user() {
                crate::process::schedule();
            }
        }
        v if (apic::IRQ_BASE..apic::IRQ_BASE + apic::GSI_COUNT as u8).contains(&v) => {
            gsi_interrupt((v - apic::IRQ_BASE) as u32)
        }
        apic::ipi::RESCHEDULE_VECTOR => apic::eoi(),
        apic::ipi::HALT_VECTOR => halt_forever(),
        // Spurious interrupts need no EOI.
        _ => {}
    }
    // Every return to user space is a chance to deliver pending signals.
    if frame.from_user() {
        signal::deliver(frame, None);
    }
}

fn halt_forever() -> ! {
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

/// The signal a user-mode exception raises, as on Linux.
fn exception_signal(vector: u8) -> u32 {
    match vector {
        0 | 9 | 16 | 19 => signal::SIGFPE,
        1 | 3 => signal::SIGTRAP,
        6 | 7 => signal::SIGILL,
        12 | 17 => signal::SIGBUS,
        _ => signal::SIGSEGV,
    }
}

fn exception_name(vector: u8) -> &'static str {
    match vector {
        0 => "divide error",
        1 => "debug",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound range exceeded",
        6 => "invalid opcode",
        7 => "device not available",
        12 => "stack fault",
        13 => "general protection fault",
        14 => "page fault",
        16 => "x87 floating-point error",
        17 => "alignment check",
        18 => "machine check",
        19 => "SIMD floating-point error",
        _ => "exception",
    }
}

fn exception(frame: &mut Frame) {
    let vector = frame.vector as u8;
    let mut sig = exception_signal(vector);
    if vector == 14 {
        use crate::process::address_space::{handle_fault, Access, Fault};
        let addr = Cr2::read_raw();
        // Error code: bit 1 = write, bit 4 = instruction fetch.
        let access = Access { write: frame.error & 2 != 0, exec: frame.error & 16 != 0 };
        match handle_fault(addr, access) {
            Ok(()) => return,
            Err(Fault::Bus) => sig = signal::SIGBUS,
            Err(Fault::Oom) if frame.from_user() => {
                crate::printkln!("[kernel] out of memory at {:#x}: process killed", addr);
                crate::process::exit(signal::SIGKILL as i32);
            }
            Err(_) => {}
        }
    }
    if frame.from_user() && vector != 18 {
        if signal::force(sig) {
            // No handler: the process dies, so say why.
            match vector {
                14 if sig == signal::SIGBUS => crate::printkln!("[kernel] bus error at {:#x} (rip {:#x}), process killed", Cr2::read_raw(), frame.rip),
                14 => crate::printkln!(
                    "[kernel] segmentation fault at {:#x} (rip {:#x}), process killed",
                    Cr2::read_raw(),
                    frame.rip
                ),
                _ => crate::printkln!("[kernel] {} (rip {:#x}), process killed", exception_name(vector), frame.rip),
            }
        }
        return;
    }
    let cr2 = if vector == 14 { Cr2::read_raw() } else { 0 };
    panic!(
        "{} in the kernel (vector {}, error {:#x}) at rip {:#x}, rsp {:#x}, cr2 {:#x}",
        exception_name(vector),
        vector,
        frame.error,
        frame.rip,
        frame.rsp,
        cr2
    );
}

pub extern "x86-interrupt" fn double_fault_handler(stack_frame: InterruptStackFrame, _error_code: u64) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

pub extern "x86-interrupt" fn nmi_handler(stack_frame: InterruptStackFrame) {
    panic!("NON-MASKABLE INTERRUPT\n{:#?}", stack_frame);
}

fn keyboard_interrupt() {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    crate::drivers::keyboard::handle_scancode(scancode);
    apic::eoi();
}

/// An I/O APIC interrupt: the keyboard, or a line owned by a user-space
/// driver (see process::irq).
fn gsi_interrupt(gsi: u32) {
    let Some(irq) = (0..16u8).find(|&irq| apic::gsi_of(irq) == gsi) else {
        return apic::eoi();
    };
    if irq == super::KEYBOARD_IRQ {
        keyboard_interrupt();
    } else {
        crate::process::irq::fire(irq);
    }
}
