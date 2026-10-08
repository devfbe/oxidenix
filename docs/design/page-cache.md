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
- **Cached-store pages** are not committed but counted (`memory::cache_pages`). Commit keeps
  `committed + cached ≤ limit`: when a commit or a new cache page would break it, clean
  unpinned unmapped cache pages are reclaimed first (second chance: a page used since the last
  pass is skipped once). If nothing can be reclaimed, the commit fails (`ENOMEM`) and the
  pagers are asked to write back the dirty pages in the way. Dirty pages are bounded by the
  dirty ratios (`balance_dirty`: write-back asked above a tenth of the commit limit, storing
  threads waiting above a fifth).
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
