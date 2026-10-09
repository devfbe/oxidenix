//! The console's terminal (phase R6d, ADR 0007): the kernel's console device (a VT100 on
//! the framebuffer, mirrored to the serial port, and the keyboard) driven by a terminal of
//! the server (`tty`). The kernel grants the device to the tree it starts until that tree's
//! first process ends: meanwhile its input wakes the service thread (`EVENT_CONSOLE`,
//! `input`), and its loss (`EVENT_CONSOLE_LOST`, `lost`) hangs the terminal up, after which
//! opening /dev/console gives ENXIO.
//!
//! The tree's first thread gets its standard descriptors here (`setup_stdio`, as Linux's
//! init gets /dev/console), and its process the console as its controlling terminal (there
//! is no getty to make it one; ADR 0007).

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
    let t = Tty::new(Driver::Console, termios(), winsize, None);
    *console = Some(t.clone());
    Some(t)
}

/// The console's terminal if it was made.
pub fn current() -> Option<Arc<Tty>> {
    CONSOLE.lock().clone()
}

/// Opens /dev/console (never the caller's controlling terminal by opening, as on Linux).
pub fn open(flags: u32, stat: [u8; 144]) -> Result<i64, i64> {
    let t = tty().ok_or(ENXIO)?;
    tty::open(&t, flags, stat, false)
}

/// Writes to the device, whole: Ok, or EINTR when a signal came while it waited for its
/// turn (nothing written). When the instance no longer holds the device the bytes go
/// (its terminal hangs up at the event).
pub fn device_write(bytes: &[u8]) -> Result<(), i64> {
    match syscall(SYS_CONSOLE_WRITE, [bytes.as_ptr() as u64, bytes.len() as u64, 0, 0, 0, 0]) {
        r if r == -tty::EINTR => Err(tty::EINTR),
        _ => Ok(()),
    }
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

/// `EVENT_CONSOLE`: the device's input into the terminal (service thread).
pub fn input() {
    let terminal = tty();
    let mut buf = [0u8; 256];
    loop {
        let n = syscall(SYS_CONSOLE_READ, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0]);
        if n <= 0 {
            return;
        }
        if let Some(t) = &terminal {
            t.input(&buf[..n as usize]);
        }
    }
}

/// `EVENT_CONSOLE_LOST`: the kernel's monitor took the device back; the terminal hangs up.
pub fn lost() {
    GONE.store(true, Ordering::Release);
    if let Some(t) = current() {
        t.hangup(false);
    }
}

/// `ROLE_INIT`: the first thread of the tree's first program gets descriptors 0, 1 and 2
/// on the console (one open file description, as Linux's init), and its process (a session
/// leader) the console as its controlling terminal.
pub fn setup_stdio() {
    const O_RDWR: u32 = 2;
    let stat = crate::namespace::resolve("/", "/dev/console", true).and_then(|r| r.node.stat()).unwrap_or([0; 144]);
    let Some(t) = tty() else { return };
    let fd = match tty::open(&t, O_RDWR, stat, false) {
        Ok(fd) => fd as u64,
        Err(_) => return,
    };
    let handle = syscall(SYS_KFILE_OBJECT, [fd, 0, 0, 0, 0, 0]);
    if handle > 0 {
        for _ in 0..2 {
            syscall(SYS_KFD_INSTALL_FILE, [handle as u64, 0, 0, 0, 0, 0]);
        }
        syscall(SYS_HANDLE_CLOSE, [handle as u64, 0, 0, 0, 0, 0]);
    }
    if let Ok(me) = tty::ids(0, 0) {
        let mut inner = t.inner.lock();
        if inner.session.is_none() && me.pid == me.sid {
            inner.session = Some(me.sid);
            inner.pgrp = Some(me.pgid);
        }
    }
}
