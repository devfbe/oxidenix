//! Linux's N_TTY line discipline as a library, for the Linux server's terminals
//! (docs/design/linux-server.md, "The terminal"): input mapping, signals from the
//! keyboard, flow control, canonical line editing with echo, the rules of
//! noncanonical reads, and output processing with Linux's column bookkeeping.
//! It only computes: the waiting, the locks and the devices are the server's.
//!
//! The input buffer holds at most `BUF_SIZE - 1` bytes: complete lines in `buf`
//! (canonical mode; noncanonical: all input) and, in canonical mode, the line
//! being edited after them. A line's end is the byte that ended it (a newline,
//! VEOL or VEOL2, read with the line) or an end-of-file mark (VEOF: taken from
//! the buffer but not read; at the start of a line a read returns 0).

#![no_std]

extern crate alloc;

pub mod termios;

pub use termios::*;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// N_TTY_BUF_SIZE: the input buffer holds one byte less.
pub const BUF_SIZE: usize = 4096;

/// What receiving a byte did besides echoing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Received {
    /// A signal for the terminal's foreground process group (VINTR, VQUIT, VSUSP).
    pub signal: Option<u32>,
    /// The signal flushed the input (done) and the output: the driver discards
    /// the output it still holds, and echoes before this byte's are gone.
    pub flushed: bool,
    /// The byte did not fit and was dropped (noncanonical mode with a full
    /// buffer, or a canonical line that is full).
    pub dropped: bool,
}

/// How a read takes the next bytes: `copy` bytes from the front go to the reader,
/// then `consume` bytes leave the buffer (one more than `copy` for an
/// end-of-file mark).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Take {
    pub copy: usize,
    pub consume: usize,
    /// The read reached a line's end (canonical mode): it ends here.
    pub line_end: bool,
}

/// How reads wait: canonical (a line), or noncanonical with VMIN and VTIME.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    Canonical,
    Raw { min: u8, time: u8 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kill {
    Erase,
    Werase,
    Line,
}

pub struct Ldisc {
    t: Termios,
    /// Readable input.
    buf: VecDeque<u8>,
    /// The absolute position of `buf[0]` (positions only grow).
    base: u64,
    /// Canonical mode: the ends of the lines in `buf` (absolute positions), and
    /// whether each is an end-of-file mark.
    ends: VecDeque<(u64, bool)>,
    /// Canonical mode: the line being edited.
    line: Vec<u8>,
    /// The output column, and the column where the current canonical line started
    /// (for erasing tabs).
    column: u32,
    canon_column: u32,
    /// The next byte is literal (VLNEXT).
    lnext: bool,
    /// ECHOPRT: erased characters are being echoed between '\' and '/'.
    erasing: bool,
    /// Output stopped by VSTOP, and by tcflow(TCOOFF).
    stopped: bool,
    tco_stopped: bool,
}

/// Whether `c` is a control character (the C locale's, as the kernel's `iscntrl`).
pub fn is_cntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7f
}

/// Whether `c` continues a UTF-8 character.
fn is_continuation(c: u8) -> bool {
    c & 0xc0 == 0x80
}

impl Ldisc {
    pub fn new(t: Termios) -> Ldisc {
        Ldisc {
            t,
            buf: VecDeque::new(),
            base: 0,
            ends: VecDeque::new(),
            line: Vec::new(),
            column: 0,
            canon_column: 0,
            lnext: false,
            erasing: false,
            stopped: false,
            tco_stopped: false,
        }
    }

    pub fn termios(&self) -> Termios {
        self.t
    }

    /// New settings (Linux's `n_tty_set_termios`). Leaving canonical mode makes the
    /// lines and the half-typed line raw input (end-of-file marks go); entering it
    /// makes the raw input there a line. True if output was stopped by VSTOP and
    /// IXON went off, which restarts it.
    pub fn set_termios(&mut self, new: Termios) -> bool {
        let old = self.t;
        self.t = new;
        if (old.lflag ^ new.lflag) & ICANON != 0 {
            self.erasing = false;
            self.lnext = false;
            if new.l(ICANON) {
                if !self.buf.is_empty() {
                    self.ends.push_back((self.base + self.buf.len() as u64 - 1, false));
                }
            } else {
                self.drop_eof_marks();
                let line = core::mem::take(&mut self.line);
                self.buf.extend(line);
            }
        }
        if old.i(IXON) && !new.i(IXON) && self.stopped && !self.tco_stopped {
            self.stopped = false;
            return true;
        }
        false
    }

    /// Removes the end-of-file marks from `buf` and forgets the line ends.
    fn drop_eof_marks(&mut self) {
        let marks: Vec<u64> = self.ends.iter().filter(|e| e.1).map(|e| e.0).collect();
        self.ends.clear();
        if marks.is_empty() {
            return;
        }
        let base = self.base;
        let mut at = base;
        self.buf.retain(|_| {
            let keep = !marks.contains(&at);
            at += 1;
            keep
        });
    }

    fn canonical(&self) -> bool {
        self.t.l(ICANON)
    }

    fn total(&self) -> usize {
        self.buf.len() + self.line.len()
    }

    /// Whether another byte of input can be stored now. A writer that feeds the
    /// terminal (a pty's master) waits while it cannot. In canonical mode a full
    /// buffer without a complete line still takes the line's end.
    pub fn room(&self) -> bool {
        self.total() < BUF_SIZE - 1 || (self.canonical() && self.ends.is_empty())
    }

    /// Discards all input (TCIFLUSH, a signal without NOFLSH, a hangup).
    pub fn flush_input(&mut self) {
        self.base += self.buf.len() as u64;
        self.buf.clear();
        self.ends.clear();
        self.line.clear();
        self.lnext = false;
        self.erasing = false;
    }

    /// Receives one byte of input (from the keyboard, a pty's master or TIOCSTI);
    /// what it echoes is appended to `echo`, already processed for output.
    pub fn receive(&mut self, byte: u8, echo: &mut Vec<u8>) -> Received {
        let t = self.t;
        let mut r = Received::default();
        let mut c = byte;
        if t.i(ISTRIP) {
            c &= 0x7f;
        }
        if t.i(IUCLC) && t.l(IEXTEN) {
            c = c.to_ascii_lowercase();
        }
        if self.lnext {
            self.lnext = false;
            self.put(c, false, echo, &mut r);
            return r;
        }
        if t.i(IXON) {
            if t.is(VSTART, c) {
                self.start(false);
                return r;
            }
            if t.is(VSTOP, c) {
                self.stop(false);
                return r;
            }
        }
        if t.l(ISIG) {
            let sig = if t.is(VINTR, c) {
                Some(SIGINT)
            } else if t.is(VQUIT, c) {
                Some(SIGQUIT)
            } else if t.is(VSUSP, c) {
                Some(SIGTSTP)
            } else {
                None
            };
            if let Some(sig) = sig {
                r.signal = Some(sig);
                if !t.l(NOFLSH) {
                    self.flush_input();
                    echo.clear();
                    r.flushed = true;
                }
                if t.i(IXON) {
                    self.start(false);
                }
                if t.l(ECHO) {
                    self.echo_char(c, echo);
                }
                return r;
            }
        }
        if self.stopped && !self.tco_stopped && t.i(IXON) && t.i(IXANY) {
            self.stopped = false;
        }
        // Special: Linux's char_map, whose bytes take the special path.
        let mut special = false;
        if c == b'\r' {
            if t.i(IGNCR) {
                return r;
            }
            if t.i(ICRNL) {
                c = b'\n';
                special = true;
            }
        } else if c == b'\n' && t.i(INLCR) {
            c = b'\r';
            special = true;
        }
        if t.l(ICANON) {
            if t.is(VERASE, c) || t.is(VKILL, c) || (t.is(VWERASE, c) && t.l(IEXTEN)) {
                self.eraser(c, echo);
                return r;
            }
            if t.is(VLNEXT, c) && t.l(IEXTEN) {
                self.lnext = true;
                if t.l(ECHO) {
                    self.finish_erasing(echo);
                    if t.l(ECHOCTL) {
                        self.echo_raw(b"^\x08", echo);
                    }
                }
                return r;
            }
            if t.is(VREPRINT, c) && t.l(ECHO) && t.l(IEXTEN) {
                self.finish_erasing(echo);
                self.echo_char(c, echo);
                self.echo_raw(b"\n", echo);
                let line = core::mem::take(&mut self.line);
                for &b in &line {
                    self.echo_char(b, echo);
                }
                self.line = line;
                return r;
            }
            if c == b'\n' {
                if t.l(ECHO) || t.l(ECHONL) {
                    self.echo_raw(b"\n", echo);
                }
                self.end_line(b'\n', false, &mut r);
                return r;
            }
            if t.is(VEOF, c) {
                self.end_line(0, true, &mut r);
                return r;
            }
            if t.is(VEOL, c) || (t.is(VEOL2, c) && t.l(IEXTEN)) {
                if t.l(ECHO) {
                    if self.line.is_empty() {
                        self.canon_column = self.column;
                    }
                    self.echo_char(c, echo);
                }
                self.end_line(c, false, &mut r);
                return r;
            }
        }
        self.put(c, special, echo, &mut r);
        r
    }

    /// Stores an ordinary byte (Linux's `n_tty_receive_char`).
    fn put(&mut self, c: u8, special: bool, echo: &mut Vec<u8>, r: &mut Received) {
        let t = self.t;
        if self.total() >= BUF_SIZE - 1 {
            if t.l(ICANON) && t.i(IMAXBEL) && t.l(ECHO) {
                self.echo_raw(b"\x07", echo);
            }
            r.dropped = true;
            return;
        }
        if self.stopped && !self.tco_stopped && t.i(IXON) && t.i(IXANY) {
            self.stopped = false;
        }
        if t.l(ECHO) {
            self.finish_erasing(echo);
            if special && c == b'\n' {
                self.echo_raw(b"\n", echo);
            } else {
                if t.l(ICANON) && self.line.is_empty() {
                    self.canon_column = self.column;
                }
                self.echo_char(c, echo);
            }
        }
        if t.l(ICANON) {
            self.line.push(c);
        } else {
            self.buf.push_back(c);
        }
    }

    /// Ends the canonical line with `c` (an end-of-file mark with `eof`). A full
    /// buffer without a complete line gives up its last byte for it (Linux's
    /// overflow rule), one with complete lines drops it.
    fn end_line(&mut self, c: u8, eof: bool, r: &mut Received) {
        if self.total() >= BUF_SIZE - 1 {
            if !self.ends.is_empty() {
                r.dropped = true;
                return;
            }
            self.line.pop();
        }
        let line = core::mem::take(&mut self.line);
        self.buf.extend(line);
        self.buf.push_back(c);
        self.ends.push_back((self.base + self.buf.len() as u64 - 1, eof));
    }

    /// VERASE, VWERASE and VKILL (Linux's `eraser`).
    fn eraser(&mut self, c: u8, echo: &mut Vec<u8>) {
        let t = self.t;
        if self.line.is_empty() {
            return;
        }
        let kind = if t.is(VERASE, c) {
            Kill::Erase
        } else if t.is(VWERASE, c) {
            Kill::Werase
        } else {
            if !t.l(ECHO) {
                self.line.clear();
                return;
            }
            if !t.l(ECHOK) || !t.l(ECHOKE) || !t.l(ECHOE) {
                self.line.clear();
                self.finish_erasing(echo);
                self.echo_char(t.cc[VKILL], echo);
                // A newline if ECHOK is on and ECHOKE off.
                if t.l(ECHOK) {
                    self.echo_raw(b"\n", echo);
                }
                return;
            }
            Kill::Line
        };
        let utf8 = t.i(IUTF8);
        let mut seen_alnums = 0;
        while !self.line.is_empty() {
            // One character, a whole UTF-8 sequence with IUTF8.
            let mut head = self.line.len();
            let mut ch;
            loop {
                head -= 1;
                ch = self.line[head];
                if !(utf8 && is_continuation(ch) && head > 0) {
                    break;
                }
            }
            // Never part of one.
            if utf8 && is_continuation(ch) {
                break;
            }
            if kind == Kill::Werase {
                // BSD's ALTWERASE.
                if ch.is_ascii_alphanumeric() || ch == b'_' {
                    seen_alnums += 1;
                } else if seen_alnums > 0 {
                    break;
                }
            }
            let erased: Vec<u8> = self.line.split_off(head);
            if t.l(ECHO) {
                if t.l(ECHOPRT) {
                    if !self.erasing {
                        self.echo_raw(b"\\", echo);
                        self.erasing = true;
                    }
                    self.echo_char(ch, echo);
                    let rest = &erased[1..];
                    self.echo_raw(rest, echo);
                } else if kind == Kill::Erase && !t.l(ECHOE) {
                    self.echo_char(t.cc[VERASE], echo);
                } else if ch == b'\t' {
                    // Back to where the tab began: the columns of what precedes it
                    // since the line's start (plus the column there) or the
                    // previous tab, modulo 8.
                    let mut num = 0u32;
                    let mut after_tab = false;
                    for &b in self.line.iter().rev() {
                        if b == b'\t' {
                            after_tab = true;
                            break;
                        } else if is_cntrl(b) {
                            if t.l(ECHOCTL) {
                                num += 2;
                            }
                        } else if !(utf8 && is_continuation(b)) {
                            num += 1;
                        }
                    }
                    if !after_tab {
                        num += self.canon_column;
                    }
                    let back = (8 - (num & 7)).min(self.column);
                    for _ in 0..back {
                        self.echo_raw(b"\x08", echo);
                    }
                } else {
                    if is_cntrl(ch) && t.l(ECHOCTL) {
                        self.echo_raw(b"\x08 \x08", echo);
                    }
                    if !is_cntrl(ch) || t.l(ECHOCTL) {
                        self.echo_raw(b"\x08 \x08", echo);
                    }
                }
            }
            if kind == Kill::Erase {
                break;
            }
        }
        if self.line.is_empty() && t.l(ECHO) {
            self.finish_erasing(echo);
        }
    }

    fn finish_erasing(&mut self, echo: &mut Vec<u8>) {
        if self.erasing {
            self.echo_raw(b"/", echo);
            self.erasing = false;
        }
    }

    /// Echoes `c`, a control character as ^X with ECHOCTL (not a tab).
    fn echo_char(&mut self, c: u8, echo: &mut Vec<u8>) {
        if self.t.l(ECHOCTL) && is_cntrl(c) && c != b'\t' {
            self.echo_raw(&[b'^', c ^ 0x40], echo);
        } else {
            self.echo_raw(&[c], echo);
        }
    }

    fn echo_raw(&mut self, bytes: &[u8], echo: &mut Vec<u8>) {
        self.output(bytes, echo);
    }

    /// Processes `input` for output (OPOST: ONLCR, OCRNL, ONOCR, ONLRET, OLCUC,
    /// XTABS) into `out`, keeping the column (Linux's `do_output_char`).
    pub fn output(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let t = self.t;
        if !t.o(OPOST) {
            out.extend_from_slice(input);
            return;
        }
        for &byte in input {
            let mut c = byte;
            match c {
                b'\n' => {
                    if t.o(ONLRET) {
                        self.column = 0;
                    }
                    if t.o(ONLCR) {
                        self.column = 0;
                        self.canon_column = 0;
                        out.extend_from_slice(b"\r\n");
                        continue;
                    }
                    self.canon_column = self.column;
                }
                b'\r' => {
                    if t.o(ONOCR) && self.column == 0 {
                        continue;
                    }
                    if t.o(OCRNL) {
                        c = b'\n';
                        if t.o(ONLRET) {
                            self.column = 0;
                            self.canon_column = 0;
                        }
                    } else {
                        self.column = 0;
                        self.canon_column = 0;
                    }
                }
                b'\t' => {
                    let spaces = 8 - (self.column & 7);
                    self.column += spaces;
                    if t.oflag & TABDLY == XTABS {
                        out.extend_from_slice(&b"        "[..spaces as usize]);
                        continue;
                    }
                }
                0x08 => self.column = self.column.saturating_sub(1),
                _ => {
                    if !is_cntrl(c) {
                        if t.o(OLCUC) {
                            c = c.to_ascii_uppercase();
                        }
                        if !(t.i(IUTF8) && is_continuation(c)) {
                            self.column += 1;
                        }
                    }
                }
            }
            out.push(c);
        }
    }

    /// The output column (for tests and erasing).
    pub fn column(&self) -> u32 {
        self.column
    }

    /// Whether output is stopped (VSTOP, TCOOFF).
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Stops output: by VSTOP (`tco` false) or by tcflow(TCOOFF).
    pub fn stop(&mut self, tco: bool) {
        if tco {
            if !self.tco_stopped {
                self.tco_stopped = true;
                self.stopped = true;
            }
        } else {
            self.stopped = true;
        }
    }

    /// Starts output: by VSTART, IXANY or a signal (not output tcflow stopped), or
    /// by tcflow(TCOON).
    pub fn start(&mut self, tco: bool) {
        if tco {
            if self.tco_stopped {
                self.tco_stopped = false;
                self.stopped = false;
            }
        } else if self.stopped && !self.tco_stopped {
            self.stopped = false;
        }
    }

    /// How reads wait.
    pub fn mode(&self) -> ReadMode {
        if self.canonical() {
            ReadMode::Canonical
        } else {
            ReadMode::Raw { min: self.t.cc[VMIN], time: self.t.cc[VTIME] }
        }
    }

    /// Whether a read finds input (Linux's `input_available_p`): a complete line,
    /// or a byte; for poll with VMIN > 0 and VTIME 0, VMIN bytes.
    pub fn readable(&self, poll: bool) -> bool {
        if self.canonical() {
            return !self.ends.is_empty();
        }
        let (min, time) = (self.t.cc[VMIN] as usize, self.t.cc[VTIME]);
        let amount = if poll && time == 0 && min > 0 { min } else { 1 };
        self.buf.len() >= amount
    }

    /// What a read of at most `max` (> 0) bytes takes next, if anything is there
    /// (canonical: never beyond the first line's end).
    pub fn take(&self, max: usize) -> Option<Take> {
        if self.canonical() {
            let &(end, eof) = self.ends.front()?;
            let len = (end - self.base) as usize + 1;
            let data = if eof { len - 1 } else { len };
            if max >= data {
                return Some(Take { copy: data, consume: len, line_end: true });
            }
            return Some(Take { copy: max, consume: max, line_end: false });
        }
        if self.buf.is_empty() {
            return None;
        }
        let n = max.min(self.buf.len());
        Some(Take { copy: n, consume: n, line_end: false })
    }

    /// Copies the first `out.len()` bytes of the readable input.
    pub fn peek(&self, out: &mut [u8]) {
        for (d, s) in out.iter_mut().zip(self.buf.iter()) {
            *d = *s;
        }
    }

    /// Drops the first `n` bytes of the readable input (after a `take`).
    pub fn consume(&mut self, n: usize) {
        let n = n.min(self.buf.len());
        self.buf.drain(..n);
        self.base += n as u64;
        while self.ends.front().is_some_and(|e| e.0 < self.base) {
            self.ends.pop_front();
        }
    }

    /// FIONREAD: the bytes a read could take, end-of-file marks not counted
    /// (canonical: of complete lines).
    pub fn available(&self) -> usize {
        if self.canonical() {
            let Some(&(last, _)) = self.ends.back() else { return 0 };
            let marks = self.ends.iter().filter(|e| e.1).count();
            return (last - self.base) as usize + 1 - marks;
        }
        self.buf.len()
    }
}
