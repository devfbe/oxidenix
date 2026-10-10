<div align="center">

# oxidenix

**A Unix-like x86_64 kernel written from scratch in Rust, designed, implemented and debugged by an AI.**

It boots in QEMU and runs an unmodified, statically linked **GNU Bash 5.3** and **BusyBox**
on top of a Linux-compatible system call interface. It is a **microkernel**: the kernel
implements no Linux system call. Linux programs trap into a user-space **Linux server** (one
instance per process tree) that implements Linux over the kernel's mechanisms, and the disk
driver, the ext2 filesystem, the network stack and `/proc` run as user-space servers too.

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
  HTTP over loopback, DNS, WebAssembly, `os.networkInterfaces()`, `fs.watch`, child processes (`spawn`, `exec`, `execSync`,
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
  user-space server (procfs, over the I/O rings) and the Linux server (each process's part,
  `/proc/self/fd`'s magic links), from the kernel's own process accounting (CPU time per
  process and CPU, memory, load average).
- **Filesystem**: each process tree's root is a tmpfs of its Linux server, unpacked from a
  cpio initramfs, with files, directories, symlinks, socket inodes, a size limit, and `/dev`
  (console, tty, ptmx, null, zero, devpts, the `/dev/fd` links) as the server's devtmpfs.
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
- **Terminal**: Linux's N_TTY line discipline in the Linux server (canonical and raw mode,
  VMIN/VTIME, echo and line editing, signals from ^C/^Z/^\, flow control, output processing),
  the controlling terminal and job control's terminal side, pseudo-terminals (`/dev/ptmx`,
  devpts), the ANSI escape sequences BusyBox, readline and ncurses use, a German keyboard
  layout and UTF-8. The kernel keeps the console as a raw device.
- **~120 Linux system calls**, all implemented by the Linux server, enough for Bash, BusyBox
  and Node.js (see [System calls](#system-calls)).

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

Node.js's smoke tests: `OXIDENIX_NODE=1 OXIDENIX_AUTORUN=$PWD/../userspace/node/run-node.sh cargo run`
(from `kernel/`) runs every `userspace/node/tests/*.test.mjs` (the builder puts them in the root
filesystem as `/usr/lib/node-tests` whenever `OXIDENIX_NODE` is set) and ends QEMU with their
result: `fs` (sync, callback and promise APIs, descriptors, streams, directories, links, times,
`fs.watch` and its promise form, `watchFile`, on `/tmp` and `/data`), `os` (every function),
`process` (signals sent with `process.kill` and caught, `hrtime`, `memoryUsage`,
`resourceUsage`, `cpuUsage`, `uptime`, `umask`, `chdir`, ids, title), `crypto` (hashes, HMAC,
randomness, PBKDF2, scrypt, HKDF, AES-CBC/GCM, ChaCha20-Poly1305, RSA, ECDSA, Ed25519, ECDH,
X25519, Web Crypto), `zlib` (gzip, deflate, brotli, zstd, streams), `timers` (and events),
`readline` (on standard input), `worker` (`worker_threads`, `SharedArrayBuffer` and Atomics,
transfers, memory limits, `BroadcastChannel`), `net` (TCP, UDP, DNS lookups and the resolver,
HTTP with keep-alive and chunked bodies, `fetch`, HTTPS with a test certificate for
`localhost` in the tests' directory, a request to example.com), `modules` (path, url, util,
buffer, stream, web streams, perf_hooks, async_hooks, vm, v8, WebAssembly, ESM, JSON and
CommonJS imports, Intl), `memory` (large buffers and garbage collection in 256 MiB) and `npm`
(an npm-style `node_modules` tree with `exports` conditions and nested versions, and a small
build). `NODE_TESTS="fs os"` in the script's environment picks tests.

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
 ┌────────────────────────────────── user space (ring 3) ───────────────────────────────────┐
 │  GNU Bash 5.3   BusyBox 1.37   Node.js 24   test programs  (static musl, restricted mode)  │
 │        │ syscall, exception: back to the server on the same thread (restricted_enter)     │
 │  Linux server instance (one per process tree, servers/linux): descriptors, VFS, tmpfs,    │
 │  /dev, pipes, terminals, sockets, processes, signals, futex, mmap, ELF loader, /proc/<pid>│
 │        │ kernel calls (crates/restricted)          │ I/O rings: shared memory, grants     │
 │  native servers (oxrt::sys): diskfs (ext2, virtio-blk)  netd (smoltcp, virtio-net) procfs │
 └────────┬───────────────────────────────────────────┬──────────────────────┬──────────────┘
   kernel calls / faults / IRQs                 channels, IPC            in/out, DMA
 ┌────────▼───────────────────────────────────────────▼──────────────────────▼──────────────┐
 │  restricted mode   two views per address space, enter/trap, kicks, exceptions to the server│
 │  memory objects    anonymous, paged (the server's pager), the server's page cache, files  │
 │  address spaces    mappings, demand paging, COW, futex keys, TLB shootdowns (PCIDs)       │
 │  threads           processes as containers, SMP fair scheduler, timers, clocks, kill      │
 │  IPC and channels  service registry, channel offers, grants, doorbells                    │
 │  devices           I/O ports, IRQ lines and DMA areas for servers; PCI, ACPI              │
 │  console device    raw bytes: framebuffer (ANSI) and keyboard (the terminals: the server) │
 │  CPU               GDT, TSS + I/O bitmap, IDT, local + I/O APIC, SSE; the boot image      │
 └──────────────────────────────────────────────────────────────────────────────────────────┘
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
│       ├── process/             processes and threads, the servers and process trees the kernel
│       │   │                    starts (mod.rs)
│       │   ├── task.rs          tasks (threads), thread groups (processes as containers)
│       │   ├── linux.rs         restricted mode: instances, the two views, the Linux server's calls
│       │   ├── native.rs        the native servers' calls (oxrt::sys)
│       │   ├── exec.rs          a native server running its program anew
│       │   ├── exit.rs          thread and process exit, the kernel's wait for its own processes
│       │   ├── kill.rs          the kernel's kill of a process; what waits ask (kicked, dying)
│       │   ├── sched.rs         run queues, wait queues, context switch, idle
│       │   ├── address_space.rs areas, demand paging, copy-on-write
│       │   ├── vm.rs            mappings as the calls ask for them (placement, remap, discard)
│       │   ├── tlb.rs           which CPUs use an address space, TLB shootdowns
│       │   ├── syscall.rs       syscall entry/return, dispatch by caller
│       │   ├── futex.rs         futex wait queues keyed by address space, object or instance
│       │   ├── clock.rs         the clocks a thread reads
│       │   ├── loader.rs        ELF segments mapped from the page cache, the initial stack
│       │   ├── elf.rs           ELF64 parser
│       │   ├── channel.rs       channels: the kernel's part of the I/O rings
│       │   ├── ipc.rs           services and message passing
│       │   ├── irq.rs           device interrupts for user-space drivers
│       │   └── uaccess.rs       copies to and from user memory
│       ├── fs/                  the boot image (mod.rs, cpio.rs: its programs by name) and the
│       │                        memory objects: page cache, paged and file objects (cache.rs)
│       ├── drivers/             framebuffer console (console.rs, glyphs.rs), the console
│       │                        device the server's terminals drive (console_device.rs),
│       │                        PS/2 keyboard (keyboard.rs), CMOS clock (rtc.rs),
│       │                        serial port mirror (serial.rs), PCI scan (pci.rs),
│       │                        ACPI MADT (acpi.rs)
│       ├── sync.rs              IrqSpinLock: fair, interrupt-safe ticket lock
│       ├── time.rs              TSC clock source, clock synchronization between CPUs
│       ├── timer.rs             per-CPU timer queues on the local APIC timer
│       └── shell/               built-in kernel monitor (fallback shell)
├── servers/
│   ├── linux/                   the Linux server (restricted mode; memory, time, the descriptor table, poll, select and epoll,
│   │                            pipes, eventfd, paths, tmpfs, /data, AF_UNIX, internet sockets over the channel to netd,
│   │                            netlink, inotify, /proc and /sys)
│   ├── diskfs/                  user-space ext2 server with its virtio-blk driver (blk.rs)
│   ├── procfs/                  /proc's system-wide files and /sys over the rings (main.rs:
│   │                            the service, tree.rs: inodes, render.rs: cpuinfo, version)
│   ├── netd/                    network server: virtio-net driver (virtio_net.rs), loopback
│   │                            (nic.rs), the instances' sockets on smoltcp over their
│   │                            channels (service.rs), DHCP
│   └── ringtest/                the self-tests' channel service (test mode only)
├── crates/
│   ├── ext2fs/                  ext2 as a library over a `Device` trait
│   ├── netring/                 the socket protocol over a channel (Linux server <-> netd): requests,
│   │                            control blocks in the shared area, byte rings, interface records
│   ├── netlink/                 rtnetlink's messages (the Linux server's NETLINK_ROUTE sockets)
│   ├── procproto/               native process and system information, /proc's text formats
│   ├── virtio/                  virtio legacy PCI transport and virtqueues (diskfs, netd)
│   ├── restricted/              restricted mode: shared region layout, register page, kernel calls
│   ├── ring/                    SPSC descriptor rings and the channel layout (I/O rings)
│   ├── fsring/                  the file protocol over the rings (Linux server <-> diskfs, procfs)
│   └── oxrt/                    runtime for servers: entry, syscalls, heap, port I/O
├── builder/                     host tool: rootfs + cpio + boot image + ext2 data disk + QEMU
└── userspace/                   C test programs, build script, rootfs and data disk templates,
                                 Node.js (node/: its build, smoke tests and their runner)
```

About 13,100 lines of Rust (without comments and blank lines) in the kernel (16,600 before R9 took its Linux code out) and 30,700 in the servers (the Linux server the largest), their libraries and runtime, plus a small host-side builder.

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
   clock (see Time) → the boot image (the ramdisk) → process subsystem (SSE, syscall MSRs, process 0) → the other
   CPUs (INIT and STARTUP IPIs; each runs a real-mode trampoline from a page below 1 MiB into
   long mode, then compares its TSC with the bootstrap CPU's and sets up its GDT, TSS, GS
   block, local APIC timer and idle task) →
   **interrupts on**.
3. The kernel starts the servers from the boot image: `/sbin/diskfs` asks for its I/O ports, mounts the ext2 disk
   and registers as service `diskfs` for channels (each Linux server instance connects one and
   mounts the disk at `/data`). Then the kernel
   scans PCI for a virtio network card and starts `/sbin/netd` with its ports, interrupt line
   and a DMA area; netd registers as service `net` once DHCP has configured the interface
   (or after three seconds without an answer). Last, `/sbin/procfs` registers as service
   `procfs` for channels (each Linux server instance connects one for `/proc` and `/sys`).
4. Process 0 (the kernel monitor) spawns `/bin/bash` in a new process tree, which gets the
   console device; its Linux server gives Bash descriptors 0-2 on the console and makes the
   console its controlling terminal. When Bash exits, the monitor takes the console back.

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
  much (`Slab` in `/proc/meminfo` is what of it is in use); before it grows, the size
  classes give back the slabs whose slots are all free
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
  thread group (`ThreadGroup`): a container of tasks that share an id, an address space
  (`Mm`) and their end (ADR 0010, ADR 0011). A task owns its references, its I/O permissions
  and a Linux thread's `CLONE_CHILD_CLEARTID` word; FPU state and TLS pointer are saved per
  task; scheduling state is atomic. The process table maps the kernel's thread ids to tasks
  and process ids to thread groups. Linux's process model (pids per process tree, the tree,
  process groups, sessions, signals, `fork`, `clone`, `execve`, `wait4`) is the Linux
  server's, over `proc_create`, `thread_create`, `exec_space`, kicks, kills and thread-exit
  events.
- **Ending** (`exit.rs`, `kill.rs`): a thread that exits clears its `CLONE_CHILD_CLEARTID` word
  and wakes its futex (that is how `pthread_join` works) and leaves no trace; the last thread
  ends the process. The kernel kills a whole process itself only when memory runs out, a
  native server faults, the Linux server fails or the monitor says so (`kill`); its own
  processes (a tree's first process, the servers, the service processes) are zombies until it
  reaps them, the ones the Linux server made are the server's to reap.
- **One kernel stack per task** (64 KiB, see Memory). A context switch saves callee-saved registers,
  the FPU/SSE state (`fxsave`), the FS base (musl's TLS pointer), and switches CR3, the TSS
  stack and I/O bitmap, and the syscall stack in the CPU block.
- **Per-CPU run queues, fair by virtual runtime** (as Linux's CFS): each thread's run time
  is counted scaled by Linux's weight of its nice value (`setpriority`, inherited by `fork`),
  and a CPU runs the thread with the smallest. A time slice is the thread's weighted share
  of a 12 ms period among the CPU's runnable threads (at least 1.5 ms), its end a deadline
  in the CPU's timer queue (see Time). A woken or new thread that is owed time (2 ms of it,
  by its weight) preempts the running one at once, by IPI on another CPU; a sleeper is
  credited at most half a period, a new thread starts a slice behind. So CPU shares come
  out as Linux's (nice -5 three times nice 0's) and a thread that wakes next to a nice -20
  loop runs at once. A Linux thread holding one of the Linux server's locks runs with nice
  -20's weight and goes to the front when preempted or woken holding it, so a low nice
  value never holds up the other threads of its tree (priority inversion). A woken task goes to an idle CPU if there is
  one (preferring the CPU it last ran on); the waker claims that CPU atomically and wakes it
  with an IPI, so a burst of new tasks spreads over all idle CPUs. An idle CPU steals work
  from the others; an idle CPU counts only tasks it may run as its work (tasks pinned to a busy
  CPU once kept the others looping with interrupts off, so their timers never fired). Each task has a CPU affinity mask (`sched_setaffinity`, inherited by
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
  serializes wakeups with the task descheduling itself. The console device, IPC, channels,
  futexes and timed sleeps all use this protocol.
- **Waiting for several things at once** is the Linux server's since R6e (its `poll`,
  `select` and `epoll`, see the Linux server below): the kernel's part is a wait for any of
  several words of the server's memory (`server_wait`), with a deadline; a kick of the
  thread (a signal for it, R8) ends it.
- The kernel is non-preemptive: only user code is preempted, and an interrupt in kernel mode
  never schedules. Syscalls nevertheless run with interrupts enabled, so a long syscall does
  not delay timer ticks or device interrupts on its CPU. An interrupt that ends the time
  slice or wakes a task sets the CPU's `need_resched`, and the switch happens at the next
  return to user space (from that interrupt or from the syscall it interrupted), so no
  request is lost. Long kernel work also checks it between pieces (`cond_resched`, as on
  Linux): a console write that redraws the whole screen lets a woken task run. Locks are
  `IrqSpinLock`s (fair tickets, interrupts off while held), and long work under a lock is
  cut into bounded pieces (the console draws at most 64 cells per lock hold).
- New threads start by *returning from a syscall*: their kernel stack is pre-filled with a
  register frame that `user_return` consumes (a Linux thread's starts in its server, which
  enters its program).

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
  stub (`interrupts/entry.rs`) that builds the same frame and calls one dispatcher, `trap`. A
  Linux program's CPU exceptions go to its server, which raises Linux's signal (`SIGSEGV`,
  `SIGFPE`, `SIGILL`, `SIGBUS`, `SIGTRAP`); a native server's end it. In the kernel they panic
  with the faulting address. In test mode a panic ends QEMU with a failure.
- All paths return through `user_return`, which uses **`iretq`** (not `sysret`). A signal can
  therefore interrupt user code at any instruction and `rt_sigreturn` restores every register
  exactly, and the classic `sysret` non-canonical-address problem cannot occur.
- Dispatch is by caller: a Linux program's `syscall` goes to its server (restricted mode), the
  server's calls are the kernel's interface for it (`crates/restricted`, `linux::server_call`),
  a native server's are `process/native.rs`'s. The kernel has no table of Linux numbers.

### Restricted mode and the Linux server

The kernel's Linux implementation moved out into a user-space **Linux server**
(`servers/linux`; `docs/design/linux-server.md`, ADRs 0001-0011 in `docs/decisions/`). Phase
R1 put the mechanism in place, with every system call passed through; the phases since moved
memory, time, files, terminals, sockets, processes and signals into the server, and R9 removed
the pass-through: the kernel implements no Linux system call.

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
- In phase R1 the server handed every Linux system call back with `legacy_syscall` (1011), which
  ran the kernel's implementation on the registers in the register page; phase by phase the
  calls moved into the server, and R9 removed the pass-through (ADR 0011): the last ones
  (`futex`, `arch_prctl`, `uname`, `sysinfo`, `clock_settime`, `settimeofday`, `prlimit64`,
  `getrlimit`, `setrlimit`, `getcpu`, `getrandom`, `reboot`, `ioperm`) are the server's over
  narrow mechanisms (`futex_wait`, `futex_wake`, `futex_requeue` on program memory,
  `thread_fs`, `clock_set`, `power`), `/dev` is the server's tmpfs, and a call the server does
  not implement is `ENOSYS` (said on the console: `syscall N not implemented`). Numbers of 1000
  and above are not Linux's: the server answers them with `ENOSYS` too. An exception in the
  server kills its process with a diagnostic.
- **Processes and signals are the server's** (phase R8, ADR 0010; `process.rs`, `signal.rs`,
  `exec.rs`, `timer.rs`): pids per instance (pid 1 is the tree's first process), the process
  tree, process groups and sessions, `fork`/`vfork`/`clone`/`clone3` (one kernel call clones
  the address space copy-on-write, `proc_create`; another makes the thread, `thread_create`),
  `execve` with the ELF loader in the server (it maps the segments, the interpreter and the
  stack with its auxiliary vector itself), `exit`/`exit_group`, `wait4`/`waitid`, Linux's
  signals (actions, masks, queued real-time signals, `sigaltstack`, `kill`/`tgkill`/
  `sigqueue`, frames and `rt_sigreturn` written by the server, the restart codes, default
  actions, stops and `SIGCONT`, `SIGCHLD` and its reaping rules), job control's orphaned
  process groups, interval timers, the credential getters and `prctl`'s basics, and
  `/proc`'s records of processes. The kernel keeps processes and threads as containers: it
  kicks a thread (the server delivers before the thread's program runs again), kills one,
  reports a thread's end (`EVENT_THREAD_EXIT`), and hands the server a program's faults as
  exceptions. Servers such as diskfs are not Linux programs and keep the kernel's interface.
- **Kernel objects by handle** (phase R2): each instance has a handle table, so an object can be
  used from any thread of its process tree. Memory objects (zero-filled, committed; the kernel's
  page cache objects) are created, read and written from the server's memory, and mapped shared
  or private into the calling thread's program view, protected and unmapped. Addresses the server
  passes for its own memory must lie in its shared region; mappings must lie below 64 TiB.
- **Paged memory objects**: the server supplies their pages. Each instance has a **pager
  process** (`linux-pager`: the server's view alone, protected like the servers) whose thread
  waits for page requests (`pager_wait`) and answers them (`mo_supply`). A thread that needs a
  missing page, by its own access or by the kernel copying from a mapping, sleeps until the
  page is there: the request cannot go to that thread's own server, which may be the kernel's
  caller. Supplied pages are committed and stay until the object goes. The pager's process
  ends when the tree's last program is gone. The wait for a page ends when the thread dies
  (`SIGKILL`, an exiting process), so a pager that never answers cannot make it unkillable. A
  pager that cannot supply a page says so (`mo_fail`): the access fails (`SIGBUS`, as an I/O
  error under `mmap` on Linux; the faulting thread waits for the page from before it asks, so
  even an answer that comes at once reaches it), and a later one asks again. A request is
  queued once until the pager takes it, and a waiting thread asks again only once its own
  request was answered or overtaken (the page came and went, or was cut off); if the pager's
  process dies, every wait for it ends (`EIO`), and so does every wait for an object whose last
  handle the pager closed (no answer could name it any more).
- **The server's runtime** (phase R3): a heap in its shared region that grows on demand
  (`shared_map`, committed memory), and a mutex for data shared by all threads of the tree
  (Drepper's three-state futex lock over the kernel's futex, which keys the server's memory by
  instance and address since that memory is pinned and in no address space's areas).
- **Memory semantics are the server's** (phase R4): `mmap`, `munmap`, `mprotect`, `mremap`,
  `madvise`, `msync` and the `mlock` family are the server's. The server
  checks the arguments and turns them into kernel mapping calls: `mo_map` with anonymous
  private memory (handle 0), a new memory object for shared anonymous memory, or a file object
  of the server's; placement (a hint, `MAP_FIXED`, `MAP_FIXED_NOREPLACE`), `MAP_NORESERVE` and
  `MAP_POPULATE` are flags of `mo_map`. The native servers map their memory with `mo_map` too
  (R9).
- **Program memory and time** (phase R5): the server reads and writes the program's memory
  directly in its view. A fault there is resolved as the program's own would be (demand paging,
  copy-on-write); one the program may not make resumes at the fixup of the server's copy
  routine, registered once (`set_usercopy`), so the call reports `EFAULT`, as Linux's
  `copy_to_user` does. The server checks every program pointer against 64 TiB first. The
  clocks, `nanosleep`, `clock_nanosleep`, `gettimeofday`, `time` and `sched_yield` are the
  server's now, over the kernel's `clock_read`, `sleep_until` and `yield`, and so are
  `sched_getscheduler` and `sched_getparam` (one policy, `SCHED_OTHER` with priority 0, for
  the thread ids of the server's process table). A call interrupted by a signal is restarted
  as Linux's restart codes say (R8: by the server's own signal delivery).
- **Pipes are the server's** (phase R6a), the first kind of file it implements, and
  `eventfd` followed (R6b). Blocking reads and writes wait on interruptible server futexes,
  and `sendfile` runs in the server, through its memory. A write without readers raises
  `SIGPIPE` and fails with `EPIPE`, as on Linux. Until R6e such a
  file was a placeholder in the kernel's descriptor table, which the server looked up for
  every call (`kfd_lookup`) and reported the readiness of (`kfd_ready`).
- **The descriptor table is the server's** (phase R6e, ADR 0009; `fdtable.rs`, `files.rs`,
  `poll.rs`, `epoll.rs`): per process, a table of open file descriptions (the
  server's files, with their status flags) and close-on-exec bits, with `close`,
  `close_range`, `dup`, `dup2`, `dup3`, `fcntl`, `FIONBIO`/`FIOCLEX`/`FIONCLEX` and
  RLIMIT_NOFILE. A description goes, and its file closes, with its last reference:
  descriptors, calls that use it (another thread's close never takes a file from under a
  call, as Linux's fdget) and descriptors in flight. Each thread record of the server's process
  table holds its table (shared by `CLONE_FILES`, copied by a fork); an execve makes the new
  program's table after its point of no return (closing the close-on-exec descriptors on that
  thread before the program runs), and an exiting thread lets its table go itself (a thread
  the kernel ended in its program: the worker thread). A descriptor's lookup is a lock
  and a reference count, no kernel call. `poll`, `ppoll`, `select`,
  `pselect6` and `epoll` are the server's: every file reports its readiness changes to its
  description's watch list (`files::ready`), which pollers and epoll interests subscribe
  to; readiness is asked of each file when it is checked. epoll has Linux's semantics:
  interests keyed by descriptor and description, level- and edge-triggered, `EPOLLONESHOT`,
  `EPOLLEXCLUSIVE`, nested instances (at most 4 deep, no cycles: `ELOOP`), regular files
  `EPERM`. poll and select also wait on the control-block words netd wakes, so a poll on a
  socket does not wait for the instance's net thread.
- **`AF_UNIX` sockets are the server's** (phase R7a, `unix.rs`, `sockcalls.rs`, `scm.rs`):
  `socket` and `socketpair` for the family come to the server (since R7b every family's do),
  and so does every socket call on one of its sockets' descriptors.
  Stream, datagram and seqpacket sockets, socket pairs, names as socket inodes of the tmpfs
  and of `/data` and in the instance's abstract namespace (autobind too), `listen` with its
  backlog, `accept4`, `connect`, `sendmsg`/`recvmsg` and their relatives (`sendmmsg`,
  `recvmmsg`), `shutdown`, `getsockname`/`getpeername`, the options of `SOL_SOCKET`
  (`SO_SNDBUF` with Linux's accounting, `SO_PASSCRED`, `SO_PEERCRED`, timeouts, ...),
  `FIONREAD`; `EPIPE` raises `SIGPIPE` unless `MSG_NOSIGNAL`, a
  closed end with unread data resets the connection, poll and epoll see Linux's readiness
  (`POLLRDHUP` included). Descriptors pass between processes (`SCM_RIGHTS`) as references
  to their open file descriptions while in flight (the receiver's descriptor with
  `MSG_CMSG_CLOEXEC` if asked), so they outlive the sender's close, and any kind of file can
  be passed (an O_PATH descriptor, an epoll instance, a terminal); sockets
  in flight that nothing but messages in flight keeps (a cycle) are collected as Linux's
  `unix_gc` does (a description's references against those in flight; asked for when a
  reference to one in flight goes, also by exit or exec), on the instance's worker thread,
  which serves no page; at most 16 Ki descriptors are in flight in an instance. No server
  lock the pager takes is held while program memory is copied. A call keeps the description
  it works on until it returns, so another thread's close does not end a blocked receive or
  accept. `SCM_CREDENTIALS` carries the sender's ids and may name only processes of the
  caller's tree.
- **Paths are the server's** (phase R6c.2): the server keeps a record per working-directory
  context (cwd and umask), shared by `CLONE_FS` (since R8 its thread records hold them; until
  then each context of the kernel's carried the server's record). Every call that takes a path (`open`, the `stat` family,
  `access`, `mkdir`, `unlink`, `rename`, `symlink`, `readlink`, `chmod`, `truncate`, `statfs`,
  `chdir`, `getcwd`, `execve`'s program) resolves in the server: `.` and `..` by name, symlinks
  by reading them (at most 16), over the server's own filesystems (until R9 `/dev` was the
  kernel's tree, reached through handles on its inodes). `umask` is real now (the kernel's was
  fixed at `022`).
- **The root is the server's own tmpfs** (phase R6c.2c): each instance unpacks the boot image's
  initramfs into a tmpfs of its own when it first resolves a path (`initramfs`: the archive as
  a read-only object; `mo_from_image`: a file object over a member's bytes, copied only page
  by page when needed and written privately). The namespace finds mounts by name, the longest
  first: `/dev` (a tmpfs of the server's, R9), `/proc` and `/sys` (procfs's and the server's,
  I/O rings step 5) and `/data` (below). The kernel reads only its own programs from the
  initramfs (the servers); a program a tree runs comes from the tree's tmpfs. Its directories
  and symlinks (and socket inodes) live in the server; a file's contents are a **file object** of the kernel's
  (`mo_create_file`), a memory object that grows and shrinks like a file, charged to the same
  tmpfs limit, read and written straight into the program's memory (`mo_file_read`,
  `mo_file_write`) and mapped with `mo_map`. Open files are the server's, whose calls (`read`,
  `write` and their vectored and positioned forms, `lseek`, `fstat`, `ftruncate`, `getdents64`,
  `fstatfs`, `sendfile`, `mmap`) the server answers. ETXTBSY works as on Linux: a
  shared mapping through a writable descriptor and a program run from the file each keep a
  **hold** on the file object (`mo_hold`), and the kernel tells the server when the last holder
  is gone. Before answering ETXTBSY the server waits until it has taken in every release the
  kernel reported so far, so a program that ended and was reaped no longer keeps its file busy. The tree has a lock per inode and, for renames and removals, a lock of its own (as
  Linux's rename mutex), under which alone two inode locks are ever held. Files there report
  device `0x1a` (`/dev`'s tmpfs `0:5`, devpts `0:0x18`: each mount its own filesystem); renames
  between it and the other mounts fail with `EXDEV`, and the mount
  points with `EBUSY`. Its regular files and directories are always ready for `poll` and
  `select`, and `epoll` refuses them (`EPERM`), as on Linux.
- **File times, watches and copies** (the server's, on tmpfs and `/data`): tmpfs keeps four
  times per inode in nanoseconds (`statx` reports its birth time); writes and truncation move
  the modification and change times, chmod and links the change time, a directory's entries
  its own, reads the access time as with `relatime`, `utimensat`/`futimens`/`utimes` set them
  (`UTIME_NOW`, `UTIME_OMIT`). On `/data` (ext2: seconds) a write sets the times when it enters
  the page cache, as on Linux; diskfs, which records its own time when the data reaches it,
  gets them after each write-back (`SETTIMES`, after the `WRITE`s), and stat reports them
  until then. `inotify` (`servers/linux/src/inotify.rs`): watches on tmpfs and `/data` inodes
  (the pseudo files of `/proc` and `/sys` report nothing), the events of creating, writing, changing,
  opening, closing, moving (with cookies) and removing names and inodes, `IN_ONESHOT`,
  `IN_ONLYDIR`, `IN_MASK_ADD`/`IN_MASK_CREATE`, `IN_IGNORED`, merged events, `IN_Q_OVERFLOW`,
  `FIONREAD`, poll and epoll readiness: Node.js's `fs.watch`. Watches are keyed by inode and
  pin nothing; an event about a name goes to the directory the inode was last created, found or
  renamed in; `IN_DELETE_SELF` comes when a removed inode's last open file description closes;
  Linux's default limits hold (128 instances, 8192 watches, 16384 queued events). `copy_file_range` copies within
  one of the server's filesystems (`EXDEV` across them). The chown family checks its target
  and keeps every file root's (one user).
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

Signals are the Linux server's (phase R8, `servers/linux/src/signal.rs`); the kernel has none
(R9): it kicks a thread so that its server looks at its signals, and kills threads.

- Per process: 64 actions, the pending set of signals sent to the process, and the interval
  timer. Per thread: the blocked mask and the pending set of signals sent to that thread
  (`tkill`/`tgkill`, faults). A process signal goes to the first thread that does not block it,
  which is woken (or interrupted on its CPU by an IPI). `fork` inherits actions and mask,
  `exec` resets caught signals.
- Default actions act on the whole process, as on Linux: a fatal signal ends every thread, and
  a stop signal starts a group stop that every thread joins; the last one to stop reports it
  to the parent, and `SIGCONT` resumes them all.
- **Delivery** happens on every return to user space (after syscalls and after timer
  preemption, by the thread's server before its program runs again). Default actions
  terminate, ignore or stop. For handlers, the server writes Linux's signal frame on the user
  stack: restorer address, saved register frame, saved mask, the
  FPU/SSE state (asynchronous handlers would otherwise clobber it) and `siginfo`.
- **Temporary masks**: `rt_sigsuspend`, `ppoll` and `pselect6` wait with the mask they are
  given. If a signal interrupts them, the mask stays until it is delivered, so the handler
  runs with it and the caller's own mask comes back when the handler returns; otherwise the
  caller's mask is put back at once and a signal the temporary one held off stays pending.
- Blocking calls (terminal and pipe I/O in the Linux server, `wait4`, `nanosleep`,
  `poll`/`select`, `pause`) return `EINTR`, but only after checking for available data or a
  finished child first.
- **Orphaned process groups**: an exit that leaves a process group orphaned with stopped
  members sends it SIGHUP and SIGCONT (POSIX), so stopped jobs do not stay stopped for ever.
- **Syscall restart**: a call interrupted by a stop, or by a handler installed with
  `SA_RESTART`, is rewound to its `syscall` instruction and runs again, so `cat` survives
  Ctrl+Z / `fg`. Sleeps and polls report `EINTR` instead, as on Linux.
- **Job control**: stopped processes leave the run queue until `SIGCONT` (or `SIGKILL`), and
  parents learn about stops and continues through `SIGCHLD` and `wait4` with `WUNTRACED` and
  `WCONTINUED`. `wait4` supports the POSIX process group selectors (`pid` 0 and < -1).
- The terminal (the Linux server's) turns Ctrl+C, Ctrl+\ and Ctrl+Z into `SIGINT`, `SIGQUIT`
  and `SIGTSTP` for the foreground process group, and stops background readers and writers
  with `SIGTTIN` and `SIGTTOU`.

### Filesystem

The kernel has no filesystem (R9): every file a program sees is its Linux server's (see the
Linux server above for the details).

- **The namespace** (`servers/linux/src/namespace.rs`): a mount table per instance, the
  instance's tmpfs at the root, `/dev` (a tmpfs: devtmpfs), devpts at `/dev/pts`, `/data`
  (diskfs's ext2 over the rings), `/proc` and `/sys`. Path resolution follows symlinks with
  loop detection and supports the `*at` family relative to directory descriptors.
- **Initramfs**: the builder writes a `newc` cpio archive. Each instance unpacks it into its
  tmpfs without copying file contents: a file's pages are read from the boot image until a
  page is written; then it gets its own frame. The kernel reads its own programs (the servers)
  from the same archive by name.
- **tmpfs**: a file's contents are a file object of the kernel's (as Linux's tmpfs, pages in
  memory). The pages are committed memory, and all of them together may take half of the
  commit limit (`ENOSPC` beyond, as Linux's default tmpfs size).
- **Open files** are the server's shared descriptions with offset and flags, so `dup`, `fork`
  and close-on-exec behave as on Linux; pipes (64 KiB, blocking, EOF and `EPIPE`) and
  `eventfd` are files of the server's.

### Microkernel architecture

oxidenix was restructured into a microkernel step by step, keeping the Linux syscall
interface working after every step; since R9 the kernel implements no Linux system call. It
keeps memory management, scheduling, restricted mode, IPC and channels and interrupt
dispatch; drivers, filesystems, the network stack and Linux itself are user-space servers.

- **IPC** (`process/ipc.rs`): a privileged server registers a service name (`ipc_register`),
  then loops over `ipc_receive` and `ipc_reply` (oxidenix syscalls 1000-1002). The kernel is
  the client on behalf of user programs: `call` queues a request and sleeps uninterruptibly
  until the reply. Messages up to 64 KiB are copied through the kernel. Only procfs still
  serves such requests (the kernel's `/proc`); diskfs and netd serve the Linux server
  instances over channels (shared-memory rings, `process/channel.rs`), whose offers are the
  kernel's control requests.
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
- **Filesystem servers over the rings**: the kernel is no filesystem client. diskfs (`/data`)
  and procfs (`/proc`'s system-wide files and `/sys`) serve each Linux server instance over a
  channel of the I/O rings in the file protocol (`fsring`); the kernel only hands out the
  channels (its IPC carries their offers, nothing else) and starts the servers.
- **Fault isolation**: when a server dies, its services are marked dead and every pending
  request fails with `EIO`; the kernel and the rest of user space keep running.
- **Self-healing**: a Linux server instance whose channel's service died connects a new
  channel, which starts the server again (in the context of the requesting program, which may
  sleep), and goes on: diskfs's inode numbers live on disk (the instance names the inodes it
  uses again, stale ones fail with `EIO`), procfs's name what a file is, so files and
  directories that were open before the crash stay usable. Requests that were in flight during
  the crash still fail with `EIO`, since they may or may not have been carried out. The restart policy (ADR 0006) stops crash loops without giving a service
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
  The kernel starts each server from the read-only boot image (its pages committed at boot),
  so nothing a program writes can smuggle a different program into a privileged process.
- **Protected servers**: Linux programs cannot reach them: a program's `kill` names only
  processes of its own tree (the Linux server's pid namespace). Only the kernel ends them (the
  monitor's `kill <pid|name>` does, for testing).
- **Servers in Rust**: `servers/diskfs`, `servers/netd`, `servers/procfs` and
  `servers/ringtest` are `no_std` Rust programs built for `x86_64-unknown-none` as a static
  `ET_EXEC` binary, using `crates/oxrt` for its entry point, the kernel's own calls
  (`oxrt::sys`, `kernel/src/process/native.rs`: no Linux numbers), heap and port I/O.
- **procfs: the Linux personality** (`servers/procfs`). The kernel has no `/proc`. It keeps
  native accounting (user and system ticks per process and CPU, start time, mapped pages,
  command line, program path, a Linux-style load average) and hands it out as fixed binary
  records through `proc_query` (syscall 1005, privileged servers only, `crates/procproto`).
  The procfs server renders the system-wide files from them on every read (`/proc/stat`,
  `meminfo`, `loadavg`, `uptime`, `cpuinfo`, `version`, `filesystems`, `/proc/sys/kernel/*`,
  oxidenix's own `/proc/counters`) and the CPU list in `/sys/devices/system/cpu`, and serves
  them to each Linux server instance over a channel of the I/O rings, in the same file
  protocol as diskfs's (`fsring`, read-only; `docs/design/io-rings.md`, step 5). The Linux
  server mounts them at `/proc` and `/sys` and makes each process's own part itself
  (`servers/linux/src/procfs.rs`): `/proc/<pid>/{stat,statm,status,cmdline,comm,exe,task,
  mounts,fd,cwd,root}`, `/proc/self`, `/proc/thread-self`, `/proc/mounts` (its own
  mount table), with the same formats (`procproto::render`), from its own process table (R8)
  and descriptor tables (any process's `fd`). `/proc/self/fd/N` are magic links: opening one
  opens the file itself (an unlinked file, a new end of a pipe: bash's `<(...)` through
  `/dev/fd`), `O_PATH` descriptors reopen through them. A non-Linux userland would simply not
  run procfs.
- What stays in the kernel: the console and keyboard as a raw device (the terminals are the
  Linux server's, R6d); no VFS, no descriptor tables, no signals, no Linux system call (R9,
  ADR 0011).

### Networking

- **netd** (`servers/netd`) owns the network card. The kernel finds it on PCI (a virtio-net
  card in legacy mode, whose registers are all I/O ports) and hands netd its ports, its
  interrupt line and a 512 KiB DMA area. netd sets up the two virtqueues with 64 fixed 2 KiB
  buffers each and runs [smoltcp](https://github.com/smoltcp-rs/smoltcp) for ARP, IPv4,
  ICMP, TCP, UDP and the DHCP client (0.14, vendored in `third_party/smoltcp` with a few
  patches: connection buffers that grow, the window scale for the largest of them, a timeout
  told from a reset). It serves the Linux server instances' sockets over the
  channels they offer it (one per instance, `servers/netd/src/service.rs`, the protocol
  `crates/netring`): each round it takes their requests, polls the card and the stack, moves
  the sockets' bytes between smoltcp and the instances' rings, and publishes what changed in
  the sockets' control blocks; while rounds make progress it polls, after a spin without any
  it arms the card's interrupt and every channel's doorbell and sleeps in `ipc_receive` (an
  offer, a doorbell, an interrupt or the stack's next timer wakes it).
- **Loopback**: frames to the host's own address or to `127.0.0.0/8` never reach the card;
  netd's device layer feeds them back as received frames and answers ARP for those addresses
  itself, so a program can talk to a server on the same machine. Frames from the wire that
  claim a `127.0.0.0/8` address are dropped, so services on `127.0.0.1` are not reachable from
  the network.
- **Sockets** are the Linux server's (phase R7b, ADR 0008; `servers/linux/src/inet.rs`,
  `inetcalls.rs`, `netclient.rs`): `AF_INET` stream and datagram sockets, plus raw ICMP
  sockets (`SOCK_RAW`, `IPPROTO_ICMP`) for `ping` (the program writes the ICMP message, netd
  adds the IPv4 header, reads return whole IPv4 packets, as on Linux; netd answers echo
  requests itself). Every socket call is the server's, with Linux's semantics: `bind`
  (`EADDRNOTAVAIL`, `EADDRINUSE` with `SO_REUSEADDR`'s rule), `listen`, `accept`/`accept4`,
  `connect` (`EINPROGRESS`, `EALREADY`, `SO_ERROR`), `send*`/`recv*` with `MSG_PEEK`,
  `MSG_DONTWAIT`, `MSG_WAITALL`, `MSG_TRUNC` and `MSG_NOSIGNAL` (`EPIPE` raises `SIGPIPE`),
  `shutdown` (half-close), `SO_RCVTIMEO`/`SO_SNDTIMEO`, `SO_RCVLOWAT`, `TCP_NODELAY` (Nagle's
  algorithm is on by default), `SO_KEEPALIVE`, `IP_TTL`, `SO_LINGER` with a zero time (a reset),
  `FIONREAD`, `SIOCOUTQ`; `AF_INET6` and other families are `EAFNOSUPPORT`. A socket is an
  open file description of the server's, so `read`, `write`, `poll`, `select`, `epoll`,
  `fcntl(O_NONBLOCK)`, `dup`, `fork` and descriptor passing work as usual.
- **The channel** to netd (one per instance) has a shared area with a control block per
  socket (netd's state bits, positions, errors and accept backlog; the server's positions);
  a socket's bytes travel in a receive and a send ring of 64 KiB in the server's buffer pool
  (memory granted to netd in 2 MiB pieces): the server copies between the program and the
  rings, netd between the rings and smoltcp, so TCP needs no request and no system call in the
  steady state, only a doorbell for a side that sleeps. Requests (socket, bind, connect,
  accept, datagram sends, close, ...) are answered at once; all waiting is the server's, on
  the control block's event counter, which netd wakes directly (so does a `poll` or `select`
  on the socket). The instance's net thread reports the readiness netd changed to the
  socket's watchers (`epoll`). Closing hands what
  the send ring still holds to netd, which sends it before the FIN even after the program (or
  the whole instance) is gone; data that arrives for a closed connection resets it.
- **Configuration**: `/etc/resolv.conf` points to QEMU's DNS proxy (`10.0.2.3`); DHCP gives
  `10.0.2.15/24` with gateway `10.0.2.2`. The self-tests use an echo service that QEMU provides
  at `10.0.2.100:7` (`guestfwd` to `cat` on the host).
- **Restarts**: a crashed netd is started again by the next socket (see self-healing): the
  sockets of the old channel fail (`ECONNRESET`, `POLLERR`), the next one makes a new channel.
  netd gets the same DMA area, which it clears before handing it to the freshly reset card.
- **Interfaces** (the Linux server's `netdev.rs`, `netlink.rs`): netd describes its interfaces
  (`LINKS` over the channel, `netring::Link`: the loopback and the card, with MAC, MTU, state
  and the DHCP address), and the server names them as Linux does (`lo`, `eth0`). It answers
  `NETLINK_ROUTE` sockets of its own: `RTM_GETLINK` (dumped, or one interface by index or name)
  and `RTM_GETADDR` dumps with `NLM_F_MULTI`, `NLMSG_DONE`, the request's sequence number and
  the socket's port id, acknowledgements (`NLM_F_ACK`, `NETLINK_CAP_ACK`), `EOPNOTSUPP` for other
  requests, datagram boundaries with `MSG_TRUNC` and `MSG_PEEK`, autobind, datagrams between
  the tree's netlink sockets by port, and `poll`/`epoll` readiness; this is what getifaddrs(3)
  asks, and with it Node.js's `os.networkInterfaces()`. A dump is produced as the reader's
  buffer has room (one at a time, `EBUSY` for another), answers that do not fit are dropped
  with `ENOBUFS`, sends beyond the send buffer fail before anything is allocated, and the
  buffer options stop at Linux's `rmem_max`/`wmem_max`. The netdevice(7) requests
  (`SIOCGIFCONF`, `SIOCGIFINDEX`, ...) work on any socket. Nothing announces configuration
  changes to multicast groups (they can be joined but stay silent).
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
  is one `DInode` of the server's. `statfs` and `/proc/mounts` (the server's mount table, which
  lists the disk at `/data`) make `df` work.
- A file that is deleted while still open stays allocated as an orphan until the last
  reference is dropped, as on Linux (the server holds it in diskfs), so its inode number cannot
  be reused under an open file. Such an inode is on ext2's orphan list (`s_last_orphan`, as
  ext3's), written in an order a crash cannot hurt (the name's removal, then the inode, then
  the list's head; off the list before it is freed): the first mount after a crash frees what
  the list still has. A diskfs that dies is restarted by the kernel and keeps every unlinked
  inode until the Linux server instances it served connected again and named what they hold
  (the kernel tells them at once; requests name inodes by number and generation, `ESTALE` for
  another file's): an open, deleted file outlives the restart. On a Linux host, the ext2
  driver ignores the list, the ext4 driver frees it at mount (also read-only, unless the
  device is read-only), `e2fsck -fy` frees it and `e2fsck -fn` reports it.
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
- **Console device**: the kernel moves bytes only: output to the screen and the serial mirror
  as they are (a line feed keeps the column, as on a VT), input from the keyboard and the
  console's answers to queries into a ring. It is granted to the process tree the kernel
  starts until that tree's first process ends; the monitor edits its command line on it
  otherwise (docs/design/linux-server.md, ADR 0007).
- **Terminals** (the Linux server's, `servers/linux/src/tty.rs`, `crates/ldisc`): Linux's N_TTY
  line discipline (termios and termios2, canonical editing with `ERASE`/`KILL`/`WERASE`/
  `REPRINT`/`LNEXT`, `VMIN`/`VTIME`, echo, `ISIG`, `IXON`, `OPOST`/`ONLCR`/`XTABS`, ...), the
  controlling terminal, `TIOCSPGRP`/`TIOCGPGRP`, `TIOCSCTTY`/`TIOCNOTTY`, `SIGTTIN`/`SIGTTOU`,
  window sizes with `SIGWINCH`, hangups, `TIOCSTI`, and pseudo-terminals (`/dev/ptmx`,
  `/dev/pts/n` in devpts). Device nodes name their driver by number (5,0 `/dev/tty`, 5,1
  `/dev/console`, 5,2 `/dev/ptmx`, 136,n).
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

Linux x86_64 numbers, grouped by area (about 120 in total), every one the Linux server's (the
kernel implements none, R9):

| Area | Calls |
|---|---|
| Files | `read` `write` `pread64` `pwrite64` `readv` `writev` `preadv` `pwritev` `preadv2` `pwritev2` `open` `openat` (also `O_DIRECT`) `close` `lseek` `sendfile` `truncate` `ftruncate` `fcntl` `ioctl` (the terminals', `FIONBIO`, `FIOCLEX`, `FIONCLEX`) `dup` `dup2` `dup3` `pipe` `pipe2` |
| Metadata | `stat` `fstat` `lstat` `newfstatat` `statx` `access` `faccessat` `faccessat2` `readlink` `readlinkat` `chmod` `fchmod` `fchmodat` `fchmodat2` `chown` `fchown` `lchown` `fchownat` (owners are not stored: every file is root's) `utimes` `futimesat` `utimensat` (timestamps kept, nanoseconds on tmpfs, seconds on ext2, by the server for `/proc` and `/sys`: the last 1024 set per process tree) `umask`; `inotify_init` `inotify_init1` `inotify_add_watch` `inotify_rm_watch`; `copy_file_range` (all the Linux server's) |
| Directories | `getdents64` `getcwd` `chdir` `fchdir` `mkdir` `mkdirat` `rmdir` `unlink` `unlinkat` `rename` `renameat` `renameat2` `symlink` `symlinkat` |
| I/O multiplexing | `poll` `ppoll` `select` `pselect6` `epoll_create` `epoll_create1` `epoll_ctl` `epoll_wait` `epoll_pwait` `epoll_pwait2` `eventfd` `eventfd2` |
| Memory | `brk` `mmap` (private, shared, anonymous, file through the page cache, `MAP_FIXED[_NOREPLACE]`, `MAP_NORESERVE`, `MAP_POPULATE`) `munmap` `mprotect` `mremap` `madvise` (`DONTNEED`, `FREE`) `msync` `mlock` (no-op) |
| CPUs | `sched_getaffinity` `sched_setaffinity` `getcpu` `sched_getscheduler` `sched_getparam` (the Linux server's: `SCHED_OTHER`, priority 0) `getpriority` `setpriority` (the Linux server's, over the kernel's fair scheduler; scoped to the caller's process tree) |
| Processes and threads | `clone` (`CLONE_VM` `FS` `FILES` `SIGHAND` `THREAD` `VFORK` `PARENT` `SETTLS` `PARENT_SETTID` `CHILD_SETTID` `CHILD_CLEARTID`) `fork` `vfork` `execve` `exit` (one thread) `exit_group` `wait4` `getpid` `getppid` `gettid` `set_tid_address` `sched_yield` `arch_prctl` (`ARCH_SET_FS`, `ARCH_GET_FS`) `prlimit64` `getrlimit` `setrlimit` (`RLIMIT_NOFILE` kept per table; the others per process, starting as Linux's defaults) |
| Groups and IDs | `setpgid` `getpgid` `getpgrp` `setsid` `getsid` `getuid` `geteuid` `getgid` `getegid` `getresuid` `getresgid` `setuid` `setgid` `getgroups` `setgroups` (none: root started by init) |
| Synchronization | `futex` (`WAIT`, `WAKE`, `WAIT_BITSET`, `WAKE_BITSET`, `REQUEUE`, `CMP_REQUEUE`; private and shared, monotonic and realtime timeouts) |
| Signals | `rt_sigaction` `rt_sigprocmask` `rt_sigreturn` `rt_sigsuspend` `rt_sigpending` `kill` `tkill` `tgkill` `pause` `sigaltstack` `alarm` `setitimer` `getitimer` (`ITIMER_REAL`) `rt_sigtimedwait` |
| Process control | `prctl` (name, parent-death signal, dumpable, no-new-privs, capability bounding set) `capget` `capset` (everything runs as root with every capability) |
| Filesystems | `statfs` `fstatfs` `sync` `syncfs` (every process tree's page cache of `/data`) `fsync` `fdatasync` |
| Linux server (normal mode only) | `restricted_enter` (1010), `handle_close` (1012), memory objects: `mo_create` (1013), `mo_map` (1014), `mo_unmap` (1015), `mo_protect` (1016), `mo_read` (1017), `mo_write` (1018), paged objects: `mo_create_paged` (1019), `pager_wait` (1020), `mo_supply` (1021), `mo_fail` (1022), runtime: `shared_map` (1023), `server_futex_wait` (1024), `server_futex_wake` (1025), address space: `vm_remap` (1026), `vm_discard` (1027), `vm_sync` (1028), time: `clock_read` (1030), `sleep_until` (1031), `yield` (1032), `set_usercopy` (1033), file objects: `mo_create_file` (1055), `mo_hold` (1056), `mo_file_read` (1057), `mo_file_write` (1058), `mo_file_size` (1059), `mo_truncate` (1060), `initramfs` (1061), `mo_from_image` (1062), a thread's nice value: `thread_nice` (1095), waiting for several words: `server_wait` (1133), processes and threads (R8): `proc_self` (1140), `proc_create` (1141), `thread_create` (1142), `thread_kick` (1143), `thread_kill` (1144), `thread_exit` (1145), `exec_space` (1146), `proc_info` (1147), `thread_info` (1148), `thread_affinity` (1149), `thread_cleartid` (1150), `thread_name` (1151), `init_args` (1152), `vm_floor` (1153), `random` (1154), the last mechanisms (R9): `futex_wait` (1160), `futex_wake` (1161), `futex_requeue` (1162), `thread_fs` (1163), `clock_set` (1164), `power` (1165), `file_pages` (1166); the full list is in `docs/codemap.md` (1011 `legacy_syscall`, the inode and kfile calls went with R9) |
| Native servers (`oxrt::sys`, no Linux numbers) | `ipc_register` (1000), `ipc_receive` (1001, with timeout and interrupt notifications), `ipc_reply` (1002), `irq_enable` (1003), `dma_map` (1004), `proc_query` (1005), `ioperm` (1006, privileged servers only), `exec` (1007, the server's own program again), `log` (1008), the service's end of channels (1068-1075), and with the Linux server's numbers and contracts on their own memory: `mo_map` (anonymous), `mo_unmap`, `mo_protect`, `futex_wait`, `futex_wake`, `futex_requeue`, `clock_read`, `yield`, `random`, `thread_exit` |
| System information | `sysinfo` `uname` (reports `oxidenix`, not Linux) `getcpu` |
| Power | `reboot` (power off ends QEMU, restart resets the machine; every process tree's page cache is written back first; a tree without the kernel's host grant is ended instead, as a Linux pid namespace's: ADR 0011); `ioperm` and `iopl` are `EPERM` (a process tree has no ports) |
| Sockets | `socket` `bind` `listen` `accept` `accept4` `connect` `sendto` `recvfrom` `sendmsg` `recvmsg` `shutdown` `getsockname` `getpeername` `setsockopt` (ignored) `getsockopt` (`AF_INET`: TCP, UDP, raw ICMP); `AF_NETLINK` with `NETLINK_ROUTE` (the Linux server's: `RTM_GETLINK`, `RTM_GETADDR`); the interface requests of netdevice(7) on any socket (`SIOCGIFCONF`, `SIOCGIFINDEX`, `SIOCGIFNAME`, `SIOCGIFFLAGS`, `SIOCGIFADDR`, `SIOCGIFNETMASK`, `SIOCGIFBRDADDR`, `SIOCGIFHWADDR`, `SIOCGIFMTU`) |
| Time | `clock_gettime` `clock_getres` `clock_settime` (every Linux clock, including the CPU-time clocks of threads and processes) `gettimeofday` `settimeofday` `time` `times` `getrusage` `nanosleep` `clock_nanosleep` (relative and `TIMER_ABSTIME`) |
| Misc | `getrandom` (ChaCha20 with fast key erasure, seeded at boot from RDSEED or RDRAND and timing jitter, reseeded as it is used; also `AT_RANDOM`); `io_uring_setup`, `io_uring_enter`, `io_uring_register` answer `ENOSYS` (from the Linux server, quietly: libuv probes them and uses epoll) |

Everything runs as root. An unknown syscall returns `ENOSYS`, and the Linux server prints
`syscall N not implemented` on the console.

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
| `oomtest` | fork bomb (stops at the process limit), memory exhaustion via `mmap`, 100 full pipes; the kernel survives and memory is reusable; a process touching `MAP_NORESERVE` memory beyond the commit limit is killed while one with committed memory touches all of it; a `read()` into an untouched `MAP_NORESERVE` buffer with nothing left to commit kills the reader too (not `EFAULT`); descriptors up to `RLIMIT_NOFILE` (4096, as `getrlimit` says), then `EMFILE`, a fork copying them all |
| `sigtest` | handlers, killing a busy loop, `SIGCHLD`, `EINTR` on pipe reads, blocked and ignored signals, FPU state across asynchronous handlers, `alarm` and repeating `setitimer`, catchable `SIGFPE`/`SIGSEGV`/`SIGTRAP` from CPU exceptions, an uncaught `SIGFPE` killing the process |
| `jobtest` | stop/continue reporting through `wait4`, restart of a stopped `read()`, `SIGKILL` on stopped processes, `SA_RESTART` |
| `waittest` | processes in the Linux server: `wait4` and `waitid` with their options (`WNOHANG`, `WNOWAIT`, `WEXITED`, `WSTOPPED`, `WCONTINUED`, `__WCLONE`, `__WALL`), the `siginfo` and `rusage` they report, an ignored `SIGCHLD` and `SA_NOCLDWAIT` reaping at once, child subreapers, the parent-death signal, process groups and sessions (`setpgid`, `setsid`, `getsid` and their errors), `clone3`, `vfork` |
| `sigframetest` | Linux's signal frames built by the Linux server: `siginfo` of `kill`, `tgkill` and `sigqueue`, the interrupted registers in the `ucontext`, real-time signals queued in order, `SA_RESETHAND`, `SA_NODEFER` nesting, a fault's address and `SEGV_ACCERR`, the alternate stack, a frame that cannot be written (`SIGSEGV`), a handler setting `rax` to a restart code (returned as it is), queued real-time signals of ended processes given back |
| `forktest` | `fork`, `execve`, `wait4`, preemptive interleaving of two workers |
| `leaktest` | repeated work leaves the kernel's memory as it was: after a warm-up, thousands of rounds of fork and exit, fork and exec, a process whose minute-long `poll` ended early (its timer entry goes with it), threads, shared mappings of `/tmp` and `/data` files, `open`/`close`, pipes and `AF_UNIX` connections keep the kernel heap in use (`Slab` in `/proc/meminfo`) within 16 KiB and the free frames within the kernel stacks' page tables (a cache of at most 1 MiB, `memory/kstack.rs`) |
| `proctest` | `prctl` name round-trip, `capget`/`capset` versions and the full capability set, no-new-privs, `PR_SET_PDEATHSIG` delivered to an orphan (via `sigwait`); `/proc` as htop reads it (directory fds with `O_PATH` and `openat`), `/proc/self`, the formats of `stat`, `meminfo`, `loadavg`, `uptime` and `/proc/<pid>/{stat,cmdline,exe}`, `/proc/counters` counting system calls and allocations, `sysinfo`, the CPU list in `/sys`, read-only `/proc`, `/proc/<tid>/status` of a thread that is not the main one (its own `Pid`, the process's `Tgid`); the Linux server's part: `/proc/mounts`, `thread-self`, `task/<tid>`, `cwd`, `statfs` types, nothing created, removed, renamed, chmodded or written (Linux's errors), `seq_file` snapshots and `pread` at 0 current; `/proc/self/fd`: paths and `(deleted)`, reopening an unlinked file, a pipe's ends (also by `/dev/fd`), a socket's `ENXIO`, an `O_PATH` descriptor opened for real, musl's `fchmod` and `fexecve` through it, another process's descriptors listed and described, another process's `RLIMIT_NOFILE` by `prlimit` |
| `threadtest` | pthreads: create/join, own tids, TLS, 4 threads counting under a mutex, condition variables and timed waits, 300 threads in a row, `Threads:` in `/proc/self/status`, `exit` and fatal signals ending all threads, the process outliving its main thread, group stop and continue, process signals reaching a thread that does not block them, `pthread_kill`, `fork` and `execve` in a thread, real `vfork`, `posix_spawn`, `munmap` and `mprotect` reaching a writer on another CPU (TLB shootdown), `MADV_DONTNEED` in a loop under three writer threads while another process checks fresh memory for stray stores (this hung the scheduler before) |
| `futextest` | `FUTEX_WAIT` on a changed value (`EAGAIN`), timeouts, `EINVAL`/`EFAULT`, interruption by a signal (`EINTR`), shared futexes across processes, private memory keeping separate keys after `fork`, bitsets, `FUTEX_CMP_REQUEUE` |
| `timetest` | nanosecond resolution of `CLOCK_MONOTONIC`, no step back on one CPU or between two, `clock_getres`, invalid clocks, `BOOTTIME`, `RAW`, `COARSE`, `gettimeofday` and `time` against `CLOCK_REALTIME`, `clock_settime` moving only the wall clock, thread and process CPU clocks (spinning counts, sleeping does not, `pthread_getcpuclockid`, `clock_getcpuclockid`), `getrusage` for the process, the thread and reaped children, `times`, `wait4`'s rusage (a reaped child's CPU time with its own children's, its peak memory, also across `exec`) |
| `randtest` | `getrandom` (the kernel's ChaCha20 generator, seeded from RDSEED/RDRAND and timing jitter): flags (`GRND_NONBLOCK`, `GRND_RANDOM`, `GRND_INSECURE`, `EINVAL` for unknown ones and `GRND_INSECURE` with `GRND_RANDOM`), answers that differ, 1 MiB at once with every byte value about as often, 32 MiB with signals coming (whole pieces, never an error), `EFAULT`; `AT_RANDOM` differing between processes |
| `timertest` | sleeps and timeouts end when due, not at the next tick, and never early (median and minimum of nine 1–2 ms waits in `nanosleep`, `poll`, `select`, `futex`, `sigtimedwait`), 100 × `usleep(100)`, `clock_nanosleep` absolute (monotonic, past wall-clock times) and on `CLOCK_BOOTTIME`, refusal on CPU clocks, the time left after an interrupted `nanosleep`, a 2 ms `setitimer` interval firing about 50 times in 100 ms, a 1 µs interval timer leaving another process on its CPU its share (and, ignored, its own process), an ignored timer resuming once handled |
| `fdtest` | the descriptor table (the Linux server's): `dup`, `dup2`, `dup3` and `F_DUPFD` (lowest free, close-on-exec, errors), status flags shared by a description's descriptors (`F_SETFL`, `FIONBIO`), `close_range` (`CLOSE_RANGE_CLOEXEC`, `CLOSE_RANGE_UNSHARE` in a thread), `RLIMIT_NOFILE` (`EMFILE`, `EBADF`, `EINVAL`, `EPERM`, raising it), `fork`'s copy and shared offsets, threads sharing the table, a blocked read outliving another thread's close, exit and `execve` closing (close-on-exec only; a failed `execve` changing nothing), a process killed inside `execve` (waiting for an argument's page that never comes) or in its program having its pipe closed and its listening port free when `waitpid` returns, a `CLONE_VM|CLONE_VFORK|CLONE_FILES` child's table, pseudo files (`/proc` read, `lseek`, `fstat`, always ready, `EPERM` for `epoll`) and `/dev` (`fchdir`, `openat`, `getdents64`, `/dev/zero` mapped, `/dev/null`) |
| `polltest` | `poll` and `select` wake within 1 ms of a pipe write or a datagram over loopback (median of nine, the writer on the same or another CPU, next to an idle descriptor), a full pipe polling writable once drained, `POLLHUP` when the last writer closes, `EINTR` in a `poll` and a `select` waiting on files (also under `SA_RESTART`), `POLLNVAL` and `EBADF` for closed and `O_PATH` descriptors, `select` and `ppoll` writing back the time left, `poll`, `select` and `ppoll` going on after a stop and a continue (`restart_syscall`), `EINTR` from `restart_syscall` with nothing to restart, wake-up within 1 ms of data on a TCP connection |
| `eventfdtest` | `eventfd` counting (initial value, adding writes, reset on read), `EFD_SEMAPHORE`, `EFD_NONBLOCK` and `EFD_CLOEXEC`, `EINVAL` for short reads, 2^64-1 and unknown flags, a full counter (`EAGAIN`, poll state), a blocking read woken by another process, `poll` waking within 1 ms of a write |
| `sigmasktest` | temporary signal masks of `sigsuspend`, `ppoll` and `pselect`: a pending or arriving signal the mask lets through interrupts them and its handler runs with that mask, the caller's mask comes back afterwards, a successful `ppoll` leaves a blocked signal pending (`sigpending`), a mask that blocks a signal holds it off until the call returns, `EINVAL` for a wrong mask size |
| `epolltest` | `epoll_create1`/`epoll_create` flags and sizes, `EPOLL_CTL_ADD`/`MOD`/`DEL` and their errors (`EEXIST`, `ENOENT`, `EPERM` for regular files, `EINVAL`, `EBADF`), level-triggered, edge-triggered and one-shot reporting, `EPOLLOUT` and `EPOLLERR` on a pipe's write end, interests removed with the file's last descriptor (not before), `maxevents` rotating through ready files, eventfds and UDP sockets in a set, nested instances (`ELOOP` for a loop and for chains longer than five, built at either end) and `poll` on an instance, wake-up within 1 ms of a write, timeouts, `EINTR` and `epoll_pwait`'s mask, `epoll_pwait2`; `EPOLLEXCLUSIVE` (its `EINVAL` rules, one write waking one of two waiting instances, both without it), an instance shared by a forked child and passed with `SCM_RIGHTS`, interests keyed by descriptor and description, `EPERM` for `/proc` files, `EBADF` for `O_PATH`, `EFAULT` keeping the event |
| `unixtest` | `AF_UNIX` sockets in the Linux server: stream pairs (`fstat` as a socket, `FIONREAD`, `MSG_PEEK`, reads across writes, names and peer names of a pair, `SO_TYPE`, `SO_DOMAIN`, `SO_PEERCRED`, `SO_SNDBUF` doubled, `EOPNOTSUPP` for other levels, `ESPIPE`), `shutdown(SHUT_WR)` (`POLLRDHUP`, the rest, end of file, `EPIPE` for the writer, the other direction still open), a closed peer (`POLLHUP`, end of file, `EPIPE` with `SIGPIPE`, none with `MSG_NOSIGNAL`, `ECONNRESET` after unread data); nonblocking (`SOCK_NONBLOCK`/`SOCK_CLOEXEC`, `EAGAIN`, edge-triggered `epoll` and new edges, a full send buffer without `POLLOUT` until drained, `EPOLLRDHUP`, `SO_RCVTIMEO`); datagram pairs (boundaries, empty datagrams, truncation with `MSG_TRUNC`) and seqpacket pairs (boundaries, the truncated rest gone, end of file); servers on a tmpfs path, a relative path, a `/data` path and an abstract name (`EADDRINUSE`, `ECONNREFUSED` before `listen`, `SO_ACCEPTCONN`, names given back, a pending connection polling readable, the client's and the listener's names and credentials on both ends, a full backlog's `EAGAIN`, pending clients reset when the listener closes), socket inodes (`S_ISSOCK`, `ENXIO` for `open`, kept after close), `ENOENT` and `ECONNREFUSED` for paths, autobind; named datagram sockets (`recvfrom`'s sender, `connect` and `AF_UNSPEC`, `ENOTCONN`, `EPROTOTYPE`, `ECONNREFUSED`); `SCM_RIGHTS` to a forked child (a pipe end and a file whose offset is shared, `MSG_CMSG_CLOEXEC`, the passed descriptors outliving the sender's close, `MSG_CTRUNC` dropping what does not fit, a socket passed back), `MSG_PEEK` installing copies (only as many as the control buffer holds), `EBADF`, a cycle of sockets in flight collected, also one an exited process left (and its descriptors in flight never blocking another process's); `SO_PASSCRED` and `SCM_CREDENTIALS`, implicit and explicit, `ESRCH` for a process outside the tree, the bytes delivered despite an unwritable control buffer; a blocked `recvmsg` and `accept` outliving another thread's close, 200 races of a close, a receive taking a socket out of flight and the collector; a datagram sender held back by a full receiver not writable for `epoll` until it reads; receives into pages of a `/data` mapping the pager brings while sockets close and the collector runs; five processes trying to put more descriptors in flight than the instance's bound; nothing left in flight when it ends; an `O_PATH` descriptor, a `/dev` file and an epoll instance passed, and an epoll instance's interest keeping nothing in flight |
| `lxtest` | the Linux server's kernel interface through its test calls (answered in test mode only, `SYS_TEST_MODE`): a memory object it filled, mapped into the program, a program store read back by the server, write protection, unmapping, mappings refused at the server's region and unaligned; a paged object whose pages the pager thread supplies when the program reads them or the kernel copies from them (`write` from the mapping), each page once; `SIGKILL` ending a thread that waits for a page the pager never supplies; a page the pager fails raising `SIGBUS` and a later access getting it, for a second such object too; a waiting thread getting `SIGBUS` once the pager closed the object's last handle; a store the pager cannot back raising `SIGBUS` and a later store going through (`TEST_MKWRITE_FAIL`); the server's heap with 2000 blocks of many sizes and its mutex serializing six threads in two processes; nothing passing through (R9): a call the server does not implement `ENOSYS`, the test call that passed one through gone; futex (`EAGAIN`, a timeout's `ETIMEDOUT`, an absolute deadline past, `EINVAL` for a zero bitset and an unaligned word, `EFAULT` for the server's memory, a wake from another thread), `arch_prctl` (`ARCH_GET_FS` the TLS pointer, `EPERM` above 64 TiB, `ARCH_SET_FS` and back), `uname`, `sysinfo`, `getrandom` beyond one piece and its flags, `getcpu`, `clock_settime` (`EINVAL` for the monotonic clock, the wall clock set and set back by `settimeofday`), the caller's CPU clocks also by pid 0's encoding, the resource limits (an 8 MiB stack, no core files, `EINVAL` for a soft limit above the hard one and for a resource beyond them), `ioperm` `EPERM`, `reboot` with a wrong magic number `EINVAL`, `/dev` as the server's devtmpfs (its nodes and numbers, its links into `/proc/self/fd`, `pts`, `shm`, tmpfs's `statfs`, its own device (`EXDEV` to `/tmp`), files root makes there, `/dev/zero` read and mapped); `mmap`, `mprotect` and `munmap` handled by the server, and `clock_gettime`, `gettimeofday` and `nanosleep`; the server writing a page the program never touched, and `EFAULT` (not death) for read-only, `PROT_NONE` and unmapped program memory, for the server's own memory and across the 64 TiB line; pipe reads and writes, end of file after the writer closes, `EPIPE` and `SIGPIPE` without readers, `FIONBIO`, `FIOCLEX` and `FIONCLEX` on a pipe (the server's descriptor table), other ioctls `ENOTTY`; eventfd likewise, `EAGAIN` when empty and non-blocking; the server's records: a forked child's copy, a thread's shared one, released when processes end; path calls: `chdir`/`getcwd`, `umask` on a new file, `O_EXCL`, symlinks (`readlink`, `stat` vs. `lstat`, `O_NOFOLLOW`, a loop's `ELOOP`, a symlinked directory in a path), `openat` relative to a directory descriptor, `rename`, a child's own working directory, `fchdir`, `rmdir`, `O_CREAT` through a dangling symlink; `/tmp` as the server's tmpfs: its own device, reads, writes, `lseek` and `fstat`, `O_APPEND`, a shared mapping writing the file, `ftruncate`, `readdir`, `EXDEV` and `EBUSY` at the mounts, the root and `/bin/busybox` from the server's tmpfs, `/proc` procfs's and `/dev` the server's devtmpfs; channels to the test service `ringtest`: requests and completions through the rings with both ends sleeping on futex doorbells, a service's doorbell watch kept once when armed twice, never moved by a requeue, woken once and arriving as an `ipc_receive` event, connect errors (`EISCONN`, `ENOENT`, `EOPNOTSUPP`, `ENOTCONN`), grant data both ways, a read-only grant the service can neither `mprotect` writable or executable nor have the kernel store into, the kernel's grant bounds, device addresses only within a grant, `EBUSY` for truncating a granted page, `ENODATA` for an unsupplied paged page, a revoked grant gone from the service (its range reserved and inaccessible until the service unmaps it), a draining grant pinned with its id held until the service lets go, the client's end closing while the service sleeps (its grant mappings gone, pins released), the service dying of a store into a read-only grant while the client waits (the client wakes with `EPIPE`, the page unchanged, the service restarted for the next channel), the service executing a new program that can then neither map the grant nor get a device address of it; the file protocol against diskfs (`TEST_DISKRING`): the disk image's README read by DMA at unaligned offsets, stat, readdir with a cursor, statfs, a symlink; aligned, unaligned, one-sector and past-the-end writes, a flush, the file read back against a model, truncate, rename, permissions; malformed requests completing with `ENOSYS`, `EINVAL`, `EBADF`, `EACCES`, `ESTALE` (handles of inodes not in use or of another generation), `ENOENT`, `ENAMETOOLONG`, `ENOTDIR`, `EEXIST`; 24 writes and 24 reads in flight; a grant revoked under diskfs (`EFAULT`, diskfs alive, `FORGET`); a client closing with reads in flight; a revoked grant's range given to no other grant; an unlinked inode freed only when no channel holds it (its handle `ESTALE` then); a client of a killed diskfs that never names its unlinked inode again losing it after the next diskfs's grace (5 s), its handle stale; a write stalled behind another with every operation slot busy; requests waiting for completion room leaving diskfs idle (its CPU time, `TEST_SERVER_TICKS`), `/proc` not showing diskfs (another tree's processes are not) and completing once there is room; the ring's file read and removed through `/data` (the server's page cache, another channel); the page cache's kernel interface (`TEST_CACHED`): a failed fill past the end of a file leaving no trace once it grows, a write-back scan longer than one call going on where the kernel says, a truncation giving up (`EBUSY`) on a page pinned by a grant never let go of, a sync across instances (`sync_others`); a nice 19 thread taking a server lock next to a nice -20 loop not holding up another thread that needs the lock |
| `lxtest` x3 (in `runtests.sh`) | all of `lxtest` three more times in the same boot, its output into a pipe, beside a loop reading `/proc` (and forking, exec-ing, waiting): every check holds in any run and whatever else runs |
| `lxtest crashloop` (in `runtests.sh`) | the restart policy on the test service dying at every use: restarts with a growing backoff (3.1 s in all), the service down (`EIO` at once) after the sixth young death in a row, up again after the 5 s cooldown |
| `libuvtest` | what libuv (Node.js) uses beyond POSIX, all answered by the Linux server: `statx` on tmpfs and `/data` against `stat` (every field, device numbers), symlinks followed and not, `AT_EMPTY_PATH` on regular files, pipes, sockets and `/dev/null`, the working directory for `AT_FDCWD` with an empty or `NULL` path, `ENOENT`, `EINVAL` (flags, `STATX__RESERVED`, both sync types), `EFAULT`, `EBADF`; `io_uring_setup`/`enter`/`register` `ENOSYS`; `getifaddrs` (`lo` 127.0.0.1/8, `eth0` with netd's MAC and DHCP address, netmask and broadcast); the netdevice(7) requests on an `AF_INET` and a netlink socket (`SIOCGIFINDEX`, `MTU`, `HWADDR`, `FLAGS`, `ADDR`, `NETMASK`, `BRDADDR`, `NAME`, `CONF` with and without a buffer, `ENODEV`, `ENOTTY` on a pipe); netlink sockets: protocol and type errors, autobind, `EADDRINUSE`, `SO_TYPE`/`SO_PROTOCOL`/`SO_DOMAIN`, `fstat`, `EAGAIN`, `poll`/`epoll` readiness, one link by index with its acknowledgement, `EOPNOTSUPP` for other requests, `MSG_PEEK`/`MSG_TRUNC` and truncation, datagrams between two sockets by port, `ECONNREFUSED`, `ENOTSOCK` for socket calls on a pipe; `copy_file_range` on tmpfs and `/data` at the positions and at offsets, `EINVAL` for overlapping ranges and flags, `EBADF` for read-only and `O_APPEND` outputs, `EISDIR`, `EXDEV` across filesystems; `copy_file_range` in both directions at once (no deadlock), the length clamped to the input's end before the overlap check; netlink's bounds: `EMSGSIZE` beyond the send buffer (also for 2 TiB of iovecs), the buffer options capped, answers beyond the receive buffer dropped with `ENOBUFS`, a dump waiting for room and `EBUSY` for another meanwhile |
| `metatest` | file times on tmpfs (nanoseconds) and `/data` (seconds): `utimensat`, `futimens` with `UTIME_OMIT`/`UTIME_NOW`, `utimes`, `EINVAL` for nanoseconds out of range; a write moving mtime and ctime (and the times staying after `fsync`, the write-back), `ftruncate`, `chmod` (ctime only), a new and a removed name moving their directory's mtime; tmpfs's birth time in `statx`, none on ext2; `fchmod`, `fchmodat2` with `AT_EMPTY_PATH`, `EOPNOTSUPP` on a symlink, the chown family, `getgroups`/`setgroups`; times before 1970, `EINVAL` for flags with a null path, times set on `/dev/null` and on `/proc` reading back |
| `inotifytest` | inotify on tmpfs and `/data`: a directory's watch seeing create, merged modifies, attrib, mkdir, a move's two events with one cookie, delete; a file's own modify, close-write, open, access, close-nowrite; its removal (attrib, delete, delete-self, ignored); `IN_ONESHOT`, `IN_ONLYDIR` (`ENOTDIR`), `IN_MASK_CREATE` (`EEXIST`), `rm_watch` (`IN_IGNORED`, then `EINVAL`); `FIONREAD`, `EINVAL` for a buffer too small, names padded to 16 bytes, poll and a blocking read woken by another thread, `EINVAL`/`EBADF` for other descriptors; a file removed while open (`IN_DELETE_SELF` at its last close); the limits: 128 instances (`EMFILE`), 8192 watches (`ENOSPC`), 16384 events then one `IN_Q_OVERFLOW`; one watch for a pseudo file named twice |
| `vmtest` | demand paging (a 64 MiB mapping costs nothing until touched), `SIGSEGV` on read-only and `PROT_NONE` pages with contents kept, split areas after a partial `munmap`, NX and the JIT pattern (1 GiB `PROT_NONE` reservation, write code, `mprotect` to executable, call it), commit limit and `MAP_NORESERVE` (also a 4 GiB reservation made writable and executable at once, as V8's code range, and forked counting only its touched pages, while the same without it is `ENOMEM`; 40 rounds of partial `munmap`, `MADV_DONTNEED`, `mremap`, `mprotect`, `fork` with copy-on-write on both sides and `exec` leave `Committed_AS` unchanged; a read-only area read in completely becomes writable with nothing left to commit), `mremap` in place and moving, `MADV_DONTNEED`, shared vs. private memory across `fork`, lazy file mappings and `SIGBUS` beyond the end, `MAP_FIXED_NOREPLACE`, stack growth to 4 MiB and overflow beyond 8 MiB, nothing mapped at or above 64 TiB (fixed mappings fail, hints and the stack stay below), the Linux server's memory out of the program's reach and its kernel calls `ENOSYS` for a program |
| `smptest` | CPU count and affinity (pinning to every CPU, empty masks), the scheduling policy (`SCHED_OTHER`, priority 0, for the caller and another thread, `ESRCH`, `EINVAL`), parallel speed-up of CPU-bound processes, `fork`/`exit`/`wait` on every CPU at once, 5000 pipe round trips between two CPUs, signals to a process running on another CPU, timers on time while a program floods the console with palette changes on the same CPU; nice values: `getpriority`/`setpriority` (clamped, inherited by a child, in `/proc/<pid>/stat`, `PRIO_PGRP`, `PRIO_USER`, `ESRCH`, `EINVAL`), and the CPU shares they give two processes pinned to one CPU (nice 19 against 0 a small one, -5 about three times 0's) while a sleeper elsewhere still wakes; nice -1 against 0, a process outside the tree `ESRCH`, the lateness of a sleeper next to a nice -20 loop on its CPU; a loop that ran alone for seconds sharing at once with a second one |
| `nettest` | TCP to an echo service through QEMU, `ECONNREFUSED`, `listen`/`accept` over loopback with a forked client, EOF after the peer closed, non-blocking `accept` and `connect` with `poll` and `SO_ERROR`, `EINTR` in a blocking `recv`, UDP over loopback, raw ICMP echo to the gateway and over loopback, source address for off-subnet destinations, overflowing message vectors (`EINVAL` for a negative iovec length, `EMSGSIZE` for a datagram over 65507 bytes), `AF_INET6` rejected; `EADDRINUSE` for a listener's port (also the wildcard address, with `SO_REUSEADDR`), `bind` to port 0 taking a port at once, `EINVAL` for binding twice; the server's sockets (R7b): 8 MiB through a loopback connection in pieces of many sizes, checked byte by byte in another process; `MSG_DONTWAIT`, `FIONREAD`, `MSG_PEEK`, `MSG_WAITALL` across two writes; `SO_RCVTIMEO` (`EAGAIN` after the time, read back), `SO_SNDTIMEO` (a send to a reader that never reads returns what went, then `EAGAIN`), `SIOCOUTQ`; half-close with `SHUT_WR` (end of file and `POLLRDHUP` for the peer, the other way still open, `EPIPE` and `SIGPIPE` for a write after it); a peer that closed: writes end in `EPIPE` with one `SIGPIPE`, none with `MSG_NOSIGNAL`, `POLLHUP`; a refused nonblocking connect (`SO_ERROR` `ECONNREFUSED`, once); `TCP_NODELAY` off by default and settable, `SO_KEEPALIVE`, `SO_TYPE`/`SO_PROTOCOL`/`SO_DOMAIN`, `ENOPROTOOPT`, `ENOTCONN` for `getpeername` and `recv` unconnected; `accept4`'s flags; an accepted connection's port (`EADDRINUSE` without `SO_REUSEADDR`, a new listener with it); of two sockets sharing a port by `SO_REUSEADDR` only one listens (`EADDRINUSE` for the other's `listen`); a UDP port shared only with `SO_REUSEADDR` on both; `EPOLLET` edges per arrival; `EINTR` in a blocking `accept`; UDP `MSG_TRUNC` and `FIONREAD`, a connected UDP socket (`getpeername`, `send`, `AF_UNSPEC` disconnecting: `EDESTADDRREQ`); `socketpair(AF_INET)` is `EOPNOTSUPP` |
| `mmaptest` | shared file mappings: stores visible to `read` and `write` visible in the mapping at once, another process's own mapping of the file, the size unchanged by stores; private mappings seeing `write` until they write a page, and never reaching the file; mappings outliving `close` and `unlink`; the zero tail of the last page and `SIGBUS` beyond it; growing and shrinking with `ftruncate` (`SIGBUS` in shared pages and private copies beyond the new end, zeros after growing again); `EACCES` for writable sharing of a read-only descriptor (also via `mprotect`); shared anonymous memory across 8 children; mapping initramfs files |
| `exectest` | eight runs of one program sharing its pages (less memory than one copy), data and bss of the loaded program, `ETXTBSY` for opening or truncating a running program and for running a program open for writing or mapped through a writable descriptor (not after `munmap`, not for a read-only mapping), a changed program file taking effect on the next run, a running program surviving the deletion of its file; arguments and environment laid out as Linux does (back to back, in order); `#!` scripts (with an argument, nested, `ELOOP`), `ENOEXEC`, `EACCES` for a directory or a file without an execute bit, `execveat` with `AT_EMPTY_PATH`, `E2BIG`; `execve` while other threads keep making threads |
| `cachetest` | the page cache of `/data` files: data read back right after writing and `fsync` (from memory), `Cached` in `/proc/meminfo`, committing all free memory reclaims cached pages (and all of it can be used), the file read again from the disk afterwards, read-only shared and private mappings of a disk file, `pwrite` visible to `pread` and both mappings, private stores staying private, `ftruncate` shrinking and growing (zeros, not old data, in reads and the mapping), a program on the disk running from the cache and `ETXTBSY` while it runs |
| `writebacktest` | stores through a shared mapping of a `/data` file: reading makes nothing dirty, a store makes its page dirty (`Dirty` in `/proc/meminfo`), `msync`, `fsync` and `fdatasync` write it back (also a page stored to again afterwards), `write` and a store in one page both arrive, the server's write-back takes a store to the disk on its own after `munmap`, a store of a process that exited, dirty pages surviving reclaim, truncation of a file with dirty pages, 1 MiB of stores; `O_DIRECT` reads as the view of the disk |
| `mmaptest /data` | all of `mmaptest` on a disk file |
| `ttytest` | the Linux server's terminals through pseudo-terminals: `/dev/ptmx`, `TIOCGPTN`, a locked slave (`EIO`), `unlockpt`, `TIOCGPTPEER`, devpts's nodes (136, n) and its refusals (`EACCES`, `EPERM`), the node gone with the master; `/dev/console` and `/dev/tty`; termios defaults and round trips (also through the master); canonical reads a line at a time with echo, partial lines, `FIONREAD`, erase/kill/werase and their echo, `^D`, `^V`, `TIOCSTI`; `ONLCR`, `XTABS`, no `OPOST`; `VMIN`/`VTIME` (0/0, 0/2, 3/0 with poll, 5/1's inter-byte timer); raw input, leaving canonical mode; `^S`/`^Q` and `tcflow`; poll and `tcflush`; window sizes; the master's `EIO` and `POLLHUP` after the slave's close; a session with the slave as controlling terminal: `^C`, `^\`, `^Z` and `SIGWINCH` for the foreground job, `SIGTTIN` and `SIGTTOU` (`TOSTOP`, `tcsetattr`) for background jobs, `EIO` with `SIGTTIN` ignored, `TIOCSPGRP`'s `EPERM`/`ESRCH`, `TIOCSCTTY`/`TIOCNOTTY`; the terminal freed when its session's leader ends; a hangup by the master's close (`SIGHUP`, end of file, `EIO`); `O_PATH` and `O_DIRECTORY` on device nodes; echoes of console input not waiting behind a process flooding the console; echoes into a master that never reads bounded, `TIOCSTI` on a full master, `TIOCSIG`'s signals; a waiting canonical read taking the half line when the mode goes raw, two readers each getting a whole line, a failed slave open leaving the master usable; an orphaned stopped job getting `SIGHUP` and `SIGCONT` |
| `datatest` | `/data` in the Linux server: reads, writes, `lseek`, `stat`, `fsync`, its own device and ext2's `statfs`; two descriptors, a mapping and another process's mapping sharing one page cache; `write` leaving dirty pages, `fsync` writing them (`Dirty` back, the data on the device: `O_DIRECT`), an `O_SYNC` write clean when it returns, a write past the end and its hole, `sync`; truncation with dirty mapped pages (`SIGBUS` beyond, the tail zero, the size on the device); 100 children storing into an inherited mapping while a thread writes it back, every store on the device; 400 forks racing a truncation of a mapped file, every child getting `SIGBUS` beyond the new end; 8 processes reading one uncached file at random offsets at once; 4 threads writing parts of one file; a 32 MiB file written and read back with 12 MiB left to cache it; a read while dirty pages fill memory; a full disk: `write` itself failing with `ENOSPC` (the space promised as data enters the cache), everything accepted on the device after `fsync`, `statfs` counting promised space, a store into a hole raising `SIGBUS`, and going through once there is room again; an open file unlinked whose diskfs is killed twice (`TEST_KILL_SERVER`): the Linux server connects to the next diskfs without a use of `/data` (`EVENT_SERVICE_GONE`), the file stays allocated, the open descriptor reads its data back from the device and writes on, and its blocks come back after the last close |
| `bash -c` (in `runtests.sh`) | Bash itself: functions, arrays, arithmetic, `[[ ]]`, a pipe into `grep`, a here-document into a `/tmp` file read back, a subshell's `cd`, command substitution |
| `sh /etc/test.sh` | files, pipes, `cd`, `mkdir`/`touch`/`rm`, rename cycles via symlinks, the tmpfs size limit |
| `fstest` | descriptor access modes (`EBADF` on read-only/write-only fds), `O_NOFOLLOW` on symlinks, unlinked-but-open files (kept until closed, never shared with new files), ext2 size limits, overflowing `mmap` offsets; `preadv2`/`pwritev2` and their flags (also in `lxtest` on `/tmp`): the offset -1 as the file position, a positional write to an `O_APPEND` descriptor appending as on Linux, `RWF_APPEND`/`RWF_NOAPPEND`, `EOPNOTSUPP`/`EINVAL` for unsupported or contradicting flags, `ESPIPE` on pipes (`userspace/rwtest.h`); `getdents64` with a huge buffer returning bounded pieces that together list every entry once, `..` the parent's inode (tmpfs and `/data`); the tmpfs bounds' room the same after files made, renamed over and unlinked while open, long symlinks and failed creations |
| `sh /etc/disktest.sh` | ext2: 150-file directory, 1.5 MiB file (double indirect), append, truncate, rename, cycles, symlinks, `rm -r`, space accounting |
| `e2fsck -fn target/test-disk.img` (host, after a test run) | the filesystem the tests wrote is consistent |
| `cargo test -p ext2fs` (host, needs e2fsprogs) | ext2 on a RAM disk that counts requests and can fail writes: 4 MiB read in about one device read per 32 KiB request, two flushes per write (data, then metadata), nothing written by reads, blocks moving between directories and files, a file larger than the block cache, corrupt block pointers (`EIO`, no crash), every write of a commit failing in turn (retried, nothing lost), failed data writes never exposing a deleted file's blocks; the ring path: writes into reserved blocks read back through both paths, extents clipped at the end with holes, reserved blocks out of every bitmap and inode until linked (`e2fsck` clean meanwhile, other allocations never take them), `sync` flushing the data before the metadata, `ENOENT` for inodes not in use; crashes: every write and flush of IPC and ring operations replayed up to a crash in each flush epoch, with arbitrary losses of what came after the last flush and a two-block metadata cache evicting all the time, never showing a deleted file's data in a file, directory or symlink; blocks freed before a failed commit not reused (ring or IPC) until a commit succeeds; freed inodes refusing reads, writes, truncation, permission changes (`ENOENT`); superblocks whose group or inode sizes do not fit a block refused at mount; blocks in flight for a promise counted once (the rest of the disk stays promisable, also after the promise ends; 128 reservations of 64 blocks in flight, half linked, half cut off by a truncation); freed blocks kept as ranges (unit test); socket inodes (a socket's mode and directory entry type, kept by a rename, freed by an unlink); `e2fsck` after each |
| `cargo test -p fsring` (host) | the file protocol: every request survives encode and decode (socket inodes too), `ENOSYS` for unknown operations, `EINVAL` for any field an operation does not use, transfers, names and targets bounded (`EINVAL`, `ENAMETOOLONG`), names without `/` or NUL, completions, stat, usage and directory entries round-trip; `SETTIMES` takes the chosen times in 32 bits only |
| `cargo test -p netlink` (host) | rtnetlink's answers: link and address dumps (`NLM_F_MULTI`, `NLMSG_DONE`, sequence numbers and port ids, the attributes getifaddrs reads), one link by index or by name with its acknowledgement, `ENODEV`, `EINVAL` for a short request, `EOPNOTSUPP` for other requests (capped with `NETLINK_CAP_ACK`), no answer for control messages and non-requests, a malformed length ending the datagram, two requests in one datagram, dumps split into page-sized datagrams; dumps produced a datagram at a time, one dump at a time (`EBUSY`), answers stopped at the room left |
| `cargo test --release -p netring` (host) | the socket protocol between the Linux server and netd: every request survives encode and decode, `ENOSYS` for unknown operations, `EINVAL` for stray fields, malformed areas (two power-of-two rings on a page), sockets, endpoints and values out of range; the shared area's layout; ring arithmetic across the wrap; datagram records and interface records round-trip; netd's port rules (Linux's `SO_REUSEADDR` rule within an instance, never a port another instance serves, UDP reuse within an instance only); netd's budgets (a cap per instance however it asks, a reserve kept for every instance with a channel, refused charges take nothing, given back only to the instance charged, a flood against one instance leaving the others theirs); ICMP messages told apart by what they concern, malformed ones (every truncation, changed bytes) never a panic; echo identifiers rewritten with their checksum and kept apart per instance; 4 MiB streamed through a ring between two threads (the reader sleeping on the control block, the writer waiting for room) and 100000 marks handed to a sleeping net thread, with no wakeup lost |
| `cargo test -p vfs` (host) | paths and cpio; `struct stat` round-trips, every `struct statx` field and `stx_mask` (with and without a birth time), device numbers, statx's checks of flags and mask |
| `timeout 1 sleep 5` | `vfork` and `SIGTERM` after the time limit (exit status 143) |
| `kill -9 1` in Bash | user space cannot kill a server (`EPERM`) |
| `kill diskfs` in the kernel monitor | the next `/data` access connects a new channel, which restarts the server; open files survive (an unlinked one fails with `EIO`), dirty pages whose write-back failed are written again; a diskfs in a crash loop is down for a while (`EIO`) and then tried again (ADR 0006); a restart still runs the boot-time program even after `/sbin/diskfs` was overwritten |
| `kill netd` in the kernel monitor, then `run nettest` | the first socket call restarts netd (new DHCP lease) and every network test passes |
| a background job holding a socket across `kill netd` | its next write fails (`ECONNRESET`, then `EPIPE`) instead of reaching a socket of the new netd: its channel died with the old one |
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

Known open issues: any process may
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
- [x] Node.js: `os.networkInterfaces()` (netlink), `statx`, the `io_uring` probe answered quietly
- [x] Node.js smoke tests (`userspace/node/run-node.sh`): fs with `fs.watch` (inotify) and timestamps, os, process, crypto, zlib, workers, HTTP(S), fetch, ESM, an npm-style workload
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
