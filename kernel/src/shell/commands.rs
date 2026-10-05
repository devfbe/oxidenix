use heapless::Vec;

pub fn dispatch(cmd: &str, args: &Vec<&str, 8>) {
    match cmd {
        "help" => cmd_help(),
        "clear" => cmd_clear(),
        "echo" => cmd_echo(args),
        "halt" => cmd_halt(),
        "info" => cmd_info(),
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
    crate::printkln!("  halt          - System anhalten");
}

fn cmd_clear() {
    crate::drivers::vga::clear_screen();
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
