use core::fmt;
use lazy_static::lazy_static;
use spin::Mutex;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ColorCode(u8);

impl ColorCode {
    fn new(fg: Color, bg: Color) -> Self {
        ColorCode((bg as u8) << 4 | (fg as u8))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii: u8,
    color: ColorCode,
}

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

// Raw VGA buffer pointer — always write through volatile to prevent optimizer elision.
const VGA_BUFFER: *mut [[ScreenChar; BUFFER_WIDTH]; BUFFER_HEIGHT] = 0xb8000 as *mut _;

pub struct Writer {
    col: usize,
    row: usize,
    color: ColorCode,
}

impl Writer {
    fn cell_write(&mut self, row: usize, col: usize, ch: ScreenChar) {
        unsafe {
            core::ptr::write_volatile(&mut (*VGA_BUFFER)[row][col], ch);
        }
    }

    fn cell_read(&self, row: usize, col: usize) -> ScreenChar {
        unsafe { core::ptr::read_volatile(&(*VGA_BUFFER)[row][col]) }
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            b'\r' => {
                self.col = 0;
            }
            byte => {
                if self.col >= BUFFER_WIDTH {
                    self.new_line();
                }
                let color = self.color;
                self.cell_write(self.row, self.col, ScreenChar { ascii: byte, color });
                self.col += 1;
            }
        }
    }

    pub fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                0x20..=0x7e | b'\n' | b'\r' => self.write_byte(byte),
                _ => self.write_byte(0xfe),
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let color = self.color;
            self.cell_write(self.row, self.col, ScreenChar { ascii: b' ', color });
        }
    }

    pub fn clear_screen(&mut self) {
        for row in 0..BUFFER_HEIGHT {
            self.clear_row(row);
        }
        self.col = 0;
        self.row = 0;
    }

    fn new_line(&mut self) {
        if self.row < BUFFER_HEIGHT - 1 {
            self.row += 1;
        } else {
            for row in 1..BUFFER_HEIGHT {
                for col in 0..BUFFER_WIDTH {
                    let ch = self.cell_read(row, col);
                    self.cell_write(row - 1, col, ch);
                }
            }
            self.clear_row(BUFFER_HEIGHT - 1);
        }
        self.col = 0;
    }

    fn clear_row(&mut self, row: usize) {
        let blank = ScreenChar {
            ascii: b' ',
            color: self.color,
        };
        for col in 0..BUFFER_WIDTH {
            self.cell_write(row, col, blank);
        }
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Writer::write_str(self, s);
        Ok(())
    }
}

lazy_static! {
    pub static ref WRITER: Mutex<Writer> = Mutex::new(Writer {
        col: 0,
        row: 0,
        color: ColorCode::new(Color::LightGray, Color::Black),
    });
}

#[macro_export]
macro_rules! printk {
    ($($arg:tt)*) => ($crate::drivers::vga::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! printkln {
    () => ($crate::printk!("\n"));
    ($($arg:tt)*) => ($crate::printk!("{}\n", format_args!($($arg)*)));
}

pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    WRITER.lock().write_fmt(args).unwrap();
}

pub fn clear_screen() {
    WRITER.lock().clear_screen();
}

pub fn backspace() {
    WRITER.lock().backspace();
}
