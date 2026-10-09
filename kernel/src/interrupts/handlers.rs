//! Interrupt and exception handlers: every vector from `entry::common_entry` lands in `trap`,
//! which dispatches CPU exceptions (page faults, signals for user faults), device interrupts and
//! inter-processor interrupts.

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
    // deliver pending signals: a native task's here; a Linux program that
    // was kicked goes back to its server, which delivers its signals.
    if frame.from_user() {
        crate::process::sched::resched_on_return();
        match crate::process::linux::mode() {
            None => signal::deliver(frame, None),
            Some(true) if crate::process::linux::kick_pending() => crate::process::linux::trap(frame, restricted::REASON_KICK),
            Some(_) => {}
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

/// Memory ran out at a copy's touch of user memory (the kernel's uaccess
/// or the Linux server's copy routine): the process dies of SIGKILL, as of
/// a touch of its own, but by a signal, so the copy still ends at its
/// fixup and the call unwinds (locks and references the kernel or the
/// server hold are given back) before the kill takes effect on the way
/// back to user mode.
fn oom_kill(addr: u64) {
    crate::printkln!("[kernel] out of memory at {:#x} (in a copy): process killed", addr);
    signal::send_to(&crate::process::sched::current().group.clone(), signal::SIGKILL);
}

fn exception(frame: &mut Frame) {
    let vector = frame.vector as u8;
    // Read now: resolving the fault may sleep, and CR2 changes meanwhile.
    let fault_addr = if vector == 14 { Cr2::read_raw() } else { 0 };
    // A fault of the Linux server on the program's memory (it reads and
    // writes that directly): resolved as the program's own fault would be,
    // else its copy routine reports EFAULT.
    if vector == 14 && frame.from_user() && crate::process::linux::mode() == Some(false) && fault_addr < crate::process::address_space::USER_END {
        use crate::process::address_space::{handle_fault, Access};
        let access = Access { write: frame.error & 2 != 0, exec: false };
        if frame.rflags & 0x200 != 0 {
            x86_64::instructions::interrupts::enable();
        }
        // A copy whose wait for the page ended because the thread dies takes
        // its fixup too (EFAULT): returning would run the copy again, and the
        // page never comes. The server then unwinds and the thread exits at
        // its next `restricted_enter`.
        match handle_fault(fault_addr, access) {
            Ok(()) => return,
            Err(e) => {
                if e == crate::process::address_space::Fault::Oom {
                    oom_kill(fault_addr);
                }
                if let Some(fixup) = crate::process::linux::server_fault(frame.rip) {
                    frame.rip = fixup;
                    return;
                }
            }
        }
    }
    if frame.from_user() && crate::process::linux::mode() == Some(false) {
        // The Linux server failed (also one whose access to program memory
        // outside its copy routine ended with the thread's death: every such
        // access goes through the copy routine, whose fixup unwinds the call).
        // Whatever it held (its locks, sleeping ones too, references) is lost
        // with it, so its instance ends: never this thread alone.
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
        crate::process::linux::break_instance();
        signal::kernel_kill_current();
    }
    let mut sig = exception_signal(vector);
    // A page fault's kind, for a Linux program's server (`FAULT_*`).
    let mut kind = 0;
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
                Err(e) if fixup.is_some() => {
                    if e == Fault::Oom {
                        oom_kill(addr);
                    }
                    frame.rip = fixup.expect("checked").to;
                    return;
                }
                Err(Fault::Bus | Fault::Retry) => {
                    sig = signal::SIGBUS;
                    kind = restricted::FAULT_BUS;
                }
                Err(Fault::Oom) => {
                    crate::printkln!("[kernel] out of memory at {:#x}: process killed", addr);
                    signal::kernel_kill_current();
                }
                Err(Fault::Segv) => kind = restricted::FAULT_UNMAPPED,
                Err(Fault::Access) => kind = restricted::FAULT_PROTECTION,
            }
        }
    }
    // A service's copy on granted memory that a revoke took away (see
    // `channel::set_copy_fixup`): the copy fails, not the service.
    if vector == 14 && frame.from_user() {
        if let Some(fixup) = crate::process::channel::copy_fixup(frame.rip) {
            frame.rip = fixup;
            return;
        }
    }
    // A Linux program's exception is its server's to turn into a signal.
    if frame.from_user() && vector != 18 && crate::process::linux::mode() == Some(true) {
        crate::process::linux::trap_exception(frame, vector as u64, frame.error, fault_addr, kind);
        return;
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
