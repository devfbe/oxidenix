use bootloader_api::info::{FrameBuffer, FrameBufferInfo, PixelFormat};
use core::fmt;
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};
use spin::Mutex;

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const RASTER_HEIGHT: RasterHeight = RasterHeight::Size16;
const CHAR_WIDTH: usize = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
const LINE_HEIGHT: usize = RASTER_HEIGHT.val() + 2;
const FG: (u8, u8, u8) = (0xc0, 0xc0, 0xc0);

pub struct Console {
    buf: &'static mut [u8],
    info: FrameBufferInfo,
    col: usize,
    row: usize,
}

impl Console {
    fn cols(&self) -> usize {
        self.info.width / CHAR_WIDTH
    }

    fn rows(&self) -> usize {
        self.info.height / LINE_HEIGHT
    }

    fn row_bytes(&self) -> usize {
        self.info.stride * self.info.bytes_per_pixel
    }

    fn put_pixel(&mut self, x: usize, y: usize, intensity: u8) {
        let scale = |c: u8| ((c as u16 * intensity as u16) / 255) as u8;
        let (r, g, b) = (scale(FG.0), scale(FG.1), scale(FG.2));
        let color = match self.info.pixel_format {
            PixelFormat::Rgb => [r, g, b, 0],
            PixelFormat::Bgr => [b, g, r, 0],
            _ => [scale(0xff), 0, 0, 0],
        };
        let bpp = self.info.bytes_per_pixel;
        let off = (y * self.info.stride + x) * bpp;
        self.buf[off..off + bpp].copy_from_slice(&color[..bpp]);
    }

    fn draw_char(&mut self, c: char, col: usize, row: usize) {
        let raster = get_raster(c, FONT_WEIGHT, RASTER_HEIGHT)
            .or_else(|| get_raster('?', FONT_WEIGHT, RASTER_HEIGHT))
            .unwrap();
        let (x0, y0) = (col * CHAR_WIDTH, row * LINE_HEIGHT);
        for (dy, line) in raster.raster().iter().enumerate() {
            for (dx, &intensity) in line.iter().enumerate() {
                self.put_pixel(x0 + dx, y0 + dy, intensity);
            }
        }
    }

    fn clear_cell(&mut self, col: usize, row: usize) {
        let (x0, y0) = (col * CHAR_WIDTH, row * LINE_HEIGHT);
        let bpp = self.info.bytes_per_pixel;
        for y in y0..y0 + LINE_HEIGHT {
            let start = (y * self.info.stride + x0) * bpp;
            self.buf[start..start + CHAR_WIDTH * bpp].fill(0);
        }
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            b'\r' => self.col = 0,
            byte => {
                if self.col >= self.cols() {
                    self.new_line();
                }
                self.draw_char(byte as char, self.col, self.row);
                self.col += 1;
            }
        }
    }

    pub fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                0x20..=0x7e | b'\n' | b'\r' => self.write_byte(byte),
                _ => self.write_byte(b'?'),
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.clear_cell(self.col, self.row);
        }
    }

    pub fn clear_screen(&mut self) {
        self.buf.fill(0);
        self.col = 0;
        self.row = 0;
    }

    fn new_line(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows() {
            self.row += 1;
            return;
        }
        let line = LINE_HEIGHT * self.row_bytes();
        let used = self.rows() * line;
        self.buf.copy_within(line..used, 0);
        self.buf[used - line..used].fill(0);
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Console::write_str(self, s);
        Ok(())
    }
}

pub static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);

pub fn init(fb: &'static mut FrameBuffer) {
    let info = fb.info();
    let mut console = Console {
        buf: fb.buffer_mut(),
        info,
        col: 0,
        row: 0,
    };
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
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(c) = CONSOLE.lock().as_mut() {
            let _ = c.write_fmt(args);
        }
    });
}

pub fn clear_screen() {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.clear_screen();
    }
}

pub fn backspace() {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.backspace();
    }
}
