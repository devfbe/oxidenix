//! The console's terminal (phase R6d, ADR 0007, ADR 0020): the kernel's console device (a
//! VT100 on the framebuffer, mirrored to the serial port, and the keyboard, whose
//! scancodes the virtual terminal's keyboard translates, `vt`) driven by a terminal of the
//! server (`tty`): /dev/console, /dev/tty0 and /dev/tty1. The kernel grants the device
//! (with the serial ports, `serial`) to the tree it starts until that tree's first process
//! ends: meanwhile its input wakes the service thread (`EVENT_CONSOLE`, `input`), and its
//! loss (`EVENT_CONSOLE_LOST`, `lost`) hangs the terminals up, after which opening
//! /dev/console gives ENXIO.
//!
//! The tree's first thread gets its standard descriptors here (`setup_stdio`, as Linux's
//! init gets /dev/console), and its process the console as its controlling terminal (there
//! is no getty to make it one; ADR 0007).

use crate::namespace::Origin;
use crate::sync::Mutex;
use crate::syscall;
use crate::tty::{self, Driver, Tty, ENXIO};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use ldisc::{Termios, IUTF8};
use restricted::*;

static CONSOLE: Mutex<Option<Arc<Tty>>> = Mutex::new(None);
/// The instance lost the device for good.
static GONE: AtomicBool = AtomicBool::new(false);

/// The console's settings: Linux's defaults, and UTF-8 input (the keyboard sends UTF-8, as
/// a console set up by `unicode_start`): erasing takes whole characters.
fn termios() -> Termios {
    let mut t = Termios::default();
    t.iflag |= IUTF8;
    t
}

/// The console's terminal, made at first use while the instance holds the device.
pub fn tty() -> Option<Arc<Tty>> {
    if GONE.load(Ordering::Acquire) {
        return None;
    }
    let mut console = CONSOLE.lock();
    if let Some(t) = console.as_ref() {
        return Some(t.clone());
    }
    let mut size = [0u64; 2];
    if syscall(SYS_CONSOLE_INFO, [size.as_mut_ptr() as u64, 0, 0, 0, 0, 0]) < 0 {
        return None;
    }
    let winsize = [size[1].min(u16::MAX as u64) as u16, size[0].min(u16::MAX as u64) as u16, 0, 0];
    // (Without memory: not yet, ENXIO for this open; the next one tries again.)
    let t = Tty::new(Driver::Console, termios(), winsize, None).ok()?;
    *console = Some(t.clone());
    Some(t)
}

/// The console's terminal if it was made.
pub fn current() -> Option<Arc<Tty>> {
    CONSOLE.lock().clone()
}

/// Opens the console: /dev/console and /dev/tty0 (never the caller's controlling terminal
/// by opening, as on Linux), /dev/tty1 (`ctty`: it may become one).
pub fn open(flags: u32, origin: Origin, ctty: bool) -> Result<i64, i64> {
    let t = tty().ok_or(ENXIO)?;
    tty::open(&t, flags, origin, ctty)
}

/// Whether the instance holds the console's device (the serial ports go with it).
pub fn held() -> bool {
    !GONE.load(Ordering::Acquire)
}

/// Writes to the device (a call per `CONSOLE_WRITE_MAX` bytes, each whole). A signal does
/// not interrupt it (the output was processed for it already; the writer checks for
/// signals between its chunks, before processing): Ok, or the bytes that went out before
/// an error: EINTR for a dying thread, EIO when the instance lost the device (its
/// terminal hangs up at the event).
pub fn device_write(bytes: &[u8]) -> Result<(), (usize, i64)> {
    let mut went = 0;
    for piece in bytes.chunks(CONSOLE_WRITE_MAX as usize) {
        let r = syscall(SYS_CONSOLE_WRITE, [piece.as_ptr() as u64, piece.len() as u64, 0, 0, 0, 0]);
        if r < 0 {
            return Err((went, -r));
        }
        went += piece.len();
    }
    Ok(())
}

/// Echoes to the device without ever waiting (the service thread's input processing must
/// not stall behind a program flooding the console): queued by the kernel, written
/// between the pieces of a write in progress; what does not fit is dropped, as Linux's
/// echo buffer drops.
pub fn device_echo(bytes: &[u8]) {
    for piece in bytes.chunks(512) {
        syscall(SYS_CONSOLE_WRITE, [piece.as_ptr() as u64, piece.len() as u64, CONSOLE_ECHO, 0, 0, 0]);
    }
}

/// `EVENT_CONSOLE`: the device's input into the terminal (service thread): the console's
/// answers to queries (and COM1's input while no terminal holds it) as they are, the
/// keyboard's scancodes through the virtual terminal's keyboard (`vt`), then the serial
/// ports' input into their terminals.
pub fn input() {
    let terminal = tty();
    let mut buf = [0u8; 256];
    loop {
        let n = syscall(SYS_CONSOLE_READ, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0]);
        if n <= 0 {
            break;
        }
        if let Some(t) = &terminal {
            t.input(&buf[..n as usize]);
        }
    }
    loop {
        let n = syscall(SYS_KBD_READ, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0]);
        if n <= 0 {
            break;
        }
        crate::vt::keyboard(&buf[..n as usize], terminal.as_ref());
    }
    crate::serial::input();
}

/// `EVENT_CONSOLE_LOST`: the kernel's monitor took the device back; the terminal hangs up,
/// and so do the serial ports' (granted with it).
pub fn lost() {
    GONE.store(true, Ordering::Release);
    if let Some(t) = current() {
        t.hangup(false);
    }
    crate::serial::lost();
}

/// `ROLE_INIT`: the first thread of the tree's first program gets descriptors 0, 1 and 2
/// on the console (one open file description, as Linux's init), and its process (a session
/// leader) the console as its controlling terminal.
pub fn setup_stdio() {
    const O_RDWR: u32 = 2;
    // /dev/console (`namespace::dev` makes it with the instance).
    let Ok(node) = crate::namespace::resolve("/", "/dev/console", true).map(|r| r.node) else { return };
    let Some(t) = tty() else { return };
    let origin = Origin::new(node, alloc::string::String::from("/dev/console"), vfs::S_IFCHR);
    let fd = match tty::open(&t, O_RDWR, origin, false) {
        Ok(fd) => fd as u64,
        Err(_) => return,
    };
    // 1 and 2: the same description.
    let table = crate::fdtable::current();
    if let Ok(file) = table.get(fd) {
        for _ in 0..2 {
            let _ = table.install(file.clone(), false, 0);
        }
    }
    if let Ok(me) = tty::ids(0, 0) {
        let mut inner = t.inner.lock();
        if inner.session.is_none() && me.pid == me.sid {
            inner.session = Some(me.sid);
            inner.pgrp = Some(me.pgid);
        }
    }
}
