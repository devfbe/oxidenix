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
5. **The machine's state is a capability** (added in review). Setting the wall clock
   (`clock_set`) and powering off or restarting (`power`) act on the whole machine, not one
   tree: they need the instance's **host grant**, which the kernel gives when it starts a
   tree itself (the monitor's `run`, autorun: today the only trees there are), as it grants
   the console, and takes back with the console once the tree's first process has ended
   (what is left of the tree runs on without either). Without it both are EPERM, and the
   server acts as Linux in a pid namespace that is not the initial one: `clock_settime` and
   `settimeofday` are EPERM, and `reboot` ends the tree (its init dies, and with it every
   process; its parent's wait reports SIGHUP for a restart, SIGINT for a power off or halt;
   the caller exits), as `reboot_pid_ns` does: any other command (CAD_ON, CAD_OFF) is
   EINVAL and a RESTART2's string is not read. The server asks for the grant first
   (`host_granted`) where Linux checks the capability before the arguments. A test-mode
   call (`TEST_HOST`) takes the grant from the self-tests' tree for the clock's checks and
   reboot's refusals; a reboot without it ends the suite's own tree and is not tried there.
6. **Requeues move only plain futex waits** (added in review): `futex_requeue` moves
   waiters that entered through `futex_wait`, never the Linux server's own waits
   (`server_futex_wait` on its memory or an object mapped there, `server_wait`) nor doorbell
   watches. A word of a channel is named by its service too: without this a native server
   could take a server thread off the word it waits on (and a vectored wait would no longer
   find its entry where it put it). As in Linux, the `n_wake` wakes go to whichever waiters
   come first and the moves to up to `n_move` of the movable ones left, so a wake spent on a
   waiter that cannot move takes nothing from the moves (`TEST_FUTEX_WATCH` checks it). A
   wake drops its references to the tasks it took out of a bucket only after the bucket's
   lock: a task's last reference takes its thread's state with it, whose drop takes other
   locks (the 21cd469 deadlock's pattern).

## Consequences

- The kernel implements no Linux system call; what a program misses is the server's ENOSYS
  (said on the console, as the kernel used to).
- The server owns every Linux semantic, so a fix to one is a server change; the kernel's
  interface grew by six narrow calls (`futex_wait`, `futex_wake`, `futex_requeue`,
  `thread_fs`, `clock_set`, `power`), `file_pages` for tmpfs's statfs and `host_granted`
  for what Linux decides by the capability before the arguments (decision 5).
- The native servers can no longer be confused with Linux programs; ringtest's checks of the
  kernel's refusals use the native calls (`random` into a read-only grant, `futex_requeue`).
- Resource limits start as Linux's defaults and are kept per process in the server
  (`ids::Limits`: inherited by fork, kept by execve and by a zombie); RLIMIT_NOFILE left the
  descriptor table for them (ADR 0009's update). The server holds to RLIMIT_STACK (the
  kernel's `MO_GROWSDOWN` takes the stack's limit), RLIMIT_NOFILE, RLIMIT_SIGPENDING and
  RLIMIT_CORE; RLIMIT_NPROC binds no root process on Linux, nor here.
