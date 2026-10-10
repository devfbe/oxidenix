# The ext3 journal of the data disk

ADR 0012 decides that diskfs writes ext2's metadata through a JBD2 journal (ext3's). This is
how: the format as we write it, the transactions in `crates/ext2fs`, recovery, the orphan
list, and how diskfs batches transactions.

## On the disk

**The filesystem.** `s_feature_compat` has `HAS_JOURNAL` (0x4); `s_journal_inum` (0xE0) is 8,
`s_journal_dev` 0, `s_journal_uuid` zero (internal journal); `s_jnl_backup_type` (0xFD) is 1
and `s_jnl_blocks` (0x10C, 17 words) holds a copy of inode 8's block map, its size's high and
low words (as mke2fs writes them; e2fsck uses them if inode 8 is damaged).
`s_feature_incompat` has `RECOVER` (0x4) while the journal may hold transactions that are
not yet at their places: set before the first transaction after mount, cleared on a clean
shutdown once the journal is empty.

**The journal** is inode 8's blocks, a ring of JBD2 log blocks after its superblock (journal
block 0). Everything in it is big-endian. Each block starts with a header: magic
`0xC03B3998`, type, sequence.

- *Superblock* (type 4, version 2): block size, length (`s_maxlen`), first log block
  (`s_first` = 1), the sequence the log starts with (`s_sequence`), where it starts
  (`s_start`; 0: empty), features: compat `CHECKSUM` (1), incompat `REVOKE` (1) and
  `ASYNC_COMMIT` (4); a UUID (the filesystem's), one user.
- *Descriptor* (type 1): tags, 8 bytes each (block number, a zero checksum, flags), the
  first followed by the 16-byte UUID, the rest flagged `SAME_UUID` (2), the last `LAST_TAG`
  (8). The blocks it describes follow it in the log, in order. A block whose first word is
  the magic number is logged with that word zeroed and the tag flagged `ESCAPE` (1).
- *Revoke* (type 5): a byte count, then block numbers: copies of those blocks in this or
  earlier transactions are not replayed.
- *Commit* (type 2): checksum type 1 (CRC-32), size 4, the CRC-32 (big-endian polynomial,
  seed `!0`, no final inversion: Linux's `crc32_be`) over every descriptor and logged block
  of the transaction in log order (revoke blocks not included), and the commit time.

A transaction is its descriptors with their blocks, its revoke blocks and its commit block;
its sequence numbers count up by one. With `ASYNC_COMMIT` the whole transaction goes out
before one flush; recovery replays a transaction only with its commit block and a matching
checksum, so a torn one is never replayed.

## In ext2fs

**Cache states.** A metadata block in the cache is *clean*, *dirty* (changed by the running
transaction: never evicted, never written home before its commit), or *committed* (in a
committed transaction, its home write issued but not yet flushed: it may be evicted).
`BlockCache` keeps dirty blocks pinned.

**A transaction.** Operations change metadata in the cache. `commit` (at the end of an
operation, or of a batch: group commit) makes the dirty set a transaction:

1. Ordered data: data blocks written since the last commit (fresh blocks zeroed, writes of
   the IPC and ring paths, new directory and symlink contents) are flushed first (one flush,
   only if there is any).
2. The superblock's block, if it changed, is one of the logged blocks.
3. Descriptor blocks, the logged blocks, revoke blocks for the blocks freed in the
   transaction that the live journal has copies of, and the commit block are written at the
   journal's head; one flush.
4. The logged blocks are written home (no flush: the next commit's flush covers them) and
   become clean.

A transaction is limited to a quarter of the journal (JBD2's rule); an operation that would
need more is cut into chunks that each leave the filesystem consistent (a truncation, see
below). The first transaction after mount also sets `RECOVER` in the superblock.

**The log's tail.** The journal is a ring from `s_first` to `s_maxlen`. A transaction's space
is free once its home writes are durable, which a later flush ensures (the next commit's).
When the head would run into the tail, the tail moves to the oldest transaction whose home
writes are not known durable (after a flush if need be) and the journal superblock is
written with the new `s_start`/`s_sequence` (and flushed) before the space is reused. On a
clean shutdown (`set_in_use(false)`, the last client gone) everything is flushed, the
superblock gets `s_start` 0, and `RECOVER` goes.

**Revokes.** A block freed (a truncated indirect block, a removed directory's blocks) may
have copies in the live journal; if the block is reused for data, replaying an old copy
would destroy it. ext2fs remembers which blocks the live journal has copies of; freeing one
adds a revoke record to the running transaction. Recovery skips any copy of a revoked block
in a transaction older than the revoke.

**Recovery** (at mount, before anything else reads metadata): if `RECOVER` is set, the
journal is scanned from `s_start` with `s_sequence` (descriptors, commit blocks with their
checksums: the last complete transaction ends the scan), the revoke records collected, and
the logged blocks of the complete transactions written home (unescaped), then flushed; the
journal superblock gets `s_start` 0. Then the orphan list is read as before.

**Failed commits.** A transaction that cannot be written breaks the filesystem
(`broken`): nothing more is written, every change fails, and diskfs exits; its restart
replays the journal. What the old design needed to survive a failed commit goes: the retry
gate, `after_commit`, `unlisted`, `releasing`, `deferred` (beyond the owner's frees),
`broken_renames`; each operation is one transaction again (a rename: both names and the
links at once; an unlink: the name, the orphan list or the free at once).

**Orphans.** As ext3: an inode whose last link went while in use is put on the orphan list
in the unlink's transaction and taken off in its release's (which frees it); the first mount
after boot frees what the list has. A truncation too large for one transaction puts the inode
on the list with its new size first, frees its blocks in chunks (one transaction each), and
takes it off in the last; a mount that finds an inode with links on the list finishes its
truncation to its size (as e2fsck and Linux do).

**Read-only.** A read-only device mounts read-only: no journal is added, nothing written;
one with `RECOVER` set is refused.

**Adding a journal** (a disk without one, on its first read-write mount, as `tune2fs -j`):
the size mke2fs would choose (1024 blocks below 32768, 4096 below 256K, 8192 below 512K,
16384 below 4M, 32768 above; in blocks), allocated as inode 8's (contiguous where it can),
zeroed, the journal superblock written, inode 8 and the bitmaps written and flushed, then the
superblock's journal fields and `HAS_JOURNAL`, flushed. A crash before the last step leaves
an ext2 disk with leaked blocks (e2fsck's), never a half journal.

## In diskfs

Barrier operations already run one at a time; each round of the event loop ends with one
commit for the operations it ran, and their completions are posted after it (group commit):
a client sees an operation complete only when it is durable, as before, and a burst of
creations or renames shares flushes. `FLUSH` commits too (and flushes data first, as now).

## Tests

- Host: transactions written by ext2fs, read by `debugfs -R logdump` and replayed by
  `e2fsck -fy` on a copy (the result checked by `e2fsck -fn`); journals made by mke2fs
  replayed by ext2fs; recovery after every crash point (the crash harness: every replayed
  image consistent, `e2fsck -fn` with nothing to fix, no leaks, no link counts too high);
  revoked blocks not replayed; a journal added to an ext2 disk accepted by e2fsck; a long
  truncation cut by a crash finished at the next mount.
- The image after a self-test run: `e2fsck -fn` clean, `debugfs -R logdump` shows an empty
  journal.
- Benchmarks: sequential writes, fsync, metadata operations (creations, renames, unlinks)
  against the ordered design.
