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

**Cache states.** A metadata block in the cache is *clean* or *dirty* (changed by the running
transaction: never evicted, never written home before its commit). A commit writes its
blocks home right after the commit block's flush and marks them clean; their durability
comes with the next flush. `BlockCache` keeps dirty blocks pinned; since a transaction takes
at most half the cache (below), the cache stays within its capacity.

**Operations as units.** Every public operation runs inside an undo scope
(`State::begin`): the first change of each cached block keeps its old contents, and the
in-memory state (superblock, group counts, orphan list, fresh blocks, promises, revokes) is
kept as it was. An operation that fails (a logical error such as `ENOSPC`, or a read error
in the middle) is rolled back whole (`State::rollback`), so a transaction never holds part
of an operation and the filesystem goes on; there is no hand-written undo path. Data
written straight to the device by an operation that fails lands in blocks that are free
again.

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

A transaction is limited to a quarter of the journal (JBD2's rule) and to half the block
cache (so that its dirty blocks fit the server's memory); before an operation starts, a
running transaction at that limit (the ring path's links add to one without committing) is
committed. An operation that needs more stops at consistent states and commits there
(`State::pause`): a truncation or the free of a big file between freed blocks (the inode on
the orphan list meanwhile, see below), a long write or link after a run of blocks (the size
as far as the data is), a long reservation after its indirect blocks. An operation that
fails after such a commit stops the filesystem (part of it is on the disk). The first
transaction after mount also sets `RECOVER` in the superblock: the superblock as the last
commit left it (kept in memory), so nothing of the running transaction goes out early. The
log blocks are written straight from the cache (only a block that must be escaped is
copied).

**The log's tail.** The journal is a ring from `s_first` to `s_maxlen`. A transaction's space
is free once its home writes are durable, which any later flush ensures. When the next
transaction does not fit the rest of the ring, the device is flushed (every home write is
durable then) and the log is emptied: the journal superblock gets `s_start` 0 (flushed), and
the next transaction starts the log again, writing `s_start` in its own flush. On a clean
shutdown (`set_in_use(false)`, the last client gone) everything is committed and flushed, the
log emptied, and `RECOVER` goes.

**Revokes.** A block freed (a truncated indirect block, a removed directory's blocks) may
have copies in the live journal; if the block is reused for data, replaying an old copy
would destroy it. ext2fs remembers which blocks the live journal has copies of; freeing one
adds a revoke record to the running transaction. Recovery skips any copy of a revoked block
in a transaction older than the revoke.

**Recovery** (at mount, before anything else reads metadata): if the log is not empty, it is
scanned from `s_start` with `s_sequence` (descriptors, commit blocks with their checksums:
the last complete transaction ends the scan), the revoke records of complete transactions
collected, and the logged blocks of the complete transactions written home (unescaped) one
at a time as the replay finds them, oldest first, then flushed; the journal superblock gets
`s_start` 0, and the filesystem is mounted again from what the disk has. Then the orphan
list is read as before. A journal mke2fs made (no features) gets ours (`CHECKSUM`, `REVOKE`,
`ASYNC_COMMIT`) at the first read-write mount, while its log is empty.

**Untrusted journals.** The journal comes from the disk: the superblock's fields are checked
(magic, version, block size equal to the filesystem's, a ring of at least 1024 blocks that
fits inode 8, which fits the filesystem; `s_start` in the ring; no unknown incompatible
feature) before anything is read by them; the scan reads every log block at most once (and
the logged ones again for the checksum and the replay); a complete transaction naming a
block beyond the filesystem makes the journal corrupt (the mount fails, nothing written);
revokes of such blocks are ignored. What recovery keeps in memory is one entry per tag (at
most the ring's blocks) and one per revoked block (at most the filesystem's blocks), never a
count read from the log.

**Failed commits.** A transaction that cannot be written breaks the filesystem
(`broken`): nothing more is written, every change fails, and diskfs exits; its restart
replays the journal. What the old design needed to survive a failed commit is gone: the
retry gate, `after_commit`, `unlisted`, `releasing`, `deferred`, `broken_renames`, the
multi-step rename and creation; each operation is one transaction (a creation: the inode
and its name; a rename: both names, a moved directory's `..`, the parents' counts and the
replaced inode's link at once; an unlink: the name, the link, and the orphan list or the
free at once). `check_free` still checks every free (tests): in the transaction that frees
it, the inode has no links, is on no orphan list, and no entry names it.

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

Barrier operations run one at a time once no data operation is in flight; the barriers
that run together (`Ext2::batch`) are one transaction, committed after the last of them, and
their completions are posted after it (group commit): a client sees an operation complete
only when it is durable, as before, and a burst of creations or renames shares one flush. A
commit that fails turns their successes into `EIO` (and diskfs starts again). `FLUSH` commits
too (and flushes data first). A read-only device (`VIRTIO_BLK_F_RO`) is mounted read-only.

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
