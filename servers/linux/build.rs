//! Builds the vDSO (`vdso/vdso.rs`, ADR 0014) the server embeds: rustc compiles it for the
//! host's Linux target into one position-independent object (the prebuilt `core` of the
//! toolchain's host target; nothing of it is linked, every helper inlines), rust-lld links
//! it into a shared object with `vdso/vdso.lds` and `vdso/vdso.map` (`linux-vdso.so.1`,
//! the `LINUX_2.6` version, SysV and GNU hash tables, no symbol table beyond the dynamic
//! one), and the result is checked: one read-only, executable PT_LOAD at address 0 and no
//! relocation of any kind, since nothing relocates the vDSO in a program. The same
//! sources and toolchain give the same bytes (no build id, no debug information, no
//! paths).

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let source = dir.join("vdso/vdso.rs");
    let script = dir.join("vdso/vdso.lds");
    let versions = dir.join("vdso/vdso.map");
    let clockpage = dir.join("../../crates/clockpage/src/lib.rs");
    for input in [&source, &script, &versions, &clockpage] {
        println!("cargo:rerun-if-changed={}", input.display());
    }

    // The toolchain's own linker, next to the host target's libraries.
    const TARGET: &str = "x86_64-unknown-linux-gnu";
    let sysroot = run(Command::new(&rustc).args(["--print", "sysroot"]));
    let lld = Path::new(sysroot.trim()).join("lib/rustlib").join(TARGET).join("bin/rust-lld");

    let object = out.join("vdso.o");
    let image = out.join("vdso.so");
    run(Command::new(&rustc)
        .args(["--edition", "2021", "--crate-type", "lib", "--crate-name", "vdso", "--target", TARGET])
        .args(["-C", "opt-level=2", "-C", "panic=abort", "-C", "relocation-model=pic", "-C", "codegen-units=1"])
        .args(["-C", "debuginfo=0", "-C", "target-cpu=x86-64"])
        .arg(format!("--remap-path-prefix={}=vdso", dir.display()))
        .arg("--emit")
        .arg(format!("obj={}", object.display()))
        .arg(&source));
    run(Command::new(&lld)
        .args(["-flavor", "gnu", "-shared", "-soname", "linux-vdso.so.1", "--hash-style=both", "--eh-frame-hdr"])
        .args(["-z", "max-page-size=4096", "-z", "noexecstack", "--no-undefined", "--build-id=none", "-Bsymbolic", "--strip-all"])
        .arg("-T")
        .arg(&script)
        .arg("--version-script")
        .arg(&versions)
        .arg("-o")
        .arg(&image)
        .arg(&object));
    check(&std::fs::read(&image).expect("the linked vDSO"));
}

/// Runs `cmd` and returns its standard output; any failure ends the build.
fn run(cmd: &mut Command) -> String {
    let output = cmd.output().unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"));
    if !output.status.success() {
        panic!("{cmd:?} failed:\n{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The image is what the server's exec maps as it is: an ELF64 shared object with one
/// PT_LOAD from offset and address 0, readable and executable, not writable, and a dynamic
/// section without relocations or text relocations.
fn check(elf: &[u8]) {
    let u16_at = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(elf[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(elf[o..o + 8].try_into().unwrap());
    assert!(elf.len() > 64 && &elf[..4] == b"\x7fELF" && elf[4] == 2 && u16_at(16) == 3, "vDSO: not an ELF64 shared object");
    let (phoff, phentsize, phnum) = (u64_at(32) as usize, u16_at(54) as usize, u16_at(56) as usize);
    let mut loads = 0;
    let mut dynamic = None;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        match u32_at(p) {
            1 => {
                loads += 1;
                assert!(u64_at(p + 8) == 0 && u64_at(p + 16) == 0, "vDSO: its PT_LOAD must start at offset and address 0");
                assert!(u32_at(p + 4) == 5, "vDSO: its PT_LOAD must be readable and executable only");
                assert!(u64_at(p + 32) == u64_at(p + 40), "vDSO: no zero-filled memory");
            }
            2 => dynamic = Some((u64_at(p + 8) as usize, u64_at(p + 32) as usize)),
            _ => {}
        }
    }
    assert!(loads == 1, "vDSO: one PT_LOAD, found {loads}");
    let (at, len) = dynamic.expect("vDSO: no PT_DYNAMIC");
    const DT_RELA: u64 = 7;
    const DT_REL: u64 = 17;
    const DT_TEXTREL: u64 = 22;
    const DT_JMPREL: u64 = 23;
    const DT_RELR: u64 = 36;
    for entry in elf[at..at + len].chunks_exact(16) {
        let tag = u64::from_le_bytes(entry[..8].try_into().unwrap());
        assert!(![DT_RELA, DT_REL, DT_TEXTREL, DT_JMPREL, DT_RELR].contains(&tag), "vDSO: it has relocations (dynamic tag {tag})");
    }
}
