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
    let rootfs = kernel_path.with_extension("rootfs");

    // OXIDENIX_TEST=1: boot straight into the self-tests and let their result
    // become QEMU's exit status (1 = success, 3 = failure); after they passed,
    // boot again with the disk they wrote read-only (QEMU's readonly=on: the
    // device says VIRTIO_BLK_F_RO) into /etc/rotests.sh.
    // OXIDENIX_TEST=readonly: that read-only boot alone, on a fresh disk.
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
    println!("Building root filesystem...");
    build_rootfs(&rootfs).expect("Failed to build root filesystem");
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
    let boots: Vec<Boot> = if let Some(script) = &autorun {
        vec![Boot { suffix: "", autorun: Autorun::Copy(script.clone()), read_only: false }]
    } else if bench_mode {
        vec![Boot { suffix: "", autorun: Autorun::Link("bench.sh"), read_only: false }]
    } else if readonly_only {
        vec![Boot { suffix: "readonly", autorun: Autorun::Script("exec sh /etc/rotests.sh\n"), read_only: true }]
    } else if self_tests {
        vec![
            Boot { suffix: "", autorun: Autorun::Link("runtests.sh"), read_only: false },
            // (The disk the first boot wrote: its last file is checked too.)
            Boot { suffix: "readonly", autorun: Autorun::Script("exec sh /etc/rotests.sh after-rw\n"), read_only: true },
        ]
    } else {
        vec![Boot { suffix: "", autorun: Autorun::None, read_only: false }]
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
    // OXIDENIX_NODE=1 (or the path of a static node binary): Node.js on the
    // data disk as /bin/node (/data/bin/node in oxidenix).
    if let Some(node) = node_binary() {
        println!("Installing {} as /bin/node on {}...", node.display(), data_disk.display());
        install_on_disk(&data_disk, &node, "bin/node").expect("Failed to put node on the data disk");
    }
    // The self-tests' read-only boot reads a file the builder put on the
    // disk (/data/rotest/hello, a copy of /bin/hello).
    if self_tests {
        install_on_disk(&data_disk, &rootfs.join("bin/hello"), "rotest/hello").expect("Failed to put rotest/hello on the data disk");
    }

    // Most of a debug build is DWARF debug information the kernel never
    // uses, and the BIOS bootloader copies the whole ELF through real-mode
    // disk reads (about 1 MB/s under QEMU). Boot a stripped copy; the full
    // ELF stays next to it for debuggers.
    let boot_kernel = strip_debug_info(kernel_path);

    // An initramfs and a boot image per boot (`<kernel>.img`, the read-only
    // boot's `<kernel>.readonly.img`): the same root filesystem with its own
    // /etc/autorun.
    let firmware = Firmware::from_env();
    let mut images = Vec::new();
    for boot in &boots {
        let name = |ext: &str| if boot.suffix.is_empty() { kernel_path.with_extension(ext) } else { kernel_path.with_extension(format!("{}.{ext}", boot.suffix)) };
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
        code = qemu(firmware, kernel_path, img, &data_disk, boot.read_only, test_mode, &args[2..]);
        if test_mode && code != 1 {
            break;
        }
    }
    process::exit(code);
}

/// One boot of a run: its images' suffix (none for the first), what its
/// /etc/autorun is, and whether it has the data disk read-only.
struct Boot {
    suffix: &'static str,
    autorun: Autorun,
    read_only: bool,
}

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

/// Boots `img` in QEMU with `data_disk` (read-only: QEMU's readonly=on,
/// which virtio-blk reports as VIRTIO_BLK_F_RO) and the user's `extra`
/// arguments; QEMU's exit status.
fn qemu(firmware: Firmware, kernel_path: &Path, img: &Path, data_disk: &Path, read_only: bool, test_mode: bool, extra: &[String]) -> i32 {
    let mode = if read_only { ", the data disk read-only" } else { "" };
    println!("Starting QEMU ({}) with {}{}", firmware.name(), img.display(), mode);
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
        if text.starts_with("[autorun] ") && !text.starts_with("[autorun] running") {
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

/// Puts the host file `src` on the ext2 image `disk` as `dest` (relative to
/// its root, in an existing or new directory), mode 0755 and owned by root,
/// replacing an older version; nothing if the disk has these contents there
/// already. debugfs edits the image in place, so the rest of the persistent
/// disk stays as it is. Afterwards the file is read back and compared.
fn install_on_disk(disk: &Path, src: &Path, dest: &str) -> io::Result<()> {
    // `dest` goes into debugfs's commands: a plain relative path only.
    assert!(
        !dest.is_empty() && dest.split('/').all(|c| !c.is_empty() && c != ".." && c.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
        "install_on_disk: unusual destination {dest}"
    );
    let work = disk.with_extension("install");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let result = install_in(&work, disk, &fs::canonicalize(src)?, dest);
    let _ = fs::remove_dir_all(&work);
    result
}

fn install_in(work: &Path, disk: &Path, src: &Path, dest: &str) -> io::Result<()> {
    // debugfs reads the source as `new` and dumps the disk's file as `old`.
    std::os::unix::fs::symlink(src, work.join("new"))?;
    let dump = format!("dump /{dest} old");
    debugfs(work, disk, &["-R", &dump])?;
    if work.join("old").exists() && same_contents(&work.join("old"), src)? {
        println!("/{dest} on {} is up to date", disk.display());
        return Ok(());
    }
    let mut script = String::new();
    // debugfs goes on after a failing command: the directories that exist
    // already and a missing old version are no errors here.
    let mut dir = String::new();
    for part in dest.split('/').take(dest.matches('/').count()) {
        dir = format!("{dir}/{part}");
        script += &format!("mkdir {dir}\n");
    }
    script += &format!("rm /{dest}\nwrite new /{dest}\n");
    script += &format!("sif /{dest} mode 0100755\nsif /{dest} uid 0\nsif /{dest} gid 0\n");
    fs::write(work.join("cmds"), script)?;
    let wrote = debugfs(work, disk, &["-w", "-f", "cmds"])?;
    // The file must now be there with the source's contents.
    let check = format!("dump /{dest} check");
    debugfs(work, disk, &["-R", &check])?;
    if !wrote.status.success() || !work.join("check").exists() || !same_contents(&work.join("check"), src)? {
        let log = String::from_utf8_lossy(&wrote.stderr);
        return Err(io::Error::other(format!("/{dest} is not on the disk as it should be after debugfs:\n{log}")));
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
