//! PS/2 keyboard: decodes scancodes in interrupt context and feeds the
//! resulting bytes (UTF-8 text, control characters, the Linux console's key
//! sequences) into the console device, whose holder's line discipline
//! interprets them.

use pc_keyboard::{layouts::De105Key, DecodedKey, HandleControl, KeyCode, KeyState, Keyboard, ScancodeSet1};
use crate::sync::IrqSpinLock;
use core::sync::atomic::{AtomicBool, Ordering};

/// Whether AltGr is held (the crate's German layout lacks several AltGr
/// characters, which `alt_gr_char` adds).
static ALT_GR: AtomicBool = AtomicBool::new(false);

/// AltGr characters of the German layout that `De105Key` does not know.
fn alt_gr_char(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::Key2 => '²',
        KeyCode::Key3 => '³',
        KeyCode::Key7 => '{',
        KeyCode::Key8 => '[',
        KeyCode::Key9 => ']',
        KeyCode::Key0 => '}',
        KeyCode::OemMinus => '\\',
        KeyCode::M => 'µ',
        _ => return None,
    })
}

static KEYBOARD: IrqSpinLock<Keyboard<De105Key, ScancodeSet1>> = IrqSpinLock::new(Keyboard::new(
    ScancodeSet1::new(),
    De105Key,
    HandleControl::MapLettersToUnicode,
));

/// Called from the keyboard interrupt handler only.
pub fn handle_scancode(scancode: u8) {
    let key = {
        let mut kb = KEYBOARD.lock();
        match kb.add_byte(scancode) {
            Ok(Some(event)) => {
                if event.code == KeyCode::RAltGr {
                    ALT_GR.store(event.state == KeyState::Down, Ordering::Relaxed);
                }
                let fixed = (event.state == KeyState::Down && ALT_GR.load(Ordering::Relaxed))
                    .then(|| alt_gr_char(event.code))
                    .flatten();
                let decoded = kb.process_keyevent(event);
                fixed.map(DecodedKey::Unicode).or(decoded)
            }
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
            // The Linux console's sequences (terminfo `linux`).
            KeyCode::Home => b"\x1b[1~",
            KeyCode::End => b"\x1b[4~",
            KeyCode::Insert => b"\x1b[2~",
            KeyCode::Delete => b"\x1b[3~",
            KeyCode::PageUp => b"\x1b[5~",
            KeyCode::PageDown => b"\x1b[6~",
            KeyCode::F1 => b"\x1b[[A",
            KeyCode::F2 => b"\x1b[[B",
            KeyCode::F3 => b"\x1b[[C",
            KeyCode::F4 => b"\x1b[[D",
            KeyCode::F5 => b"\x1b[[E",
            KeyCode::F6 => b"\x1b[17~",
            KeyCode::F7 => b"\x1b[18~",
            KeyCode::F8 => b"\x1b[19~",
            KeyCode::F9 => b"\x1b[20~",
            KeyCode::F10 => b"\x1b[21~",
            KeyCode::F11 => b"\x1b[23~",
            KeyCode::F12 => b"\x1b[24~",
            _ => return,
        },
        None => return,
    };
    super::console_device::input(bytes);
}
