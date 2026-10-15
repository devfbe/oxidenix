//! COM1 serial port (output only). Everything the console prints is mirrored
//! here, so a headless QEMU (`-serial stdio`) shows the kernel's output.
//! Machines without one (most laptops) are recognized at start-up, and the
//! port is left alone: the framebuffer console is all there is.

use crate::sync::IrqSpinLock;
use x86_64::instructions::interrupts::without_interrupts;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3f8;

/// Whether the port exists and is set up.
static LOCK: IrqSpinLock<bool> = IrqSpinLock::new(false);

/// Whether a UART answers at COM1: its scratch register keeps what is
/// written to it (an absent port reads as 0xff whatever was written).
fn present() -> bool {
    let mut scratch = Port::<u8>::new(COM1 + 7);
    [0x5a, 0xa5].iter().all(|&v| unsafe {
        scratch.write(v);
        scratch.read() == v
    })
}

pub fn init() {
    let mut ready = LOCK.lock();
    if !present() {
        return;
    }
    unsafe {
        Port::<u8>::new(COM1 + 1).write(0x00); // no interrupts
        Port::<u8>::new(COM1 + 3).write(0x80); // divisor latch access
        Port::<u8>::new(COM1).write(0x01); // 115200 baud
        Port::<u8>::new(COM1 + 1).write(0x00);
        Port::<u8>::new(COM1 + 3).write(0x03); // 8 bits, no parity, one stop bit
        Port::<u8>::new(COM1 + 2).write(0xc7); // FIFO on, cleared
    }
    *ready = true;
}

fn put(b: u8) {
    unsafe {
        let mut status = Port::<u8>::new(COM1 + 5);
        for _ in 0..100_000 {
            if status.read() & 0x20 != 0 {
                break;
            }
        }
        Port::<u8>::new(COM1).write(b);
    }
}

pub fn write_bytes(bytes: &[u8]) {
    without_interrupts(|| {
        let ready = LOCK.lock();
        if !*ready {
            return;
        }
        // As they are: line ends are the writer's (the terminal's ONLCR, the
        // kernel's own messages add their carriage returns).
        for &b in bytes {
            put(b);
        }
    });
}

/// Whether the machine has the port (after `init`).
pub fn exists() -> bool {
    without_interrupts(|| *LOCK.lock())
}

/// The port's lock, for a panic to take over (`console::take_over_for_panic`).
pub fn lock_for_panic() -> &'static IrqSpinLock<bool> {
    &LOCK
}
