# The Linux server: system calls in restricted mode

Status: accepted; phase R1 implemented (the shared region uses PML4 slot 128, 512 GiB). Decisions: ADR 0001-0004, 0007 (terminals), 0008 (sockets), 0009 (the descriptor table), 0010 (processes and signals).

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
| memory objects | `mo_create(size)`, `mo_create_paged(size, key)` with `pager_wait` and `mo_supply` for the pager thread, `mo_read`/`mo_write` (`fork` clones a whole address space with `proc_create`, R8), `mo_physical` for DMA/MMIO (drivers) |
| mappings | `map(space, addr, mo, offset, len, prot, flags)`, `unmap`, `protect` in a process's restricted region (the server keeps the Linux VMAs; the kernel keeps page tables) |
| waiting | `futex_wait(addr, value, deadline)`, `futex_wake`, `clock_get`; `server_wait` for any of several words (poll, select, epoll; R6e) |
| descriptor tables | the server's, per thread as its process model shares them (R6e, R8); the kernel's own open files by handle (`inode_open`, `kfile_call`; R6e) |
| IPC | today's services, and shared-memory rings for the I/O paths (separate design) |
| devices | interrupts, I/O ports, PCI functions, DMA areas (as today; IOMMU per `iommu.md`) |
| console | the framebuffer console and keyboard as a raw device the server's tty layer drives, held by one instance at a time (ADR 0004; `console_read`, `console_write`, `console_info`, `EVENT_CONSOLE`; R6d) |

What leaves the kernel over the migration: `process/syscall.rs`'s dispatch, `sys_*.rs`,
`signal.rs`, `epoll.rs` and `poll.rs` (gone with R6e), `prctl.rs`, `exec.rs`, `loader.rs`, `elf.rs`, `clone.rs`
and `exit.rs` (as Linux semantics), `fs/` (VFS, tmpfs, page cache, remote filesystems, cpio),
`net.rs`, `drivers/tty.rs`, procfs's data source. What stays: `address_space.rs` (as memory
objects and mappings), `sched.rs`, `task.rs` (threads), `futex.rs`, `ipc.rs`, `irq.rs`,
`tlb.rs`, `memory/`, `interrupts/`, `smp.rs`, timers, time, drivers for console and keyboard
input, PCI and ACPI.

### Priority and the server's locks

The server runs on its programs' threads, so with their nice values, and its locks
(`servers/linux/src/sync.rs`, futexes on its memory) are held in preemptible user mode. A
low-priority thread preempted while it holds a lock would make every thread of the
instance that needs the lock wait as long as the low-priority thread waits for the CPU
(priority inversion: seconds for nice 19 next to a nice −20 loop). The server counts the
locks each thread holds in a word of the thread's State page
(`restricted::SERVER_LOCKS_OFFSET`), by plain loads and stores, since only the thread
writes it (a locked increment and decrement per lock cost a path lookup, with about fifty
locks, some 900 cycles); the scheduler, which knows each Linux thread's page,
gives a thread holding one the weight of nice −20 (its time with the lock counts at that
weight) and puts it at the front of its CPU's virtual time when it is preempted or wakes
holding one. A holder thus comes back as soon as the most favored program would, and a
program gains no more than that by its calls. A full priority inheritance (waiters lending
their weight to the owner) needs the owner's identity in every lock word and is not needed
while the boost bounds the wait.

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
   objects with the instance's pager thread, mapping into a program's view (done; `fork`'s
   copy-on-write clone of a whole address space came with R8).
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
   range the kernel finds), `MAP_FIXED_NOREPLACE` and population, and a file mapping took
   the file through a bridge from the kernel's descriptor table (`kfile_object(fd)`) while
   files were the kernel's (since R6e it maps the handle of the kernel's open file that the
   server's descriptor table holds). `brk` came with the process model (R8), which owns the
   break. The kernel's own `mmap` stays for its native servers until R9.
5. **R5 — Time and sleeping** (done, with the server's direct access to program memory: faults
   resolved as the program's, a registered fixup for EFAULT): the clocks, `nanosleep`, `clock_nanosleep`, `gettimeofday`,
   `times` over the kernel's clock and deadline waits (interruptible by signals, which are
   still the kernel's).
6. **R6 — Files.** Descriptors, the VFS, tmpfs and the initramfs, pipes, the page cache (as
   paged objects whose pager is the server), the remote filesystem client with rings to
   diskfs, `poll`/`select`/`epoll` (over a kernel primitive that waits for the server's events
   and the kernel's at once), the tty layer (ADR 0004): into the server. The largest phase.
   R6 goes in steps, each green, by kind of file. Until the last one (R6e), the descriptor
   *table* stayed the kernel's, and a file the server implements was a **placeholder** in it
   (an open file that named the server's object): `dup`, `close`, `fcntl`'s descriptor flags,
   `fork`'s copy and `exec`'s close-on-exec worked unchanged, and `poll`/`epoll` saw the
   readiness the server reported for it (`kfd_ready`). The server looked up a descriptor
   before it passed a call through (`kfd_lookup`) and handled the call itself if the
   descriptor was one of its files; when the last descriptor of a placeholder went, the kernel
   told the instance's service thread (the pager thread, whose wait became a wait for any
   event of the instance). R6e removed all of that.
   - **R6a — Placeholders and pipes** (done): the mechanism above, and pipes as the first kind
     (blocking with interruptible futexes, `O_NONBLOCK`, end of file and `EPIPE`/`SIGPIPE`).
   - **R6b — eventfd** (done).
   - **R6c — The namespace**: the VFS, tmpfs from the initramfs (an object of the image the
     kernel keeps), the mounts of the filesystem servers (`/data`, `/proc`, `/sys`) with the
     server as their client, the page cache as the server's paged objects, every path call,
     `mmap` of the server's files, and `execve`, which resolves the program in the server's
     namespace and handed the kernel's loader its memory object (until R8 moved the loader
     into the server).
     The kernel keeps a read-only view of the initramfs for what it starts at boot.
     In steps:
     - **c1** (done): `crates/vfs`, the pure parts (path arithmetic, the cpio format), tested
       on the host.
     - **c2a — Records** (done): the server's state per working-directory context (cwd,
       umask). Until R8 the kernel's clone decided who shares a working directory
       (`CLONE_FS`), and each such context of the kernel's carried the server's record
       (`fs_record`); since R8 the server's thread records hold them.
     - **c2b — Path calls in the server** (done): resolution, the working directory and every call
       that takes a path move into the server, over a mount table whose filesystems are, at
       first, the kernel's tree, reached through handles on its inodes (lookup, create,
       unlink, rename, readlink, stat, open into a descriptor, exec). The same bridge served
       `/data` until c3 and `/proc` and `/sys` until I/O rings step 5 ("/proc and /sys"
       below); it still serves `/dev` until the server makes its own.
     - **c2c — tmpfs in the server** (done): the server's own tmpfs, with files as file objects
       (read, write, `mmap` and exec without the kernel's VFS; ETXTBSY through holds the kernel
       reports when let go; a thread about to answer ETXTBSY first waits for the releases
       reported until then, `SYS_EVENT_RELEASES`). First mounted at `/tmp`, with the kernel's tree at the root;
       then (done) the root became the server's tmpfs, unpacked from the initramfs (an object
       of the boot image, its members file objects over its bytes), and the kernel's tree
       stays mounted for what it still serves (`/dev`; `/data` until c3, `/proc` and `/sys`
       until I/O rings step 5). Each
       instance has its own copy (a process tree is a container); pages of a program two
       trees run are not shared between them.
     - **c3 — `/data` in the server** (done; I/O rings step 4, `io-rings.md`, ADR 0005): the
       server is diskfs's client over shared-memory rings with granted buffers
       (`servers/linux/src/datafs.rs`, `fsclient.rs`, `datafile.rs`), with its own page cache:
       one cached object of the kernel's per file (`SYS_MO_CREATE_CACHED`), filled and written
       back by DMA into and out of granted pages, write-back instead of write-through. The
       kernel's `/data` (its remote store, write-through, flusher, `O_DIRECT` path and diskfs's
       IPC protocol) is gone; the kernel only starts diskfs and, before it powers off, waits
       until the instances wrote their caches back. procfs over the rings followed (I/O rings
       step 5, "/proc and /sys" below).
   - **R6d — The terminal** (done; ADR 0004, ADR 0007): the console as a device of the server, the
     line discipline and job control's terminal side in the server, and pseudo-terminals.
     See "The terminal" below.
   - **R6e — The descriptor table, `poll`, `select` and `epoll`** (done; ADR 0009): the
     table is the server's, per process, and the placeholders turn around: the kernel's files
     a program uses are open file descriptions the server holds by handle. See "The
     descriptor table" below.
7. **R7 — Sockets** into the server, talking to netd over rings.
   - **R7a — `AF_UNIX`** (done; `servers/linux/src/unix.rs`, `sockcalls.rs`, `scm.rs`):
     stream, datagram and seqpacket sockets are files of the server, as pipes
     are; `socket` and `socketpair` of the family come to the server (since R7b every socket
     call does), and so does every socket call on one of its descriptors. Names are socket inodes of the server's tmpfs and
     of `/data` (ext2's socket type, `fsring`'s `KIND_SOCKET`), found by inode, or names of
     the instance's abstract namespace. Linux's semantics as `net/unix/af_unix.c` has them:
     messages charged to their sender until read (`SO_SNDBUF`, poll's quarter rule),
     datagram queues holding back senders that are not their peer, connections made at once
     and queued on the listener (backlog, `EAGAIN`), a closed end shutting the other down
     with `ECONNRESET` for unread data, `EPIPE` with `SIGPIPE` unless `MSG_NOSIGNAL`
     (the server's own signals since R8), `MSG_PEEK`, `MSG_WAITALL`,
     `MSG_TRUNC`, `MSG_CTRUNC`, autobind, `SO_PASSCRED`/`SCM_CREDENTIALS` and `SO_PEERCRED`
     (the ids of the server's process table), timeouts.

     **Passing descriptors** (`SCM_RIGHTS`): a descriptor in flight is a reference to its
     open file description (`scm::Passed`; until R6e a handle on the kernel's open file,
     `kfile_object` with `KFILE_INFLIGHT`, installed by `kfd_install_file`), whatever the
     file is, kept by the message in the receiver's queue and counted in the description's
     `inflight`; the receiver gets a new descriptor for the same description (shared offset
     and status flags, close-on-exec with `MSG_CMSG_CLOEXEC`), and the reference goes. It
     keeps the description alive after the sender closed its descriptor, and a message
     dropped unread lets go of its references (a description whose last one that was
     closes). Sockets that only messages in flight keep (a socket in its own queue, a cycle
     of them) are found by a collector as Linux's `unix_gc`: a socket in flight whose
     description has no reference but those in flight (its strong count against
     `inflight`) is a candidate, candidates referred to from outside the candidates' queues
     and what they refer to are reachable, the rest have their queues emptied. A call that
     uses a description holds a reference until it returns (Linux's `fdget`), so a socket a
     call works on is never a candidate, and another thread's close does not end a blocked
     receive or accept; a lock the collector takes exclusively (making, installing and
     letting go of descriptors in flight take it shared) keeps its view still while it runs.
     When a reference to a description in flight goes (a descriptor closed, by close, exit or
     exec, or a call that used one ended), the collector is asked for after the reference is
     gone, so that it sees it gone; it runs on the instance's **worker** thread
     (`ROLE_WORKER`: a second thread of the pager's process that serves neither a program nor
     a page), which also closes the descriptor tables of processes that ended (R6e). Never on the pager: the collector waits for sockets' locks, and nothing may
     wait for the pager while it holds one; for the same reason no server lock the pager
     takes is held while program memory is copied (a fault there may need a page the pager
     brings): a receive copies with only its socket's (or pipe's) receive lock held. The
     collector also runs on a sender's thread before it is refused with `ETOOMANYREFS`: at
     most 16 Ki descriptors are in flight per user and in the instance, so forks gain
     nothing.
     `SCM_CREDENTIALS` may name only a process of the caller's tree (`thread_exists` with
     `THREAD_IN_INSTANCE`).
   - **Netlink** (`NETLINK_ROUTE`): the server's files too (`netlink.rs`, messages in
     `crates/netlink`), answered from netd's description of its interfaces (`netring`'s
     `Link` records, asked for over the instance's channel to netd since R7b).
   - **R7b — Internet sockets** (ADR 0008; `servers/linux/src/inet.rs`, `inetcalls.rs`,
     `netclient.rs`, the protocol `crates/netring`, netd's `servers/netd/src/service.rs`):
     `AF_INET` TCP, UDP and raw ICMP sockets are files of the server, as
     `AF_UNIX` ones are; every socket call comes to the server (`AF_INET6` and the families
     nobody implements are `EAFNOSUPPORT` there), and the kernel's socket layer and its IPC
     protocol to netd are gone: the kernel only starts netd (and restarts it, ADR 0006).

     **The channel.** Each instance has one channel to netd (service `net`), made at its
     first socket: 64 request slots, and a **shared area** beside the rings (an extension of
     `chan_create`: pages both ends map read and write for the protocol's own state, which
     no revoke takes from the service). The area holds a **control block** per socket (128
     bytes, at most 1024 sockets an instance) and two bitmaps with a doorbell word each.
     A control block has netd's cache line (an event counter `seq`, the state bits, the
     receive ring's `rx_tail`, the send ring's `tx_head`, the latest error with a count,
     the connections ready to accept, `rx_wait`) and the server's (`rx_head`, `tx_tail`,
     how many threads wait). Atomics there are safe for netd: the area stays mapped while it
     is attached, whatever the client does.

     **Data.** A socket's bytes live in a **byte ring** pair (receive and send, 64 KiB
     each) in the server's buffer pool: memory objects of 2 MiB (16 socket areas) mapped
     into the server's region and granted to the channel once, more of them as sockets
     come, the last empty one kept. The server copies between the program and the rings
     (one copy, no lock but the socket's receive or send lock); netd copies between the
     rings and smoltcp's socket buffers with the fault-surviving copy (`oxrt::copy`), so a
     revoked grant fails the socket, never netd. The protocol for a ring is the SPSC one of
     `crates/ring` on positions instead of slots: the producer writes bytes, then publishes
     its position (Release); the consumer reads the position (Acquire), copies, then
     publishes its own. TCP needs no request and no system call in the steady state:

     - *Sending*: the server appends to the send ring, publishes `tx_tail`, sets the
       socket's bit in the service bitmap and rings the submission doorbell (a futex wake
       only if netd sleeps). netd moves what fits into smoltcp's send buffer, publishes
       `tx_head` and wakes the socket's waiters (writers waiting for room).
     - *Receiving*: netd moves what smoltcp received into the receive ring as soon as there
       is room (it never waits for a request), publishes `rx_tail` and wakes the socket's
       waiters. The reader copies out and publishes `rx_head`; only if netd had data that
       did not fit (`rx_wait`, checked after a fence: Dekker's pattern) does it ring netd.
     - Datagrams (UDP, raw ICMP) arrive in the receive ring as records (16-byte header:
       length, source address and port; aligned to 16 bytes, wrapping); a datagram is sent
       by a request (`SEND`) from the start of the send half, so its errors (`EMSGSIZE`,
       `ENETUNREACH`, `EAGAIN` for a full smoltcp buffer) are the call's own, as on Linux.

     **Requests** are the rare operations: `SOCKET`, `BIND`, `LISTEN`, `CONNECT`, `ACCEPT`,
     `SEND` (datagrams), `SHUTDOWN`, `CLOSE`, `NAME`, `SETOPT`, `LINKS`, `FORGET`. netd
     answers every one **at once**: nothing waits in netd. A connect starts the handshake and
     completes; the outcome comes through the control block (`ESTABLISHED`, or `CLOSED`
     with `ECONNREFUSED` or `ETIMEDOUT`); an accept takes a connection the listener's count
     announced, or answers `EAGAIN`. So all waiting is the server's, on the socket's `seq`
     (a futex in the shared area, which netd and the server both advance and wake):
     interruptible by signals (`EINTR`, restarted under `SA_RESTART`), with `SO_RCVTIMEO`
     and `SO_SNDTIMEO` as deadlines, `O_NONBLOCK`/`MSG_DONTWAIT` as `EAGAIN` (`EINPROGRESS`
     for a connect), and a signal never leaves anything half done in netd (an interrupted
     connect goes on: `EALREADY` while it runs, as Linux).

     **Readiness** for `poll`, `select` and `epoll` is computed by the server from the
     control block as Linux's `tcp_poll` and `udp_poll` do (with `POLLRDHUP`, `SO_RCVLOWAT`,
     `POLLHUP` for a socket never connected, `POLLERR` with a pending error) and reported as
     for every file of the server (`files::ready`). A call that changes it reports it (a
     read that empties the ring); what netd changes reaches the instance's **net thread**
     (`ROLE_NET`, a third service thread of the pager's process): netd sets the socket's bit
     in the client bitmap and wakes the thread if it sleeps (one wake per netd round), the
     thread reports each marked socket's readiness, and an edge for new data or room
     (`EPOLLET`). A blocked call does not wait for the net thread: netd wakes it directly,
     and so it wakes a poll or select on the socket (they wait on the control block's word
     too, R6e).

     **Semantics** (Linux's, `man 7 tcp`, `udp`, `ip`, `socket`): `bind` checks the address
     is local (`EADDRNOTAVAIL`) and the port, and `listen` checks the port again (Linux's
     `inet_csk_bind_conflict`, `netring::tcp_port_conflict`): a socket on an overlapping
     address conflicts unless both have `SO_REUSEADDR` and it does not listen
     (`EADDRINUSE`), so of two sockets sharing a port only one may listen; port 0 takes an
     ephemeral port; connections inherit their listener's. `EPIPE` with `SIGPIPE`
     (unless `MSG_NOSIGNAL`) once the connection cannot send, after a pending error
     (`ECONNRESET`) is reported once; `MSG_PEEK`, `MSG_WAITALL`, `MSG_TRUNC` (datagrams),
     `SO_ERROR` (taking the pending error: a nonblocking connect's outcome), `shutdown`
     (`SHUT_WR` sends `FIN` after what is queued), `TCP_NODELAY` (Nagle's algorithm is on
     by default, as Linux), `SO_KEEPALIVE`, `IP_TTL`, `FIONREAD`, `SIOCOUTQ`, `SO_LINGER`
     with a zero time (close sends `RST`), and close with unread data sends `RST`.
     **Closing**: `close(2)` sends `CLOSE` before it returns, so the port is free then, as
     on Linux; what the pager learns (a process's exit closing its descriptors) it hands to
     the net thread, never waiting for netd itself. netd then copies what the send ring
     still holds into its own memory and sends it before its `FIN`, so data written before
     an exit reaches the peer even after the instance ended (the pager waits for the net
     thread's closes before it lets the instance go).

     **netd** serves every instance's channel from one TCP/IP stack: ports are global, and
     a channel names only its own sockets (its control blocks), so one instance can neither
     see nor touch another's. Port sharing never crosses instances where it could take
     traffic from one: another instance's bound or listening TCP socket conflicts whatever
     both opted in to (since the review: any port another instance holds), and a UDP
     port is shared by `SO_REUSEADDR` only within an instance (`netring::udp_port_conflict`;
     both rules are tested on the host). netd knows each channel's instance from the
     kernel's offer (`Offer::instance`, which no client can forge) and accounts per instance,
     whatever number of channels it opens (`netring::Budget`, charged before anything is
     allocated, given back to the instance charged, tested on the host): every resource
     (netd's socket memory: smoltcp's buffers and closing connections' leftovers; smoltcp
     sockets, connections in TIME-WAIT among them; orphans; half-open connections) has a
     limit in all and keeps a
     reserve for every instance with a channel and for every one that may still come (64
     channels), which others never cut into; beyond the
     reserves it goes to whoever asks first (one instance alone may use nearly all of it,
     n instances can each count on their reserve). `ENOBUFS` beyond (and a reset for a
     close whose leftovers do not fit); at most two channels an instance, and a channel
     without sockets and requests for 10 s gives its slot to another instance's when all
     64 are taken (its client makes a new channel when it wants one).
     **Memory follows use**, as Linux autotunes its buffers: a TCP socket has no buffers
     until it connects or a connection arrives (a listener's backlog of up to 128 costs
     only the sockets' places); then Linux's first sizes (64 KiB to receive, 16 KiB to
     send; given before the SYN-ACK goes, so it offers a window), which double up to 1 MiB
     while they limit the transfer (smoltcp, vendored, has patches to grow buffers and to
     announce the window scale of the largest). Under pressure (an instance with less than
     4 MiB of room left, as Linux's tcp_mem per instance) its connections start and stay small and idle ones give their
     send buffers back (a receive buffer never shrinks below the window it announced: the
     right edge never moves left; a segment beyond a buffer is dropped, never half kept),
     so hundreds of connections fit (nettest opens 600). Closed connections
     that finish in order (orphans, at most 2048) are reset after 60 s in FIN-WAIT-2
     (tcp_fin_timeout) or 100 s without progress (a zero window, a peer that stopped
     acknowledging); a connection attempt gives up after 127 s, unacknowledged data after
     924 s (Linux's SYN and data retries), reported as `ETIMEDOUT`. A connection in
     TIME-WAIT (only the side that closed first enters it) keeps its smoltcp socket, without
     buffers, until smoltcp's timer ends it: a retransmitted FIN is answered with an ACK, and
     its port and 4-tuple stay taken. It counts among its instance's sockets; at most 60 s
     in all however often the peer sends its FIN again (each restarts smoltcp's timer); and
     when an instance needs a socket its share or the whole has no room for, the oldest
     TIME-WAIT connection of that instance goes (never another's), and an instance without
     room skips TIME-WAIT, as Linux drops TIME-WAIT beyond
     tcp_max_tw_buckets, so TIME-WAIT never refuses service. Ports are never shared across instances (TCP: bound,
     listening, connected, closing or in TIME-WAIT; UDP: bound), and a connect never takes a
     live, closing or TIME-WAIT 4-tuple (`EADDRNOTAVAIL`). Raw ICMP sockets see the host's
     ICMP packets as on Linux, but no instance sees another's: netd gives each instance's
     echo requests identifiers of their own on the wire (`netring::EchoIds`, the checksum
     updated) and hands replies, and errors about the requests, to that instance alone with
     its identifier put back; errors about TCP or UDP packets go to the instance holding the
     quoted port; requests of other hosts and other messages to all (`netring::icmp_key`
     parses them, every length checked, tested on the host). A connected UDP or raw socket
     takes its peer's datagrams only. At most 256 connections are half-open (SYN-RECEIVED)
     at once, each with a 4 KiB receive buffer and nothing to send until the peer completes
     the handshake; the loopback and the card push back on smoltcp (no token while their
     queues are full) instead of dropping frames. Each round it takes requests (no more than the completion ring
     has room for), the service bitmaps, polls the card and smoltcp, moves data between
     smoltcp and the rings of the sockets that can move some, and publishes what changed;
     it sleeps (doorbells armed, `ipc_receive` with smoltcp's next deadline) after a spin
     without progress. When a channel's client goes, its sockets close (`FIN` after the data
     netd already holds); when netd dies, every socket of the old channel fails
     (`ECONNRESET`, `POLLERR`) and the next socket makes a new channel (netd started again).
8. **R8 — Processes and signals** (ADR 0010; see "Processes and signals" below): pids (a
   namespace per instance, its first process pid 1), the process tree, sessions and process
   groups, `fork`/`vfork`/`clone`/`clone3` (over a copy-on-write clone of the address space),
   `execve` with the ELF loader in the server, `exit`, `wait4`/`waitid`, signals with Linux's
   frames, job control, interval timers, `prctl`, the credentials, `brk`, and the records of
   `/proc`'s per-process part (`process` and `threads` in `procfs.rs`, see "/proc and /sys"). The kernel's process model shrinks to processes and threads as
   containers driven by a small set of calls. The transitional calls (`thread_exists`,
   `signal_thread`, `thread_ids`, `proc_ids`, `signal_group`, `signal_state`, `fs_record`,
   `exec_target`, `EVENT_SESSION_END`, `EVENT_RELEASE` of records) and the kernel's
   `kill_orphaned_pgrp` go: the server answers from its own tables, scoped to its instance.
9. **R9 — Remove the pass-through.** `legacy_syscall` and the kernel's Linux code go; the
   kernel implements no system call of Linux. Programs that are not Linux (the servers) keep
   the kernel's own system call interface.

The order changed after R2 (files were R3, memory semantics R6): every piece of Linux
semantics needs the server's runtime first, and memory semantics is the smallest piece that
takes Linux code out of the kernel, while files are the largest and touch almost everything
else.

## Processes and signals (R8)

Linux's process model is the server's: pids, the tree, sessions and process groups, signals,
`fork`, `exec`, `exit` and `wait`. The kernel keeps processes and threads as **containers**:
an address space, a descriptor table (until R6e), threads it schedules, their CPU time and
memory counts. Decisions in ADR 0010.

### What the kernel offers

| call | what it does |
|---|---|
| `proc_self() -> handle` | a handle on the calling thread's process (the tree's first process registers itself with it) |
| `proc_create(flags) -> handle` | a new, empty process of the instance: its address space a copy-on-write clone of the caller's (`PROC_FORK`) or the caller's own (`PROC_SHARE_VM`); its descriptor table a copy of the caller's or, with `PROC_SHARE_FILES`, the caller's (the table's part goes to the server with R6e) |
| `thread_create(process, state, flags, tls, ctid, cookie) -> key` | a thread in a new process (handle) or in the caller's (0): its program starts with the registers at `state`, the caller's FPU registers and FS base (or `tls`), `ctid` as its `CLONE_CHILD_CLEARTID` word; its server starts with `cookie` and its key |
| `thread_kick(key)` | the thread looks at its signals (below) |
| `thread_kill(key)` | the thread dies: its waits end, it exits at its next `restricted_enter` |
| `thread_exit(status, flags)` | the caller ends; with `EXIT_GROUP` its process (`exit_group`: the others are killed) |
| `exec_space(exe, name, len)` | the point of no return of `execve`: the caller's process gets a new, empty address space (attached to the instance, `exe`'s hold kept while it runs), the FPU state and FS base reset, `CLONE_CHILD_CLEARTID` done for the old one, close-on-exec descriptors closed (the table's part goes with R6e) |
| `proc_info(handle, out)`, `thread_info(key, out)` | CPU time, memory, state, start, last CPU, nice, the kernel's own end of a process it killed (out of memory, the monitor's `kill`) |
| `thread_nice(key, set, nice)`, `thread_affinity(key, set, mask)` | per thread; the server picks the threads (process groups, users) |
| `thread_cleartid(addr)` | the caller's `CLONE_CHILD_CLEARTID` word (`set_tid_address`) |
| `thread_name(key, name, len)` | the kernel's name of a thread (its monitor and messages) |
| `init_args(buf, cap)` | the command line the kernel started the tree with (for its first thread) |
| `vm_floor(addr)` | the lowest address a mapping without `MO_FIXED` is placed at (the break: `brk` is the server's) |

A thread's **key** is its thread area's number and a generation (`slot | gen << 32`): the
server names threads by it, and a key outlives no thread (a reused area gets a new
generation). Processes are handles in the instance's table.

**Kicks.** `thread_kick` sets the thread's kick flag (Linux's `TIF_SIGPENDING`) and gets it
into its server: a thread running its program returns from `restricted_enter` with
`REASON_KICK` at once (an IPI if it runs on another CPU), a thread in an interruptible wait (a
server futex with `FUTEX_INTERRUPTIBLE`, `sleep_until`, a pass-through call's wait in the
kernel) ends it with EINTR. The flag stays set until the thread enters restricted mode again:
`restricted_enter` with the flag set returns `REASON_KICK` without entering the program (and
clears it). So no signal is lost between the server's last look and the program running, and
every wait after a kick ends at once, as Linux's `signal_pending`. A thread the server or the
kernel kills is marked dying: every wait ends, also the uninterruptible ones (the server's
locks yield instead), and the thread exits at its next `restricted_enter`, the one point where
the server holds no lock of its own.

**Faults and exceptions** the kernel cannot resolve (no mapping, a protection violation, a
read beyond a file's end, `#DE`, `#UD`, `#BP`, `#GP`, `#XM`, ...) return from
`restricted_enter` with `REASON_FAULT` or `REASON_EXCEPTION`; `State` carries the vector, the
error code and the address, and the server raises the signal with Linux's `siginfo`
(`SEGV_MAPERR`/`SEGV_ACCERR`, `BUS_ADRERR`, `FPE_INTDIV`, `ILL_ILLOPN`, `TRAP_BRKPT`, ...).

**Thread exits** reach the service thread as `EVENT_THREAD_EXIT` (its key) once the thread is
gone: its references to the address space and the descriptor table are dropped, so a parent
that sees its child dead sees its pipes closed (Linux's `exit_files` before `exit_notify`).
The room for the event is reserved when the thread is created: it is never lost.

**The first process of a tree** (the monitor's `run`, autorun): the kernel makes the process
with an empty address space attached to a new instance and its first thread in `ROLE_INIT`,
and keeps the command line for it (`init_args`). The server makes it pid 1, a session and
process group leader with the console as its controlling terminal, and runs `execve` on it
itself. The kernel's ELF loader stays only for its native servers. The kernel still waits for
the tree's first process (the monitor), so that process alone is a zombie of the kernel's
too; the processes the server makes leave the kernel's table when their last thread ends.

### The server's tables (`process.rs`)

One lock (`PROCS`) guards the processes, threads and signal state of the instance (Linux's
`tasklist_lock` and `siglock` in one): it is held briefly and never across a copy to or from
program memory, so the service thread may take it (exit events) without waiting for a page.

- **Processes**: pid, parent, children, process group, session, the kernel's handle, threads,
  state (alive, zombie with its wait status), the group exit, the stop in progress and the
  report for `wait` (stopped, continued), `exit_signal`, `pdeath_signal`, dumpable,
  no_new_privs, child subreaper, name, command line and program path (for `/proc`), the CPU
  time and peak memory of reaped children, the interval timer, the break (shared by
  `CLONE_VM` processes), the signal actions (shared with `CLONE_SIGHAND`) and the pending
  process signals.
- **Threads**: tid, key, process, mask, pending thread signals, the alternate signal stack,
  the mask to restore after `sigsuspend`/`ppoll`/`pselect`/`epoll_pwait`, the signals a
  `sigtimedwait` waits for, a restart block (`restart_syscall`), the working-directory record
  (`CLONE_FS`), the descriptor table (`CLONE_FILES`), its name. A block in the thread's State
  page (`SERVER_LOCAL_OFFSET`, the kernel never reads it) holds its role, tid, pid, record and
  table for the hot paths, without the lock.
- **Pids** are allocated per instance from 1, up to `pid_max` (32768, then from 300 again,
  never one in use as a pid, process group or session). A thread's tid is a pid too.

### fork, vfork, clone, clone3

`proc_create` (unless `CLONE_THREAD`), then `thread_create` with the caller's registers
(`rax` 0, the new stack). `CLONE_PARENT_SETTID` is written before the call returns,
`CLONE_CHILD_SETTID` by the child's own server before its first instruction (it runs in the
child's address space), `CLONE_SETTLS` and `CLONE_CHILD_CLEARTID` by the kernel. `CLONE_FS`
shares the record, `CLONE_SIGHAND` the actions (also between processes, with `CLONE_VM`),
`CLONE_FILES` the descriptor table (else the child gets `FilesContext::fork`'s copy),
`CLONE_PARENT` makes the caller's parent the parent (with the caller's exit signal),
`CLONE_VFORK` makes the caller wait (killable only) until the child execs or exits.
`CLONE_PIDFD` and namespaces are EINVAL (no pidfds yet).

### execve

The server resolves the program (`execveat` too, with `AT_EMPTY_PATH`), checks it (a regular
file with an execute bit: EACCES; not open for writing: ETXTBSY), follows `#!` lines (four
deep, ELOOP), reads the ELF headers through its file object (ENOEXEC), copies the arguments
and environment (E2BIG beyond `MAX_ARG_STRLEN` or 2 MiB), and only then reaches the point of
no return: the process's other threads are killed and waited for (a thread that is not the
leader takes the leader's pid), `exec_space` swaps the address space, and the server maps
the segments (`ET_EXEC` where they say, `ET_DYN` at a fixed base, with its `PT_INTERP`
interpreter beside it), zeroes the tail of the last file page, maps the bss and a stack that
grows down (`MO_GROWSDOWN`), writes the strings (arguments, then environment, back to back),
`AT_RANDOM`, the platform name, the vectors and the auxiliary vector, and starts the program.
A failure after the point of no return kills the process with SIGSEGV, as on Linux. Handlers
go back to their default (ignored signals stay ignored), the alternate stack goes, the mask
and pending signals stay; a vfork parent goes on.

### exit and wait

`exit` ends the thread (`thread_exit`), `exit_group` and a fatal signal the process (every
other thread killed). When the service thread learns that a process's last thread is gone, the
process becomes a zombie: its children go to the nearest child subreaper or to pid 1 (none if
pid 1 is gone: they are reaped when they end), those with `PR_SET_PDEATHSIG` get their
signal, process groups it leaves orphaned with stopped members get SIGHUP and SIGCONT
(Linux's `kill_orphaned_pgrp`), a session leader's end dissociates its terminal, and the parent
gets its exit signal with `CLD_EXITED`/`CLD_KILLED` and is woken; a parent that ignores
SIGCHLD or set `SA_NOCLDWAIT` reaps it at once. `wait4` and `waitid` select by pid, process
group or any (`__WCLONE`, `__WALL`), report exits, stops (`WUNTRACED`/`WSTOPPED`) and
continues (`WCONTINUED`), with `WNOHANG` and `WNOWAIT`, the child's usage (CPU time and peak
memory, with its reaped children) and `siginfo`. The default action "core" ends the process
like "terminate" (no core files: `RLIMIT_CORE` is 0, so the status has no core flag).

### Signals

Standard signals pend once, real-time ones queue with their `siginfo` (at most 4096 queued in
the instance; `sigqueue` beyond is EAGAIN, `kill` keeps the signal without its data). A
process signal is taken by a thread that does not block it (the main thread first); the
server kicks it. SIGKILL starts the group exit at once; SIGCONT ends a stop at once (and drops
pending stop signals), a stop signal drops pending SIGCONT. Pid 1 of the instance gets only
the signals it has handlers for (Linux's `SIGNAL_UNKILLABLE` from inside its namespace).

**Delivery** happens when a thread goes back to its program after a kick or a call that
changed its mask: synchronous signals first (SIGSEGV, SIGBUS, SIGILL, SIGTRAP, SIGFPE, SIGSYS),
then the lowest number. Ignored ones are dropped; a default stop starts or joins the group
stop (dropped in an orphaned process group, but SIGSTOP); a default terminate or core ends the
process; a handler gets **Linux's x86-64 frame** (`rt_sigframe`: the return address from
`SA_RESTORER`, `ucontext` with `uc_flags`, `uc_stack`, `sigcontext` and `uc_sigmask`, the
`siginfo`, the FPU state in `fxsave` format 64-byte aligned above it), on the alternate stack
with `SA_ONSTACK` (`SS_AUTODISARM` honored), with `rdi` the signal, `rsi` the `siginfo`, `rdx`
the `ucontext`; the handler's mask adds `sa_mask` and the signal unless `SA_NODEFER`;
`SA_RESETHAND` resets the action. Every deliverable signal gets its frame before the program
runs, as Linux nests them. A frame that cannot be written kills with SIGSEGV. The server
saves and restores the program's FPU registers itself (`fxsave64`, `fxrstor64` with MXCSR's
reserved bits cleared): they stay in the CPU while it runs. `rt_sigreturn` restores the
registers (the kernel accepts only user addresses and harmless flags), the mask, the FPU
state and the alternate stack.

**Interrupted calls** restart as Linux's restart codes say, which the server's calls return
as Linux's do (`signal::ERESTARTSYS` and the others; they never reach the program), and a
plain EINTR of an interruptible wait counts as `ERESTARTSYS`: most calls restart if no
handler ran or the handler has `SA_RESTART`; `poll`'s and the sleeps' rest is kept in the
thread's restart block; `ppoll`, `select`, `pselect6`, `pause`, `rt_sigsuspend` and absolute
sleeps (`ERESTARTNOHAND`) restart only if no handler ran; `poll` and relative sleeps
(`ERESTART_RESTARTBLOCK`) go on to their first deadline through `restart_syscall` if no
handler ran; `epoll_wait`, `rt_sigtimedwait` and `sigreturn` never.

**Stops**: the thread that takes a stop signal starts the group stop and kicks the others;
each stops in the server (a futex of the process) until SIGCONT or SIGKILL; the last one
reports `CLD_STOPPED` to the parent (SIGCHLD unless `SA_NOCLDSTOP`, a report for `WUNTRACED`).
SIGCONT reports `CLD_CONTINUED`.

**Interval timers** (`alarm`, `setitimer`'s `ITIMER_REAL`) are the instance's **timer thread**
(`ROLE_TIMER`, a fourth service thread of the pager's process): it sleeps until the earliest
deadline and sends SIGALRM. A periodic timer reloads when its signal is taken (delivered or
returned by `sigtimedwait`), so a tiny interval costs one expiry per signal handled.

The server's terminals, pipes and sockets use the same tables: a background read gets SIGTTIN
for its group (`tty`), a write to a pipe or socket without a reader SIGPIPE for the thread.

### /proc

`/proc/<pid>` (also for a thread's tid, as Linux), `/proc/self` and `/proc/thread-self` are
the server's, from its tables and the kernel's `proc_info`/`thread_info`: `stat`, `statm`,
`status`, `cmdline`, `comm`, `exe`, `cwd`, `task/<tid>/...`, and oxidenix's `counters` (the
calls the server passed through for the process). The system's files (`stat`, `meminfo`,
`uptime`, `loadavg`, `cpuinfo`, `mounts`, `counters`, `sys/`) stay procfs's.

### Calls that name a pid

Every call that takes a pid or tid is the server's, which knows its namespace:
`kill`/`tkill`/`tgkill`/`rt_sigqueueinfo`/`rt_tgsigqueueinfo`, `wait4`/`waitid`, the
process group and session calls, `getpriority`/`setpriority`, `sched_getaffinity`/
`sched_setaffinity`, `sched_getscheduler`/`sched_getparam`, `prlimit64`, `getrusage`,
`times`, `clock_gettime` of another process's or thread's CPU clock, `capget`/`capset`.

## The terminal (R6d)

Before R6d the kernel had a line discipline for the console (`drivers/tty.rs`), answered the
terminal ioctls itself and kept one foreground process group. With R6d terminals are the
server's, as Linux's tty layer: the line discipline, termios, the controlling terminal and
the foreground group, hangups and pseudo-terminals. The kernel keeps the console as a raw
device. Decisions in ADR 0007.

### The kernel: the console as a device

- **Output**: bytes to the framebuffer console's VT100 interpreter and the serial mirror,
  as they are. A line feed is a line feed only (as on a VT: the cursor keeps its column);
  turning `\n` into `\r\n` is the terminal's `ONLCR`. The kernel's own messages (`printk`,
  the native servers' standard output) add their carriage returns themselves.
- **Input**: the keyboard's bytes (UTF-8, control characters, the Linux console's key
  sequences) and the console's answers to queries a program writes (a cursor position
  report, device attributes), in a 4 KiB ring. Nothing is interpreted.
- **One holder**: the kernel grants the device to the process tree it starts (at boot, by
  autorun, by the monitor's `run`) and takes it back when that tree's first process has ended
  (the monitor reads its commands from it then); an instance that ends lets it go. Only the
  holder reads input (`console_read`) and writes (`console_write`, EIO for others);
  `console_info` gives the size. Input wakes the holder's service thread with
  `EVENT_CONSOLE` (the keyboard interrupt sets a flag and wakes the service thread's
  channel; no lock of the instance is taken in interrupt context); losing the device is
  `EVENT_CONSOLE_LOST`.
- **Writes and echoes**: a write (at most 4 KiB per call) goes out whole in a writer's turn,
  a fair sleeping lock of the kernel's (first come first served, so a program flooding the
  console starves no other writer). The turn is never held beyond one call, nor across a
  copy: the kernel checks the holder, copies the call's bytes into its own memory, then
  takes a ticket and writes them. A signal does not end the wait for the turn (the output
  was processed for it); a dying thread's wait ends (EINTR, the ticket skipped), and so do
  the waits of an instance that loses the device (EIO: a change of the holder wakes them),
  so no instance can keep another's writers waiting. The service
  thread processes the keyboard's input and must never wait behind such a write (the
  instance's page faults wait for it): its echoes (`console_write` with `CONSOLE_ECHO`) go
  into a bounded queue (4 KiB, the rest dropped, as Linux's echo buffer) that the writer
  holding the turn drains between its pieces, or the echoing thread when nobody holds it.
  The signals of ^C, ^\ and ^Z are sent before the echo. A change of the holder empties the
  queue (an echo checks the holder under the queue's lock): one holder's echoes never reach
  the next one's screen. The console's answers to queries are
  taken in the lock hold that made them (each costs budget, so one hold's always fit): they
  become input for the holder's own writes only, the kernel's text gets none.
- **The monitor** (the kernel's fallback shell) edits its command line on the raw device
  while it holds it: echo, backspace, Ctrl+U, Enter. It is not Linux and has no termios.

### Process groups and sessions

The terminal asks the server's process table (R8; before it, the kernel's `proc_ids`,
`signal_group`, `signal_state` and `EVENT_SESSION_END`): a process's or a process group's
pid, group, session, and whether the group is orphaned; signals from the terminal to a
process group, a process, or a session's leader if it still leads it; whether the calling
thread blocks a signal or its process ignores it (SIGTTIN and SIGTTOU); and a session
leader's end. The process model sends SIGHUP and SIGCONT to a process group an exit leaves
orphaned with stopped members (POSIX, Linux's `kill_orphaned_pgrp`).

### The line discipline (`crates/ldisc`)

Linux's N_TTY as a pure library, tested on the host (`cargo test -p ldisc`): `struct termios`
and `struct termios2` in x86-64's layout with the defaults of Linux's `tty_std_termios`;
input mapping (`ISTRIP`, `INLCR`, `IGNCR`, `ICRNL`, `IUCLC`), `ISIG` with `NOFLSH`, flow
control (`IXON`, `IXANY`), canonical editing (`VERASE`, `VKILL` with `ECHOK`/`ECHOKE`,
`VWERASE`, `VREPRINT`, `VLNEXT`, `VEOF`, `VEOL`, `VEOL2`, `IUTF8` erasing whole characters,
tabs erased by column), the four cases of `VMIN`/`VTIME` (the waiting is the server's, the
rules are here), echo (`ECHO`, `ECHOE`, `ECHOK`, `ECHOKE`, `ECHONL`, `ECHOCTL`, `ECHOPRT`) and
output processing (`OPOST`, `ONLCR`, `OCRNL`, `ONOCR`, `ONLRET`, `OLCUC`, `XTABS`) with
Linux's column bookkeeping. Switching `ICANON` hands a half-typed line to a raw reader, and
raw input to a canonical one as a line, as Linux does. The buffer holds 4095 bytes: in
canonical mode a full line takes only its end (`IMAXBEL` rings), in noncanonical mode the
input waits (a pty master's write) or is dropped (the keyboard).

### Terminals in the server (`servers/linux/src/tty.rs`)

A terminal is a line discipline, its job control state (session, foreground group, window
size), its hangup generation, the readiness it reported to each of its open file
descriptions, and a driver: the console (`console.rs`) or a pseudo-terminal's slave
(`pty.rs`). Each open of a terminal is an open file description of the server's, as a
pipe's end is.

- **Reading** (`n_tty_read`'s rules): canonical reads end at a line's end (an `VEOF` at the
  start of a line reads 0); noncanonical reads follow `VMIN`/`VTIME` (with `VTIME` an
  inter-byte timer once a byte came, or the whole read's timeout with `VMIN` 0), the waits
  are interruptible server futex waits with a deadline; whether the read is canonical is
  asked at every pass, as Linux's. A read takes the read turn for the whole call (Linux's
  `atomic_read_lock`: a line or a `VMIN` batch is never split between readers); turns are
  interruptible waits on the turn's own counter (woken only when the turn passes on), not
  server locks, since they are held
  across waits for input, and first come first served (tickets; one given up by a signal is
  skipped), so a caller that comes back at once queues behind the others instead of barging
  in before the woken waiter runs. Bytes are peeked under the terminal's lock, copied to the program
  with only the reader lock (`rlock`) held, and consumed after (a flush in between is seen by
  an epoch). That lock is safe across the copy, where a fault may wait for the pager: only
  programs' reads and settings changes take it, never the pager.
- **Writing**: a write takes the write turn for the whole call (Linux's
  `atomic_write_lock`), so its output processing and the device's write keep their order;
  output processing happens under the terminal's lock into the server's memory, a pty's
  output goes to its master's buffer under it, the console's to the device after it. No lock
  of `sync` is held across the console's write (a flooding writer would get a lock holder's
  priority for the whole write and starve its CPU's other threads). As on Linux, signals
  end a write in its wait for the write turn and between its chunks, before a chunk is
  processed (`n_tty_write` checks `signal_pending` each pass: what went, or EINTR,
  restarted, if nothing), never in the device's write after processing (no signal ends its
  wait for the kernel's turn): output processed is output that goes out, so the column
  bookkeeping moves once and echoes never see it half done. Output stopped by
  `VSTOP` (or `tcflow`) waits for `VSTART`. Echoes take neither turn.
- **Job control** (Linux's `tty_check_change`): a process of a background group reading its
  controlling terminal gets SIGTTIN for its group and the call restarts after it (EIO if it
  ignores or blocks SIGTTIN or its group is orphaned); writing with `TOSTOP`, and
  `tcsetattr`, `tcflush`, `tcflow`, `TIOCSPGRP`, `tcsendbreak` from the background get
  SIGTTOU likewise (allowed if it ignores or blocks SIGTTOU, EIO if orphaned). `VINTR`,
  `VQUIT` and `VSUSP` signal the foreground group (flushing unless `NOFLSH`), `TIOCSWINSZ`
  sends SIGWINCH when the size changes.
- **The controlling terminal** belongs to a session: the terminal records the session it
  controls, and a process's controlling terminal is the one whose session is the process's.
  A session leader without one gets one by `TIOCSCTTY` or by opening a terminal that has
  none (not `/dev/console`, not with `O_NOCTTY`, as Linux); `TIOCNOTTY` by the leader and the
  leader's exit (`tty::session_ended`, from the process model) dissociate it as Linux's
  `disassociate_ctty` does (the
  console is hung up, a pty's foreground group gets SIGHUP). The instance's first process is
  a session leader and starts with the console as its controlling terminal, as busybox's
  `cttyhack` would make it (there is no getty). `TIOCNOTTY` by a process that is not its
  session's leader takes effect for the whole session only at its leader (a controlling
  terminal is the session's, kept by the terminal, not per process).
- **Hangup** (Linux's `__tty_hangup`): the terminal's generation advances; its open file
  descriptions from before read 0, fail writes and ioctls with EIO (`TIOCSPGRP`: ENOTTY) and
  poll as readable, writable, error and hangup; the session leader gets SIGHUP and SIGCONT,
  and on a session's end the foreground group SIGHUP. Then the terminal controls no session.
  The console is hung up when its session ends or the instance loses the device (after which
  opening it gives ENXIO); a pty's slave when its master closes.
- **Devices by number**: a character device node names its driver by its device number, as on
  Linux: (5,0) `/dev/tty` (the caller's controlling terminal, ENXIO without one), (5,1)
  `/dev/console`, (5,2) `/dev/ptmx`, (136,n) `/dev/pts/n` are the server's, whatever
  filesystem holds the node; (1,3) and (1,5) are null and zero: the kernel's for its own
  nodes, the server's (`devices.rs`) for nodes of its filesystems (zero maps anonymous
  memory, EACCES as Linux's for a descriptor not open for reading or a shared writable
  mapping of one not open for writing, read-only for good then; reads fill the whole count
  unless a signal comes; counts and buffers checked as Linux's `rw_verify_area` and
  `import_iovec`); any other number has no driver (ENXIO). `O_DIRECTORY` is ENOTDIR. The
  server's terminal, null and zero descriptors keep the node they were opened by (`Origin`):
  fstat is the node's, live, and fchmod, fchown and futimens change it. The kernel's `/dev`
  gives its nodes their numbers (`st_rdev`).
- **O_PATH** (`pathfile.rs`, any node of the namespace): the descriptor names the node and
  opens nothing, no driver, no file, as Linux's: only O_DIRECTORY, O_NOFOLLOW and O_CLOEXEC
  are kept (O_CREAT creates nothing, the access mode is ignored), O_NOFOLLOW names a symlink
  itself; fstat and fstatfs describe the node (live), it serves as the directory of *at calls
  and with AT_EMPTY_PATH as their target, fchdir takes a directory; reads, writes, ioctls,
  mmap, getdents and fchmod, fchown, futimens are EBADF. Its description's status flags
  carry O_PATH: F_GETFL shows it, only dup, close and fcntl's F_DUPFD, F_GETFD, F_SETFD and
  F_GETFL take it of the descriptor table's calls (Linux's fdget_raw), and SCM_RIGHTS passes
  it; poll gives POLLNVAL, select never finds it ready, epoll, ioctl and F_SETFL give EBADF; inotify and socket calls
  on one are EBADF. As a directory of *at calls it must be a
  directory (ENOTDIR: an O_NOFOLLOW symlink's path is not followed); `readlinkat` with an
  empty path reads the symlink it names. It keeps its node (a /data inode unlinked meanwhile
  goes with its blocks when the last descriptor closes, as an open file's). Opening
  `/proc/self/fd/N` of one opens its node for real (the magic links, "/proc and /sys"
  below), as musl's `fchmod` and `fexecve` of an `O_PATH` descriptor rely on. Not done yet:
  `execveat` with `AT_EMPTY_PATH`.
- **ioctls**: `TCGETS`/`TCSETS`/`TCSETSW`/`TCSETSF` and the `termios2` forms, `TCSBRK`,
  `TCSBRKP`, `TCXONC`, `TCFLSH`, `TIOCGWINSZ`/`TIOCSWINSZ`, `TIOCGPGRP`/`TIOCSPGRP`,
  `TIOCGSID`, `TIOCSCTTY`, `TIOCNOTTY`, `TIOCSTI` (everyone is root), `FIONREAD`,
  `TIOCOUTQ`, `TIOCEXCL`/`TIOCNXCL`/`TIOCGEXCL`, `TIOCGETD`/`TIOCSETD` (N_TTY only),
  `TIOCVHANGUP`; on a pty master also `TIOCGPTN`, `TIOCSPTLCK`/`TIOCGPTLCK`, `TIOCGPTPEER`
  and `TIOCSIG`. Not done: packet mode (`TIOCPKT`), the VT and keyboard ioctls of the Linux
  console (`KDGKBTYPE`, `VT_*`), modem lines (ENOTTY, as for a pty).

### Pseudo-terminals (`servers/linux/src/pty.rs`)

Opening `/dev/ptmx` makes a pair: the master is a file of the server, the slave a terminal
whose driver hands its output to the master. Its node `/dev/pts/n` is in the server's devpts,
a tmpfs mounted at `/dev/pts` whose names only the server makes and removes (with
`/dev/pts/ptmx`, mode 000, as Linux's): from the master's open until its close. The slave is
locked until `TIOCSPTLCK` unlocks it (`unlockpt`; opening it before is EIO). A master write is
the slave's input (in noncanonical mode it waits while the slave's buffer is full); the
slave's output goes to the master's 64 KiB buffer (its writers wait for room; echoes may
fill it 4 KiB further, the rest is dropped, so a master that never reads cannot grow it;
`TIOCSTI` on a full master drops; `TIOCSIG` takes SIGINT, SIGQUIT and SIGTSTP only). The master
reads EIO once the slave's last descriptor went (after what was left), and polls with
POLLHUP; the master's last close hangs the slave up and removes its node. The master's
termios, window size and process group ioctls act on the slave, as on Linux.

### What is tested where

`userspace/ttytest.c` drives terminals through ptys, which is everything the server's tty
layer does: termios round trips, canonical and raw reads, `VMIN`/`VTIME`, echo and editing,
output processing, ^C/^Z/^\ reaching the foreground group, SIGTTIN and SIGTTOU for a
background group, the controlling terminal, hangups, window sizes and SIGWINCH, poll, the
turns of readers, bounded echoes, orphaned stopped jobs. An echo through the console's
`TIOCSTI` (the keyboard's path) while another process floods the console checks that echoes
never wait for a writer. The console's own input is the keyboard's (QEMU's serial port is
output only), so the console
driver's input path is checked interactively (bash, busybox, htop); its output path carries
the whole test suite's output.

## The descriptor table (R6e)

Before R6e the descriptor table was the kernel's: the server's files were placeholders in it,
and poll, select and epoll were the kernel's, over the readiness the server reported. With
R6e the table, poll, select and epoll are the server's (`fdtable.rs`, `poll.rs`, `epoll.rs`);
the kernel keeps descriptor tables for its native servers only. Decisions in ADR 0009.

### Tables, descriptions and references

- **An open file description** (`files::Description`) is a file of the server's (`File`: a
  pipe end, a socket, an open file of tmpfs or /data, a terminal, an epoll instance, ...) or
  one of the kernel's it holds by handle (below), with its status flags (the access mode,
  O_APPEND, O_NONBLOCK, ...; O_PATH for one that names a node only), its watch list
  (readiness, below) and its references in flight. Descriptors refer to it
  (`files::FileRef`), and so do a call that uses it (Linux's fdget: another thread's close
  never takes a description from under a call) and a descriptor in flight. With its last
  reference the file closes (Linux's fput), on the thread that let go of it: the closing
  thread for close(2), the execve's thread for close-on-exec descriptors, the worker when a
  process's table ends and for a message the collector dropped (an internet socket's close then goes through the net
  thread, which may wait for netd).
- **The table** (`fdtable::FilesContext`) holds the descriptors (a description and the
  close-on-exec bit), the lowest free slot, and RLIMIT_NOFILE (4096 by default, at most
  2^20, Linux's fs.nr_open). Its calls: close, close_range (with `CLOSE_RANGE_CLOEXEC` and
  `CLOSE_RANGE_UNSHARE`), dup, dup2, dup3, fcntl's `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`,
  `F_SETFD`, `F_GETFL` and `F_SETFL` (O_APPEND, O_NONBLOCK; O_DIRECT, O_NOATIME and O_ASYNC are ignored), the ioctls `FIONBIO`, `FIOCLEX`
  and `FIONCLEX`, and RLIMIT_NOFILE of prlimit64 (any process's), getrlimit and setrlimit
  (the limit is kept with the table, so processes sharing one with `CLONE_FILES` but not
  `CLONE_THREAD` share it too, where Linux keeps one per process). No lock of a table is held
  while program memory is copied or a description is let go of.
- **Which threads share a table** is the process model's (R8; until then the kernel's clone
  decided, with the server's record of each table in the kernel's, `SYS_FILES_RECORD`): each
  thread record holds its table (`process::Thread::files`), `CLONE_FILES` shares it, and a
  clone without (fork, vfork) gets `FilesContext::fork`'s copy before the child exists. An
  execve makes the new program's table after the point of no return (the old one's
  descriptors without the close-on-exec ones, `FilesContext::for_exec`) and lets the old
  table go on that same thread before the new program runs, so close-on-exec descriptors are
  closed when it starts, as on Linux (a pipe's reader sees its end, a socket's port is free).
  An exiting thread lets its table go itself (Linux's exit_files, before its parent learns of
  the end); a thread the kernel ended without the server (killed while in its program) still
  has its table when the service thread learns of its end, and the worker lets that one go
  (closing a socket takes its locks, which the service thread must never wait for); the
  service thread waits for the worker before the instance ends. A thread finds its own table
  in its local block (`local::Local::files`): a descriptor's lookup is a lock and a
  reference count, no kernel call.

### The kernel's files

What remains of the kernel's files for a Linux program are the inodes of the kernel's tree
(its `/dev`: the nodes null and zero, the directory; `/proc` and `/sys` are the server's since
I/O rings step 5): the inverse of the placeholders. `inode_open` returns a handle on the kernel's open
file description (no descriptor), which the server's description holds (`kfile.rs`); its calls
are the kernel's with the program's buffers (`kfile_call`: read, write, the positional and
vectored forms, lseek, getdents64, ioctl, fsync, ftruncate; fstat and fstatfs into the
server's memory), mmap maps the handle, and *at calls and fchdir reach its inode
(`kfile_inode`). The description's status flags are the server's; the kernel's file gets
O_APPEND and O_NONBLOCK too, for its own operations. These files are always ready (as
Linux's regular files: epoll refuses them with EPERM), so poll, select and epoll need nothing
from the kernel for them.

### Readiness and waiting

- **Watches**: each description has a watch list (`poll::Watch`, Linux's wait queue for
  poll), which pollers and epoll interests subscribe to. Every file reports each change of
  its readiness, and each event that is an edge for EPOLLET (new data, room), under its own
  lock (`files::ready`, by the description's id, as it reported to the kernel before): the
  report wakes the subscribed pollers and queues the subscribed epoll interests. Reports
  find the watch in a sharded table, and not at all while nothing in the instance is
  subscribed (a pipe's report costs a load then). Readiness itself is asked of each file
  when it is checked (`Description::poll_mask`, Linux's poll methods), never taken from the
  reports.
- **poll, ppoll, select, pselect6** subscribe to every description they look at, check, and
  sleep on their own word (which the reports advance and wake) until a report, the deadline
  or a signal; on an internet socket they also wait on the control block's word that netd
  wakes, so they do not wait for the net thread to pass netd's change on. The kernel's
  primitive is one call for any of several words (`server_wait`: up to 64 words of the
  server's memory or of an object mapped there, and a deadline). ppoll, pselect6 and
  epoll_pwait wait with their temporary signal mask (`signal::with_mask`, with the saved-mask
  semantics of sigsuspend: a call that does not end with EINTR, an epoll_pwait that returns
  events after all, puts the caller's mask back without delivering what only the temporary
  one let through, Linux's restore_saved_sigmask_unless). A poller announces that it sleeps, so a report makes the
  kernel call that wakes it only then, and an epoll instance reports its own readiness only
  while something watches it; a description that is always ready has no watch at all.
  Whether a file is always ready is decided per kind of file (`File::always_ready`: tmpfs
  and /data files, null and zero, the kernel's files, /proc and /sys); a file of such a kind
  with readiness of its own (a pollable /proc file, as Linux's `/proc/self/mounts`) would need
  it per node, with a watch.
- **Signals**: interrupted with nothing ready, poll answers `ERESTART_RESTARTBLOCK`
  (restarted as restart_syscall, which goes on with the deadline kept in the thread's restart
  block, unless a handler ran: then EINTR), ppoll, select and pselect6 `ERESTARTNOHAND`
  (restarted unless a handler ran, with the time left, which they write back first, Linux's
  poll_select_finish), epoll_wait EINTR. The server's signal delivery maps these codes
  (`signal::ERESTARTSYS` and the others), as Linux's does.

### epoll

An instance (`epoll.rs`) is a description with an interest list, keyed by (descriptor
number, description) as Linux's, and a ready list. An interest is subscribed to its
description's watch: a report queues it (if it asks for the events reported) and wakes one
waiter of the instance, and reports the instance itself (to a poll on it, and to the
instances that watch it: nesting, at most 4 deep and without cycles, ELOOP). epoll_wait takes
interests off the ready list and asks each file for its readiness now: a level-triggered one
that is still ready goes back to the end of the list after delivery (so a full `maxevents`
rotates through them), an edge-triggered one waits for the next report, a one-shot one is
disabled until `EPOLL_CTL_MOD`. `EPOLLEXCLUSIVE` interests in one file wake their instances
one at a time (up to the first that had a waiter), as Linux's exclusive wakeup. A waiter that
leaves without taking the events (a signal, the deadline) passes the wake on. Interests
belong to the description: they go when it goes (its last reference), whatever descriptor
numbers still name it, and they hold no reference to it. No lock of an instance is held while
events are copied to the program.

### What went

The placeholders (`ServerFile`, `kfd_install`, `kfd_lookup` and its pins, `kfd_ready`,
`kfd_close`, `kfd_read`, `kfd_write`, `kfd_stat`, `kfd_inode`), the bridges for descriptors in
flight (`kfile_object`, `KFILE_INFLIGHT`, `kfd_install_file`, `kfile_info`, `EVENT_INFLIGHT`),
`EVENT_CLOSED` and `legacy_syscall`'s list of closed files, and the kernel's epoll, poll and
select with the wait queues' callbacks. Calls on descriptors no longer pass through to the
kernel; the kernel's descriptor tables, open files, pipes and eventfds stay for its native
servers until R9.

## /proc and /sys (I/O rings step 5)

`/proc` and `/sys` are mounts of the server's namespace (`namespace.rs`: `Fs::Proc`,
`Fs::Sys`; `procfs.rs`), made of two sources:

- **procfs's files**, system-wide: `/proc/{stat,meminfo,loadavg,uptime,cpuinfo,version,
  filesystems,counters}`, `/proc/sys/kernel/*` and all of `/sys`. procfs is a service of the
  file protocol over the I/O rings (one channel per instance, `fsclient`; the protocol's
  side in `docs/design/io-rings.md`, "procfs over the rings"): the server looks names up,
  stats and lists with requests, and reads a file's contents, made by procfs at the moment,
  through its scratch grant. Nothing of it is cached; a procfs that died is started again
  for a new channel and serves the same inodes.
- **The server's own part**, each process's: `/proc/<pid>` with `stat`, `statm`, `status`,
  `cmdline`, `comm`, `exe`, `counters` (oxidenix's), `mounts`, `task/<tid>/{stat,statm,
  status,cmdline,comm}`, `fd/<n>`, `cwd` and `root`; `/proc/self`, `/proc/thread-self` and
  `/proc/mounts` (a link to `self/mounts`), merged into procfs's root listing. The server
  knows its mounts, its descriptors (any process's table, R6e) and its processes (R8:
  `process::query` answers `procproto`'s records from the server's process table, with the
  kernel's accounts of CPU time and memory, `proc_info` and `thread_info` of a process's
  handle). `/proc` shows the instance's own processes only, as a pid namespace would: its
  pids are the instance's own, and the kernel's `proc_info` answers only for the handles of
  the instance's processes; another tree's processes, the kernel's servers and the kernel
  itself are not listed. procfs, which serves every
  instance, gets only the system-wide record from the kernel (`proc_query`: `QUERY_SYSTEM`,
  EPERM for the per-process ones), so it cannot be made a deputy for one instance's view of
  another's; system-wide figures (`/proc/stat`, `loadavg`, `meminfo`, `counters`) are
  aggregates, as in a container on Linux. The self-tests measure a server's CPU time with a
  test-mode call instead (`TEST_SERVER_TICKS`). The text formats are `procproto::render`'s,
  shared with procfs and tested on the host.

**Rules** (Linux's): nothing can be created (`open` with `O_CREAT`: EACCES; `mkdir`,
`symlink`: EPERM, EEXIST for a name that exists), removed, renamed (EPERM; EXDEV across
`/proc` and `/sys`) or chmodded (EPERM); files open read-only (for writing: EACCES; directories
EISDIR), nothing executes (EACCES), `statfs` says `PROC_SUPER_MAGIC` and `SYSFS_MAGIC`; times
set with `utimensat` are kept by the server per device and inode, as for the kernel's `/dev`
(`namespace::set_pseudo_times`). An open file (`procfile.rs`) keeps a snapshot of its
contents as Linux's `seq_file` does: a read from offset 0 makes them anew, reads further on
continue in what was kept; `lseek` from the start or the position (`SEEK_END`: EINVAL); a
directory's entries are taken at a read from the start. Copies to the program are made with
no lock held but the description's offset, which the pager never takes.

**Magic links.** `/proc/<pid>/fd/<n>` reads as the path the descriptor was opened by (with
" (deleted)" for a file unlinked since), `pipe:[ino]`, `socket:[ino]` or
`anon_inode:[eventfd]`, `anon_inode:inotify`, `anon_inode:[eventpoll]`. Following it does
not read it: resolution goes to the open file itself (`procfs::follow`, Linux's
`proc_fd_link`), as the last name its node (a tmpfs or `/data` inode, also unlinked; a
terminal's, null's or an `O_PATH` descriptor's node, a file of the kernel's `/dev`), with
names after it through the path the file was opened by (a limit of the namespace, which
resolves by path: Linux goes on from the directory itself, so a directory renamed or
removed since its descriptor was opened differs; the descriptor's own node is reached only
as the last name). A resolved symlink is opened only with `O_PATH` (ELOOP otherwise: an
`O_PATH|O_NOFOLLOW` descriptor's symlink reached through `/proc/self/fd/N`). A file without a node (a pipe, a
socket, an eventfd, inotify, epoll) is `ProcNode::Open`, which keeps nothing of the file
alive that its last close should end (an `O_PATH` descriptor of it may outlive every real
one): a pipe as the pipe itself, without an end, anything else as its status when it was
reached. Opening it opens the file again where Linux does: a pipe gets a new end
reading, writing or both, as `fifo_open` of a pipe, its readiness reported once its
description is installed; bash's process substitution through `/dev/fd/N` uses it),
everything else is ENXIO. `/dev/fd`, `/dev/stdin`, `/dev/stdout` and
`/dev/stderr` are links into `/proc/self/fd` (in the kernel's `/dev`, as udev makes them).
Until the process model is the server's, only a process's own
`fd`, `cwd` and `root` are shown: another process's are EACCES (Linux's answer for another
user's), its `fd` directory unopenable (mode 0500). "Own" is the caller's process (any of
its threads' ids names it), and what `fd` lists is the calling thread's descriptor table
(the server's, R6e): the process's, unless a thread made its own with `clone` without
`CLONE_FILES` (then `/proc/<pid>/fd` shows the caller's table, not the main thread's, as
`/proc/thread-self/fd` would on Linux). A child after `fork` sees its own copy. R8 (the
process model) makes this exact. `exe` is an ordinary link to the
program's path.

**The kernel** keeps no part of `/proc`: its static `/proc` of the early boot, its mount
table, its `RemoteFs` (IPC requests to procfs in `fsproto`) and its procfs mounts are gone;
procfs only gets started (and restarted on the next channel, ADR 0006).

## Decisions taken in the review

- One server instance per process tree, not one for all (ADR 0002).
- 46 bits of address space for Linux programs (ADR 0003).
- The tty layer runs in the Linux server; the kernel keeps the console as a device (ADR 0004).
- Terminals: the console as a granted raw device, devices named by number, the controlling
  terminal per session, pseudo-terminals in the server (ADR 0007).
- Internet sockets keep their state in control blocks in the channel's shared area, their data
  in byte rings in granted memory, and netd answers every request at once (ADR 0008).
- Processes and signals: the kernel keeps processes and threads as containers driven by kicks,
  kills and exit events; the server builds Linux's signal frames itself, `fork` clones the
  address space in one call, the ELF loader is the server's, and a tree's first process
  starts in the server (ADR 0010).
- The descriptor table is the server's, shared as its process model decides (R8; until then
  the kernel's clone, with records), the kernel's files are held by handle, readiness goes
  through the server's watch lists over one kernel wait for any of several words (ADR 0009).
