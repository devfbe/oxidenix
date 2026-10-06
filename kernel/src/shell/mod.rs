//! Kernel monitor: starts a shell at boot and takes over when it exits.

pub mod commands;

use heapless::{String, Vec};

pub fn run() -> ! {
    let shell = if crate::fs::resolve("/", "/bin/bash", true).is_ok() { "bash" } else { "sh" };
    crate::printkln!("rust-kernel: starting /bin/{} ('exit' returns to the kernel monitor)", shell);
    commands::run_program(&[shell]);

    crate::printkln!("rust-kernel monitor. Type 'help' for help, 'run bash' for a shell.");
    loop {
        crate::drivers::tty::set_foreground(0);
        crate::printk!("> ");
        let line = read_line();
        let (cmd, args) = parse(&line);
        commands::dispatch(cmd, &args);
    }
}

/// Reads one line through the TTY, which handles echo and line editing.
fn read_line() -> String<256> {
    let mut buf = [0u8; 256];
    let n = crate::drivers::tty::read(&mut buf, false).unwrap_or(0);
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    let mut line = String::new();
    let _ = line.push_str(text.trim_end_matches('\n'));
    line
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
