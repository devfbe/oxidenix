//! Keyboards of the HID class in the boot protocol (HID 1.11, appendix B):
//! the 8-byte reports (modifier bits, a reserved byte, up to six keys
//! pressed) turned into the PS/2 keyboard's scancodes (set 1, as the i8042
//! translates them), which the kernel's keyboard queue takes and the Linux
//! keyboard (crates/kbd) understands: a USB keyboard types as a PS/2 one.
//! The kernel takes the firmware's USB legacy emulation away when usbd
//! takes the controller over; this is what keeps a USB keyboard working.
//!
//! USB keyboards do not repeat keys by themselves (PS/2 ones do): the host
//! does (`Repeat`), with the PS/2 keyboard's typematic settings.

/// The interface class, the boot subclass and the keyboard protocol.
pub const CLASS_HID: u8 = 0x03;
pub const SUBCLASS_BOOT: u8 = 0x01;
pub const PROTOCOL_KEYBOARD: u8 = 0x01;

/// Class requests.
pub const SET_REPORT: u8 = 0x09;
pub const SET_IDLE: u8 = 0x0a;
pub const SET_PROTOCOL: u8 = 0x0b;
/// SET_PROTOCOL's value for the boot protocol.
pub const BOOT_PROTOCOL: u16 = 0;
/// SET_REPORT's value for output report 0 (the LEDs).
pub const OUTPUT_REPORT: u16 = 2 << 8;

pub const REPORT_BYTES: usize = 8;

/// The output report's LED bits.
pub const LED_NUM_LOCK: u8 = 1 << 0;
pub const LED_CAPS_LOCK: u8 = 1 << 1;
pub const LED_SCROLL_LOCK: u8 = 1 << 2;

/// The LEDs as the output report has them, from the PS/2 keyboard's
/// (the kernel's: bit 0 Scroll Lock, 1 Num Lock, 2 Caps Lock).
pub fn leds_from_ps2(leds: u8) -> u8 {
    (if leds & 1 != 0 { LED_SCROLL_LOCK } else { 0 }) | (if leds & 2 != 0 { LED_NUM_LOCK } else { 0 }) | (if leds & 4 != 0 { LED_CAPS_LOCK } else { 0 })
}

/// The modifier bits' scancodes (left control, shift, alt, GUI; right
/// control, shift, alt, GUI).
const MODIFIERS: [&[u8]; 8] = [&[0x1d], &[0x2a], &[0x38], &[0xe0, 0x5b], &[0xe0, 0x1d], &[0x36], &[0xe0, 0x38], &[0xe0, 0x5c]];

/// A key's make code in set 1 (with its 0xe0 prefix where it has one), by
/// its usage (HID usage tables, keyboard page 0x07); None for usages that
/// have none.
pub fn make_code(usage: u8) -> Option<&'static [u8]> {
    const LETTERS: [u8; 26] = [
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
    ];
    // Usages 0x1e-0x38: 1-9, 0, Enter, Escape, Backspace, Tab, Space, - = [ ] \ (non-US #) ; ' ` , . /
    const ROW: [u8; 27] = [
        0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x1c, 0x01, 0x0e, 0x0f, 0x39, 0x0c, 0x0d, 0x1a, 0x1b, 0x2b, 0x2b, 0x27, 0x28, 0x29, 0x33, 0x34, 0x35,
    ];
    static SINGLE: [[u8; 1]; 256] = {
        let mut t = [[0u8; 1]; 256];
        let mut i = 0;
        while i < 26 {
            t[0x04 + i][0] = LETTERS[i];
            i += 1;
        }
        let mut i = 0;
        while i < 27 {
            t[0x1e + i][0] = ROW[i];
            i += 1;
        }
        // Caps Lock, F1-F10, F11, F12, Scroll Lock.
        t[0x39][0] = 0x3a;
        let mut f = 0;
        while f < 10 {
            t[0x3a + f][0] = 0x3b + f as u8;
            f += 1;
        }
        t[0x44][0] = 0x57;
        t[0x45][0] = 0x58;
        t[0x47][0] = 0x46;
        // Num Lock, the keypad (* - + 1-9 0 .), the non-US \ (<>| on German
        // keyboards), and the international keys.
        t[0x53][0] = 0x45;
        t[0x55][0] = 0x37;
        t[0x56][0] = 0x4a;
        t[0x57][0] = 0x4e;
        let pad = [0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49, 0x52, 0x53];
        let mut k = 0;
        while k < 11 {
            t[0x59 + k][0] = pad[k];
            k += 1;
        }
        t[0x64][0] = 0x56;
        t[0x87][0] = 0x73;
        t[0x88][0] = 0x70;
        t[0x89][0] = 0x7d;
        t[0x8a][0] = 0x79;
        t[0x8b][0] = 0x7b;
        t
    };
    static EXTENDED: [[u8; 2]; 256] = {
        let mut t = [[0u8; 2]; 256];
        // Print Screen (its "make" as Linux's atkbd takes it), Insert, Home,
        // Page Up, Delete, End, Page Down, Right, Left, Down, Up, keypad /,
        // keypad Enter, the application (menu) key.
        let codes: [(usize, u8); 14] =
            [(0x46, 0x37), (0x49, 0x52), (0x4a, 0x47), (0x4b, 0x49), (0x4c, 0x53), (0x4d, 0x4f), (0x4e, 0x51), (0x4f, 0x4d), (0x50, 0x4b), (0x51, 0x50), (0x52, 0x48), (0x54, 0x35), (0x58, 0x1c), (0x65, 0x5d)];
        let mut i = 0;
        while i < codes.len() {
            t[codes[i].0] = [0xe0, codes[i].1];
            i += 1;
        }
        t
    };
    if usage == 0x48 {
        // Pause: its whole sequence is the make; it has no break.
        return Some(&[0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]);
    }
    if EXTENDED[usage as usize][0] != 0 {
        return Some(&EXTENDED[usage as usize]);
    }
    let s = &SINGLE[usage as usize];
    (s[0] != 0).then_some(&s[..])
}

/// A make code's break code (the last byte with bit 7), into `out`; its
/// length (0 for Pause, which has none).
fn break_code(make: &[u8], out: &mut [u8; 2]) -> usize {
    if make[0] == 0xe1 {
        return 0;
    }
    out[..make.len()].copy_from_slice(make);
    out[make.len() - 1] |= 0x80;
    make.len()
}

/// A keyboard's last report: what changed in the next one becomes
/// scancodes.
#[derive(Default)]
pub struct Keyboard {
    modifiers: u8,
    keys: [u8; 6],
}

/// Usages 0-3 in a key slot are no keys (none, roll-over error, POST
/// fail, undefined error).
fn is_key(usage: u8) -> bool {
    usage > 3
}

impl Keyboard {
    /// The report `r`'s changes as scancodes, each sequence through
    /// `emit` (breaks first, then makes). A roll-over report (more keys
    /// than it holds) changes nothing. The key pressed last, if any key
    /// is held that repeats (`Repeat`).
    pub fn report(&mut self, r: &[u8], mut emit: impl FnMut(&[u8])) -> Option<u8> {
        if r.len() < REPORT_BYTES {
            return self.held();
        }
        let (modifiers, keys) = (r[0], [r[2], r[3], r[4], r[5], r[6], r[7]]);
        if keys.contains(&1) {
            return self.held();
        }
        let mut b = [0u8; 2];
        for (bit, code) in MODIFIERS.iter().enumerate() {
            let (was, is) = (self.modifiers & 1 << bit != 0, modifiers & 1 << bit != 0);
            if was && !is {
                let n = break_code(code, &mut b);
                emit(&b[..n]);
            }
        }
        for &k in self.keys.iter().filter(|&&k| is_key(k) && !keys.contains(&k)) {
            if let Some(make) = make_code(k) {
                let n = break_code(make, &mut b);
                if n > 0 {
                    emit(&b[..n]);
                }
            }
        }
        for (bit, code) in MODIFIERS.iter().enumerate() {
            if self.modifiers & 1 << bit == 0 && modifiers & 1 << bit != 0 {
                emit(code);
            }
        }
        let mut last = None;
        for &k in keys.iter().filter(|&&k| is_key(k) && !self.keys.contains(&k)) {
            if let Some(make) = make_code(k) {
                emit(make);
                last = Some(k);
            }
        }
        self.modifiers = modifiers;
        self.keys = keys;
        last.or_else(|| self.held())
    }

    /// A key still held that repeats (the last in the report: the newest).
    fn held(&self) -> Option<u8> {
        self.keys.iter().rev().copied().find(|&k| is_key(k) && repeats(k))
    }

    /// Every key and modifier up (the keyboard went).
    pub fn release_all(&mut self, emit: impl FnMut(&[u8])) {
        self.report(&[0; REPORT_BYTES], emit);
    }
}

/// Whether a key repeats while held (as on a PS/2 keyboard: all but the
/// locks, Pause and Print Screen).
pub fn repeats(usage: u8) -> bool {
    !matches!(usage, 0x39 | 0x47 | 0x48 | 0x53 | 0x46) && make_code(usage).is_some()
}

/// The PS/2 typematic byte's delay and period, in milliseconds (bits 5-6:
/// 250, 500, 750, 1000 ms; bits 0-4: 2^B * (8 + D) * 4.17 ms).
pub fn typematic(byte: u8) -> (u64, u64) {
    let delay = 250 * (1 + (byte >> 5 & 0b11) as u64);
    let (d, b) = ((byte & 0b111) as u64, (byte >> 3 & 0b11) as u64);
    let period = ((1 << b) * (8 + d) * 417 + 50) / 100;
    (delay, period)
}

/// The host's autorepeat of the key held last.
#[derive(Default)]
pub struct Repeat {
    /// The key and when its next repeat is due (uptime, ms).
    key: Option<(u8, u64)>,
}

impl Repeat {
    /// The keyboard's report made `held` the key held last, at `now`.
    pub fn held(&mut self, held: Option<u8>, now: u64, delay: u64) {
        self.key = match (held, self.key) {
            (Some(k), Some((old, at))) if k == old => Some((k, at)),
            (Some(k), _) => Some((k, now + delay)),
            (None, _) => None,
        };
    }

    /// The make code to repeat now, if one is due (the next one then a
    /// period later).
    pub fn due(&mut self, now: u64, period: u64) -> Option<&'static [u8]> {
        let (k, at) = self.key?;
        if now < at {
            return None;
        }
        self.key = Some((k, now.max(at) + period.max(1)));
        make_code(k)
    }

    /// When the next repeat is due, if one is.
    pub fn deadline(&self) -> Option<u64> {
        self.key.map(|(_, at)| at)
    }
}
