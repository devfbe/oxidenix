//! Device interrupts for user-space drivers.
//!
//! A server owns the interrupt line the kernel assigned to it. When the
//! line fires, the kernel masks it, marks it pending and wakes the server;
//! `ipc_receive` reports it as a notification. The server handles the
//! device, then calls `irq_enable` to unmask the line again.

use super::errno::*;
use super::{current_pid, wakeup, with_current, Pid};
use crate::interrupts::apic;
use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

const LINES: usize = 16;
/// Lines no driver may take: timer, keyboard, cascade.
const RESERVED: u16 = 0b111;

static OWNER: [AtomicUsize; LINES] = [const { AtomicUsize::new(0) }; LINES];
static PENDING: AtomicU16 = AtomicU16::new(0);

/// Sleep channel a server waits on for requests and notifications.
pub fn server_chan(pid: Pid) -> usize {
    0x3_0000_0000 + pid as usize
}

fn set_masked(line: u8, masked: bool) {
    apic::set_masked(line, masked);
}

/// irq_enable(line): takes the line on first use and unmasks it.
pub fn enable(line: u64) -> SysResult {
    let allowed = with_current(|p| p.server.as_ref().and_then(|s| s.irq)).ok_or(EPERM)?;
    if line >= LINES as u64 || RESERVED & 1 << line != 0 || allowed as u64 != line {
        return Err(EPERM);
    }
    let me = current_pid();
    let owner = &OWNER[line as usize];
    if owner.load(Ordering::Relaxed) != me as usize
        && owner.compare_exchange(0, me as usize, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return Err(EBUSY);
    }
    x86_64::instructions::interrupts::without_interrupts(|| set_masked(line as u8, false));
    Ok(0)
}

/// Lines that fired for `pid` since the last call, as a bit mask.
pub fn take_pending(pid: Pid) -> u16 {
    let mine = (0..LINES).filter(|&l| OWNER[l].load(Ordering::Relaxed) == pid as usize).fold(0u16, |m, l| m | 1 << l);
    PENDING.fetch_and(!mine, Ordering::Relaxed) & mine
}

/// Releases the lines of an exiting process.
pub fn on_exit(pid: Pid) {
    for line in 0..LINES {
        if OWNER[line].compare_exchange(pid as usize, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            x86_64::instructions::interrupts::without_interrupts(|| set_masked(line as u8, true));
            PENDING.fetch_and(!(1 << line), Ordering::Relaxed);
        }
    }
}

/// Called from the interrupt handler of `line` (interrupts are off).
pub fn fire(line: u8) {
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 {
        set_masked(line, true);
        PENDING.fetch_or(1 << line, Ordering::Relaxed);
    }
    apic::eoi();
    if owner != 0 {
        wakeup(server_chan(owner as Pid));
    }
}
