//! Uniform entry for every interrupt and exception (except NMI and double
//! fault): a per-vector stub pushes the vector (and a dummy error code where
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
    divide_error = 0, debug = 1, breakpoint = 3, overflow = 4, bound_range = 5,
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
    // Inter-processor interrupts and the spurious vector.
    reschedule = 0xf0, halt = 0xf1, tlb = 0xf2, spurious = 0xff,
);

/// Size check for the assembly offsets above.
const _: () = assert!(core::mem::offset_of!(Frame, cs) == FRAME_CS_OFFSET);
