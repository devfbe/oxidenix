//! The Linux console's virtual terminal (ADR 0020, console_ioctl(4)): the one VT there is,
//! tty1 (/dev/tty0 and /dev/console lead to it), its keyboard and its KD and VT requests.
//!
//! The keyboard is Linux's (crates/kbd): the kernel hands over the scancodes (`keyboard`,
//! on the service thread), the keyboard mode, keymaps, function strings, accent table,
//! scancode table, meta key and lock flags make the terminal's input of them, the LEDs
//! follow the flags (`SYS_CONSOLE_CTL`; Scroll Lock shows the console's output stopped),
//! and what a key asks beyond input is done here: Scroll Lock holds output, Break breaks,
//! Ctrl-Alt-Del restarts (`system::ctrl_alt_del`), the keyboard signal goes to the process
//! KDSIGACCEPT named. The console's modes the keyboard depends on (application keypad,
//! cursor keys, new line mode, autorepeat) are the kernel's VT100's, asked per scancode
//! batch.
//!
//! One VT: VT_ACTIVATE and VT_WAITACTIVE for tty1 are done at once, others are ENXIO;
//! VT_OPENQRY finds none free; VT_GETSTATE shows tty1; VT_SETMODE keeps the mode (no
//! switch ever asks a VT_PROCESS owner to release). KD_GRAPHICS stops the console's text
//! drawing (the kernel's); the console has no loadable font, so the font requests are
//! ENOSYS (EINVAL in KD_GRAPHICS), as Linux's for a console driver without font
//! operations.

use crate::files::{EINVAL, ENOTTY};
use crate::sync::Mutex;
use crate::syscall;
use crate::tty::{self, Tty, ENXIO};
use crate::usercopy;
use alloc::sync::Arc;
use kbd::{Action, ConsoleModes, Diacr, Keyboard, Mode};
use restricted::*;

const ENOSYS: i64 = 38;
const EBUSY: i64 = 16;
const EFAULT: i64 = 14;

/// Virtual terminals Linux numbers (MAX_NR_CONSOLES).
const MAX_NR_CONSOLES: u64 = 63;

// linux/kd.h
const KIOCSOUND: u32 = 0x4b2f;
const KDMKTONE: u32 = 0x4b30;
const KDGETLED: u32 = 0x4b31;
const KDSETLED: u32 = 0x4b32;
const KDGKBTYPE: u32 = 0x4b33;
const KDSETMODE: u32 = 0x4b3a;
const KDGETMODE: u32 = 0x4b3b;
const KDMAPDISP: u32 = 0x4b3c;
const KDUNMAPDISP: u32 = 0x4b3d;
const KDGKBMODE: u32 = 0x4b44;
const KDSKBMODE: u32 = 0x4b45;
const KDGKBENT: u32 = 0x4b46;
const KDSKBENT: u32 = 0x4b47;
const KDGKBSENT: u32 = 0x4b48;
const KDSKBSENT: u32 = 0x4b49;
const KDGKBDIACR: u32 = 0x4b4a;
const KDSKBDIACR: u32 = 0x4b4b;
const KDGETKEYCODE: u32 = 0x4b4c;
const KDSETKEYCODE: u32 = 0x4b4d;
const KDSIGACCEPT: u32 = 0x4b4e;
const KDKBDREP: u32 = 0x4b52;
const KDGKBMETA: u32 = 0x4b62;
const KDSKBMETA: u32 = 0x4b63;
const KDGKBLED: u32 = 0x4b64;
const KDSKBLED: u32 = 0x4b65;
const KDGKBDIACRUC: u32 = 0x4bfa;
const KDSKBDIACRUC: u32 = 0x4bfb;
const GIO_FONT: u32 = 0x4b60;
const PIO_FONT: u32 = 0x4b61;
const GIO_UNIMAP: u32 = 0x4b66;
const PIO_UNIMAP: u32 = 0x4b67;
const PIO_UNIMAPCLR: u32 = 0x4b68;
const GIO_FONTX: u32 = 0x4b6b;
const PIO_FONTX: u32 = 0x4b6c;
const PIO_FONTRESET: u32 = 0x4b6d;
const KDFONTOP: u32 = 0x4b72;
const KB_101: u8 = 0x02;
const KD_TEXT: u64 = 0;
const KD_GRAPHICS: u64 = 1;
const KD_TEXT0: u64 = 2;
const KD_TEXT1: u64 = 3;

// linux/vt.h
const VT_OPENQRY: u32 = 0x5600;
const VT_GETMODE: u32 = 0x5601;
const VT_SETMODE: u32 = 0x5602;
const VT_GETSTATE: u32 = 0x5603;
const VT_RELDISP: u32 = 0x5605;
const VT_ACTIVATE: u32 = 0x5606;
const VT_WAITACTIVE: u32 = 0x5607;
const VT_DISALLOCATE: u32 = 0x5608;
const VT_RESIZE: u32 = 0x5609;
const VT_RESIZEX: u32 = 0x560a;
const VT_LOCKSWITCH: u32 = 0x560b;
const VT_UNLOCKSWITCH: u32 = 0x560c;
const VT_GETHIFONTMASK: u32 = 0x560d;
const VT_WAITEVENT: u32 = 0x560e;
const VT_SETACTIVATE: u32 = 0x560f;
const VT_AUTO: u8 = 0;
const VT_PROCESS: u8 = 1;
const VT_ACKACQ: u64 = 2;

// TIOCLINUX and its subcodes (linux/tiocl.h).
const TIOCLINUX: u32 = 0x541c;
const TIOCL_UNBLANKSCREEN: u8 = 4;
const TIOCL_GETSHIFTSTATE: u8 = 6;
const TIOCL_GETMOUSEREPORTING: u8 = 7;
const TIOCL_SETVESABLANK: u8 = 10;
const TIOCL_SETKMSGREDIRECT: u8 = 11;
const TIOCL_GETFGCONSOLE: u8 = 12;
const TIOCL_SCROLLCONSOLE: u8 = 13;
const TIOCL_BLANKEDSCREEN: u8 = 15;
const TIOCL_GETKMSGREDIRECT: u8 = 17;

/// Signals up to SIGRTMAX (Linux's `valid_signal`).
const NSIG: u64 = 64;
const SIGKILL: u64 = 9;

struct Vt {
    /// The keyboard, made at first use (None again while there is no memory for it).
    kbd: Option<Keyboard>,
    /// KD_GRAPHICS.
    graphics: bool,
    /// `struct vt_mode` as VT_SETMODE set it (mode, waitv, relsig, acqsig; frsig 0).
    mode: [u8; 8],
    /// KDSIGACCEPT's process and signal for the keyboard signal.
    spawn: Option<(u32, u32)>,
    /// The LEDs last lit, and whether the console's output was stopped when last seen.
    leds: Option<u8>,
    stopped: bool,
    /// The autorepeat's delay and period (KDKBDREP; ms), as the keyboard runs them.
    repeat: (i32, i32),
    /// TIOCL_SETKMSGREDIRECT's console (0: the current one).
    kmsg_redirect: u8,
}

static VT: Mutex<Vt> = Mutex::new(Vt {
    kbd: None,
    graphics: false,
    mode: [0; 8],
    spawn: None,
    leds: None,
    stopped: false,
    repeat: (250, 33),
    kmsg_redirect: 0,
});

impl Vt {
    /// The keyboard (made now if it was not), or ENOMEM.
    fn kbd(&mut self) -> Result<&mut Keyboard, i64> {
        if self.kbd.is_none() {
            self.kbd = Some(Keyboard::new().map_err(err)?);
        }
        Ok(self.kbd.as_mut().expect("made above"))
    }

    /// Lights the LEDs the keyboard wants, if they changed.
    fn sync_leds(&mut self) {
        let Some(k) = &self.kbd else { return };
        let leds = k.leds();
        if self.leds != Some(leds) {
            self.leds = Some(leds);
            syscall(SYS_CONSOLE_CTL, [CONSOLE_LEDS, leds as u64, 0, 0, 0, 0]);
        }
    }
}

fn err(e: kbd::Error) -> i64 {
    match e {
        kbd::Error::Inval => EINVAL,
        kbd::Error::NoMem => crate::heap::ENOMEM,
    }
}

/// The console's modes the keyboard depends on (the kernel's VT100 keeps them).
fn console_modes() -> ConsoleModes {
    let m = syscall(SYS_CONSOLE_CTL, [CONSOLE_MODES, 0, 0, 0, 0, 0]);
    if m < 0 {
        return ConsoleModes::default();
    }
    let m = m as u64;
    ConsoleModes {
        applic: m & CONSOLE_MODE_APPLIC != 0,
        ckmode: m & CONSOLE_MODE_CKMODE != 0,
        crlf: m & CONSOLE_MODE_CRLF != 0,
        repeat: m & CONSOLE_MODE_REPEAT != 0,
    }
}

/// The keyboard's scancodes (service thread, `console::input`): each made into input
/// for the console's terminal and actions, the LEDs kept up. Without memory for the
/// keyboard the keys are lost.
pub fn keyboard(codes: &[u8], terminal: Option<&Arc<Tty>>) {
    let modes = console_modes();
    let Ok(mut out) = kbd::Output::try_new(kbd::OUTPUT_ROOM, 4) else { return };
    for &code in codes {
        out.clear();
        {
            let mut vt = VT.lock();
            let Ok(k) = vt.kbd() else { return };
            k.scancode(code, modes, &mut out);
            vt.sync_leds();
        }
        if let Some(t) = terminal {
            if !out.bytes.is_empty() {
                t.input(&out.bytes);
            }
        }
        for &a in &out.actions {
            act(a, terminal);
        }
    }
}

/// What a key asks beyond input (Linux's `fn_*` handlers): one VT, so console switches,
/// scrollback (the console keeps none) and the debugging dumps do nothing.
fn act(a: Action, terminal: Option<&Arc<Tty>>) {
    match a {
        Action::Hold => {
            if let Some(t) = terminal {
                t.hold();
            }
        }
        Action::Break => {
            if let Some(t) = terminal {
                t.input_break();
            }
        }
        Action::Boot => crate::system::ctrl_alt_del(),
        Action::Spawn => {
            let spawn = VT.lock().spawn;
            if let Some((pid, sig)) = spawn {
                if !crate::signal::send_process(pid, sig) {
                    // Gone: the request goes with it (Linux's `fn_spawn_con`).
                    VT.lock().spawn = None;
                }
            }
        }
        _ => {}
    }
}

/// The console's output stopped or started (from the terminal's change, its lock held):
/// the Scroll Lock flag and LED follow (Linux's `vt_kbd_con_stop`, `_start`).
pub fn output_stopped(stopped: bool) {
    let mut vt = VT.lock();
    if vt.stopped == stopped {
        return;
    }
    vt.stopped = stopped;
    if let Ok(k) = vt.kbd() {
        k.set_scroll_lock(stopped);
    }
    vt.sync_leds();
}

/// Whether tty1 is in use (open) — the console's terminal has an open description.
fn in_use(t: &Tty) -> bool {
    t.open_count() > 0
}

/// The KD, VT and TIOCLINUX requests on the console (after the terminal's own).
pub fn ioctl(t: &Tty, request: u32, arg: u64) -> Result<i64, i64> {
    match request {
        // No PC speaker: a tone is silence (Linux's `kd_mksound` without one).
        KIOCSOUND | KDMKTONE => Ok(0),
        KDGKBTYPE => put(arg, &KB_101),
        KDGETLED => {
            let mut vt = VT.lock();
            let leds = vt.kbd()?.leds();
            drop(vt);
            put(arg, &leds)
        }
        KDSETLED => {
            let mut vt = VT.lock();
            vt.kbd()?.set_leds(arg);
            vt.sync_leds();
            Ok(0)
        }
        KDGKBLED => {
            let flags = VT.lock().kbd()?.led_flags();
            put(arg, &flags)
        }
        KDSKBLED => {
            let mut vt = VT.lock();
            vt.kbd()?.set_led_flags(arg).map_err(err)?;
            vt.sync_leds();
            Ok(0)
        }
        KDGKBMODE => {
            let mode = VT.lock().kbd()?.mode() as i32;
            put(arg, &mode)
        }
        KDSKBMODE => {
            let mode = Mode::from_raw(arg).ok_or(EINVAL)?;
            VT.lock().kbd()?.set_mode(mode);
            // What was typed in the old mode goes (Linux's `tty_ldisc_flush`).
            t.flush(true, false);
            Ok(0)
        }
        KDGKBMETA => {
            let meta = VT.lock().kbd()?.meta() as i32;
            put(arg, &meta)
        }
        KDSKBMETA => VT.lock().kbd()?.set_meta(arg).map(|()| 0).map_err(err),
        KDGKBENT => {
            let e: [u8; 4] = usercopy::read(arg)?;
            let value = VT.lock().kbd()?.entry(e[0], e[1]);
            put(arg + 2, &value)
        }
        KDSKBENT => {
            let e: [u8; 4] = usercopy::read(arg)?;
            VT.lock().kbd()?.set_entry(e[0], e[1], u16::from_le_bytes([e[2], e[3]])).map_err(err)?;
            Ok(0)
        }
        KDGKBSENT => {
            let func: u8 = usercopy::read(arg)?;
            // The string and its NUL (at most 511 bytes: it fits `kb_string`).
            let mut s = [0u8; kbd::FUNC_MAX + 1];
            let n = {
                let mut vt = VT.lock();
                let f = vt.kbd()?.func(func);
                s[..f.len()].copy_from_slice(f);
                f.len()
            };
            usercopy::to_program(arg + 1, &s[..n + 1])?;
            Ok(0)
        }
        KDSKBSENT => {
            let func: u8 = usercopy::read(arg)?;
            // Up to its NUL, which must come within `kb_string` (Linux's `strndup_user`).
            let mut buf = [0u8; kbd::FUNC_MAX + 1];
            let s = read_string(arg + 1, &mut buf)?;
            VT.lock().kbd()?.set_func(func, s).map_err(err)?;
            Ok(0)
        }
        KDGKBDIACR | KDGKBDIACRUC => {
            let uc = request == KDGKBDIACRUC;
            let mut out = [0u8; 4 + 12 * kbd::MAX_DIACR];
            let len = {
                let mut vt = VT.lock();
                let accents = vt.kbd()?.accents();
                out[..4].copy_from_slice(&(accents.len() as u32).to_le_bytes());
                for (i, a) in accents.iter().enumerate() {
                    if uc {
                        let at = 4 + 12 * i;
                        out[at..at + 4].copy_from_slice(&a.diacr.to_le_bytes());
                        out[at + 4..at + 8].copy_from_slice(&a.base.to_le_bytes());
                        out[at + 8..at + 12].copy_from_slice(&a.result.to_le_bytes());
                    } else {
                        // Latin-1, or 0xff for a character beyond it (Linux's
                        // `conv_uni_to_8bit` failing).
                        let latin1 = |c: u32| if c < 0x100 { c as u8 } else { 0xff };
                        let at = 4 + 3 * i;
                        out[at..at + 3].copy_from_slice(&[latin1(a.diacr), latin1(a.base), latin1(a.result)]);
                    }
                }
                4 + accents.len() * if uc { 12 } else { 3 }
            };
            usercopy::to_program(arg, &out[..len])?;
            Ok(0)
        }
        KDSKBDIACR | KDSKBDIACRUC => {
            let uc = request == KDSKBDIACRUC;
            let count: u32 = usercopy::read(arg)?;
            if count as usize >= kbd::MAX_DIACR {
                return Err(EINVAL);
            }
            let size = if uc { 12 } else { 3 };
            let mut raw = [0u8; 12 * kbd::MAX_DIACR];
            let raw = &mut raw[..count as usize * size];
            usercopy::from_program(arg + 4, raw)?;
            let mut table = [Diacr { diacr: 0, base: 0, result: 0 }; kbd::MAX_DIACR];
            for (i, d) in table.iter_mut().take(count as usize).enumerate() {
                let e = &raw[i * size..(i + 1) * size];
                *d = if uc {
                    let at = |k: usize| u32::from_le_bytes([e[k], e[k + 1], e[k + 2], e[k + 3]]);
                    Diacr { diacr: at(0), base: at(4), result: at(8) }
                } else {
                    Diacr { diacr: e[0] as u32, base: e[1] as u32, result: e[2] as u32 }
                };
            }
            VT.lock().kbd()?.set_accents(&table[..count as usize]).map_err(err)?;
            Ok(0)
        }
        KDGETKEYCODE => {
            let e: [u32; 2] = usercopy::read(arg)?;
            let keycode = VT.lock().kbd()?.getkeycode(e[0]).map_err(err)? as u32;
            put(arg + 4, &keycode)
        }
        KDSETKEYCODE => {
            let e: [u32; 2] = usercopy::read(arg)?;
            VT.lock().kbd()?.setkeycode(e[0], e[1]).map_err(err)?;
            Ok(0)
        }
        KDSIGACCEPT => {
            if arg < 1 || arg > NSIG || arg == SIGKILL {
                return Err(EINVAL);
            }
            let me = tty::ids(0, 0)?;
            VT.lock().spawn = Some((me.pid as u32, arg as u32));
            Ok(0)
        }
        KDKBDREP => {
            let r: [i32; 2] = usercopy::read(arg)?;
            let now = {
                let mut vt = VT.lock();
                let (mut delay, mut period) = vt.repeat;
                if r[0] > 0 {
                    delay = r[0];
                }
                if r[1] > 0 {
                    period = r[1];
                }
                // The keyboard's own steps (Linux's `atkbd_set_repeat_rate`).
                const PERIODS: [i32; 32] = [
                    33, 37, 42, 46, 50, 54, 58, 63, 67, 75, 83, 92, 100, 109, 116, 125, 133, 149, 167, 182, 200, 217, 232,
                    250, 270, 303, 333, 370, 400, 435, 470, 500,
                ];
                const DELAYS: [i32; 4] = [250, 500, 750, 1000];
                let i = PERIODS.iter().position(|&p| p >= period).unwrap_or(PERIODS.len() - 1);
                let j = DELAYS.iter().position(|&d| d >= delay).unwrap_or(DELAYS.len() - 1);
                if (DELAYS[j], PERIODS[i]) != vt.repeat {
                    vt.repeat = (DELAYS[j], PERIODS[i]);
                    syscall(SYS_CONSOLE_CTL, [CONSOLE_TYPEMATIC, (i | j << 5) as u64, 0, 0, 0, 0]);
                }
                vt.repeat
            };
            usercopy::write(arg, &[now.0, now.1])?;
            Ok(0)
        }
        KDSETMODE => {
            let graphics = match arg {
                KD_TEXT | KD_TEXT0 | KD_TEXT1 => false,
                KD_GRAPHICS => true,
                _ => return Err(EINVAL),
            };
            let mut vt = VT.lock();
            if vt.graphics != graphics {
                vt.graphics = graphics;
                syscall(SYS_CONSOLE_CTL, [CONSOLE_GRAPHICS, graphics as u64, 0, 0, 0, 0]);
            }
            Ok(0)
        }
        KDGETMODE => {
            let mode = if VT.lock().graphics { KD_GRAPHICS } else { KD_TEXT } as i32;
            put(arg, &mode)
        }
        KDMAPDISP | KDUNMAPDISP => Err(EINVAL),
        // No loadable font (Linux's `con_font_op` for a driver without font operations).
        GIO_FONT | PIO_FONT | GIO_FONTX | PIO_FONTX | PIO_FONTRESET | KDFONTOP | GIO_UNIMAP | PIO_UNIMAP | PIO_UNIMAPCLR => {
            Err(if VT.lock().graphics { EINVAL } else { ENOSYS })
        }
        VT_OPENQRY => {
            // tty1 is the only one: free while nothing has it open.
            let free: i32 = if in_use(t) { -1 } else { 1 };
            put(arg, &free)
        }
        VT_GETSTATE => {
            // The active VT, no signal, and the VTs in use (bit 0: tty0, always).
            let state: u16 = 1 | if in_use(t) { 2 } else { 0 };
            usercopy::write(arg, &[1u16, 0, state])?;
            Ok(0)
        }
        VT_GETMODE => {
            let mode = VT.lock().mode;
            usercopy::to_program(arg, &mode)?;
            Ok(0)
        }
        VT_SETMODE => {
            let mut m = [0u8; 8];
            usercopy::from_program(arg, &mut m)?;
            set_mode(m)
        }
        VT_SETACTIVATE => {
            let mut b = [0u8; 12];
            usercopy::from_program(arg, &mut b)?;
            let console = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64;
            if console == 0 || console > MAX_NR_CONSOLES || console != 1 {
                return Err(ENXIO);
            }
            set_mode(b[4..12].try_into().expect("eight bytes"))
        }
        VT_RELDISP => {
            // No switch is ever pending: only an acquisition's acknowledgement.
            if VT.lock().mode[0] != VT_PROCESS || arg != VT_ACKACQ {
                return Err(EINVAL);
            }
            Ok(0)
        }
        VT_ACTIVATE => match arg {
            1 => Ok(0),
            _ => Err(ENXIO),
        },
        VT_WAITACTIVE => match arg {
            1 => Ok(0),
            0 => Err(ENXIO),
            n if n > MAX_NR_CONSOLES => Err(ENXIO),
            // A VT that never comes: until a signal.
            _ => t.sleep_until(0).map(|()| 0),
        },
        VT_WAITEVENT => {
            // No VT switches: nothing to wait for but a signal.
            let mut ev = [0u8; 28];
            usercopy::from_program(arg, &mut ev)?;
            t.sleep_until(0).map(|()| 0)
        }
        VT_DISALLOCATE => match arg {
            // tty1 is the foreground VT (Linux's `vt_busy`); the others were never made.
            1 => Err(EBUSY),
            n if n > MAX_NR_CONSOLES => Err(ENXIO),
            _ => Ok(0),
        },
        VT_RESIZE | VT_RESIZEX => {
            // The framebuffer's size is the console's: only it (or 0, unchanged) goes.
            let (cols, rows) = console_size();
            let s: [u16; 2] = usercopy::read(arg)?;
            let (r, c) = (s[0] as u64, s[1] as u64);
            if (r != 0 && r != rows) || (c != 0 && c != cols) {
                return Err(EINVAL);
            }
            Ok(0)
        }
        // There is no switch to lock out.
        VT_LOCKSWITCH | VT_UNLOCKSWITCH => Ok(0),
        VT_GETHIFONTMASK => put(arg, &0u16),
        TIOCLINUX => linux(arg),
        _ => Err(ENOTTY),
    }
}

/// VT_SETMODE (Linux's): VT_AUTO or VT_PROCESS with valid signals; frsig is not kept.
/// (No switch ever comes to signal its owner.)
fn set_mode(mut m: [u8; 8]) -> Result<i64, i64> {
    if m[0] != VT_AUTO && m[0] != VT_PROCESS {
        return Err(EINVAL);
    }
    let sig = |at: usize| i16::from_le_bytes([m[at], m[at + 1]]);
    if [sig(2), sig(4)].iter().any(|&s| s < 0 || s as u64 > NSIG) {
        return Err(EINVAL);
    }
    m[6] = 0;
    m[7] = 0;
    VT.lock().mode = m;
    Ok(0)
}

/// TIOCLINUX: its subcode is the byte at `arg`.
fn linux(arg: u64) -> Result<i64, i64> {
    let sub: u8 = usercopy::read(arg)?;
    match sub {
        // The console never blanks.
        TIOCL_UNBLANKSCREEN | TIOCL_SETVESABLANK => Ok(0),
        TIOCL_BLANKEDSCREEN => Ok(0),
        TIOCL_GETSHIFTSTATE => {
            let s = VT.lock().kbd()?.shift_state();
            put(arg, &s)
        }
        TIOCL_GETMOUSEREPORTING => put(arg, &0u8),
        TIOCL_SETKMSGREDIRECT => {
            let to: u8 = usercopy::read(arg + 1)?;
            VT.lock().kmsg_redirect = to;
            Ok(0)
        }
        TIOCL_GETKMSGREDIRECT => Ok(VT.lock().kmsg_redirect as i64),
        // tty1 (VT 0) is the foreground console.
        TIOCL_GETFGCONSOLE => Ok(0),
        // No scrollback to move through.
        TIOCL_SCROLLCONSOLE => {
            let _lines: i32 = usercopy::read(arg + 4)?;
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

fn console_size() -> (u64, u64) {
    let mut size = [0u64; 2];
    syscall(SYS_CONSOLE_INFO, [size.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
    (size[0], size[1])
}

fn put<T: Copy>(arg: u64, v: &T) -> Result<i64, i64> {
    usercopy::write(arg, v)?;
    Ok(0)
}

/// Reads a NUL-terminated string at `addr` into `buf`: the string without its NUL;
/// EINVAL if no NUL comes within `buf`.
fn read_string(addr: u64, buf: &mut [u8]) -> Result<&[u8], i64> {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = usercopy::read(addr + i as u64).map_err(|_| EFAULT)?;
        if *b == 0 {
            return Ok(&buf[..i]);
        }
    }
    Err(EINVAL)
}
