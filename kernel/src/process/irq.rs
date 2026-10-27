//! Device interrupts for user-space drivers.
//!
//! A server owns the interrupt line the kernel assigned to it. Lines 0-15
//! are the legacy (ISA and PCI INTx) lines of the I/O APIC: when one fires,
//! the kernel masks it, marks it pending and wakes the server;
//! `ipc_receive` reports it as a notification. The server handles the
//! device, then calls `irq_enable` to unmask the line again.
//!
//! Lines 16-31 are message-signalled: the kernel programmed a device's
//! MSI or MSI-X capability (`pci::Device::enable_msi`) to send vector
//! `apic::MSI_BASE + line - 16` (`alloc_msi`). A message cannot be masked
//! at the I/O APIC, and need not be: it is an edge, sent once per event of
//! the device, so it only marks the line pending (also before the server
//! first enabled it: nothing is lost while it starts) and wakes the owner;
//! `irq_enable` takes the line and is otherwise a no-op.

use super::errno::*;
use super::{current_pid, wakeup, with_current, Pid};
use crate::interrupts::apic;
use core::sync::atomic::{AtomicU32, AtomicU8, AtomicUsize, Ordering};

const LEGACY: usize = 16;
const LINES: usize = LEGACY + apic::MSI_COUNT as usize;
/// Lines no driver may take: timer, keyboard, cascade.
const RESERVED: u32 = 0b111;

static OWNER: [AtomicUsize; LINES] = [const { AtomicUsize::new(0) }; LINES];
static PENDING: AtomicU32 = AtomicU32::new(0);
/// Message-signalled lines handed out so far (they are never given back:
/// a device keeps its vector for the kernel's life).
static MSI_USED: AtomicU8 = AtomicU8::new(0);

/// Sleep channel a server waits on for requests and notifications.
pub fn server_chan(pid: Pid) -> usize {
    0x3_0000_0000 + pid as usize
}

/// A message-signalled line for one device: (line, vector), None once
/// all `apic::MSI_COUNT` are taken.
pub fn alloc_msi() -> Option<(u8, u8)> {
    let slot = MSI_USED.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < apic::MSI_COUNT).then_some(n + 1)).ok()?;
    Some((LEGACY as u8 + slot, apic::MSI_BASE + slot))
}

fn is_msi(line: u64) -> bool {
    (LEGACY as u64..LINES as u64).contains(&line)
}

/// irq_enable(line): takes the line on first use and unmasks it.
pub fn enable(line: u64) -> SysResult {
    let allowed = with_current(|p| p.server.as_ref().and_then(|s| s.irq)).ok_or(EPERM)?;
    if line >= LINES as u64 || allowed as u64 != line {
        return Err(EPERM);
    }
    // (A present serial port's line is the kernel's too.)
    if !is_msi(line) && (RESERVED & 1 << line != 0 || crate::drivers::serial::uses_irq(line as u8)) {
        return Err(EPERM);
    }
    let me = current_pid();
    let owner = &OWNER[line as usize];
    if owner.load(Ordering::Relaxed) != me as usize
        && owner.compare_exchange(0, me as usize, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return Err(EBUSY);
    }
    if !is_msi(line) {
        x86_64::instructions::interrupts::without_interrupts(|| apic::set_masked(line as u8, false));
    }
    Ok(0)
}

/// Lines that fired for `pid` since the last call, as a bit mask.
pub fn take_pending(pid: Pid) -> u32 {
    let mine = (0..LINES).filter(|&l| OWNER[l].load(Ordering::Relaxed) == pid as usize).fold(0u32, |m, l| m | 1 << l);
    PENDING.fetch_and(!mine, Ordering::Relaxed) & mine
}

/// Releases the lines of an exiting process.
pub fn on_exit(pid: Pid) {
    for line in 0..LINES {
        if OWNER[line].compare_exchange(pid as usize, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            if !is_msi(line as u64) {
                x86_64::instructions::interrupts::without_interrupts(|| apic::set_masked(line as u8, true));
            }
            PENDING.fetch_and(!(1 << line), Ordering::Relaxed);
        }
    }
}

/// Called from the interrupt handler of legacy `line` (interrupts are off).
pub fn fire(line: u8) {
    let owner = OWNER[line as usize].load(Ordering::Relaxed);
    if owner != 0 {
        apic::set_masked(line, true);
        PENDING.fetch_or(1 << line, Ordering::Relaxed);
    }
    apic::eoi();
    if owner != 0 {
        wakeup(server_chan(owner as Pid));
    }
}

/// Called from the handler of message-signalled vector `MSI_BASE + slot`
/// (interrupts are off).
pub fn fire_msi(slot: u8) {
    let line = LEGACY + slot as usize;
    PENDING.fetch_or(1 << line, Ordering::Relaxed);
    apic::eoi();
    let owner = OWNER[line].load(Ordering::Relaxed);
    if owner != 0 {
        wakeup(server_chan(owner as Pid));
    }
}
