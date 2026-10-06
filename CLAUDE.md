# rust-kernel

Bare-metal x86_64 kernel in Rust (bootloader 0.11, BIOS) that boots in QEMU and
runs static musl binaries (BusyBox) via a Linux-compatible syscall ABI.

## Language

- **All code is written in English, always**: identifiers, comments, doc
  comments, log/panic/kernel messages, shell output, test programs, scripts,
  rootfs files and commit messages.
- Conversation with the user stays in German.

## Build and run

- Build the kernel from `kernel/` (`cd kernel && cargo build`); building from
  the workspace root lacks the kernel's `build-std`/target config.
- `cd kernel && cargo run` builds the rootfs, the disk image and starts QEMU
  (extra arguments after `--` are passed to QEMU).
- `cargo` lives in `~/.cargo/bin`.
- QEMU must always run with a visible window; never use `-display none`.
