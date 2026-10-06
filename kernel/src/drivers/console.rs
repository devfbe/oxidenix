//! Framebuffer text console with a cell grid, 16 colors, a visible cursor
//! and the subset of ANSI escape sequences used by BusyBox.

use bootloader_api::info::{FrameBuffer, FrameBufferInfo, PixelFormat};
use core::fmt;
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const RASTER_HEIGHT: RasterHeight = RasterHeight::Size24;
const CHAR_WIDTH: usize = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
const LINE_HEIGHT: usize = RASTER_HEIGHT.val() + 2;
const MAX_COLS: usize = 256;
const MAX_ROWS: usize = 128;
const MAX_PARAMS: usize = 8;

/// ANSI color order: black, red, green, yellow, blue, magenta, cyan, white,
/// then the bright variants.
const PALETTE: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (170, 0, 0),
    (0, 170, 0),
    (170, 85, 0),
    (0, 0, 170),
    (170, 0, 170),
    (0, 170, 170),
    (170, 170, 170),
    (85, 85, 85),
    (255, 85, 85),
    (85, 255, 85),
    (255, 255, 85),
    (85, 85, 255),
    (255, 85, 255),
    (85, 255, 255),
    (255, 255, 255),
];
const DEFAULT_FG: u8 = 7;
const DEFAULT_BG: u8 = 0;

#[derive(Clone, Copy, PartialEq)]
struct Cell {
    ch: char,
    fg: u8,
    bg: u8,
}

const BLANK: Cell = Cell { ch: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG };

/// The console starts before the heap exists, so the cell grid is static.
static mut CELLS: [[Cell; MAX_COLS]; MAX_ROWS] = [[BLANK; MAX_COLS]; MAX_ROWS];

#[derive(PartialEq)]
enum Parse {
    Normal,
    Escape,
    Csi,
}

pub struct Console {
    fb: &'static mut [u8],
    info: FrameBufferInfo,
    cells: &'static mut [[Cell; MAX_COLS]; MAX_ROWS],
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    /// Set after writing the last column; the wrap happens on the next char.
    wrap_pending: bool,
    fg: u8,
    bg: u8,
    bold: bool,
    reverse: bool,
    saved: (usize, usize),
    cursor_visible: bool,
    cursor_drawn: Option<(usize, usize)>,
    parse: Parse,
    params: [u16; MAX_PARAMS],
    nparams: usize,
    private: bool,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
    reply: [u8; 32],
    reply_len: usize,
}

impl Console {
    fn put_pixel(&mut self, x: usize, y: usize, (r, g, b): (u8, u8, u8)) {
        let color = match self.info.pixel_format {
            PixelFormat::Rgb => [r, g, b, 0],
            PixelFormat::Bgr => [b, g, r, 0],
            _ => [((r as u16 + g as u16 + b as u16) / 3) as u8, 0, 0, 0],
        };
        let bpp = self.info.bytes_per_pixel;
        let off = (y * self.info.stride + x) * bpp;
        self.fb[off..off + bpp].copy_from_slice(&color[..bpp]);
    }

    fn draw_cell(&mut self, col: usize, row: usize, invert: bool) {
        let cell = self.cells[row][col];
        let (mut fg, mut bg) = (PALETTE[cell.fg as usize], PALETTE[cell.bg as usize]);
        if invert {
            core::mem::swap(&mut fg, &mut bg);
        }
        let raster = get_raster(cell.ch, FONT_WEIGHT, RASTER_HEIGHT)
            .or_else(|| get_raster('?', FONT_WEIGHT, RASTER_HEIGHT))
            .expect("font lacks '?'");
        let glyph = raster.raster();
        let (x0, y0) = (col * CHAR_WIDTH, row * LINE_HEIGHT);
        for dy in 0..LINE_HEIGHT {
            for dx in 0..CHAR_WIDTH {
                let a = glyph.get(dy).and_then(|l| l.get(dx)).copied().unwrap_or(0) as u16;
                let mix = |f: u8, b: u8| ((f as u16 * a + b as u16 * (255 - a)) / 255) as u8;
                self.put_pixel(x0 + dx, y0 + dy, (mix(fg.0, bg.0), mix(fg.1, bg.1), mix(fg.2, bg.2)));
            }
        }
    }

    fn set_cell(&mut self, col: usize, row: usize, cell: Cell) {
        self.cells[row][col] = cell;
        self.draw_cell(col, row, false);
    }

    fn blank(&self) -> Cell {
        Cell { ch: ' ', fg: DEFAULT_FG, bg: self.bg }
    }

    fn hide_cursor(&mut self) {
        if let Some((c, r)) = self.cursor_drawn.take() {
            self.draw_cell(c, r, false);
        }
    }

    fn show_cursor(&mut self) {
        if self.cursor_visible {
            let pos = (self.col.min(self.cols - 1), self.row);
            self.draw_cell(pos.0, pos.1, true);
            self.cursor_drawn = Some(pos);
        }
    }

    fn scroll_up(&mut self) {
        let line = LINE_HEIGHT * self.info.stride * self.info.bytes_per_pixel;
        let used = self.rows * line;
        self.fb.copy_within(line..used, 0);
        for r in 1..self.rows {
            self.cells[r - 1] = self.cells[r];
        }
        let blank = self.blank();
        for c in 0..self.cols {
            self.set_cell(c, self.rows - 1, blank);
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll_up();
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.col = 0;
            self.line_feed();
        }
        let mut fg = if self.bold && self.fg < 8 { self.fg + 8 } else { self.fg };
        let mut bg = self.bg;
        if self.reverse {
            core::mem::swap(&mut fg, &mut bg);
        }
        self.set_cell(self.col, self.row, Cell { ch, fg, bg });
        if self.col + 1 >= self.cols {
            self.wrap_pending = true;
        } else {
            self.col += 1;
        }
    }

    fn erase(&mut self, row: usize, from: usize, to: usize) {
        let blank = self.blank();
        for c in from..to.min(self.cols) {
            self.set_cell(c, row, blank);
        }
    }

    fn move_to(&mut self, col: usize, row: usize) {
        self.col = col.min(self.cols - 1);
        self.row = row.min(self.rows - 1);
        self.wrap_pending = false;
    }

    pub fn clear_screen(&mut self) {
        for r in 0..self.rows {
            self.erase(r, 0, self.cols);
        }
        self.move_to(0, 0);
    }

    fn param(&self, i: usize, default: u16) -> usize {
        match self.params.get(i) {
            Some(&p) if i < self.nparams && p != 0 => p as usize,
            _ => default as usize,
        }
    }

    fn push_reply(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.reply.len() - self.reply_len);
        self.reply[self.reply_len..self.reply_len + n].copy_from_slice(&bytes[..n]);
        self.reply_len += n;
    }

    fn sgr(&mut self) {
        let count = self.nparams.max(1);
        for i in 0..count {
            let p = if i < self.nparams { self.params[i] } else { 0 };
            match p {
                0 => {
                    self.fg = DEFAULT_FG;
                    self.bg = DEFAULT_BG;
                    self.bold = false;
                    self.reverse = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                7 => self.reverse = true,
                27 => self.reverse = false,
                30..=37 => self.fg = (p - 30) as u8,
                39 => self.fg = DEFAULT_FG,
                40..=47 => self.bg = (p - 40) as u8,
                49 => self.bg = DEFAULT_BG,
                90..=97 => self.fg = (p - 90 + 8) as u8,
                100..=107 => self.bg = (p - 100 + 8) as u8,
                _ => {}
            }
        }
    }

    fn csi(&mut self, final_byte: u8) {
        let n = self.param(0, 1);
        let (col, row) = (self.col.min(self.cols - 1), self.row);
        match final_byte {
            b'A' => self.move_to(col, row.saturating_sub(n)),
            b'B' => self.move_to(col, row + n),
            b'C' => self.move_to(col + n, row),
            b'D' => self.move_to(col.saturating_sub(n), row),
            b'E' => self.move_to(0, row + n),
            b'F' => self.move_to(0, row.saturating_sub(n)),
            b'G' => self.move_to(n - 1, row),
            b'd' => self.move_to(col, n - 1),
            b'H' | b'f' => self.move_to(self.param(1, 1) - 1, n - 1),
            b'J' => match self.param(0, 0) {
                0 => {
                    self.erase(row, col, self.cols);
                    for r in row + 1..self.rows {
                        self.erase(r, 0, self.cols);
                    }
                }
                1 => {
                    for r in 0..row {
                        self.erase(r, 0, self.cols);
                    }
                    self.erase(row, 0, col + 1);
                }
                _ => {
                    for r in 0..self.rows {
                        self.erase(r, 0, self.cols);
                    }
                }
            },
            b'K' => match self.param(0, 0) {
                0 => self.erase(row, col, self.cols),
                1 => self.erase(row, 0, col + 1),
                _ => self.erase(row, 0, self.cols),
            },
            b'X' => self.erase(row, col, col + n),
            b'P' => {
                for c in col..self.cols {
                    let src = if c + n < self.cols { self.cells[row][c + n] } else { self.blank() };
                    self.set_cell(c, row, src);
                }
            }
            b'@' => {
                for c in (col..self.cols).rev() {
                    let src = if c >= col + n { self.cells[row][c - n] } else { self.blank() };
                    self.set_cell(c, row, src);
                }
            }
            b'm' => self.sgr(),
            b'n' if self.param(0, 0) == 6 => {
                let mut buf = [0u8; 24];
                let mut w = Cursor { buf: &mut buf, len: 0 };
                let _ = fmt::write(&mut w, format_args!("\x1b[{};{}R", row + 1, col + 1));
                let len = w.len;
                self.push_reply(&buf[..len]);
            }
            b'n' if self.param(0, 0) == 5 => self.push_reply(b"\x1b[0n"),
            b'h' | b'l' if self.private && self.param(0, 0) == 25 => {
                self.cursor_visible = final_byte == b'h';
            }
            b's' => self.saved = (col, row),
            b'u' => self.move_to(self.saved.0, self.saved.1),
            _ => {}
        }
    }

    fn control(&mut self, b: u8) {
        match b {
            b'\n' => {
                self.col = 0;
                self.wrap_pending = false;
                self.line_feed();
            }
            b'\r' => self.move_to(0, self.row),
            0x08 => self.move_to(self.col.min(self.cols - 1).saturating_sub(1), self.row),
            b'\t' => self.move_to((self.col / 8 + 1) * 8, self.row),
            0x1b => self.parse = Parse::Escape,
            _ => {}
        }
    }

    fn byte(&mut self, b: u8) {
        match self.parse {
            Parse::Escape => {
                self.parse = Parse::Normal;
                match b {
                    b'[' => {
                        self.parse = Parse::Csi;
                        self.params = [0; MAX_PARAMS];
                        self.nparams = 0;
                        self.private = false;
                    }
                    b'7' => self.saved = (self.col, self.row),
                    b'8' => self.move_to(self.saved.0, self.saved.1),
                    b'c' => {
                        self.fg = DEFAULT_FG;
                        self.bg = DEFAULT_BG;
                        self.clear_screen();
                    }
                    _ => {}
                }
            }
            Parse::Csi => match b {
                b'0'..=b'9' => {
                    if self.nparams == 0 {
                        self.nparams = 1;
                    }
                    if let Some(p) = self.params.get_mut(self.nparams - 1) {
                        *p = p.saturating_mul(10).saturating_add((b - b'0') as u16);
                    }
                }
                b';' => self.nparams = (self.nparams.max(1) + 1).min(MAX_PARAMS),
                b'?' => self.private = true,
                0x40..=0x7e => {
                    self.parse = Parse::Normal;
                    self.csi(b);
                }
                _ => {}
            },
            Parse::Normal => self.text_byte(b),
        }
    }

    fn text_byte(&mut self, b: u8) {
        if self.utf8_need > 0 {
            if b & 0xc0 == 0x80 {
                self.utf8[self.utf8_len] = b;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_need {
                    let ch = core::str::from_utf8(&self.utf8[..self.utf8_len])
                        .ok()
                        .and_then(|s| s.chars().next())
                        .unwrap_or('?');
                    self.utf8_need = 0;
                    self.put_char(ch);
                }
                return;
            }
            self.utf8_need = 0;
            self.put_char('?');
        }
        match b {
            0x00..=0x1f | 0x7f => self.control(b),
            0x20..=0x7e => self.put_char(b as char),
            0xc0..=0xf7 => {
                self.utf8[0] = b;
                self.utf8_len = 1;
                self.utf8_need = match b {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    _ => 4,
                };
            }
            _ => self.put_char('?'),
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.hide_cursor();
        for &b in bytes {
            self.byte(b);
        }
        self.show_cursor();
    }
}

/// Minimal `fmt::Write` sink over a fixed buffer.
struct Cursor<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl fmt::Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

pub static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);

pub fn init(fb: &'static mut FrameBuffer) {
    let info = fb.info();
    let mut console = Console {
        fb: fb.buffer_mut(),
        info,
        cells: unsafe { &mut *(&raw mut CELLS) },
        cols: (info.width / CHAR_WIDTH).min(MAX_COLS),
        rows: (info.height / LINE_HEIGHT).min(MAX_ROWS),
        col: 0,
        row: 0,
        wrap_pending: false,
        fg: DEFAULT_FG,
        bg: DEFAULT_BG,
        bold: false,
        reverse: false,
        saved: (0, 0),
        cursor_visible: true,
        cursor_drawn: None,
        parse: Parse::Normal,
        params: [0; MAX_PARAMS],
        nparams: 0,
        private: false,
        utf8: [0; 4],
        utf8_len: 0,
        utf8_need: 0,
        reply: [0; 32],
        reply_len: 0,
    };
    console.fb.fill(0);
    console.clear_screen();
    *CONSOLE.lock() = Some(console);
}

#[macro_export]
macro_rules! printk {
    ($($arg:tt)*) => ($crate::drivers::console::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! printkln {
    () => ($crate::printk!("\n"));
    ($($arg:tt)*) => ($crate::printk!("{}\n", format_args!($($arg)*)));
}

pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    without_interrupts(|| {
        if let Some(c) = CONSOLE.lock().as_mut() {
            let _ = c.write_fmt(args);
        }
    });
}

pub fn write_bytes(bytes: &[u8]) {
    without_interrupts(|| {
        if let Some(c) = CONSOLE.lock().as_mut() {
            c.write_bytes(bytes);
        }
    });
}

/// Takes the bytes the terminal wants to send back (e.g. a cursor position
/// report); they belong into the TTY input queue.
pub fn take_reply(out: &mut [u8; 32]) -> usize {
    without_interrupts(|| match CONSOLE.lock().as_mut() {
        Some(c) => {
            let n = c.reply_len;
            out[..n].copy_from_slice(&c.reply[..n]);
            c.reply_len = 0;
            n
        }
        None => 0,
    })
}

/// (columns, rows)
pub fn size() -> (usize, usize) {
    without_interrupts(|| CONSOLE.lock().as_ref().map_or((80, 25), |c| (c.cols, c.rows)))
}

pub fn clear_screen() {
    without_interrupts(|| {
        if let Some(c) = CONSOLE.lock().as_mut() {
            c.hide_cursor();
            c.clear_screen();
            c.show_cursor();
        }
    });
}
