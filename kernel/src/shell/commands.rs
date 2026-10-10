//! The commands of the kernel's built-in fallback shell (help, mem, run, kill, ...).

use heapless::Vec;

pub fn dispatch(cmd: &str, args: &Vec<&str, 8>) {
    match cmd {
        "help" => cmd_help(),
        "clear" => cmd_clear(),
        "echo" => cmd_echo(args),
        "halt" => cmd_halt(),
        "info" => cmd_info(),
        "mem" => cmd_mem(),
        "run" => cmd_run(args),
        "kill" => cmd_kill(args),
        "ps" => cmd_ps(),
        "lspci" => cmd_lspci(),
        "cpus" => cmd_cpus(),
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
    crate::printkln!("  run <prog>    - start a program from /bin");
    crate::printkln!("  ps            - list processes");
    crate::printkln!("  lspci         - list PCI devices");
    crate::printkln!("  cpus          - CPUs with load and context switches");
    crate::printkln!("  kill <pid|name> - end a process (also a server's)");
    crate::printkln!("  halt          - halt the system");
}

fn cmd_cpus() {
    crate::printkln!("  CPU  APIC  BUSY  SWITCHES  QUEUED");
    for i in 0..crate::smp::MAX_CPUS {
        let Some(cpu) = crate::smp::by_index(i) else { continue };
        let s = crate::process::sched::cpu_stats(cpu);
        let busy = s.user + s.system;
        let percent = busy * 100 / (busy + s.idle).max(1);
        crate::printkln!("{:5} {:5} {:4}% {:9} {:7}", i, cpu.apic_id(), percent, s.switches, s.queued);
    }
}

fn cmd_lspci() {
    for d in crate::drivers::pci::scan() {
        crate::printkln!(
            "{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x} irq {}",
            d.bus, d.slot, d.func, d.vendor, d.device, d.class, d.subclass, d.irq
        );
    }
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
    super::settle();
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
    let ok = |b: bool| if b { "ok" } else { "FAIL" };
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
/// and waits for it; returns how it ended.
pub fn run_program(args: &[&str]) -> Option<crate::process::WaitStatus> {
    let name = args[0];
    let tree = match crate::process::spawn(name, args) {
        Ok(tree) => tree,
        Err(errno) => {
            crate::printkln!("run: cannot start {} (errno {})", name, errno);
            return None;
        }
    };
    use crate::process::WaitStatus;
    let pid = tree.pid;
    let result = match crate::process::wait_for(pid).map(crate::process::decode_status) {
        Ok(WaitStatus::Exited(code)) => {
            crate::printkln!("[{} (pid {}) exited with code {}]", name, pid, code);
            Some(WaitStatus::Exited(code))
        }
        Ok(WaitStatus::Killed(sig)) => {
            crate::printkln!("[{} (pid {}) killed by signal {}]", name, pid, sig);
            Some(WaitStatus::Killed(sig))
        }
        Err(errno) => {
            crate::printkln!("run: wait failed (errno {})", errno);
            None
        }
    };
    // The tree's first process ended: the console is the monitor's again
    // and the host grant goes (what is left of the tree finds its terminal
    // hung up and the machine's state no longer its to change).
    drop(tree);
    crate::process::reap_orphans();
    result
}


fn cmd_ps() {
    crate::printkln!("  PID  CPU  THR  STATE     NAME");
    for (pid, name, state, server, cpu, threads) in crate::process::list() {
        crate::printkln!("{:5}  {:3}  {:3}  {:9} {}{}", pid, cpu, threads, state, name, if server { " (server)" } else { "" });
    }
}

fn cmd_kill(args: &Vec<&str, 8>) {
    let Some(&target) = args.first() else {
        crate::printkln!("usage: kill <pid|name>");
        return;
    };
    let pid = target.parse::<u64>().ok().or_else(|| {
        crate::process::list().into_iter().find(|p| p.1 == target && p.0 != 0).map(|p| p.0)
    });
    let Some(pid) = pid.filter(|&p| p > 0) else {
        crate::printkln!("kill: no such process: {}", target);
        return;
    };
    match crate::process::kill::kill_pid(pid) {
        Ok(()) => crate::printkln!("killed {}", pid),
        Err(errno) => crate::printkln!("kill: errno {}", errno),
    }
    crate::process::reap_orphans();
}
