# ADR 0001: The Linux ABI moves out of the kernel, into a server in restricted mode

Date: 2026-10-07. Status: accepted (design and migration to follow).

## Context

The kernel implements the Linux system call ABI itself (`process/syscall.rs` and the
`sys_*` handlers: file descriptors, VFS, signals, process tree, `mmap`, ELF loading, ttys,
sockets). Drivers, the filesystem and the network stack already run in user space. The goal
is a microkernel that can run a Linux userland without implementing Linux: the kernel offers
mechanisms (address spaces, threads, scheduling, IPC, interrupts, timers), and Linux
semantics live in a user-space server that receives the trapped system calls and exceptions.

## Options

- **A. A separate server process.** The kernel turns each trapped `syscall` into a message
  carrying the registers, the Linux server handles it (reaching the client's memory and
  address space through kernel primitives) and replies; the thread resumes. Simple and
  strictly separated, but every system call costs a full round trip: two address space
  switches and the scheduler, even with a direct-handoff fast path and PCID.
- **B. Restricted mode (as Fuchsia's starnix).** The Linux server is mapped into every Linux
  process, but the Linux code cannot reach it. A `syscall` switches the same thread into the
  server's mode (a different page table root, tagged with its PCID, so no TLB flush), without
  the scheduler and without copying a message; the server reads and writes the client's
  memory directly. Close to the cost of an in-kernel system call; the isolation between the
  Linux layer and the microkernel stays. More complex: mode switches in the kernel, and the
  server's state shared by all Linux processes.
- **C. Keep the ABI in the kernel** (today). Fastest, but the kernel implements Linux, which
  is what the goal excludes.

## Decision

B. System calls are the most frequent kernel entry of all; a design that costs two context
switches for each of them (A) would make every program slow, not just I/O. B keeps the
isolation and the cost of a call near C.

## Consequences

- Most of today's kernel code (system call handlers, VFS, signals, loader, pipes, ttys, the
  socket and filesystem clients) moves into the Linux server; the kernel shrinks to
  mechanisms. A design document will define the mode switch, the kernel primitives the
  server needs, and the migration in steps that keep the suite green.
- The I/O refactoring (rings, buffer grants: see `docs/io-path-audit.md`), the IOMMU work and
  Node.js follow this structure instead of today's.
- Before the move, the benchmarks of today's system are taken as the baseline.
