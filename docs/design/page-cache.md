# Page cache and file-backed mappings

Status: steps 4a-4c implemented; 4d in progress (see the end).

## Goals

- **One copy of a file page in memory.** `read`, `write`, private and shared mappings and
  `execve` all use the same physical frame for a page of a file. A shared mapping sees `write`
  at once and `read` sees stores through a shared mapping at once, across processes.
- **Programs are mapped, not copied.** `execve` maps the ELF segments from the page cache
  (demand-paged, copy-on-write), so the text of a program is in memory once however many
  processes run it, and a large program (Node.js: ~100 MB) starts without being read whole.
- **Disk files are cached.** Reads of `/data` files hit memory after the first time; writes stay
  synchronous (write-through), so durability does not change.
- **Shared writable file mappings write back** (`msync`, `fsync`, and in the background).
- Memory accounting stays honest: anonymous commit keeps its guarantee, and cached pages give
  way to it (reclaim).

## Model

Every regular file has a **page cache** (`fs/cache.rs`, `PageCache`): the frames of its pages
by page index, the file size, and where pages come from (the *store*):

| Store | Files | Missing page | Page can be dropped |
|---|---|---|---|
| `Memory` | tmpfs files (initramfs, `/tmp`), anonymous shared memory | read from the initramfs image while it is still valid there, else zero | never (the cache is the only copy) |
| `Remote` | files of a filesystem server (`/data`) | read from the server, with readahead | when clean and not mapped |

A tmpfs file is a page cache without a backing store, as on Linux (shmem). Anonymous shared
memory (`MAP_SHARED|MAP_ANONYMOUS`) is an unnamed tmpfs file, so every shared mapping is a
file mapping.

The cache owns one reference on each of its frames; every page table entry that maps the frame
owns another (frame reference counts already exist for copy-on-write). A page dropped from the
cache (truncation, reclaim) stays valid for whoever still maps it until they let go.

### Mappings

`Backing::File { inode, offset, shared }` replaces `Backing::Shared` and the old private file
copy. A mapping holds the inode, so a deleted file stays alive while mapped.

- **Shared**: the fault maps the cache frame itself. For a `Remote` store a write fault marks
  the cache page dirty (pages are mapped read-only until the first write, so the cache knows
  every dirty page).
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

- **Truncation** (shrinking): drops the cache pages beyond the new end, zeroes the tail of the
  last page, then removes every mapping of the dropped range, private copies included (later
  accesses are `SIGBUS`, as on Linux).
- **Write back** of a dirty shared page: clear its dirty bit, write-protect it in every mapper,
  then write it to the server. A store between these steps faults, marks the page dirty again
  and is written next time; none is lost.

### Locks

Order (outer to inner): address space (`Mm`, sleeping) → cache I/O lock (sleeping) → cache
state (`IrqSpinLock`: pages, size) → frames.

- The **I/O lock** serializes what changes contents or size against the server: filling missing
  pages, `write`, truncation and write-back I/O. It is never held while an address space is
  locked, so a fault (address space → I/O) cannot deadlock against truncation or write-back,
  which lock address spaces only without it.
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
- **Remote cache pages** are not committed but counted (`memory::cache_pages`). Commit keeps
  `committed + cached ≤ limit`: when a commit or a new cache page would break it, clean
  unmapped cache pages are reclaimed first (second chance: a page used since the last pass is
  skipped once). If nothing can be reclaimed, the commit fails (`ENOMEM`) or the read bypasses
  the cache.
- The file metadata quota (inodes, symlink targets, pipes) on the kernel heap stays.

### Remote files

- The size is cached in the page cache (all changes go through the kernel), so cached reads
  need no request.
- A miss reads up to 64 KiB of missing pages ahead.
- `write` sends the data to the server first, then updates the cached pages it covers and
  creates those it covers whole (or that lie past the old end), under the I/O lock (a
  concurrent fill cannot insert stale data).
- The caches belong to the filesystem client (`RemoteFs`), not to the VFS inode, which goes
  with its last reference: as on Linux, pages stay cached after `close`. A cache goes when
  the server frees the inode (its number may be reused) or once reclaim emptied it and nobody
  uses it.
- Filesystems whose files are generated on every read (procfs) are not cached.
- Dirty pages of shared mappings are written back by `msync`, `fsync`/`fdatasync` and `sync`,
  and by a kernel flusher every 5 seconds. Dropping the last reference to a cache with dirty
  pages writes them back first.
- `O_DIRECT` reads and writes bypass the cache (after writing back dirty pages of the range),
  which also lets tests see what reached the disk.

## Steps

1. **4a** `PageCache` with the memory store: tmpfs file contents in frames (replacing the heap
   `Data`), anonymous shared memory as unnamed tmpfs files, mappings from the cache (private
   copy-on-write, shared), the reverse map and truncation.
2. **4b** `execve` maps segments from the page cache instead of copying the program.
3. **4c** The remote store: cached reads with readahead, write-through, cached size, reclaim.
4. **4d** Shared writable mappings of remote files: dirty tracking, write-back, flusher,
   `O_DIRECT`.
