//! The host-side builder (`cargo run` in kernel/ runs it): builds the root filesystem and its
//! cpio initramfs, the data disk and the UEFI or BIOS boot image, then starts QEMU.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

/// A copy of `kernel` without debug information (`<kernel>.boot`), made
/// with the toolchain's llvm-objcopy; the original if that is unavailable.
fn strip_debug_info(kernel: &Path) -> PathBuf {
    let stripped = kernel.with_extension("boot");
    let objcopy = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|sysroot| {
            let host = std::env::consts::ARCH.to_string() + "-unknown-linux-gnu";
            Path::new(sysroot.trim()).join("lib/rustlib").join(host).join("bin/llvm-objcopy")
        })
        .filter(|p| p.exists());
    let Some(objcopy) = objcopy else {
        eprintln!("warning: llvm-objcopy not found (rustup component llvm-tools); booting the unstripped kernel");
        return kernel.to_path_buf();
    };
    let ok = Command::new(&objcopy)
        .arg("--strip-debug")
        .arg(kernel)
        .arg(&stripped)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        stripped
    } else {
        eprintln!("warning: stripping the kernel failed; booting the unstripped kernel");
        kernel.to_path_buf()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: builder <kernel-elf-path> [qemu-args...]");
        process::exit(1);
    }
    // Every Nix call below (userspace/build.sh, mke2fs, OVMF) takes its
    // packages from the pinned nixpkgs, as CI does.
    let pin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nix/nixpkgs.nix");
    std::env::set_var("NIX_PATH", format!("nixpkgs={}", pin.canonicalize().expect("nix/nixpkgs.nix is missing").display()));

    let kernel_path = Path::new(&args[1]);
    let img_path = kernel_path.with_extension("img");
    let rootfs = kernel_path.with_extension("rootfs");
    let cpio_path = kernel_path.with_extension("cpio");

    // OXIDENIX_TEST=1: boot straight into the self-tests and let their result
    // become QEMU's exit status (1 = success, 3 = failure).
    // OXIDENIX_BENCH=1: the same, with the benchmarks (/etc/bench.sh,
    // docs/benchmarks) instead of the tests.
    let bench_mode = std::env::var_os("OXIDENIX_BENCH").is_some();
    let test_mode = bench_mode || std::env::var_os("OXIDENIX_TEST").is_some();
    println!("Building root filesystem...");
    build_rootfs(&rootfs).expect("Failed to build root filesystem");
    if test_mode {
        let script = if bench_mode { "bench.sh" } else { "runtests.sh" };
        std::os::unix::fs::symlink(script, rootfs.join("etc/autorun")).expect("Failed to link /etc/autorun");
    }
    let mut cpio = io::BufWriter::new(fs::File::create(&cpio_path).expect("Failed to create cpio"));
    write_cpio(&rootfs, &mut cpio).expect("Failed to write cpio");
    drop(cpio);

    // OXIDENIX_DISK=<path>: another data disk (the benchmarks use a fresh one).
    let data_disk = match std::env::var_os("OXIDENIX_DISK") {
        Some(path) => PathBuf::from(path),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../disk.img"),
    };
    if !data_disk.exists() {
        println!("Creating persistent ext2 disk {}...", data_disk.display());
        create_data_disk(&data_disk).expect("Failed to create the data disk");
    }

    // Most of a debug build is DWARF debug information the kernel never
    // uses, and the BIOS bootloader copies the whole ELF through real-mode
    // disk reads (about 1 MB/s under QEMU). Boot a stripped copy; the full
    // ELF stays next to it for debuggers.
    let boot_kernel = strip_debug_info(kernel_path);

    let firmware = Firmware::from_env();
    println!("Building {} disk image...", firmware.name());
    match firmware {
        Firmware::Uefi => bootloader::UefiBoot::new(&boot_kernel)
            .set_ramdisk(&cpio_path)
            .create_disk_image(&img_path),
        Firmware::Bios => bootloader::BiosBoot::new(&boot_kernel)
            .set_ramdisk(&cpio_path)
            .create_disk_image(&img_path),
    }
    .expect("Failed to create disk image");

    // OXIDENIX_BUILD_ONLY=1: produce the images without starting QEMU.
    if std::env::var_os("OXIDENIX_BUILD_ONLY").is_some() {
        println!("Images ready: {} and {}", img_path.display(), data_disk.display());
        return;
    }

    println!("Starting QEMU ({}) with {}", firmware.name(), img_path.display());
    let mut qemu = Command::new("qemu-system-x86_64");
    if firmware == Firmware::Uefi {
        // OVMF: its code read-only, its variable store a fresh copy per run
        // (the boot is reproducible: no boot entries left from earlier runs).
        let ovmf = ovmf_dir();
        let vars = kernel_path.with_extension("ovmf-vars.fd");
        fs::copy(ovmf.join("OVMF_VARS.fd"), &vars).expect("Failed to copy the OVMF variable store");
        fs::set_permissions(&vars, fs::Permissions::from_mode(0o644)).expect("Failed to make the OVMF variable store writable");
        qemu.args([
            "-drive",
            &format!("if=pflash,format=raw,readonly=on,file={}", ovmf.join("OVMF_CODE.fd").display()),
            "-drive",
            &format!("if=pflash,format=raw,file={}", vars.display()),
        ]);
    }
    if test_mode {
        // The serial port carries the kernel's output to stdout. CI has no
        // display; locally the window stays.
        qemu.args(["-serial", "stdio", "-no-reboot"]);
        if std::env::var_os("CI").is_some() {
            qemu.args(["-display", "none"]);
        }
    }
    let exit_status = qemu
        .args([
            "-drive",
            &format!("format=raw,file={},if=ide,index=0", img_path.display()),
            "-drive",
            &format!("format=raw,file={},if=none,id=data", data_disk.display()),
            "-device",
            "virtio-blk-pci,drive=data,disable-modern=on",
            // q35 (ICH9) rather than the default i440fx: it can have an IOMMU.
            "-machine",
            "q35",
            // The host's CPU features under KVM (PCIDs among them), all that
            // QEMU emulates otherwise.
            "-cpu",
            "max",
            "-device",
            "isa-debug-exit,iobase=0xf4,iosize=0x04",
            "-m",
            "256M",
            "-smp",
            "4",
            // Hardware virtualization where available (about 10x faster,
            // the boot included); QEMU falls back to emulation otherwise.
            "-accel",
            "kvm",
            "-accel",
            "tcg",
            // User-mode networking (10.0.2.0/24, DHCP, DNS at 10.0.2.3) and a
            // TCP echo service at 10.0.2.100:7 for the self-tests.
            "-netdev",
            "user,id=net0,guestfwd=tcp:10.0.2.100:7-cmd:cat",
            "-device",
            "virtio-net-pci,netdev=net0,disable-modern=on",
        ])
        .args(if std::env::var_os("CI").is_some() && test_mode {
            &[][..]
        } else {
            // Scale the guest picture with the window (resize or fullscreen).
            &["-display", "gtk,zoom-to-fit=on"][..]
        })
        .args(&args[2..])
        .status()
        .expect("Failed to run QEMU");

    process::exit(exit_status.code().unwrap_or(0));
}

/// The firmware the image boots with: `OXIDENIX_FIRMWARE=uefi` (the
/// default, OVMF under QEMU) or `bios` (SeaBIOS).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Firmware {
    Uefi,
    Bios,
}

impl Firmware {
    fn from_env() -> Firmware {
        match std::env::var("OXIDENIX_FIRMWARE").as_deref() {
            Err(_) | Ok("uefi") => Firmware::Uefi,
            Ok("bios") => Firmware::Bios,
            Ok(other) => {
                eprintln!("OXIDENIX_FIRMWARE={other}: expected uefi or bios");
                process::exit(2);
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Firmware::Uefi => "UEFI",
            Firmware::Bios => "BIOS",
        }
    }
}

/// The directory holding OVMF_CODE.fd and OVMF_VARS.fd: `OXIDENIX_OVMF`,
/// else nixpkgs' OVMF (built or fetched once, then from the Nix store).
fn ovmf_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OXIDENIX_OVMF") {
        return PathBuf::from(dir);
    }
    let out = Command::new("nix-build")
        .args(["<nixpkgs>", "-A", "OVMF.fd", "--no-out-link"])
        .stderr(process::Stdio::inherit())
        .output()
        .expect("Failed to run nix-build for OVMF (or set OXIDENIX_OVMF)");
    if !out.status.success() {
        eprintln!("nix-build OVMF.fd failed; set OXIDENIX_OVMF to a directory with OVMF_CODE.fd and OVMF_VARS.fd");
        process::exit(2);
    }
    let store = String::from_utf8(out.stdout).expect("nix-build printed a non-UTF-8 path");
    Path::new(store.trim()).join("FV")
}

fn build_rootfs(root: &Path) -> io::Result<()> {
    let userspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../userspace");
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    for dir in ["bin", "tmp", "dev", "root"] {
        fs::create_dir_all(root.join(dir))?;
    }
    copy_tree(&userspace.join("rootfs"), root)?;
    let status = Command::new(userspace.join("build.sh"))
        .arg(root.join("bin"))
        .status()?;
    if !status.success() {
        return Err(io::Error::other("userspace/build.sh failed"));
    }
    Ok(())
}

/// A 64 MiB ext2 filesystem (1 KiB blocks, 128-byte inodes, no extensions the
/// kernel does not implement), pre-filled from userspace/disk.
fn create_data_disk(path: &Path) -> io::Result<()> {
    let content = Path::new(env!("CARGO_MANIFEST_DIR")).join("../userspace/disk");
    let mkfs = format!(
        "unset SOURCE_DATE_EPOCH; mke2fs -q -t ext2 -b 1024 -I 128 -O none,filetype,sparse_super,large_file -L oxidenix -d '{}' -F '{}' 65536",
        content.display(),
        path.display()
    );
    let status = Command::new("nix-shell").args(["-p", "e2fsprogs", "--run", &mkfs]).status()?;
    if !status.success() {
        let _ = fs::remove_file(path);
        return Err(io::Error::other("mke2fs failed"));
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Writes `root` as a cpio archive in "newc" format (like a Linux initramfs).
fn write_cpio(root: &Path, out: &mut impl Write) -> io::Result<()> {
    let mut entries = Vec::new();
    collect(root, root, &mut entries)?;
    entries.sort();
    for (ino, rel) in entries.iter().enumerate() {
        let path = root.join(rel);
        let meta = fs::symlink_metadata(&path)?;
        let data = if meta.file_type().is_symlink() {
            fs::read_link(&path)?.into_os_string().into_encoded_bytes()
        } else if meta.is_file() {
            fs::read(&path)?
        } else {
            Vec::new()
        };
        let mode = meta.mode() & 0o170000 | meta.permissions().mode() & 0o7777;
        write_entry(out, ino as u32 + 1, mode, rel.to_str().unwrap(), &data)?;
    }
    write_entry(out, 0, 0, "TRAILER!!!", &[])
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        out.push(entry.path().strip_prefix(root).unwrap().to_path_buf());
        if entry.file_type()?.is_dir() {
            collect(root, &entry.path(), out)?;
        }
    }
    Ok(())
}

fn write_entry(out: &mut impl Write, ino: u32, mode: u32, name: &str, data: &[u8]) -> io::Result<()> {
    let nlink = if mode & 0o170000 == 0o040000 { 2 } else { 1 };
    let fields = [ino, mode, 0, 0, nlink, 0, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
    let mut header = String::from("070701");
    for f in fields {
        header.push_str(&format!("{f:08x}"));
    }
    out.write_all(header.as_bytes())?;
    out.write_all(name.as_bytes())?;
    out.write_all(&[0])?;
    pad(out, 110 + name.len() + 1)?;
    out.write_all(data)?;
    pad(out, data.len())
}

fn pad(out: &mut impl Write, len: usize) -> io::Result<()> {
    out.write_all(&[0; 3][..(4 - len % 4) % 4])
}
