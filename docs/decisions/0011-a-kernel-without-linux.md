# ADR 0011: A kernel without Linux: the last calls in the server, natives on the kernel's own interface

Date: 2026-10-09. Status: accepted.

## Context

R9 removes the pass-through (`docs/design/linux-server.md`, "Removing the pass-through"): the
Linux server handed every call it did not implement back to the kernel (`legacy_syscall`),
which kept a Linux implementation for it and, through the same table, for the native servers
(diskfs, netd, procfs, ringtest), which spoke Linux's numbers. A count of what still reached
the kernel (the self-tests and the Node.js smoke tests) found nine calls (futex, arch_prctl,
uname, sysinfo, clock_settime, prlimit64, getcpu, getrandom, and a test call), and the files
of the kernel's tree that `/dev` still was. Four questions decided the shape:

1. Where futex's semantics live, now that the server must wait on program memory.
2. What the native servers call, once the kernel has no Linux table.
3. What becomes of `/dev` and the kernel's VFS.
4. What the kernel keeps of a process.

## Decision

1. **Futex in the server, keys in the kernel.** The server decodes Linux's operations
   (FUTEX_WAIT, FUTEX_WAIT_BITSET with FUTEX_CLOCK_REALTIME, FUTEX_WAKE, FUTEX_WAKE_BITSET,
   FUTEX_REQUEUE, FUTEX_CMP_REQUEUE), reads the timeouts and decides restarts (a timed wait
   goes on through restart_syscall). The kernel offers wait, wake and requeue on the caller's
   address space with the keys it always had (address space and address, or a shared
   mapping's object and offset, so processes and native servers sharing a channel meet), a
   bitset, a monotonic deadline and kicks: the mechanism a futex needs and nothing of Linux's
   encoding. The same three calls serve the native servers on their own memory.
2. **The natives get the kernel's own numbering.** Their calls are `kernel/src/process/
   native.rs`'s: where a call is a mechanism the server has too (anonymous `mo_map`,
   `mo_unmap`, `mo_protect`, the futex calls, `clock_read`, `yield`, `random`,
   `thread_exit`) it takes `restricted`'s number and contract, so one implementation serves
   both; `ioperm`, `exec` (the server's own program again) and `log` (text on the console)
   join IPC, interrupts, DMA and channels. A Linux number means nothing to the kernel, from
   anyone.
3. **No VFS in the kernel.** `/dev` is a tmpfs of the server's, made with the instance (the
   device nodes name the server's drivers by number, ADR 0007; udev's links; devpts and shm
   below it). The kernel reads its own programs from the boot image by name (a cpio lookup),
   committed at boot so a server never dies of a full tmpfs at a fault, and hands the image
   whole to each instance as before. Its descriptor tables, open files, pipes, eventfds and
   working directories went with the VFS.
4. **Processes are containers with an end.** The kernel keeps threads, an address space,
   CPU time and memory counts, and how a process ends (`kill.rs`): the kernel's own kill of a
   whole process (the monitor, memory running out, a native server's fault, a failed Linux
   server), and what its waits ask (`interrupted`: a Linux thread's kick or the process's
   end; `dying`). Signals, process groups, sessions, parents other than the kernel and wait4
   are gone; the kernel's own processes are its zombies until it reaps them, with Linux's
   encoding of wait statuses.

## Consequences

- The kernel implements no Linux system call; what a program misses is the server's ENOSYS
  (said on the console, as the kernel used to).
- The server owns every Linux semantic, so a fix to one is a server change; the kernel's
  interface grew by six narrow calls (`futex_wait`, `futex_wake`, `futex_requeue`,
  `thread_fs`, `clock_set`, `power`) and `file_pages` for tmpfs's statfs.
- The native servers can no longer be confused with Linux programs; ringtest's checks of the
  kernel's refusals use the native calls (`random` into a read-only grant, `futex_requeue`).
- Resource limits other than RLIMIT_NOFILE are Linux's defaults the server holds to, accepted
  but not kept per process until the process table keeps them.
