# oxidenix

Bare-metal x86_64 kernel in Rust (bootloader 0.11, BIOS) that boots in QEMU and
runs static musl binaries (Bash, BusyBox) via a Linux-compatible syscall ABI.
An AI research project; see README.md.

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
- `cd kernel && OXIDENIX_TEST=1 cargo run` runs all self-tests (`/etc/runtests.sh`) and
  exits QEMU with 1 on success, 3 on failure; CI runs the same on every push.
- QEMU must always run with a visible window; never use `-display none`.

## Workflow

- Commit every finished, tested step without asking and continue directly
  with the next step.
- After every commit, push to GitHub (`origin`, account devfbe) without touching the
  global git config: `git -c credential.helper= -c credential.helper='!gh auth git-credential' push`.
