# ADR 0020: The Linux keyboard and the serial terminals in the server, scancodes and serial bytes from the kernel

Date: 2026-10-15. Status: accepted.

## Context

ADR 0007 left three terminal features open: packet mode, the Linux console's VT and
keyboard requests (console_ioctl(4): `KDGKBTYPE`, `KDGKBMODE`/`KDSKBMODE`, `KDGKBENT`,
`KDSETLED`, `VT_GETSTATE`, ...) and serial input. Packet mode is the pty's alone and needed
no decision. The other two did:

- **The keyboard was translated in the kernel.** The PS/2 interrupt decoded scancodes with
  the `pc-keyboard` crate's German layout into UTF-8 and the Linux console's key sequences,
  and the server got bytes. That made the keyboard requests impossible to answer: `K_RAW`
  and `K_MEDIUMRAW` need the scancodes and keycodes, `KDGKBENT`/`KDSKBENT` (dumpkmap,
  loadkmap, loadkeys) a keymap in Linux's format that is the one in use, `K_XLATE` Latin-1
  output, `KDGKBLED` the lock state, `KDSKBMETA` the meta key's behaviour. All of that is
  Linux's VT keyboard (drivers/tty/vt/keyboard.c), which ADR 0011 keeps out of the kernel.
- **The serial port was output only**, a mirror of the console for the kernel's messages and
  the test output. A terminal on it (`/dev/ttyS0`) needs its input, its settings applied to
  the UART and its modem lines; the test output must keep flowing.

## Decision

- **The kernel delivers scancodes, the server translates them.** The keyboard interrupt
  queues the controller's bytes (set 1) untranslated for the console's holder (`kbd_read`,
  `SYS_KBD_READ`, woken by `EVENT_CONSOLE` like the console's other input). Linux's keyboard
  is a library, `crates/kbd`, tested on the host: scancode to keycode (a table
  `KDGETKEYCODE`/`KDSETKEYCODE` change), the five keyboard modes, 256 keymaps of Linux
  keysyms (the default one German, as kbd's `de-latin1-nodeadkeys`), modifiers, locks and
  sticky keys, the keypad with Num Lock and application mode, dead keys and Compose with
  Linux's accent table, Alt-numpad entry, function strings, the meta key, and what keys
  ask beyond input as actions (hold, break, Ctrl-Alt-Del, the keyboard signal, console
  switches). The server's virtual terminal (`vt.rs`) owns one; the kernel's monitor uses the
  same library with the default keymap, so it needs no layout of its own and `pc-keyboard`
  goes. What the kernel keeps is hardware: the LEDs and the autorepeat (the keyboard's
  commands 0xED and 0xF3, acknowledged in the interrupt; `console_ctl`), and the console's
  VT100 modes the keyboard depends on (application keypad, cursor keys, LNM, DECARM, which
  the escape sequences written to the console set: `console_ctl(CONSOLE_MODES)`, asked per
  batch of scancodes).
- **One virtual terminal.** /dev/tty1 is the console's terminal, /dev/tty0 (the current VT)
  and /dev/console lead to it; tty2 and up are ENXIO. More VTs would need the kernel's
  console to keep a screen per VT and switch between them; nothing needs that yet. The VT
  requests answer as Linux's with one VT allocated and in use: `VT_OPENQRY` finds none free,
  `VT_ACTIVATE`/`VT_WAITACTIVE` for tty1 are done at once (others ENXIO, or a wait that
  only a signal ends), `VT_SETMODE` keeps its mode (no switch ever signals the owner).
  `KD_GRAPHICS` stops the kernel's console from drawing text (kept and redrawn on
  `KD_TEXT`; a change of the console's holder returns to text). The console has no loadable
  font: the font requests are ENOSYS (EINVAL in `KD_GRAPHICS`), as Linux's for a console
  driver without font operations.
- **The serial ports are raw devices granted with the console.** The kernel drives COM1-COM4
  (probed): receive interrupts (IRQ 4 and 3, no longer offered to user-space drivers), polled
  output in FIFO-sized pieces, line settings, breaks and modem lines (`serial_read`,
  `serial_write`, `serial_ctl`, SYS 1260-1262). The server's `/dev/ttyS0-3` are terminals of
  its own (`serial.rs`): the first open claims the port (`SERIAL_CLAIM`), the settings
  program the UART as Linux's 8250 driver does, the modem lines are `TIOCM*`'s.
- **COM1 stays the console's mirror.** The kernel's messages and the console's output go to
  it as before (the test output with them). Its input goes to /dev/ttyS0 while a terminal
  holds the port open, and otherwise into the console's input: with `-serial stdio` the
  serial line is a second keyboard of the console, and `setsid -c sh -l <>/dev/ttyS0` gives
  it a terminal of its own (the console's output is still mirrored there, as Linux's
  messages reach a serial console that a getty also uses).
- **Test hooks**: `TEST_KBD_INPUT` (1630) queues scancodes as if typed, so the keyboard path
  from the kernel's queue on is tested; the serial path is tested on QEMU's second port (COM2,
  nothing attached) in the UART's loopback mode (`TIOCM_LOOP`), through the real interrupt.
  The builder gives test mode that port and no standard input.

## Consequences

The kernel's keyboard driver is a byte queue and two commands; every keyboard request is
answered by the server from the state that is really used, and a keymap loaded with loadkmap
or `KDSKBENT` takes effect. A batch of keys costs one kernel call for the console's modes.
The monitor and the server translate independently (each its own lock state). Ctrl-Alt-Del
restarts the machine through the server's worker thread (reboot(2)'s path; CAD_OFF makes it
SIGINT to init). Serial output is polled, as the console mirror was; an interrupt-driven
transmitter would free the CPU during long writes at low speeds. Not done: more than one VT,
console fonts and the screen maps (`GIO_SCRNMAP`, `PIO_CMAP`), blanking, scrollback,
`TIOCMIWAIT`/`TIOCGICOUNT` and carrier detection (`CLOCAL` off waits for nothing), received
breaks and parity errors (a break arrives as a NUL), the secure attention key.
