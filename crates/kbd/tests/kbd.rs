//! The console keyboard against Linux's keyboard.c: scancodes to keycodes, the modes
//! (raw, medium raw, Latin-1, Unicode, off), modifiers and locks, the keypad, function
//! and cursor keys, dead keys, Alt-numpad entry, the meta key, and the keymap, function
//! string, accent table and LED requests.

use kbd::keysym::k as sym;
use kbd::keysym::*;
use kbd::*;

const M: ConsoleModes = ConsoleModes { applic: false, ckmode: false, crlf: false, repeat: true };

fn kb() -> Keyboard {
    Keyboard::new().unwrap()
}

/// Feeds scancodes; the bytes and actions they made.
fn scan(k: &mut Keyboard, codes: &[u8]) -> Output {
    let mut out = Output::try_new(4096, 16).unwrap();
    for &c in codes {
        k.scancode(c, M, &mut out);
    }
    out
}

fn scan_with(k: &mut Keyboard, modes: ConsoleModes, codes: &[u8]) -> Vec<u8> {
    let mut out = Output::try_new(4096, 16).unwrap();
    for &c in codes {
        k.scancode(c, modes, &mut out);
    }
    out.bytes
}

/// A press and a release of a key without a prefix.
fn tap(sc: u8) -> [u8; 2] {
    [sc, sc | 0x80]
}

#[test]
fn letters_digits_and_the_german_layout() {
    let mut k = kb();
    // a, z (the German Z sits where the US Y is), y, 1, ß, ä in UTF-8.
    let mut codes = Vec::new();
    for sc in [0x1e, 0x15, 0x2c, 0x02, 0x0c, 0x28] {
        codes.extend_from_slice(&tap(sc));
    }
    assert_eq!(scan(&mut k, &codes).bytes, "azy1ßä".as_bytes());
    // Shift: capitals and the shifted symbols.
    let out = scan(&mut k, &[0x2a, 0x1e, 0x9e, 0x03, 0x83, 0x28, 0xa8, 0xaa]);
    assert_eq!(out.bytes, "A\"Ä".as_bytes());
    // AltGr (E0 38): @, €, braces.
    let out = scan(&mut k, &[0xe0, 0x38, 0x10, 0x90, 0x12, 0x92, 0x08, 0x88, 0xe0, 0xb8]);
    assert_eq!(out.bytes, "@€{".as_bytes());
    // Enter is CR, Backspace DEL, Tab, Escape.
    let mut codes = Vec::new();
    for sc in [0x1c, 0x0e, 0x0f, 0x01] {
        codes.extend_from_slice(&tap(sc));
    }
    assert_eq!(scan(&mut k, &codes).bytes, b"\r\x7f\t\x1b");
}

#[test]
fn control_and_alt() {
    let mut k = kb();
    // Ctrl+C, Ctrl+Space, Alt+b (ESC prefix by default), Ctrl+Alt+Del: Boot.
    let out = scan(&mut k, &[0x1d, 0x2e, 0xae, 0x39, 0xb9, 0x9d]);
    assert_eq!(out.bytes, b"\x03\x00");
    let out = scan(&mut k, &[0x38, 0x30, 0xb0, 0xb8]);
    assert_eq!(out.bytes, b"\x1bb");
    assert_eq!(k.meta(), K_ESCPREFIX);
    k.set_meta(K_METABIT as u64).unwrap();
    let out = scan(&mut k, &[0x38, 0x30, 0xb0, 0xb8]);
    assert_eq!(out.bytes, [b'b' | 0x80]);
    assert_eq!(k.set_meta(7), Err(Error::Inval));
    let out = scan(&mut k, &[0x1d, 0x38, 0xe0, 0x53, 0xe0, 0xd3, 0xb8, 0x9d]);
    assert_eq!(out.actions, [Action::Boot]);
    // Alt+F2: console 2; Alt+Left: the previous one.
    let out = scan(&mut k, &[0x38, 0x3c, 0xbc, 0xe0, 0x4b, 0xe0, 0xcb, 0xb8]);
    assert_eq!(out.actions, [Action::Console(1), Action::DecConsole]);
}

#[test]
fn cursor_function_and_editing_keys() {
    let mut k = kb();
    let out = scan(&mut k, &[0xe0, 0x48, 0xe0, 0xc8, 0x3b, 0xbb, 0x40, 0xc0, 0xe0, 0x47, 0xe0, 0xc7, 0xe0, 0x53, 0xe0, 0xd3]);
    assert_eq!(out.bytes, b"\x1b[A\x1b[[A\x1b[17~\x1b[1~\x1b[3~");
    // DECCKM: the cursor keys as ESC O.
    let app = ConsoleModes { ckmode: true, ..M };
    assert_eq!(scan_with(&mut k, app, &[0xe0, 0x4b, 0xe0, 0xcb]), b"\x1bOD");
    // Shift+F1 is F11; LNM: Enter sends CR LF.
    assert_eq!(scan(&mut k, &[0x2a, 0x3b, 0xbb, 0xaa]).bytes, b"\x1b[23~");
    let lnm = ConsoleModes { crlf: true, ..M };
    assert_eq!(scan_with(&mut k, lnm, &tap(0x1c)), b"\r\n");
}

#[test]
fn caps_and_num_lock_and_the_keypad() {
    let mut k = kb();
    // Caps Lock: letters (umlauts too) in capitals, digits not; Shift undoes it.
    let out = scan(&mut k, &[0x3a, 0xba, 0x1e, 0x9e, 0x28, 0xa8, 0x02, 0x82, 0x2a, 0x1e, 0x9e, 0xaa]);
    assert_eq!(out.bytes, "AÄ1a".as_bytes());
    assert_eq!(k.leds(), LED_CAP);
    assert_eq!(k.led_flags(), LED_CAP);
    scan(&mut k, &tap(0x3a));
    assert_eq!(k.leds(), 0);
    // Keypad without Num Lock: cursor and editing keys; with it the digits.
    let out = scan(&mut k, &[0x48, 0xc8, 0x47, 0xc7, 0x4c, 0xcc, 0x53, 0xd3]);
    assert_eq!(out.bytes, b"\x1b[A\x1b[1~\x1b[G\x1b[3~");
    scan(&mut k, &tap(0x45));
    assert_eq!(k.leds(), LED_NUM);
    let out = scan(&mut k, &[0x48, 0xc8, 0x53, 0xd3, 0x4e, 0xce, 0xe0, 0x1c, 0xe0, 0x9c, 0xe0, 0x35, 0xe0, 0xb5]);
    assert_eq!(out.bytes, b"8,+\r/");
    // The application keypad: ESC O x whatever Num Lock says.
    let app = ConsoleModes { applic: true, ..M };
    assert_eq!(scan_with(&mut k, app, &[0x48, 0xc8, 0x45, 0xc5]), b"\x1bOx\x1bOP");
}

#[test]
fn alt_numpad_entry() {
    let mut k = kb();
    scan(&mut k, &tap(0x45));
    // Alt held, 2 3 9 on the keypad, Alt let go: U+00EF in UTF-8.
    let out = scan(&mut k, &[0x38, 0x50, 0xd0, 0x51, 0xd1, 0x49, 0xc9, 0xb8]);
    assert_eq!(out.bytes, "ï".as_bytes());
    k.set_mode(Mode::Xlate);
    let out = scan(&mut k, &[0x38, 0x50, 0xd0, 0x51, 0xd1, 0x49, 0xc9, 0xb8]);
    assert_eq!(out.bytes, [0xef]);
}

#[test]
fn dead_keys_and_compose() {
    let mut k = kb();
    // A dead circumflex on the plain map (keycode 41), then e: ê; then space: the accent.
    k.set_entry(0, 41, sym(KT_DEAD, 2)).unwrap();
    let out = scan(&mut k, &[0x29, 0xa9, 0x12, 0x92, 0x29, 0xa9, 0x39, 0xb9]);
    assert_eq!(out.bytes, "ê^".as_bytes());
    // No composition: the accent, then the character.
    let out = scan(&mut k, &[0x29, 0xa9, 0x2d, 0xad]);
    assert_eq!(out.bytes, b"^x");
    // Compose (keycode 127, E0 5D), then " and a: ä (the accent table).
    let out = scan(&mut k, &[0xe0, 0x5d, 0xe0, 0xdd, 0x2a, 0x03, 0x83, 0xaa, 0x1e, 0x9e]);
    assert_eq!(out.bytes, "ä".as_bytes());
    // KDSKBDIACRUC: a table of our own.
    k.set_accents(&[Diacr { diacr: '^' as u32, base: 'x' as u32, result: 0x2716 }]).unwrap();
    let out = scan(&mut k, &[0x29, 0xa9, 0x2d, 0xad]);
    assert_eq!(out.bytes, "✖".as_bytes());
    let big: Vec<Diacr> = (0..MAX_DIACR as u32).map(|i| Diacr { diacr: i, base: i, result: i }).collect();
    assert_eq!(k.set_accents(&big), Err(Error::Inval));
}

#[test]
fn raw_mediumraw_and_off() {
    let mut k = kb();
    k.set_mode(Mode::Raw);
    let codes = [0x1e, 0x9e, 0xe0, 0x48, 0xe0, 0xc8];
    assert_eq!(scan(&mut k, &codes).bytes, codes);
    k.set_mode(Mode::MediumRaw);
    // a (30), Up (103), Sleep (142: three bytes).
    let out = scan(&mut k, &[0x1e, 0x9e, 0xe0, 0x48, 0xe0, 0xc8, 0xe0, 0x5f, 0xe0, 0xdf]);
    assert_eq!(out.bytes, [30, 30 | 0x80, 103, 103 | 0x80, 0, 0x81, 0x8e, 0x80, 0x81, 0x8e]);
    // Pause: E1 1D 45 E1 9D C5, keycode 119.
    let out = scan(&mut k, &[0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]);
    assert_eq!(out.bytes, [119, 119 | 0x80]);
    // Shifts are tracked in the raw modes: back to Unicode with Shift held.
    scan(&mut k, &[0x2a]);
    k.set_mode(Mode::Unicode);
    assert_eq!(scan(&mut k, &tap(0x1e)).bytes, b"A");
    scan(&mut k, &[0xaa]);
    k.set_mode(Mode::Off);
    assert!(scan(&mut k, &tap(0x1e)).bytes.is_empty());
    // Latin-1 mode: ß as one byte, € not at all.
    k.set_mode(Mode::Xlate);
    assert_eq!(scan(&mut k, &[0x0c, 0x8c, 0xe0, 0x38, 0x12, 0x92, 0xe0, 0xb8]).bytes, [0xdf]);
}

#[test]
fn autorepeat() {
    let mut k = kb();
    // A key pressed again while held repeats; with DECARM off it does not.
    assert_eq!(scan(&mut k, &[0x1e, 0x1e, 0x1e, 0x9e]).bytes, b"aaa");
    let norepeat = ConsoleModes { repeat: false, ..M };
    assert_eq!(scan_with(&mut k, norepeat, &[0x1e, 0x1e, 0x1e, 0x9e]), b"a");
    // Caps Lock held does not toggle twice.
    scan(&mut k, &[0x3a, 0x3a, 0x3a, 0xba]);
    assert_eq!(k.leds(), LED_CAP);
}

#[test]
fn keymap_entries() {
    let mut k = kb();
    // KDGKBENT: user space's view, K(t, v).
    assert_eq!(k.entry(0, 30), sym(KT_LETTER, b'a'));
    assert_eq!(k.entry(0, 28), K_ENTER);
    assert_eq!(k.entry(0, 42), sym(KT_SHIFT, KG_SHIFT));
    assert_eq!(k.entry(0, 0), K_HOLE);
    assert_eq!(k.entry(3, 0), K_NOSUCHMAP);
    assert_eq!(k.entry(3, 30), K_HOLE);
    // A Unicode keysym: U(0x20ac) in Unicode mode, a hole otherwise.
    assert_eq!(k.entry(2, 18), 0x20ac ^ 0xf000);
    k.set_mode(Mode::Xlate);
    assert_eq!(k.entry(2, 18), K_HOLE);
    assert_eq!(k.set_entry(0, 30, 0x20ac ^ 0xf000), Err(Error::Inval));
    k.set_mode(Mode::Unicode);
    // KDSKBENT: values within their type's, a new keymap of holes, removal.
    assert_eq!(k.set_entry(0, 30, sym(KT_SPEC, 20)), Err(Error::Inval));
    assert_eq!(k.set_entry(0, 30, sym(NR_TYPES - 1, 11)), Err(Error::Inval));
    k.set_entry(0, 30, sym(KT_LATIN, b'q')).unwrap();
    assert_eq!(scan(&mut k, &tap(0x1e)).bytes, b"q");
    k.set_entry(3, 30, sym(KT_LATIN, b'x')).unwrap();
    assert_eq!(k.entry(3, 0), K_ALLOCATED);
    assert_eq!(k.entry(3, 31), K_HOLE);
    // Shift+AltGr (keymap 3) now has a map (with its modifiers, or they never go up).
    k.set_entry(3, 42, sym(KT_SHIFT, KG_SHIFT)).unwrap();
    k.set_entry(3, 100, sym(KT_SHIFT, KG_ALTGR)).unwrap();
    let out = scan(&mut k, &[0x2a, 0xe0, 0x38, 0x1e, 0x9e, 0xe0, 0xb8, 0xaa]);
    assert_eq!(out.bytes, b"x");
    k.set_entry(3, 0, K_NOSUCHMAP).unwrap();
    assert_eq!(k.entry(3, 0), K_NOSUCHMAP);
    // The plain keymap stays.
    k.set_entry(0, 0, K_NOSUCHMAP).unwrap();
    assert_eq!(k.entry(0, 30), sym(KT_LATIN, b'q'));
    // A Unicode keysym typed.
    k.set_entry(0, 31, 0x263a ^ 0xf000).unwrap();
    assert_eq!(scan(&mut k, &tap(0x1f)).bytes, "☺".as_bytes());
    // Making a key a shift recomputes the shift state from the keys held.
    scan(&mut k, &[0x20]);
    k.set_entry(0, 32, sym(KT_SHIFT, KG_SHIFT)).unwrap();
    assert_eq!(k.shift_state(), 1);
}

#[test]
fn scancode_keycode_table_and_bounded_output() {
    let mut k = kb();
    // KDGETKEYCODE: set 1's codes, 128 up after E0.
    assert_eq!(k.getkeycode(0x1e), Ok(30));
    assert_eq!(k.getkeycode(0x80 | 0x48), Ok(103));
    assert_eq!(k.getkeycode(256), Err(Error::Inval));
    // KDSETKEYCODE: the A key (0x1e) made B (48).
    k.setkeycode(0x1e, 48).unwrap();
    assert_eq!(scan(&mut k, &tap(0x1e)).bytes, b"b");
    assert_eq!(k.setkeycode(0x1e, KEY_MAX as u32 + 1), Err(Error::Inval));
    // A keycode beyond the keymaps makes nothing, but medium raw reports it.
    k.setkeycode(0x1e, 0x1d0).unwrap();
    assert!(scan(&mut k, &tap(0x1e)).bytes.is_empty());
    k.set_mode(Mode::MediumRaw);
    assert_eq!(scan(&mut k, &[0x1e]).bytes, [0, 0x83, 0xd0]);
    // Output takes what its room holds.
    k.set_mode(Mode::Unicode);
    let mut out = Output::try_new(3, 0).unwrap();
    k.set_func(0, b"hello").unwrap();
    k.scancode(0x3b, M, &mut out);
    assert_eq!(out.bytes, b"hel");
    assert!(out.actions.is_empty());
    out.clear();
    k.scancode(0xbb, M, &mut out);
    assert!(out.bytes.is_empty());
}

#[test]
fn function_strings_and_leds() {
    let mut k = kb();
    assert_eq!(k.func(0), b"\x1b[[A");
    assert_eq!(k.func(29), b"\x1b[P");
    assert_eq!(k.func(27), b"");
    k.set_func(0, b"hello").unwrap();
    assert_eq!(scan(&mut k, &tap(0x3b)).bytes, b"hello");
    assert_eq!(k.set_func(1, &[b'x'; FUNC_MAX + 1]), Err(Error::Inval));
    // KDSKBLED: flags and defaults; KDSETLED overrides the LEDs until a value beyond them.
    assert_eq!(k.set_led_flags(0x80), Err(Error::Inval));
    k.set_led_flags(0x23).unwrap();
    assert_eq!(k.led_flags(), 0x23);
    assert_eq!(k.leds(), 3);
    k.set_leds(4);
    assert_eq!(k.leds(), 4);
    k.set_leds(0xff);
    assert_eq!(k.leds(), 3);
    k.set_scroll_lock(false);
    assert_eq!(k.leds(), 2);
    // Scroll Lock asks for a hold.
    assert_eq!(scan(&mut k, &tap(0x46)).actions, [Action::Hold]);
}
