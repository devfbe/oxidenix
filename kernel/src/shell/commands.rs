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
        "" => {}
        other => crate::printkln!("unbekannter befehl: {}", other),
    }
}

fn cmd_help() {
    crate::printkln!("Verfuegbare Befehle:");
    crate::printkln!("  help          - diese Hilfe");
    crate::printkln!("  clear         - Bildschirm leeren");
    crate::printkln!("  echo <text>   - Text ausgeben");
    crate::printkln!("  info          - CPU-Infos");
    crate::printkln!("  mem           - Speicherstatistik + Selbsttest");
    crate::printkln!("  run <prog>    - Userspace-Programm starten");
    crate::printkln!("  halt          - System anhalten");
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
    crate::printkln!("Tschuess!");
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
        "RAM:  {} / {} KiB belegt ({} Frames frei)",
        after.used_frames * 4,
        after.total_frames * 4,
        after.total_frames - after.used_frames
    );
    crate::printkln!(
        "Heap: {} / {} KiB belegt",
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
    let Some(&name) = args.first() else {
        crate::printk!("Programme:");
        for (name, _) in crate::process::PROGRAMS {
            crate::printk!(" {}", name);
        }
        crate::printkln!();
        return;
    };
    match crate::process::run(name, args) {
        Ok(code) => crate::printkln!("[{} beendet mit Code {}]", name, code),
        Err(e) => crate::printkln!("run: {}", e),
    }
}
