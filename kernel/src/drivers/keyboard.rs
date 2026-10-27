//! PS/2 keyboard (ADR 0020): the interrupt queues the controller's bytes (scancode set 1,
//! as the controller translates) for the console's holder, untranslated. The Linux
//! server's virtual terminal makes input of them by its keyboard mode and keymaps
//! (crates/kbd, Linux's keyboard.c); the kernel's monitor by the same library with the
//! default keymap (`monitor_byte`). The keyboard's LEDs and autorepeat are set by its
//! commands 0xED and 0xF3, their acknowledgements (0xFA) taken here in the interrupt.

use crate::sync::IrqSpinLock;
use heapless::Deque;
use x86_64::instructions::port::Port;

/// Scancodes not taken yet: what does not fit is dropped.
const RING: usize = 1024;
static SCANCODES: IrqSpinLock<Deque<u8, RING>> = IrqSpinLock::new(Deque::new());

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const CMD_SET_LEDS: u8 = 0xed;
const CMD_SET_TYPEMATIC: u8 = 0xf3;
const ACK: u8 = 0xfa;
const RESEND: u8 = 0xfe;

/// The keyboard's settings and the commands that make them: for the LEDs and the
/// autorepeat, the value wanted and the one last set; the byte awaiting the keyboard's
/// acknowledgement (its command, the byte, the time stamp counter when it was sent).
struct Commands {
    leds: (u8, Option<u8>),
    typematic: (u8, Option<u8>),
    awaiting: Option<(u8, u8, u64)>,
}

/// The typematic byte of Linux's default: a 250 ms delay, 33 ms between repeats.
const DEFAULT_TYPEMATIC: u8 = 0;

static COMMANDS: IrqSpinLock<Commands> =
    IrqSpinLock::new(Commands { leds: (0, None), typematic: (DEFAULT_TYPEMATIC, None), awaiting: None });
/// A keyboard that never answers (none there): give the command up after this many
/// cycles of the time stamp counter (a tenth of a second or so).
const ANSWER_CYCLES: u64 = 1 << 28;

fn tsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Writes a byte for the keyboard once the controller takes one (lock held).
fn send(b: u8) {
    unsafe {
        let mut status = Port::<u8>::new(STATUS);
        for _ in 0..100_000 {
            if status.read() & 2 == 0 {
                break;
            }
        }
        Port::<u8>::new(DATA).write(b);
    }
}

/// Sends the next command a setting needs, if none is in flight (or the one in flight
/// was never answered).
fn next_command(c: &mut Commands) {
    let stale = c.awaiting.is_some_and(|(_, _, at)| tsc().wrapping_sub(at) > ANSWER_CYCLES);
    if c.awaiting.is_some() && !stale {
        return;
    }
    c.awaiting = None;
    let cmd = if c.leds.1 != Some(c.leds.0) {
        CMD_SET_LEDS
    } else if c.typematic.1 != Some(c.typematic.0) {
        CMD_SET_TYPEMATIC
    } else {
        return;
    };
    send(cmd);
    c.awaiting = Some((cmd, cmd, tsc()));
}

/// Lights the LEDs (bit 0 Scroll Lock, 1 Num Lock, 2 Caps Lock, as KDSETLED's): sent
/// now, or after the command in flight.
pub fn set_leds(leds: u8) {
    let mut c = COMMANDS.lock();
    c.leds.0 = leds & 7;
    next_command(&mut c);
}

/// Sets the autorepeat (the typematic byte: bits 0-4 the period's step, 5-6 the delay's).
pub fn set_typematic(byte: u8) {
    let mut c = COMMANDS.lock();
    c.typematic.0 = byte & 0x7f;
    next_command(&mut c);
}

/// The LEDs and the typematic byte the keyboard should have (USB keyboards follow them:
/// the kernel's `kbd_state`).
pub fn wanted() -> (u8, u8) {
    let c = COMMANDS.lock();
    (c.leds.0, c.typematic.0)
}

/// The keyboard's answer to the command in flight, if `b` is one (interrupt): after the
/// command's acknowledgement its value goes, after the value's the setting is made.
fn command_answer(b: u8) -> bool {
    let mut c = COMMANDS.lock();
    let Some((cmd, sent, _)) = c.awaiting else { return false };
    match b {
        ACK if sent == cmd => {
            let value = if cmd == CMD_SET_LEDS { c.leds.0 } else { c.typematic.0 };
            send(value);
            c.awaiting = Some((cmd, value, tsc()));
        }
        ACK => {
            if cmd == CMD_SET_LEDS {
                c.leds.1 = Some(sent);
            } else {
                c.typematic.1 = Some(sent);
            }
            c.awaiting = None;
            next_command(&mut c);
        }
        RESEND => send(sent),
        _ => return false,
    }
    true
}

/// The keyboard interrupt's byte.
pub fn interrupt(b: u8) {
    if command_answer(b) {
        return;
    }
    queue(&[b]);
}

/// Queues scancodes for the console's holder (the interrupt's; the self-tests' as if
/// typed, `TEST_KBD_INPUT`).
pub fn queue(codes: &[u8]) {
    {
        let mut ring = SCANCODES.lock();
        for &c in codes {
            if ring.push_back(c).is_err() {
                break;
            }
        }
    }
    super::console_device::notify();
}

/// Takes up to `out.len()` scancodes.
pub fn read(out: &mut [u8]) -> usize {
    let mut ring = SCANCODES.lock();
    let mut n = 0;
    while n < out.len() {
        let Some(c) = ring.pop_front() else { break };
        out[n] = c;
        n += 1;
    }
    n
}

/// Whether scancodes wait.
pub fn pending() -> bool {
    !SCANCODES.lock().is_empty()
}

/// The monitor's keyboard (made at its first read) and the bytes its keys made that
/// were not read yet.
struct Monitor {
    kbd: Option<kbd::Keyboard>,
    out: Deque<u8, 64>,
}

static MONITOR: spin::Mutex<Monitor> = spin::Mutex::new(Monitor { kbd: None, out: Deque::new() });

/// The monitor's next byte typed, if there is one: the scancodes translated by the
/// default keymap (no actions: the monitor is not Linux).
pub fn monitor_byte() -> Option<u8> {
    let mut m = MONITOR.lock();
    if m.kbd.is_none() {
        m.kbd = kbd::Keyboard::new().ok();
    }
    loop {
        if let Some(b) = m.out.pop_front() {
            return Some(b);
        }
        let mut code = [0u8; 1];
        if read(&mut code) == 0 {
            return None;
        }
        let modes = super::console::kbd_modes();
        let (Ok(mut made), Monitor { kbd: Some(k), out }) = (kbd::Output::try_new(kbd::OUTPUT_ROOM, 0), &mut *m) else {
            // (Without memory for the keymaps or the output the keys are lost.)
            continue;
        };
        k.scancode(code[0], modes, &mut made);
        for b in made.bytes {
            let _ = out.push_back(b);
        }
        let leds = k.leds();
        drop(m);
        set_leds(leds);
        m = MONITOR.lock();
    }
}
