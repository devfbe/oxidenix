# I/O rings: the data plane between the Linux server and the device servers

Status: accepted (ADR 0005). Implements principles 1-7 of the I/O audit
(`docs/io-path-audit.md`) for the paths the Linux server takes over in R6c.3 (files on
`/data`), R7 (sockets) and later.

## Problem

Every byte between a program and a device server travels inside kernel IPC messages today:
copied into the kernel, out to the server, back, in 32 KiB pieces with a synchronous round trip
each, under one global lock (audit findings 1-4, 6, 7). Once the Linux server owns files and
sockets, the kernel's VFS and socket layer are no longer in the path, so the IPC between the
Linux server and diskfs or netd becomes the whole I/O path. It must not be a message copy.

## Model

**Control plane and data plane are separate.**

- *Control*: opening a channel to a service, registering buffers, errors, teardown: the existing
  synchronous IPC (`ipc_call`), rare and small.
- *Data*: a **channel** is a pair of single-producer single-consumer rings in a shared memory
  object, mapped into both ends: a **submission ring** (client → service) and a **completion
  ring** (service → client). Entries are fixed-size descriptors (64 bytes); data never travels in
  the rings, only references to **granted buffers**.

**Buffers are granted, not copied.** A client grants pages of its memory objects to a channel:
the kernel maps them into the service (read-only or writable, as granted) and pins them while
granted. A descriptor names a buffer by (grant, offset, length). For the Linux server's page
cache, the granted pages *are* the cache's pages: diskfs reads a block straight into the page a
program will map (zero copy with DMA; one copy without it).

**Devices get device addresses, never physical addresses by convention.** A service asks the
kernel for the device-visible address of a granted page (`grant_dma(grant, offset)`). Without an
IOMMU this is the physical address; with one (`docs/design/iommu.md`, steps 2-3) the kernel maps
the page into the device's domain and returns the IOVA, and revokes it with the grant. The
interface does not change; the confinement does.

**Servers stay out of the data path where they can.** A page cache hit never reaches diskfs (the
Linux server serves it from its own cache). diskfs translates file offsets to blocks and starts
the device; the data moves device → page by DMA. netd, likewise, moves frames between granted
socket buffers and the NIC.

**Few kernel entries.** Submitting is writing descriptors and one release store; the service
learns of it by polling while busy and by a doorbell (a futex wake on the ring's doorbell word)
only when it said it sleeps. Completions likewise. Under load the steady state is no kernel
entry per request at all; idle, one wake per batch.

## Rings

```
struct Ring {                 // in the channel's shared memory object
    head: AtomicU32,          // consumer's position (written by the consumer only)
    _pad: [u8; 60],           // own cache line
    tail: AtomicU32,          // producer's position (written by the producer only)
    _pad: [u8; 60],
    sleeping: AtomicU32,      // the consumer sleeps on `tail`'s futex (doorbell requested)
    _pad: [u8; 60],
    slots: [Desc; N],         // N a power of two
}
```

Invariants (each checked by the ring's tests):

1. Only the producer writes `tail` and the slots in `[tail, head + N)`; only the consumer writes
   `head`. Positions run freely modulo 2^32; `tail - head` is the fill level and never exceeds N.
2. **Publication**: the producer writes a slot, then stores `tail + 1` with `Release`. The
   consumer loads `tail` with `Acquire` before reading slots below it, so it sees the slot's
   contents (and everything the producer wrote before, such as data in a granted buffer).
3. **Recycling**: the consumer reads a slot, then stores `head + 1` with `Release`. The producer
   loads `head` with `Acquire` before writing a slot at or beyond the old `head + N`, so the
   consumer's read happened before the overwrite.
4. **No lost wakeup**: only the consumer writes `sleeping`. Finding the ring empty, it stores
   `sleeping = 1`, fences (`SeqCst`), loads `tail` again and sleeps on the futex only if it is
   unchanged; it clears `sleeping` once it has an entry. A producer stores `tail` (`Release`),
   fences (`SeqCst`) and loads `sleeping`; if set, it wakes. Either the consumer's second load
   sees the new tail, or the producer's load sees `sleeping`. (A producer that cleared the
   flag could erase the one of the consumer's next sleep: the ring's tests caught exactly that.)
5. A malformed descriptor (bad grant, offset beyond the grant, unknown operation) completes with
   an error; it never touches memory outside the grants. The service validates every field it
   reads from shared memory once (copies the descriptor out first: no double fetch).

## Descriptors

```
struct Desc {                 // 64 bytes
    op: u16, flags: u16, len: u32,
    tag: u64,                 // the client's: echoed in the completion
    object: u64,              // file (inode) or socket the operation is on
    offset: u64,              // in the object
    grant: u32, buf_off: u32, // the buffer: a page range of a grant
    arg: [u64; 3],
}
```

A completion carries the tag, a status (bytes or `-errno`) and up to two values. Requests in a
ring complete in any order (the device may reorder); the tag matches them up.

## Channel setup and lifetime

1. The client creates a channel object (`chan_create(slots)`: one memory object holding both
   rings) and asks the service to attach it over IPC (`Attach(channel handle)`); the kernel maps
   it into the service. One channel per (instance, service) to start; one per CPU later if the
   locks show up in profiles.
2. Grants: `grant(channel, object, offset, pages, writable) -> grant id`, `revoke(grant)`. A
   revoke completes once the service has no request in flight on the grant (it acknowledges
   through the completion ring), so no device writes a page after it was revoked.
3. When either end dies, the kernel tears the channel down: the client's in-flight requests
   fail (`EIO`), its grants are revoked (device mappings first), the service's mappings of them
   go, and the survivor wakes and sees the end gone.
   A restarted service gets new channels; clients reattach and resubmit (idempotent reads,
   writes are re-sent by the page cache's write-back).

### The kernel's interface (step 2)

`kernel/src/process/channel.rs`; the layout both ends map is `crates/ring` (`ring::channel`).

**Layout.** A channel of `slots` slots per ring (a power of two, 2-4096) is one zeroed,
committed memory object: a header page (magic, version, slots, the rings' offsets, and on its
own cache line the `state` word), then the submission ring and the completion ring, each a
`RingMemory<slots>` starting on a page of its own. The layout is a function of `slots` alone
(`Layout::new`); an end never needs to trust the header, which the other end can write. The
positions start at 0.

**Client calls** (the Linux server, `crates/restricted`):

| Call | |
|------|---|
| `chan_create(slots, &addr) -> handle` (1064) | the channel, mapped writable into the server's region (`MAPS_BASE`..`HEAP_BASE`, a range the kernel maps memory objects into for the server); at most 64 per instance |
| `chan_connect(handle, name, len)` (1065) | offers it to the service `name` (a dead server of the kernel's is started again) and waits until it attached or refused; `EISCONN`, `ENOENT`, `EOPNOTSUPP` for a service that takes no channels, `EIO` if it died, `EINTR` for a signal before it attached |
| `grant(handle, object, offset, pages, flags) -> id` (1066) | pins `pages` pages of a memory or file object (`GRANT_WRITE`: writable); `ENOTCONN` before the service attached, `EPIPE` after it went; at most 4096 grants and 65536 pages per channel |
| `revoke(handle, id) -> 0 \| REVOKE_DRAINING` (1067) | see below |
| `handle_close(handle)` | the client's end goes (also when the instance ends) |

**Service calls** (`oxrt::sys`, for the kernel's servers):

| Call | |
|------|---|
| `ipc_register(name, len, arg, IPC_CHANNELS)` (1000) | the service accepts channel offers |
| `ipc_receive` | an offer comes as a control request (id with bit 63 set, `oxrt::Event::Control`) whose payload is a `ring::channel::Offer` (channel id, slots, client pid); the service answers with an 8-byte status |
| `chan_attach(channel) -> addr` (1068) | maps an offered channel (only the service it was offered to, only once) |
| `chan_detach(channel)` (1069) | lets go of it: the service's mappings of the channel and of its grants go, and the service vouches that no device uses the grants any more |
| `grant_map(channel, id, &info) -> addr` (1070) | maps a grant: read-only unless granted writable (`mprotect` cannot add write or execute), not inherited by `fork`, not movable by `mremap`; `info` gets (pages, writable) |
| `grant_dma(channel, id, offset) -> device address` (1071) | of the byte at `offset` (`EINVAL` beyond the grant), valid to the end of its page |
| `grant_dma_unmap(channel, id)` (1072) | the service's devices are done with the grant |

**Attach.** The Linux server has no IPC of its own (only the kernel is an IPC client), so the
kernel carries the offer: `chan_connect` sends the service a control request, typed by its id
(no protocol message can pass for one) and only to services registered with `IPC_CHANNELS`.
Whether the channel is attached is the kernel's to say, not the service's answer: a service
that attaches and answers an error still has the channel, and one that answers 0 without
attaching has not.

**Doorbells.** The ring words are futex words. The service's futex on its shared mapping and
the server's `server_futex_wait` on its region mapping are both keyed by the channel's memory
object and the offset, so they meet.

**Pinning.** A grant takes each page's frame with a reference of its own and a pin count in
the object's page cache: a pinned page stays the object's page (a truncation over it fails with
`EBUSY`, reclaim skips it). Pages are made present when granted; a paged object's page must
have been supplied (`ENODATA`: its pager may be the caller). Remote (disk) caches are not
grantable. Step 4 grants pages before they are supplied, for diskfs to read into.

**Revoke.** The grant's mappings in the service are gone (TLB shootdowns included) when
`revoke` returns, and its device mappings are taken out of the service's `DmaDomain` (the one
place that maps pages for devices). Without an IOMMU a device address cannot be taken back: a
grant the service has device addresses of stays pinned, its id taken, until the service calls
`grant_dma_unmap` (`revoke` returns `REVOKE_DRAINING`). Data integrity is the protocol's: the
client revokes after the requests on the grant completed. The kernel only guarantees that no
memory a device or the service can still reach is freed or reused.

**Teardown.** When the client's end goes (its handle closed, or the instance ended) every grant
is revoked as above; when the service's end goes (`chan_detach`, or its process ended) every
grant is released, except that grants a dead service's device may still reach wait in its
server's `DmaDomain` until the server's next process registers (a server resets its device
before it registers). Either way the kernel sets the end's bit in `state` (`CLIENT_GONE`,
`SERVICE_GONE`) and hangs the channel's memory up: from then on every futex wait on it fails
with `EPIPE` (checked under the futex bucket lock) and every sleeper on it is woken, so an end
sleeping on a doorbell always wakes (`Consumer::pop_wait_while` stops once `state` is set). An
end's mapping of the ring memory itself stays until it closes its handle or detaches. In-flight
requests are the client's to fail. Teardowns triggered where the kernel may not sleep (a
process's end, an instance's last reference) run on the `channels` kernel thread; closing the
handle tears down synchronously.

**Hostile peers.** The kernel never reads the rings. Every grant id, offset and length a service
passes is checked against the grant; a grant's id is reused only after the grant is fully gone,
and a service's mapping of a grant can never be made after its revoke (a per-grant lock orders
the two).

**Tests.** `lxtest` has the Linux server run five scenarios (`TEST_CHANNEL`) against
`servers/ringtest`, a service started in test mode only (`ring::selftest`): rings and doorbells
in both directions, connect errors; grants (data both ways, read-only enforced by mprotect, by
the kernel's own stores and, fatally, by a CPU store; the kernel's bounds; device addresses;
pinning against truncation; unsupplied pages); revoking (the mapping gone, a draining grant
pinned and its id held until `grant_dma_unmap`, ids reused); the client's end going while the
service sleeps; the service dying while the client waits, and coming back for the next channel.
`cargo test --release -p ring` covers the layout, the offer encoding and `pop_wait_while`.

## Paths built on it

- **Files on `/data` (R6c.3)**: the Linux server's page cache holds paged objects, one per open
  file, whose pager is the server. A miss: the pager thread grants the missing page (and the
  read-ahead window) and submits `Read(ino, offset)`; diskfs maps file blocks and has the
  virtio queue DMA into the pages; the completion makes the pages present (`mo_supply` without
  data: the page already holds it). Writes dirty pages in the cache; write-back submits `Write`
  descriptors over granted dirty pages in batches, and `fsync` waits for their completions and
  a `Flush`. Durability per `write(2)` (audit finding 8) ends: as Linux, only `fsync`/`O_SYNC`
  wait for the device.
- **Metadata** (lookup, create, unlink, rename, stat, readdir) goes through the same ring with
  a small buffer for names and results; it is not hot enough to need more.
- **Sockets (R7)**: per socket a receive and a send buffer, granted to netd; TCP segments are
  copied once (NIC buffer ↔ socket buffer), as Linux does without zero-copy sockets.

## Polling and interrupts

diskfs and netd poll their rings and devices while they have work, then arm the device
interrupt and the ring doorbell and sleep (NAPI's pattern). The Linux server waits for
completions on the completion ring's futex the same way. The spin budget before sleeping is a
tuning knob measured with `iobench`.

## Steps

1. `crates/ring`: the SPSC ring and descriptors, host-tested (single- and two-thread tests of
   the invariants, including wrap-around and the sleep/wake protocol under contention).
2. Kernel: channel objects, grants with pinning, `grant_dma`, teardown on death; shared futex
   doorbells already work (futexes on shared memory objects). Done: see "The kernel's
   interface" above.
3. diskfs: the ring protocol beside the IPC one (the kernel's `RemoteFs` stays the IPC client
   until R6c.3 ends), virtio-blk with requests in flight and DMA into granted pages.
4. The Linux server: `/data` through its page cache over the ring; the bridge to the kernel's
   `/data` goes. Benchmarks: sequential and random reads and writes, `fstat`, against the
   numbers in the audit and Linux.
5. The same for procfs (metadata only) and, in R7, netd.
