//! Kernel monitor: starts a shell at boot and takes over when it exits.

pub mod commands;

use heapless::{String, Vec};

pub fn run() -> ! {
    // Test mode: the builder links /etc/autorun to the test script; its exit
    // status becomes QEMU's (1 = success, 3 = failure).
    if crate::fs::program("/etc/autorun").is_some() {
        crate::printkln!("[autorun] running /etc/autorun");
        let ok = matches!(commands::run_program(&["sh", "/etc/autorun"]), Some(crate::process::WaitStatus::Exited(0)));
        crate::printkln!("[autorun] {}", if ok { "success" } else { "failure" });
        settle();
        crate::power_off(if ok { 0 } else { 1 });
    }
    let shell = if crate::fs::program("/bin/bash").is_some() { "bash" } else { "sh" };
    crate::printkln!("\x1b[1;33m  oxidenix\x1b[0m - a Unix-like kernel in Rust, built with AI");
    crate::printkln!("  starting /bin/{} ('exit' returns to the kernel monitor)\n", shell);
    commands::run_program(&[shell]);

    crate::printkln!("oxidenix monitor. Type 'help' for help, 'run bash' for a shell.");
    loop {
        crate::printk!("> ");
        let line = read_line();
        let (cmd, args) = parse(&line);
        commands::dispatch(cmd, &args);
    }
}

/// Before the machine goes off: every Linux server instance is asked to
/// write its caches back (one whose programs still run, too) and the
/// instances end (each writes back once more), then their channels go
/// (diskfs commits what they held). Each wait lasts `SETTLE` without
/// progress (pages written back, instances ended) and `SETTLE_MAX` at
/// most: a big write-back is not cut short, a hung one does not hold the
/// shutdown for ever.
pub fn settle() {
    use crate::time::{now, NSEC_PER_SEC};
    const SETTLE: u64 = 10 * NSEC_PER_SEC;
    const SETTLE_MAX: u64 = 300 * NSEC_PER_SEC;
    let limit = now() + SETTLE_MAX;
    let progress = || (crate::fs::cache::cleaned_pages(), crate::process::linux::live());
    // Runs `step(until)` until it reports done; whether it did in time.
    let wait = |step: &mut dyn FnMut(u64) -> bool| {
        let mut seen = progress();
        let mut deadline = (now() + SETTLE).min(limit);
        loop {
            let soon = (now() + 50_000_000).min(deadline);
            if step(soon) {
                return true;
            }
            let p = progress();
            if p != seen {
                seen = p;
                deadline = (now() + SETTLE).min(limit);
            }
            if now() >= deadline {
                return false;
            }
        }
    };
    let ticket = crate::process::linux::sync_start(None);
    if !wait(&mut |until| crate::process::linux::sync_wait(ticket, None, until)) {
        crate::printkln!("[kernel] a Linux server instance did not write its caches back");
    }
    // (The pagers' processes end last, as the kernel's children: reaped
    // here, their instances go.)
    let ended = wait(&mut |until| {
        crate::process::reap_orphans();
        crate::process::linux::settle(until)
    });
    let deadline = now() + SETTLE;
    if !ended {
        crate::printkln!("[kernel] a Linux server instance did not end; its caches may not be written back. Still there:");
        for (pid, name, state, server, _, _) in crate::process::list() {
            if !server || name == "linux-pager" {
                crate::printkln!("  {} {} ({})", pid, name, state);
            }
        }
    }
    if !crate::process::channel::settle(deadline) {
        crate::printkln!("[kernel] a channel did not close");
    }
}

/// Reads one command line from the console device, which the monitor holds
/// between programs: echo, Backspace (a whole UTF-8 character), Ctrl+U and
/// Enter. The monitor is not Linux: no termios, no signals.
fn read_line() -> String<256> {
    use crate::drivers::{console, console_device};
    let mut buf: Vec<u8, 255> = Vec::new();
    // A byte read after an Esc that began no sequence: the next one to handle.
    let mut pending: Option<u8> = None;
    loop {
        let byte = pending.take().unwrap_or_else(console_device::monitor_read);
        match byte {
            b'\r' | b'\n' => {
                console::write_bytes(b"\r\n");
                break;
            }
            0x7f | 0x08 => {
                if buf.is_empty() {
                    continue;
                }
                while buf.pop().is_some_and(|b| b & 0xc0 == 0x80) {}
                console::write_bytes(b"\x08 \x08");
            }
            0x15 => {
                while let Some(b) = buf.pop() {
                    if b & 0xc0 != 0x80 {
                        console::write_bytes(b"\x08 \x08");
                    }
                }
            }
            // Other control characters and escape sequences (arrow keys,
            // function keys: ESC [ or ESC O, then parameters up to a final
            // byte) are ignored. A bare Esc is ignored alone: the byte after
            // it, if it begins no sequence, is handled as typed.
            0x1b => match console_device::monitor_read() {
                b'[' | b'O' => {
                    // The Linux console's F1-F5 are ESC [ [ A..E.
                    let mut b = console_device::monitor_read();
                    if b == b'[' {
                        b = console_device::monitor_read();
                    }
                    while !(0x40..=0x7e).contains(&b) {
                        b = console_device::monitor_read();
                    }
                }
                other => pending = Some(other),
            },
            b if b < 0x20 => {}
            b => {
                if buf.push(b).is_ok() {
                    console::write_bytes(&[b]);
                }
            }
        }
    }
    let mut line = String::new();
    let _ = line.push_str(core::str::from_utf8(&buf).unwrap_or(""));
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
