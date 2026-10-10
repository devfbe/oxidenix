# ADR 0012: An ext3 journal for the data disk

Date: 2026-10-10. Status: accepted.

## Context

diskfs keeps ext2 consistent without a journal: every metadata change is split into steps,
each committed (written and flushed) before the next, so that no crash state has a name
pointing to a freed inode, a link count below its names, a ".." at a freed directory or an
orphan freed twice (`crates/ext2fs`, "Names and links", "Failed commits"). That works, and the
crash and failure harness shows it, but it costs:

- **Flushes.** A rename takes 4 flushes (over an existing name 9), a creation 2, a directory's
  3; ext2 without ordering takes 1.
- **Leaks.** A crash between steps leaves what only e2fsck repairs: link counts too high, an
  unattached inode, a file with two names that looks like hard links, a ".." whose parent
  counts it once too often. The disk is safe, not clean.
- **Machinery.** Steps that broke off must be finished or undone: `after_commit`, `unlisted`,
  `releasing`, `deferred`, `broken_renames`, a retry gate for failed commits; every new
  operation needs its own order and its own failure analysis.

The alternatives were a mount-time repair pass (an e2fsck of our own after every crash: the
leaks go, the flushes and the machinery stay) or a journal. The user chose the journal, in
the format Linux and e2fsprogs already know.

## Decision

1. **JBD2, as ext3 has it.** The data disk gets the `has_journal` feature with an internal
   journal (inode 8; `s_journal_inum` 8, `s_jnl_blocks` its block map, as mke2fs writes it).
   The journal speaks JBD2 version 2 (superblock type 4), with 32-bit block numbers (no
   `64bit`), revoke records (`INCOMPAT_REVOKE`), and commit-block checksums
   (`COMPAT_CHECKSUM`, CRC-32 over the transaction's log blocks) with `INCOMPAT_ASYNC_COMMIT`:
   a transaction's blocks and its commit block go out together and one flush makes the
   transaction durable; a commit block whose checksum does not match marks the transaction
   incomplete, so a torn transaction is never replayed. No `CSUM_V2/V3` (they need
   `metadata_csum`, which the filesystem does not have) and no fast commits. e2fsck replays
   and checks such a journal, and Linux's ext4 driver mounts the disk (`ext3` is ext4's).
2. **Every disk has one.** The builder makes new data disks with `mke2fs -O has_journal`. A
   disk without a journal (made before, or by a test) gets one on its first read-write mount,
   as `tune2fs -j` adds one: inode 8, the size mke2fs would choose for the disk, zeroed, the
   superblock's fields last. So there is one code path: diskfs never writes ext2 without a
   journal (only a filesystem too small for one, under 2048 blocks, stays plain ext2).
3. **One operation, one transaction; data first.** Each operation's metadata changes form a
   transaction (several operations may share one: group commit); the ordered-data rule of
   ext3's `data=ordered` stays: file data a transaction's metadata points to is written and
   flushed before the transaction's commit block. A transaction is never torn across
   operations' consistency: it ends only between operations (or between the chunks of a long
   truncation, each of which leaves the filesystem consistent, with the inode on the orphan
   list until the last, as ext3 does).
4. **Metadata goes home after the commit, the log empties when full.** After its commit, a
   transaction's blocks are written to their places (no flush of their own: the next commit's
   flush covers them). When the ring has no room for the next transaction, a flush makes every
   home write durable and the log starts again empty. A metadata block freed while copies of
   it are in the live journal gets a revoke record, so recovery never writes an old copy over
   the block's new use. The journal's superblock is written when the log starts and empties,
   and on a clean shutdown, which leaves the journal empty and the superblock without
   `needs_recovery`.
5. **Recovery at mount.** A disk with `needs_recovery` (`INCOMPAT_RECOVER`) has its journal
   replayed when it is mounted (scan, revokes, replay, as JBD2's three passes), before the
   orphan list is read. The orphan list keeps its ext3 meaning: inodes whose last link went
   while in use, freed at the first mount after boot, and inodes in a truncation that a
   crash cut short, whose truncation that mount finishes.
6. **A failed commit stops the filesystem.** A transaction that cannot be written leaves the
   cache with changes that are not durable and must not be partly written: the filesystem
   is broken (as ext3's `errors=remount-ro`), diskfs exits, and its restart replays what was
   committed. The retry gate, the step queues and the orderings that existed only to survive
   a failed commit go.
7. **Read-only disks.** A read-only device is mounted read-only (nothing is written, no
   journal is added); one that needs recovery cannot be mounted read-only (as Linux refuses
   without `noload`).

## Consequences

- A rename, a creation, an unlink is one transaction: one flush (plus a data flush when the
  transaction carries data), fewer with group commit. Every crash point leaves a disk that
  is consistent after replay: e2fsck finds nothing to fix, no leaks, no link counts too
  high. The crash harness says so for every replayed image.
- Metadata is written twice (journal, home), as on ext3.
- The journal takes space (mke2fs's choice: 4 MiB for the 64 MiB test disk, 16 MiB for the
  2 GiB data disk with its 1 KiB blocks).
- An operation that fails is undone in memory whole (no hand-written undo paths), so a
  transaction never carries part of one.
- Older e2fsprogs or kernels that do not know `ASYNC_COMMIT` refuse the disk; e2fsprogs 1.47
  and Linux 6.x know it.
- `docs/design/ext3-journal.md` has the design; `crates/ext2fs` the implementation.
