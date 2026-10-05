pub mod commands;

use heapless::{String, Vec};
use lazy_static::lazy_static;
use pc_keyboard::{layouts, DecodedKey, HandleControl, Keyboard, ScancodeSet1};
use spin::Mutex;

lazy_static! {
    static ref KEYBOARD: Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>> =
        Mutex::new(Keyboard::new(
            ScancodeSet1::new(),
            layouts::Us104Key,
            HandleControl::Ignore,
        ));
}

pub fn run() -> ! {
    crate::printkln!("rust-kernel shell v0.1");
    crate::printkln!("Tippe 'help' fuer Hilfe.");
    crate::printkln!();

    loop {
        crate::printk!("> ");
        let line = read_line();
        let (cmd, args) = parse(&line);
        commands::dispatch(cmd, &args);
    }
}

fn read_line() -> String<256> {
    let mut buf: String<256> = String::new();
    loop {
        x86_64::instructions::hlt();

        while let Some(scancode) = crate::drivers::keyboard::pop_scancode() {
            let mut kb = KEYBOARD.lock();
            if let Ok(Some(key_event)) = kb.add_byte(scancode) {
                if let Some(key) = kb.process_keyevent(key_event) {
                    drop(kb);
                    match key {
                        DecodedKey::Unicode('\n') => {
                            crate::printkln!();
                            return buf;
                        }
                        DecodedKey::Unicode('\x08') => {
                            if !buf.is_empty() {
                                buf.pop();
                                crate::drivers::console::backspace();
                            }
                        }
                        DecodedKey::Unicode(c) if c.is_ascii() && !c.is_ascii_control() => {
                            if buf.push(c).is_ok() {
                                crate::printk!("{}", c);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn parse(line: &str) -> (&str, Vec<&str, 8>) {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let mut args: Vec<&str, 8> = Vec::new();
    for arg in parts {
        let _ = args.push(arg);
    }
    (cmd, args)
}
