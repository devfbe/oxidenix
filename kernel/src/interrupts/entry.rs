//! Uniform entry for every interrupt and exception (except NMI and double
//! fault, and #DB's own paranoid entry on its IST stack, `debug_entry`): a per-vector stub pushes the vector (and a dummy error code where
//! the CPU pushes none), the common path saves all registers into a `Frame`,
//! switches GS to the kernel's per-CPU block when coming from ring 3 and
//! calls `trap`. The return path (`user_return`) is shared with syscalls.

use crate::process::syscall::Frame;

/// Byte offset of `Frame::cs`: 15 registers, vector, error code and rip.
pub const FRAME_CS_OFFSET: usize = 18 * 8;

/// Saves the registers, switches GS if the CPU came from ring 3 and calls
/// `trap`; returns through `user_return`.
#[unsafe(naked)]
unsafe extern "C" fn common_entry() {
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
        "test byte ptr [rsp + {cs}], 3",
        "jz 2f",
        "swapgs",
        "2:",
        "cld",
        "mov rdi, rsp",
        "call {trap}",
        "jmp {ret}",
        cs = const FRAME_CS_OFFSET,
        trap = sym super::handlers::trap,
        ret = sym crate::process::syscall::user_return,
    );
}

/// The size of a `Frame` in u64s.
const FRAME_WORDS: usize = core::mem::size_of::<Frame>() / 8;

/// The debug exception (#DB), on its own IST stack (`gdt::DEBUG_IST_INDEX`), safe whatever
/// state it comes in (Linux's paranoid entry; CVE-2018-8897): a data breakpoint or single
/// step a `mov ss`/`pop ss` held off is delivered at the first instruction of the kernel's
/// entry the next instruction made (`syscall`, `int3`, a fault), in ring 0 but still on the
/// user's stack and with the user's GS. So:
/// - from ring 3 (the saved CS): GS is switched, the frame moves to the task's kernel stack
///   and the exception goes the common way (`trap`: the program's server makes it a SIGTRAP);
/// - from ring 0: GS is the kernel's only if IA32_GS_BASE holds a kernel address (the user
///   cannot set GS's base: FSGSBASE is off), else it is switched for the handler and back;
///   `handlers::debug_kernel` acknowledges the exception on this stack (it never reaches a
///   program as a signal and never sleeps), and the interrupted code goes on as it was.
#[unsafe(naked)]
pub unsafe extern "C" fn debug_entry() {
    core::arch::naked_asm!(
        "push 0",
        "push 1",
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
        "cld",
        "test byte ptr [rsp + {cs}], 3",
        "jnz 3f",
        // Ring 0: the kernel's GS unless IA32_GS_BASE is a user address.
        "mov ecx, 0xc0000101",
        "rdmsr",
        "xor ebx, ebx",
        "test edx, edx",
        "js 1f",
        "swapgs",
        "mov ebx, 1",
        "1:",
        "mov rdi, rsp",
        "call {kernel}",
        "test ebx, ebx",
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
        // Ring 3: onto the task's kernel stack, then as every other entry.
        "3:",
        "swapgs",
        "mov rsi, rsp",
        "mov rsp, gs:[{kernel_stack}]",
        "sub rsp, {frame}",
        "mov rdi, rsp",
        "mov ecx, {words}",
        "rep movsq",
        "mov rdi, rsp",
        "call {trap}",
        "jmp {ret}",
        cs = const FRAME_CS_OFFSET,
        kernel = sym super::handlers::debug_kernel,
        kernel_stack = const crate::smp::KERNEL_STACK_OFFSET,
        frame = const FRAME_WORDS * 8,
        words = const FRAME_WORDS,
        trap = sym super::handlers::trap,
        ret = sym crate::process::syscall::user_return,
    );
}

/// Defines a stub for a vector without an error code (a 0 is pushed in its
/// place) or, with `error`, for one where the CPU pushes it.
macro_rules! stubs {
    ($($name:ident = $vector:literal $($error:ident)?),* $(,)?) => {
        $(
            #[unsafe(naked)]
            unsafe extern "C" fn $name() {
                core::arch::naked_asm!(
                    stubs!(@error $($error)?),
                    concat!("push ", stringify!($vector)),
                    "jmp {common}",
                    common = sym common_entry,
                );
            }
        )*
        /// (vector, entry address) of every stub.
        pub fn stubs() -> &'static [(u8, unsafe extern "C" fn())] {
            &[$(($vector, $name)),*]
        }
    };
    (@error error) => { "" };
    (@error) => { "push 0" };
}

stubs!(
    // Exceptions (2 = NMI and 8 = double fault have their own handlers).
    // (1 = debug has its own entry, `debug_entry`.)
    divide_error = 0, breakpoint = 3, overflow = 4, bound_range = 5,
    invalid_opcode = 6, device_not_available = 7,
    invalid_tss = 10 error, segment_not_present = 11 error, stack_fault = 12 error,
    general_protection = 13 error, page_fault = 14 error, x87_fpu = 16,
    alignment_check = 17 error, machine_check = 18, simd_fpu = 19, virtualization = 20,
    control_protection = 21 error,
    // Local APIC timer.
    timer = 0x20,
    // I/O APIC pins (GSI 0-23).
    gsi0 = 0x30, gsi1 = 0x31, gsi2 = 0x32, gsi3 = 0x33, gsi4 = 0x34, gsi5 = 0x35, gsi6 = 0x36,
    gsi7 = 0x37, gsi8 = 0x38, gsi9 = 0x39, gsi10 = 0x3a, gsi11 = 0x3b, gsi12 = 0x3c,
    gsi13 = 0x3d, gsi14 = 0x3e, gsi15 = 0x3f, gsi16 = 0x40, gsi17 = 0x41, gsi18 = 0x42,
    gsi19 = 0x43, gsi20 = 0x44, gsi21 = 0x45, gsi22 = 0x46, gsi23 = 0x47,
    // Message-signalled interrupts of the servers' devices (`process::irq`).
    msi0 = 0x60, msi1 = 0x61, msi2 = 0x62, msi3 = 0x63, msi4 = 0x64, msi5 = 0x65, msi6 = 0x66,
    msi7 = 0x67, msi8 = 0x68, msi9 = 0x69, msi10 = 0x6a, msi11 = 0x6b, msi12 = 0x6c,
    msi13 = 0x6d, msi14 = 0x6e, msi15 = 0x6f,
    // Inter-processor interrupts and the spurious vector.
    reschedule = 0xf0, halt = 0xf1, tlb = 0xf2, spurious = 0xff,
);

/// Size check for the assembly offsets above.
const _: () = assert!(core::mem::offset_of!(Frame, cs) == FRAME_CS_OFFSET);
