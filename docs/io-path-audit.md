# I/O path audit

State of commit `34bdbc5` (2026-10-07), before any I/O refactoring. The counts come from
reading the code path by path; the instrumented counters of the benchmark step (see
`docs/benchmarks/`) are to confirm them.

## How a Linux system call reaches a server

oxidenix implements the Linux system call ABI **in the kernel**: `syscall` enters
`process/syscall.rs`, which dispatches to the handlers (`sys_file.rs`, `sys_net.rs`, ...).
Processes, memory, signals, futexes, epoll, pipes, the VFS, tmpfs and the page cache are
handled there entirely. Only operations that need a service become messages to a user-space
server, with the kernel as the IPC client on behalf of the calling thread:

| Service | Server | Protocol |
|---|---|---|
| files on `/data` (metadata always, file data on a page cache miss or write) | diskfs (ext2 + virtio-blk) | `fsproto` |
| `/proc`, `/sys` | procfs | `fsproto` |
| sockets | netd (smoltcp + virtio-net) | `netproto` |

### The IPC mechanism (`kernel/src/process/ipc.rs`)

One synchronous call (`ipc::call`) is:

1. **Client (kernel, in the calling thread)**: encode the request into a fresh `Vec`
   (allocation + copy of the payload); take the global `IPC` spinlock; insert a `Request`
   into a `BTreeMap` (allocation) and the service's `VecDeque`; wake the server; sleep.
2. **Switch to the server** (another process: CR3 load, which flushes the TLB since PCID is
   not used).
3. **Server**: returns from `ipc_receive` (the kernel copies the message into the server's
   buffer under the global lock's protocol); decodes; works; encodes the response into its
   buffer (copy of the payload); calls `ipc_reply` (system call), where the kernel copies the
   response into a fresh `Vec` (allocation + copy), takes the global lock, marks the request
   done and wakes the client; the server calls `ipc_receive` again (system call) and sleeps.
4. **Switch back to the client** (CR3 load, TLB flush).
5. **Client**: takes the global lock, removes the request (frees the map node), decodes, and
   copies the payload into another `Vec` (`payload.to_vec()`: allocation + copy), from which
   the caller copies it again to where it belongs.

Per call: **2 address space switches, 2 server system calls, payload copies inside the IPC
alone: 2 for a request (encode, into the server) and 3 for a response (encode, into the
kernel, `to_vec`), 3-4 heap allocations, 4 acquisitions of one global lock** that every server and every
CPU shares. Messages carry at most 32 KiB of data (`MAX_DATA`), so larger transfers are
several calls, strictly one after the other. Nothing is batched, nothing is mapped.

## (a) Reading a file

`read(fd, buf, 4096)` on a file in `/data`.

### Page cache hit

`sys_file::read` → `read_to_user` allocates a kernel buffer (up to 64 KiB) → `PageCache::read`
copies from the page frame into it under the cache's spinlock → `copy_to` copies it to the
user.

| | count |
|---|---|
| IPC round trips | 0 |
| address space switches | 0 |
| CPU copies of the data | **2** (page → kernel buffer → user; Linux: 1) |
| heap allocations | 1 (the kernel buffer) |
| kernel entries | 1 |

### Page cache miss (cold file)

The miss reads ahead 16 pages (64 KiB): `PageCache::fetch` allocates a 64 KiB buffer and calls
`RemoteFs::read`, which splits it into two 32 KiB `Op::Read` calls. In diskfs, each request:
`vec![0; 32 KiB]` (allocation), ext2 `read` → the inode (a cached metadata block, cloned into
a `Vec`), `bmap` per file block (each step clones the cached indirect block, 1 KiB per data
block with 1 KiB blocks), contiguous runs read by virtio-blk into the DMA bounce buffer, which
the driver copies out.

| per 64 KiB read-ahead | count |
|---|---|
| IPC round trips | 2 |
| address space switches | 4 |
| server system calls | 4 (2 × receive + reply) |
| virtio requests | 2-3 (one per contiguous run; each notification is a VM exit, then the driver polls) |
| CPU copies of the data | **8**: DMA buffer → diskfs `Vec` → response buffer → kernel reply `Vec` → `payload.to_vec()` → fetch buffer → page frames → kernel buffer → user (the last two are the hit path) |
| heap allocations | up to about 70 per 32 KiB request: 1 for the data, 1-2 per `bmap` in the indirect region (a cloned metadata block per step and file block), plus the IPC's 3-4 |

Linux: the device writes into the page cache pages by DMA, then one copy to the user; no
context switch.

## (b) TCP send and receive

### `send(fd, buf, 64 KiB)`

`sys_net::sendto` → `write_from_user` allocates a 64 KiB kernel buffer and copies the user
data in → `Socket::send` splits it into two 32 KiB `Op::Send` calls → netd: `send_slice`
copies into smoltcp's socket buffer, `respond` allocates a response `Vec` → `iface.poll`
builds segments: smoltcp writes each frame into a fresh `Vec` (`TxToken`), which is copied
into a virtio transmit buffer.

| per 64 KiB send | count |
|---|---|
| IPC round trips | 2 (blocking sends whose buffer is full: the request is parked in netd until there is room) |
| address space switches | 4 |
| CPU copies of the data | **6**: user → kernel buffer → request `Vec` → netd buffer → socket buffer → frame `Vec` → virtio buffer (Linux: 1, or 0 with zero-copy send) |
| heap allocations | 2 per IPC + 1 per frame (about 45 frames of 1460 bytes) |

### `recv(fd, buf, 64 KiB)`

Each received frame: the device writes it into a virtio receive buffer (DMA), `Nic::receive`
copies it into a `Vec` (`f.to_vec()`), smoltcp copies the payload into the socket buffer. The
`Op::Recv` call (at most 32 KiB): `vec![0; max]` and `recv_slice` (copy), the response
buffer (copy), the kernel's reply `Vec` (copy), `payload.to_vec()` (copy), into the
`recvfrom` buffer (allocated per call, copy), to the user (copy).

| per 32 KiB received | count |
|---|---|
| IPC round trips | 1 (blocking with no data: parked in netd, retried on every event loop iteration) |
| address space switches | 2 |
| CPU copies of the data | **8** (Linux: 1) |
| heap allocations | about 6 per call + 1 per frame |

### Readiness

`poll`/`epoll` registration asks netd per socket with an `Op::Poll` IPC; wakeups come by
`ipc_notify`. A server loop over many sockets pays a round trip per socket per poll.

### Loopback

A frame to 127.0.0.1 or the own address is copied into a `Vec` queue in netd and received
from there: both directions' costs on one side, plus that copy.

## (c) Writing to the block device

`write(fd, buf, 64 KiB)` to a file in `/data` (write-through):

`write_from_user` (64 KiB kernel buffer, copy) → `PageCache::write` → `RemoteFs::write`, two
32 KiB `Op::Write` calls → diskfs: ext2 `write` allocates the blocks (bitmap, group
descriptor and superblock changes in the metadata cache), writes contiguous runs of data
through the virtio-blk bounce buffer (copy), then commits: the dirty metadata blocks (inode
table, bitmap, group descriptor, indirect blocks: adjacent ones together), the superblock,
one flush. After the server answered, the kernel copies the data into the cached pages.

| per 64 KiB write | count |
|---|---|
| IPC round trips | 2 |
| address space switches | 4 |
| virtio requests | about 12 (per 32 KiB: 1-2 data runs, 3-4 metadata runs, superblock, flush), each synchronous |
| device flushes | 2 (one per IPC: each `write` call is durable when it returns) |
| CPU copies of the data | **6**: user → kernel buffer → request `Vec` → diskfs buffer → DMA bounce buffer; plus kernel buffer → page cache |
| heap allocations | the IPC's, plus one cloned metadata block per `bmap` and per metadata change |

Linux: the data is copied once into the page cache and written back later (in large
batches, by DMA from the page cache pages); `fsync` makes it durable.

## Findings

1. **IPC is the data plane.** Every byte to or from a server travels inside messages, in
   32 KiB pieces, with a full synchronous round trip each. That is principle 1 violated at
   the root, and it multiplies everything below.
2. **Copies.** 6-8 CPU copies per byte where Linux has one. The IPC layer alone copies two to
   three times per direction; the rest come from `Vec` round trips at every layer boundary and from bounce
   buffers in front of the devices.
3. **Allocations on the hot path**: in the IPC (request map, message, reply, payload), in the
   servers (data buffers, metadata block clones per `bmap`, response buffers, frame `Vec`s).
4. **A global IPC lock** (`IrqSpinLock<Ipc>`) serializes all services on all CPUs.
5. **Address space switches** flush the TLB (no PCID); two per round trip.
6. **No batching**: one request per message, one message per wakeup; the page cache's
   read-ahead and the socket layer issue their pieces one after the other.
7. **Synchronous, polled devices**: diskfs has one request in flight; netd processes one
   message per loop iteration.
8. **Durability per write call**: each `write` to `/data` is flushed to the device before it
   returns (write-through), which Linux only does on `fsync`/`O_SYNC`.
9. **DMA is not confined** (no IOMMU yet): see `docs/design/iommu.md`. A buffer-grant API for
   zero-copy must therefore be designed so that the IOMMU confines devices to granted buffers
   later, without changing the interface.
10. **Kernel entries** are not the bottleneck yet (the copies and switches are), but each
    round trip costs the server two system calls, and there are no mitigations (KPTI,
    IBRS) in this kernel that would make them more expensive than plain `syscall`/`sysret`.

## Not in the system

The task's ground rules mention curl and IPv6: neither exists in oxidenix today (BusyBox
`wget` is the HTTP client; the network stack is IPv4 only). The smoke test uses what exists:
boot, Bash, `wget` over the network.

## Measured baseline

`scripts/bench.sh` on commit `841f4cf` against a Linux 6.18 guest in the same QEMU
configuration (`docs/benchmarks/2026-10-07-841f4cf-baseline.md`,
`docs/benchmarks/2026-10-07-linux-6.18.54.md`; i5-1240P host, KVM):

| benchmark | oxidenix | Linux | |
|---|---:|---:|---|
| null system call, p50 | 723 cycles | 1412 cycles | Linux pays for its Spectre/Meltdown mitigations; oxidenix has none |
| `fstat` of a disk file, p50 (oxidenix: one IPC round trip) | 33071 cycles | 2172 cycles | the IPC round trip costs ~31600 cycles, 45 null system calls |
| `fstat`, p99 | 165324 cycles | 5251 cycles | |
| sequential write, 64 KiB + `fsync` | 2.9 MB/s | 162.1 MB/s | 56× slower: a device flush per `write`, and diskfs waits for it by spinning on `sched_yield` (35000 system calls per 64 KiB) |
| sequential read from the disk (`O_DIRECT`, 64 KiB) | 126.2 MB/s | 211.1 MB/s | |
| sequential read from the page cache | 3384 MB/s | 3213 MB/s | equal: no IPC on this path |
| 4 KiB read from the page cache, p50 / p99 | 2366 / 3411 ns | 1954 / 20586 ns | |
| 4 KiB read from the disk, p50 / p99 | 76 / 394 µs | 117 / 1461 µs | both mostly the device |
| TCP over loopback | 229 MB/s | 1660 MB/s | 7× slower: 8 IPC round trips, 14 address space switches and 33 kernel allocations per 64 KiB |
| TCP through the network card (echo) | 20.6 MB/s | 55.1 MB/s | |

The counters confirm the audit: a disk `read` or `write` of 64 KiB is 2 IPC round trips with
64 KiB through IPC and twice the data through user copies; a cached read has no IPC and
copies the data once to the user (plus once more inside the kernel).
