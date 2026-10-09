<div align="center">

# oxidenix

**A Unix-like x86_64 kernel written from scratch in Rust, designed, implemented and debugged by an AI.**

It boots in QEMU and runs an unmodified, statically linked **GNU Bash 5.3** and **BusyBox**
on top of a Linux-compatible system call interface. It is moving towards a **microkernel**: the
disk driver and the ext2 filesystem already run as a user-space server.

[![test](https://github.com/devfbe/oxidenix/actions/workflows/test.yml/badge.svg)](https://github.com/devfbe/oxidenix/actions/workflows/test.yml)
![Rust](https://img.shields.io/badge/language-Rust%20(nightly)-orange?logo=rust)
![Arch](https://img.shields.io/badge/arch-x86__64-blue)
![Boot](https://img.shields.io/badge/boot-UEFI%20%2B%20BIOS%20via%20bootloader%200.11-lightgrey)
![Userland](https://img.shields.io/badge/userland-Bash%205.3%20%2B%20BusyBox-green)
![Status](https://img.shields.io/badge/status-research%20project-purple)
![License](https://img.shields.io/badge/license-GPL--3.0-blue)

![oxidenix booting in QEMU and running a few commands in GNU Bash](docs/images/oxidenix-demo.gif)

</div>

---

## About this project

oxidenix is a **research project about AI-driven systems programming**. The question behind it:
*how far can an AI coding agent get when it is asked to build an operating system kernel from an
empty directory, with a human only setting goals?*

- **Every line of code** in this repository (kernel, build tooling, test programs, this README)
  was written by **Claude** (Anthropic), working as an agent in **Claude Code**.
- The human collaborator set the direction in short sentences ("I want a minimal shell",
  "make Bash run", "do copy-on-write, then job control"), chose between design options and
  tested the result by typing into the QEMU window.
- The AI did the design, the implementation and the debugging. Debugging included reading
  QEMU monitor dumps (`info registers`, `info pic`), disassembling crash addresses with
  `addr2line`/`objdump`, and driving the guest through the QEMU monitor (`sendkey`,
  `screendump`) to look at screenshots of its own output.
- Automated security reviews ran on every commit. The kernel panics, overflows, deadlocks and
  resource exhaustion paths they found were fixed in follow-up commits (see
  [Security](#security)).

The project went from an empty directory to an interactive Bash in one long working session.
Its [commit history](#development-history) records every step.

## What it can do

- **Boots** via UEFI (OVMF) or BIOS into a 64-bit higher-half kernel with a framebuffer text console.
- **Runs real Linux binaries**: static musl executables such as GNU Bash 5.3 (with readline),
  BusyBox 1.37 (~400 applets: `ls`, `cat`, `grep`, `sed`, `dd`, `vi`, ...) and its own C test
  programs, all unmodified. **Node.js 24** (a static musl build, opt-in on the data disk, see
  [Build and run](#build-and-run)) runs scripts with V8's JIT, timers, files, worker threads,
  HTTP over loopback, DNS and WebAssembly, child processes (`spawn`, `exec`, `execSync`,
  `fork` with its IPC channel, also passing sockets and servers to the child) and `net`
  servers and clients on Unix paths.
- **Interactive shell** with line editing, history (arrow keys), tab completion, colors, pipes,
  redirections, subshells, command substitution and arithmetic.
- **Symmetric multiprocessing**: all CPUs (QEMU runs with 4) execute user programs and the
  kernel in parallel, with fine-grained locks instead of a big kernel lock. CPU-bound programs
  scale (about 3x on 4 emulated CPUs), and `sched_setaffinity` pins processes to CPUs.
- **Preemptive multitasking**: round-robin scheduler at 100 Hz, separate address spaces,
  `fork` with **copy-on-write**, `execve`, `wait4`, process groups and sessions.
- **Threads**: `clone` with Linux's sharing flags, so musl's pthreads work unchanged (mutexes
  and condition variables on `futex`, thread-local storage, join, detached threads); threads of
  one process run in parallel on all CPUs, with TLB shootdowns keeping their view of memory
  coherent. Real `vfork` and `posix_spawn`.
- **POSIX signals**: handlers, masks, `kill`, `SIGCHLD`, `EINTR` with automatic syscall restart,
  and **Ctrl+C** interrupting any foreground program, even a busy loop without system calls.
- **Job control**: Ctrl+Z stops the foreground job, then `jobs`, `fg`, `bg` and `kill %n` work
  in Bash; background jobs reading from the terminal are stopped with `SIGTTIN`.
- **htop, ps, top, free, uptime**: `/proc` and the CPU part of `/sys` are served live by a
  user-space server, from the kernel's own process accounting (CPU time per process and CPU,
  memory, load average).
- **Filesystem**: an in-memory, tmpfs-like VFS populated from a cpio initramfs, with files,
  directories, symlinks, `/dev/{console,tty,null,zero}`, and quotas against heap exhaustion.
- **Microkernel-style drivers**: the virtio block driver and the read-write **ext2** filesystem
  run in `diskfs`, an ordinary ring-3 process that talks to the kernel over IPC and reaches the
  disk through I/O ports and a DMA area the kernel granted it. If it dies, the kernel restarts it on the next access, and
  the rest of the system keeps running.
- **Networking**: TCP, UDP and raw ICMP sockets over IPv4 with DNS, so `wget`, `nc`, `ping` and
  `nslookup` from BusyBox reach the Internet through QEMU's user network. The driver for the virtio network
  card and the TCP/IP stack (smoltcp) run in `netd`, a user-space server; the interface is
  configured by DHCP, and loopback (`127.0.0.1`) works too.
- **Persistent storage**: the data disk is mounted at `/data`, survives reboots, and stays
  consistent enough that `e2fsck` on the host accepts it. The Linux server serves it from its
  own page cache, which the disk server fills and writes back by DMA through shared-memory
  rings (write-back, as on Linux: `fsync` makes data durable).
- **Clocks** with nanosecond resolution from the TSC: every Linux clock, exact CPU time per
  thread and process (`getrusage`, `times`), and wall-clock time that starts from the CMOS
  real-time clock and can be set (`date`, file timestamps).
- **Terminal**: a termios line discipline (canonical and raw mode, echo, erase/kill/word-erase,
  EOF), the ANSI escape sequences BusyBox and readline use, a German keyboard layout and UTF-8.
- **~120 Linux system calls**, enough for Bash and BusyBox (see [System calls](#system-calls)).

## Quick start

### Requirements

`nix develop` (or [direnv](https://direnv.net/) with the checked-in `.envrc`: `direnv allow` once)
opens a shell with everything below. Its packages come from the same pinned nixpkgs as the
builder and CI: rustup, QEMU, e2fsprogs, Python, gh and jq, and the Linux and POSIX man pages
(`man 2 openat`). The shell also points
`OXIDENIX_OVMF` at the firmware and `NIX_PATH` at the pin. Without it you need:

- **Rust nightly**, a dated one pinned in `rust-toolchain.toml` with `rust-src`, `llvm-tools-preview` and
  `rust-analyzer` (rustup installs it on first use).
- **QEMU** (`qemu-system-x86_64`). With KVM (`/dev/kvm` accessible) the builder runs the
  guest with hardware virtualization: oxidenix boots in about 2 s; without it QEMU falls back
  to emulation (about 8 s).
- **Nix**: the userland is fetched and cross-compiled from nixpkgs
  (`pkgsStatic.stdenv.cc` for musl, `pkgsStatic.busybox`, `pkgsStatic.bash`).

### Build and run

```sh
cd kernel
cargo run
```

This builds the kernel, assembles the root filesystem (C test programs, Bash, BusyBox and
`userspace/rootfs/`), packs it as a cpio initramfs, creates a UEFI disk image and starts QEMU with the OVMF firmware
from nixpkgs (`OXIDENIX_OVMF=<dir>` uses another directory holding `OVMF_CODE.fd` and
`OVMF_VARS.fd`). `OXIDENIX_FIRMWARE=bios` builds a BIOS image and boots it with SeaBIOS instead.
On the first run it also creates `disk.img`, a 2 GiB ext2 data disk (via `mke2fs` from nixpkgs,
pre-filled from `userspace/disk/`; a sparse file, about 20 MB on the host when new). This file is
kept between runs; delete it for a fresh disk. A `disk.img` from before the disk grew from 64 MiB
to 2 GiB stays 64 MiB (the builder points it out): delete it to get the bigger one.
Extra arguments after `--` are passed to QEMU; `OXIDENIX_BUILD_ONLY=1 cargo run` only builds the images.
`OXIDENIX_DISK=<path>` uses another data disk image (created if missing). Test mode
(`OXIDENIX_TEST=1`) never touches `disk.img`: it boots with a fresh 64 MiB disk of its own,
`target/test-disk.img`, made anew on every run.

`OXIDENIX_NODE=1 cargo run` also puts Node.js on the data disk as `/data/bin/node`: a fully
static musl build (`userspace/node/default.nix`, no npm; its header says which of Node's checks
are off and why). The first build compiles V8 and runs Node's test suites (about an hour on 12
cores); later runs take it from the Nix store (`target/node` keeps it from garbage collection).
`OXIDENIX_NODE=<path>` installs another static node binary instead. The builder writes it into
the existing disk image with `debugfs` (replacing an older version, leaving the rest of the disk
as it is; nothing if the disk has the same binary already), then reads it back and compares it.
Normal runs and CI never build or need Node.

`OXIDENIX_AUTORUN=<script> cargo run` boots like test mode into a shell script of the host's
(copied to `/etc/autorun`): its output and the kernel log go to stdout, its exit status ends
QEMU (1 = 0, 3 = otherwise). For trying a program, for example
`OXIDENIX_NODE=1 OXIDENIX_AUTORUN=run-node.sh cargo run > serial.log` with
`/data/bin/node -e 'console.log(1+1)'` in `run-node.sh`; the kernel logs the calls it does not
implement ("syscall N not implemented"). `/data/bin/node /etc/node-sockets.js` checks child
processes and Unix sockets (`userspace/rootfs/etc/node-sockets.js`).

`scripts/bench.sh` runs the I/O benchmarks (`iobench`) on a fresh disk and records the results
with the commit in `docs/benchmarks/` (see its README); `/proc/counters` counts system calls, IPC
requests and bytes, address space switches, user copies and kernel heap allocations since boot.

The kernel boots straight into Bash. Things to try:

```sh
ls -l /bin | head          # BusyBox applets
cowtest; vmtest; futextest; threadtest; timetest; timertest; polltest; epolltest; sigtest; jobtest; oomtest; fstest; forktest; nettest; smptest # self-tests
nproc; cat /proc/cpuinfo   # 4 CPUs; 'cpus' in the kernel monitor shows their load
wget -O - http://example.com # DNS and HTTP through netd; nslookup and nc work as well
ping -c 3 1.1.1.1          # raw ICMP sockets; oxidenix answers pings itself, too
sh /etc/test.sh            # filesystem, pipes, quotas, rename semantics
sh /etc/disktest.sh        # ext2: big files, directories, truncate, rename, symlinks
echo hello > /data/x       # survives a reboot; df -h shows the disk
sleep 100                  # then press Ctrl+Z, try jobs / bg / fg, then Ctrl+C
exit                       # drops to the built-in kernel monitor ('help', 'ps', 'cpus', 'lspci', 'mem', 'run bash')
```

The window scales when it is resized (`zoom-to-fit`), and Ctrl+Alt+F toggles fullscreen.

## Architecture

### Big picture

```
 ┌──────────────────────────────── user space (ring 3) ────────────────────────────────┐
 │  GNU Bash 5.3   BusyBox 1.37   test programs     │  diskfs server (Rust, no_std)    │
 │  statically linked against musl libc             │  ext2 + virtio-blk, DMA          │
 │                                                  │  netd server: virtio-net, DMA,   │
 │                                                  │  IRQs, smoltcp TCP/IP, sockets   │
 └───────────────────────┬────────────────▲─────────┴────────▲──────────────┬──────────┘
          syscall / fault / IRQ       iretq            IPC receive/reply   in/out
 ┌───────────────────────▼────────────────┴──────────────────┴──────────────│──────────┐
 │  syscall layer   process/syscall.rs ─ sys_file.rs ─ sys_mem.rs ─ signal.rs│          │
 │  IPC             services, synchronous call/receive/reply, async post    │          │
 │  drivers         PCI scan, IRQ lines and DMA areas handed to servers     │          │
 │  processes       scheduler, fork/exec/wait, sleep/wakeup, pgid/sid, ioperm          │
 │  memory          frame allocator (refcounted), heap, address spaces, COW │          │
 │  VFS             memory inodes, remote inodes (fs/remote.rs), pipes, cpio│          │
 │  sockets         Linux socket ABI, forwarded to netd (net.rs)            │          │
 │  terminal        TTY line discipline ─ console (framebuffer, ANSI) ─ keyboard       │
 │  CPU             GDT, TSS + I/O bitmap, IDT, local + I/O APIC (ACPI), SSE│          │
 └──────────────────────────────────────────────────────────────────────────▼──────────┘
      bootloader 0.11 (UEFI/BIOS), QEMU q35, 4 CPUs, 256 MiB RAM, AHCI boot disk, virtio-blk data disk, virtio-net
```

### Repository layout

```
oxidenix/
├── kernel/                      the kernel (no_std, target x86_64-unknown-none)
│   ├── assets/                  boot logo (logo.svg source, rendered logo.png)
│   ├── build.rs                 turns the logo into raw pixels for the kernel
│   └── src/
│       ├── main.rs              entry point, boot configuration, init order
│       ├── smp.rs               per-CPU blocks (GS base), CPU start-up trampoline
│       ├── interrupts/          per-CPU GDT/TSS (gdt.rs), IDT (mod.rs), entry stubs and
│       │                        the trap dispatcher (entry.rs, handlers.rs), local and I/O APIC
│       │                        with timer calibration (apic.rs),
│       │                        exception, timer and keyboard handlers (handlers.rs)
│       ├── memory/              physical frame allocator with refcounts (frame.rs), kernel stacks (kstack.rs),
│       │                        kernel heap and page table access (mod.rs)
│       ├── process/             process lifecycle and syscalls on it (mod.rs),
│       │   ├── task.rs          tasks (threads), thread groups (processes), shared tables
│       │   ├── clone.rs         clone, fork, vfork
│       │   ├── exec.rs          execve, ending the other threads first
│       │   ├── exit.rs          thread and process exit, wait4
│       │   ├── sched.rs         run queues, wait queues, context switch, idle
│       │   ├── poll.rs          waiting on the wait queues of several files (poll, select)
│       │   ├── epoll.rs         epoll: interest and ready lists, nesting
│       │   ├── address_space.rs areas, demand paging, copy-on-write
│       │   ├── tlb.rs           which CPUs use an address space, TLB shootdowns
│       │   ├── syscall.rs       syscall entry/return, dispatch table
│       │   ├── sys_file.rs      file, directory, pipe, tty-ioctl, poll/select
│       │   ├── sys_mem.rs       brk, mmap, mprotect, mremap
│       │   ├── sys_net.rs       socket syscalls (sockaddr_in, msghdr, options)
│       │   ├── signal.rs        signal state, delivery, sigreturn, kill
│       │   ├── futex.rs         futex wait queues keyed by address space or page cache
│       │   ├── loader.rs        ELF segments mapped from the page cache, the Linux initial stack
│       │   ├── elf.rs           ELF64 parser
│       │   ├── ipc.rs           services and message passing
│       │   ├── irq.rs           device interrupts for user-space drivers
│       │   └── uaccess.rs       copies to and from user memory
│       ├── net.rs               socket client: operations become netd requests
│       ├── fs/                  VFS (mod.rs), page cache (cache.rs), open files and
│       │                        pipes (file.rs), initramfs unpacker (cpio.rs), IPC
│       │                        client for filesystem servers (remote.rs)
│       ├── drivers/             framebuffer console (console.rs, glyphs.rs), TTY (tty.rs),
│       │                        PS/2 keyboard (keyboard.rs), CMOS clock (rtc.rs),
│       │                        serial port mirror (serial.rs), PCI scan (pci.rs),
│       │                        ACPI MADT (acpi.rs)
│       ├── sync.rs              IrqSpinLock: fair, interrupt-safe ticket lock
│       ├── time.rs              TSC clock source, clock synchronization between CPUs
│       ├── timer.rs             per-CPU timer queues on the local APIC timer
│       └── shell/               built-in kernel monitor (fallback shell)
├── servers/
│   ├── linux/                   the Linux server (restricted mode; memory, time, pipes, eventfd, paths, tmpfs, /data, AF_UNIX)
│   ├── diskfs/                  user-space ext2 server with its virtio-blk driver (blk.rs)
│   ├── procfs/                  /proc and /sys from the kernel's process information
│   │                            (main.rs: tree and inodes, render.rs: Linux formats)
│   ├── netd/                    network server: virtio-net driver (virtio_net.rs), loopback
│   │                            (nic.rs), sockets on smoltcp (service.rs), DHCP
│   └── ringtest/                the self-tests' channel service (test mode only)
├── crates/
│   ├── ext2fs/                  ext2 as a library over a `Device` trait
│   ├── fsproto/                 message format between the VFS and filesystem servers
│   ├── netproto/                socket operations between the kernel and netd
│   ├── procproto/               native process and system information for procfs
│   ├── virtio/                  virtio legacy PCI transport and virtqueues (diskfs, netd)
│   ├── restricted/              restricted mode: shared region layout, register page, kernel calls
│   ├── ring/                    SPSC descriptor rings and the channel layout (I/O rings)
│   ├── fsring/                  the file protocol over the rings (Linux server <-> diskfs)
│   └── oxrt/                    runtime for servers: entry, syscalls, heap, port I/O
├── builder/                     host tool: rootfs + cpio + boot image + ext2 data disk + QEMU
└── userspace/                   C test programs, build script, rootfs and data disk templates
```

About 9,200 lines of Rust (without comments and blank lines) in the kernel and 3,200 in the servers, their libraries and runtime, plus a small host-side builder.

### Boot sequence

1. The **bootloader** (`bootloader` 0.11, as a UEFI application under OVMF or from the BIOS)
   loads the position-independent kernel ELF (the builder hands it a copy without debug
   information: 1.5 MB instead of 16 MB, since the BIOS path reads at about 1 MB/s under
   emulation) into
   the upper half (`dynamic_range_start = 0xffff_8000_0000_0000`). It maps all physical
   memory at a dynamic offset, sets up a framebuffer (UEFI GOP or VESA) and loads the initramfs as a ramdisk.
2. `kernel_main` runs these steps in order: framebuffer console and boot logo → GDT/TSS/IDT
   (interrupts still off) → frame allocator and 16 MiB kernel heap → ACPI tables (MADT), the
   local APIC with its calibrated timer and the I/O APIC (the 8259 PICs are masked) → the TSC
   clock (see Time) → VFS from the cpio ramdisk → process subsystem (SSE, syscall MSRs, process 0) → the other
   CPUs (INIT and STARTUP IPIs; each runs a real-mode trampoline from a page below 1 MiB into
   long mode, then compares its TSC with the bootstrap CPU's and sets up its GDT, TSS, GS
   block, local APIC timer and idle task) →
   **interrupts on**.
3. The kernel starts the servers: `/sbin/diskfs` asks for its I/O ports, mounts the ext2 disk
   and registers as service `diskfs` for channels (each Linux server instance connects one and
   mounts the disk at `/data`). Then the kernel
   scans PCI for a virtio network card and starts `/sbin/netd` with its ports, interrupt line
   and a DMA area; netd registers as service `net` once DHCP has configured the interface
   (or after three seconds without an answer). Last, `/sbin/procfs` provides `/proc` and
   `/sys`, replacing the static `/proc` files of the early boot.
4. Process 0 (the kernel monitor) spawns `/bin/bash` as the foreground process and waits for
   it. If Bash exits, the monitor takes over the terminal.

### Memory

| Region | Address | Notes |
|---|---|---|
| User ELF image | from `0x40_0000` | static `ET_EXEC` binaries, one area per segment with its rights (read, write, execute), mapped from the file's page cache |
| `brk` heap | after the highest segment | one area that grows and shrinks; never over another mapping |
| `mmap` area | below `0x3000_0000_0000` | free gaps searched top-down; anonymous, shared and file mappings |
| User stack | below `0x3fff_ffff_f000` | starts at 256 KiB (Linux initial stack: argc, argv, envp, auxv), grows on demand to 8 MiB |
| (reserved) | `0x4000_0000_0000` – `0x7fff_ffff_ffff` | the Linux server's shared region (`docs/design/linux-server.md`): programs map nothing above 64 TiB (ADR 0003) |
| Kernel image, stacks, boot info, framebuffer, physical memory map | upper half, chosen by the bootloader | shared by all address spaces |
| Kernel heap | `0xffff_c000_0000_0000` | starts at 8 MiB and grows on demand (up to 1 GiB virtual) |

- **Frame allocator**: a bump allocator over the usable regions of the boot memory map, plus a
  free list threaded through the freed frames themselves. Every frame carries a **reference
  count**; freeing drops one reference.
- **Address spaces**: each process has its own level-4 table. The lower half belongs to the
  process, the upper half entries are copied from the kernel's table. Dropping an address space
  walks the lower half and releases every table and frame.
- **Areas (VMAs)** (`process/address_space.rs`): each address space keeps its areas (start,
  end, rights, backing) next to its page tables. Backings: zero-filled private memory, file
  pages (shared or private) and device memory (DMA). Anonymous shared memory is an unnamed
  tmpfs file, so every shared mapping is a file mapping. `mmap`, `munmap`, `mprotect` and
  `mremap` cut and join areas at page granularity.
- **Demand paging**: mapping creates an area, not pages. The first access faults; the handler
  checks the area's rights (else `SIGSEGV`), then maps a zeroed frame or the file's page from
  the page cache (`SIGBUS` beyond the end of the file). An access just below a stack grows it,
  up to 8 MiB. Kernel accesses to user memory fault pages in the same way first.
- **Page cache** (`fs/cache.rs`, design in `docs/design/page-cache.md`): the pages of a regular
  file live in frames that `read`, `write` and every mapping share, so a store through a shared
  mapping is visible to `read` and to other processes at once, and `write` is visible in their
  mappings. A private mapping maps the cache's frame copy-on-write and sees the file until it
  writes a page. Each cache knows which address spaces map it: shrinking a file removes the
  pages beyond the new end from all of them, private copies included, so later accesses raise
  `SIGBUS` as on Linux. A shared mapping of a file opened read-only cannot become writable
  (`EACCES`).
- **Programs are mapped, not copied**: `execve` maps each ELF segment privately from the
  program's page cache (the rest of a data segment's last file page and the bss are zeroed), so
  pages are read on first use and every process running a program shares the ones it does not
  write: eight `busybox sleep` take 0.9 MB instead of 10.6 MB. Since the running program's pages
  are the file's pages, a program cannot be opened for writing (or truncated) while it runs, nor
  run while it is open for writing (`ETXTBSY`), as on Linux. A mapping made through a descriptor
  open for writing keeps that right after the descriptor is closed, as on Linux, so a program
  cannot be changed through a shared mapping while it runs. Servers run from a private copy
  taken at boot.
- **Disk files are cached** (the cached store): a file on `/data` is a *cached object* of the
  Linux server's page cache (below, `/data` in the server): the kernel keeps its pages, its
  size and which pages are dirty, the server fills and writes them back through diskfs. As on
  Linux, a file's pages stay cached after it is closed, until reclaim takes them. procfs
  generates its files on every read, so they are never cached.
- **Dirty pages**: a write or a store marks a cached page dirty (`Dirty` in `/proc/meminfo`);
  a shared mapping maps a page read-only until the first store, whose fault marks it dirty and
  makes it writable. Write-back takes the dirty marks of a run of pages, write-protects them in
  every address space that maps them, then has them written; a store in between faults, marks
  the page again and is written next time, so none is lost. Reclaim cannot drop dirty pages:
  above a tenth of the commit limit (or when reclaim finds them in its way) the server is asked
  to write back, above a fifth a storing thread waits for it. A fault that needs a page from
  the server waits for it with the address space unlocked.
- **Protection**: `mprotect` really changes the rights (including `PROT_NONE`, which keeps the
  pages' contents in entries marked by a software bit) and execution is denied by NX unless an
  area is executable, so JIT compilers can write code and then make it executable (W^X).
- **Commit accounting** (like Linux's `overcommit_memory=2`): writable private memory is
  promised when it is mapped, against all usable RAM except the kernel's reserve. Running out is
  an `ENOMEM` from `mmap`, `brk`, `mprotect`, `mremap` or `fork`, not a killed process.
  `PROT_NONE` mappings promise nothing until they become writable, so huge reservations are
  cheap. `MAP_NORESERVE` mappings are not committed as a whole, also after `mprotect` makes them
  writable (as Linux's `VM_NORESERVE`): V8 reserves its code range (512 MiB, more than the
  machine has) that way and makes all of it writable and executable at once. Their pages (and
  those of other areas not committed as a whole) are committed one by one when they get a
  frame of their own (a software bit in the page table entry says so until the page goes); if
  that fails, the process touching it is killed (also when a copy of the kernel or the Linux
  server touches it for the process: the copy ends, then SIGKILL), never one whose memory was
  committed. Cached
  disk pages count against the same limit but give way: when a commit (or a new cache page) would exceed it, clean cached pages that no mapping
  uses are dropped first, visiting the files in turn and giving a page used since the last look
  a second chance. `/proc/meminfo` shows `Committed_AS`, `CommitLimit`, `Cached` (with tmpfs),
  `Shmem` (tmpfs and shared memory) and a `MemAvailable` that includes the droppable pages.
- **Copy-on-write**: `fork` shares all private frames. Writable pages become read-only in both
  processes and are tagged with an OS-available page table bit. A write fault either copies the
  frame or, for the last owner, just restores write access. Shared memory stays shared.
- **Shared address spaces and TLB shootdowns** (`tlb.rs`): an address space (`Mm`) sits behind a
  sleeping lock, so a fault that reads a file page can hold it. Each address space records the
  CPUs that have it loaded; removing, write-protecting or moving mappings sends those CPUs an
  IPI to drop their stale TLB entries, and the frames are freed only afterwards. One shootdown
  runs at a time, and a CPU waiting with interrupts off serves requests addressed to it itself,
  so shooters never deadlock. Unmapping walks only the page tables that exist, so huge sparse
  reservations cost what is mapped in them.
- **PCIDs**: with CPUs that have them (QEMU runs with `-cpu max`), a switch between address
  spaces keeps the TLB: each CPU tags its last six address spaces with process-context ids.
  Address spaces are known by a unique id (never by their page table's frame, which can be
  reused), and every shootdown advances the address space's generation, so a CPU that held its
  entries without having it loaded flushes them when it loads it again.
- **futex** (`futex.rs`): a futex is keyed by the address space and address, or, in shared
  memory, by the file's page cache and offset, so processes meet whatever address they mapped
  it at.
  A waiter compares the word under its hash bucket's lock with a load that never resolves page
  faults (if the page is missing it drops the lock, faults it in and retries), so the check and
  the enqueue are atomic with respect to a waker.
- **Out of memory is an error, not a panic**: the kernel heap grows by mapping more frames
  when an allocation fails. It never shrinks, so each growth lowers the commit limit by as
  much; before it grows, the size classes give back the slabs whose slots are all free
  (`slab::FreeList::reclaim`; the Linux server's heap does the same), so a burst of many
  objects of one size does not keep that memory from all others. User memory (pages, page tables, kernel stacks) may not take the last 16 MiB of RAM,
  which stay reserved for the heap. Large allocations that user space can trigger (kernel
  stacks, file contents, pipe buffers, `execve` arguments) are fallible and return
  `ENOMEM`/`E2BIG`, and at most 256 tasks (threads and zombies) can exist (`EAGAIN` beyond that).
- **Kernel stacks** (`memory/kstack.rs`) are not on the heap: each has a slot in its own
  virtual region, mapped page by page with 64 KiB of unmapped guard below, so an overflow
  faults instead of corrupting a neighbor. A user task's stack is charged to the commit limit
  (so `clone` fails with `ENOMEM` before promised memory runs out), and its frames go back to
  the frame allocator when the task is gone. Freed slots are reused only after one flush of the
  region on every CPU.
- **User memory access** (`uaccess.rs`): the kernel copies to and from user memory only in one
  copy routine, never through references. A page fault in it is handled like the program's own
  (demand paging, copy-on-write); if the access is not allowed, the fault handler resumes at a
  fixup that ends the copy, and the syscall returns `EFAULT`. Reads take data from a file, pipe
  or socket only for the part of the buffer that may be written, so a bad buffer loses nothing. File and socket data passes
  through kernel buffers of 64 KiB, so no lock is ever held while user memory is touched.
- What a context switch saves (FS base, FPU state) lives apart from the process's own state, so
  a task may sleep in a page fault (reading a file page) while it holds its address space.

### Processes and scheduling

The scheduler is built for several CPUs (`process/sched.rs`, design in
[docs/design/smp.md](docs/design/smp.md)); there is no big kernel lock.

- **Tasks and thread groups** (`process/task.rs`): a task is one thread; a process is a
  thread group (`ThreadGroup`) of tasks that share the process id, signal handlers, the
  process's pending signals and interval timer, relations, and the exit status (each with its
  own lock). As on Linux, the rest is shared per clone flag: the address space (`Mm`,
  `CLONE_VM`), the descriptor table (`Files`, `CLONE_FILES`) and the working directory
  (`FsInfo`, `CLONE_FS`) are separate objects that tasks hold references to. A task owns those
  references, its I/O permissions and its `CLONE_CHILD_CLEARTID` word; FPU state and TLS
  pointer are saved per task; scheduling state is atomic. The process table maps thread ids to
  tasks and process ids to thread groups (zombies included); both share one number space, and a
  process's id is its main thread's.
- **Threads** (`clone.rs`, `exit.rs`, `exec.rs`): `clone` checks Linux's flag rules (threads
  need shared handlers, shared handlers need shared memory), sets the new thread's stack, TLS
  (`CLONE_SETTLS`) and id words (`CLONE_PARENT_SETTID`, `CLONE_CHILD_SETTID`). A thread that
  exits clears its `CLONE_CHILD_CLEARTID` word and wakes its futex (that is how
  `pthread_join` works) and leaves no trace; the last thread ends the process, which stays a
  zombie until reaped. `exit_group` and fatal signals end all threads; `execve` in a thread
  first ends the others and takes over the process id. `fork` copies only the calling thread.
  `vfork` (and `clone` with `CLONE_VM|CLONE_VFORK`, as `posix_spawn` uses it) shares the address
  space and suspends the parent until the child execs or exits.
- **One kernel stack per task** (64 KiB, see Memory). A context switch saves callee-saved registers,
  the FPU/SSE state (`fxsave`), the FS base (musl's TLS pointer), and switches CR3, the TSS
  stack and I/O bitmap, and the syscall stack in the CPU block.
- **Per-CPU run queues**, round robin with a 10 ms time slice, ended by a deadline in each
  CPU's timer queue (see Time). A woken task goes to an idle CPU if there is
  one (preferring the CPU it last ran on); the waker claims that CPU atomically and wakes it
  with an IPI, so a burst of new tasks spreads over all idle CPUs. An idle CPU steals work
  from the others. Each task has a CPU affinity mask (`sched_setaffinity`, inherited by
  `fork`) that queueing, stealing and migration respect. Each CPU has an idle task; idling loads the kernel's page table, so an address
  space is only ever active on the CPU running its process.
- **Kernel threads** (`sched::spawn_kernel_thread`) run a kernel function on their own stack,
  without an address space, signals or a pid; the channels' teardown worker is one.
- **`on_cpu`** marks a task whose kernel stack is still in use; a CPU picks no such task (from
  its queue or by stealing) until its previous CPU finished switching away: waiting for it with
  interrupts off let two CPUs wait for each other's outgoing task for good. An exited task's last reference is dropped
  by the next task on that CPU, after the switch, so its stack is freed only then.
- **Sleeping without lost wakeups**: a blocking path first registers on its wait channel
  (`prepare_to_wait`), then checks its condition, then sleeps; a per-task wake lock
  serializes wakeups with the task descheduling itself. Pipes, the TTY, IPC, `wait4`, stops
  and timed sleeps all use this protocol.
- **Waiting on several files** (`process/poll.rs`): a file announces readiness changes on a
  wait queue (a channel of the global queues for pipes, eventfds and the TTY, its own queue
  for an epoll instance). Wait-queue entries are tasks or callbacks; `poll`, `select` and
  `epoll_wait` put one callback on the queue of every file they check, so a wakeup on any
  of them ends the wait at once instead of at a periodic re-check; a wakeup that comes while
  the files are being checked is noted and not lost. Sockets, whose readiness lives in
  netd, wait on two channels: one that netd wakes for the socket (`ipc_notify`) and one
  woken when that netd dies, so a poll sees the error at once.
- **epoll** (`process/epoll.rs`): every interest is a callback on its file's wait queue that
  puts it on the instance's ready list (without allocating: the list keeps room for every
  item, so this works in interrupt context) and wakes the instance's own queue.
  `epoll_wait` asks each listed file for its readiness: level-triggered items that are
  still ready go back to the end of the list (a full `maxevents` rotates through them),
  edge-triggered ones wait for the next wakeup, one-shot ones are disabled until
  `EPOLL_CTL_MOD`. Interests belong to the open file and disappear when its last
  descriptor closes. Instances may watch each other (and be polled) in chains of at most five,
  counted through the whole chain whichever end it grows at, and without cycles (`ELOOP`); regular files are `EPERM`, as on Linux.
- The kernel is non-preemptive: only user code is preempted, and an interrupt in kernel mode
  never schedules. Syscalls nevertheless run with interrupts enabled, so a long syscall does
  not delay timer ticks or device interrupts on its CPU. An interrupt that ends the time
  slice or wakes a task sets the CPU's `need_resched`, and the switch happens at the next
  return to user space (from that interrupt or from the syscall it interrupted), so no
  request is lost. Long kernel work also checks it between pieces (`cond_resched`, as on
  Linux): a console write that redraws the whole screen lets a woken task run. Locks are
  `IrqSpinLock`s (fair tickets, interrupts off while held), and long work under a lock is
  cut into bounded pieces (the console draws at most 64 cells per lock hold).
- New processes start by *returning from a syscall*: their kernel stack is pre-filled with a
  register frame that `user_return` consumes. A `fork` child is the parent's frame with
  `rax = 0`.
- Process groups, sessions and the terminal's foreground group follow POSIX closely enough for
  Bash and BusyBox job handling.

### Time

`time.rs` keeps time with the TSC, which every CPU reads in a few cycles without a system
call into a device:

- **Frequency**: from CPUID leaf 0x15 where the CPU states it, else from KVM's paravirtual
  clock (its published TSC scaling), else measured against the PIT (two intervals, so the
  fixed cost of starting the PIT cancels out). Nanoseconds since boot are
  `(tsc - boot_tsc) * mult >> 32`.
- **Synchronized CPUs**: a starting CPU answers 64 requests from the bootstrap CPU with its
  counter; the answer belongs to the middle of the request's round trip, as when clocks are
  compared across a network. A difference larger than the shortest round trip is corrected
  per CPU, so `CLOCK_MONOTONIC` does not go back when a thread moves to another CPU.
- **Clocks**: `CLOCK_MONOTONIC` (and `RAW`, `COARSE`, `BOOTTIME`: nothing suspends) counts from
  boot; `CLOCK_REALTIME` is the RTC's reading at boot plus that, and `clock_settime` moves its
  starting point. All clocks have a resolution of 1 ns.
- **CPU time**: each context switch adds the time slice to the task's run time (under a
  sequence counter, so other CPUs read it consistently); the current slice counts too. The
  per-thread and per-process CPU clocks (also those of other threads, as
  `pthread_getcpuclockid` encodes them) read this. Timer ticks that hit user or kernel mode
  split the exact sum into user and system time, as on Linux. Exited threads add theirs to the
  process; reaped children add theirs (with their own children's) to the parent, for
  `RUSAGE_CHILDREN` and `times`.
- **Timers** (`timer.rs`): sleeps with a timeout, interval timers and the scheduler tick are
  deadlines in nanoseconds in a queue of the CPU that armed them, and that CPU's local APIC
  interrupts exactly when the first is due: in TSC-deadline mode where the CPU has it, else
  with a one-shot count (its frequency measured against the PIT). `nanosleep`, `poll`,
  `select`, `futex`, `sigtimedwait` and `setitimer` therefore keep microsecond precision.
  Cancelling is lazy (an entry whose sequence number its owner no longer holds is skipped),
  entries hold their owner weakly, and the queues never allocate after start-up: a full
  queue first drops its stale entries, and there is at most one live entry per task and per
  process. An interval timer is not reloaded in its interrupt but when its `SIGALRM` is
  taken (delivered, or returned by `sigtimedwait`), as Linux does for POSIX timers: while
  the signal pends, the timer does not fire again, so even a 1 µs interval cannot keep a CPU
  busy in timer interrupts. Periods missed meanwhile are skipped; an ignored `SIGALRM`
  parks the timer until a handler is installed or a thread waits for it. An interrupt runs
  only the entries due when it starts, so its work is bounded by the queue's size. A timer
  wake-up switches to the woken task at the next return to user space.

### System call path

- **Per-CPU data**: each CPU has a `Cpu` block (`smp.rs`) with its own GDT, TSS (kernel stack,
  I/O permission bitmap, double-fault stack) and entry scratch space. In the kernel, the GS base
  points to it; every entry from ring 3 executes `swapgs` and every return to ring 3 swaps back
  (the user's GS base is always 0).
- `syscall` enters `syscall_entry`, which switches to the running task's kernel stack (from the
  `Cpu` block) and builds a **22-word frame**: all general-purpose registers, the vector and
  error code, and `rip, cs, rflags, rsp, ss`. That tail is exactly what the CPU pushes on an
  interrupt from ring 3.
- **Every interrupt and exception** (except NMI and double fault) enters through a per-vector
  stub (`interrupts/entry.rs`) that builds the same frame and calls one dispatcher, `trap`. CPU
  exceptions in user code become signals as on Linux (`SIGSEGV`, `SIGFPE`, `SIGILL`, `SIGBUS`,
  `SIGTRAP`); they cannot be blocked or ignored, but a handler can catch them. In the kernel
  they panic with the faulting address. In test mode a panic ends QEMU with a failure.
- All paths return through `user_return`, which uses **`iretq`** (not `sysret`). A signal can
  therefore interrupt user code at any instruction and `rt_sigreturn` restores every register
  exactly, and the classic `sysret` non-canonical-address problem cannot occur.
- The dispatch table maps Linux x86_64 syscall numbers to Rust functions returning
  `Result<i64, errno>`.

### Restricted mode and the Linux server

The kernel is moving its Linux implementation out into a user-space **Linux server**
(`servers/linux`; `docs/design/linux-server.md`, ADRs 0001-0004 in `docs/decisions/`). Phase
R1 put the mechanism in place, with every system call passed through; the phases since have
moved memory, time, pipes, eventfd, paths, the root tmpfs, `/data` and `AF_UNIX` sockets into
the server.

- Each process tree started by the kernel (`/init`, the monitor's `run`) gets an **instance**
  of the server: the server's program and, per thread, a stack and the page with the program's
  registers, in a **shared region** at 64 TiB (PML4 slot 128) that every process of the tree
  shares (`process/linux.rs`). A Linux process's address space has two top-level tables over
  the same lower tables: the program's view (slots 0-127) and the **normal view**, which adds
  the shared region. Programs never see the server's memory.
- A thread starts in the server, which calls `restricted_enter` (1010): the kernel keeps the
  server's registers, loads the program's from the thread's register page and switches to the
  program's view. The program's `syscall` traps back: its registers go to the register page,
  the server's return with the reason, and the normal view is loaded. Both switches keep the
  TLB (PCIDs); the server uses neither the FPU nor the FS/GS bases, so only general registers
  move.
- In phase R1 the server hands every Linux system call back with `legacy_syscall` (1011), which
  runs the kernel's implementation on the registers in the register page (signal frames and
  `execve` included). Until the server takes over signals (R8), `rt_sigreturn` is such a pass-through
  call, so a signal's round trip costs more than before (three kernel entries): an interval
  timer of a microsecond or two whose handler runs every time can now starve its program, as
  it would on any system where the round trip outlasts the interval. Numbers of 1000 and above are not Linux's: the server answers them with
  `ENOSYS` itself. Faults and exceptions of the program are still the kernel's; an exception in
  the server kills its process with a diagnostic.
- `fork` and `clone` give the new thread its own server thread in the same instance; `execve`
  keeps the process in its instance. Servers such as diskfs are not Linux programs and keep the
  kernel's interface.
- **Kernel objects by handle** (phase R2): each instance has a handle table, so an object can be
  used from any thread of its process tree. Memory objects (zero-filled, committed; the kernel's
  page cache objects) are created, read and written from the server's memory, and mapped shared
  or private into the calling thread's program view, protected and unmapped. Addresses the server
  passes for its own memory must lie in its shared region; mappings must lie below 64 TiB.
  A legacy call runs in the program's view, where the server's memory does not exist.
- **Paged memory objects**: the server supplies their pages. Each instance has a **pager
  process** (`linux-pager`: the server's view alone, protected like the servers) whose thread
  waits for page requests (`pager_wait`) and answers them (`mo_supply`). A thread that needs a
  missing page, by its own access or by the kernel copying from a mapping, sleeps until the
  page is there: the request cannot go to that thread's own server, which may be the kernel's
  caller. Supplied pages are committed and stay until the object goes. The pager's process
  ends when the tree's last program is gone. The wait for a page ends when the thread dies
  (`SIGKILL`, an exiting process), so a pager that never answers cannot make it unkillable. A
  pager that cannot supply a page says so (`mo_fail`): the access fails (`SIGBUS`, as an I/O
  error under `mmap` on Linux), and a later one asks again. A request counts as asked only
  until the pager takes it, and if the pager's process dies, every wait for it ends (`EIO`).
- **The server's runtime** (phase R3): a heap in its shared region that grows on demand
  (`shared_map`, committed memory), and a mutex for data shared by all threads of the tree
  (Drepper's three-state futex lock over the kernel's futex, which keys the server's memory by
  instance and address since that memory is pinned and in no address space's areas).
- **Memory semantics are the server's** (phase R4): `mmap`, `munmap`, `mprotect`, `mremap`,
  `madvise`, `msync` and the `mlock` family no longer reach the kernel's Linux code. The server
  checks the arguments and turns them into kernel mapping calls: `mo_map` with anonymous
  private memory (handle 0), a new memory object for shared anonymous memory, or the file
  behind a descriptor of the kernel's table (`kfile_object`, until files are the server's);
  placement (a hint, `MAP_FIXED`, `MAP_FIXED_NOREPLACE`), `MAP_NORESERVE` and `MAP_POPULATE`
  are flags of `mo_map`. The kernel's own `mmap` remains for its native servers.
  `/proc/counters` counts the calls still passed through (`legacy_calls`), and
  `/proc/<pid>/counters` a process's own.
- **Program memory and time** (phase R5): the server reads and writes the program's memory
  directly in its view. A fault there is resolved as the program's own would be (demand paging,
  copy-on-write); one the program may not make resumes at the fixup of the server's copy
  routine, registered once (`set_usercopy`), so the call reports `EFAULT`, as Linux's
  `copy_to_user` does. The server checks every program pointer against 64 TiB first. The
  clocks, `nanosleep`, `clock_nanosleep`, `gettimeofday`, `time` and `sched_yield` are the
  server's now, over the kernel's `clock_read`, `sleep_until` and `yield`, and so are
  `sched_getscheduler` and `sched_getparam` (one policy, `SCHED_OTHER` with priority 0; the
  kernel says whether a thread id exists, `thread_exists`, 1090, until the process model is the
  server's). A call the server
  handles itself is restarted after a signal exactly as the kernel's own handling would (the
  kernel remembers the trapped call for the delivery that follows `restricted_enter`).
- **Pipes are the server's** (phase R6a), the first kind of file it implements. Until the
  descriptor table moves, such a file is a **placeholder** in the kernel's table
  (`kfd_install`): `dup`, `close`, `fcntl`'s descriptor flags, `fork` and close-on-exec work
  on it unchanged, and `poll`, `select` and `epoll` see the readiness the server reports
  (`kfd_ready`; new data is reported even without a change, as an edge for `EPOLLET`). The
  server looks up every descriptor it is called with (`kfd_lookup`) and handles calls on its
  own files itself; when the last descriptor of a placeholder goes, the server learns it at
  once if its own pass-through call closed it (`legacy_syscall` returns the ids), otherwise
  from the service thread. Blocking reads and writes wait on interruptible server futexes, and
  `sendfile` with a pipe at either end runs in the server (`kfd_read`/`kfd_write` for the
  kernel's file at the other end). A write without readers raises `SIGPIPE`
  (`signal_thread`, 1102) and fails with `EPIPE`, as on Linux. `eventfd` followed (R6b), on the
  same footing.
- **`AF_UNIX` sockets are the server's** (phase R7a, `unix.rs`, `sockcalls.rs`, `scm.rs`):
  `socket` and `socketpair` for the family come to the server (other families pass through to
  the kernel and netd), and so does every socket call on one of its sockets' descriptors.
  Stream, datagram and seqpacket sockets, socket pairs, names as socket inodes of the tmpfs
  and of `/data` and in the instance's abstract namespace (autobind too), `listen` with its
  backlog, `accept4`, `connect`, `sendmsg`/`recvmsg` and their relatives (`sendmmsg`,
  `recvmmsg`), `shutdown`, `getsockname`/`getpeername`, the options of `SOL_SOCKET`
  (`SO_SNDBUF` with Linux's accounting, `SO_PASSCRED`, `SO_PEERCRED`, timeouts, ...),
  `FIONREAD`; `EPIPE` raises `SIGPIPE` (`signal_thread`, 1102) unless `MSG_NOSIGNAL`, a
  closed end with unread data resets the connection, poll and epoll see Linux's readiness
  (`POLLRDHUP` included). Descriptors pass between processes (`SCM_RIGHTS`) as handles on
  their open file descriptions while in flight (`kfile_object`; the receiver's descriptor
  from `kfd_install_file`, 1100, with `MSG_CMSG_CLOEXEC`), so they outlive the sender's
  close, and any kind of file can be passed; sockets in flight that nothing but messages in
  flight keeps (a cycle) are collected as Linux's `unix_gc` does (`kfile_info`, 1101; the
  kernel reports a socket in flight that only its handles in flight keep once a descriptor
  went, also by exit or exec, `EVENT_INFLIGHT`), on the instance's worker thread, which
  serves no page; at most 16 Ki descriptors are in flight in an instance. No server lock the
  pager takes is held while program memory is copied. A call keeps the server's file it works on until it returns
  (`kfd_lookup` pins it), so another thread's close does not end a blocked receive or
  accept. `SCM_CREDENTIALS` carries the sender's ids (`thread_ids`, 1103) and may name only
  processes of the caller's tree.
- **Paths are the server's** (phase R6c.2): the server keeps a record per working-directory
  context (cwd and umask; `fs_record`). The kernel's clone still decides who shares one
  (`CLONE_FS`): before a clone that makes a new context passes through, the server hands the
  kernel a copy of the caller's record for it, and the kernel reports each record once its
  context ended (`EVENT_RELEASE`). Every call that takes a path (`open`, the `stat` family,
  `access`, `mkdir`, `unlink`, `rename`, `symlink`, `readlink`, `chmod`, `truncate`, `statfs`,
  `chdir`, `getcwd`, `execve`'s program) resolves in the server: `.` and `..` by name, symlinks
  by reading them (at most 16). The tree is still the kernel's, reached through handles on its
  inodes (`inode_walk` walks as far as it can and stops after a symlink, so a path without one
  costs one call); `inode_open` makes the descriptor, `exec_target` hands the resolved program to
  the `execve` that passes through. `umask` is real now (the kernel's was fixed at `022`).
- **The root is the server's own tmpfs** (phase R6c.2c): each instance unpacks the boot image's
  initramfs into a tmpfs of its own when it first resolves a path (`initramfs`: the archive as
  a read-only object; `mo_from_image`: a file object over a member's bytes, copied only page
  by page when needed and written privately). The kernel's tree stays mounted where it still
  serves: `/dev`, `/proc` and `/sys` (the namespace finds mounts by name, the longest first);
  `/data` is the server's own (below). The kernel keeps its own view of the initramfs for what it starts itself (the
  servers, `/init`); a program a tree runs comes from the tree's tmpfs. Its directories
  and symlinks (and socket inodes) live in the server; a file's contents are a **file object** of the kernel's
  (`mo_create_file`), a memory object that grows and shrinks like a file, charged to the same
  tmpfs limit, read and written straight into the program's memory (`mo_file_read`,
  `mo_file_write`) and mapped with `mo_map`. Open files are placeholders whose calls (`read`,
  `write` and their vectored and positioned forms, `lseek`, `fstat`, `ftruncate`, `getdents64`,
  `fstatfs`, `sendfile`, `mmap`) the server answers. ETXTBSY works as in the kernel's VFS: a
  shared mapping through a writable descriptor and a program run from the file each keep a
  **hold** on the file object (`mo_hold`), and the kernel tells the server when the last holder
  is gone. Before answering ETXTBSY the server waits until it has taken in every release the
  kernel reported so far, so a program that ended and was reaped no longer keeps its file busy. The tree has a lock per inode and, for renames and removals, a lock of its own (as
  Linux's rename mutex), under which alone two inode locks are ever held. Files there report
  device `0x1a`; renames between it and the kernel's mounts fail with `EXDEV`, and the mount
  points with `EBUSY`. Its regular files and directories are always ready for `poll` and
  `select`, and `epoll` refuses them (`EPERM`), as on Linux (`kfd_install`'s kind).
- **Channels to device servers** (I/O rings step 2, `docs/design/io-rings.md`, ADR 0005): the
  data plane's kernel part. A channel is a memory object with a submission and a completion
  ring (`crates/ring`), mapped into the server's region (`chan_create`) and, once the kernel
  carried its offer to a service registered for channels (a control request over IPC), into
  the service (`chan_attach`); futexes on the ring words are the doorbells. The server grants
  pages of its memory objects to the channel (`grant`): they are pinned (no truncation over
  them, no reclaim), the service maps them read-only or writable as granted and gets their
  device addresses through its server's DMA domain (physical addresses until there is an
  IOMMU). `revoke` removes the service's mappings at once (their ranges stay reserved and inaccessible until the service unmaps them); a grant a device may still reach
  stays pinned until the service lets go of it. When either end goes, every grant is taken
  back, the other end's sleepers wake (futex waits on the channel fail with `EPIPE`) and its
  `state` word says which end is gone. `servers/ringtest` is the test service. A service
  whose event loop sleeps in `ipc_receive` arms doorbell watches on its submission rings
  (`chan_watch`: a client's doorbell becomes an `ipc_receive` event), its CPU copies into
  grants survive a revoke (`set_copy_fixup`: the fault fails the copy, not the service), and
  `grant_dma_pages` hands out the device addresses of a page range in one call.
- **diskfs's ring service** (I/O rings step 3, `crates/fsring`, `servers/diskfs/src/service.rs`):
  diskfs takes channels and serves the file protocol over them (its only protocol): reads and writes of files (up to 32 in flight, the device moving the data by DMA
  straight between the disk and the client's granted pages; the CPU only zero-fills holes and
  reads a partial sector before a write that does not cover it), lookup, create, unlink,
  rename, truncate, readdir, readlink, stat, statfs, flush and grant release (`FORGET`, at once
  for a grant no request uses). Writes are
  write-back: blocks reserved in memory, linked once the data is on the device, durable with a
  `FLUSH` that writes data before metadata. Every descriptor is copied out once and validated,
  malformed ones complete with an error, a grant revoked under diskfs fails the request (its
  copies run behind the kernel's copy fixup), resources are bounded per channel. One thread:
  polling while busy, doorbells into `ipc_receive` while idle (also while a client's requests
  wait for room in its completion ring). An unlinked inode is freed only when no channel holds
  it any more.
- **`/data` in the server** (phase R6c.3, I/O rings step 4, `servers/linux/src/datafs.rs`,
  `fsclient.rs`, `datafile.rs`): the Linux server is diskfs's client over one channel per
  instance. Paths on `/data` are resolved with `LOOKUP`s, every path call and every call on an
  open file (`read`, `write` and their vectored and positioned forms, `lseek`, `fstat`,
  `ftruncate`, `fsync`, `fdatasync`, `getdents64`, `fstatfs`, `sendfile`, `mmap`, `execve`,
  `sync`, `syncfs`, `msync`) is the server's, none passes through to the kernel. Each regular
  file the server uses has one cached object for all its descriptors, mappings and programs. A
  read that meets a missing page fills it itself, a fault through the pager thread: a run of
  missing pages (read-ahead from 64 KiB up to 4 MiB while the file is read in order) is granted
  to diskfs, whose device writes them by DMA, and declared filled. `write` copies into the cache
  and marks pages dirty; write-back grants runs of dirty pages and sends `WRITE`s from them by
  DMA, many in flight: `fsync`, `fdatasync`, `msync(MS_SYNC)`, `sync`, `O_SYNC`, `O_DSYNC` and
  `RWF_(D)SYNC` wait for it and a `FLUSH`; the pager writes a file back five seconds after it got
  dirty, everything when the kernel asks for room and when the instance ends (the kernel waits
  for that before it powers off). `O_DIRECT` reads write their range back and read from the
  disk. Requests share the channel's slots (as many as the rings hold), one waiting thread at a
  time takes completions and hands them out; every completion is checked against what was
  asked. The server holds the inodes it uses in diskfs and releases them when it lets go: an
  unlinked one once nothing uses it (its blocks are free when `unlink` or the last `close`
  returns), the least recently used beyond 512. If diskfs dies, the next request connects a new
  channel (diskfs is started again); in-flight reads fail with `EIO`, failed write-backs are
  written again, inodes in use are held again (an unlinked one becomes stale: `EIO`).
- Programs can ask their server for test calls (1500 and up, `lxtest`) that exercise this
  interface on the calling process.

### Signals

- Per process: 64 actions, the pending set of signals sent to the process, and the interval
  timer. Per thread: the blocked mask and the pending set of signals sent to that thread
  (`tkill`/`tgkill`, faults). A process signal goes to the first thread that does not block it,
  which is woken (or interrupted on its CPU by an IPI). `fork` inherits actions and mask,
  `exec` resets caught signals.
- Default actions act on the whole process, as on Linux: a fatal signal ends every thread, and
  a stop signal starts a group stop that every thread joins; the last one to stop reports it
  to the parent, and `SIGCONT` resumes them all.
- **Delivery** happens on every return to user space (after syscalls and after timer
  preemption). Default actions terminate, ignore or stop. For handlers, the kernel pushes a
  signal frame on the user stack: restorer address, saved register frame, saved mask, the
  FPU/SSE state (asynchronous handlers would otherwise clobber it) and `siginfo`.
- **Temporary masks**: `rt_sigsuspend`, `ppoll` and `pselect6` wait with the mask they are
  given. If a signal interrupts them, the mask stays until it is delivered, so the handler
  runs with it and the caller's own mask comes back when the handler returns; otherwise the
  caller's mask is put back at once and a signal the temporary one held off stays pending.
- Blocking calls (TTY and pipe I/O, `wait4`, `nanosleep`, `poll`/`select`, `pause`) return
  `EINTR`, but only after checking for available data or a finished child first.
- **Syscall restart**: a call interrupted by a stop, or by a handler installed with
  `SA_RESTART`, is rewound to its `syscall` instruction and runs again, so `cat` survives
  Ctrl+Z / `fg`. Sleeps and polls report `EINTR` instead, as on Linux.
- **Job control**: stopped processes leave the run queue until `SIGCONT` (or `SIGKILL`), and
  parents learn about stops and continues through `SIGCHLD` and `wait4` with `WUNTRACED` and
  `WCONTINUED`. `wait4` supports the POSIX process group selectors (`pid` 0 and < -1).
- The TTY turns Ctrl+C, Ctrl+\ and Ctrl+Z into `SIGINT`, `SIGQUIT` and `SIGTSTP` for the
  foreground process group.

### Filesystem

- **VFS**: reference-counted inodes (`Arc<Inode>`) holding directories (`BTreeMap`), files,
  symlinks or devices. Path resolution follows symlinks with loop detection and supports the
  `*at` family relative to directory descriptors.
- **Initramfs**: the builder writes a `newc` cpio archive, and the kernel unpacks it at boot
  without copying file contents. A file's pages are read from the ramdisk until a page is
  written or mapped; then it gets its own frame in the file's page cache.
- **tmpfs**: the files in memory keep their contents in their page cache (as Linux's tmpfs).
  The pages are committed memory, and all of them together may take half of the commit limit
  (`ENOSPC` beyond, as Linux's default tmpfs size).
- **Open files** are shared descriptions (`Arc<OpenFile>`) with offset and flags, so `dup`,
  `fork` and close-on-exec behave as on Linux.
- **eventfd**: a 64-bit counter as a file (reads take it, or 1 of it with `EFD_SEMAPHORE`; writes add and block before it would overflow), on one wait channel that `poll` and `select` listen on too.
- **Pipes** have a 64 KiB buffer with blocking reads and writes and EOF/`EPIPE` semantics.
- **Quotas**: inodes, symlink targets and pipe buffers live on the kernel heap and are charged
  against an 8 MiB budget (`ENOSPC`), so user programs cannot exhaust the heap.

### Microkernel architecture

oxidenix is being restructured into a microkernel step by step, keeping the Linux syscall
interface working after every step. The kernel keeps memory management, scheduling, IPC and
interrupt dispatch; drivers and filesystems move into user-space servers.

- **IPC** (`process/ipc.rs`): a privileged server registers a service name (`ipc_register`),
  then loops over `ipc_receive` and `ipc_reply` (oxidenix syscalls 1000-1002). The kernel is
  the client on behalf of user programs: `call` queues a request and sleeps uninterruptibly
  until the reply; `post` queues a message without waiting, for contexts that must not sleep
  (releasing an unlinked inode while dropping it). Messages up to 64 KiB are copied through the
  kernel. A server announces that one of its objects changed with `ipc_notify(token)`
  (syscall 1006), which wakes whoever waits on that object's channel; the channel belongs to
  the server's registration, so a restarted server cannot wake its predecessor's waiters.
- **Hardware access without kernel drivers**: servers started by the kernel are *privileged*
  and may call `ioperm`, but only for the ports the kernel assigned to them (diskfs gets the
  I/O BAR of its virtio block device, netd the one of its network card;
  anything else is `EPERM`). Granted ports are set in the TSS I/O permission bitmap, which is
  installed on every switch to that process. The server cannot touch other ports or disable
  interrupts (no IOPL 3). The kernel itself only scans PCI (`drivers/pci.rs`) and enables I/O
  decoding and bus mastering for the devices it hands out.
- **Device interrupts** (`process/irq.rs`): a server may take the interrupt line assigned to it
  (`irq_enable`, syscall 1003). When the line fires, the kernel masks it, marks it pending and
  wakes the server; `ipc_receive` reports it as a notification (request id 0, result = mask of
  lines) before any queued request. The server handles the device and unmasks the line with
  `irq_enable` again. `ipc_receive` also takes a timeout, which a network stack needs for its
  timers.
- **DMA** (`dma_map`, syscall 1004): a server with a DMA budget gets a physically contiguous,
  zeroed area mapped into its address space, plus its physical address for the device. The
  area belongs to the server description, not to the process, so a restarted server gets the
  same memory instead of leaking it while the device may still write to it.
- **Remote filesystems**: the VFS has a second kind of inode whose operations become
  `fsproto` requests to a server (`fs/remote.rs`): procfs's `/proc` and `/sys`, generated on
  every read and never cached. (The disk is the Linux server's, over the I/O rings.)
- **Fault isolation**: when a server dies, its services are marked dead and every pending
  request fails with `EIO`; the kernel and the rest of user space keep running.
- **Self-healing**: the next request to a dead server starts it again (in the
  context of the requesting program, which may sleep) and continues transparently. Inode numbers
  live on disk, so files and directories that were open before the crash stay usable. Requests
  that were in flight during the crash still fail with `EIO`, since they may or may not have
  been carried out. The restart policy (ADR 0006) stops crash loops without giving a service
  up for good: a server that lived half a second is restarted at once; after young deaths in
  a row it is restarted with a backoff (0.1 s, doubling); after a sixth young death in a row,
  or more than 20 restarts in a minute, the service is down (`EIO` at once) for 5 s (doubling
  with every crash loop in a row, at most 5 min), and then the next use tries again.
  diskfs is restarted when a Linux server instance connects a new channel after it died.
  The same holds for netd: the next socket call restarts it, which resets the network card and
  repeats DHCP; sockets that were open in the old netd fail with `EIO`. Every registration of a
  service carries a generation number, and a socket only ever talks to the netd instance that
  created it, so a stale socket handle can never reach a new connection of another program.
  Restarted servers are children of the kernel, never of the program that triggered them.
  The kernel reads each server program once at boot and restarts it from that copy, so
  replacing `/sbin/diskfs` later cannot smuggle a different program into a privileged process.
- **Protected servers**: like init on Linux, privileged servers ignore signals from user space:
  a direct `kill` fails with `EPERM`, and group, broadcast and terminal signals skip them. Only
  the kernel can stop them (the monitor's `kill <pid|name>` does, for testing).
- **Servers in Rust**: `servers/diskfs` and `servers/netd` are `no_std` Rust programs built for
  `x86_64-unknown-none` as a static `ET_EXEC` binary, using `crates/oxrt` for its entry point,
  syscalls, heap and port I/O. They share no code with the kernel except the message format.
- **procfs: the Linux personality** (`servers/procfs`). The kernel has no `/proc`. It keeps
  native accounting (user and system ticks per process and CPU, start time, mapped pages,
  command line, program path, a Linux-style load average) and hands it out as fixed binary
  records through `proc_query` (syscall 1005, privileged servers only, `crates/procproto`).
  The procfs server renders the Linux formats from them on every read (`/proc/stat`,
  `meminfo`, `loadavg`, `uptime`, `cpuinfo`, `mounts`, `version`, `/proc/sys/kernel/*`,
  `/proc/<pid>/{stat,statm,status,cmdline,comm,exe,task}`, and oxidenix's own `/proc/counters`
  and `/proc/<pid>/counters`) and the CPU list in
  `/sys/devices/system/cpu`; the kernel mounts its two trees at `/proc` and `/sys`. Requests
  carry the caller's pid, which `/proc/self` resolves to. A non-Linux userland would simply not
  run this server.
- Still in the kernel today: the VFS itself, pipes, the TTY, console and keyboard. They are the
  next candidates for servers.

### Networking

- **netd** (`servers/netd`) owns the network card. The kernel finds it on PCI (a virtio-net
  card in legacy mode, whose registers are all I/O ports) and hands netd its ports, its
  interrupt line and a 512 KiB DMA area. netd sets up the two virtqueues with 64 fixed 2 KiB
  buffers each, sleeps in `ipc_receive` until a request, a card interrupt or the next timer
  of the TCP/IP stack, and runs [smoltcp](https://github.com/smoltcp-rs/smoltcp) for ARP,
  IPv4, ICMP, TCP, UDP and the DHCP client. After every step of the stack and every request
  it compares each socket's poll events with the last ones and calls `ipc_notify` for every
  socket that gained some, so `poll`, `select` and `epoll` wake when data, a connection or
  buffer space arrives.
- **Loopback**: frames to the host's own address or to `127.0.0.0/8` never reach the card;
  netd's device layer feeds them back as received frames and answers ARP for those addresses
  itself, so a program can talk to a server on the same machine. Frames from the wire that
  claim a `127.0.0.0/8` address are dropped, so services on `127.0.0.1` are not reachable from
  the network.
- **Sockets** (`process/sys_net.rs`, `net.rs`): `socket`, `bind`, `listen`, `accept`/`accept4`,
  `connect`, `send*`/`recv*` (also `sendmsg`/`recvmsg`), `shutdown`, `getsockname`,
  `getpeername` and `getsockopt(SO_ERROR)` for `AF_INET` stream and datagram sockets, plus raw
  ICMP sockets (`SOCK_RAW`, `IPPROTO_ICMP`) for `ping`: the program writes the ICMP message,
  netd adds the IPv4 header, and reads return whole IPv4 packets, as on Linux. netd answers
  echo requests itself. A socket is
  an open file, so `read`, `write`, `poll`, `select`, `fcntl(O_NONBLOCK)`, `dup` and `fork`
  work as usual. Every operation is a `netproto` request to netd; closing the last descriptor
  posts `Close` without waiting.
- **Blocking without blocking netd**: netd keeps a request that cannot complete yet (accept
  without a connection, recv without data, a connect in progress) and answers it after the
  stack made progress, while it keeps serving other requests. A signal interrupts the waiting
  program: the kernel abandons the request (`EINTR`) and tells netd to drop it. At most 128
  requests (holding at most 512 KiB of data) wait at a time; beyond that, calls fail with
  `ENOBUFS` instead of exhausting netd's heap. Data is sent straight from user memory in 32 KiB
  messages, never copied whole into the kernel.
- **Configuration**: `/etc/resolv.conf` points to QEMU's DNS proxy (`10.0.2.3`); DHCP gives
  `10.0.2.15/24` with gateway `10.0.2.2`. The self-tests use an echo service that QEMU provides
  at `10.0.2.100:7` (`guestfwd` to `cat` on the host).
- **Restarts**: a crashed netd is started again by the next socket call (see self-healing). It
  gets the same DMA area, which it clears before handing it to the freshly reset card.
- Not yet: IPv6, other raw protocols and interface configuration from user space
  (`ifconfig`). `AF_UNIX` sockets are the Linux server's (see "Restricted mode and the Linux
  server").

### Persistent storage

- **virtio-blk driver** in `diskfs` (`blk.rs`, legacy PCI interface on the shared transport in
  `crates/virtio`) for the data disk. Several requests are in flight at once, each a chain of
  header, data buffers (scatter-gather: the DMA area, or a ring client's granted pages) and
  status taken from the queue's free list; the device finishes them in any order. The
  filesystem's own synchronous I/O goes through a 128 KiB bounce buffer as one more request,
  keeping the others' completions for the ring service. The driver polls for completion with
  the device's interrupts off (PCI interrupt lines may be shared: under QEMU the disk shares one
  with the network card, while the kernel gives each line to one server). A flush empties the
  device's write cache.
- **ext2** (`crates/ext2fs`; revision 1 with the `filetype` feature, 1/2/4 KiB blocks) supports reading and
  writing files through direct, single, double and triple indirect blocks, holes, truncation
  (freeing whole indirect subtrees), directories growing by blocks, fast and block symlinks,
  `rename` across directories (with `..` and link count updates and a cycle check), `rmdir`, and
  `chmod`. Allocation goes through the block and inode bitmaps, and group descriptors and the
  superblock's free counts are updated on every change.
- **Block cache** (`crates/ext2fs/src/cache.rs`, 1 MiB, least recently used): metadata blocks
  (inode tables, bitmaps, group descriptors, directories, indirect blocks) are read once and
  changed in memory. Every operation commits before it answers: its dirty blocks are written
  (adjacent ones in one request), then the device is flushed once, so a finished `write` or
  `create` is durable as before. File data bypasses this cache (the Linux server's page cache
  holds it): whole blocks are read and written straight to the device, a run of contiguous blocks as
  one request, and new data blocks are not zeroed first when they are written whole. Reading
  2 MiB from the disk takes about 60 ms (4.4 s with the former ATA PIO driver).
- Failures stay safe: a block stays dirty until it was written, so what a failed commit (or
  eviction) could not write goes with the next request; a new data block whose write failed is
  zeroed before any metadata pointing to it reaches the disk, so no file ever shows a deleted
  file's data; block pointers read from the disk must lie inside the filesystem (`EIO`
  otherwise).
- `write` leaves dirty pages in the Linux server's page cache; `fsync`, `sync`, `msync` and the
  server's write-back write them, a `FLUSH` makes them durable. Before the machine powers off,
  the kernel waits until every instance wrote its caches back and diskfs closed their channels.
  After a session, `e2fsck -fn disk.img` on the host reports a clean filesystem, and `debugfs`
  can read the files.
- **Integration**: `/data` is a mount of every Linux server instance's namespace; one disk inode
  is one `DInode` of the server's. `statfs` and the kernel's static `/proc/mounts` (which lists
  the disk at `/data`) make `df` work.
- A file that is deleted while still open stays allocated as an orphan until the last
  reference is dropped, as on Linux (the server holds it in diskfs), so its inode number cannot
  be reused under an open file.
- Only regular files are read, written, truncated or executed through their data blocks; a
  fast symlink's block pointers hold text, never block numbers.
- Directory reads take a snapshot at offset 0, so `rm -r` deleting entries while it reads never
  skips any.

### Terminal and console

- **Console**: a cell grid on the framebuffer with a 24 px Noto Sans Mono bitmap font, 16 ANSI
  colors, a visible cursor, deferred line wrap and UTF-8. It implements the Linux console's
  terminal type, so `TERM=linux` (and with it ncurses and htop) works: scroll regions, line
  and character insertion and deletion, index and reverse index, the DEC line-drawing set
  (G0/G1, SO/SI), insert mode, auto-wrap control, underline, the palette sequences
  (`ESC ] P`, `ESC ] R`), cursor position reports and device attributes.
- **Drawn glyphs** (`drivers/glyphs.rs`): box drawing (light, heavy, double, rounded), block
  elements and shades, triangles, diamonds, circles, squares, arrows and the VT100 scan lines
  are drawn geometrically for the exact cell, so lines join seamlessly across cells, as in
  modern terminals.
- **Boot logo**: drawn centered above the first boot message. Its source is
  `kernel/assets/logo.svg`; `kernel/build.rs` decodes the rendered `logo.png` into raw RGB at
  build time, so the kernel needs no image decoder. Like on Linux, the logo is plain pixels and
  scrolls away with the text.
- **TTY**: a termios subset (`TCGETS`/`TCSETS*`, `ICANON`, `ECHO*`, `ISIG`, `ICRNL`, `VMIN`, ...)
  with canonical line editing, raw mode for readline, EOF handling and `FIONREAD`. Its buffers
  have fixed sizes because the keyboard path runs in interrupt context and must not allocate.
- **Keyboard**: PS/2 scancodes are decoded in the IRQ handler with the German (`De105Key`)
  layout and translated to terminal bytes: CR, DEL, and the Linux console's sequences for
  arrows, editing keys and F1-F12. The AltGr characters the crate's layout lacks
  (`{ [ ] } \ ² ³ µ`) are added by the driver.

### Userland build

`userspace/build.sh` cross-compiles every `userspace/*.c` with the musl toolchain from nixpkgs
and copies static Bash and BusyBox (with symlinks for all applets) into the root filesystem. The
builder adds `userspace/rootfs/` (`/etc/passwd`, `/etc/motd`, `/etc/test.sh`, ...), writes the
cpio archive and hands it to the bootloader as a ramdisk.

## System calls

Linux x86_64 numbers, grouped by area (about 120 in total):

| Area | Calls |
|---|---|
| Files | `read` `write` `pread64` `pwrite64` `readv` `writev` `preadv` `pwritev` `preadv2` `pwritev2` `open` `openat` (also `O_DIRECT`) `close` `lseek` `sendfile` `truncate` `ftruncate` `fcntl` `ioctl` (the tty's, `FIONBIO`, `FIOCLEX`, `FIONCLEX`) `dup` `dup2` `dup3` `pipe` `pipe2` |
| Metadata | `stat` `fstat` `lstat` `newfstatat` `access` `faccessat` `faccessat2` `readlink` `readlinkat` `chmod` `fchmodat` `utimes` `futimesat` `utimensat` `umask` |
| Directories | `getdents64` `getcwd` `chdir` `fchdir` `mkdir` `mkdirat` `rmdir` `unlink` `unlinkat` `rename` `renameat` `renameat2` `symlink` `symlinkat` |
| I/O multiplexing | `poll` `ppoll` `select` `pselect6` `epoll_create` `epoll_create1` `epoll_ctl` `epoll_wait` `epoll_pwait` `epoll_pwait2` `eventfd` `eventfd2` |
| Memory | `brk` `mmap` (private, shared, anonymous, file through the page cache, `MAP_FIXED[_NOREPLACE]`, `MAP_NORESERVE`, `MAP_POPULATE`) `munmap` `mprotect` `mremap` `madvise` (`DONTNEED`, `FREE`) `msync` `mlock` (no-op) |
| CPUs | `sched_getaffinity` `sched_setaffinity` `getcpu` `sched_getscheduler` `sched_getparam` (the Linux server's: `SCHED_OTHER`, priority 0) |
| Processes and threads | `clone` (`CLONE_VM` `FS` `FILES` `SIGHAND` `THREAD` `VFORK` `PARENT` `SETTLS` `PARENT_SETTID` `CHILD_SETTID` `CHILD_CLEARTID`) `fork` `vfork` `execve` `exit` (one thread) `exit_group` `wait4` `getpid` `getppid` `gettid` `set_tid_address` `sched_yield` `arch_prctl` `prlimit64` |
| Groups and IDs | `setpgid` `getpgid` `getpgrp` `setsid` `getsid` `getuid` `geteuid` `getgid` `getegid` `getresuid` `getresgid` `setuid` `setgid` |
| Synchronization | `futex` (`WAIT`, `WAKE`, `WAIT_BITSET`, `WAKE_BITSET`, `REQUEUE`, `CMP_REQUEUE`; private and shared, monotonic and realtime timeouts) |
| Signals | `rt_sigaction` `rt_sigprocmask` `rt_sigreturn` `rt_sigsuspend` `rt_sigpending` `kill` `tkill` `tgkill` `pause` `sigaltstack` `alarm` `setitimer` `getitimer` (`ITIMER_REAL`) `rt_sigtimedwait` |
| Process control | `prctl` (name, parent-death signal, dumpable, no-new-privs, capability bounding set) `capget` `capset` (everything runs as root with every capability) |
| Filesystems | `statfs` `fstatfs` `sync` `syncfs` (every process tree's page cache of `/data`) `fsync` `fdatasync` |
| Linux server (normal mode only) | `restricted_enter` (1010), `legacy_syscall` (1011), `handle_close` (1012), memory objects: `mo_create` (1013), `mo_map` (1014), `mo_unmap` (1015), `mo_protect` (1016), `mo_read` (1017), `mo_write` (1018), paged objects: `mo_create_paged` (1019), `pager_wait` (1020), `mo_supply` (1021), `mo_fail` (1022), runtime: `shared_map` (1023), `server_futex_wait` (1024), `server_futex_wake` (1025), address space: `vm_remap` (1026), `vm_discard` (1027), `vm_sync` (1028), bridge: `kfile_object` (1029), time: `clock_read` (1030), `sleep_until` (1031), `yield` (1032), `set_usercopy` (1033), placeholders: `kfd_install` (1034), `kfd_lookup` (1035), `kfd_ready` (1036), `kfd_close` (1037), `kfd_read` (1038), `kfd_write` (1039), records: `fs_record` (1040), the kernel's tree: `inode_root` (1041), `inode_walk` (1042), `inode_stat` (1043), `inode_readlink` (1044), `inode_create` (1045), `inode_symlink` (1046), `inode_unlink` (1047), `inode_rename` (1048), `inode_chmod` (1049), `inode_truncate` (1050), `inode_open` (1051), `inode_statfs` (1052), `kfd_inode` (1053), `exec_target` (1054), file objects: `mo_create_file` (1055), `mo_hold` (1056), `mo_file_read` (1057), `mo_file_write` (1058), `mo_file_size` (1059), `mo_truncate` (1060), `initramfs` (1061), `mo_from_image` (1062) |
| Servers | `ioperm` (privileged servers only), `ipc_register` (1000), `ipc_receive` (1001, with timeout and interrupt notifications), `ipc_reply` (1002), `irq_enable` (1003), `dma_map` (1004), `proc_query` (1005), `ipc_notify` (1006) |
| System information | `sysinfo` `uname` (reports `oxidenix`, not Linux) |
| Power | `reboot` (power off ends QEMU, restart resets the machine; every process tree's page cache is written back first) |
| Sockets | `socket` `bind` `listen` `accept` `accept4` `connect` `sendto` `recvfrom` `sendmsg` `recvmsg` `shutdown` `getsockname` `getpeername` `setsockopt` (ignored) `getsockopt` (`AF_INET` only: TCP, UDP, raw ICMP) |
| Time | `clock_gettime` `clock_getres` `clock_settime` (every Linux clock, including the CPU-time clocks of threads and processes) `gettimeofday` `settimeofday` `time` `times` `getrusage` `nanosleep` `clock_nanosleep` (relative and `TIMER_ABSTIME`) |
| Misc | `getrandom` |

Everything runs as root. Unknown syscalls print a kernel message and return `ENOSYS`.

## Testing

`OXIDENIX_TEST=1 cargo run` (in `kernel/`) boots straight into `/etc/runtests.sh`, which runs
every self-test below. The kernel mirrors its console to the serial port and powers off when
the script ends; QEMU's exit status is 1 if everything passed and 3 otherwise. The tests run on
a fresh 64 MiB data disk of their own (`target/test-disk.img`, made anew on every run; the
full-disk tests fill it to the last block), never on the persistent `disk.img`. GitHub Actions
does exactly this on every push (without a display), then checks the test disk with `e2fsck`.

Each of these programs and scripts lives in the root filesystem and runs inside oxidenix:

| Test | Covers |
|---|---|
| `cowtest` | copy-on-write isolation between parent and child, kernel writes into shared pages, 50 forks, shared read-only frames stay unchanged, `brk` does not grow over a mapping |
| `oomtest` | fork bomb (stops at the process limit), memory exhaustion via `mmap`, 100 full pipes; the kernel survives and memory is reusable; a process touching `MAP_NORESERVE` memory beyond the commit limit is killed while one with committed memory touches all of it; a `read()` into an untouched `MAP_NORESERVE` buffer with nothing left to commit kills the reader too (not `EFAULT`) |
| `sigtest` | handlers, killing a busy loop, `SIGCHLD`, `EINTR` on pipe reads, blocked and ignored signals, FPU state across asynchronous handlers, `alarm` and repeating `setitimer`, catchable `SIGFPE`/`SIGSEGV`/`SIGTRAP` from CPU exceptions, an uncaught `SIGFPE` killing the process |
| `jobtest` | stop/continue reporting through `wait4`, restart of a stopped `read()`, `SIGKILL` on stopped processes, `SA_RESTART` |
| `forktest` | `fork`, `execve`, `wait4`, preemptive interleaving of two workers, 4000 forked and reaped processes leaving the kernel heap as it was |
| `proctest` | `prctl` name round-trip, `capget`/`capset` versions and the full capability set, no-new-privs, `PR_SET_PDEATHSIG` delivered to an orphan (via `sigwait`); `/proc` as htop reads it (directory fds with `O_PATH` and `openat`), `/proc/self`, the formats of `stat`, `meminfo`, `loadavg`, `uptime` and `/proc/<pid>/{stat,cmdline,exe}`, `/proc/counters` counting system calls and allocations, `sysinfo`, the CPU list in `/sys`, read-only `/proc`, `/proc/<tid>/status` of a thread that is not the main one (its own `Pid`, the process's `Tgid`) |
| `threadtest` | pthreads: create/join, own tids, TLS, 4 threads counting under a mutex, condition variables and timed waits, 300 threads in a row, `Threads:` in `/proc/self/status`, `exit` and fatal signals ending all threads, the process outliving its main thread, group stop and continue, process signals reaching a thread that does not block them, `pthread_kill`, `fork` and `execve` in a thread, real `vfork`, `posix_spawn`, `munmap` and `mprotect` reaching a writer on another CPU (TLB shootdown), `MADV_DONTNEED` in a loop under three writer threads while another process checks fresh memory for stray stores (this hung the scheduler before) |
| `futextest` | `FUTEX_WAIT` on a changed value (`EAGAIN`), timeouts, `EINVAL`/`EFAULT`, interruption by a signal (`EINTR`), shared futexes across processes, private memory keeping separate keys after `fork`, bitsets, `FUTEX_CMP_REQUEUE` |
| `timetest` | nanosecond resolution of `CLOCK_MONOTONIC`, no step back on one CPU or between two, `clock_getres`, invalid clocks, `BOOTTIME`, `RAW`, `COARSE`, `gettimeofday` and `time` against `CLOCK_REALTIME`, `clock_settime` moving only the wall clock, thread and process CPU clocks (spinning counts, sleeping does not, `pthread_getcpuclockid`, `clock_getcpuclockid`), `getrusage` for the process, the thread and reaped children, `times`, `wait4`'s rusage (a reaped child's CPU time with its own children's, its peak memory, also across `exec`) |
| `timertest` | sleeps and timeouts end when due, not at the next tick, and never early (median and minimum of nine 1–2 ms waits in `nanosleep`, `poll`, `select`, `futex`, `sigtimedwait`), 100 × `usleep(100)`, `clock_nanosleep` absolute (monotonic, past wall-clock times) and on `CLOCK_BOOTTIME`, refusal on CPU clocks, the time left after an interrupted `nanosleep`, a 2 ms `setitimer` interval firing about 50 times in 100 ms, a 1 µs interval timer leaving another process on its CPU its share (and, ignored, its own process), an ignored timer resuming once handled |
| `polltest` | `poll` and `select` wake within 1 ms of a pipe write or a datagram over loopback (median of nine, the writer on the same or another CPU, next to an idle descriptor), a full pipe polling writable once drained, `POLLHUP` when the last writer closes, `EINTR` in a `poll` waiting on files |
| `eventfdtest` | `eventfd` counting (initial value, adding writes, reset on read), `EFD_SEMAPHORE`, `EFD_NONBLOCK` and `EFD_CLOEXEC`, `EINVAL` for short reads, 2^64-1 and unknown flags, a full counter (`EAGAIN`, poll state), a blocking read woken by another process, `poll` waking within 1 ms of a write |
| `sigmasktest` | temporary signal masks of `sigsuspend`, `ppoll` and `pselect`: a pending or arriving signal the mask lets through interrupts them and its handler runs with that mask, the caller's mask comes back afterwards, a successful `ppoll` leaves a blocked signal pending (`sigpending`), a mask that blocks a signal holds it off until the call returns, `EINVAL` for a wrong mask size |
| `epolltest` | `epoll_create1`/`epoll_create` flags and sizes, `EPOLL_CTL_ADD`/`MOD`/`DEL` and their errors (`EEXIST`, `ENOENT`, `EPERM` for regular files, `EINVAL`, `EBADF`), level-triggered, edge-triggered and one-shot reporting, `EPOLLOUT` and `EPOLLERR` on a pipe's write end, interests removed with the file's last descriptor (not before), `maxevents` rotating through ready files, eventfds and UDP sockets in a set, nested instances (`ELOOP` for a loop and for chains longer than five, built at either end) and `poll` on an instance, wake-up within 1 ms of a write, timeouts, `EINTR` and `epoll_pwait`'s mask, `epoll_pwait2` |
| `unixtest` | `AF_UNIX` sockets in the Linux server: stream pairs (`fstat` as a socket, `FIONREAD`, `MSG_PEEK`, reads across writes, names and peer names of a pair, `SO_TYPE`, `SO_DOMAIN`, `SO_PEERCRED`, `SO_SNDBUF` doubled, `EOPNOTSUPP` for other levels, `ESPIPE`), `shutdown(SHUT_WR)` (`POLLRDHUP`, the rest, end of file, `EPIPE` for the writer, the other direction still open), a closed peer (`POLLHUP`, end of file, `EPIPE` with `SIGPIPE`, none with `MSG_NOSIGNAL`, `ECONNRESET` after unread data); nonblocking (`SOCK_NONBLOCK`/`SOCK_CLOEXEC`, `EAGAIN`, edge-triggered `epoll` and new edges, a full send buffer without `POLLOUT` until drained, `EPOLLRDHUP`, `SO_RCVTIMEO`); datagram pairs (boundaries, empty datagrams, truncation with `MSG_TRUNC`) and seqpacket pairs (boundaries, the truncated rest gone, end of file); servers on a tmpfs path, a relative path, a `/data` path and an abstract name (`EADDRINUSE`, `ECONNREFUSED` before `listen`, `SO_ACCEPTCONN`, names given back, a pending connection polling readable, the client's and the listener's names and credentials on both ends, a full backlog's `EAGAIN`, pending clients reset when the listener closes), socket inodes (`S_ISSOCK`, `ENXIO` for `open`, kept after close), `ENOENT` and `ECONNREFUSED` for paths, autobind; named datagram sockets (`recvfrom`'s sender, `connect` and `AF_UNSPEC`, `ENOTCONN`, `EPROTOTYPE`, `ECONNREFUSED`); `SCM_RIGHTS` to a forked child (a pipe end and a file whose offset is shared, `MSG_CMSG_CLOEXEC`, the passed descriptors outliving the sender's close, `MSG_CTRUNC` dropping what does not fit, a socket passed back), `MSG_PEEK` installing copies (only as many as the control buffer holds), `EBADF`, a cycle of sockets in flight collected, also one an exited process left (and its descriptors in flight never blocking another process's); `SO_PASSCRED` and `SCM_CREDENTIALS`, implicit and explicit, `ESRCH` for a process outside the tree, the bytes delivered despite an unwritable control buffer; a blocked `recvmsg` and `accept` outliving another thread's close, 200 races of a close, a receive taking a socket out of flight and the collector; a datagram sender held back by a full receiver not writable for `epoll` until it reads; receives into pages of a `/data` mapping the pager brings while sockets close and the collector runs; five processes trying to put more descriptors in flight than the instance's bound; nothing left in flight when it ends |
| `lxtest` | the Linux server's kernel interface through its test calls: a memory object it filled, mapped into the program, a program store read back by the server, write protection, unmapping, mappings refused at the server's region and unaligned; a paged object whose pages the pager thread supplies when the program reads them or the kernel copies from them (`write` from the mapping), each page once; `SIGKILL` ending a thread that waits for a page the pager never supplies; a page the pager fails raising `SIGBUS` and a later access getting it, for a second such object too; the server's heap with 2000 blocks of many sizes and its mutex serializing six threads in two processes; `/proc/self/counters` counting the process's own calls passed through (`legacy_calls`; five passed through on purpose, `TEST_PASS_THROUGH`); `mmap`, `mprotect` and `munmap` handled by the server without a call passed through, and `clock_gettime`, `gettimeofday` and `nanosleep` likewise; the server writing a page the program never touched, and `EFAULT` (not death) for read-only, `PROT_NONE` and unmapped program memory, for the server's own memory and across the 64 TiB line; pipe reads and writes without a call passed through, end of file after the writer closes, `EPIPE` and `SIGPIPE` without readers, `FIONBIO`, `FIOCLEX` and `FIONCLEX` on a pipe (the kernel's descriptor flags), other ioctls `ENOTTY`; eventfd likewise, `EAGAIN` when empty and non-blocking; the server's records: a forked child's copy, a thread's shared one, released when processes end; path calls without a call passed through: `chdir`/`getcwd`, `umask` on a new file, `O_EXCL`, symlinks (`readlink`, `stat` vs. `lstat`, `O_NOFOLLOW`, a loop's `ELOOP`, a symlinked directory in a path), `openat` relative to a directory descriptor, `rename`, a child's own working directory, `fchdir`, `rmdir`, `O_CREAT` through a dangling symlink; `/tmp` as the server's tmpfs: its own device, reads, writes, `lseek` and `fstat` without a call passed through, `O_APPEND`, a shared mapping writing the file, `ftruncate`, `readdir`, `EXDEV` and `EBUSY` at the mounts, the root and `/bin/busybox` from the server's tmpfs, `/proc` and `/dev` the kernel's; channels to the test service `ringtest`: requests and completions through the rings with both ends sleeping on futex doorbells, a service's doorbell watch kept once when armed twice, never moved by a requeue, woken once and arriving as an `ipc_receive` event, connect errors (`EISCONN`, `ENOENT`, `EOPNOTSUPP`, `ENOTCONN`), grant data both ways, a read-only grant the service can neither `mprotect` writable or executable nor have the kernel store into, the kernel's grant bounds, device addresses only within a grant, `EBUSY` for truncating a granted page, `ENODATA` for an unsupplied paged page, a revoked grant gone from the service (its range reserved and inaccessible until the service unmaps it), a draining grant pinned with its id held until the service lets go, the client's end closing while the service sleeps (its grant mappings gone, pins released), the service dying of a store into a read-only grant while the client waits (the client wakes with `EPIPE`, the page unchanged, the service restarted for the next channel), the service executing a new program that can then neither map the grant nor get a device address of it; the file protocol against diskfs (`TEST_DISKRING`): the disk image's README read by DMA at unaligned offsets, stat, readdir with a cursor, statfs, a symlink; aligned, unaligned, one-sector and past-the-end writes, a flush, the file read back against a model, truncate, rename, permissions; malformed requests completing with `ENOSYS`, `EINVAL`, `EBADF`, `EACCES`, `ENOENT`, `ENAMETOOLONG`, `ENOTDIR`, `EEXIST`; 24 writes and 24 reads in flight; a grant revoked under diskfs (`EFAULT`, diskfs alive, `FORGET`); a client closing with reads in flight; a revoked grant's range given to no other grant; an unlinked inode freed only when no channel holds it; a write stalled behind another with every operation slot busy; requests waiting for completion room leaving diskfs idle (its CPU ticks in `/proc`) and completing once there is room; the ring's file read and removed through `/data` (the server's page cache, another channel); the page cache's kernel interface (`TEST_CACHED`): a failed fill past the end of a file leaving no trace once it grows, a write-back scan longer than one call going on where the kernel says, a truncation giving up (`EBUSY`) on a page pinned by a grant never let go of, a sync across instances (`sync_others`) |
| `lxtest` x3 (in `runtests.sh`) | all of `lxtest` three more times in the same boot, its output into a pipe, beside a loop reading `/proc` (calls passed through): every check holds in any run and whatever else runs |
| `lxtest crashloop` (in `runtests.sh`) | the restart policy on the test service dying at every use: restarts with a growing backoff (3.1 s in all), the service down (`EIO` at once) after the sixth young death in a row, up again after the 5 s cooldown |
| `vmtest` | demand paging (a 64 MiB mapping costs nothing until touched), `SIGSEGV` on read-only and `PROT_NONE` pages with contents kept, split areas after a partial `munmap`, NX and the JIT pattern (1 GiB `PROT_NONE` reservation, write code, `mprotect` to executable, call it), commit limit and `MAP_NORESERVE` (also a 4 GiB reservation made writable and executable at once, as V8's code range, and forked counting only its touched pages, while the same without it is `ENOMEM`; 40 rounds of partial `munmap`, `MADV_DONTNEED`, `mremap`, `mprotect`, `fork` with copy-on-write on both sides and `exec` leave `Committed_AS` unchanged; a read-only area read in completely becomes writable with nothing left to commit), `mremap` in place and moving, `MADV_DONTNEED`, shared vs. private memory across `fork`, lazy file mappings and `SIGBUS` beyond the end, `MAP_FIXED_NOREPLACE`, stack growth to 4 MiB and overflow beyond 8 MiB, nothing mapped at or above 64 TiB (fixed mappings fail, hints and the stack stay below), the Linux server's memory out of the program's reach and its kernel calls `ENOSYS` for a program |
| `smptest` | CPU count and affinity (pinning to every CPU, empty masks), the scheduling policy (`SCHED_OTHER`, priority 0, for the caller and another thread, `ESRCH`, `EINVAL`), parallel speed-up of CPU-bound processes, `fork`/`exit`/`wait` on every CPU at once, 5000 pipe round trips between two CPUs, signals to a process running on another CPU, timers on time while a program floods the console with palette changes on the same CPU |
| `nettest` | TCP to an echo service through QEMU, `ECONNREFUSED`, `listen`/`accept` over loopback with a forked client, EOF after the peer closed, non-blocking `accept` and `connect` with `poll` and `SO_ERROR`, `EINTR` in a blocking `recv`, UDP over loopback, raw ICMP echo to the gateway and over loopback, source address for off-subnet destinations, overflowing message vectors, `AF_INET6` rejected |
| `mmaptest` | shared file mappings: stores visible to `read` and `write` visible in the mapping at once, another process's own mapping of the file, the size unchanged by stores; private mappings seeing `write` until they write a page, and never reaching the file; mappings outliving `close` and `unlink`; the zero tail of the last page and `SIGBUS` beyond it; growing and shrinking with `ftruncate` (`SIGBUS` in shared pages and private copies beyond the new end, zeros after growing again); `EACCES` for writable sharing of a read-only descriptor (also via `mprotect`); shared anonymous memory across 8 children; mapping initramfs files |
| `exectest` | eight runs of one program sharing its pages (less memory than one copy), data and bss of the loaded program, `ETXTBSY` for opening or truncating a running program and for running a program open for writing or mapped through a writable descriptor (not after `munmap`, not for a read-only mapping), a changed program file taking effect on the next run, a running program surviving the deletion of its file |
| `cachetest` | the page cache of `/data` files: data read back right after writing and `fsync` (from memory), `Cached` in `/proc/meminfo`, committing all free memory reclaims cached pages (and all of it can be used), the file read again from the disk afterwards, read-only shared and private mappings of a disk file, `pwrite` visible to `pread` and both mappings, private stores staying private, `ftruncate` shrinking and growing (zeros, not old data, in reads and the mapping), a program on the disk running from the cache and `ETXTBSY` while it runs |
| `writebacktest` | stores through a shared mapping of a `/data` file: reading makes nothing dirty, a store makes its page dirty (`Dirty` in `/proc/meminfo`), `msync`, `fsync` and `fdatasync` write it back (also a page stored to again afterwards), `write` and a store in one page both arrive, the server's write-back takes a store to the disk on its own after `munmap`, a store of a process that exited, dirty pages surviving reclaim, truncation of a file with dirty pages, 1 MiB of stores; `O_DIRECT` reads as the view of the disk |
| `mmaptest /data` | all of `mmaptest` on a disk file |
| `datatest` | `/data` in the Linux server: reads, writes, `lseek`, `stat`, `fsync` without a call passed through (`legacy_calls` of `/proc/self/counters`), its own device and ext2's `statfs`; two descriptors, a mapping and another process's mapping sharing one page cache; `write` leaving dirty pages, `fsync` writing them (`Dirty` back, the data on the device: `O_DIRECT`), an `O_SYNC` write clean when it returns, a write past the end and its hole, `sync`; truncation with dirty mapped pages (`SIGBUS` beyond, the tail zero, the size on the device); 100 children storing into an inherited mapping while a thread writes it back, every store on the device; 400 forks racing a truncation of a mapped file, every child getting `SIGBUS` beyond the new end; 8 processes reading one uncached file at random offsets at once; 4 threads writing parts of one file; a 32 MiB file written and read back with 12 MiB left to cache it; a read while dirty pages fill memory; a full disk: `write` itself failing with `ENOSPC` (the space promised as data enters the cache), everything accepted on the device after `fsync`, `statfs` counting promised space, a store into a hole raising `SIGBUS`, and going through once there is room again |
| `bash -c` (in `runtests.sh`) | Bash itself: functions, arrays, arithmetic, `[[ ]]`, a pipe into `grep`, a here-document into a `/tmp` file read back, a subshell's `cd`, command substitution |
| `sh /etc/test.sh` | files, pipes, `cd`, `mkdir`/`touch`/`rm`, rename cycles via symlinks, the tmpfs size limit |
| `fstest` | descriptor access modes (`EBADF` on read-only/write-only fds), `O_NOFOLLOW` on symlinks, unlinked-but-open files (kept until closed, never shared with new files), ext2 size limits, overflowing `mmap` offsets; `preadv2`/`pwritev2` and their flags (also in `lxtest` on `/tmp`): the offset -1 as the file position, a positional write to an `O_APPEND` descriptor appending as on Linux, `RWF_APPEND`/`RWF_NOAPPEND`, `EOPNOTSUPP`/`EINVAL` for unsupported or contradicting flags, `ESPIPE` on pipes (`userspace/rwtest.h`) |
| `sh /etc/disktest.sh` | ext2: 150-file directory, 1.5 MiB file (double indirect), append, truncate, rename, cycles, symlinks, `rm -r`, space accounting |
| `e2fsck -fn target/test-disk.img` (host, after a test run) | the filesystem the tests wrote is consistent |
| `cargo test -p ext2fs` (host, needs e2fsprogs) | ext2 on a RAM disk that counts requests and can fail writes: 4 MiB read in about one device read per 32 KiB request, two flushes per write (data, then metadata), nothing written by reads, blocks moving between directories and files, a file larger than the block cache, corrupt block pointers (`EIO`, no crash), every write of a commit failing in turn (retried, nothing lost), failed data writes never exposing a deleted file's blocks; the ring path: writes into reserved blocks read back through both paths, extents clipped at the end with holes, reserved blocks out of every bitmap and inode until linked (`e2fsck` clean meanwhile, other allocations never take them), `sync` flushing the data before the metadata, `ENOENT` for inodes not in use; crashes: every write and flush of IPC and ring operations replayed up to a crash in each flush epoch, with arbitrary losses of what came after the last flush and a two-block metadata cache evicting all the time, never showing a deleted file's data in a file, directory or symlink; blocks freed before a failed commit not reused (ring or IPC) until a commit succeeds; freed inodes refusing reads, writes, truncation, permission changes (`ENOENT`); superblocks whose group or inode sizes do not fit a block refused at mount; blocks in flight for a promise counted once (the rest of the disk stays promisable, also after the promise ends; 128 reservations of 64 blocks in flight, half linked, half cut off by a truncation); freed blocks kept as ranges (unit test); socket inodes (a socket's mode and directory entry type, kept by a rename, freed by an unlink); `e2fsck` after each |
| `cargo test -p fsring` (host) | the file protocol: every request survives encode and decode (socket inodes too), `ENOSYS` for unknown operations, `EINVAL` for any field an operation does not use, transfers, names and targets bounded (`EINVAL`, `ENAMETOOLONG`), names without `/` or NUL, completions, stat, usage and directory entries round-trip |
| `timeout 1 sleep 5` | `vfork` and `SIGTERM` after the time limit (exit status 143) |
| `kill -9 1` in Bash | user space cannot kill a server (`EPERM`) |
| `kill diskfs` in the kernel monitor | the next `/data` access connects a new channel, which restarts the server; open files survive (an unlinked one fails with `EIO`), dirty pages whose write-back failed are written again; a diskfs in a crash loop is down for a while (`EIO`) and then tried again (ADR 0006); a restart still runs the boot-time program even after `/sbin/diskfs` was overwritten |
| `kill netd` in the kernel monitor, then `run nettest` | the first socket call restarts netd (new DHCP lease) and every network test passes |
| a background job holding a socket across `kill netd` | its next write fails with `EIO` instead of reaching a socket of the new netd |
| `mem` (kernel monitor) | frame and heap accounting, allocator self-test, leak checks after workloads |

During development the AI drove these tests through the QEMU monitor socket (`sendkey`,
`screendump`) and checked the screenshots.

Performance on QEMU (TCG), 30 iterations in Bash: a subshell `fork` takes about 1.3 ms, and
`fork` + `exec` of `/bin/true` about 5.7 ms.

## Security

oxidenix is a research kernel and is **not hardened for hostile workloads**. Still, every commit
went through an automated security review, and these classes of user-triggerable failures were
fixed:

- arithmetic overflows that panicked the kernel (`nanosleep`, `brk`/`mmap` sizes, `kill(INT_MIN)`, `sigreturn` stack pointer, timer values, `sendmsg`/`recvmsg` vector lengths)
- `iretq` to non-user addresses (signal handlers, ELF entry points, restored contexts), which
  would fault in ring 0
- kernel heap exhaustion through huge or numerous files, pipes, processes or arguments (quotas,
  process limit, `ARG_MAX`, a frame reserve for the heap and fallible allocations)
- directory cycles through `rename` (also via symlinks) and recursion deep enough to overflow
  the kernel stack
- spinlock self-deadlocks and sleeping while holding an inode lock
- the `sysret` non-canonical return problem, which the `iretq` return path avoids entirely

Known open issues: `getrandom` and `AT_RANDOM` are not cryptographically secure, any process may
call `reboot` or set the wall clock (everything runs as root), the kernel heap
never returns grown memory to the frame allocator, and there are no users or permissions
(everything runs as root). The ext2 driver trusts the on-disk metadata of the image it was given.
There is no IOMMU support: a server that drives a bus-mastering device (netd, diskfs) can make the
device read or write any physical memory, so such a server is effectively as trusted as the
kernel. Its program is fixed at boot (see self-healing), but a bug in it is a kernel-level bug.
A wakeup reaches nested epoll instances along every path that watches the file, as on Linux:
nesting is limited to chains of five, but a program that builds wide layers of instances
watching each other multiplies the cost of each wakeup (with interrupts off).

## Limitations and roadmap

- [x] Copy-on-write `fork`
- [x] `ENOMEM` instead of a kernel panic when memory runs out
- [x] Job control: stopping (Ctrl+Z), `fg`/`bg`, `SIGCONT`
- [x] Persistent storage: a disk driver and an on-disk filesystem
- [x] Unlinked-but-open files kept until closed
- [x] A block cache for the filesystem metadata, DMA disk I/O (virtio-blk)
- [ ] Hard links
- [x] Networking: TCP/UDP sockets, DNS, DHCP and loopback through a user-space server (`netd`)
- [x] `ping` (raw ICMP sockets)
- [x] `AF_UNIX` sockets, with descriptor passing
- [ ] IPv6, `ifconfig`
- [x] SMP with fine-grained locking, per-CPU run queues and CPU affinity
- [x] Threads: `clone`, `futex`, TLB shootdowns
- [x] A TSC clock with nanosecond resolution and exact CPU time
- [x] High-resolution timers (TSC-deadline or one-shot local APIC)
- [x] `eventfd`
- [x] `epoll`
- [x] A page cache with file-backed shared mappings (and programs mapped, not copied)
- [x] Node.js: `node -e 'console.log(1+1)'`, files, timers, workers, HTTP, DNS, WebAssembly
- [x] Node.js: child processes with pipes (`AF_UNIX` socketpairs), `fork` and handle passing,
  `net` on Unix paths
- [ ] Node.js: `os.networkInterfaces()` (netlink), `statx` and `io_uring` (libuv falls back to
  `stat` and epoll)
- [ ] Dynamic linking, real entropy, users and permissions

## Development history

| Step | Commit message |
|---|---|
| Visible boot | make the shell visible and keyboard input work under bootloader 0.11 |
| Memory | physical frame allocator and kernel heap |
| User space | run static musl programs in ring 3 |
| Processes | processes with preemptive scheduling, fork, execve and wait4 |
| Filesystem | in-memory filesystem, initramfs, file descriptors and pipes |
| Terminal | TTY with line discipline and an interactive BusyBox shell at boot |
| Signals | POSIX signals with handlers, Ctrl+C and EINTR |
| Bash | boot into an interactive Bash; harden signal and exec paths |
| Copy-on-write | copy-on-write fork and an optimized dev profile |
| Out of memory | grow the kernel heap and turn memory exhaustion into errors |
| Job control | job control with stopped processes and syscall restart |
| Disk | persistent ext2 data disk on an ATA drive, wall-clock time |
| Microkernel | move the disk driver and ext2 into a user-space server |
| CI | run the self-tests in QEMU on every push |
| Self-healing | restart a crashed filesystem server on the next access |

Run `git log` for the full history, including the security fixes between these steps.

## Acknowledgements

oxidenix builds on excellent open source work: the
[`bootloader`](https://github.com/rust-osdev/bootloader) and
[`x86_64`](https://github.com/rust-osdev/x86_64) crates of the rust-osdev community,
`pc-keyboard`, `pic8259`, `linked_list_allocator`, `heapless`, `spin`,
`noto-sans-mono-bitmap`, and in user space [musl](https://musl.libc.org/),
[BusyBox](https://busybox.net/) and [GNU Bash](https://www.gnu.org/software/bash/) as packaged
by [nixpkgs](https://github.com/NixOS/nixpkgs).

## License

oxidenix is free software: you can redistribute it and/or modify it under the terms of the
GNU General Public License, version 3, as published by the Free Software Foundation. See
[LICENSE](LICENSE) for the full text.

The user-space programs that the build fetches from nixpkgs (GNU Bash, BusyBox, musl) are not
part of this repository and keep their own licenses.
