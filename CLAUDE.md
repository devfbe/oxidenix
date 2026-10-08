# oxidenix

Bare-metal x86_64 kernel in Rust (bootloader 0.11, UEFI or BIOS) that boots in QEMU and
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
- `nix-shell -p e2fsprogs --run "cargo test -p ext2fs"` (workspace root) tests the ext2
  library on the host against a RAM disk; CI runs it too. `cargo test -p vfs` tests the pure
  parts of the Linux server's namespace (paths, cpio) on the host, `cargo test -p slab` the
  size-class allocator of the kernel's and the server's heaps, `cargo test --release -p ring`
  the I/O ring's invariants (with threads; release for realistic interleavings).
- QEMU must always run with a visible window; never use `-display none`.
- The image boots via UEFI (OVMF from nixpkgs) by default; `OXIDENIX_FIRMWARE=bios` builds and
  boots a BIOS image instead. CI runs the self-tests with both.
- `cd kernel && OXIDENIX_BUILD_ONLY=1 cargo run` builds the boot image and data disk without
  starting QEMU.

## Engineering standard

- Always recommend and build the technically excellent solution, not the quickest one:
  maximum engineering quality. No stopgaps (e.g. a big kernel lock) when the proper design
  is feasible; say what the excellent solution costs and do it.

## Workflow

- After every change, review the documentation (README.md, CLAUDE.md, code comments that
  describe behavior) and update it in the same commit if it no longer matches.
- Commit in small steps: one commit per finished logical step, tested before committing.
- Push every commit to `main` on GitHub (`origin`, account devfbe) on your own, without
  asking, and without touching the global git config:
  `git -c credential.helper= -c credential.helper='!gh auth git-credential' push`.
- Then continue directly with the next step.
- Parallelize with subagents whenever work splits into independent parts (e.g. separate
  subsystems): run them concurrently, each in its own git worktree, and merge their branches
  into `main` after reviewing and testing the result.
