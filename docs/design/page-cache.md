# Page cache and file-backed mappings

Status: implemented (steps 4a-4d, see the end). Since R6c.3 the disk's files are the Linux
server's (its page cache, `docs/design/io-rings.md`, "The page cache's kernel interface"): the
kernel keeps their pages as *cached objects*, the server fills and writes them back over the
I/O rings; the remote store of 4c and 4d is gone. The README describes the details as built.

## Goals

- **One copy of a file page in memory.** `read`, `write`, private and shared mappings and
  `execve` all use the same physical frame for a page of a file. A shared mapping sees `write`
  at once and `read` sees stores through a shared mapping at once, across processes.
- **Programs are mapped, not copied.** `execve` maps the ELF segments from the page cache
  (demand-paged, copy-on-write), so the text of a program is in memory once however many
  processes run it, and a large program (Node.js: ~100 MB) starts without being read whole.
- **Disk files are cached.** Reads of `/data` files hit memory after the first time. (Since
  R6c.3 writes are write-back, as on Linux: `fsync` makes them durable.)
- **Shared writable file mappings write back** (`msync`, `fsync`, and in the background).
- Memory accounting stays honest: anonymous commit keeps its guarantee, and cached pages give
  way to it (reclaim).

## Model

Every regular file has a **page cache** (`fs/cache.rs`, `PageCache`): the frames of its pages
by page index, the file size, and where pages come from (the *store*):

| Store | Files | Missing page | Page can be dropped |
|---|---|---|---|
| `Memory` | tmpfs files (initramfs, `/tmp`), anonymous shared memory | read from the initramfs image while it is still valid there, else zero | never (the cache is the only copy) |
| `Paged` | the Linux server's paged objects | supplied by its pager thread (copied in) | never |
| `Cached` | the Linux server's page cache of disk files (`/data`) | filled by the server: diskfs reads into the pending page by DMA | when clean, unpinned and not mapped |

A tmpfs file is a page cache without a backing store, as on Linux (shmem). Anonymous shared
memory (`MAP_SHARED|MAP_ANONYMOUS`) is an unnamed tmpfs file, so every shared mapping is a
file mapping.

The cache owns one reference on each of its frames; every page table entry that maps the frame
owns another (frame reference counts already exist for copy-on-write). A page dropped from the
cache (truncation, reclaim) stays valid for whoever still maps it until they let go.

### Mappings

`Backing::File { inode, offset, shared }` replaces `Backing::Shared` and the old private file
copy. A mapping holds the inode, so a deleted file stays alive while mapped.

- **Shared**: the fault maps the cache frame itself. For a `Cached` store a write fault marks
  the cache page dirty (pages are mapped read-only until the first write, so the cache knows
  every dirty page). A page that must come from a pager is waited for with the address space
  unlocked, then the fault is tried again.
- **Private**: a read fault maps the cache frame read-only (copy-on-write if the area is
  writable); a write fault copies it into a private frame. Until a page is copied the mapping
  sees `write`s to the file, as on Linux.
- Access beyond the end of the file is `SIGBUS`.

### Reverse map

To truncate a mapped file and to write back a shared page, the kernel must reach the page
table entries that map a cache page. Each cache keeps the address spaces that map it
(`mappers`, weak references, registered when a file area is created and when an address space
is set up). To act on a page it locks each mapper's address space and finds the areas of this
file in it. This scales with the processes mapping a file, not with all processes.

The list is walked one mapper at a time, in the order they registered, with no cache lock held
while a mapper's address space is locked; mappers that register during a walk are visited too
(each entry has a sequence number, and a walk goes on after the last one it visited, so dead
entries can be removed at any time: by every registration and at the end of every walk; forks
in a tight loop prolong a walk until it overtakes them). `fork`
registers the child with the caches of every file area it inherits under the parent's lock,
before the child gets copies of the parent's page table entries (`Mm::fork`, holding the child's
lock until the copy is done). A truncation or write-back that the copy may have missed visits the
parent only after the copy (it needs the parent's lock) and then the child (registered later),
so the child's stale copy is cut or write-protected as the parent's is; one that visited the
parent before the copy left nothing stale to copy. The child of a fork therefore keeps a
writable entry of a dirty disk-file page writable, as its parent has it.

- **Truncation** (shrinking): drops the cache pages beyond the new end, zeroes the tail of the
  last page, then removes every mapping of the dropped range, private copies included (later
  accesses are `SIGBUS`, as on Linux).
- **Write back** of a dirty page (the Linux server's `GRANT_DIRTY`): clear its dirty bit,
  write-protect it in every mapper, then the server has it written. A store between these steps
  faults, marks the page dirty again and is written next time; none is lost.

### Locks

Order (outer to inner): address space (`Mm`, sleeping; a fork holds the parent's, then the
child's) → cache I/O lock (sleeping) → cache state (`IrqSpinLock`: pages, size) → frames. A
cache's list of mappers (`IrqSpinLock`) is innermost too: registration takes it under an address
space, and the walks (truncation, write-back) drop it before they lock a mapper.

- The **I/O lock** serializes what changes contents or size: `write` and truncation. It is
  never held while an address space is locked, so a fault (address space → I/O) cannot deadlock
  against truncation, which locks address spaces only without it. No thread waits for a pager
  while it holds an address space (a pager's write-back locks them).
- Hits take only the state spinlock (reads copy under it; faults take a frame reference under
  it), so a cached read never sleeps.
- Memory-store pages are filled under the state lock (no I/O).
- File data passes `read`/`write` through kernel buffers, so no cache lock is held while user
  memory is touched (a read into a mapping of the same file cannot deadlock).

### Memory accounting

- **tmpfs pages** are committed (`memory::commit`) when created and released when dropped, and
  tmpfs as a whole is limited to half of the commit limit, as Linux's default (`ENOSPC`, or
  `SIGBUS` for a fault in a shared mapping). Anonymous shared memory is committed whole when
  it is created, as before, and not counted again per page.
- **Cached-store pages** are not committed but counted (`memory::cache_charge`, `Cached:`),
  as on Linux, where page cache is no part of `Committed_AS`: commit keeps only `committed ≤
  limit`, and the cache lives in the frames that commitments have not claimed yet (memory
  promised but never touched, such as thread stacks, is most of what is promised: eight Node.js
  workers promise about 190 MB of a 256 MB machine and touch less than half). It gives way
  when a commitment claims a frame: an allocation for user memory (`memory::user_frame`) that
  would take the free frames below a low watermark (1/128 of RAM above the kernel's reserve,
  left for allocations that cannot reclaim: page tables made with the frames locked,
  allocations with interrupts off) first reclaims clean unpinned cache pages (second chance: a
  page used since the last pass is skipped once). Pages only the cache holds go first; then
  pages that programs map, through the reverse map (`mappers`): each address space that maps
  the file is taken if it is free (never waited for: its holder may be waiting for memory
  itself), its entries for the pages are aged by the accessed bit (cleared, the page kept) or
  removed, and a page that only the cache (and reclaim) holds afterwards is dropped. Reclaim
  holds a reference on each mapped candidate's frame during the walk, so the frame cannot be
  dropped by another reclaim and reused (say, for a private copy of the same file page) while
  it looks at the entries: an entry holding the frame can only map this page.
- **The commit guarantee.** Cache pages reclaim cannot drop now, dirty and pinned ones
  (`cache::unavailable_pages`), count against the limit as taken: `memory::commit` refuses
  what would need them, and a store that makes committed memory and them exceed the limit
  waits (killably, asking the pagers again every 100 ms) until write-back made room
  (`balance_dirty`, beside the dirty ratios: write-back asked above a tenth of the commit
  limit, storing threads waiting up to a second above a fifth). Every other cache page can be
  reclaimed, so a committed page always gets its frame: a fault that finds none reclaims again
  with its address space unlocked, so that its own mappings can go too, after a round without
  progress also pages used lately (Linux's rising reclaim priority), waiting 100 ms at a time
  for write-back or busy address spaces (Linux's reclaim throttling); after 16 fruitless tries
  (300, about 30 s, while dirty or pinned pages may still become droppable; 16 for a pager's
  own thread, which may be the one to write them) the toucher is killed.
- **Commits that only write-back stands in the way of** wait for it instead of failing: such
  a commit fails with `Fault::CommitWait` (its pages in `short_commit`; a tmpfs page's commit
  says EAGAIN, which becomes the same), and only that cause is waited for: mmap, mprotect and
  mremap (`Mm::committing`) and faults (`Mm::retrying`) wait with the address space unlocked
  (the pager may need it), killably, up to 30 s, asking the pagers, until the commit fits
  (dirty and pinned pages counted as taken), and try again; a wait that ends without room
  tries once more with waiting off, so the operation fails as it would without the cache (an
  OOM kill for a page, SIGSEGV for a stack that cannot grow, SIGBUS for tmpfs, ENOMEM for
  mmap). A tmpfs write or a grant waits in place (one deadline per call), an object mapped into
  the Linux server's region has its pages made before the region's lock. Waits happen only
  there and in the fills, never inside an allocation (it may run with any lock held; debug
  builds assert interrupts are on, so no spinlock is held).
- **The background reclaimer** (`memory::start_reclaimer`, Linux's kswapd): woken when free
  frames above the kernel's reserve come below the low watermark, it reclaims with no lock held
  until they are above twice that, so the allocations that cannot reclaim (page tables made
  with the frames locked, kernel stacks, a fork's copies, anything with interrupts off) find
  frames without direct reclaim; the kernel heap's growth asks for it through a flag the timer
  tick turns into a wakeup. It gives pages used lately their second chance: only below the low
  watermark, after a sweep of the whole cache dropped nothing, it takes them too, and when even
  that drops nothing it rests, longer each time up to a second (Linux's kswapd_failures), until
  a kick. Reclaim never drops a reference it took to an address space or a cache (its drop may
  be the teardown, which must not run inside an allocation): each goes, as the `Arc`, into a
  fixed array of 256 that the reclaimer empties (`defer_drop`), a slot taken before the
  reference; a walk with no slot left ends there. A page whose mappings one reclaim walks is
  isolated from the others (`Page::isolated`). Below the low watermark, and after a commit was
  refused at the limit, it also asks the Linux servers to give back what they can do without
  (`EVENT_SHRINK`: their heaps' free pages, unused clean `/data` inodes; linux-server.md, "The
  server's heap").
- **Reads progress** under any pressure: the kernel's read of a cached object copies from each
  page it waited for with a reference of the wait's own, so reclaim cannot take it in between;
  the Linux server falls back to such a read when the pages it filled were reclaimed before it
  could copy them. A fill takes its frames before it reserves its pins.
- **Bounded, per instance.** No wait for memory is endless or unkillable: a throttled store
  waits at most 30 s (then the committed side has its own end, above), a fill waits for frames
  as a fault does. One reclaim pass looks at no more than 4096 pages (16 per page asked for, if
  more), taking each cache's lock for at most 256 at a time and no lock across the walks of the
  mappings. No Linux server instance can hold more than its share of what reclaim cannot drop
  (`CacheCounts`): a page is charged to the instance that owns its file (whoever stored to
  it) when it is marked dirty, under the cache's lock, and refunded exactly once when it is
  cleaned, cut off or freed; a storing thread waits for write-back while the owner is above a
  tenth of the commit limit (Linux bounds each device's share likewise; each writer passes it
  by at most the page or 64 KiB chunk it just stored). Fills and write-backs reserve their
  pins against a quarter of the limit in one atomic step (`reserve_pins`) and return what they
  did not pin, so concurrent ones cannot pass it together; a grant beyond it is refused
  (EBUSY) without waiting in the kernel (the caller may hold pins of its own in flight): the
  server goes on once one of its transfers ended, or waits for the other threads' (up to 30 s
  from its latest refusal; then the fill fails as an I/O error, never as out of memory). `Writeback:` in /proc/meminfo
  counts the pinned pages.
- **Overcommit** stays strict (`overcommit_memory=2`, ratio 100%). A heuristic mode as Linux's
  default would let touches of promised memory fail at fault time; it needs a real OOM killer
  first (one that picks its victim by size, not the toucher). Each Linux server instance holds
  up to 2 MiB of commitment for its heap beyond what is mapped there, its own, when there is room
  (all together at most a 16th of the limit; a server cannot fail an allocation but by breaking
  its instance; linux-server.md, "The server's heap").
- The file metadata quota (inodes, symlink targets, pipes) on the kernel heap stays.

### Disk files

They were the kernel's (the remote store: write-through to diskfs over IPC, a flusher thread
writing shared mappings back) until R6c.3; now each is a cached object of the Linux server,
which knows the file and the disk (`servers/linux/src/datafs.rs`): the kernel keeps the pages,
the size and the dirty marks, reports missing and dirty pages, and grants runs of them to be
filled or written back by DMA. Filesystems whose files are generated on every read (procfs)
stay the kernel's and are not cached.

## Steps

1. **4a** `PageCache` with the memory store: tmpfs file contents in frames (replacing the heap
   `Data`), anonymous shared memory as unnamed tmpfs files, mappings from the cache (private
   copy-on-write, shared), the reverse map and truncation.
2. **4b** `execve` maps segments from the page cache instead of copying the program.
3. **4c** The remote store: cached reads with readahead, write-through, cached size, reclaim.
4. **4d** Shared writable mappings of remote files: dirty tracking, write-back, flusher,
   `O_DIRECT`.
5. **R6c.3** The remote store goes: disk files are the Linux server's cached objects (filled and
   written back by the server over the I/O rings), write-back replaces write-through.
