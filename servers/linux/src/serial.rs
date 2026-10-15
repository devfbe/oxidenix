//! Serial terminals (ADR 0020): /dev/ttyS0-ttyS3 on the kernel's serial ports, which it
//! grants with the console (`console::held`). Each is a terminal of the server (`tty`,
//! `Driver::Serial`) made at its first open, with Linux's serial defaults (8N1, CREAD,
//! HUPCL, CLOCAL) at the 115200 baud the kernel runs the ports at; a port the machine
//! lacks opens EIO, as Linux's.
//!
//! The first open claims the port (`SERIAL_CLAIM`: its input comes to this terminal, by
//! the console's event, `input`) and raises DTR and RTS; the last close gives the input
//! back (COM1's to the console, which it mirrors) and with HUPCL drops them (not COM1's:
//! it stays the console's). The settings program the UART (speed, word length, stop bits,
//! parity: Linux's `serial8250_do_set_termios`, the speeds it runs at recorded; B0 drops
//! DTR and RTS); CREAD off drops the input. `TIOCMGET`/`TIOCMSET`/`TIOCMBIS`/`TIOCMBIC`
//! (DTR, RTS, OUT1, OUT2, loopback; CTS, DSR, CD, RI read from the port), `TIOCGSERIAL`,
//! `TIOCSERGETLSR` and breaks (`TCSBRK`, `TCSBRKP`, `TIOCSBRK`, `TIOCCBRK`) are the port's.

use crate::files::{EINVAL, ENOTTY};
use crate::namespace::Origin;
use crate::sync::Mutex;
use crate::syscall;
use crate::tty::{self, Driver, Tty, EIO, ENXIO};
use crate::usercopy;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};
use ldisc::*;
use restricted::*;

/// Ports at most (ttyS0-ttyS3, as Linux's 8250 driver makes by default).
pub const PORTS: usize = 4;

/// The kernel's clock for the UART: speeds are 115200 baud divided by a divisor.
const BAUD_BASE: u32 = 115200;
/// The ports' I/O bases and interrupt lines (as `TIOCGSERIAL` reports them).
const BASES: [u32; PORTS] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];
const IRQS: [i32; PORTS] = [4, 3, 4, 3];

// TIOCM_* (the modem lines as Linux reports them).
const TIOCM_DTR: u32 = 0x002;
const TIOCM_RTS: u32 = 0x004;
const TIOCM_CTS: u32 = 0x020;
const TIOCM_CAR: u32 = 0x040;
const TIOCM_RNG: u32 = 0x080;
const TIOCM_DSR: u32 = 0x100;
const TIOCM_OUT1: u32 = 0x2000;
const TIOCM_OUT2: u32 = 0x4000;
const TIOCM_LOOP: u32 = 0x8000;
/// The lines a program sets (Linux's `uart_tiocmset`).
const TIOCM_SETTABLE: u32 = TIOCM_DTR | TIOCM_RTS | TIOCM_OUT1 | TIOCM_OUT2 | TIOCM_LOOP;

const TCSBRK: u32 = 0x5409;
const TIOCSBRK: u32 = 0x5427;
const TCSBRKP: u32 = 0x5425;
const TIOCMGET: u32 = 0x5415;
const TIOCMBIS: u32 = 0x5416;
const TIOCMBIC: u32 = 0x5417;
const TIOCMSET: u32 = 0x5418;
const TIOCGSERIAL: u32 = 0x541e;
const TIOCSSERIAL: u32 = 0x541f;
const TIOCSERGETLSR: u32 = 0x5459;

/// The ports' terminals (made at first open) and their modem lines (`TIOCM_*` as set).
struct Port {
    tty: Option<Arc<Tty>>,
    mctrl: u32,
}

static PORT: Mutex<[Port; PORTS]> = Mutex::new([const { Port { tty: None, mctrl: 0 } }; PORTS]);
/// The ports the machine has, a bit each (0xff: not asked yet).
static PRESENT: AtomicU8 = AtomicU8::new(0xff);
/// The ports a terminal holds open (claimed), whose input `input` takes.
static OPEN: AtomicU8 = AtomicU8::new(0);

/// A serial port's settings at its first open: Linux's serial defaults (8N1, the
/// receiver on, a hangup on close, no modem control) at 115200 baud.
fn termios() -> Termios {
    let mut t = Termios::default();
    t.cflag = B115200 | CS8 | CREAD | HUPCL | CLOCAL;
    t.ispeed = BAUD_BASE;
    t.ospeed = BAUD_BASE;
    t
}

/// Whether the machine has port `n`: ENXIO while the instance does not hold the console
/// (no port is its), EIO for a port it lacks.
fn present(n: u8) -> Result<(), i64> {
    if !crate::console::held() {
        return Err(ENXIO);
    }
    let mut mask = PRESENT.load(Ordering::Acquire);
    if mask == 0xff {
        let r = syscall(SYS_SERIAL_CTL, [0, SERIAL_PORTS, 0, 0, 0, 0]);
        if r < 0 {
            return Err(ENXIO);
        }
        mask = r as u8 & 0x0f;
        PRESENT.store(mask, Ordering::Release);
    }
    if mask & 1 << n == 0 {
        return Err(EIO);
    }
    Ok(())
}

/// Port `n`'s terminal, made at first use.
fn terminal(n: u8) -> Result<Arc<Tty>, i64> {
    present(n)?;
    let mut ports = PORT.lock();
    if let Some(t) = &ports[n as usize].tty {
        return Ok(t.clone());
    }
    let t = Tty::new(Driver::Serial(n), termios(), [0; 4], None)?;
    ports[n as usize].tty = Some(t.clone());
    Ok(t)
}

/// Port `n`'s terminal if it was made.
pub fn current(n: usize) -> Option<Arc<Tty>> {
    PORT.lock()[n].tty.clone()
}

/// Opens /dev/ttyS<n> (it may become the caller's controlling terminal).
pub fn open(n: u8, flags: u32, origin: Origin) -> Result<i64, i64> {
    let t = terminal(n)?;
    tty::open(&t, flags, origin, true)
}

fn ctl(n: u8, op: u64, arg: u64) -> i64 {
    syscall(SYS_SERIAL_CTL, [n as u64, op, arg, 0, 0, 0])
}

/// The modem control bits for the kernel of `TIOCM_*` lines (OUT2 is the kernel's: on).
fn mcr_of(mctrl: u32) -> u64 {
    let mut mcr = 0;
    for (tiocm, bit) in [(TIOCM_DTR, 0x01), (TIOCM_RTS, 0x02), (TIOCM_OUT1, 0x04), (TIOCM_LOOP, 0x10)] {
        if mctrl & tiocm != 0 {
            mcr |= bit;
        }
    }
    mcr
}

/// Sets port `n`'s modem lines (`PORT` held by the caller's guard).
fn set_mctrl(ports: &mut [Port; PORTS], n: u8, mctrl: u32) {
    ports[n as usize].mctrl = mctrl;
    ctl(n, SERIAL_MODEM, mcr_of(mctrl));
}

/// The first open (terminal's lock held): the port's input is the terminal's, its line
/// as the settings say, DTR and RTS up unless the speed is B0 (Linux's `uart_startup`).
pub fn first_open(n: u8, t: &Termios) {
    OPEN.fetch_or(1 << n, Ordering::AcqRel);
    ctl(n, SERIAL_CLAIM, 1);
    let mut cur = *t;
    program(n, None, &mut cur);
    let mut ports = PORT.lock();
    let mut mctrl = TIOCM_OUT2 | (ports[n as usize].mctrl & (TIOCM_OUT1 | TIOCM_LOOP));
    if t.baud_rate() != 0 {
        mctrl |= TIOCM_DTR | TIOCM_RTS;
    }
    set_mctrl(&mut ports, n, mctrl);
}

/// The last close (terminal's lock held): the input goes back to the kernel; with HUPCL
/// DTR and RTS drop (not COM1's: the console's line stays up), loopback ends.
pub fn last_close(n: u8, t: &Termios) {
    OPEN.fetch_and(!(1 << n), Ordering::AcqRel);
    ctl(n, SERIAL_CLAIM, 0);
    let mut ports = PORT.lock();
    let mut mctrl = ports[n as usize].mctrl & !TIOCM_LOOP;
    if t.cflag & HUPCL != 0 && n != 0 {
        mctrl &= !(TIOCM_DTR | TIOCM_RTS);
    }
    set_mctrl(&mut ports, n, mctrl);
}

/// Programs the UART for `new` (Linux's `uart_get_baud_rate` and
/// `serial8250_do_set_termios`): a speed beyond the port's goes back to the old one (or
/// the port's top speed), B0 runs the line at 9600 baud; the speed recorded is the one
/// asked for (a BOTHER speed between divisors stays as asked).
fn program(n: u8, old: Option<&Termios>, new: &mut Termios) {
    let mut baud = new.baud_rate();
    let hung_up = baud == 0;
    if hung_up {
        baud = 9600;
    }
    if baud > BAUD_BASE {
        baud = old.map(|o| o.baud_rate()).filter(|&b| b != 0 && b <= BAUD_BASE).unwrap_or(BAUD_BASE);
    }
    if !hung_up {
        new.encode_baud_rate(baud, baud);
    }
    let divisor = ((BAUD_BASE + baud / 2) / baud).clamp(1, 0xffff);
    let mut format = match new.cflag & CSIZE {
        CS5 => 0,
        CS6 => 1,
        CS7 => 2,
        _ => 3,
    };
    if new.cflag & CSTOPB != 0 {
        format |= 0x04;
    }
    if new.cflag & PARENB != 0 {
        format |= 0x08;
        if new.cflag & PARODD == 0 {
            format |= 0x10;
        }
        if new.cflag & CMSPAR != 0 {
            format |= 0x20;
        }
    }
    ctl(n, SERIAL_LINE, divisor as u64 | (format as u64) << 16);
}

/// New settings (terminal's lock held): the UART follows; a change to B0 drops DTR and
/// RTS, one from B0 raises them (Linux's `uart_set_termios`).
pub fn set_termios(n: u8, old: &Termios, new: &mut Termios) {
    program(n, Some(old), new);
    let (was, is) = (old.cflag & CBAUD != B0, new.cflag & CBAUD != B0);
    if was != is {
        let mut ports = PORT.lock();
        let lines = TIOCM_DTR | TIOCM_RTS;
        let mctrl = ports[n as usize].mctrl;
        set_mctrl(&mut ports, n, if is { mctrl | lines } else { mctrl & !lines });
    }
}

/// Writes processed output to port `n` (a call per `SERIAL_WRITE_MAX` bytes): Ok, or the
/// bytes that went out before an error (EIO when the instance lost the ports).
pub fn device_write(n: u8, bytes: &[u8]) -> Result<(), (usize, i64)> {
    let mut went = 0;
    for piece in bytes.chunks(SERIAL_WRITE_MAX as usize) {
        let r = syscall(SYS_SERIAL_WRITE, [n as u64, piece.as_ptr() as u64, piece.len() as u64, 0, 0, 0]);
        if r < 0 {
            return Err((went, -r));
        }
        went += piece.len();
    }
    Ok(())
}

/// Echoes to port `n` (the service thread's: a polled write of a few bytes, which never
/// waits for anything but the UART).
pub fn device_echo(n: u8, bytes: &[u8]) {
    let _ = device_write(n, bytes);
}

/// `EVENT_CONSOLE`: the open ports' input into their terminals (service thread); with
/// CREAD off it is dropped (Linux's `ignore_status_mask`).
pub fn input() {
    let open = OPEN.load(Ordering::Acquire);
    let mut buf = [0u8; 256];
    for n in (0..PORTS as u8).filter(|n| open & 1 << n != 0) {
        let Some(t) = current(n as usize) else { continue };
        loop {
            let r = syscall(SYS_SERIAL_READ, [n as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]);
            if r <= 0 {
                break;
            }
            let receiving = t.inner.lock().ld.termios().cflag & CREAD != 0;
            if receiving {
                t.input(&buf[..r as usize]);
            }
        }
    }
}

/// The instance lost the console and with it the ports: their terminals hang up.
pub fn lost() {
    for n in 0..PORTS {
        if let Some(t) = current(n) {
            t.hangup(false);
        }
    }
}

/// A break (`TCSBRK` with 0: a quarter second; `TCSBRKP`: tenths of a second, 0 a
/// quarter; `TIOCSBRK`/`TIOCCBRK`: on and off). A timed one waits for the write in
/// progress and keeps others out meanwhile (Linux's `tty_send_break`); a signal ends it
/// early (EINTR). `TCSBRK` with an argument is tcdrain: nothing waits to go out.
pub fn send_break(t: &Tty, n: u8, request: u32, arg: u64) -> Result<i64, i64> {
    let ms = match request {
        TIOCSBRK => {
            ctl(n, SERIAL_BREAK, 1);
            return Ok(0);
        }
        TCSBRK if arg != 0 => return Ok(0),
        TCSBRK => 250,
        TCSBRKP if arg == 0 => 250,
        TCSBRKP => arg.min(u32::MAX as u64) * 100,
        // TIOCCBRK
        _ => {
            ctl(n, SERIAL_BREAK, 0);
            return Ok(0);
        }
    };
    let _turn = t.turn(tty::TURN_WRITE, false)?;
    ctl(n, SERIAL_BREAK, 1);
    let r = t.sleep_until(tty::now() + ms * 1_000_000);
    ctl(n, SERIAL_BREAK, 0);
    r.map(|()| 0)
}

/// `struct serial_struct` (TIOCGSERIAL), 72 bytes.
fn serial_struct(n: u8) -> [u8; 72] {
    const PORT_16550A: i32 = 4;
    const UPF_BOOT_AUTOCONF: i32 = 1 << 28;
    let mut s = [0u8; 72];
    let mut put = |at: usize, v: &[u8]| s[at..at + v.len()].copy_from_slice(v);
    put(0, &PORT_16550A.to_le_bytes());
    put(4, &(n as i32).to_le_bytes());
    put(8, &BASES[n as usize].to_le_bytes());
    put(12, &IRQS[n as usize].to_le_bytes());
    put(16, &UPF_BOOT_AUTOCONF.to_le_bytes());
    put(20, &16i32.to_le_bytes()); // xmit_fifo_size
    put(28, &(BAUD_BASE as i32).to_le_bytes());
    put(32, &50u16.to_le_bytes()); // close_delay: half a second
    put(40, &3000u16.to_le_bytes()); // closing_wait: 30 seconds
    s
}

/// The port's own requests (after the terminal's).
pub fn ioctl(_t: &Tty, n: u8, request: u32, arg: u64) -> Result<i64, i64> {
    match request {
        TIOCMGET => {
            let status = ctl(n, SERIAL_STATUS, 0);
            if status < 0 {
                return Err(-status);
            }
            let msr = (status >> 8) as u8;
            let mut lines = PORT.lock()[n as usize].mctrl;
            for (bit, tiocm) in [(0x10, TIOCM_CTS), (0x20, TIOCM_DSR), (0x40, TIOCM_RNG), (0x80, TIOCM_CAR)] {
                if msr & bit != 0 {
                    lines |= tiocm;
                }
            }
            usercopy::write(arg, &(lines as i32))?;
            Ok(0)
        }
        TIOCMSET | TIOCMBIS | TIOCMBIC => {
            let v = usercopy::read::<i32>(arg)? as u32 & TIOCM_SETTABLE;
            let mut ports = PORT.lock();
            let cur = ports[n as usize].mctrl;
            let new = match request {
                TIOCMSET => v,
                TIOCMBIS => cur | v,
                _ => cur & !v,
            };
            set_mctrl(&mut ports, n, new);
            Ok(0)
        }
        TIOCGSERIAL => {
            usercopy::to_program(arg, &serial_struct(n))?;
            Ok(0)
        }
        TIOCSSERIAL => {
            // The ports are the machine's: what describes them (type, line, port,
            // interrupt, FIFO, clock) cannot change; the rest is not kept.
            let mut s = [0u8; 72];
            usercopy::from_program(arg, &mut s)?;
            let mine = serial_struct(n);
            if s[0..16] != mine[0..16] || s[20..24] != mine[20..24] || s[28..32] != mine[28..32] {
                return Err(EINVAL);
            }
            Ok(0)
        }
        TIOCSERGETLSR => {
            // TIOCSER_TEMT: the transmitter is empty.
            let status = ctl(n, SERIAL_STATUS, 0);
            if status < 0 {
                return Err(-status);
            }
            let temt = ((status >> 16) as u8 & 0x40 != 0) as i32;
            usercopy::write(arg, &temt)?;
            Ok(0)
        }
        _ => Err(ENOTTY),
    }
}
