use std::path::{Path, PathBuf};
use std::process::{self, Command};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: builder <kernel-elf-path>");
        process::exit(1);
    }

    let kernel_path = Path::new(&args[1]);
    let img_path = kernel_path.with_extension("img");

    println!("Building BIOS disk image...");
    bootloader::BiosBoot::new(kernel_path)
        .create_disk_image(&img_path)
        .expect("Failed to create disk image");

    println!("Starting QEMU with {}", img_path.display());
    let exit_status = Command::new("qemu-system-x86_64")
        .args([
            "-drive",
            &format!("format=raw,file={}", img_path.display()),
            "-device",
            "isa-debug-exit,iobase=0xf4,iosize=0x04",
            "-m",
            "128M",
        ])
        .status()
        .expect("Failed to run QEMU");

    // QEMU exit code 33 = halt via isa-debug-exit (write 0 → exit(2*0+1=1), not 33)
    // success means user closed window or halt command ran
    process::exit(exit_status.code().unwrap_or(0));
}
