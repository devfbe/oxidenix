# ADR 0014: One page cache per file of /data for every process tree

Date: 2026-10-15. Status: accepted.

## Context

Each process tree has its own Linux server instance (ADR 0002), and until now each instance
cached `/data` on its own: a cached object of the kernel's per file and instance
(`mo_create_cached`), filled and written back over the instance's channel to diskfs (I/O rings
step 4). Two trees using one file saw each other's changes only through the disk: contents
after the writer's write-back (fsync, a `sync(2)` anywhere) and only for pages the reader had
not cached, sizes likewise, the modification time a write set only after its write-back, and
`MAP_SHARED` mappings of the same file in two trees were two copies. A file one tree unlinked
kept its blocks while any other instance cached its inode (fsring's holds), possibly for good.

The goal is what two processes on one Linux machine see of a local filesystem: a `write()` in
one tree visible to a `read()` in the other as soon as it returned (POSIX read-after-write),
sizes, names, times and link counts likewise, stores through shared mappings seen at once,
appends of several trees neither lost nor torn, and an unlinked file's blocks freed at its last
close anywhere. A crashed or hostile instance must not block the other trees for ever, and a
file used by one tree only must cost nothing extra.

## Options

1. **A coherence protocol between per-instance caches**, with diskfs as the coherence point,
   as NFSv4 delegations, Lustre and CephFS capabilities or 9P2000.L's cache modes: an instance
   caches a file's pages under a read lease, writes under an exclusive one; another instance's
   access recalls it (the holder writes its dirty pages back and drops or downgrades its
   cache); attributes and directory entries are invalidated by callbacks. It keeps the trees'
   caches apart, but every change of hands goes through the disk (a write-back, then a read),
   appends of two trees ping-pong the lease with a disk write each, and `MAP_SHARED` across
   trees needs page ownership moved at each fault, a distributed shared memory whose every
   transfer is two disk operations. Recalls make one tree wait for another's server, so each
   needs a timeout and a revocation. Much machinery, and slow exactly where trees share.
2. **diskfs owns the page cache** (the filesystem server as the pager, as Mach's external
   pagers or Fuchsia's pager-backed VMOs): one object per file made by diskfs, filled and
   written back by it. Coherent by construction, but delayed allocation, write-back, the dirty
   limits and the fault paths move into single-threaded diskfs, every fault waits for its loop,
   and a diskfs that dies takes the dirty pages of every tree with it, where today they survive
   a restart.
3. **One cached object per file for every instance, the instances stay its pagers**
   (chosen). The kernel's cached object is Linux's page cache of an inode: the system has one
   per file, and every instance that uses the file shares it. What the cache holds (pages,
   size, dirty marks, the times data changed) is coherent by construction; what it does not
   (names, link counts, which instance holds which inode) stays diskfs's, which already
   serializes it, plus one callback for what an instance holds in its own memory: a note when
   the last link of an inode it holds went. Each instance keeps filling and writing back over
   its own channel, so no tree depends on another's server for its own requests.

## Decision

1. **One object per file.** `mo_shared(channel, file, key, limit, flags, out)` (1210) finds
   the cached object of file `file` (fsring's handle: inode and generation) of the service the
   channel is connected to (scoped by the kernel's server record, which survives restarts of
   diskfs: the ids mean the same file), or with `SHARED_CREATE` makes it. The registry
   (`fs::cache::SHARED`) holds weak references; an object whose last sharer went is as good as
   none. A new object is registered before its maker asks diskfs for the size and is used by
   no one else until the maker tells it (`mo_shared_init`, 1211), as Linux's `I_NEW` inodes:
   the size read is the file's, since an object that went before had its data written back
   first. A maker that does not tell the size within a minute (`FILL_DEADLINE`) is replaced by
   the next instance that opens the file.
2. **Sharers.** Each instance that holds a handle of the object is a sharer, with its pager,
   the key it knows the object by and its counts of dirty and pinned pages. A page an instance
   wants is asked of its own pager (a fault, the kernel reading for one of its threads), which
   fills it over its own channel; any instance writes back any dirty page (fsync, sync,
   eviction, memory pressure). The instance whose store made a clean object dirty answers for
   its dirty pages: they count against its share (the per-instance dirty limit), it gets
   `EVENT_DIRTY` and the requests to write back. When it lets go of the object (its last
   handle, or its pager's end), the next sharer takes them over and is told. Pins are counted
   per pin for the instance that pinned (the grant knows its counts). The object goes with its
   last sharer.
3. **No instance holds another's thread for ever.** A pending page records its filler: only
   that instance's `mo_filled` counts for it, and a fill of another instance not finished
   within a minute is taken from it (the page goes as missing again, its frame stays pinned
   for the late grant, the waiter asks its own pager). A backing asked of another instance's
   pager and not answered within a minute is asked again. When an instance's pager ends, its
   fills in flight go (their waiters ask their own pagers) and backings are asked again.
   Joining an object whose size is being made known waits a minute at most. The append lock
   (below) is held only within one kernel call. A truncation waiting for pages another
   instance keeps pinned gives up after its wait (EBUSY), as before.
4. **Times.** The kernel sets a cached file's modification and change times when data enters
   the cache (a write, the first store to a clean page), as Linux's `file_update_time`, and
   keeps them with the object, pending until a write-back: diskfs stamps its own time on data
   when it reaches it, so whichever instance writes the file back sends the object's times
   after the data (`SETTIMES`) and clears them unless they changed meanwhile (`mo_attrs`,
   1212). `stat` overlays them, and the object's size, on diskfs's answer, from the object if
   the instance holds it, else, if diskfs says another client holds the inode (`STAT`'s
   `shared`), from the object another instance holds (`SHARED_QUERY`; if that went meanwhile,
   its data was written back first, and diskfs is asked again). `utimensat`,
   `chmod` and truncation change them there too. (A FIFO's times stay its instance's: its
   traffic asks diskfs nothing, and its pipe is the instance's own.)
5. **Appends.** `mo_file_append` (1213) finds the end and writes there in one step for every
   appender of every instance, holding the object's append lock for the call (Linux holds the
   inode lock): the whole write or nothing. The server makes the end ready first (the page
   holding it filled, the space promised) and appends again when another appender moved the
   end meanwhile; `writev`'s vectors go in as one (`MO_IOV`).
6. **Truncation and O_DIRECT** reach the object another instance holds: the server joins it
   (without making one) and cuts it, or writes its range back before reading the disk.
7. **Notes: the last link gone.** A channel to diskfs carries one page of shared area with a
   ring of notes (`fsring`, "Notes"), diskfs the producer. When the last link of an inode goes
   through one client, every other client that holds it gets a `GONE` note (so does a client
   that comes to hold an inode without links, after a restart of diskfs). The instance's
   worker thread sleeps on that ring beside its own requests (`server_wait`), marks the inode
   unlinked and lets go of it once nothing uses it, so its blocks are freed at its last close
   in any tree. A note finding the ring full waits in diskfs (a bit per inode and channel)
   until the client took notes and rang.
8. **Promises across clients.** A block one client promised needs no other's promise, and a
   client that goes passes its promises of files another client holds to that client
   (`Ext2::pass_promises`): the other writes those pages back now, and a full disk must not
   fail that write-back.

## Costs

- **A file used by one tree only:** no extra request to diskfs anywhere. Opening a file's
  object is one kernel call as before (`mo_shared` for `mo_create_cached`, the same `STAT`).
  `stat` costs what it did: diskfs's `STAT` says whether another client holds the inode, and
  only then is the registry asked (a kernel call); with an object here `mo_attrs` replaces
  `mo_file_size`. An O_DIRECT read of a file this tree does not cache asks the registry
  whether another does (one kernel call beside a disk read). An O_APPEND write takes one kernel call fewer
  (the append finds the end itself). A write takes the object's state lock once more to set
  the times, and no longer the server's lock of the inode's times. Measured: docs/benchmarks
  (`2026-10-15-*-coherence-*` against `*-base-*`, in turn on a host busy with another agent).
- **Two trees:** sharing costs nothing either: reads and writes hit the same pages; a page is
  read from the disk once for all trees. A note per unlink of an inode another tree holds.
- **Memory:** a page per channel to diskfs for the notes; per object a list of its sharers;
  per cached page 16 bytes more (its filler and since when it is filled or backed).

## Consequences

- Trees using one file see each other as processes on one machine do: writes, sizes, times,
  truncations, appends and shared mappings at once, an unlinked file's blocks freed at its last
  close anywhere. `treetest` checks each of these across two trees, and a third tree killed
  while it owns a file's dirty pages.
- An instance can change what other trees see of a file's cache, as it can change the file
  itself: every tree is root on /data (the server stores no owners). On a read-only disk an
  instance could make the others see data that is not on the disk; the trust between trees is
  that of root users on one machine.
- Dirty pages of a file whose last sharer died without writing them back are lost, as on a
  crash (a tree that ends normally writes its caches back first).
- What stays per instance: a FIFO's pipe and times, inotify (events of other trees' changes are
  not delivered), the directory snapshot of an open directory (as Linux's readdir, a listing
  begun before a change may or may not show it). `sendfile` and `copy_file_range` into an
  O_APPEND file are atomic within their tree only.
