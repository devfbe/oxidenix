//! A Linux program thread's hardware breakpoints (`restricted::SYS_THREAD_DEBUG`, ADR 0019):
//! DR0-DR3 and the enables of DR7, armed only while the thread runs its program.
//!
//! The server sets them for its own thread (a tracee's server thread applies what its
//! tracer poked when it resumes); the kernel loads them when the thread enters its program
//! (`linux::enter`) or is switched to while it runs it (`sched`), and clears DR7 whenever
//! the CPU leaves it (back to the server, or a switch to another task). So neither the
//! server (whose copies to program memory would hit a data breakpoint), the kernel nor
//! another thread ever runs with them; a CPU knows whether its DR7 is armed (`Cpu::dr7_live`)
//! and disarms it before anything else runs there. DR6 is read and reset at each debug
//! exception (`take_dr6`), for the server's SIGTRAP.

use super::address_space::USER_END;
use super::errno::EINVAL;
use core::sync::atomic::Ordering::Relaxed;

/// A thread's breakpoints as the CPU takes them: the four addresses and DR7 with only the
/// enabled slots' local enable, RW and LEN bits (0: none armed).
#[derive(Clone, Copy, Default)]
pub struct DebugRegs {
    pub addr: [u64; 4],
    pub dr7: u64,
}

/// DR7's RW field of an instruction breakpoint.
const RW_EXEC: u64 = 0;
/// DR7's RW field of an I/O breakpoint (needs CR4.DE: never offered).
const RW_IO: u64 = 2;
/// DR6's bits the kernel reports: B0-B3 and BS.
const DR6_REPORTED: u64 = 0xf | restricted::DR6_STEP;
/// What DR6 reads with nothing recorded (its reserved bits set).
const DR6_CLEAR: u64 = 0xffff_0ff0;

impl DebugRegs {
    /// Checks and normalizes what the server asks for (`regs`: DR0-DR3 and DR7): each slot
    /// enabled locally or globally becomes a local one; it must be an instruction breakpoint
    /// of length 1 or a data breakpoint of length 1, 2, 4 or 8 at an aligned address below
    /// 64 TiB.
    pub fn from_raw(regs: [u64; 5]) -> Result<DebugRegs, i64> {
        let raw = regs[4];
        let mut out = DebugRegs { addr: [regs[0], regs[1], regs[2], regs[3]], dr7: 0 };
        for slot in 0..4 {
            if (raw >> (2 * slot)) & 3 == 0 {
                continue;
            }
            let rw = (raw >> (16 + 4 * slot)) & 3;
            let len_bits = (raw >> (18 + 4 * slot)) & 3;
            // LEN: 00 one byte, 01 two, 11 four, 10 eight.
            let len = match len_bits {
                0 => 1,
                1 => 2,
                3 => 4,
                _ => 8,
            };
            let addr = out.addr[slot];
            if rw == RW_IO || (rw == RW_EXEC && len != 1) || addr % len != 0 || addr.saturating_add(len) > USER_END {
                return Err(EINVAL);
            }
            out.dr7 |= (1 << (2 * slot)) | (rw << (16 + 4 * slot)) | (len_bits << (18 + 4 * slot));
        }
        if out.dr7 == 0 {
            out.addr = [0; 4];
        }
        Ok(out)
    }

    pub fn armed(&self) -> bool {
        self.dr7 != 0
    }

    /// Whether one of the slots DR6 reports hit (`dr6`) is an instruction breakpoint.
    pub fn exec_hit(&self, dr6: u64) -> bool {
        (0..4).any(|slot| dr6 & (1 << slot) != 0 && self.dr7 & (1 << (2 * slot)) != 0 && (self.dr7 >> (16 + 4 * slot)) & 3 == RW_EXEC)
    }

    /// Arms them on the calling CPU (interrupts off: the CPU must not change under it).
    pub fn load(&self) {
        unsafe {
            core::arch::asm!("mov dr0, {}", in(reg) self.addr[0], options(nostack, nomem));
            core::arch::asm!("mov dr1, {}", in(reg) self.addr[1], options(nostack, nomem));
            core::arch::asm!("mov dr2, {}", in(reg) self.addr[2], options(nostack, nomem));
            core::arch::asm!("mov dr3, {}", in(reg) self.addr[3], options(nostack, nomem));
            core::arch::asm!("mov dr7, {}", in(reg) self.dr7, options(nostack, nomem));
        }
        crate::smp::cpu().dr7_live.store(true, Relaxed);
    }
}

/// Disarms the calling CPU's breakpoints if a thread's are armed there (interrupts off).
pub fn disarm() {
    let live = &crate::smp::cpu().dr7_live;
    if live.load(Relaxed) {
        unsafe { core::arch::asm!("mov dr7, {}", in(reg) 0u64, options(nostack, nomem)) };
        live.store(false, Relaxed);
    }
}

/// What DR6 says of the debug exception being handled (`DR6_REPORTED`'s bits), and DR6 reset
/// for the next one (the CPU never clears it).
pub fn take_dr6() -> u64 {
    let dr6: u64;
    unsafe {
        core::arch::asm!("mov {}, dr6", out(reg) dr6, options(nostack, nomem));
        core::arch::asm!("mov dr6, {}", in(reg) DR6_CLEAR, options(nostack, nomem));
    }
    dr6 & DR6_REPORTED
}
