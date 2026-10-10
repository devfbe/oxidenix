# I/O rings: the data plane between the Linux server and the device servers

Status: accepted (ADR 0005); steps 1-5 done (files on `/data` go through the rings since
R6c.3, sockets since R7b, `/proc` and `/sys` since step 5). Implements principles 1-7 of the
I/O audit (`docs/io-path-audit.md`) for the paths the Linux server takes over in R6c.3 (files
on `/data`), R7 (sockets) and step 5 (procfs).

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
`RingMemory<slots>` starting on a page of its own, then the shared area if the client asked for
one. The layout is a function of `slots` and the shared pages alone (`Layout::with_shared`; the
offer carries both). Both ends map the header page read-only, so neither can forge `state`; the
rings and the shared area are read and write. The positions start at 0.

**Client calls** (the Linux server, `crates/restricted`):

| Call | |
|------|---|
| `chan_create(slots, &addr, shared) -> handle` (1064) | the channel, mapped into the server's region (header read-only) (`MAPS_BASE`..`HEAP_BASE`, a range the kernel maps memory objects into for the server), with `shared` pages (at most 256) of **shared area** after the rings: the protocol's own state, mapped read and write into both ends like the rings and, unlike grants, never taken from the service while it is attached (it may use atomics there; `netring`'s control blocks, ADR 0008); at most 64 per instance |
| `chan_connect(handle, name, len)` (1065) | offers it to the service `name` (a dead server of the kernel's is started again) and waits until it attached or refused; `EISCONN`, `ENOENT`, `EOPNOTSUPP` for a service that takes no channels, `EIO` if it died, `EINTR` for a signal before it attached |
| `grant(handle, object, offset, pages, flags) -> id` (1066) | pins `pages` pages of a memory or file object (`GRANT_WRITE`: writable); `ENOTCONN` before the service attached, `EPIPE` after it went; at most 4096 grants and 65536 pages per channel |
| `revoke(handle, id) -> 0 \| REVOKE_DRAINING` (1067) | see below |
| `handle_close(handle)` | the client's end goes (also when the instance ends) |

**Service calls** (`oxrt::sys`, for the kernel's servers):

| Call | |
|------|---|
| `ipc_register(name, len, arg, IPC_CHANNELS)` (1000) | the service accepts channel offers |
| `ipc_receive` | an offer comes as a control request (id with bit 63 set, `oxrt::Event::Control`) whose payload is a `ring::channel::Offer` (channel id, slots, shared pages, client pid, the client's instance, by which a service accounts what all of an instance's channels take); the service answers with an 8-byte status |
| `chan_attach(channel) -> addr` (1068) | maps an offered channel (only the service it was offered to, only once) |
| `chan_detach(channel)` (1069) | lets go of it: the service's mappings of the channel and of its grants go, and the service vouches that no device uses the grants any more |
| `grant_map(channel, id, &info, max_pages) -> addr` (1070) | maps a grant: read-only unless granted writable (`mprotect` cannot add write or execute), not inherited by `fork`, not movable by `mremap`; `info` gets (pages, writable); a grant of more than `max_pages` pages (0: any) is refused with `E2BIG` before anything is mapped (`info` stored), so a service bounds what a client makes it map |
| `grant_dma(channel, id, offset) -> device address` (1071) | of the byte at `offset` (`EINVAL` beyond the grant), valid to the end of its page |
| `grant_dma_unmap(channel, id)` (1072) | the service's devices are done with the grant |
| `chan_watch(channel, value)` (1073) | arms the service's doorbell watch on the submission ring: if its `tail` still holds `value` (else `EAGAIN`), the client's next doorbell or its end going makes `ipc_receive` return `Event::Doorbell` (step 3) |
| `set_copy_fixup(insn, fixup)` (1074) | a fault of the program's copy instruction on granted memory resumes at `fixup` instead of killing it (`oxrt::copy`, step 3) |
| `grant_dma_pages(channel, id, first, count, &out)` (1075) | `grant_dma` for up to 512 pages in one call (step 3) |
| `chan_predecessors() -> n` (1086) | how many channels earlier processes of the caller's server attached whose clients are still there; only falls, each fall rings the doorbell ("Restarts of diskfs") |

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
in both directions, connect errors, the header read-only to the service, a shared area (what
either end stores the other reads, a futex wake of the service's meets the client's wait); grants (data both ways, read-only enforced by mprotect, by
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
| `STAT`, `STATFS` | inode / - | 0, the values packed in v0..v3 (`fsring::Stat` with the inode's generation, `Usage`) |
| `ROOT` | - | the root directory as `LOOKUP` returns an inode |
| `LOOKUP`, `CREATE`, `UNLINK`, `RENAME` | directory, names in a grant (a symlink's target or the new name right after the first name) | v0 = the inode found or made (v1 = its mode, v2 = its generation), or the one whose last link went and that someone holds (v1 = its generation; 0: none, it was freed at once) |
| `TRUNCATE`, `SETPERM` | inode and the new size / permissions | 0 |
| `RELEASE` | inode | 0: the client holds it no more (see "Holds" below) |
| `READDIR`, `READLINK` | a result buffer in a writable grant (`READDIR` with a cursor) | bytes, v0 = the next cursor |
| `FORGET` | a grant | 0 once diskfs let go of it (send before `revoke`: no draining) |
| `PROMISE` | inode, offset, `arg[0]` = length (at most 1 MiB) | 0 once the blocks a later `WRITE` of the range needs are kept for it; `ENOSPC` |

A completion is (tag, op, status, four values); every field an operation does not use must be
0 (`EINVAL`), an unknown operation is `ENOSYS`, an unknown or revoked grant `EBADF`, a range
beyond its grant `EINVAL`, an inode not in use `ENOENT`, a copy into a grant revoked under
diskfs `EFAULT` (`set_copy_fixup`).

**Ordering and durability.** Reads and writes run concurrently (at most 32 in flight in
diskfs, the device reordering them); a write waits for writes in flight on the same blocks.
Everything else is a barrier: it starts when every request taken before it (any channel)
completed (except a `FORGET` of a grant no request in flight uses, which completes at once: the
client sends one after every fill and write-back of its page cache). A completed `WRITE` is visible to every later request, also through the kernel's
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

**Promises.** A client that caches writes promises their blocks before it accepts them
(`PROMISE`, delayed allocation's reservation; `ext2fs`, "Promises"): the data blocks the range
lacks and the indirect blocks they need are counted against the free blocks (`ENOSPC` if they do
not cover them) and no other allocation takes them; the `WRITE` that comes later links them
and spends the promise. Promising again what the same channel promised costs nothing; a promise
ends when its blocks are written, truncated away or freed with the file, or when its channel
goes, and `STATFS` counts promised blocks as used. A `PROMISE` runs at once (no barrier).

**Handles.** A request names an inode by its number and its ext2 generation (`fsring::Node`,
in `object`): a handle whose inode is gone or is another file of the number now completes with
`ESTALE` and holds nothing, so no request acts on another file than the one meant (after a
restart of diskfs, say). The root's handle comes from `ROOT`.

**Holds.** A client holds every inode it named (by a valid handle) or got back from `ROOT`,
`LOOKUP`, `CREATE`, `UNLINK` or `RENAME`, until its `RELEASE` or the end of its channel. An
inode whose last link went is freed when no client holds it, so one client never frees what
another uses; while held it is on ext2's orphan list (written in an order a crash cannot hurt:
the name's removal, the inode, the list's head; off the list before it is freed), so a crash
leaves nothing allocated for good: the first diskfs after boot frees the list. (Until step 4
the kernel's IPC client held inodes too; it is gone.)

**Restarts of diskfs.** Holds are diskfs's memory, but the clients' open files outlive a diskfs
that dies. The kernel binds a channel to the server whose process attached it until its client
end goes or the service detaches it, also after that process died; a restarted diskfs asks how
many such channels of its predecessors still have clients (`chan_predecessors`, 1086) and,
while there are any, frees nothing they might hold: no inode on the orphan list at its start,
no inode that existed then whose last link goes (it frees those it created as usual). The
kernel tells each such client that the service died (`EVENT_SERVICE_GONE`); the Linux server
connects again at once, names every inode it holds (`STAT` by handle; unlinked open files
included), and only then closes its old channel. Each old channel that goes rings diskfs's
doorbell; at 0 diskfs frees what is on the orphan list and no channel holds. So an open,
unlinked file reads and writes on across a restart and goes with its last close. (A grace
period instead would free files of instances that are merely idle; the kernel knows exactly
which clients to wait for.)

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
  window (`grant(.., GRANT_WRITE | GRANT_FILL, out)`: at most 256, the run's first page, its
  length and the file's size at `out`): they become **pending**, zeroed frames pinned for the grant that nobody reads,
  maps or writes. diskfs reads into them by DMA; `mo_filled(handle, offset, pages, ok)` (1077)
  makes them the file's pages or drops them (the threads waiting for them get `EIO`, a mapping
  `SIGBUS`: a faulting thread is among a page's waiters before it asks, so the answer to its own
  request cannot pass it by; nothing is recorded where nobody waits, so a later access asks
  again) and wakes the
  waiters. No copy: the page the program maps is the page the device wrote. A grant looks at a
  bounded number of present pages per call (with interrupts off) and answers `EAGAIN` with the
  page to go on from. When the pager's thread ends, its pending pages go and their waiters fail.
  A fault keeps a reference on the page it waited for until it tried again, so reclaim cannot
  take it in between (and gives up with `SIGBUS` after 16 tries).
- **Write-back.** Writes and stores through shared mappings mark pages dirty (a mapping's page
  is writable only once dirty); an object's first dirty page sends `EVENT_DIRTY` (key).
  `grant(.., GRANT_DIRTY, out)` takes the first run of dirty pages of a window: clean from then
  on, write-protected in every mapping (a store marks them dirty again), pinned read-only for
  diskfs to write from by DMA; a write that failed puts them back with `mo_redirty` (1078). A child of
  `fork` is among the file's mappers before it gets copies of its parent's entries, so a
  write-back (or truncation) racing the fork reaches the child's copy as it reaches the
  parent's (`docs/design/page-cache.md`, reverse map). Rights raised without a fault (`mprotect`, a page kept under `PROT_NONE`
  accessed again) keep a page writable only if it was writable before, so dirty and backed;
  any other page is mapped read-only and its first store marks it dirty. The
  file's size at `out` is the one under the lock that took the dirty marks: a write makes a page
  dirty and the file longer at once, so the run's data ends there (a size read earlier would cut
  off pages a concurrent append dirtied, and lose them).
- **Disk space (delayed allocation).** A page takes data only once its disk space is secured
  (*backed*) up to the file's end: the server promised diskfs the blocks (`PROMISE`), or they
  exist. The kernel keeps, per present page, how many of its bytes are backed. The server's
  writes say `MO_CHECK_BACKED` (a page not backed ends the write there, `ENOSPC` if it is the
  first) and, after promising a range, `MO_BACKED` with the byte it is backed to: so `write(2)`
  fails with `ENOSPC` itself when the disk is full, never the write-back later. A store through
  a shared mapping into a page not backed sends `EVENT_MKWRITE` (key, offset) and waits with
  the address space unlocked, among the page's waiters from the moment it found the page
  unbacked; `mo_backed(handle, first, end, ok)` (1083) answers (no room:
  `SIGBUS`, as Linux's `page_mkwrite`; a kernel copy into the mapping, e.g. `read(2)` from a pipe,
  fails with `EFAULT` or a short count instead). A failed backing fails only the threads that
  wait for the backing, not those that only read the page. The kernel's own stores into a space no one can
  lock yet (a fork child's `CLONE_CHILD_SETTID` word) wait for the backing the same way, holding
  the space. A file that grows write-protects the page that held its
  end if that page's new file bytes are not backed, so a mapping's next store there asks
  (Linux's `pagecache_isize_extended`); truncation trims the cut page's backing. A diskfs that
  restarted lost the promises: `mo_unback(handle, from, out)` (1084) clears the pages' backing
  and returns the runs of dirty pages, which the server promises again before anyone uses the
  new channel. A final write-back that fails is logged on the console (`server_log`, 1085).
- **Memory.** The pages are cached memory (`Cached:`, `memory::cache_charge`): clean ones that
  nothing pins or maps are reclaimed when a commit or a new cache page needs room; pending and
  dirty ones are not. Dirty pages of all caches count in `Dirty:`; above a tenth of the commit
  limit, or when reclaim finds them in its way, the pagers get `EVENT_WRITEBACK` (one queued at a
  time), and above a fifth a thread that stores waits up to a second for them (Linux's dirty
  ratios; never the pager, which does the writing). `event_wait` takes a deadline (`EVENT_TIMER`,
  for the server's periodic write-back) and delivers `EVENT_CLOSING` once when the instance's
  last program is gone, for its final write-back.
- **Syncs across instances.** Each instance caches /data on its own, so `sync(2)` and `syncfs`
  ask every other instance too: `sync_others(0)` (1081) queues `EVENT_SYNC` for each other
  instance's pager and returns a ticket, the caller writes its own caches back meanwhile, and
  `sync_others(ticket)` waits until each asked pager wrote back and flushed and answered
  `sync_done(ticket)` (1082), or is gone. `reboot(2)` does the same for every instance (60 s at
  most) before the machine goes; the shutdown after the last program (`shell::settle`) too,
  then waits for the instances to end, as long as pages are still being written back or
  instances ending (ten seconds without progress, five minutes at most).
- **Faults wait unlocked.** A fault that needs a page from a pager asks for it and waits with
  the address space unlocked, then tries again (`Fault::Retry`; `MAP_POPULATE` only asks): the
  pager's write-back write-protects mappings, which locks address spaces, so no thread may wait
  for the pager while holding one.
- **msync.** `vm_sync(addr, len, flags, out, cap)` reports the cached objects mapped shared in
  the range (key, first and end page) for the server to write back with `MS_SYNC`.
- `mo_map_server(handle, pages)` / `mo_unmap_server(addr)` (1079, 1080) map a plain memory
  object into the server's region: the scratch buffer it grants diskfs for names and results.

## Paths built on it

- **Files on `/data` (R6c.3, done: step 4)**: `servers/linux/src/datafs.rs` over
  `fsclient.rs`; open files in `datafile.rs`. The server's page cache holds one cached object per
  file it uses (shared by its descriptors, mappings and programs; see "The page cache's kernel
  interface"). A miss in `read(2)` (`MO_NOFILL`: the kernel reports it) is filled by the reading
  thread itself, a fault's by the pager thread (`EVENT_PAGE`): a run of missing pages from the
  window is granted and read by diskfs straight into them (DMA), then declared filled; the
  window starts at 64 KiB and doubles to 4 MiB (four 1 MiB `READ`s in flight) while the file is
  read in order.
  `write(2)` copies into cache pages (filling first a page whose data it does not cover) and
  marks them dirty; write-back grants runs of dirty pages and sends `WRITE`s from them (up to
  16 MiB pinned per write-back, many requests in flight), `fsync`/`fdatasync`/`msync`/`sync`,
  `O_SYNC`, `O_DSYNC` and `RWF_(D)SYNC` wait for them and a `FLUSH`. The pager writes a file
  back five seconds after it got dirty (`EVENT_DIRTY`, `EVENT_TIMER`), everything when the
  kernel asks (`EVENT_WRITEBACK`) and when the instance ends (`EVENT_CLOSING`). Durability per
  `write(2)` (audit finding 8) ended: as on Linux, only those calls wait for the device.
  `O_DIRECT` reads write their range back, then read from the disk into the scratch buffer.
  Every fill and write-back sends `FORGET` before it revokes its grant (no drain; diskfs runs a
  `FORGET` of an idle grant at once, without a barrier).
- **The ring client** (`fsclient.rs`): one channel per instance shared by every thread. A
  request in flight has one of `SLOTS` slots (its tag names it), so neither ring ever overflows;
  one waiting thread at a time takes completions and hands them to their slots, waking their
  owners, and hands the job over when its own request completed. A thread holding slots never
  waits for another (`run` completes its oldest first), so slots always come free. Completions
  whose tag or operation does not match a slot in flight are dropped; every status and value is
  checked before use (a READ's bytes, a READDIR's entries, a READLINK's length). A dying diskfs,
  or one that leaves a request unanswered for `REQUEST_TIMEOUT` (60 s), fails the requests in
  flight with `EIO`; no wait of the server on diskfs or on other programs is unbounded (fills
  that write back for memory, grant scans and truncations waiting for pinned pages are capped
  too); the next request connects a new channel (diskfs is
  started again; also at once when the kernel says diskfs died, `EVENT_SERVICE_GONE`), the
  inodes in use are named again to hold them before anyone uses the new channel (unlinked open
  files too: the new diskfs kept them, "Restarts of diskfs"; one that is gone or another file
  now, `ESTALE`, is stale: `EIO`), dirty pages whose write failed are written on the new channel, and a write that
  completed but was not flushed before diskfs died makes the next `fsync` of its file report
  `EIO`.
- **Metadata** (lookup, create, unlink, rename, stat, readdir, readlink) goes through the same
  ring with names and results in a scratch buffer granted once (64 pages, mapped into the
  server's region). The server holds every inode it uses (`DInode`, one per inode number) and
  `RELEASE`s it when it lets go: an unlinked one once nothing uses it (its blocks are free when
  `unlink` or the last `close` returns), the least recently used beyond 512 cached ones. A
  release never overtakes a lookup of the same inode (a reader-writer lock orders them).
- **Sockets (R7)**: per socket a receive and a send buffer, granted to netd; TCP segments are
  copied once (NIC buffer ↔ socket buffer), as Linux does without zero-copy sockets.
- **`/proc` and `/sys` (step 5)**: see "procfs over the rings" below.

## procfs over the rings (step 5)

procfs (`servers/procfs`) serves the system-wide part of `/proc` (`stat`, `meminfo`,
`loadavg`, `uptime`, `cpuinfo`, `version`, `filesystems`, `counters`, `sys/kernel/*`) and all
of `/sys` (`devices/system/cpu`) in the **file protocol** (`fsring`), as diskfs serves
`/data`: one channel per Linux server instance (service `procfs`, `fsring::SLOTS` slots, no
shared area), the server's end the same `fsclient::Client` as diskfs's (the service and the
size of its scratch buffer are parameters: 16 pages for procfs). Before step 5 the kernel was
procfs's client (`fs/remote.rs`, IPC messages in the `fsproto` format, the server reaching
the files through the kernel's inode bridge); both are gone, with the kernel's static
`/proc` of the early boot and its mount table.

- **Read-only and stateless.** procfs answers `LOOKUP` (v0 the inode, v1 its mode; every
  inode's generation is 0, its two roots are known by number: `ROOT` is `EINVAL`), `STAT`,
  `READDIR` (cursor = entry index), `READ`, `STATFS`, `FORGET`, and `RELEASE`/`FLUSH` with
  nothing to do; whatever would change a file is `EROFS` (the server refuses those itself,
  with Linux's errors, before asking). Inode numbers name what a file is (below 1024): there
  are no holds, so a procfs that died and was started again serves the same inodes on the
  client's new channel.
- **Contents are made per `READ`.** A `READ` at offset 0 into a scratch range gets the bytes
  that fit and, in v0, the length of the whole contents as made now; the server reads on at
  the next offset only for contents beyond its scratch buffer (none of procfs's files is near
  64 KiB). The server keeps what it read in the open file description (`procfile`): reads
  from offset 0 make the contents anew, reads further on continue in what was kept (Linux's
  `seq_file`), so a file read in pieces is one snapshot and `pread(fd, .., 0)` (top, htop)
  is always current.
- **Names and results travel in the scratch grant**, copied by procfs with the copy that
  survives a revoke (`oxrt::copy`): a client that revokes its scratch under a request gets
  `EFAULT` for it; procfs drops its mapping of the grant and goes on.
- **One thread, answers at once.** procfs's loop is diskfs's without a device: it takes a
  request only while the completion ring has room, answers it on the spot (nothing waits in
  procfs), polls its rings for `SPIN_BUDGET` rounds after work, then arms every doorbell and
  sleeps in `ipc_receive`. A channel whose client went is detached in the next round.
- **No instance takes what the others need** (`procproto::admission`, host-tested): procfs
  charges before it takes. At most 2 channels per instance (the instance is the kernel's word
  in the offer) of 64 in all, so one instance leaves the others 62; at most 16 grants and 256
  pages mapped per channel (`grant_map` with the room left as `max_pages`: a larger grant is
  refused before anything is mapped, `ENOMEM` for the request, and one larger than the whole
  budget is remembered as refused until `FORGET`, so naming it again costs no kernel call; the
  server grants one scratch buffer of 16 pages); the memory of one answer at most 64 KiB whatever buffer a
  request names (a listing goes on at its cursor); requests bounded by the rings and taken
  round-robin, a few per channel per round. Nothing else outlives a request: procfs keeps no
  snapshots (the server's open files do, in the instance's own memory).
- **No instance's processes.** procfs serves only system-wide figures: the kernel gives it
  the system record alone (`proc_query`'s other queries are `EPERM`), so no client can use it
  to read another instance's processes; each instance's `/proc/<pid>` is its own server's,
  scoped by the kernel to that instance.
- **What the server makes itself.** Each process's part of `/proc` (`/proc/<pid>`, `self`,
  `thread-self`, `mounts`) is the Linux server's (`procfs.rs`), merged into procfs's root
  listing: it knows its processes (its own process table since R8), their descriptors (its
  own tables since R6e) and its mounts. See `docs/design/linux-server.md`, "/proc and /sys".

**Cost.** A `/proc/meminfo` read is one request on the ring (`READ`) against two IPC round
trips before (a `STAT` for the file type, then the `READ`); a process's own file
(`/proc/self/stat`) no request at all (the kernel's record by one kernel call). Benchmarks
`proc_meminfo_pread`, `proc_self_stat_pread`, `proc_self_stat_open_read_close`
(`docs/benchmarks/`).

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
`ESTALE` for handles of inodes not in use or of another generation, `EINVAL`, `ENAMETOOLONG`, `ENOTDIR`, `EEXIST`) with the channel
working afterwards; 24 writes and then 24 reads in flight, completions matched by tag; a grant
revoked under diskfs (`REVOKE_DRAINING`, then `EFAULT` for a copy into it, diskfs alive,
`FORGET` freeing the id, `FORGET` before `revoke` not draining) and a client closing its
channel with 16 reads in flight (diskfs serves the next one); the range of a revoked grant
given to no other grant (a copy to it `EFAULT`, the next grant untouched); an unlinked inode
one channel released still working for another that holds it, freed when that one releases
it or goes (its handle `ESTALE` then); a write stalled behind an overlapping one while another channel's long reads keep
the operation slots busy; requests waiting for room in their completion ring with diskfs using
no CPU meanwhile (its ticks in `/proc` over 500 ms), all completing once the client makes room.
The file the second scenario leaves is read through `/data` (the server's page cache, over its
own channel) and removed there. ringtest's `CHECK_GONE` checks the reservation a revoke leaves (`EACCES`
for `mprotect`, `ENOMEM` once the service unmapped it).

## Steps

1. `crates/ring`: the SPSC ring and descriptors, host-tested (single- and two-thread tests of
   the invariants, including wrap-around and the sleep/wake protocol under contention).
2. Kernel: channel objects, grants with pinning, `grant_dma`, teardown on death; shared futex
   doorbells already work (futexes on shared memory objects). Done: see "The kernel's
   interface" above.
3. diskfs: the ring protocol beside the IPC one, virtio-blk with requests in flight and DMA
   into granted pages. Done: see "The file protocol" above.
4. The Linux server: `/data` through its page cache over the ring; the bridge to the kernel's
   `/data` goes. Done: see "The page cache's kernel interface" and "Paths built on it". The
   kernel's remote store, its flusher and diskfs's IPC protocol are gone (procfs kept the
   kernel's `RemoteFs` until step 5). Each instance caches `/data` on its own: two process trees
   writing one file see each other's changes only through the disk (after write-back), for
   pages the other has not cached, as two machines sharing a disk without a lock manager would.
   Benchmarks: sequential and random reads and writes, `fstat`, against the numbers in the
   audit and Linux (`datatest` checks the semantics: no pass-through, shared pages, write-back
   and durability, truncation, concurrent readers and writers, a file larger than the memory
   left for the cache).
5. The same for procfs, and (in R7b) netd. Done: see "procfs over the rings"; the kernel's
   `RemoteFs`, its IPC client side (`ipc::call`) and the `fsproto` crate are gone: the
   kernel's IPC carries only channel offers now. The server's namespace reaches the kernel's
   tree (the inode bridge, `SYS_INODE_*`) for `/dev` alone. `proctest` checks procfs's
   files, `/sys`, the server's per-process part and its magic links; `lxtest` connects to a
   service without channels (`ringtest-plain`) for `EOPNOTSUPP`, since procfs takes them.
