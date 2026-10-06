//! COM1 serial port (output only). Everything the console prints is mirrored
//! here, so a headless QEMU (`-serial stdio`) shows the kernel's output.

use core::fmt;
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3f8;

static LOCK: Mutex<bool> = Mutex::new(false);

pub fn init() {
    let mut ready = LOCK.lock();
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
        for &b in bytes {
            if b == b'\n' {
                put(b'\r');
            }
            put(b);
        }
    });
}

struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_bytes(s.as_bytes());
        Ok(())
    }
}

pub fn write_fmt(args: fmt::Arguments) {
    let _ = fmt::Write::write_fmt(&mut Writer, args);
}
