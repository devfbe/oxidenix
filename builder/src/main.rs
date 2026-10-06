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

    let kernel_path = Path::new(&args[1]);
    let img_path = kernel_path.with_extension("img");
    let rootfs = kernel_path.with_extension("rootfs");
    let cpio_path = kernel_path.with_extension("cpio");

    // OXIDENIX_TEST=1: boot straight into the self-tests and let their result
    // become QEMU's exit status (1 = success, 3 = failure).
    let test_mode = std::env::var_os("OXIDENIX_TEST").is_some();
    println!("Building root filesystem...");
    build_rootfs(&rootfs).expect("Failed to build root filesystem");
    if test_mode {
        std::os::unix::fs::symlink("runtests.sh", rootfs.join("etc/autorun")).expect("Failed to link /etc/autorun");
    }
    let mut cpio = io::BufWriter::new(fs::File::create(&cpio_path).expect("Failed to create cpio"));
    write_cpio(&rootfs, &mut cpio).expect("Failed to write cpio");
    drop(cpio);

    let data_disk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../disk.img");
    if !data_disk.exists() {
        println!("Creating persistent ext2 disk {}...", data_disk.display());
        create_data_disk(&data_disk).expect("Failed to create the data disk");
    }

    // The BIOS bootloader copies the whole ELF through real-mode disk reads
    // (about 1 MB/s under QEMU), and most of a debug build is DWARF debug
    // information the kernel never uses. Boot a stripped copy; the full ELF
    // stays next to it for debuggers.
    let boot_kernel = strip_debug_info(kernel_path);

    println!("Building BIOS disk image...");
    bootloader::BiosBoot::new(&boot_kernel)
        .set_ramdisk(&cpio_path)
        .create_disk_image(&img_path)
        .expect("Failed to create disk image");

    // OXIDENIX_BUILD_ONLY=1: produce the images without starting QEMU.
    if std::env::var_os("OXIDENIX_BUILD_ONLY").is_some() {
        println!("Images ready: {} and {}", img_path.display(), data_disk.display());
        return;
    }

    println!("Starting QEMU with {}", img_path.display());
    let mut qemu = Command::new("qemu-system-x86_64");
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
            &format!("format=raw,file={},if=ide,index=1", data_disk.display()),
            "-device",
            "isa-debug-exit,iobase=0xf4,iosize=0x04",
            "-m",
            "256M",
            "-smp",
            "4",
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
