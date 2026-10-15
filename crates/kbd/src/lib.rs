//! Linux's console keyboard as a library (drivers/tty/vt/keyboard.c), for the Linux
//! server's virtual terminal and the kernel's monitor (ADR 0020): PS/2 scancodes (set 1,
//! as the controller translates them) become Linux keycodes, and keycodes become the
//! terminal's input by the keyboard mode (`K_RAW`: the scancodes, `K_MEDIUMRAW`: the
//! keycodes, `K_XLATE`: Latin-1 bytes, `K_UNICODE`: UTF-8, `K_OFF`: nothing) and the
//! keymaps (up to 256 tables of 256 keysyms, one per combination of modifiers), with
//! Linux's modifier and lock state, the LEDs' flags, dead keys and the accent table, the
//! function keys' strings, the keypad, Alt-numpad entry and the meta key. What it cannot
//! do itself (stop output, switch consoles, reboot) it reports as `Action`s.
//!
//! It only computes: the device, the terminal and the console's modes (application keypad,
//! cursor keys, new line mode, autorepeat: the console's escape sequences set them) are the
//! caller's.

#![no_std]

extern crate alloc;

mod defkeymap;
pub mod keysym;

use alloc::vec::Vec;
use keysym::*;

/// Keysyms per keymap and keymaps (linux/keyboard.h).
pub const NR_KEYS: usize = 256;
pub const MAX_NR_KEYMAPS: usize = 256;
/// Function strings (KT_FN values), and their longest (`kb_string` holds 512 bytes with
/// the NUL).
pub const MAX_NR_FUNC: usize = 256;
pub const FUNC_MAX: usize = 511;
/// Entries of the accent table at most.
pub const MAX_DIACR: usize = 256;

/// KDGKBMETA's answers.
pub const K_METABIT: u8 = 3;
pub const K_ESCPREFIX: u8 = 4;

/// Linux's keycodes this crate names itself (linux/input-event-codes.h), and the largest.
pub const KEY_PAUSE: u16 = 119;
pub const KEY_MAX: u16 = 0x2ff;

/// The keyboard mode (KDGKBMODE, KDSKBMODE: `K_RAW` 0 to `K_OFF` 4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Raw = 0,
    Xlate = 1,
    MediumRaw = 2,
    Unicode = 3,
    Off = 4,
}

impl Mode {
    pub fn from_raw(v: u64) -> Option<Mode> {
        Some(match v {
            0 => Mode::Raw,
            1 => Mode::Xlate,
            2 => Mode::MediumRaw,
            3 => Mode::Unicode,
            4 => Mode::Off,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// EINVAL.
    Inval,
    /// ENOMEM.
    NoMem,
}

/// The console's modes the keyboard depends on (Linux keeps them in the keyboard's
/// flags; the console's escape sequences set them): DECKPAM (the keypad's application
/// keys), DECCKM (cursor keys as ESC O), LNM (Enter sends CR LF) and DECARM (autorepeat).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConsoleModes {
    pub applic: bool,
    pub ckmode: bool,
    pub crlf: bool,
    pub repeat: bool,
}

impl Default for ConsoleModes {
    fn default() -> Self {
        ConsoleModes { applic: false, ckmode: false, crlf: false, repeat: true }
    }
}

/// What a key asks of the console or the system beyond input bytes (KT_SPEC, KT_CONS).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Scroll Lock: output stops, or starts again (Linux's `fn_hold`).
    Hold,
    /// A break on the terminal's input (`fn_send_intr`).
    Break,
    /// Ctrl-Alt-Del (`fn_boot_it`).
    Boot,
    /// The secure attention key.
    Sak,
    /// The keyboard signal (KDSIGACCEPT's, `fn_spawn_con`).
    Spawn,
    /// Switch to console `n` (0: tty1), the previous, the next or the last one.
    Console(u8),
    DecConsole,
    IncConsole,
    LastConsole,
    ScrollBack,
    ScrollForward,
    ShowRegs,
    ShowMem,
    ShowState,
}

/// An accent table entry: dead key `diacr` and `base` make `result` (code points).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Diacr {
    pub diacr: u32,
    pub base: u32,
    pub result: u32,
}

/// What a scancode or keycode made: bytes for the terminal and actions, as many as the
/// room made for them (`try_new`) takes; the rest is dropped (as input that overflows a
/// full buffer).
pub struct Output {
    pub bytes: Vec<u8>,
    pub actions: Vec<Action>,
}

impl Output {
    /// Room for `bytes` bytes and `actions` actions, or ENOMEM.
    pub fn try_new(bytes: usize, actions: usize) -> Result<Output, Error> {
        let mut out = Output { bytes: Vec::new(), actions: Vec::new() };
        out.bytes.try_reserve_exact(bytes).map_err(|_| Error::NoMem)?;
        out.actions.try_reserve_exact(actions).map_err(|_| Error::NoMem)?;
        Ok(out)
    }

    /// Empties it (the room stays).
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.actions.clear();
    }

    fn put(&mut self, b: u8) {
        if self.bytes.len() < self.bytes.capacity() {
            self.bytes.push(b);
        }
    }

    fn put_all(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.put(b);
        }
    }

    fn act(&mut self, a: Action) {
        if self.actions.len() < self.actions.capacity() {
            self.actions.push(a);
        }
    }
}

/// The room one scancode's output needs at most: a function key's string.
pub const OUTPUT_ROOM: usize = FUNC_MAX + 1;

/// Where a scancode sequence stands (E0 and E1 prefixes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prefix {
    None,
    E0,
    /// E1 1D 45 (Pause pressed) or E1 9D C5: the bytes after E1 so far.
    E1(u8),
}

/// The LED flag bits (KDGKBLED's low three, KDSETLED's and the PS/2 command's).
pub const LED_SCR: u8 = 1;
pub const LED_NUM: u8 = 2;
pub const LED_CAP: u8 = 4;

pub struct Keyboard {
    /// The keymaps by modifier combination (bit `KG_*` set for each modifier held,
    /// locked or sticky): entries `U(K(t, v))` or Unicode code points.
    maps: Vec<Option<Vec<u16>>>,
    funcs: Vec<Vec<u8>>,
    accents: Vec<Diacr>,
    mode: Mode,
    /// VC_META: the meta key sends ESC first (K_ESCPREFIX), else sets bit 7.
    meta_esc: bool,
    /// The LED flags (scroll, num and caps lock) and their defaults (KDGKBLED).
    ledflagstate: u8,
    default_ledflagstate: u8,
    /// KDSETLED's LEDs, while they show those rather than the flags.
    ledioctl: Option<u8>,
    /// KT_LOCK's locked modifiers and KT_SLOCK's sticky ones.
    lockstate: u8,
    slockstate: u8,
    shift_state: u8,
    shift_down: [u8; NR_SHIFT],
    key_down: [u64; (KEY_MAX as usize + 1) / 64],
    /// The keycode of each scancode (KDGETKEYCODE, KDSETKEYCODE): set 1's codes, 128 up
    /// for those after an E0 prefix (as setkeycodes numbers them).
    keycodes: [u16; 256],
    /// The dead key waiting for its base (a code point, 0: none), and Compose's.
    diacr: u32,
    dead_key_next: bool,
    /// Alt-numpad entry (KT_ASCII) under way.
    npadch: Option<u32>,
    prefix: Prefix,
    /// The key event being handled is an autorepeat.
    rep: bool,
}

/// PS/2 scancode set 1 to Linux keycodes: the codes without a prefix (0 for none).
fn plain_keycode(sc: u8) -> u16 {
    match sc {
        0x01..=0x53 | 0x56..=0x58 => sc as u16,
        // Alt+Print sends SysRq's own code.
        0x54 => 99,
        0x59 => 117, // KEY_KPEQUAL
        0x5a..=0x5f => 0,
        0x70 => 93,  // KEY_KATAKANAHIRAGANA
        0x73 => 89,  // KEY_RO
        0x79 => 92,  // KEY_HENKAN
        0x7b => 94,  // KEY_MUHENKAN
        0x7d => 124, // KEY_YEN
        0x7e => 121, // KEY_KPCOMMA
        _ => 0,
    }
}

/// The codes after an E0 prefix (0 for none, and for the fake shifts some keyboards
/// send around the gray keys).
fn e0_keycode(sc: u8) -> u16 {
    match sc {
        0x1c => 96,  // KEY_KPENTER
        0x1d => 97,  // KEY_RIGHTCTRL
        0x20 => 113, // KEY_MUTE
        0x2e => 114, // KEY_VOLUMEDOWN
        0x30 => 115, // KEY_VOLUMEUP
        0x35 => 98,  // KEY_KPSLASH
        0x37 => 99,  // KEY_SYSRQ (Print)
        0x38 => 100, // KEY_RIGHTALT
        0x46 => 119, // KEY_PAUSE (Ctrl+Break)
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4b => 105, // KEY_LEFT
        0x4d => 106, // KEY_RIGHT
        0x4f => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5b => 125, // KEY_LEFTMETA
        0x5c => 126, // KEY_RIGHTMETA
        0x5d => 127, // KEY_COMPOSE
        0x5e => 116, // KEY_POWER
        0x5f => 142, // KEY_SLEEP
        0x63 => 143, // KEY_WAKEUP
        _ => 0,
    }
}

/// Appends `c` as UTF-8 (Linux's `to_utf8`: no surrogates, no U+FFFF).
fn to_utf8(c: u32, out: &mut Output) {
    if (0xd800..0xe000).contains(&c) || c == 0xffff || c >= 0x110000 {
        return;
    }
    if let Some(ch) = char::from_u32(c) {
        let mut buf = [0u8; 4];
        out.put_all(ch.encode_utf8(&mut buf).as_bytes());
    }
}

/// A vector of `n` copies of `v`, or ENOMEM.
fn try_filled<T: Clone>(n: usize, v: T) -> Result<Vec<T>, Error> {
    let mut out = Vec::new();
    out.try_reserve_exact(n).map_err(|_| Error::NoMem)?;
    out.resize(n, v);
    Ok(out)
}

impl Keyboard {
    /// A keyboard with the default keymap (German, as the console's machine has), the
    /// default function strings and accent table, in Unicode mode (Linux's
    /// `default_utf8`), Alt sending ESC (KBD_DEFMODE's VC_META), no LED lit.
    pub fn new() -> Result<Keyboard, Error> {
        let mut maps = try_filled(MAX_NR_KEYMAPS, None)?;
        for (table, entries) in defkeymap::maps() {
            let mut map = try_filled(NR_KEYS, u(K_HOLE))?;
            for (keycode, sym) in entries {
                map[keycode as usize] = u(sym);
            }
            maps[table] = Some(map);
        }
        let mut funcs = try_filled(MAX_NR_FUNC, Vec::new())?;
        for (i, s) in defkeymap::FUNCS.iter().enumerate() {
            let mut v = Vec::new();
            v.try_reserve_exact(s.len()).map_err(|_| Error::NoMem)?;
            v.extend_from_slice(s);
            funcs[i] = v;
        }
        let mut accents = Vec::new();
        accents.try_reserve_exact(defkeymap::ACCENTS.len()).map_err(|_| Error::NoMem)?;
        accents.extend(defkeymap::ACCENTS.iter().map(|&(d, b, r)| Diacr { diacr: d as u32, base: b as u32, result: r as u32 }));
        Ok(Keyboard {
            maps,
            funcs,
            accents,
            mode: Mode::Unicode,
            meta_esc: true,
            ledflagstate: 0,
            default_ledflagstate: 0,
            ledioctl: None,
            lockstate: 0,
            slockstate: 0,
            shift_state: 0,
            shift_down: [0; NR_SHIFT],
            key_down: [0; (KEY_MAX as usize + 1) / 64],
            keycodes: core::array::from_fn(|i| if i < 128 { plain_keycode(i as u8) } else { e0_keycode(i as u8 & 0x7f) }),
            diacr: 0,
            dead_key_next: false,
            npadch: None,
            prefix: Prefix::None,
            rep: false,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// KDSKBMODE: entering a translating mode recomputes the shift state from the keys
    /// held (Linux's `vt_do_kdskbmode`).
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if matches!(mode, Mode::Xlate | Mode::Unicode) {
            self.compute_shiftstate();
        }
    }

    /// KDGKBMETA.
    pub fn meta(&self) -> u8 {
        if self.meta_esc { K_ESCPREFIX } else { K_METABIT }
    }

    /// KDSKBMETA.
    pub fn set_meta(&mut self, v: u64) -> Result<(), Error> {
        match v {
            3 => self.meta_esc = false,
            4 => self.meta_esc = true,
            _ => return Err(Error::Inval),
        }
        Ok(())
    }

    /// KDGKBLED: the lock flags and, four bits up, their defaults.
    pub fn led_flags(&self) -> u8 {
        self.ledflagstate | self.default_ledflagstate << 4
    }

    /// KDSKBLED.
    pub fn set_led_flags(&mut self, v: u64) -> Result<(), Error> {
        if v & !0x77 != 0 {
            return Err(Error::Inval);
        }
        self.ledflagstate = v as u8 & 7;
        self.default_ledflagstate = (v >> 4) as u8 & 7;
        Ok(())
    }

    /// The LEDs as they should be lit (KDGETLED): KDSETLED's, or the flags.
    pub fn leds(&self) -> u8 {
        self.ledioctl.unwrap_or(self.ledflagstate & 7)
    }

    /// KDSETLED: these LEDs regardless of the flags; a value beyond the three LEDs
    /// gives them back to the flags (Linux's `setledstate`).
    pub fn set_leds(&mut self, v: u64) {
        self.ledioctl = (v & !7 == 0).then_some(v as u8);
    }

    /// The console's output stopped or started (Linux's `vt_kbd_con_stop` and
    /// `_start`): the Scroll Lock flag shows it.
    pub fn set_scroll_lock(&mut self, on: bool) {
        if on {
            self.ledflagstate |= LED_SCR;
        } else {
            self.ledflagstate &= !LED_SCR;
        }
    }

    /// The modifiers held (TIOCLINUX's TIOCL_GETSHIFTSTATE).
    pub fn shift_state(&self) -> u8 {
        self.shift_state
    }

    /// KDGKBENT: the keysym at `index` of keymap `table` (K_NOSUCHMAP for entry 0 of a
    /// keymap that does not exist, K_HOLE for the others; a Unicode keysym outside
    /// Unicode mode shows as K_HOLE).
    pub fn entry(&self, table: u8, index: u8) -> u16 {
        match &self.maps[table as usize] {
            Some(map) => {
                let v = u(map[index as usize]);
                if self.mode != Mode::Unicode && ktyp(v) >= NR_TYPES {
                    K_HOLE
                } else {
                    v
                }
            }
            None if index != 0 => K_HOLE,
            None => K_NOSUCHMAP,
        }
    }

    /// KDSKBENT (Linux's `vt_do_kdsk_ioctl`): K_NOSUCHMAP at entry 0 removes a keymap
    /// (not the plain one); a keysym's value must be within its type's; a Unicode keysym
    /// needs Unicode mode; entry 0 is only checked; a missing keymap is made of holes.
    pub fn set_entry(&mut self, table: u8, index: u8, v: u16) -> Result<(), Error> {
        let t = table as usize;
        if index == 0 && v == K_NOSUCHMAP {
            if t != 0 {
                self.maps[t] = None;
            }
            return Ok(());
        }
        if ktyp(v) < NR_TYPES {
            if kval(v) > MAX_VALS[ktyp(v) as usize] {
                return Err(Error::Inval);
            }
        } else if self.mode != Mode::Unicode {
            return Err(Error::Inval);
        }
        if index == 0 {
            return Ok(());
        }
        if self.maps[t].is_none() {
            let mut map = try_filled(NR_KEYS, u(K_HOLE))?;
            map[0] = u(K_ALLOCATED);
            self.maps[t] = Some(map);
        }
        let map = self.maps[t].as_mut().expect("made above");
        let old = u(map[index as usize]);
        if old == v {
            return Ok(());
        }
        map[index as usize] = u(v);
        if t == 0 && (ktyp(old) == KT_SHIFT || ktyp(v) == KT_SHIFT) {
            self.compute_shiftstate();
        }
        Ok(())
    }

    /// KDGETKEYCODE: scancode `sc`'s keycode (0-127 set 1's, 128-255 after E0).
    pub fn getkeycode(&self, sc: u32) -> Result<u16, Error> {
        self.keycodes.get(sc as usize).copied().ok_or(Error::Inval)
    }

    /// KDSETKEYCODE.
    pub fn setkeycode(&mut self, sc: u32, keycode: u32) -> Result<(), Error> {
        if sc as usize >= self.keycodes.len() || keycode > KEY_MAX as u32 {
            return Err(Error::Inval);
        }
        self.keycodes[sc as usize] = keycode as u16;
        Ok(())
    }

    /// KDGKBSENT: function key `i`'s string.
    pub fn func(&self, i: u8) -> &[u8] {
        &self.funcs[i as usize]
    }

    /// KDSKBSENT (the string without its NUL, at most `FUNC_MAX` bytes).
    pub fn set_func(&mut self, i: u8, s: &[u8]) -> Result<(), Error> {
        if s.len() > FUNC_MAX {
            return Err(Error::Inval);
        }
        let mut v = Vec::new();
        v.try_reserve_exact(s.len()).map_err(|_| Error::NoMem)?;
        v.extend_from_slice(s);
        self.funcs[i as usize] = v;
        Ok(())
    }

    /// KDGKBDIACR(UC): the accent table.
    pub fn accents(&self) -> &[Diacr] {
        &self.accents
    }

    /// KDSKBDIACR(UC): a new accent table (fewer than `MAX_DIACR` entries).
    pub fn set_accents(&mut self, entries: &[Diacr]) -> Result<(), Error> {
        if entries.len() >= MAX_DIACR {
            return Err(Error::Inval);
        }
        let mut v = Vec::new();
        v.try_reserve_exact(entries.len()).map_err(|_| Error::NoMem)?;
        v.extend_from_slice(entries);
        self.accents = v;
        Ok(())
    }

    fn is_down(&self, keycode: u16) -> bool {
        self.key_down[keycode as usize / 64] & 1 << (keycode % 64) != 0
    }

    fn set_down(&mut self, keycode: u16, down: bool) {
        let (word, bit) = (keycode as usize / 64, 1u64 << (keycode % 64));
        if down {
            self.key_down[word] |= bit;
        } else {
            self.key_down[word] &= !bit;
        }
    }

    /// The shift state from the keys held, by the plain keymap (Linux's
    /// `do_compute_shiftstate`).
    fn compute_shiftstate(&mut self) {
        self.shift_state = 0;
        self.shift_down = [0; NR_SHIFT];
        let Some(plain) = &self.maps[0] else { return };
        for keycode in 0..NR_KEYS as u16 {
            if !self.is_down(keycode) {
                continue;
            }
            let sym = plain[keycode as usize];
            let t = ktyp(sym).wrapping_sub(0xf0);
            if t != KT_SHIFT && t != KT_SLOCK {
                continue;
            }
            let mut val = kval(sym);
            if val == KG_CAPSSHIFT {
                val = KG_SHIFT;
            }
            if (val as usize) < NR_SHIFT {
                self.shift_down[val as usize] += 1;
                self.shift_state |= 1 << val;
            }
        }
    }

    /// A byte from the keyboard controller (scancode set 1).
    pub fn scancode(&mut self, sc: u8, modes: ConsoleModes, out: &mut Output) {
        if self.mode == Mode::Raw {
            // As the hardware sent it (Linux's `kbd_rawcode`).
            out.put(sc);
        }
        let (keycode, down) = match self.prefix {
            Prefix::E1(seen) => {
                // E1 1D 45: Pause pressed; E1 9D C5: released.
                if seen == 0 {
                    self.prefix = Prefix::E1(1);
                    return;
                }
                self.prefix = Prefix::None;
                (KEY_PAUSE, sc & 0x80 == 0)
            }
            _ if sc == 0xe0 => {
                self.prefix = Prefix::E0;
                return;
            }
            _ if sc == 0xe1 => {
                self.prefix = Prefix::E1(0);
                return;
            }
            Prefix::E0 => {
                self.prefix = Prefix::None;
                (self.keycodes[0x80 | (sc & 0x7f) as usize], sc & 0x80 == 0)
            }
            Prefix::None => {
                // The controller's answers and errors are no keys.
                if matches!(sc, 0x00 | 0xfa | 0xfe | 0xff) {
                    return;
                }
                (self.keycodes[(sc & 0x7f) as usize], sc & 0x80 == 0)
            }
        };
        if keycode != 0 {
            self.keycode(keycode, down, modes, out);
        }
    }

    /// A key pressed or released (Linux's `kbd_keycode`); pressed again while held is an
    /// autorepeat.
    pub fn keycode(&mut self, keycode: u16, down: bool, modes: ConsoleModes, out: &mut Output) {
        if keycode == 0 || keycode > KEY_MAX {
            return;
        }
        self.rep = down && self.is_down(keycode);
        let mut raw = self.mode == Mode::Raw;
        if self.mode == Mode::MediumRaw {
            // Keycodes above 127 as 0, the high seven bits, the low seven bits, the up
            // flag on the 0 and bit 7 set on the other two.
            let up = if down { 0 } else { 0x80 };
            if keycode < 128 {
                out.put(keycode as u8 | up);
            } else {
                out.put_all(&[up, (keycode >> 7) as u8 | 0x80, keycode as u8 | 0x80]);
            }
            raw = true;
        }
        self.set_down(keycode, down);
        if self.rep && !modes.repeat {
            return;
        }
        let shift_final = (self.shift_state | self.slockstate) ^ self.lockstate;
        let Some(map) = &self.maps[shift_final as usize] else {
            self.compute_shiftstate();
            self.slockstate = 0;
            return;
        };
        // (Keycodes beyond the keymaps make no keysym.)
        let Some(&entry) = map.get(keycode as usize) else { return };
        let mut sym = entry;
        let t = ktyp(sym);
        if t < 0xf0 {
            // A Unicode keysym.
            if down && !raw {
                self.k_unicode(sym as u32, false, out);
            }
            return;
        }
        let mut t = t - 0xf0;
        if t == KT_LETTER {
            t = KT_LATIN;
            if self.ledflagstate & LED_CAP != 0 {
                if let Some(other) = &self.maps[(shift_final ^ 1 << KG_SHIFT) as usize] {
                    sym = other[keycode as usize];
                }
            }
        }
        if (raw || self.mode == Mode::Off) && t != KT_SPEC && t != KT_SHIFT {
            return;
        }
        let v = kval(sym);
        let up = !down;
        match t {
            KT_LATIN => self.k_unicode(v as u32, up, out),
            KT_FN => {
                if !up {
                    self.k_fn(v, out);
                }
            }
            KT_SPEC => self.k_spec(v, up, modes, out),
            KT_PAD => self.k_pad(v, up, modes, out),
            KT_DEAD => {
                if let Some(&c) = DEAD_CHARS.get(v as usize) {
                    self.k_deadunicode(c as u32, up, out);
                }
            }
            KT_CONS => {
                if !up {
                    out.act(Action::Console(v));
                }
            }
            KT_CUR => {
                if !up {
                    self.k_cur(v, modes, out);
                }
            }
            KT_SHIFT => self.k_shift(v, up, out),
            KT_META => {
                if !up {
                    if self.meta_esc {
                        out.put_all(&[0x1b, v]);
                    } else {
                        out.put(v | 0x80);
                    }
                }
            }
            KT_ASCII => {
                if !up {
                    // Decimal while Alt is held, hexadecimal (values 10 and up) with AltGr.
                    let (digit, base) = if v < 10 { (v as u32, 10) } else { (v as u32 - 10, 16) };
                    let n = self.npadch.unwrap_or(0);
                    self.npadch = Some(n.wrapping_mul(base).wrapping_add(digit));
                }
            }
            KT_LOCK => {
                if !up && !self.rep && v < 8 {
                    self.lockstate ^= 1 << v;
                }
            }
            KT_SLOCK => {
                self.k_shift(v, up, out);
                if !up && !self.rep && v < 8 {
                    self.slockstate ^= 1 << v;
                    // Sticky keys that lead to no keymap are no use.
                    if self.maps[(self.lockstate ^ self.slockstate) as usize].is_none() {
                        self.slockstate = 1 << v;
                    }
                }
            }
            KT_DEAD2 => self.k_deadunicode(v as u32, up, out),
            _ => {}
        }
        if t != KT_SLOCK {
            self.slockstate = 0;
        }
    }

    /// Puts a code point for the terminal: UTF-8 in Unicode mode, else its Latin-1 byte
    /// (none for others).
    fn put_char(&self, c: u32, out: &mut Output) {
        if self.mode == Mode::Unicode {
            to_utf8(c, out);
        } else if c < 0x100 {
            out.put(c as u8);
        }
    }

    /// A character typed (Linux's `k_unicode`): composed with a waiting dead key, kept
    /// as one after Compose.
    fn k_unicode(&mut self, mut c: u32, up: bool, out: &mut Output) {
        if up {
            return;
        }
        if self.diacr != 0 {
            c = self.handle_diacr(c, out);
        }
        if self.dead_key_next {
            self.dead_key_next = false;
            self.diacr = c;
            return;
        }
        self.put_char(c, out);
    }

    /// The waiting dead key and `c` (Linux's `handle_diacr`): their composition, or the
    /// dead key alone for a space or itself, or the dead key put and `c` returned.
    fn handle_diacr(&mut self, c: u32, out: &mut Output) -> u32 {
        let d = core::mem::take(&mut self.diacr);
        if let Some(a) = self.accents.iter().find(|a| a.diacr == d && a.base == c) {
            return a.result;
        }
        if c == b' ' as u32 || c == d {
            return d;
        }
        self.put_char(d, out);
        c
    }

    fn k_deadunicode(&mut self, c: u32, up: bool, out: &mut Output) {
        if up {
            return;
        }
        self.diacr = if self.diacr != 0 { self.handle_diacr(c, out) } else { c };
    }

    fn k_fn(&self, v: u8, out: &mut Output) {
        out.put_all(&self.funcs[v as usize]);
    }

    /// ESC O `key` (application mode) or ESC [ `key`.
    fn applkey(key: u8, app: bool, out: &mut Output) {
        out.put_all(&[0x1b, if app { b'O' } else { b'[' }, key]);
    }

    fn k_cur(&self, v: u8, modes: ConsoleModes, out: &mut Output) {
        if let Some(&c) = b"BDCA".get(v as usize) {
            Self::applkey(c, modes.ckmode, out);
        }
    }

    fn k_spec(&mut self, v: u8, up: bool, modes: ConsoleModes, out: &mut Output) {
        if up || v >= 20 {
            return;
        }
        // The raw modes still take the secure attention key.
        if matches!(self.mode, Mode::Raw | Mode::MediumRaw | Mode::Off) && v != kval(K_SAK) {
            return;
        }
        match v {
            1 => {
                // Enter: a waiting dead key goes first.
                if self.diacr != 0 {
                    let d = core::mem::take(&mut self.diacr);
                    self.put_char(d, out);
                }
                out.put(b'\r');
                if modes.crlf {
                    out.put(b'\n');
                }
            }
            2 => out.act(Action::ShowRegs),
            3 => out.act(Action::ShowMem),
            4 => out.act(Action::ShowState),
            5 => out.act(Action::Break),
            6 => out.act(Action::LastConsole),
            7 => {
                if !self.rep {
                    self.ledflagstate ^= LED_CAP;
                }
            }
            8 => {
                if modes.applic {
                    Self::applkey(b'P', true, out);
                } else if !self.rep {
                    self.ledflagstate ^= LED_NUM;
                }
            }
            9 => {
                if !self.rep {
                    out.act(Action::Hold);
                }
            }
            10 => out.act(Action::ScrollForward),
            11 => out.act(Action::ScrollBack),
            12 => out.act(Action::Boot),
            13 => {
                if !self.rep {
                    self.ledflagstate |= LED_CAP;
                }
            }
            14 => self.dead_key_next = true,
            15 => out.act(Action::Sak),
            16 => out.act(Action::DecConsole),
            17 => out.act(Action::IncConsole),
            18 => out.act(Action::Spawn),
            19 => {
                if !self.rep {
                    self.ledflagstate ^= LED_NUM;
                }
            }
            _ => {}
        }
    }

    fn k_pad(&mut self, v: u8, up: bool, modes: ConsoleModes, out: &mut Output) {
        const PAD_CHARS: &[u8] = b"0123456789+-*/\r,.?()#";
        const APP_MAP: &[u8] = b"pqrstuvwxylSRQMnnmPQS";
        if up {
            return;
        }
        // Shift forces the keypad's numbers in application mode.
        if modes.applic && self.shift_down[KG_SHIFT as usize] == 0 {
            if let Some(&c) = APP_MAP.get(v as usize) {
                Self::applkey(c, true, out);
            }
            return;
        }
        if self.ledflagstate & LED_NUM == 0 {
            let fnkey = |k: u16| Some(kval(k));
            let act = match v {
                K_PCOMMA | K_PDOT => fnkey(K_REMOVE),
                0 => fnkey(K_INSERT),
                1 => fnkey(K_SELECT),
                3 => fnkey(K_PGDN),
                7 => fnkey(K_FIND),
                9 => fnkey(K_PGUP),
                _ => None,
            };
            if let Some(f) = act {
                self.k_fn(f, out);
                return;
            }
            let cur = match v {
                2 => Some(K_DOWN),
                4 => Some(K_LEFT),
                6 => Some(K_RIGHT),
                8 => Some(K_UP),
                _ => None,
            };
            if let Some(c) = cur {
                self.k_cur(c, modes, out);
                return;
            }
            if v == 5 {
                Self::applkey(b'G', modes.applic, out);
                return;
            }
        }
        if let Some(&c) = PAD_CHARS.get(v as usize) {
            out.put(c);
        }
        if v == K_PENTER && modes.crlf {
            out.put(b'\n');
        }
    }

    fn k_shift(&mut self, v: u8, up: bool, out: &mut Output) {
        if self.rep {
            return;
        }
        let old = self.shift_state;
        let mut v = v;
        // A CapsShift key is a Shift that undoes Caps Lock.
        if v == KG_CAPSSHIFT {
            v = KG_SHIFT;
            if !up {
                self.ledflagstate &= !LED_CAP;
            }
        }
        if v as usize >= NR_SHIFT || v >= 8 {
            return;
        }
        let i = v as usize;
        if up {
            self.shift_down[i] = self.shift_down[i].saturating_sub(1);
        } else {
            self.shift_down[i] = self.shift_down[i].saturating_add(1);
        }
        if self.shift_down[i] != 0 {
            self.shift_state |= 1 << v;
        } else {
            self.shift_state &= !(1 << v);
        }
        // The modifier of an Alt-numpad entry let go: the code it made.
        if up && self.shift_state != old {
            if let Some(n) = self.npadch.take() {
                if self.mode == Mode::Unicode {
                    to_utf8(n, out);
                } else {
                    out.put(n as u8);
                }
            }
        }
    }
}
