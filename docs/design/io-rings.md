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
(`Layout::new`). Both ends map the header page read-only, so neither can forge `state`; the
rings are read and write. The positions start at 0.

**Client calls** (the Linux server, `crates/restricted`):

| Call | |
|------|---|
| `chan_create(slots, &addr) -> handle` (1064) | the channel, mapped into the server's region (header read-only) (`MAPS_BASE`..`HEAP_BASE`, a range the kernel maps memory objects into for the server); at most 64 per instance |
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
| `chan_watch(channel, value)` (1073) | arms the service's doorbell watch on the submission ring: if its `tail` still holds `value` (else `EAGAIN`), the client's next doorbell or its end going makes `ipc_receive` return `Event::Doorbell` (step 3) |
| `set_copy_fixup(insn, fixup)` (1074) | a fault of the program's copy instruction on granted memory resumes at `fixup` instead of killing it (`oxrt::copy`, step 3) |
| `grant_dma_pages(channel, id, first, count, &out)` (1075) | `grant_dma` for up to 512 pages in one call (step 3) |

**Attach.** The Linux server has no IPC of its own (only the kernel is an IPC client), so the
kernel carries the offer: `chan_connect` sends the service a control request, typed by its id
(no protocol message can pass for one) and only to services registered with `IPC_CHANNELS`.
Whether the channel is attached is the kernel's to say, not the service's answer: a service
that attaches and answers an error still has the channel, and one that answers 0 without
attaching has not. So `chan_connect` returns as soon as the service attached, answered or not
(a service that attaches and never answers cannot hold the client); before that, a signal (a
fatal one always) gives the offer up, and a later `chan_attach` fails.

**Doorbells.** The ring words are futex words. The service's futex on its shared mapping and
the server's `server_futex_wait` on its region mapping are both keyed by the channel's memory
object and the offset, so they meet. A service whose event loop waits for several things at
once (IPC requests, offers, several channels: diskfs) sleeps in `ipc_receive` instead: it
announces the sleep in each ring (`Consumer::prepare_sleep`, invariant 4) and arms a watch on
each submission `tail` (`chan_watch`), a one-shot waiter in the futex bucket that a wake turns
into a pending doorbell of the service's server and a wakeup of its `ipc_receive`. The watch
compares the word under the bucket lock like a wait, so no wakeup is lost; one per word and
process (arming it twice keeps one), gone with the wake or the channel's hang-up.

**Pinning.** A grant takes each page's frame with a reference of its own and a pin count in
the object's page cache: a pinned page stays the object's page (a truncation over it fails with
`EBUSY`, reclaim skips it). Pages are made present when granted; a paged object's page must
have been supplied (`ENODATA`: its pager may be the caller). Remote (disk) caches are not
grantable. Step 4 grants pages before they are supplied, for diskfs to read into.

What a pin costs and how long it lasts: the kernel keeps 8 bytes per pinned page in the grant
(its frame) and a counter in the page; the pages themselves stay charged to their object. The
limits per channel (4096 grants, 65536 pages, 64 channels per instance) bound that. A draining
grant (below) is pinned until the service lets go, and a quarantined one until the service's
server registers again: a crashed service whose server is not restarted, or a service that
never calls `grant_dma_unmap`, keeps the client's pages pinned (their objects alive and
untruncatable over them) for as long as that lasts. That is the price of never freeing memory
a device may still write without an IOMMU; with one, revocation is immediate. The kernel's
teardown paths allocate nothing (a teardown's work is allocated with the end it tears down, a
quarantine is a list through the grants), so running out of memory never stops a teardown.

**Revoke.** The grant's mappings in the service are gone (TLB shootdowns included) when
`revoke` returns; their ranges stay reserved and inaccessible (`Backing::Revoked`: no access
can be added, nothing else is mapped there) until the service unmaps them or its end of the
channel goes, so a service copy to the address it knew faults and fails rather than reaching
another grant or the service's own memory. Its device mappings are taken out of the service's `DmaDomain` (the one
place that maps pages for devices). Without an IOMMU a device address cannot be taken back: a
grant the service has device addresses of stays pinned, its id taken, until the service calls
`grant_dma_unmap` (`revoke` returns `REVOKE_DRAINING`). Data integrity is the protocol's: the
client revokes after the requests on the grant completed. The kernel only guarantees that no
memory a device or the service can still reach is freed or reused.

**Teardown.** When the client's end goes (its handle closed, or the instance ended) every grant
is revoked as above; when the service's end goes (`chan_detach`, or its process ended or executed a new program:
the service is its address space, not its process id) every
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
the two). The service is its address space: after an `execve` the new program is not the
service.

**The service's contract for grant memory.** A revoke removes the service's mapping at once,
whatever the service is doing: the client may revoke (or its end may go) while the service
still works on a request, and a CPU access to a revoked grant then faults (`SIGSEGV`). The
kernel does not wait for the service, since a service that never acknowledged would otherwise
keep the client's memory hostage. So a service reaches grant memory either by DMA (its device
addresses stay valid until `grant_dma_unmap`, the draining above) or with a copy that survives a
fault: the CPU copy sits behind a fault handler that turns the fault into an error for that
request (as the Linux server's copies into program memory do, `set_usercopy`), or the service
runs CPU copies only on grants whose requests the client has not completed. A well-behaved
client revokes only after the requests on a grant completed, so the fault path only meets a
broken or hostile client; the service must survive it and fail the request (`EIO`/`EFAULT`),
nothing more. (servers/ringtest touches grants directly: its client is the test.) The fault
handler is `set_copy_fixup`: a service registers its copy instruction (`oxrt::copy`, one `rep
movsb`), and a page fault there that the kernel cannot resolve resumes at the fixup, which
reports the copy as failed, as the Linux server's `set_usercopy` does for program memory.

**Tests.** `lxtest` has the Linux server run seven scenarios (`TEST_CHANNEL`) against
`servers/ringtest`, a service started in test mode only (`ring::selftest`): rings and doorbells
in both directions, connect errors, the header read-only to the service; grants (data both ways, read-only enforced by mprotect, by
the kernel's own stores and, fatally, by a CPU store; the kernel's bounds; device addresses;
pinning against truncation; unsupplied pages); revoking (the mapping gone, a draining grant
pinned and its id held until `grant_dma_unmap`, ids reused); the client's end going while the
service sleeps; the service dying while the client waits, and coming back for the next channel;
the service executing a new program, which reaches no grant; a service that attaches but
answers the offer only after it served the channel.
`cargo test --release -p ring` covers the layout, the offer encoding and `pop_wait_while`.

### The file protocol (step 3)

`crates/fsring` (the encodings, the one validator, host tests); diskfs's side is
`servers/diskfs/src/service.rs`, the test client `servers/linux/src/disktest.rs`
(`TEST_DISKRING`). A client connects a channel of `fsring::SLOTS` (128) slots to the service
`diskfs`, grants pages of its memory objects and sends descriptors:

| op | request | completion |
|----|---------|------------|
| `READ` / `WRITE` | inode, file offset, a grant range (at most 1 MiB; `READ` needs a writable grant) | bytes (a read is short at the end of the file), v0 = file size |
| `FLUSH` | - | 0: every write completed before it is durable |
| `STAT`, `STATFS` | inode / - | 0, the values packed in v0..v3 (`fsring::Stat`, `Usage`) |
| `LOOKUP`, `CREATE`, `UNLINK`, `RENAME` | directory, names in a grant (a symlink's target or the new name right after the first name) | v0 = the inode found or made, or the one whose last link went |
| `TRUNCATE`, `SETPERM` | inode and the new size / permissions | 0 |
| `RELEASE` | inode | 0: the client holds it no more (see "Holds" below) |
| `READDIR`, `READLINK` | a result buffer in a writable grant (`READDIR` with a cursor) | bytes, v0 = the next cursor |
| `FORGET` | a grant | 0 once diskfs let go of it (send before `revoke`: no draining) |

A completion is (tag, op, status, four values); every field an operation does not use must be
0 (`EINVAL`), an unknown operation is `ENOSYS`, an unknown or revoked grant `EBADF`, a range
beyond its grant `EINVAL`, an inode not in use `ENOENT`, a copy into a grant revoked under
diskfs `EFAULT` (`set_copy_fixup`).

**Ordering and durability.** Reads and writes run concurrently (at most 32 in flight in
diskfs, the device reordering them); a write waits for writes in flight on the same blocks.
Everything else is a barrier: it starts when every request taken before it (any channel)
completed. A completed `WRITE` is visible to every later request, also through the kernel's
IPC path, and durable after a `FLUSH`: write-back. diskfs reserves the blocks of a write in
memory (in no bitmap or inode), writes the data by DMA, then links them; a `FLUSH` (and any
commit) flushes the device before metadata reaches it and again after, so a crash never
leaves a pointer to a block whose data did not reach the disk (ext2fs, "Ordering").

**What moves the data.** The device, by DMA between the disk and the granted pages, at their
device addresses (`grant_dma_pages`, cached per channel): no copy for any read and for any
write of whole sectors. Sector bytes a read does not want go to a sink page; a write padded
to whole blocks (a new block) takes zeros from a zero page. The CPU copies only: zeros into
the holes of a read; one or two sectors read from the disk into scratch memory for a write
that starts or ends inside an existing block's sector (the device then writes them from there
and the grant); names and small results (`READDIR`, `READLINK`); and, after a failed commit
left a block's data in ext2fs's cache, the whole request through ext2fs's own read or write.
With 1 KiB ext2 blocks a 4 KiB page is four blocks, contiguous or not: a run of contiguous
blocks is one device request, otherwise several, all still DMA.

**Waiting.** diskfs has one thread: `ipc_receive` brings the kernel's IPC requests (which wait
until the ring operations in flight completed), channel offers and doorbells. With work it
polls the rings and the device; after `SPIN_BUDGET` (2000) polls without progress it yields
(device busy: its interrupts stay off, the line being shared with the network card) or arms
every ring's doorbell (`prepare_sleep`, `chan_watch`) and sleeps in `ipc_receive`.

**Holds.** A client holds every inode it named or got back from `LOOKUP`, `CREATE`, `UNLINK`
or `RENAME`, until its `RELEASE` or the end of its channel; the kernel's IPC client holds what
it named until its `Release` (which it sends only for the inodes it unlinked). An inode whose
last link went is freed when no client holds it, so one client never frees what another
uses; one a ring client unlinks while the kernel holds it stays allocated (an orphan for
e2fsck) until step 4 removes the kernel's client.

**Room.** A request waits in the submission ring while its channel's completion ring has no
room for its completion; diskfs then sleeps on that ring's doorbell instead of polling, and a
client that took completions while requests of its waited rings it.

**Limits.** 16 channels, 1024 grants a channel diskfs keeps (`FORGET` lets go), no more
requests taken from a channel than its completion ring has room for, 32 operations in flight
(a write waiting for a slot stalls), a client gone: its operations in flight finish into the
pinned pages, then diskfs detaches and its holds go. A device whose segments are under a
sector, or that takes too few per request for a page and its pads, or an image with blocks
over a page, is refused at start.

### The page cache's kernel interface (step 4)

The Linux server's page cache of a disk file is a **cached object**
(`mo_create_cached(size, key, limit)`, 1076; `kernel/src/fs/cache.rs`, the cached store): a
file object (reads, writes, mappings, programs use it like a tmpfs file object) whose size,
pages and dirty marks the kernel keeps and whose data the server moves:

- **Fill.** A missing page is asked for with `EVENT_PAGE` (a fault, or the kernel reading the
  object) or reported missing to the server's own reads and writes (`MO_NOFILL`: the call stops
  there, `EAGAIN` if nothing was done). The server grants the first run of missing pages of a
  window (`grant(.., GRANT_WRITE | GRANT_FILL, out)`: at most 256, the run's first page and
  length at `out`): they become **pending**, zeroed frames pinned for the grant that nobody reads,
  maps or writes. diskfs reads into them by DMA; `mo_filled(handle, offset, pages, ok)` (1077)
  makes them the file's pages or drops them (their waiters get `EIO`, a mapping `SIGBUS`, a later
  access asks again) and wakes the waiters. No copy: the page the program maps is the page the
  device wrote.
- **Write-back.** Writes and stores through shared mappings mark pages dirty (a mapping's page
  is writable only once dirty, as for the kernel's own disk files); an object's first dirty page
  sends `EVENT_DIRTY` (key). `grant(.., GRANT_DIRTY, out)` takes the first run of dirty pages of
  a window: clean from then on, write-protected in every mapping (a store marks them dirty again),
  pinned read-only for diskfs to write from by DMA; a write that failed puts them back with
  `mo_redirty` (1078).
- **Memory.** The pages are cached memory (`Cached:`, `memory::cache_charge`): clean ones that
  nothing pins or maps are reclaimed when a commit or a new cache page needs room; pending and
  dirty ones are not. Dirty pages of all caches count in `Dirty:`; above a tenth of the commit
  limit, or when reclaim finds them in its way, the pagers get `EVENT_WRITEBACK` (one queued at a
  time), and above a fifth a thread that stores waits up to a second for them (Linux's dirty
  ratios; never the pager, which does the writing). `event_wait` takes a deadline (`EVENT_TIMER`,
  for the server's periodic write-back) and delivers `EVENT_CLOSING` once when the instance's
  last program is gone, for its final write-back.
- **Faults wait unlocked.** A fault that needs a page from a pager asks for it and waits with
  the address space unlocked, then tries again (`Fault::Retry`; `MAP_POPULATE` only asks): the
  pager's write-back write-protects mappings, which locks address spaces, so no thread may wait
  for the pager while holding one.
- **msync.** `vm_sync(addr, len, flags, out, cap)` reports the cached objects mapped shared in
  the range (key, first and end page) for the server to write back with `MS_SYNC`.
- `mo_map_server(handle, pages)` / `mo_unmap_server(addr)` (1079, 1080) map a plain memory
  object into the server's region: the scratch buffer it grants diskfs for names and results.

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
tuning knob measured with `iobench` (diskfs: `service::SPIN_BUDGET`). diskfs keeps the disk's
interrupt off for now (its line is shared with the network card, and the kernel gives a line
to one server): with requests in flight it polls the device and yields between rounds.

**Tests of step 3.** `lxtest` runs nine scenarios against diskfs (`TEST_DISKRING`): the disk
image's README read by DMA from an unaligned file offset into an unaligned grant offset,
`STAT`, `READDIR` with and without a cursor, `STATFS`, a symlink made, read (`ERANGE` for a
short buffer) and removed; writes aligned, unaligned within existing blocks, within one sector
and past the end leaving a hole, a `FLUSH`, the file read back against a model, truncated
shorter and longer, renamed, permissions changed; malformed requests (`ENOSYS`, stray flags
and arguments, `EBADF`, ranges beyond a grant, over 1 MiB, `EACCES` for a read-only grant,
`ENOENT` for inodes not in use, `EINVAL`, `ENAMETOOLONG`, `ENOTDIR`, `EEXIST`) with the channel
working afterwards; 24 writes and then 24 reads in flight, completions matched by tag; a grant
revoked under diskfs (`REVOKE_DRAINING`, then `EFAULT` for a copy into it, diskfs alive,
`FORGET` freeing the id, `FORGET` before `revoke` not draining) and a client closing its
channel with 16 reads in flight (diskfs serves the next one); the range of a revoked grant
given to no other grant (a copy to it `EFAULT`, the next grant untouched); an unlinked inode
one channel released still working for another that holds it, freed when that one releases
it or goes; a write stalled behind an overlapping one while another channel's long reads keep
the operation slots busy; requests waiting for room in their completion ring with diskfs using
no CPU meanwhile (its ticks in `/proc` over 500 ms), all completing once the client makes room.
The file the second scenario leaves is read through the kernel's `/data` (the IPC protocol)
and removed there. ringtest's `CHECK_GONE` checks the reservation a revoke leaves (`EACCES`
for `mprotect`, `ENOMEM` once the service unmapped it).

## Steps

1. `crates/ring`: the SPSC ring and descriptors, host-tested (single- and two-thread tests of
   the invariants, including wrap-around and the sleep/wake protocol under contention).
2. Kernel: channel objects, grants with pinning, `grant_dma`, teardown on death; shared futex
   doorbells already work (futexes on shared memory objects). Done: see "The kernel's
   interface" above.
3. diskfs: the ring protocol beside the IPC one (the kernel's `RemoteFs` stays the IPC client
   until R6c.3 ends), virtio-blk with requests in flight and DMA into granted pages. Done: see
   "The file protocol" above. Until step 4 both protocols serve one filesystem without
   coherence between their clients' caches (a ring client's changes reach the kernel's page
   cache only for files the kernel had not cached), and an inode unlinked on one side may be
   released on the other while still open there.
4. The Linux server: `/data` through its page cache over the ring; the bridge to the kernel's
   `/data` goes. Benchmarks: sequential and random reads and writes, `fstat`, against the
   numbers in the audit and Linux.
5. The same for procfs (metadata only) and, in R7, netd.
