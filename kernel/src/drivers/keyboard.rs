//! PS/2 keyboard: decodes scancodes in interrupt context and feeds the
//! resulting bytes (UTF-8 text, control characters, VT100 key sequences)
//! into the TTY.

use pc_keyboard::{layouts::De105Key, DecodedKey, HandleControl, KeyCode, Keyboard, ScancodeSet1};
use spin::Mutex;

static KEYBOARD: Mutex<Keyboard<De105Key, ScancodeSet1>> = Mutex::new(Keyboard::new(
    ScancodeSet1::new(),
    De105Key,
    HandleControl::MapLettersToUnicode,
));

/// Called from the keyboard interrupt handler only.
pub fn handle_scancode(scancode: u8) {
    let key = {
        let mut kb = KEYBOARD.lock();
        match kb.add_byte(scancode) {
            Ok(Some(event)) => kb.process_keyevent(event),
            _ => None,
        }
    };
    let mut utf8 = [0u8; 4];
    let bytes: &[u8] = match key {
        // Terminals send CR for Enter and DEL for Backspace.
        Some(DecodedKey::Unicode('\n')) => b"\r",
        Some(DecodedKey::Unicode('\x08')) => b"\x7f",
        Some(DecodedKey::Unicode('\x7f')) => b"\x1b[3~",
        Some(DecodedKey::Unicode(c)) => c.encode_utf8(&mut utf8).as_bytes(),
        Some(DecodedKey::RawKey(code)) => match code {
            KeyCode::ArrowUp => b"\x1b[A",
            KeyCode::ArrowDown => b"\x1b[B",
            KeyCode::ArrowRight => b"\x1b[C",
            KeyCode::ArrowLeft => b"\x1b[D",
            KeyCode::Home => b"\x1b[H",
            KeyCode::End => b"\x1b[F",
            KeyCode::Insert => b"\x1b[2~",
            KeyCode::Delete => b"\x1b[3~",
            KeyCode::PageUp => b"\x1b[5~",
            KeyCode::PageDown => b"\x1b[6~",
            _ => return,
        },
        None => return,
    };
    super::tty::input(bytes);
}
