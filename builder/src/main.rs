//! The host-side builder (`cargo run` in kernel/ runs it): builds the root filesystem and its
//! cpio initramfs, the data disk and the UEFI or BIOS boot image, then starts QEMU.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

/// A copy of `kernel` without debug information (at `stripped`), made
/// with the toolchain's llvm-objcopy; the original if that is unavailable.
fn strip_debug_info(kernel: &Path, stripped: &Path) -> PathBuf {
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
        .arg(stripped)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        stripped.to_path_buf()
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

    // OXIDENIX_TEST=1: boot straight into the self-tests and let their result
    // become QEMU's exit status (1 = success, 3 = failure); after they passed,
    // boot again with the disk they wrote read-only (QEMU's readonly=on: the
    // device says VIRTIO_BLK_F_RO) into /etc/rotests.sh.
    // OXIDENIX_TEST=readonly: that read-only boot alone, on a fresh disk.
    // After both passed, a third boot without the network card, with USB
    // Ethernet functions instead (`Network::Usb`), into /etc/usbtests.sh;
    // OXIDENIX_TEST=usb: that boot alone.
    // OXIDENIX_BENCH=1: the same, with the benchmarks (/etc/bench.sh,
    // docs/benchmarks) instead of the tests.
    // OXIDENIX_AUTORUN=<host file>: the same with that shell script (say, a
    // program under development and the calls it makes in the kernel log).
    let bench_mode = std::env::var_os("OXIDENIX_BENCH").is_some();
    let autorun = std::env::var_os("OXIDENIX_AUTORUN").map(PathBuf::from);
    let test_var = std::env::var_os("OXIDENIX_TEST");
    let test_mode = bench_mode || autorun.is_some() || test_var.is_some();
    let self_tests = test_var.is_some() && autorun.is_none() && !bench_mode;
    let readonly_only = self_tests && test_var.as_deref() == Some("readonly".as_ref());
    let usb_only = self_tests && test_var.as_deref() == Some("usb".as_ref());

    // Each mode has artifacts of its own (`<kernel>.img`, `<kernel>.test.img`, ...): a test
    // run neither rewrites the image nor the root filesystem an interactive QEMU has open
    // (whose write lock on the image would stop the run).
    let mode = if bench_mode {
        Some("bench")
    } else if autorun.is_some() {
        Some("autorun")
    } else if test_mode {
        Some("test")
    } else {
        None
    };
    let artifact = |ext: &str| {
        let mut name = kernel_path.as_os_str().to_owned();
        for part in mode.iter().chain([&ext]) {
            name.push(".");
            name.push(part);
        }
        PathBuf::from(name)
    };
    let rootfs = artifact("rootfs");
    println!("Building root filesystem...");
    build_rootfs(&rootfs).expect("Failed to build root filesystem");
    // The dynamically linked programs: in /bin, their store paths on the data
    // disk (below).
    println!("Building the dynamically linked programs...");
    let dynamic = dynamic_programs();
    install_dynamic(&rootfs, &dynamic).expect("Failed to put the dynamic programs in the root filesystem");
    // With Node.js, its smoke tests (userspace/node/tests, run by
    // userspace/node/run-node.sh) go along, as /usr/lib/node-tests.
    if std::env::var_os("OXIDENIX_NODE").is_some_and(|v| !v.is_empty()) {
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../userspace/node/tests");
        let dest = rootfs.join("usr/lib/node-tests");
        fs::create_dir_all(&dest).expect("Failed to create /usr/lib/node-tests");
        copy_tree(&tests, &dest).expect("Failed to copy the Node.js tests");
    }
    // The boots of this run, in order: what /etc/autorun is in each, and
    // whether the data disk is read-only there.
    let plain = |suffix, autorun, read_only| Boot { suffix, autorun, read_only, network: Network::Virtio };
    // The USB boot: no network card, USB Ethernet functions only (as a
    // machine that tethers through a phone), into /etc/usbtests.sh.
    let usb = || Boot { suffix: "usb", autorun: Autorun::Script("exec sh /etc/usbtests.sh\n"), read_only: false, network: Network::Usb };
    let boots: Vec<Boot> = if let Some(script) = &autorun {
        vec![plain("", Autorun::Copy(script.clone()), false)]
    } else if bench_mode {
        vec![plain("", Autorun::Link("bench.sh"), false)]
    } else if readonly_only {
        vec![plain("readonly", Autorun::Script("exec sh /etc/rotests.sh\n"), true)]
    } else if usb_only {
        vec![usb()]
    } else if self_tests {
        vec![
            plain("", Autorun::Link("runtests.sh"), false),
            // (The disk the first boot wrote: its last file is checked too.)
            plain("readonly", Autorun::Script("exec sh /etc/rotests.sh after-rw\n"), true),
            usb(),
        ]
    } else {
        vec![plain("", Autorun::None, false)]
    };

    // The data disk: OXIDENIX_DISK=<path> (created if missing; the
    // benchmarks use a fresh one); in test mode a fresh small one of its own
    // on every run (target/test-disk.img), so the tests (which fill the disk
    // to the last block) neither touch the persistent disk.img nor depend on
    // what earlier runs left; else the persistent disk.img.
    let data_disk = match std::env::var_os("OXIDENIX_DISK") {
        Some(path) => PathBuf::from(path),
        None if self_tests => Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-disk.img"),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../disk.img"),
    };
    if self_tests && std::env::var_os("OXIDENIX_DISK").is_none() {
        let _ = fs::remove_file(&data_disk);
        println!("Creating the test disk {}...", data_disk.display());
        create_data_disk(&data_disk, TEST_DISK_BYTES).expect("Failed to create the test disk");
    } else if !data_disk.exists() {
        println!("Creating persistent ext2 disk {}...", data_disk.display());
        create_data_disk(&data_disk, DATA_DISK_BYTES).expect("Failed to create the data disk");
    } else if std::env::var_os("OXIDENIX_DISK").is_none() && fs::metadata(&data_disk).is_ok_and(|m| m.len() < DATA_DISK_BYTES) {
        println!(
            "note: {} is smaller than the {} GiB a new data disk has; delete it to get a new one",
            data_disk.display(),
            DATA_DISK_BYTES >> 30
        );
    }
    // The store paths of the dynamic programs (/nix/store, /data/nix/store in
    // oxidenix).
    let mut files = store_files(&dynamic).expect("Failed to list the dynamic programs' store paths");
    // OXIDENIX_NODE=1 (or the path of a static node binary): Node.js on the
    // data disk as /bin/node (/data/bin/node in oxidenix).
    if let Some(node) = node_binary() {
        println!("Installing {} as /bin/node on {}...", node.display(), data_disk.display());
        files.push(Install { what: Content::File(node), dest: "bin/node".into(), mode: 0o755 });
    }
    // The self-tests' read-only boot reads a file the builder put on the
    // disk (/data/rotest/hello, a copy of /bin/hello).
    if self_tests {
        files.push(Install { what: Content::File(rootfs.join("bin/hello")), dest: "rotest/hello".into(), mode: 0o755 });
    }
    install_on_disk(&data_disk, &files).expect("Failed to put the files on the data disk");

    // Most of a debug build is DWARF debug information the kernel never
    // uses, and the BIOS bootloader copies the whole ELF through real-mode
    // disk reads (about 1 MB/s under QEMU). Boot a stripped copy; the full
    // ELF stays next to it for debuggers.
    let boot_kernel = strip_debug_info(kernel_path, &artifact("boot"));

    // An initramfs and a boot image per boot (the mode's, `<kernel>.test.img`
    // say, and the read-only boot's `<kernel>.test.readonly.img`): the same
    // root filesystem with its own /etc/autorun.
    let firmware = Firmware::from_env();
    let mut images = Vec::new();
    for boot in &boots {
        let name = |ext: &str| if boot.suffix.is_empty() { artifact(ext) } else { artifact(&format!("{}.{ext}", boot.suffix)) };
        let (cpio_path, img_path) = (name("cpio"), name("img"));
        boot.autorun.install(&rootfs.join("etc/autorun")).expect("Failed to make /etc/autorun");
        let mut cpio = io::BufWriter::new(fs::File::create(&cpio_path).expect("Failed to create cpio"));
        write_cpio(&rootfs, &mut cpio).expect("Failed to write cpio");
        drop(cpio);
        println!("Building {} disk image {}...", firmware.name(), img_path.display());
        match firmware {
            Firmware::Uefi => bootloader::UefiBoot::new(&boot_kernel)
                .set_ramdisk(&cpio_path)
                .create_disk_image(&img_path),
            Firmware::Bios => bootloader::BiosBoot::new(&boot_kernel)
                .set_ramdisk(&cpio_path)
                .create_disk_image(&img_path),
        }
        .expect("Failed to create disk image");
        images.push(img_path);
    }

    // OXIDENIX_BUILD_ONLY=1: produce the images without starting QEMU.
    if std::env::var_os("OXIDENIX_BUILD_ONLY").is_some() {
        for img in &images {
            println!("Image ready: {}", img.display());
        }
        println!("Data disk: {}", data_disk.display());
        return;
    }

    // One boot after the other; in test mode a boot that did not succeed
    // (QEMU's 1) ends the run with its status.
    let mut code = 0;
    for (boot, img) in boots.iter().zip(&images) {
        code = qemu(firmware, img, &data_disk, boot, test_mode, &args[2..]);
        if test_mode && code != 1 {
            break;
        }
    }
    process::exit(code);
}

/// One boot of a run: its images' suffix (none for the first), what its
/// /etc/autorun is, whether it has the data disk read-only, and its
/// network.
struct Boot {
    suffix: &'static str,
    autorun: Autorun,
    read_only: bool,
    network: Network,
}

/// What a boot's machine has for a network.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Network {
    /// The virtio network card on QEMU's user network (10.0.2.0/24), and
    /// an xHCI controller with nothing on it.
    Virtio,
    /// No network card: USB Ethernet functions on the xHCI controller,
    /// each on a user network of its own (`USB_NETWORK`).
    Usb,
}

/// The USB boot's devices (QEMU's usb-net offers CDC-ECM and RNDIS): one
/// on a root port, ECM, on 10.0.3.0/24; one behind a USB 2 hub, RNDIS
/// (usbd takes its RNDIS function in test mode for the serial number
/// "rndis"), on 10.0.4.0/24. Each network has a TCP echo service at
/// .100:7 (as 10.0.2.100 is the card's).
const USB_NETWORK: [&str; 10] = [
    "-netdev",
    "user,id=usb0,net=10.0.3.0/24,guestfwd=tcp:10.0.3.100:7-cmd:cat",
    "-device",
    "usb-net,netdev=usb0,bus=xhci.0,port=1,mac=52:54:00:aa:00:01",
    "-device",
    "usb-hub,bus=xhci.0,port=2",
    "-netdev",
    "user,id=usb1,net=10.0.4.0/24,guestfwd=tcp:10.0.4.100:7-cmd:cat",
    "-device",
    "usb-net,netdev=usb1,bus=xhci.0,port=2.1,serial=rndis,mac=52:54:00:aa:00:02",
];

/// What /etc/autorun is in a boot's root filesystem (the kernel runs it
/// with `sh` and powers off with its status: test mode).
enum Autorun {
    None,
    /// A link to a script of /etc.
    Link(&'static str),
    /// A host file (OXIDENIX_AUTORUN).
    Copy(PathBuf),
    /// These commands.
    Script(&'static str),
}

impl Autorun {
    fn install(&self, at: &Path) -> io::Result<()> {
        if fs::symlink_metadata(at).is_ok() {
            fs::remove_file(at)?;
        }
        match self {
            Autorun::None => Ok(()),
            Autorun::Link(script) => std::os::unix::fs::symlink(script, at),
            Autorun::Copy(file) => fs::copy(file, at).map(|_| ()),
            Autorun::Script(text) => fs::write(at, text),
        }
    }
}

/// Boots `img` in QEMU with `data_disk` (read-only if the boot says:
/// QEMU's readonly=on, which virtio-blk reports as VIRTIO_BLK_F_RO), the
/// boot's network and the user's `extra` arguments; QEMU's exit status.
fn qemu(firmware: Firmware, img: &Path, data_disk: &Path, boot: &Boot, test_mode: bool, extra: &[String]) -> i32 {
    let read_only = boot.read_only;
    let mode = match (read_only, boot.network) {
        (true, _) => ", the data disk read-only",
        (false, Network::Usb) => ", USB Ethernet only",
        (false, Network::Virtio) => "",
    };
    println!("Starting QEMU ({}) with {}{}", firmware.name(), img.display(), mode);
    let mut qemu = Command::new("qemu-system-x86_64");
    if firmware == Firmware::Uefi {
        // OVMF: its code read-only, its variable store a fresh copy per run
        // (the boot is reproducible: no boot entries left from earlier runs).
        let ovmf = ovmf_dir();
        // (Next to the image: each mode and boot has its own.)
        let vars = img.with_extension("ovmf-vars.fd");
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
        // The serial port carries the kernel's output to stdout (and takes
        // nothing in: the tests' input is their own). A second one, COM2 with
        // nothing attached, is ttytest's to loop back (/dev/ttyS1). CI has no
        // display; locally the window stays.
        qemu.args(["-serial", "stdio", "-serial", "null", "-no-reboot"]);
        qemu.stdin(process::Stdio::null());
        if std::env::var_os("CI").is_some() {
            qemu.args(["-display", "none"]);
        }
    }
    let readonly = if read_only { ",readonly=on" } else { "" };
    qemu.args([
            "-drive",
            &format!("format=raw,file={},if=ide,index=0", img.display()),
            "-drive",
            &format!("format=raw,file={},if=none,id=data{}", data_disk.display(), readonly),
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
        ])
        .args(accelerators())
        // An xHCI controller (usbd's): USB devices go on its bus `xhci.0`
        // (`-device usb-host,...` passes one of the host's through, a phone
        // to tether through, say).
        .args(["-device", "qemu-xhci,id=xhci"])
        .args(match boot.network {
            // User-mode networking (10.0.2.0/24, DHCP, DNS at 10.0.2.3) and a
            // TCP echo service at 10.0.2.100:7 for the self-tests.
            Network::Virtio => &["-netdev", "user,id=net0,guestfwd=tcp:10.0.2.100:7-cmd:cat", "-device", "virtio-net-pci,netdev=net0,disable-modern=on"][..],
            Network::Usb => &USB_NETWORK[..],
        })
        .args(if std::env::var_os("CI").is_some() && test_mode {
            &[][..]
        } else {
            // Scale the guest picture with the window (resize or fullscreen).
            &["-display", "gtk,zoom-to-fit=on"][..]
        })
        .args(extra);
    if !test_mode {
        return qemu.status().expect("Failed to run QEMU").code().unwrap_or(0);
    }
    // Test mode: the serial output passes through here, and the shutdown
    // after the script (`shell::settle`: every instance's write-back, its
    // end, the channels' close) must go without a complaint, or the boot
    // failed whatever the script said.
    let mut child = qemu.stdout(process::Stdio::piped()).spawn().expect("Failed to run QEMU");
    let mut serial = io::BufReader::new(child.stdout.take().expect("piped"));
    let (mut line, mut ended, mut complaint) = (Vec::new(), false, None);
    let mut out = io::stdout();
    while io::BufRead::read_until(&mut serial, b'\n', &mut line).is_ok_and(|n| n > 0) {
        let _ = out.write_all(&line).and_then(|_| out.flush());
        let text = String::from_utf8_lossy(&line);
        // (The tree's own end, with its final write-back, may come before
        // the monitor's word on the script: from the scripts' last line.)
        if text.starts_with("=== summary:") || text.starts_with("[autorun] ") && !text.starts_with("[autorun] running") {
            ended = true;
        } else if ended && SHUTDOWN_COMPLAINTS.iter().any(|c| text.contains(c)) && complaint.is_none() {
            complaint = Some(text.trim_end().to_string());
        }
        line.clear();
    }
    let code = child.wait().expect("Failed to wait for QEMU").code().unwrap_or(0);
    match complaint {
        Some(c) if code == 1 => {
            println!("builder: the shutdown after the tests went wrong ({c}): failure");
            3
        }
        _ => code,
    }
}

/// QEMU's accelerators: hardware virtualization where available (about 10x
/// faster, the boot included), falling back to emulation; `OXIDENIX_ACCEL=tcg`
/// emulates even where KVM is there (QEMU's own devices and CPU model: the
/// time sources and APIC modes a KVM guest does not use).
fn accelerators() -> Vec<String> {
    let list = std::env::var("OXIDENIX_ACCEL").unwrap_or_else(|_| "kvm,tcg".into());
    list.split(',').filter(|a| !a.is_empty()).flat_map(|a| ["-accel".to_string(), a.to_string()]).collect()
}

/// What the kernel and the Linux server say when the shutdown does not go as it should.
const SHUTDOWN_COMPLAINTS: [&str; 4] = [
    "did not write its caches back",
    "Linux server instance did not end",
    "a channel did not close",
    "writing the cache back failed",
];

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

/// The size of a new persistent data disk.
const DATA_DISK_BYTES: u64 = 2 << 30;
/// The size of the self-tests' disk: small, so that filling it is quick.
const TEST_DISK_BYTES: u64 = 64 << 20;

/// An ext2 filesystem of `bytes` with an ext3 journal (1 KiB blocks, 128-byte
/// inodes, no extensions diskfs does not implement; docs/design/ext3-journal.md),
/// pre-filled from userspace/disk.
/// The image is a sparse file: only the blocks mke2fs writes (and the journal's)
/// take space on the host.
fn create_data_disk(path: &Path, bytes: u64) -> io::Result<()> {
    let content = Path::new(env!("CARGO_MANIFEST_DIR")).join("../userspace/disk");
    // (Paths go to mke2fs as arguments of their own, through no shell.)
    let status = Command::new(e2fsprogs("mke2fs"))
        .args(["-q", "-t", "ext2", "-b", "1024", "-I", "128", "-O", "none,has_journal,filetype,sparse_super,large_file", "-L", "oxidenix", "-d"])
        .arg(&content)
        .arg("-F")
        .arg(path)
        .arg((bytes / 1024).to_string())
        .env_remove("SOURCE_DATE_EPOCH")
        .status()?;
    if !status.success() {
        let _ = fs::remove_file(path);
        return Err(io::Error::other("mke2fs failed"));
    }
    if let Err(e) = write_journal_zeros(path) {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

/// Writes the zeros of the journal's blocks (inode 8) for real: mke2fs zeroes a file's range
/// with `fallocate`, leaving unwritten extents, and the host's filesystem would convert one
/// on every first write of a log block, making each of the guest's flushes a commit of the
/// host's own journal (several times slower) until the log has gone round once.
fn write_journal_zeros(path: &Path) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let out = Command::new(e2fsprogs("debugfs")).args(["-R", "blocks <8>"]).arg(path).output()?;
    if !out.status.success() {
        return Err(io::Error::other("debugfs failed"));
    }
    let blocks: Vec<u64> = String::from_utf8_lossy(&out.stdout).split_whitespace().filter_map(|w| w.parse().ok()).collect();
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut block = [0u8; 1024];
    for n in blocks {
        file.seek(SeekFrom::Start(n * 1024))?;
        file.read_exact(&mut block)?;
        // (The journal's superblock stays as it is.)
        if block.iter().all(|&b| b == 0) {
            file.seek(SeekFrom::Start(n * 1024))?;
            file.write_all(&block)?;
        }
    }
    file.sync_all()
}

/// The Node.js binary `OXIDENIX_NODE` asks for: `1` builds userspace/node
/// (a static musl Node.js; the first build compiles V8 and takes an hour or more,
/// later ones come from the Nix store, kept from garbage collection by the
/// link target/node), any other value is the path of a static node binary.
/// None without the variable: normal runs and CI never need Node.
fn node_binary() -> Option<PathBuf> {
    let value = std::env::var_os("OXIDENIX_NODE").filter(|v| !v.is_empty())?;
    if value != "1" {
        return Some(PathBuf::from(value));
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let link = root.join("target/node");
    println!("Building Node.js (userspace/node)...");
    let status = Command::new("nix-build")
        .arg(root.join("userspace/node/default.nix"))
        .arg("-o")
        .arg(&link)
        .status()
        .expect("Failed to run nix-build for Node.js");
    if !status.success() {
        eprintln!("nix-build userspace/node failed");
        process::exit(2);
    }
    Some(link.join("bin/node"))
}

/// The path of e2fsprogs' program `tool` (mke2fs, debugfs) from the pinned
/// nixpkgs, built or fetched once.
fn e2fsprogs(tool: &str) -> PathBuf {
    static BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let out = Command::new("nix-build")
            .args(["<nixpkgs>", "-A", "e2fsprogs.bin", "--no-out-link"])
            .stderr(process::Stdio::inherit())
            .output()
            .expect("Failed to run nix-build for e2fsprogs");
        if !out.status.success() {
            eprintln!("nix-build e2fsprogs failed");
            process::exit(2);
        }
        let store = String::from_utf8(out.stdout).expect("nix-build printed a non-UTF-8 path");
        Path::new(store.trim()).join("bin")
    })
    .join(tool)
}

/// Runs debugfs on `disk` in `dir` with `args` (no shell: every path is an
/// argument of its own, and the names in debugfs's own commands are fixed
/// names in `dir`). Its exit status says nothing about its commands.
fn debugfs(dir: &Path, disk: &Path, args: &[&str]) -> io::Result<process::Output> {
    Command::new(e2fsprogs("debugfs"))
        .current_dir(dir)
        .args(args)
        .arg(disk)
        .env("DEBUGFS_PAGER", "__none__")
        .env_remove("SOURCE_DATE_EPOCH")
        .output()
}

/// Whether the files `a` and `b` have the same contents.
fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    use std::io::Read;
    let (mut fa, mut fb) = (fs::File::open(a)?, fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// A file for `install_on_disk`: the host file, where it goes on the disk
/// (relative to its root) and its permission bits.
struct Install {
    what: Content,
    dest: String,
    mode: u32,
}

/// What an `Install` puts there: the contents of a host file, or a symbolic
/// link with this target (its mode is a link's).
enum Content {
    File(PathBuf),
    Link(PathBuf),
}

/// Whether `s` can go into a debugfs command as it is: path components of
/// letters, digits and `._-+[` (no spaces, quotes or `..`; `[` is coreutils').
fn plain_path(s: &str) -> bool {
    s.split('/').all(|c| c != ".." && c.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+[".contains(&b)))
}

/// Puts the host files and links `files` on the ext2 image `disk` (each in
/// an existing or new directory, owned by root), replacing older versions; a
/// file the disk has with these contents already (a link with this target)
/// stays as it is. debugfs edits the image in place, so the rest of the
/// persistent disk stays as it is. Afterwards everything written is read back
/// and compared. Three debugfs runs however many files: one reads the disk's
/// versions, one writes what differs, one reads that back.
fn install_on_disk(disk: &Path, files: &[Install]) -> io::Result<()> {
    for f in files {
        // `dest` goes into debugfs's commands: a plain relative path only.
        let dest = &f.dest;
        assert!(!dest.is_empty() && !dest.starts_with('/') && !dest.contains("//") && plain_path(dest), "install_on_disk: unusual destination {dest}");
        if let Content::Link(target) = &f.what {
            let t = target.to_str().unwrap_or("");
            assert!(!t.is_empty() && plain_path(t), "install_on_disk: unusual link target {t}");
        }
    }
    let work = disk.with_extension("install");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let result = install_in(&work, disk, files);
    let _ = fs::remove_dir_all(&work);
    result
}

/// The disk's version of `f`, read into `at` (as `read_back` asked debugfs
/// to): whether it is what `f` puts there.
fn same_install(f: &Install, at: &Path) -> io::Result<bool> {
    match &f.what {
        Content::File(src) => Ok(at.exists() && same_contents(at, src)?),
        // (rdump made the link inside the directory `at`.)
        Content::Link(target) => {
            let name = f.dest.rsplit('/').next().expect("a name");
            Ok(fs::read_link(at.join(name)).is_ok_and(|t| &t == target))
        }
    }
}

/// The debugfs command that reads the disk's version of `f` into `at` (a
/// name in the work directory): a file's contents with `dump`, a link
/// itself with `rdump` into a directory made for it.
fn read_back(f: &Install, at: &Path) -> io::Result<String> {
    let name = at.file_name().and_then(|n| n.to_str()).expect("a plain name");
    Ok(match &f.what {
        Content::File(_) => format!("dump /{} {name}\n", f.dest),
        Content::Link(_) => {
            fs::create_dir_all(at)?;
            format!("rdump /{} {name}\n", f.dest)
        }
    })
}

fn install_in(work: &Path, disk: &Path, files: &[Install]) -> io::Result<()> {
    // debugfs reads source i as `new<i>` and reads the disk's version into
    // `old<i>`, then what it wrote into `check<i>` (names of its own, in `work`).
    let mut dumps = String::new();
    for (i, f) in files.iter().enumerate() {
        if let Content::File(src) = &f.what {
            std::os::unix::fs::symlink(fs::canonicalize(src)?, work.join(format!("new{i}")))?;
        }
        dumps += &read_back(f, &work.join(format!("old{i}")))?;
    }
    fs::write(work.join("dumps"), dumps)?;
    debugfs(work, disk, &["-f", "dumps"])?;
    let mut changed = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if !same_install(f, &work.join(format!("old{i}")))? {
            changed.push(i);
        }
    }
    println!("{} of {} files and links to write on {}", changed.len(), files.len(), disk.display());
    if changed.is_empty() {
        return Ok(());
    }
    // debugfs goes on after a failing command: the directories that exist
    // already and a missing old version are no errors here. (Sorted, a
    // directory comes before what it holds.)
    let mut dirs = std::collections::BTreeSet::new();
    for &i in &changed {
        let dest = &files[i].dest;
        let mut dir = String::new();
        for part in dest.split('/').take(dest.matches('/').count()) {
            dir = format!("{dir}/{part}");
            dirs.insert(dir.clone());
        }
    }
    let mut script: String = dirs.iter().map(|d| format!("mkdir {d}\n")).collect();
    let mut checks = String::new();
    for &i in &changed {
        let Install { what, dest, mode } = &files[i];
        script += &format!("rm /{dest}\n");
        match what {
            Content::File(_) => {
                script += &format!("write new{i} /{dest}\n");
                script += &format!("sif /{dest} mode 0100{:o}\n", mode & 0o7777);
            }
            Content::Link(target) => script += &format!("symlink /{dest} {}\n", target.display()),
        }
        script += &format!("sif /{dest} uid 0\nsif /{dest} gid 0\n");
        checks += &read_back(&files[i], &work.join(format!("check{i}")))?;
    }
    fs::write(work.join("cmds"), script)?;
    fs::write(work.join("checks"), checks)?;
    let wrote = debugfs(work, disk, &["-w", "-f", "cmds"])?;
    // Each file and link must now be there as its source says.
    debugfs(work, disk, &["-f", "checks"])?;
    for &i in &changed {
        if !wrote.status.success() || !same_install(&files[i], &work.join(format!("check{i}")))? {
            let log = String::from_utf8_lossy(&wrote.stderr);
            return Err(io::Error::other(format!("/{} is not on the disk as it should be after debugfs:\n{log}", files[i].dest)));
        }
    }
    Ok(())
}

/// The dynamically linked programs (userspace/dynamic/default.nix, ADR 0021),
/// built by Nix and kept from garbage collection by the link target/dynamic:
/// their store path.
fn dynamic_programs() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let link = root.join("target/dynamic");
    let status = Command::new("nix-build")
        .arg(root.join("userspace/dynamic/default.nix"))
        .arg("-o")
        .arg(&link)
        .stdout(process::Stdio::null())
        .status()
        .expect("Failed to run nix-build for userspace/dynamic");
    if !status.success() {
        eprintln!("nix-build userspace/dynamic failed");
        process::exit(2);
    }
    fs::canonicalize(&link).expect("nix-build made no target/dynamic")
}

/// The dynamic programs in the root filesystem: `dynamic/rootfs` copied
/// into it, links as links (the programs in /bin, links into the store, and
/// /nix, a link to /data/nix, where the data disk holds the store paths they
/// need, `store_files`, under their own names: the interpreters and the
/// libraries are found as Nix built the programs, unmodified).
fn install_dynamic(rootfs: &Path, dynamic: &Path) -> io::Result<()> {
    fn copy(from: &Path, to: &Path) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let target = to.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
            } else if kind.is_dir() {
                fs::create_dir_all(&target)?;
                copy(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }
    copy(&dynamic.join("rootfs"), rootfs)
}

/// What the store paths `dynamic/closure` lists (files, links or whole
/// directories) put on the data disk, each under its own path (as
/// `nix/store/...`): files with their execute bits, links as links, with
/// what they point to (within the store) as well.
fn store_files(dynamic: &Path) -> io::Result<Vec<Install>> {
    fn walk(path: &Path, out: &mut Vec<Install>, seen: &mut std::collections::BTreeSet<PathBuf>) -> io::Result<()> {
        if !seen.insert(path.to_path_buf()) {
            return Ok(());
        }
        assert!(path.starts_with("/nix/store/"), "closure: {} is not in the store", path.display());
        let dest = path.strip_prefix("/").expect("absolute").to_str().expect("UTF-8 store path").to_string();
        let meta = fs::symlink_metadata(path)?;
        if meta.is_symlink() {
            let target = fs::read_link(path)?;
            // (Relative to the link's directory; resolved by its components, so
            // that each link of a chain is installed.)
            let mut to = path.parent().expect("in a directory").to_path_buf();
            for part in target.components() {
                match part {
                    std::path::Component::RootDir => to = PathBuf::from("/"),
                    std::path::Component::ParentDir => {
                        to.pop();
                    }
                    std::path::Component::CurDir => {}
                    std::path::Component::Normal(p) => to.push(p),
                    std::path::Component::Prefix(_) => unreachable!(),
                }
            }
            out.push(Install { what: Content::Link(target), dest, mode: 0o777 });
            walk(&to, out, seen)
        } else if meta.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                walk(&e.path(), out, seen)?;
            }
            Ok(())
        } else {
            let mode = if meta.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 };
            out.push(Install { what: Content::File(path.to_path_buf()), dest, mode });
            Ok(())
        }
    }
    let (mut out, mut seen) = (Vec::new(), std::collections::BTreeSet::new());
    for line in fs::read_to_string(dynamic.join("closure"))?.lines() {
        walk(Path::new(line), &mut out, &mut seen)?;
    }
    Ok(out)
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
