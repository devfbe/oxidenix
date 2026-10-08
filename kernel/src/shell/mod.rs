//! Kernel monitor: starts a shell at boot and takes over when it exits.

pub mod commands;

use heapless::{String, Vec};

pub fn run() -> ! {
    // Test mode: the builder links /etc/autorun to the test script; its exit
    // status becomes QEMU's (1 = success, 3 = failure).
    if crate::fs::resolve("/", "/etc/autorun", true).is_ok() {
        crate::printkln!("[autorun] running /etc/autorun");
        let ok = matches!(commands::run_program(&["sh", "/etc/autorun"]), Some(crate::process::WaitStatus::Exited(0)));
        crate::printkln!("[autorun] {}", if ok { "success" } else { "failure" });
        settle();
        crate::power_off(if ok { 0 } else { 1 });
    }
    let shell = if crate::fs::resolve("/", "/bin/bash", true).is_ok() { "bash" } else { "sh" };
    crate::printkln!("\x1b[1;33m  oxidenix\x1b[0m - a Unix-like kernel in Rust, built with AI");
    crate::printkln!("  starting /bin/{} ('exit' returns to the kernel monitor)\n", shell);
    commands::run_program(&[shell]);

    crate::printkln!("oxidenix monitor. Type 'help' for help, 'run bash' for a shell.");
    loop {
        crate::drivers::tty::set_foreground(0);
        crate::drivers::tty::reset();
        crate::printk!("> ");
        let line = read_line();
        let (cmd, args) = parse(&line);
        commands::dispatch(cmd, &args);
    }
}

/// Before the machine goes off: the Linux server instances end (each writes
/// its caches back to the disk), then their channels (diskfs commits what
/// they held), each wait at most a few seconds.
pub fn settle() {
    let deadline = crate::time::now() + 10 * crate::time::NSEC_PER_SEC;
    // (The pagers' processes end last, as the kernel's children: reaped
    // here, their instances go.)
    let ended = loop {
        crate::process::reap_orphans();
        let soon = (crate::time::now() + 50_000_000).min(deadline);
        if crate::process::linux::settle(soon) {
            break true;
        }
        if crate::time::now() >= deadline {
            break false;
        }
    };
    if !ended {
        crate::printkln!("[kernel] a Linux server instance did not end; its caches may not be written back. Still there:");
        for (pid, _, name, state, server, _, _) in crate::process::list() {
            if !server || name == "linux-pager" {
                crate::printkln!("  {} {} ({})", pid, name, state);
            }
        }
    }
    if !crate::process::channel::settle(deadline) {
        crate::printkln!("[kernel] a channel did not close");
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
