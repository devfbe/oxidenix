//! Terminal line discipline for the console: a termios subset with
//! canonical line editing, echo and raw mode.
//!
//! `input` runs in interrupt context, so nothing here allocates.

use super::console;
use crate::process::errno::*;
use crate::process::{sleep_on, wakeup};
use heapless::{Deque, Vec};
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

const TTY_CHAN: usize = 2;
const NCCS: usize = 19;
const BUF_SIZE: usize = 4096;

// c_iflag
const INLCR: u32 = 0o100;
const IGNCR: u32 = 0o200;
const ICRNL: u32 = 0o400;
// c_oflag
const OPOST: u32 = 0o1;
const ONLCR: u32 = 0o4;
// c_cflag: B38400 | CS8 | CREAD
const CFLAG_DEFAULT: u32 = 0o17 | 0o60 | 0o200;
// c_lflag
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHONL: u32 = 0o100;
const ECHOCTL: u32 = 0o1000;
const IEXTEN: u32 = 0o100000;
// c_cc indices
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VMIN: usize = 6;
const VSUSP: usize = 10;
const VWERASE: usize = 14;

/// Kernel `struct termios` layout as used by TCGETS/TCSETS.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
}

impl Termios {
    const fn default() -> Self {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03;
        cc[VQUIT] = 0x1c;
        cc[VERASE] = 0x7f;
        cc[VKILL] = 0x15;
        cc[VEOF] = 0x04;
        cc[VMIN] = 1;
        cc[VSUSP] = 0x1a;
        cc[VWERASE] = 0x17;
        Termios {
            iflag: ICRNL,
            oflag: OPOST | ONLCR,
            cflag: CFLAG_DEFAULT,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | IEXTEN,
            line: 0,
            cc,
        }
    }
}

struct Tty {
    termios: Termios,
    /// Line being edited in canonical mode.
    line: Vec<u8, BUF_SIZE>,
    /// Bytes ready for `read`.
    ready: Deque<u8, BUF_SIZE>,
    /// Pending end-of-file marks (Ctrl+D on an empty line).
    eofs: usize,
    fg_pgrp: u64,
}

static TTY: Mutex<Tty> = Mutex::new(Tty {
    termios: Termios::default(),
    line: Vec::new(),
    ready: Deque::new(),
    eofs: 0,
    fg_pgrp: 0,
});

/// Collects echo output while the TTY lock is held.
struct Echo {
    buf: [u8; 64],
    len: usize,
}

impl Echo {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len == self.buf.len() {
                console::write_bytes(&self.buf);
                self.len = 0;
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn char(&mut self, b: u8, lflag: u32) {
        if b < 0x20 && b != b'\t' && b != b'\n' && lflag & ECHOCTL != 0 {
            self.push(&[b'^', b + 0x40]);
        } else {
            self.push(&[b]);
        }
    }
}

impl Tty {
    fn flag(&self, lflag: u32) -> bool {
        self.termios.lflag & lflag != 0
    }

    fn commit_line(&mut self) {
        for &b in self.line.iter() {
            let _ = self.ready.push_back(b);
        }
        self.line.clear();
    }

    /// Removes the last character (a whole UTF-8 sequence) from the line.
    fn erase_char(&mut self, echo: &mut Echo) -> bool {
        let Some(mut b) = self.line.pop() else { return false };
        while b & 0xc0 == 0x80 {
            match self.line.pop() {
                Some(prev) => b = prev,
                None => break,
            }
        }
        if self.flag(ECHO) && self.flag(ECHOE) {
            echo.push(b"\x08 \x08");
        }
        true
    }

    fn input_byte(&mut self, mut b: u8, echo: &mut Echo) {
        let t = self.termios;
        if b == b'\r' {
            if t.iflag & IGNCR != 0 {
                return;
            }
            if t.iflag & ICRNL != 0 {
                b = b'\n';
            }
        } else if b == b'\n' && t.iflag & INLCR != 0 {
            b = b'\r';
        }

        // Signals are not implemented yet: Ctrl+C only discards pending input.
        if self.flag(ISIG) && b == t.cc[VINTR] {
            self.line.clear();
            self.ready.clear();
            if self.flag(ECHO) {
                echo.push(b"^C\n");
            }
            return;
        }

        if !self.flag(ICANON) {
            let _ = self.ready.push_back(b);
            if self.flag(ECHO) {
                echo.char(b, t.lflag);
            }
            return;
        }

        if b == t.cc[VERASE] || b == 0x08 {
            self.erase_char(echo);
        } else if b == t.cc[VKILL] {
            while self.erase_char(echo) {}
        } else if b == t.cc[VWERASE] {
            while self.line.last() == Some(&b' ') && self.erase_char(echo) {}
            while self.line.last().is_some_and(|&c| c != b' ') && self.erase_char(echo) {}
        } else if b == t.cc[VEOF] {
            if self.line.is_empty() {
                self.eofs += 1;
            }
            self.commit_line();
        } else if b == b'\n' {
            let _ = self.line.push(b);
            self.commit_line();
            if self.flag(ECHO) || self.flag(ECHONL) {
                echo.push(b"\n");
            }
        } else {
            // A full line silently drops further input, like Linux.
            if self.line.push(b).is_ok() && self.flag(ECHO) {
                echo.char(b, t.lflag);
            }
        }
    }

    fn try_read(&mut self, buf: &mut [u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        let canonical = self.flag(ICANON);
        if self.ready.is_empty() {
            if canonical && self.eofs > 0 {
                self.eofs -= 1;
                return Some(0);
            }
            if !canonical && self.termios.cc[VMIN] == 0 {
                return Some(0);
            }
            return None;
        }
        let mut n = 0;
        while n < buf.len() {
            let Some(b) = self.ready.pop_front() else { break };
            buf[n] = b;
            n += 1;
            if canonical && b == b'\n' {
                break;
            }
        }
        Some(n)
    }

    fn readable(&self) -> bool {
        !self.ready.is_empty() || (self.flag(ICANON) && self.eofs > 0)
    }
}

/// Feeds bytes typed on the keyboard (or terminal replies) into the TTY.
pub fn input(bytes: &[u8]) {
    let mut echo = Echo { buf: [0; 64], len: 0 };
    without_interrupts(|| {
        let mut tty = TTY.lock();
        for &b in bytes {
            tty.input_byte(b, &mut echo);
        }
    });
    console::write_bytes(&echo.buf[..echo.len]);
    wakeup(TTY_CHAN);
}

/// Blocking read honoring canonical/raw mode.
pub fn read(buf: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    without_interrupts(|| loop {
        if let Some(n) = TTY.lock().try_read(buf) {
            return Ok(n);
        }
        if nonblock {
            return Err(EAGAIN);
        }
        sleep_on(TTY_CHAN);
    })
}

pub fn write(buf: &[u8]) -> usize {
    console::write_bytes(buf);
    let mut reply = [0u8; 32];
    let n = console::take_reply(&mut reply);
    if n > 0 {
        input(&reply[..n]);
    }
    buf.len()
}

pub fn readable() -> bool {
    without_interrupts(|| TTY.lock().readable())
}

pub fn termios() -> Termios {
    without_interrupts(|| TTY.lock().termios)
}

pub fn set_termios(t: Termios, flush: bool) {
    without_interrupts(|| {
        let mut tty = TTY.lock();
        if flush {
            tty.line.clear();
            tty.ready.clear();
            tty.eofs = 0;
        }
        // Leaving canonical mode hands a half-typed line to the reader.
        if tty.flag(ICANON) && t.lflag & ICANON == 0 {
            tty.commit_line();
        }
        tty.termios = t;
    });
    wakeup(TTY_CHAN);
}

pub fn flush_input() {
    without_interrupts(|| {
        let mut tty = TTY.lock();
        tty.line.clear();
        tty.ready.clear();
        tty.eofs = 0;
    });
}

pub fn pending() -> usize {
    without_interrupts(|| TTY.lock().ready.len())
}

pub fn foreground() -> u64 {
    without_interrupts(|| TTY.lock().fg_pgrp)
}

pub fn set_foreground(pgrp: u64) {
    without_interrupts(|| TTY.lock().fg_pgrp = pgrp);
}
