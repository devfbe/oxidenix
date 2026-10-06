use heapless::Vec;

pub fn dispatch(cmd: &str, args: &Vec<&str, 8>) {
    match cmd {
        "help" => cmd_help(),
        "clear" => cmd_clear(),
        "echo" => cmd_echo(args),
        "halt" => cmd_halt(),
        "info" => cmd_info(),
        "mem" => cmd_mem(),
        "disk" => cmd_disk(),
        "run" => cmd_run(args),
        "" => {}
        other => crate::printkln!("unknown command: {}", other),
    }
}

fn cmd_help() {
    crate::printkln!("Available commands:");
    crate::printkln!("  help          - this help");
    crate::printkln!("  clear         - clear the screen");
    crate::printkln!("  echo <text>   - print text");
    crate::printkln!("  info          - CPU info");
    crate::printkln!("  mem           - memory statistics + self-test");
    crate::printkln!("  disk          - show the data disk");
    crate::printkln!("  run <prog>    - start a program from /bin");
    crate::printkln!("  halt          - halt the system");
}

fn cmd_clear() {
    crate::drivers::console::clear_screen();
}

fn cmd_echo(args: &Vec<&str, 8>) {
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            crate::printk!(" ");
        }
        crate::printk!("{}", arg);
    }
    crate::printkln!();
}

fn cmd_halt() {
    crate::printkln!("Bye!");
    // isa-debug-exit: write to port 0xf4 exits QEMU with code 2*val+1
    use x86_64::instructions::port::Port;
    unsafe {
        Port::<u32>::new(0xf4).write(0);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

fn cmd_info() {
    let (ebx, edx, ecx): (u32, u32, u32);
    unsafe {
        // RBX is reserved by LLVM; save/restore around cpuid and copy via free reg
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx,
            in("eax") 0u32,
            lateout("ecx") ecx,
            lateout("edx") edx,
            options(nostack, preserves_flags),
        );
    }
    let vendor = [
        (ebx & 0xff) as u8,
        ((ebx >> 8) & 0xff) as u8,
        ((ebx >> 16) & 0xff) as u8,
        ((ebx >> 24) & 0xff) as u8,
        (edx & 0xff) as u8,
        ((edx >> 8) & 0xff) as u8,
        ((edx >> 16) & 0xff) as u8,
        ((edx >> 24) & 0xff) as u8,
        (ecx & 0xff) as u8,
        ((ecx >> 8) & 0xff) as u8,
        ((ecx >> 16) & 0xff) as u8,
        ((ecx >> 24) & 0xff) as u8,
    ];
    crate::printk!("CPU Vendor: ");
    for &b in &vendor {
        if b.is_ascii_graphic() || b == b' ' {
            crate::printk!("{}", b as char);
        }
    }
    crate::printkln!();
}

fn cmd_mem() {
    use crate::memory;
    use alloc::{boxed::Box, format, vec::Vec};
    use x86_64::structures::paging::{FrameAllocator, FrameDeallocator};

    let before = memory::stats();

    let v: Vec<u64> = (0..10_000).collect();
    let b = Box::new(0xdead_beef_u64);
    let s = format!("heap ok {}", v.len());
    let heap_ok = v.iter().sum::<u64>() == 49_995_000 && *b == 0xdead_beef && s == "heap ok 10000";
    let heap_during = memory::stats().heap_used;
    drop((v, b, s));
    let heap_back = memory::stats().heap_used == before.heap_used;

    let frames_ok = {
        let mut guard = memory::FRAMES.lock();
        let fa = guard.as_mut().unwrap();
        let a = fa.allocate_frame().unwrap();
        let b = fa.allocate_frame().unwrap();
        unsafe {
            fa.deallocate_frame(a);
            fa.deallocate_frame(b);
        }
        let b2 = fa.allocate_frame().unwrap();
        let a2 = fa.allocate_frame().unwrap();
        unsafe {
            fa.deallocate_frame(a2);
            fa.deallocate_frame(b2);
        }
        a != b && a2 == a && b2 == b
    };
    let after = memory::stats();

    crate::printkln!(
        "RAM:  {} / {} KiB used ({} frames free)",
        after.used_frames * 4,
        after.total_frames * 4,
        after.total_frames - after.used_frames
    );
    crate::printkln!(
        "Heap: {} / {} KiB used",
        after.heap_used / 1024,
        (after.heap_used + after.heap_free) / 1024
    );
    let ok = |b: bool| if b { "ok" } else { "FEHLER" };
    crate::printkln!(
        "Test: heap alloc {} (peak {} KiB), heap free {}, frame reuse {}, frames balanced {}",
        ok(heap_ok),
        heap_during / 1024,
        ok(heap_back),
        ok(frames_ok),
        ok(after.used_frames == before.used_frames)
    );
}

fn cmd_run(args: &Vec<&str, 8>) {
    if args.is_empty() {
        crate::printkln!("usage: run <program> [args...]  (looked up in /bin, e.g. run ls -l /)");
        return;
    }
    run_program(args);
}

/// Starts `args[0]` (from /bin unless it contains a '/') in the foreground
/// and waits for it.
pub fn run_program(args: &[&str]) {
    let name = args[0];
    let pid = match crate::process::spawn(name, args) {
        Ok(pid) => pid,
        Err(errno) => {
            crate::printkln!("run: cannot start {} (errno {})", name, errno);
            return;
        }
    };
    use crate::process::WaitStatus;
    loop {
        match crate::process::wait_for(pid).map(crate::process::decode_status) {
            Ok(WaitStatus::Exited(code)) => crate::printkln!("[{} (pid {}) exited with code {}]", name, pid, code),
            Ok(WaitStatus::Killed(sig)) => crate::printkln!("[{} (pid {}) killed by signal {}]", name, pid, sig),
            Ok(WaitStatus::Stopped(sig)) => {
                // The monitor has no job control: resume the program in the foreground.
                crate::printkln!("[{} (pid {}) stopped by signal {}; the monitor resumes it]", name, pid, sig);
                crate::drivers::tty::set_foreground(pid);
                crate::process::signal::send(pid, crate::process::signal::SIGCONT);
                continue;
            }
            Err(errno) => crate::printkln!("run: wait failed (errno {})", errno),
        }
        break;
    }
    crate::process::reap_orphans();
}

fn cmd_disk() {
    use crate::drivers::ata;
    let Some(sectors) = ata::init() else {
        crate::printkln!("no data disk (primary slave) found");
        return;
    };
    crate::printkln!("data disk: {} sectors ({} MiB)", sectors, sectors / 2048);
    let mut sb = [0u8; 1024];
    match ata::read(2, &mut sb) {
        Ok(()) => {
            let magic = u16::from_le_bytes([sb[56], sb[57]]);
            let label = core::str::from_utf8(&sb[120..136]).unwrap_or("?").trim_end_matches('\0');
            crate::printkln!("superblock magic {:#06x} ({}), label '{}'", magic, if magic == 0xef53 { "ext2" } else { "unknown" }, label);
        }
        Err(e) => crate::printkln!("read failed: {:?}", e),
    }
}
