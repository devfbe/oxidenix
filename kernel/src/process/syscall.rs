//! The system call entry: the register `Frame` of every kernel entry, the `syscall` MSR setup,
//! and the dispatch by who calls: a Linux program's call goes to its server (restricted mode),
//! the Linux server's calls are the kernel's interface for it (`linux::server_call`), a native
//! server's are `native`'s. The kernel implements no Linux system call (R9).

use crate::interrupts::gdt;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

/// Complete register state at a kernel entry. The tail (rip..ss) is exactly
/// what the CPU pushes on an interrupt from ring 3; `vector` and `error`
/// identify the interrupt or exception (syscalls use `SYSCALL_VECTOR`). All
/// entries share this layout and return through `user_return` (iretq).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Frame {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl Frame {
    pub fn user_start(entry: u64, sp: u64) -> Self {
        Frame {
            rip: entry,
            cs: gdt::USER_CS as u64,
            rflags: 0x202,
            rsp: sp,
            ss: gdt::USER_SS as u64,
            ..Default::default()
        }
    }

    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

/// Marks frames built by `syscall_entry` (not a real interrupt vector).
pub const SYSCALL_VECTOR: u64 = 0x100;

/// Syscall MSRs of the calling CPU.
pub fn init() {
    Star::write(gdt::user_code(), gdt::user_data(), gdt::kernel_code(), gdt::kernel_data())
        .expect("GDT layout does not match syscall/sysret");
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
    // Clear IF on entry; `syscall_entry` enables interrupts once it is on
    // the kernel stack.
    SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG | RFlags::TRAP_FLAG);
    unsafe { Efer::update(|f| *f |= EferFlags::SYSTEM_CALL_EXTENSIONS) };
}

/// Builds a `Frame` on the kernel stack (iret part first, as an interrupt
/// would) and dispatches the syscall. Interrupts are off on entry (SFMASK),
/// so nothing runs between `swapgs` and the switch to the kernel stack;
/// once the frame is saved they are enabled for the syscall itself. The
/// kernel stays non-preemptive (an interrupt in kernel mode never
/// schedules), but long syscalls no longer delay timer ticks and device
/// interrupts on their CPU.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        "swapgs",
        "mov gs:[{user_rsp}], rsp",
        "mov rsp, gs:[{kernel_stack}]",
        "push {ss}",
        "push qword ptr gs:[{user_rsp}]",
        "push r11",
        "push {cs}",
        "push rcx",
        "push 0",
        "push {vector}",
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
        "sti",
        "mov rdi, rsp",
        "call {dispatch}",
        "jmp {ret}",
        ss = const gdt::USER_SS,
        cs = const gdt::USER_CS,
        user_rsp = const crate::smp::USER_RSP_OFFSET,
        kernel_stack = const crate::smp::KERNEL_STACK_OFFSET,
        vector = const SYSCALL_VECTOR,
        dispatch = sym dispatch,
        ret = sym user_return,
    );
}

/// Restores the `Frame` at rsp and returns with iretq. Used after syscalls,
/// after interrupts and exceptions, and as the first entry of new
/// processes. Returning to ring 3 swaps the user's GS base back in.
#[unsafe(naked)]
pub unsafe extern "C" fn user_return() {
    core::arch::naked_asm!(
        "cli",
        "test byte ptr [rsp + {cs}], 3",
        "jz 2f",
        "swapgs",
        "2:",
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rbp",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "add rsp, 16",
        "iretq",
        cs = const crate::interrupts::entry::FRAME_CS_OFFSET,
    );
}

extern "sysv64" fn dispatch(f: &mut Frame) {
    crate::counters::add(|c| &c.syscalls, 1);
    match super::linux::mode() {
        // A Linux program's system call goes to its server.
        Some(true) => {
            super::linux::trap(f, restricted::REASON_SYSCALL);
            super::sched::resched_on_return();
        }
        // The server's own calls.
        Some(false) => {
            let result = match f.rax {
                restricted::SYS_RESTRICTED_ENTER => match super::linux::enter(f) {
                    // `f` is the program's now (or the server's with
                    // `REASON_KICK`): signals are the server's.
                    Ok(()) => {
                        super::sched::resched_on_return();
                        return;
                    }
                    Err(e) => Err(e),
                },
                nr => super::linux::server_call(nr, [f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9]).map(|v| Some(v as u64)),
            };
            // A service thread (the worker; the pager ends its process
            // itself) never enters a program, where a dying thread ends:
            // it ends at its next call once its process exits.
            if super::linux::is_pager() && super::kill::dying() {
                super::exit::exit_thread(0);
            }
            match result {
                Ok(None) => unreachable!("restricted_enter returns above"),
                Ok(Some(v)) => {
                    f.rax = v;
                    super::sched::resched_on_return();
                }
                Err(e) => {
                    f.rax = (-e) as u64;
                    super::sched::resched_on_return();
                }
            }
        }
        None => {
            super::native::dispatch(f);
            super::sched::resched_on_return();
            // A native server's process that is ending ends here.
            super::kill::exit_if_dying(f);
        }
    }
}
