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
- All Nix packages (musl toolchain, Bash, BusyBox, OVMF, e2fsprogs) come from the nixpkgs pinned
  in `nix/nixpkgs.nix`; the builder, `userspace/build.sh`, `scripts/bench.sh` and CI set
  `NIX_PATH=nixpkgs=nix/nixpkgs.nix`. Use the same for manual Nix calls (the host ext2 test:
  `NIX_PATH=nixpkgs=$PWD/nix/nixpkgs.nix nix-shell -p e2fsprogs --run ...`).
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

## Finding code

- `docs/codemap.md` lists every source file with a one-sentence summary and its public types,
  the Linux server's kernel ABI (call number → handling match arm) and all documents. Read it
  before searching the tree. It is generated: run `python3 scripts/codemap.py` after adding,
  removing or re-documenting files (CI fails if it is stale); every source file starts with a
  module comment (`//!` or `/* */`) whose first sentence says what the file is for.
- Entry points by topic:
  - Boot: `builder/src/main.rs` (images, QEMU), `kernel/src/main.rs` (`kernel_main`).
  - System calls: `kernel/src/process/syscall.rs` (entry, dispatch), `sys_*.rs` beside it.
  - Restricted mode and the Linux server's kernel interface: `crates/restricted/src/lib.rs`
    (ABI constants, documented), `kernel/src/process/linux.rs`, `linux_inode.rs`;
    design in `docs/design/linux-server.md`.
  - The Linux server: `servers/linux/src/main.rs` (dispatch order mm → time → files → paths →
    pass-through), `namespace.rs`/`paths.rs` (paths, mounts), `tmpfs.rs`/`tmpfile.rs` (root fs).
  - Memory: `kernel/src/memory/`, `kernel/src/process/address_space.rs`, page cache
    `kernel/src/fs/cache.rs`.
  - Other servers and their protocols: `servers/diskfs` + `crates/fsproto` + `crates/ext2fs`,
    `servers/netd` + `crates/netproto`, `servers/procfs` + `crates/procproto`; I/O rings
    `crates/ring`, `docs/design/io-rings.md`.
  - Tests: C programs in `userspace/*.c` (built by `userspace/build.sh`), run by the list in
    `userspace/rootfs/etc/runtests.sh`; benchmarks `userspace/iobench.c`, `scripts/bench.sh`,
    `docs/benchmarks/README.md`. Decisions: `docs/decisions/`.

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
