<div align="center">

# oxidenix

**A Unix-like x86_64 kernel written from scratch in Rust, designed, implemented and debugged by an AI.**

It boots in QEMU and runs an unmodified, statically linked **GNU Bash 5.3** and **BusyBox**
on top of a Linux-compatible system call interface. It is moving towards a **microkernel**: the
disk driver and the ext2 filesystem already run as a user-space server.

[![test](https://github.com/devfbe/oxidenix/actions/workflows/test.yml/badge.svg)](https://github.com/devfbe/oxidenix/actions/workflows/test.yml)
![Rust](https://img.shields.io/badge/language-Rust%20(nightly)-orange?logo=rust)
![Arch](https://img.shields.io/badge/arch-x86__64-blue)
![Boot](https://img.shields.io/badge/boot-BIOS%20via%20bootloader%200.11-lightgrey)
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

- **Boots** via BIOS into a 64-bit higher-half kernel with a framebuffer text console.
- **Runs real Linux binaries**: static musl executables such as GNU Bash 5.3 (with readline),
  BusyBox 1.37 (~400 applets: `ls`, `cat`, `grep`, `sed`, `dd`, `vi`, ...) and its own C test
  programs, all unmodified.
- **Interactive shell** with line editing, history (arrow keys), tab completion, colors, pipes,
  redirections, subshells, command substitution and arithmetic.
- **Symmetric multiprocessing**: all CPUs (QEMU runs with 4) execute user programs and the
  kernel in parallel, with fine-grained locks instead of a big kernel lock. CPU-bound programs
  scale (about 3x on 4 emulated CPUs), and `sched_setaffinity` pins processes to CPUs.
- **Preemptive multitasking**: round-robin scheduler at 100 Hz, separate address spaces,
  `fork` with **copy-on-write**, `execve`, `wait4`, process groups and sessions.
- **POSIX signals**: handlers, masks, `kill`, `SIGCHLD`, `EINTR` with automatic syscall restart,
  and **Ctrl+C** interrupting any foreground program, even a busy loop without system calls.
- **Job control**: Ctrl+Z stops the foreground job, then `jobs`, `fg`, `bg` and `kill %n` work
  in Bash; background jobs reading from the terminal are stopped with `SIGTTIN`.
- **procfs (static)**: `/proc/mounts` and `/proc/cpuinfo`.
- **Filesystem**: an in-memory, tmpfs-like VFS populated from a cpio initramfs, with files,
  directories, symlinks, `/dev/{console,tty,null,zero}`, and quotas against heap exhaustion.
- **Microkernel-style drivers**: the ATA driver and the read-write **ext2** filesystem run in
  `diskfs`, an ordinary ring-3 process that talks to the kernel over IPC and reaches the disk
  through I/O ports the kernel granted it. If it dies, the kernel restarts it on the next access, and
  the rest of the system keeps running.
- **Networking**: TCP, UDP and raw ICMP sockets over IPv4 with DNS, so `wget`, `nc`, `ping` and
  `nslookup` from BusyBox reach the Internet through QEMU's user network. The driver for the virtio network
  card and the TCP/IP stack (smoltcp) run in `netd`, a user-space server; the interface is
  configured by DHCP, and loopback (`127.0.0.1`) works too.
- **Persistent storage**: the data disk is mounted at `/data`, survives reboots, and stays
  consistent enough that `e2fsck` on the host accepts it.
- **Wall-clock time** from the CMOS real-time clock (`date`, file timestamps).
- **Terminal**: a termios line discipline (canonical and raw mode, echo, erase/kill/word-erase,
  EOF), the ANSI escape sequences BusyBox and readline use, a German keyboard layout and UTF-8.
- **~120 Linux system calls**, enough for Bash and BusyBox (see [System calls](#system-calls)).

## Quick start

### Requirements

- **Rust nightly** with `rust-src` and `llvm-tools-preview` (pinned in `rust-toolchain.toml`).
- **QEMU** (`qemu-system-x86_64`).
- **Nix**: the userland is fetched and cross-compiled from nixpkgs
  (`pkgsStatic.stdenv.cc` for musl, `pkgsStatic.busybox`, `pkgsStatic.bash`).

### Build and run

```sh
cd kernel
cargo run
```

This builds the kernel, assembles the root filesystem (C test programs, Bash, BusyBox and
`userspace/rootfs/`), packs it as a cpio initramfs, creates a BIOS disk image and starts QEMU.
On the first run it also creates `disk.img`, a 64 MiB ext2 data disk (via `mke2fs` from nixpkgs,
pre-filled from `userspace/disk/`). This file is kept between runs; delete it for a fresh disk.
Extra arguments after `--` are passed to QEMU; `OXIDENIX_BUILD_ONLY=1 cargo run` only builds the images.

The kernel boots straight into Bash. Things to try:

```sh
ls -l /bin | head          # BusyBox applets
cowtest; sigtest; jobtest; oomtest; fstest; forktest; nettest; smptest # self-tests
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
 │  statically linked against musl libc             │  ext2 + ATA driver, I/O ports    │
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
      bootloader 0.11 (BIOS), QEMU x86_64, 4 CPUs, 256 MiB RAM, IDE disk, virtio-net
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
│       ├── memory/              physical frame allocator with refcounts (frame.rs),
│       │                        kernel heap and page table access (mod.rs)
│       ├── process/             process lifecycle and syscalls on it (mod.rs),
│       │   ├── task.rs          tasks: owned state, locked info and signals
│       │   ├── sched.rs         run queues, wait queues, context switch, idle
│       │   ├── address_space.rs per-process page tables, copy-on-write
│       │   ├── syscall.rs       syscall entry/return, dispatch table
│       │   ├── sys_file.rs      file, directory, pipe, tty-ioctl, poll/select
│       │   ├── sys_mem.rs       brk, mmap, munmap
│       │   ├── sys_net.rs       socket syscalls (sockaddr_in, msghdr, options)
│       │   ├── signal.rs        signal state, delivery, sigreturn, kill
│       │   ├── loader.rs        ELF loading and the Linux initial stack
│       │   ├── elf.rs           ELF64 parser
│       │   ├── ipc.rs           services and message passing
│       │   ├── irq.rs           device interrupts for user-space drivers
│       │   └── uaccess.rs       checked access to user memory
│       ├── net.rs               socket client: operations become netd requests
│       ├── fs/                  VFS (mod.rs), open files and pipes (file.rs),
│       │                        initramfs unpacker (cpio.rs), IPC client for
│       │                        filesystem servers (remote.rs)
│       ├── drivers/             framebuffer console (console.rs), TTY (tty.rs),
│       │                        PS/2 keyboard (keyboard.rs), CMOS clock (rtc.rs),
│       │                        serial port mirror (serial.rs), PCI scan (pci.rs),
│       │                        ACPI MADT (acpi.rs)
│       ├── sync.rs              IrqSpinLock: fair, interrupt-safe ticket lock
│       └── shell/               built-in kernel monitor (fallback shell)
├── servers/
│   ├── diskfs/                  user-space ext2 server with its own ATA driver
│   └── netd/                    network server: virtio-net driver (virtio.rs), loopback
│                                (nic.rs), sockets on smoltcp (service.rs), DHCP
├── crates/
│   ├── ext2fs/                  ext2 as a library over a `Device` trait
│   ├── fsproto/                 message format between the VFS and filesystem servers
│   ├── netproto/                socket operations between the kernel and netd
│   └── oxrt/                    runtime for servers: entry, syscalls, heap, port I/O
├── builder/                     host tool: rootfs + cpio + boot image + ext2 data disk + QEMU
└── userspace/                   C test programs, build script, rootfs and data disk templates
```

About 9,206 lines of Rust in the kernel and 3,067 in the servers, their libraries and runtime, plus a small host-side builder.

### Boot sequence

1. The **bootloader** (BIOS, `bootloader` 0.11) loads the position-independent kernel ELF into
   the upper half (`dynamic_range_start = 0xffff_8000_0000_0000`). It maps all physical
   memory at a dynamic offset, sets up a VESA framebuffer and loads the initramfs as a ramdisk.
2. `kernel_main` runs these steps in order: framebuffer console and boot logo → GDT/TSS/IDT
   (interrupts still off) → frame allocator and 16 MiB kernel heap → ACPI tables (MADT), the
   local APIC with its calibrated timer and the I/O APIC (the 8259 PICs are masked) → VFS from the cpio
   ramdisk and real-time clock → process subsystem (SSE, syscall MSRs, process 0) → the other
   CPUs (INIT and STARTUP IPIs; each runs a real-mode trampoline from a page below 1 MiB into
   long mode, then sets up its GDT, TSS, GS block, local APIC timer and idle task) →
   **interrupts on**.
3. The kernel starts the servers: `/sbin/diskfs` asks for its I/O ports, mounts the ext2 disk
   and registers as service `diskfs`; the kernel then mounts it at `/data`. Then the kernel
   scans PCI for a virtio network card and starts `/sbin/netd` with its ports, interrupt line
   and a DMA area; netd registers as service `net` once DHCP has configured the interface
   (or after three seconds without an answer).
4. Process 0 (the kernel monitor) spawns `/bin/bash` as the foreground process and waits for
   it. If Bash exits, the monitor takes over the terminal.

### Memory

| Region | Address | Notes |
|---|---|---|
| User ELF image | from `0x40_0000` | static `ET_EXEC` binaries, segments mapped with W/NX from program headers |
| `brk` heap | after the highest segment | grows on demand |
| `mmap` area | top-down from `0x7000_0000_0000` | anonymous and private file mappings |
| User stack | below `0x7fff_ffff_f000` | 256 KiB, Linux initial stack (argc, argv, envp, auxv) |
| Kernel image, stacks, boot info, framebuffer, physical memory map | upper half, chosen by the bootloader | shared by all address spaces |
| Kernel heap | `0xffff_c000_0000_0000` | starts at 8 MiB and grows on demand (up to 1 GiB virtual) |

- **Frame allocator**: a bump allocator over the usable regions of the boot memory map, plus a
  free list threaded through the freed frames themselves. Every frame carries a **reference
  count**; freeing drops one reference.
- **Address spaces**: each process has its own level-4 table. The lower half belongs to the
  process, the upper half entries are copied from the kernel's table. Dropping an address space
  walks the lower half and releases every table and frame.
- **Copy-on-write**: `fork` shares all user frames. Writable pages become read-only in both
  processes and are tagged with an OS-available page table bit. A write fault either copies the
  frame or, for the last owner, just restores write access. Kernel writes into user memory
  resolve COW first, because they would not fault.
- **Out of memory is an error, not a panic**: the kernel heap grows by mapping more frames
  when an allocation fails. User memory (pages, page tables, kernel stacks for `fork`) may not
  take the last 16 MiB of RAM, which stay reserved for the heap. Large allocations that user space
  can trigger (kernel stacks, file contents, pipe buffers, `execve` arguments) are fallible and
  return `ENOMEM`/`E2BIG`, and at most 256 processes can exist (`EAGAIN` beyond that).
- **User memory access** (`uaccess.rs`) checks every page of a user range against the active
  page tables before the kernel touches it. Bad pointers yield `EFAULT`, never a kernel fault.

### Processes and scheduling

The scheduler is built for several CPUs (`process/sched.rs`, design in
[docs/design/smp.md](docs/design/smp.md)); there is no big kernel lock.

- **Tasks** (`process/task.rs`): a process is an `Arc<Task>`. What only the process itself
  touches (address space, descriptors, cwd, FPU state) is owned by the task; relations,
  name and exit/stop reports (`info`) and signal state with the interval timer (`sig`) have
  their own locks; the scheduling state is atomic. The process table maps pids to tasks.
- **One kernel stack per process** (64 KiB). A context switch saves callee-saved registers,
  the FPU/SSE state (`fxsave`), the FS base (musl's TLS pointer), and switches CR3, the TSS
  stack and I/O bitmap, and the syscall stack in the CPU block.
- **Per-CPU run queues**, round robin, driven by each CPU's local APIC timer at 100 Hz
  (calibrated against the PIT once at boot). A woken task goes to an idle CPU if there is
  one (preferring the CPU it last ran on); the waker claims that CPU atomically and wakes it
  with an IPI, so a burst of new tasks spreads over all idle CPUs. An idle CPU steals work
  from the others. Each task has a CPU affinity mask (`sched_setaffinity`, inherited by
  `fork`) that queueing, stealing and migration respect. Each CPU has an idle task; idling loads the kernel's page table, so an address
  space is only ever active on the CPU running its process.
- **`on_cpu`** marks a task whose kernel stack is still in use; a CPU that picks it waits
  until its previous CPU finished switching away, and the reaper frees a zombie only then.
- **Sleeping without lost wakeups**: a blocking path first registers on its wait channel
  (`prepare_to_wait`), then checks its condition, then sleeps; a per-task wake lock
  serializes wakeups with the task descheduling itself. Pipes, the TTY, IPC, `wait4`, stops
  and timed sleeps all use this protocol.
- The kernel is non-preemptive: only user code is preempted, and syscalls run with
  interrupts disabled. Locks are `IrqSpinLock`s (fair tickets, interrupts off while held).
- New processes start by *returning from a syscall*: their kernel stack is pre-filled with a
  register frame that `user_return` consumes. A `fork` child is the parent's frame with
  `rax = 0`.
- Process groups, sessions and the terminal's foreground group follow POSIX closely enough for
  Bash and BusyBox job handling.

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

### Signals

- Per process: 64 actions, a blocked mask and a pending set. `fork` inherits actions and mask,
  `exec` resets caught signals.
- **Delivery** happens on every return to user space (after syscalls and after timer
  preemption). Default actions terminate, ignore or stop. For handlers, the kernel pushes a
  signal frame on the user stack: restorer address, saved register frame, saved mask, the
  FPU/SSE state (asynchronous handlers would otherwise clobber it) and `siginfo`.
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
  without copying file contents. Files reference the ramdisk until their first write
  (copy-on-write).
- **Open files** are shared descriptions (`Arc<OpenFile>`) with offset and flags, so `dup`,
  `fork` and close-on-exec behave as on Linux.
- **Pipes** have a 64 KiB buffer with blocking reads and writes and EOF/`EPIPE` semantics.
- **Quotas**: file contents and inode metadata are charged against an 8 MiB budget (`ENOSPC`),
  and single files are limited to 64 MiB (`EFBIG`). Without this, user programs could exhaust
  the kernel heap.

### Microkernel architecture

oxidenix is being restructured into a microkernel step by step, keeping the Linux syscall
interface working after every step. The kernel keeps memory management, scheduling, IPC and
interrupt dispatch; drivers and filesystems move into user-space servers.

- **IPC** (`process/ipc.rs`): a privileged server registers a service name (`ipc_register`),
  then loops over `ipc_receive` and `ipc_reply` (oxidenix syscalls 1000-1002). The kernel is
  the client on behalf of user programs: `call` queues a request and sleeps uninterruptibly
  until the reply; `post` queues a message without waiting, for contexts that must not sleep
  (releasing an unlinked inode while dropping it). Messages up to 64 KiB are copied through the
  kernel.
- **Hardware access without kernel drivers**: servers started by the kernel are *privileged*
  and may call `ioperm`, but only for the ports the kernel assigned to them (diskfs gets the
  primary ATA channel, `0x1f0`-`0x1f7` and `0x3f6`; netd gets the I/O BAR of its network card;
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
  `fsproto` requests to a server (`fs/remote.rs`). Reads and writes are split into 32 KiB
  messages. The kernel still decides when an unlinked inode may be freed, because only it knows
  whether a file is open.
- **Fault isolation**: when a server dies, its services are marked dead and every pending
  request fails with `EIO`; the kernel and the rest of user space keep running.
- **Self-healing**: the next request to a dead server starts it again (in the
  context of the requesting program, which may sleep) and continues transparently. Inode numbers
  live on disk, so files and directories that were open before the crash stay usable. Requests
  that were in flight during the crash still fail with `EIO`, since they may or may not have
  been carried out. After five restarts the kernel gives up and the mount stays at `EIO`.
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
- Still in the kernel today: the VFS itself, pipes, the TTY, console and keyboard. They are the
  next candidates for servers.

### Networking

- **netd** (`servers/netd`) owns the network card. The kernel finds it on PCI (a virtio-net
  card in legacy mode, whose registers are all I/O ports) and hands netd its ports, its
  interrupt line and a 512 KiB DMA area. netd sets up the two virtqueues with 64 fixed 2 KiB
  buffers each, sleeps in `ipc_receive` until a request, a card interrupt or the next timer
  of the TCP/IP stack, and runs [smoltcp](https://github.com/smoltcp-rs/smoltcp) for ARP,
  IPv4, ICMP, TCP, UDP and the DHCP client.
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
- Not yet: IPv6, other raw protocols, `AF_UNIX`, and interface configuration from user space
  (`ifconfig`). Times shown by `ping` have the 10 ms resolution of the timer tick.

### Persistent storage

- **ATA PIO driver** in `diskfs` for the second IDE disk (primary bus, slave). It uses LBA28,
  polls with the controller interrupt disabled, and flushes the write cache after every write.
- **ext2** (`crates/ext2fs`; revision 1 with the `filetype` feature, 1/2/4 KiB blocks) supports reading and
  writing files through direct, single, double and triple indirect blocks, holes, truncation
  (freeing whole indirect subtrees), directories growing by blocks, fast and block symlinks,
  `rename` across directories (with `..` and link count updates and a cycle check), `rmdir`, and
  `chmod`. Allocation goes through the block and inode bitmaps, and group descriptors and the
  superblock's free counts are updated on every change.
- Writes are synchronous (no cache), so `sync`/`fsync` have nothing left to do. After a session,
  `e2fsck -fn disk.img` on the host reports a clean filesystem, and `debugfs` can read the files.
- **VFS integration**: every inode operation (`child`, `list`, `create`, `unlink`, `read_at`,
  `write_at`, `truncate`, ...) works on memory and remote (server-backed) inodes alike. Remote inodes are cached per
  filesystem so that one disk inode always maps to one `Arc<Inode>`. The disk root is mounted
  at `/data`, and `statfs` and a static `/proc/mounts` make `df` work.
- A file that is deleted while still open stays allocated as an orphan until the last
  reference is dropped, as on Linux, so its inode number cannot be reused under an open file.
- Only regular files are read, written, truncated or executed through their data blocks; a
  fast symlink's block pointers hold text, never block numbers.
- Directory reads take a snapshot at offset 0, so `rm -r` deleting entries while it reads never
  skips any.

### Terminal and console

- **Console**: a cell grid on the framebuffer with a 24 px Noto Sans Mono bitmap font, 16 ANSI
  colors, a visible cursor, deferred line wrap, UTF-8 (Latin-1), cursor movement, erase,
  insert/delete characters, SGR attributes and cursor position reports.
- **Boot logo**: drawn centered above the first boot message. Its source is
  `kernel/assets/logo.svg`; `kernel/build.rs` decodes the rendered `logo.png` into raw RGB at
  build time, so the kernel needs no image decoder. Like on Linux, the logo is plain pixels and
  scrolls away with the text.
- **TTY**: a termios subset (`TCGETS`/`TCSETS*`, `ICANON`, `ECHO*`, `ISIG`, `ICRNL`, `VMIN`, ...)
  with canonical line editing, raw mode for readline, EOF handling and `FIONREAD`. Its buffers
  have fixed sizes because the keyboard path runs in interrupt context and must not allocate.
- **Keyboard**: PS/2 scancodes are decoded in the IRQ handler with the German (`De105Key`)
  layout and translated to terminal bytes: CR, DEL, and VT100 sequences for arrows and editing
  keys.

### Userland build

`userspace/build.sh` cross-compiles every `userspace/*.c` with the musl toolchain from nixpkgs
and copies static Bash and BusyBox (with symlinks for all applets) into the root filesystem. The
builder adds `userspace/rootfs/` (`/etc/passwd`, `/etc/motd`, `/etc/test.sh`, ...), writes the
cpio archive and hands it to the bootloader as a ramdisk.

## System calls

Linux x86_64 numbers, grouped by area (about 120 in total):

| Area | Calls |
|---|---|
| Files | `read` `write` `pread64` `pwrite64` `readv` `writev` `open` `openat` `close` `lseek` `sendfile` `ftruncate` `fcntl` `ioctl` `dup` `dup2` `dup3` `pipe` `pipe2` |
| Metadata | `stat` `fstat` `lstat` `newfstatat` `access` `faccessat` `faccessat2` `readlink` `readlinkat` `chmod` `fchmodat` `utimes` `futimesat` `utimensat` `umask` |
| Directories | `getdents64` `getcwd` `chdir` `fchdir` `mkdir` `mkdirat` `rmdir` `unlink` `unlinkat` `rename` `renameat` `renameat2` `symlink` `symlinkat` |
| I/O multiplexing | `poll` `ppoll` `select` `pselect6` |
| Memory | `brk` `mmap` `munmap` `mprotect` (no-op) |
| CPUs | `sched_getaffinity` `sched_setaffinity` `getcpu` |
| Processes | `fork` `vfork` (as `fork`) `execve` `exit` `exit_group` `wait4` `getpid` `getppid` `gettid` `set_tid_address` `sched_yield` `arch_prctl` `prlimit64` `getrusage` |
| Groups and IDs | `setpgid` `getpgid` `getpgrp` `setsid` `getsid` `getuid` `geteuid` `getgid` `getegid` `getresuid` `getresgid` `setuid` `setgid` |
| Signals | `rt_sigaction` `rt_sigprocmask` `rt_sigreturn` `kill` `tkill` `tgkill` `pause` `sigaltstack` `alarm` `setitimer` `getitimer` (`ITIMER_REAL`, 10 ms resolution) |
| Filesystems | `statfs` `fstatfs` `sync` `fsync` `fdatasync` |
| Servers | `ioperm` (privileged servers only), `ipc_register` (1000), `ipc_receive` (1001, with timeout and interrupt notifications), `ipc_reply` (1002), `irq_enable` (1003), `dma_map` (1004) |
| Power | `reboot` (power off ends QEMU, restart resets the machine) |
| Sockets | `socket` `bind` `listen` `accept` `accept4` `connect` `sendto` `recvfrom` `sendmsg` `recvmsg` `shutdown` `getsockname` `getpeername` `setsockopt` (ignored) `getsockopt` (`AF_INET` only: TCP, UDP, raw ICMP) |
| Time and misc | `nanosleep` `clock_gettime` (`CLOCK_REALTIME` from the RTC) `uname` (reports `oxidenix`, not Linux) `getrandom` |

Everything runs as root. Unknown syscalls print a kernel message and return `ENOSYS`.

## Testing

`OXIDENIX_TEST=1 cargo run` (in `kernel/`) boots straight into `/etc/runtests.sh`, which runs
every self-test below. The kernel mirrors its console to the serial port and powers off when
the script ends; QEMU's exit status is 1 if everything passed and 3 otherwise. GitHub Actions
does exactly this on every push (without a display), then checks the ext2 image with `e2fsck`.

Each of these programs and scripts lives in the root filesystem and runs inside oxidenix:

| Test | Covers |
|---|---|
| `cowtest` | copy-on-write isolation between parent and child, kernel writes into shared pages, 50 forks, shared read-only frames under `brk` |
| `oomtest` | fork bomb (stops at the process limit), memory exhaustion via `mmap`, 100 full pipes; the kernel survives and memory is reusable |
| `sigtest` | handlers, killing a busy loop, `SIGCHLD`, `EINTR` on pipe reads, blocked and ignored signals, FPU state across asynchronous handlers, `alarm` and repeating `setitimer`, catchable `SIGFPE`/`SIGSEGV`/`SIGTRAP` from CPU exceptions, an uncaught `SIGFPE` killing the process |
| `jobtest` | stop/continue reporting through `wait4`, restart of a stopped `read()`, `SIGKILL` on stopped processes, `SA_RESTART` |
| `forktest` | `fork`, `execve`, `wait4`, preemptive interleaving of two workers |
| `smptest` | CPU count and affinity (pinning to every CPU, empty masks), parallel speed-up of CPU-bound processes, `fork`/`exit`/`wait` on every CPU at once, 5000 pipe round trips between two CPUs, signals to a process running on another CPU |
| `nettest` | TCP to an echo service through QEMU, `ECONNREFUSED`, `listen`/`accept` over loopback with a forked client, EOF after the peer closed, non-blocking `accept` and `connect` with `poll` and `SO_ERROR`, `EINTR` in a blocking `recv`, UDP over loopback, raw ICMP echo to the gateway and over loopback, source address for off-subnet destinations, overflowing message vectors, `AF_INET6` rejected |
| `sh /etc/test.sh` | files, pipes, `cd`, `mkdir`/`touch`/`rm`, rename cycles via symlinks, file quota |
| `fstest` | descriptor access modes (`EBADF` on read-only/write-only fds), `O_NOFOLLOW` on symlinks, unlinked-but-open files (kept until closed, never shared with new files), ext2 size limits, overflowing `mmap` offsets |
| `sh /etc/disktest.sh` | ext2: 150-file directory, 1.5 MiB file (double indirect), append, truncate, rename, cycles, symlinks, `rm -r`, space accounting |
| `e2fsck -fn disk.img` (host) | the filesystem written by oxidenix is consistent |
| `timeout 1 sleep 5` | `vfork` and `SIGTERM` after the time limit (exit status 143) |
| `kill -9 1` in Bash | user space cannot kill a server (`EPERM`) |
| `kill diskfs` in the kernel monitor | the next `/data` access restarts the server; open files survive; after five restarts accesses fail with `EIO`; a restart still runs the boot-time program even after `/sbin/diskfs` was overwritten |
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
call `reboot` (everything runs as root), the kernel heap
never returns grown memory to the frame allocator, and there are no users or permissions
(everything runs as root). The ext2 driver trusts the on-disk metadata of the image it was given.
There is no IOMMU support: a server that drives a bus-mastering device (netd) can make the
device read or write any physical memory, so such a server is effectively as trusted as the
kernel. Its program is fixed at boot (see self-healing), but a bug in it is a kernel-level bug.

## Limitations and roadmap

- [x] Copy-on-write `fork`
- [x] `ENOMEM` instead of a kernel panic when memory runs out
- [x] Job control: stopping (Ctrl+Z), `fg`/`bg`, `SIGCONT`
- [x] Persistent storage: a disk driver and an on-disk filesystem
- [x] Unlinked-but-open files kept until closed
- [ ] Hard links and a block cache
- [x] Networking: TCP/UDP sockets, DNS, DHCP and loopback through a user-space server (`netd`)
- [x] `ping` (raw ICMP sockets)
- [ ] IPv6, `AF_UNIX`, `ifconfig`
- [x] SMP with fine-grained locking, per-CPU run queues and CPU affinity
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
