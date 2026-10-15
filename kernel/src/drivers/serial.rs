//! The serial ports COM1-COM4 (16550 UARTs, ADR 0020): found at start-up by their scratch
//! register (most laptops have none), run at 115200 baud, 8N1. COM1 carries the kernel's
//! messages and mirrors the console (`console`); every port is also a raw device of the
//! Linux server, its `/dev/ttyS<n>`, granted with the console (`console_device::holder`).
//!
//! Output is polled, in pieces of the transmitter's FIFO with the port's lock held and
//! interrupts off (a piece takes at most about 1.4 ms at 115200 baud), so another writer's
//! piece, an echo or a kernel message can come between two pieces of a long write.
//! Input comes by the receive interrupt (IRQ 4 for COM1 and COM3, 3 for COM2 and COM4):
//! a port its terminal holds open (`claim`) queues it in its ring and wakes the holder's
//! service thread (the console's event, `console_device::notify`); COM1 unclaimed gives it
//! to the console's input, so a terminal on the serial line types into the console it
//! mirrors; the other ports drop it. In loopback mode (`MCR_LOOP`, TIOCM_LOOP) a PC's port
//! raises no interrupt: a write reads its own echo back after each piece.

use crate::sync::IrqSpinLock;
use core::sync::atomic::{AtomicBool, Ordering};
use heapless::Deque;
use x86_64::instructions::interrupts::without_interrupts;
use x86_64::instructions::port::Port;

/// Ports at most, their I/O bases and interrupt lines.
pub const PORTS: usize = 4;
const BASES: [u16; PORTS] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];
const IRQS: [u8; PORTS] = [4, 3, 4, 3];

// Registers (offsets from the base) and their bits.
const RBR_THR: u16 = 0;
const IER: u16 = 1;
const IIR_FCR: u16 = 2;
const LCR: u16 = 3;
const MCR: u16 = 4;
const LSR: u16 = 5;
const MSR: u16 = 6;
const SCRATCH: u16 = 7;
const IER_RX: u8 = 0x01;
const IER_LINE: u8 = 0x04;
const LCR_DLAB: u8 = 0x80;
const LCR_BREAK: u8 = 0x40;
const LSR_DATA: u8 = 0x01;
const LSR_THRE: u8 = 0x20;
pub const MCR_DTR: u8 = 0x01;
pub const MCR_RTS: u8 = 0x02;
pub const MCR_OUT1: u8 = 0x04;
/// Gates the port's interrupt to the bus on PCs: always on.
const MCR_OUT2: u8 = 0x08;
pub const MCR_LOOP: u8 = 0x10;
/// The modem control bits the Linux server may change.
pub const MCR_SETTABLE: u8 = MCR_DTR | MCR_RTS | MCR_OUT1 | MCR_LOOP;
/// The transmitter's FIFO: a piece of output.
const FIFO: usize = 16;
/// 8N1 at 115200 baud.
const LCR_8N1: u8 = 0x03;
const DIVISOR_115200: u16 = 1;

/// A port's state, under its lock: whether it exists, and the registers the driver
/// changes (the line control's format and break, the modem control lines).
pub struct Uart {
    present: bool,
    lcr: u8,
    mcr: u8,
}

static UARTS: [IrqSpinLock<Uart>; PORTS] = [const { IrqSpinLock::new(Uart { present: false, lcr: LCR_8N1, mcr: 0 }) }; PORTS];
/// The input of a port its terminal holds (what does not fit is dropped: Linux's
/// overrun).
const RING: usize = 4096;
static INPUT: [IrqSpinLock<Deque<u8, RING>>; PORTS] = [const { IrqSpinLock::new(Deque::new()) }; PORTS];
/// A terminal of the holder has the port open: its input is the terminal's.
static CLAIMED: [AtomicBool; PORTS] = [const { AtomicBool::new(false) }; PORTS];
/// Whether each port exists (read without its lock).
static PRESENT: [AtomicBool; PORTS] = [const { AtomicBool::new(false) }; PORTS];

fn reg(port: usize, r: u16) -> Port<u8> {
    Port::new(BASES[port] + r)
}

/// Whether a UART answers at `base`: its scratch register keeps what is written to it
/// (an absent port reads as 0xff whatever was written).
fn probe(port: usize) -> bool {
    let mut scratch = reg(port, SCRATCH);
    [0x5a, 0xa5].iter().all(|&v| unsafe {
        scratch.write(v);
        scratch.read() == v
    })
}

/// Sets the speed's divisor and the format (port lock held).
fn program_line(port: usize, divisor: u16, lcr: u8) {
    unsafe {
        reg(port, LCR).write(lcr | LCR_DLAB);
        reg(port, RBR_THR).write(divisor as u8);
        reg(port, IER).write((divisor >> 8) as u8);
        reg(port, LCR).write(lcr);
    }
}

/// Finds the ports and sets them up, their interrupts off until `enable_interrupts`.
/// COM1 raises DTR and RTS at once (the kernel's messages go out on it); the others when
/// their terminal opens.
pub fn init() {
    for port in 0..PORTS {
        let mut u = UARTS[port].lock();
        if !probe(port) {
            continue;
        }
        u.present = true;
        u.lcr = LCR_8N1;
        u.mcr = MCR_OUT2 | if port == 0 { MCR_DTR | MCR_RTS } else { 0 };
        unsafe {
            reg(port, IER).write(0);
            program_line(port, DIVISOR_115200, LCR_8N1);
            reg(port, IIR_FCR).write(0xc7); // FIFOs on and cleared, receive at 14 bytes
            reg(port, MCR).write(u.mcr);
            // Whatever waited from before.
            while reg(port, LSR).read() & LSR_DATA != 0 {
                reg(port, RBR_THR).read();
            }
        }
        PRESENT[port].store(true, Ordering::Release);
    }
}

/// Once the interrupt controllers are up: the present ports' receive interrupts on.
pub fn enable_interrupts() {
    for port in 0..PORTS {
        if !exists(port) {
            continue;
        }
        without_interrupts(|| {
            let _u = UARTS[port].lock();
            unsafe { reg(port, IER).write(IER_RX | IER_LINE) };
        });
        crate::interrupts::apic::set_masked(IRQS[port], false);
    }
}

/// Whether ISA interrupt line `irq` is a present port's (no user-space driver may take it).
pub fn uses_irq(irq: u8) -> bool {
    (0..PORTS).any(|p| IRQS[p] == irq && exists(p))
}

/// The interrupt of line `irq` (interrupts off): every port on it is drained until none
/// has an interrupt pending (the line is edge-triggered: a port left pending would never
/// raise it again).
pub fn interrupt(irq: u8) {
    for _ in 0..64 {
        let mut pending = false;
        for port in (0..PORTS).filter(|&p| IRQS[p] == irq && exists(p)) {
            let iir = unsafe { reg(port, IIR_FCR).read() };
            if iir & 1 == 0 {
                pending = true;
                receive(port);
            }
        }
        if !pending {
            return;
        }
    }
}

/// Takes what the port received (and clears its line and modem status) and hands it on.
fn receive(port: usize) {
    let mut buf = [0u8; 64];
    let mut n = 0;
    {
        let _u = UARTS[port].lock();
        unsafe {
            while n < buf.len() {
                let lsr = reg(port, LSR).read();
                if lsr & LSR_DATA == 0 {
                    break;
                }
                buf[n] = reg(port, RBR_THR).read();
                n += 1;
            }
            reg(port, MSR).read();
        }
    }
    if n == 0 {
        return;
    }
    let bytes = &buf[..n];
    if CLAIMED[port].load(Ordering::Acquire) {
        {
            let mut ring = INPUT[port].lock();
            for &b in bytes {
                if ring.push_back(b).is_err() {
                    break;
                }
            }
        }
        super::console_device::notify();
    } else if port == 0 {
        super::console_device::input(bytes);
    }
}

/// Whether port `port` exists (after `init`).
pub fn exists(port: usize) -> bool {
    port < PORTS && PRESENT[port].load(Ordering::Acquire)
}

/// The present ports, a bit each.
pub fn present_mask() -> u64 {
    (0..PORTS).filter(|&p| exists(p)).fold(0, |m, p| m | 1 << p)
}

fn put(port: usize, b: u8) {
    unsafe {
        let mut status = reg(port, LSR);
        for _ in 0..100_000 {
            if status.read() & LSR_THRE != 0 {
                break;
            }
        }
        reg(port, RBR_THR).write(b);
    }
}

/// Writes `bytes` to port `port` as they are, a FIFO's worth per lock hold; in loopback
/// mode each piece's echo is read back (no interrupt brings it on a PC).
pub fn write_port(port: usize, bytes: &[u8]) {
    if !exists(port) {
        return;
    }
    for piece in bytes.chunks(FIFO) {
        let looped = without_interrupts(|| {
            let u = UARTS[port].lock();
            for &b in piece {
                put(port, b);
            }
            u.mcr & MCR_LOOP != 0
        });
        if looped {
            without_interrupts(|| receive(port));
        }
    }
}

/// The console's mirror and the kernel's messages (COM1), as they are: line ends are the
/// writer's (the terminal's ONLCR, the kernel's own messages add their carriage returns).
pub fn write_bytes(bytes: &[u8]) {
    write_port(0, bytes);
}

/// Whether the machine has COM1 (after `init`).
pub fn exists_console() -> bool {
    exists(0)
}

/// COM1's lock, for a panic to take over (`console::take_over_for_panic`).
pub fn lock_for_panic() -> &'static IrqSpinLock<Uart> {
    &UARTS[0]
}

/// A terminal of the holder holds port `port` open (`on`), or no longer: its input goes
/// to the port's ring (what was there before goes), or back to the console (COM1) or
/// nowhere.
pub fn claim(port: usize, on: bool) {
    let mut ring = INPUT[port].lock();
    ring.clear();
    CLAIMED[port].store(on, Ordering::Release);
}

/// The console's holder changed: no terminal holds a port any more.
pub fn release_all() {
    for port in 0..PORTS {
        claim(port, false);
    }
}

/// Takes up to `out.len()` bytes of port `port`'s input.
pub fn read(port: usize, out: &mut [u8]) -> usize {
    let mut ring = INPUT[port].lock();
    let mut n = 0;
    while n < out.len() {
        let Some(b) = ring.pop_front() else { break };
        out[n] = b;
        n += 1;
    }
    n
}

/// The speed's divisor (of 115200 baud) and the format (the line control register's
/// low six bits: word length, stop bits, parity).
pub fn set_line(port: usize, divisor: u16, format: u8) {
    without_interrupts(|| {
        let mut u = UARTS[port].lock();
        u.lcr = (u.lcr & LCR_BREAK) | (format & 0x3f);
        program_line(port, divisor, u.lcr);
    });
}

/// Sends a break (`on`) or ends it.
pub fn set_break(port: usize, on: bool) {
    without_interrupts(|| {
        let mut u = UARTS[port].lock();
        u.lcr = if on { u.lcr | LCR_BREAK } else { u.lcr & !LCR_BREAK };
        unsafe { reg(port, LCR).write(u.lcr) };
    });
}

/// Sets the modem control lines (`MCR_SETTABLE`'s bits; OUT2 stays on).
pub fn set_modem(port: usize, bits: u8) {
    without_interrupts(|| {
        let mut u = UARTS[port].lock();
        u.mcr = MCR_OUT2 | (bits & MCR_SETTABLE);
        unsafe { reg(port, MCR).write(u.mcr) };
    });
}

/// The line status, modem status and modem control registers: `lsr << 16 | msr << 8 | mcr`.
pub fn status(port: usize) -> u64 {
    without_interrupts(|| {
        let u = UARTS[port].lock();
        let (lsr, msr) = unsafe { (reg(port, LSR).read(), reg(port, MSR).read()) };
        (lsr as u64) << 16 | (msr as u64) << 8 | u.mcr as u64
    })
}
