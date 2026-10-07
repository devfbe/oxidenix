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
            // The kernel is not preemptive: the switch waits for the
            // return to user space (below, or that of the running syscall).
            if crate::timer::interrupt(frame.from_user()) {
                crate::process::sched::set_need_resched();
            }
        }
        v if (apic::IRQ_BASE..apic::IRQ_BASE + apic::GSI_COUNT as u8).contains(&v) => {
            gsi_interrupt((v - apic::IRQ_BASE) as u32)
        }
        apic::ipi::RESCHEDULE_VECTOR => apic::eoi(),
        apic::ipi::HALT_VECTOR => halt_forever(),
        apic::ipi::TLB_VECTOR => {
            apic::eoi();
            crate::process::tlb::serve();
        }
        // Spurious interrupts need no EOI.
        _ => {}
    }
    // Every return to user space is a chance to switch tasks and to
    // deliver pending signals (to a Linux program, not to its server).
    if frame.from_user() {
        crate::process::sched::resched_on_return();
        if crate::process::linux::mode() != Some(false) {
            signal::deliver(frame, None);
        }
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
    // Read now: resolving the fault may sleep, and CR2 changes meanwhile.
    let fault_addr = if vector == 14 { Cr2::read_raw() } else { 0 };
    if frame.from_user() && crate::process::linux::mode() == Some(false) {
        // The Linux server failed: its process cannot go on.
        crate::printkln!(
            "[linux] {} in the Linux server (rip {:#x}, address {:#x}, error {:#x}, cr3 {:#x}, rdi {:#x}), process killed",
            exception_name(vector),
            frame.rip,
            fault_addr,
            frame.error,
            x86_64::registers::control::Cr3::read_raw().0.start_address().as_u64(),
            frame.rdi
        );
        if let Some(mm) = crate::process::current_mm() {
            let (program, normal) = mm.tlb.roots();
            crate::printkln!("[linux] program view {:#x}, normal view {:#x}", program, normal);
        }
        crate::process::exit_group(signal::SIGKILL as i32);
    }
    let mut sig = exception_signal(vector);
    if vector == 14 {
        use crate::process::address_space::{handle_fault, Access, Fault, USER_END};
        let addr = fault_addr;
        // Error code: bit 1 = write, bit 4 = instruction fetch.
        let access = Access { write: frame.error & 2 != 0, exec: frame.error & 16 != 0 };
        // The kernel touches user memory only in uaccess's copy routine; a
        // fault there is handled like the user's own, and if the access is
        // not allowed the copy ends early (EFAULT) instead of the kernel.
        let fixup = if frame.from_user() { None } else { crate::process::uaccess::fixup(frame.rip) };
        if let Some(f) = fixup.as_ref().filter(|f| !f.resolve || addr >= USER_END) {
            frame.rip = f.to;
            return;
        }
        if frame.from_user() || fixup.is_some() {
            // Resolving the fault may sleep (the address space is locked, a
            // file page may be read); the interrupted code had interrupts on.
            if frame.rflags & 0x200 != 0 {
                x86_64::instructions::interrupts::enable();
            }
            match handle_fault(addr, access) {
                Ok(()) => return,
                // The wait for the page ended because the thread dies: the
                // return to user mode carries out SIGKILL or the exit.
                Err(_) if fixup.is_none() && signal::dying() => return,
                Err(_) if fixup.is_some() => {
                    frame.rip = fixup.expect("checked").to;
                    return;
                }
                Err(Fault::Bus) => sig = signal::SIGBUS,
                Err(Fault::Oom) => {
                    crate::printkln!("[kernel] out of memory at {:#x}: process killed", addr);
                    crate::process::exit_group(signal::SIGKILL as i32);
                }
                Err(Fault::Segv | Fault::Access) => {}
            }
        }
    }
    if frame.from_user() && vector != 18 {
        if signal::force(sig) {
            // No handler: the process dies, so say why.
            match vector {
                14 if sig == signal::SIGBUS => crate::printkln!("[kernel] bus error at {:#x} (rip {:#x}), process killed", fault_addr, frame.rip),
                14 => crate::printkln!("[kernel] segmentation fault at {:#x} (rip {:#x}), process killed", fault_addr, frame.rip),
                _ => crate::printkln!("[kernel] {} (rip {:#x}), process killed", exception_name(vector), frame.rip),
            }
        }
        return;
    }
    let cr2 = fault_addr;
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
