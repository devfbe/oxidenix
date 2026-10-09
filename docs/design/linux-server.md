# The Linux server: system calls in restricted mode

Status: accepted; phase R1 implemented (the shared region uses PML4 slot 128, 512 GiB). Decisions: ADR 0001-0004.

## Goal

oxidenix runs a Linux userland without the kernel implementing Linux. The kernel offers
mechanisms: address spaces and memory objects, threads and scheduling, waiting and waking,
timers, IPC, interrupts and device access. Everything that is Linux — file descriptors, the
VFS and its filesystems, pipes, ttys, sockets, signals, process ids and the process tree,
`fork`/`exec`/`wait`, `mmap` semantics, ELF loading, `/proc` — lives in one user-space program,
the **Linux server** (`servers/linux`). A Linux program's `syscall` instruction traps into the
kernel, which hands it to the Linux server on the same thread, without a message and without
the scheduler (restricted mode, as Fuchsia's starnix).

Each process tree the kernel starts (`/init`, a program run from the kernel monitor) gets its
own **instance** of the server (ADR 0002): a container with its own VFS and tmpfs, pids,
pipes and signals; the disk, the network and the console device are shared through the
servers and the kernel. Non-goal: binary compatibility with Linux kernel modules.

## Restricted mode

### Two views of one address space

The lower half of the virtual address space (47 bits) is split:

| range | PML4 slots | contents | visible in |
|---|---|---|---|
| `0` – `0x3fff_ffff_ffff` (64 TiB) | 0–127 | the Linux program: the **restricted region**, one per Linux process | both views |
| `0x4000_0000_0000` – `0x7fff_ffff_ffff` | 128–255 | the Linux server: code, heap, per-thread stacks and state; the **shared region**, the same page tables in every process of one instance | normal view only |
| upper half | 256–511 | the kernel | supervisor only |

Each Linux process has two page table roots that share all lower-level tables: the
**restricted root** has slots 0–127 only, the **normal root** has slots 0–127 and the shared
slots 128–255. A Linux program runs on the restricted root and cannot reach the server's
memory; the server runs on the normal root and reaches the program's memory directly, as a
kernel would. (The kernel keeps slots 0–127 of both roots equal; they change only when a
new PDPT is created.)

Trade-off: Linux programs get 46 bits of address space instead of 47 (ADR 0003).

### Threads and the mode switch

A thread of a Linux process has two register contexts: the **restricted context** (the Linux
program's registers) and the **normal context** (the server's). The server's side is a loop:

```rust
loop {
    let reason = restricted_enter(state);   // runs the program until it traps
    match reason {
        Syscall => handle_syscall(state),   // state.regs: rax, rdi, ...; result into state.regs.rax
        Fault { addr, access } => handle_fault(state, addr, access),
        Exception { vector, error } => handle_exception(state, vector, error),
        Kick => handle_kick(state),          // a signal or a stop for this thread
    }
}
```

- `restricted_enter(state)` (a kernel call) saves the server's registers in the kernel, loads
  the program's from `state` (a block in the server's memory, bound to the thread), switches
  to the restricted root and returns to the program.
- The program's `syscall` (or an exception, or a page fault the kernel cannot resolve) enters
  the kernel, which writes the program's registers to `state`, switches to the normal root
  and returns from `restricted_enter` with the reason. No scheduler, no queue, no copy beyond
  the registers.
- Page table switches use PCIDs (CR4.PCIDE; the no-flush bit on CR3 writes), so neither
  switch flushes the TLB. Both roots of a process get their own PCID; invalidating a mapping
  of the restricted region flushes it under both (`invpcid`, or a generation that forces a
  flush at the next load on CPUs that do not run the process now).
- **The server uses neither the FPU/SSE nor the FS and GS bases** (it is built for
  `x86_64-unknown-none`, which has no SSE, and has no thread-local storage). So a mode switch
  saves and restores only the general registers: the program's FPU state and TLS base stay in
  the CPU while the server runs, and the server sets them through `state` when Linux semantics
  change them (`arch_prctl`, signal frames). This invariant is what makes the switch cheap;
  the build enforces the target, a debug check verifies FS/GS on return.

Expected cost of a forwarded system call: two kernel entries and two CR3 loads, roughly 3×
today's in-kernel null system call (~2000 cycles against ~720), against ~33000 cycles for
today's IPC round trip. `iobench` gets a `forwarded_null_syscall` line to hold the design to
this (target: p50 ≤ 2500 cycles).

### Faults, exceptions and asynchronous events

- **Page faults** in the restricted region that the kernel can resolve from the mapping (a
  present page of a memory object, copy-on-write, demand-zero) never reach the server. A
  missing page of a **paged object** is requested from the instance's **pager thread** (a
  thread of the server in a process of its own, `pager_wait`/`mo_supply`), and the faulting
  thread sleeps until it is supplied. The request cannot go back to the faulting thread's own
  server: the fault may come from the kernel copying from a mapping during a system call,
  where that server is the caller (the Zircon pager model). Faults without a mapping return
  from `restricted_enter` as `Fault`, and the server turns them into `SIGSEGV` or `SIGBUS`.
- **CPU exceptions** (`#DE`, `#UD`, `#BP`, ...) return as `Exception`; the server delivers the
  Linux signal. Signal frames are written by the server into the program's memory.
- **Kick**: another thread (or a timer) that needs this thread's attention — a signal, a stop,
  an exit — calls `thread_kick(thread)`. A thread running restricted code returns with `Kick`
  (an IPI ends its time in restricted mode at once); a thread waiting in the server wakes from
  its wait with `EINTR`. This is how `kill`, job control and interval timers reach a busy
  program.

### Blocking in the server

The server runs on the Linux thread, so it blocks like a kernel does: a `read` from an empty
pipe waits on a futex in the server's memory; the writer wakes it. The shared region is shared
memory in every Linux process, and the kernel's futex already keys shared mappings by object
and offset, so a futex in it works across processes. Timeouts are futex waits
with a deadline. `epoll`, `poll` and `select` become server code over the same primitive.

## The kernel's interface to the server

Objects are referenced by **handles** in a per-process table (the kernel's first capability
model; until now privilege was a flag on the process). The Linux server holds handles to its
processes, threads, memory objects and the services it uses.

| area | calls (sketch) |
|---|---|
| processes | `process_create() -> (process, space)`; `process_kill`; exit notification on a port |
| threads | `thread_create(process, state)`, `thread_kick`, `restricted_enter(state)` |
| memory objects | `mo_create(size)`, `mo_create_paged(size, key)` with `pager_wait` and `mo_supply` for the pager thread, `mo_read`/`mo_write`, `mo_clone_cow(mo)` (for `fork`, R8), `mo_physical` for DMA/MMIO (drivers) |
| mappings | `map(space, addr, mo, offset, len, prot, flags)`, `unmap`, `protect` in a process's restricted region (the server keeps the Linux VMAs; the kernel keeps page tables) |
| waiting | `futex_wait(addr, value, deadline)`, `futex_wake`, `clock_get` |
| IPC | today's services, and shared-memory rings for the I/O paths (separate design) |
| devices | interrupts, I/O ports, PCI functions, DMA areas (as today; IOMMU per `iommu.md`) |
| console | the framebuffer console and keyboard as a device the server's tty layer drives, held by one instance at a time (ADR 0004) |

What leaves the kernel over the migration: `process/syscall.rs`'s dispatch, `sys_*.rs`,
`signal.rs`, `epoll.rs`, `poll.rs`, `prctl.rs`, `exec.rs`, `loader.rs`, `elf.rs`, `clone.rs`
and `exit.rs` (as Linux semantics), `fs/` (VFS, tmpfs, page cache, remote filesystems, cpio),
`net.rs`, `drivers/tty.rs`, procfs's data source. What stays: `address_space.rs` (as memory
objects and mappings), `sched.rs`, `task.rs` (threads), `futex.rs`, `ipc.rs`, `irq.rs`,
`tlb.rs`, `memory/`, `interrupts/`, `smp.rs`, timers, time, drivers for console and keyboard
input, PCI and ACPI.

### The page cache and the I/O paths

The page cache moves with the VFS into the server. Each cached file is a memory object whose
pager is the server (a cached object, done in R6c.3): a page fault on a mapping of the file
comes to the pager thread, which has diskfs read the page by DMA straight into the object's
granted page (`GRANT_FILL`, `mo_filled`; no `mo_supply` copy). `read`/`write` copy between the
object and the program directly (one copy, as Linux). The server talks to diskfs and netd
through shared-memory rings with buffers granted from its memory objects (zero copy,
IOMMU-confined DMA); that design follows the principles of the I/O audit and is its own
document (`io-rings.md`).

## Migration

Each phase keeps the suite green, has its benchmark numbers, and is a series of commits.

1. **R1 — The mechanism, with pass-through.** Handles (minimal), PCIDs, the two roots and the
   shared region, `restricted_enter` and the mode switch. The Linux server is a loop that
   hands every system call back to the kernel's existing implementation
   (`legacy_syscall(state)`, which runs the old handler against the restricted context) and
   every fault and exception likewise. Every program from `/init` on runs in restricted mode.
   Measures the cost of the switch (`forwarded_null_syscall`).
2. **R2 — Memory objects and mappings** as kernel objects by handle: anonymous and paged
   objects with the instance's pager thread, mapping into a program's view (done; the
   copy-on-write clone follows with `fork` in R8).
3. **R3 — The server's runtime** (done): a heap in the shared region that grows (a kernel
   call maps more of the region for the instance) and locks that work across the tree's
   processes (futexes on the server's memory, keyed by instance and address, since that memory
   is pinned and outside any address space's areas). Records per process come with the first
   per-process Linux state the server owns (descriptors, R6), with the kernel's notice when a
   process ends.
4. **R4 — Memory semantics** (done): `mmap`, `munmap`, `mprotect`, `mremap`, `madvise`, `msync` and
   the `mlock` family as server code. The kernel keeps the page tables and areas
   (mechanism); the server validates and decides. `mo_map` grows to anonymous private memory
   (committed when writable, `MAP_NORESERVE`, demand-zero), placement (a hint, or a free
   range the kernel finds), `MAP_FIXED_NOREPLACE` and population, and a file mapping takes
   the file through a bridge from the kernel's descriptor table (`kfile_object(fd)`) while
   files are still the kernel's. `brk` follows with the process model (R8), which owns the
   break. The kernel's own `mmap` stays for its native servers until R9.
5. **R5 — Time and sleeping** (done, with the server's direct access to program memory: faults
   resolved as the program's, a registered fixup for EFAULT): the clocks, `nanosleep`, `clock_nanosleep`, `gettimeofday`,
   `times` over the kernel's clock and deadline waits (interruptible by signals, which are
   still the kernel's).
6. **R6 — Files.** Descriptors, the VFS, tmpfs and the initramfs, pipes, the page cache (as
   paged objects whose pager is the server), the remote filesystem client with rings to
   diskfs, `poll`/`select`/`epoll` (over a kernel primitive that waits for the server's events
   and the kernel's at once), the tty layer (ADR 0004): into the server. The largest phase.
   R6 goes in steps, each green, by kind of file. Until the last one, the descriptor *table*
   stays the kernel's, and a file the server implements is a **placeholder** in it (an open
   file that names the server's object): `dup`, `close`, `fcntl`'s descriptor flags, `fork`'s
   copy and `exec`'s close-on-exec then work unchanged, and `poll`/`epoll` see the readiness
   the server reports for it (`kfd_ready`). The server looks up a descriptor before it passes a
   call through (`kfd_lookup`) and handles the call itself if the descriptor is one of its
   files; when the last descriptor of a placeholder goes, the kernel tells the instance's
   service thread (the pager thread, whose wait becomes a wait for any event of the instance).
   - **R6a — Placeholders and pipes** (done): the mechanism above, and pipes as the first kind
     (blocking with interruptible futexes, `O_NONBLOCK`, end of file and `EPIPE`/`SIGPIPE`).
   - **R6b — eventfd** (done).
   - **R6c — The namespace**: the VFS, tmpfs from the initramfs (an object of the image the
     kernel keeps), the mounts of the filesystem servers (`/data`, `/proc`, `/sys`) with the
     server as their client, the page cache as the server's paged objects, every path call,
     `mmap` of the server's files, and `execve`, which resolves the program in the server's
     namespace and hands the kernel's loader its memory object (until R8 moves the loader).
     The kernel keeps a read-only view of the initramfs for what it starts at boot.
     In steps:
     - **c1** (done): `crates/vfs`, the pure parts (path arithmetic, the cpio format), tested
       on the host.
     - **c2a — Records** (done): the server's state per working-directory context (cwd,
       umask). Until R8 the kernel's clone decides who shares a working directory
       (`CLONE_FS`); each such context of the kernel's carries the server's record
       (`fs_record`), a clone that makes a new one gets a copy the server made before the call
       passed through, and the kernel reports a record whose context ended (`EVENT_RELEASE`).
     - **c2b — Path calls in the server** (done): resolution, the working directory and every call
       that takes a path move into the server, over a mount table whose filesystems are, at
       first, the kernel's tree, reached through handles on its inodes (lookup, create,
       unlink, rename, readlink, stat, open into a descriptor, exec). The same bridge served
       `/data` until c3 and still serves `/proc`, `/sys` and `/dev` until I/O rings step 5 and
       R6d.
     - **c2c — tmpfs in the server** (done): the server's own tmpfs, with files as file objects
       (read, write, `mmap` and exec without the kernel's VFS; ETXTBSY through holds the kernel
       reports when let go; a thread about to answer ETXTBSY first waits for the releases
       reported until then, `SYS_EVENT_RELEASES`). First mounted at `/tmp`, with the kernel's tree at the root;
       then (done) the root became the server's tmpfs, unpacked from the initramfs (an object
       of the boot image, its members file objects over its bytes), and the kernel's tree
       stays mounted for what it still serves (`/dev`, `/proc`, `/sys`; `/data` until c3). Each
       instance has its own copy (a process tree is a container); pages of a program two
       trees run are not shared between them.
     - **c3 — `/data` in the server** (done; I/O rings step 4, `io-rings.md`, ADR 0005): the
       server is diskfs's client over shared-memory rings with granted buffers
       (`servers/linux/src/datafs.rs`, `fsclient.rs`, `datafile.rs`), with its own page cache:
       one cached object of the kernel's per file (`SYS_MO_CREATE_CACHED`), filled and written
       back by DMA into and out of granted pages, write-back instead of write-through. The
       kernel's `/data` (its remote store, write-through, flusher, `O_DIRECT` path and diskfs's
       IPC protocol) is gone; the kernel only starts diskfs and, before it powers off, waits
       until the instances wrote their caches back. procfs over the rings follows (I/O rings
       step 5).
   - **R6d — The terminal** (ADR 0004): the console as a device of the server, the line
     discipline and job control's terminal side in the server.
   - **R6e — The descriptor table, `poll`, `select` and `epoll`** move with the sockets (R7),
     the last kind the kernel implements, over a kernel wait for the server's events and the
     netd's at once. Until then the descriptor's own requests pass through to the kernel,
     which holds the flags and the close-on-exec bit: `fcntl` and the generic ioctls
     `FIONBIO`, `FIOCLEX` and `FIONCLEX` (the server passes them through for its own files,
     too); they move with the table.
7. **R7 — Sockets** into the server, talking to netd over rings.
   - **R7a — `AF_UNIX`** (done; `servers/linux/src/unix.rs`, `sockcalls.rs`, `scm.rs`):
     stream, datagram and seqpacket sockets are files of the server, placeholders as pipes
     are; `socket` and `socketpair` of the family come to the server (the others pass
     through: `AF_INET` stays the kernel's and netd's until the rest of R7), and so does every
     socket call on one of its descriptors. Names are socket inodes of the server's tmpfs and
     of `/data` (ext2's socket type, `fsring`'s `KIND_SOCKET`), found by inode, or names of
     the instance's abstract namespace. Linux's semantics as `net/unix/af_unix.c` has them:
     messages charged to their sender until read (`SO_SNDBUF`, poll's quarter rule),
     datagram queues holding back senders that are not their peer, connections made at once
     and queued on the listener (backlog, `EAGAIN`), a closed end shutting the other down
     with `ECONNRESET` for unread data, `EPIPE` with `SIGPIPE` unless `MSG_NOSIGNAL`
     (`signal_thread`, 1102: signals are the kernel's until R8), `MSG_PEEK`, `MSG_WAITALL`,
     `MSG_TRUNC`, `MSG_CTRUNC`, autobind, `SO_PASSCRED`/`SCM_CREDENTIALS` and `SO_PEERCRED`
     (the ids from `thread_ids`, 1103), timeouts.

     **Passing descriptors** (`SCM_RIGHTS`) while the descriptor table is still the kernel's:
     a descriptor in flight is a handle on its open file description (`kfile_object`, 1029,
     which takes any descriptor: a file of the kernel's, a socket of netd's or a placeholder
     of the server's), kept by the message in the receiver's queue; the receiver gets a new
     descriptor for the same description (`kfd_install_file`, 1100: shared offset and status
     flags, close-on-exec with `MSG_CMSG_CLOEXEC`), and the handle goes. The handle keeps
     the description alive after the sender closed its descriptor, and a message dropped
     unread closes its handles (a placeholder's last reference gone is reported as its last
     descriptor closed). The kernel needs no notion of sockets for this. Sockets that only
     messages in flight keep (a socket in its own queue, a cycle of them) are found by a
     collector as Linux's `unix_gc`: a socket in flight whose description has no reference
     but its handles in flight (`kfile_info`, 1101) is a candidate, candidates referred to
     from outside the candidates' queues and what they refer to are reachable, the rest have
     their queues emptied. A call that uses one of the server's files keeps it referenced
     until it returns (`kfd_lookup` pins it, as Linux's `fdget`), so a socket a call works on
     is never a candidate, and another thread's close does not end a blocked receive or
     accept; a lock the collector takes exclusively (making, installing and letting go of
     descriptors in flight take it shared) keeps its view still while it runs. Handles of
     descriptors in flight are marked (`KFILE_INFLIGHT`): every descriptor of such a
     placeholder that goes (by close, exit or exec) and every call that pinned one ending
     queues `EVENT_INFLIGHT`, and the collector runs; it also runs before a sender with too
     many descriptors in flight (16 Ki per process) is refused with `ETOOMANYREFS`.
     `SCM_CREDENTIALS` may name only a process of the caller's tree (`thread_exists` with
     `THREAD_IN_INSTANCE`). When the descriptor table moves into the server (R6e), the
     handles become references in the server's own table and these calls go.
8. **R8 — Processes and signals**: pids, the process tree, `fork` (with the copy-on-write clone
   of memory objects), `exec`, `wait`, signals, job control, `/proc`'s data. The kernel's
   process model shrinks to processes and threads as containers. `thread_exists` (1090), with
   which the server checks the target of `sched_getscheduler`/`sched_getparam` today and which
   sees every task of the kernel (other instances' and the servers' threads too), goes then:
   the server answers from its own table, scoped to its instance.
9. **R9 — Remove the pass-through.** `legacy_syscall` and the kernel's Linux code go; the
   kernel implements no system call of Linux. Programs that are not Linux (the servers) keep
   the kernel's own system call interface.

The order changed after R2 (files were R3, memory semantics R6): every piece of Linux
semantics needs the server's runtime first, and memory semantics is the smallest piece that
takes Linux code out of the kernel, while files are the largest and touch almost everything
else.

## Decisions taken in the review

- One server instance per process tree, not one for all (ADR 0002).
- 46 bits of address space for Linux programs (ADR 0003).
- The tty layer runs in the Linux server; the kernel keeps the console as a device (ADR 0004).
