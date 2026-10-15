//! ext2 with an ext3 journal on the data disk (any `Device`): revision 1 with the
//! `filetype` feature, 1/2/4 KiB blocks, direct and single/double/triple indirect blocks,
//! and a JBD2 journal in inode 8 (`journal.rs`; docs/design/ext3-journal.md, ADR 0012).
//!
//! **Transactions.** Metadata goes through a block cache (`cache.rs`). Every public
//! operation is one unit: all of it joins the running transaction, or, if it fails, none
//! of it (`State::rollback` undoes what it changed in the cache and in memory). The
//! transaction is committed when the operation ends (or, batched, `Ext2::batch`, when the
//! owner says: diskfs's group commit), so a completed operation is durable: its blocks
//! are written to the journal's log with a descriptor and a commit block carrying their
//! CRC-32, the device is flushed once, and then they are written to their places (the
//! next commit's flush covers those). A crash at any point leaves the filesystem as the
//! last commit left it once the journal is replayed (`mount` does that, as do e2fsck and
//! Linux): an operation is there whole or not at all, a transaction whose commit did not
//! reach the disk whole is not replayed. A freed block's copies in the log are revoked,
//! so a replay never writes old metadata over what the block holds now. The log is a
//! ring: when it is full, everything written to its place is flushed and the log starts
//! again. A transaction is bounded (a quarter of the log, as JBD2's): long operations (a
//! truncation, the free of a big file, a long write or link) commit part way at
//! consistent states (`State::pause`), a truncation on the orphan list meanwhile, so that
//! a crash's recovery finishes it. Dirty blocks stay in the cache until their transaction
//! commits, so the cache holds at most a transaction more than its capacity.
//!
//! **Ordering.** File data is read and written straight from and to the device, in runs
//! of contiguous blocks, and is flushed before the transaction whose metadata points to
//! it (ext3's `data=ordered`): file data, zeroed fresh blocks, and the data diskfs's ring
//! path wrote. A crash therefore never leaves a pointer to a block that still holds a
//! deleted file's data (tested by replaying every write and flush with arbitrary losses).
//! A write thus costs two flushes: data, then its transaction.
//!
//! **Orphans.** An inode whose last link went while a client still uses it (an open file
//! unlinked) stays allocated until `release`. So that a crash or a diskfs that dies before
//! the release does not leave it allocated for good, it is on the superblock's orphan list
//! (`s_last_orphan`, chained through the inodes' deletion-time field, as ext3's), put
//! there in the transaction that takes its last link and taken off in the one that frees
//! it. While on the list an inode's deletion time is the next orphan's number, so it is in
//! use despite a nonzero one. `mount` reads the list (trusting an entry only as e2fsck and
//! ext4 do: a number from `s_first_ino` on, allocated in the inode bitmap, a mode, no
//! links unless a regular file, not seen before; the list is cut before the first one
//! that is not), finishes the truncations a crash interrupted (a listed file with links is
//! cut to its size), and frees nothing else: whoever mounts decides with
//! `recover_orphans`. The first mount since boot frees them all (no user of them
//! survived); a restarted server keeps those its clients may still have open (diskfs,
//! "Holds"). Freeing an inode bumps its generation (as reusing its number does), so stale
//! handles stay stale.
//!
//! **Names and links.** An operation changes names and link counts in one transaction: a
//! creation's inode and name, an unlink's name and link (and an inode's free or orphan
//! listing when it was the last), a rename's new name, old name, a moved directory's ".."
//! and parents' counts, and the replaced inode's link. So no state of the disk has a name
//! of an inode the disk does not have, or more names than links, or fewer. An inode is
//! freed only in a transaction in which it has no links, is on no orphan list, and no
//! directory entry (".." included) names it (and, the owner's part, no client holds it);
//! on a device that asks for it (`Device::checks`, the tests) every free checks exactly
//! that first (`State::check_free`). Each of these operations costs one flush.
//!
//! **Failures.** An operation that fails (ENOSPC, a failed read, ...) is undone whole and
//! changes nothing. A commit that fails (a device write or flush did) stops the filesystem
//! (`broken`): every change fails from then on and nothing more is written; its owner
//! mounts again (diskfs exits and is restarted), and the replay goes on from the last
//! commit the disk has. So does an operation that fails after part of it was committed
//! (`State::pause`). A read-only device (`Device::read_only`) is mounted read-only:
//! changes fail with EROFS, nothing is written (no journal is added, and one that needs
//! recovery cannot be mounted).
//!
//! **Corruption.** The disk may be corrupt or hostile: nothing read from it panics, loops
//! or allocates by a number it holds beyond what the disk has, and no change goes through
//! metadata that is corrupt to other data. `mount` refuses a superblock or group
//! descriptors that contradict themselves or the device (a revision, first data block,
//! inode count or first inode that is wrong, descriptors that do not fit the first group,
//! a filesystem larger than the device, a group's bitmaps or inode table outside it or on
//! each other, counts beyond a group, a root that is no sound directory, a journal larger
//! than half the filesystem, with holes, a block twice or one of the metadata); the
//! superblock's free counts are the descriptors' sums (as Linux counts them). Later,
//! corruption fails the operation that needs it with EUCLEAN (ext4's `EFSCORRUPTED`) and
//! marks the filesystem as having errors (`STATE_ERROR` in `s_state`, written with the
//! next commit, `Ext2::errors`), so that e2fsck checks it; the rest goes on (ext4's
//! `errors=continue`). Checked: block pointers (ext4's `block_validity`: none into the
//! "system zone", each group's superblock copy, descriptors, bitmaps and inode table and
//! the journal: `State::is_system`), inodes before anything is done by them (a known type,
//! sizes their type allows: `State::check_inode`), directories (whole blocks, no holes, no
//! block twice: work bounded by blocks the disk has; records as ext4's
//! `ext4_check_dir_entry` checks them, names without '/', inode numbers neither reserved
//! nor beyond the inodes: `parse_entries`), an inode a name leads to (in use, with a
//! link), symlinks (their size within their form). Allocation never takes the system zone
//! or an inode reserved or in use that a bitmap calls free: that group's bitmap is corrupt
//! and allocations skip the group. Removing a corrupt file goes on past what is corrupt (a
//! pointer to the system zone or to a block not allocated is noted and not followed,
//! nothing freed twice), so a truncation walks each indirect block at most twice however
//! they are shared. What no check short of e2fsck sees stays: a block two files share, a
//! bitmap that calls a file's block free.
//!
//! **Clean.** The superblock's state (`s_state`) says "not cleanly unmounted" while the
//! filesystem is in use (`set_in_use`: diskfs, while any client is connected) and anything
//! has been changed, so after a crash a host's `e2fsck -p` (and Linux's mount) knows to
//! check it; it goes out with the first change's transaction. When it is no longer in use
//! and no orphan is owed, the state it had at mount is written back, the journal is
//! emptied and the superblock's `needs_recovery` cleared (a filesystem that was not clean
//! at mount stays so until e2fsck). A filesystem without a journal (too small for one, or
//! of revision 0) is plain ext2: its commits write in place, and a crash is e2fsck's.
//!
//! **Special files.** Sockets, FIFOs and character and block devices are inodes without
//! data (no blocks, size 0), listed with their types (`FT_SOCK`, `FT_FIFO`, `FT_CHRDEV`,
//! `FT_BLKDEV`). A device's number is in its block pointers as Linux's ext2 keeps it: a
//! major and minor below 256 each in `i_block[0]` (`old_encode_dev`), any other in
//! `i_block[1]` with `i_block[0]` 0 (`new_encode_dev`: 12 bits of major, 20 of minor);
//! the rest of the pointers are 0, as e2fsck wants them. What the filesystem takes and
//! gives is the number as `st_rdev` has it (`dev_make`).
//!
//! On a host: Linux's ext3/ext4 driver replays the journal and frees the listed inodes at
//! mount (also read-only, unless the device itself is read-only); `e2fsck -fy` does both,
//! `e2fsck -fn` reports a journal that needs recovery and a non-empty list. A cleanly
//! unmounted disk has an empty journal and an empty list.
//!
//! The ring path (`read_map`, `reserve`, `link`, `sync`) is the exception
//! to the commit per operation: there the caller moves file data between
//! the device and its client's pages itself, several requests at once,
//! and makes it durable with `sync` (write-back), under the same ordering.
//!
//! **Promises.** A client that caches writes (write-back) promises their
//! blocks first (`promise`, delayed allocation's reservation): the data
//! blocks the range lacks and the indirect blocks they will need are
//! counted against the free blocks, so the write that comes later finds
//! room, and `write(2)` can fail with ENOSPC up front instead of the
//! write-back later. Allocations a promise does not cover never take the
//! promised blocks; one it covers spends its promise. A block one owner
//! promised needs no other's promise (owners that share a cache write each
//! other's pages back). A promise ends when its blocks are allocated,
//! truncated away or freed with the file, or when its owner goes
//! (`forget_promises`), unless it passes to another owner then
//! (`pass_promises`); free space reported by `usage` excludes it.

#![no_std]

extern crate alloc;

mod blockset;
mod cache;
pub mod journal;

use blockset::BlockSet;
use cache::BlockCache;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Linux errno values used in results.
pub mod errno {
    pub const ENOENT: i64 = 2;
    pub const EIO: i64 = 5;
    pub const EAGAIN: i64 = 11;
    pub const EEXIST: i64 = 17;
    pub const ENOTDIR: i64 = 20;
    pub const EISDIR: i64 = 21;
    pub const EINVAL: i64 = 22;
    pub const EFBIG: i64 = 27;
    pub const ENOSPC: i64 = 28;
    pub const EROFS: i64 = 30;
    /// A directory has as many subdirectories as a link count may say (`LINK_MAX`).
    pub const EMLINK: i64 = 31;
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOTEMPTY: i64 = 39;
    pub const ELOOP: i64 = 40;
    /// A handle (number and generation) of an inode that is gone, or whose
    /// number is another file's now.
    pub const ESTALE: i64 = 116;
    /// The metadata an operation needs is corrupt (ext4's `EFSCORRUPTED`; see the crate's
    /// "Corruption").
    pub const EUCLEAN: i64 = 117;
}
use errno::*;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFIFO: u32 = 0o010000;
pub const NAME_MAX: usize = 255;
/// The most links an inode may have (ext2's `EXT2_LINK_MAX`): a directory with this many
/// takes no more subdirectories (EMLINK).
pub const LINK_MAX: u16 = 32000;
const SECTOR_SIZE: usize = 512;

/// The disk and the clock the filesystem lives on.
pub trait Device {
    /// Reads whole 512-byte sectors starting at `lba`.
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()>;
    /// Writes whole 512-byte sectors starting at `lba`.
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()>;
    /// Makes everything written so far durable.
    fn flush(&mut self) -> Result<(), ()>;
    /// Seconds since the Unix epoch, for timestamps.
    fn now(&self) -> u32;
    /// A random number, for new inodes' generations (so that a handle of a file cannot be
    /// guessed from its number). None by default: generations then count up.
    fn random(&mut self) -> u32 {
        0
    }
    /// Whether the device takes no writes: the filesystem is mounted read-only (nothing is
    /// written, no journal added; one that needs recovery cannot be mounted).
    fn read_only(&self) -> bool {
        false
    }
    /// Whether every free checks the filesystem's invariant first (`State::check_free`:
    /// a scan of the whole tree each time, for tests).
    fn checks(&self) -> bool {
        false
    }
}

/// What `Ext2::create` makes.
pub enum NewNode {
    File,
    Dir,
    Symlink(String),
    /// A socket's name (bind(2) of an AF_UNIX socket): an inode without
    /// data.
    Socket,
    /// A FIFO (mknod(2), mkfifo(3)): an inode without data; its pipe is the opener's.
    Fifo,
    /// A character or block device with its number (as `st_rdev` has it, `dev_make`;
    /// EINVAL for a major from 4096 or a minor from 2^20 on, which ext2 cannot keep).
    CharDev(u64),
    BlockDev(u64),
}

/// A device number as `st_rdev` has it (glibc's and musl's `makedev`, Linux's
/// `new_encode_dev` for numbers ext2 can keep).
pub fn dev_make(major: u32, minor: u32) -> u64 {
    let (major, minor) = (major as u64, minor as u64);
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

/// `dev_make`'s inverse: (major, minor).
pub fn dev_split(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// A device number's block pointers 0 and 1, as Linux's ext2 writes them
/// (`ext2_set_inode`): the old 16-bit encoding when major and minor fit a byte each, else
/// the new 32-bit one in the second pointer. EINVAL for a number ext2 cannot keep.
fn dev_to_blocks(rdev: u64) -> Result<[u32; 2], i64> {
    let (major, minor) = dev_split(rdev);
    if major >= 1 << 12 || minor >= 1 << 20 {
        return Err(EINVAL);
    }
    if major < 256 && minor < 256 {
        Ok([major << 8 | minor, 0])
    } else {
        Ok([0, (minor & 0xff) | major << 8 | (minor & !0xff) << 12])
    }
}

/// A device's number from its block pointers 0 and 1 (Linux's `ext2_iget`: the old
/// encoding if the first is not 0, else the new one).
fn dev_from_blocks(b0: u32, b1: u32) -> u64 {
    if b0 != 0 {
        dev_make(b0 >> 8 & 0xff, b0 & 0xff)
    } else {
        dev_make(b1 >> 8 & 0xfff, (b1 & 0xff) | (b1 >> 12 & 0xfff00))
    }
}

pub const ROOT_INO: u32 = 2;
const MAGIC: u16 = 0xef53;
/// `s_state`: cleanly unmounted (`EXT2_VALID_FS`).
const STATE_VALID: u16 = 1;
/// `s_state`: errors were detected (`EXT2_ERROR_FS`): e2fsck checks the filesystem.
const STATE_ERROR: u16 = 2;
/// `resize_inode`: reserved descriptor blocks (`s_reserved_gdt_blocks`) follow each copy
/// of the group descriptors.
const COMPAT_RESIZE_INODE: u32 = 0x10;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
/// The first inode number mke2fs gives files in revision 0 (`EXT2_GOOD_OLD_FIRST_INO`).
const GOOD_OLD_FIRST_INO: u32 = 11;
/// The most blocks a journal may have (mke2fs and tune2fs make none larger).
const MAX_JOURNAL_BLOCKS: u64 = 10_240_000;
const INCOMPAT_FILETYPE: u32 = 0x2;
/// `needs_recovery`: the journal may hold transactions not yet at their places.
const INCOMPAT_RECOVER: u32 = 0x4;
const COMPAT_HAS_JOURNAL: u32 = 0x4;
/// The journal's inode (`s_journal_inum`).
pub const JOURNAL_INO: u32 = 8;
const RO_COMPAT_SUPPORTED: u32 = 0x1 | 0x2; // sparse_super, large_file
const DIRECT: usize = 12;
/// Symlink targets shorter than this live in the block pointers ("fast").
const FAST_SYMLINK_MAX: usize = 60;

/// Memory for cached metadata blocks.
const CACHE_BYTES: usize = 1024 * 1024;

const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHRDEV: u8 = 3;
const FT_BLKDEV: u8 = 4;
const FT_FIFO: u8 = 5;
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

/// A directory entry's type for an inode of `mode`.
fn ftype(mode: u32) -> u8 {
    match mode & S_IFMT {
        S_IFDIR => FT_DIR,
        S_IFLNK => FT_SYMLINK,
        S_IFSOCK => FT_SOCK,
        S_IFIFO => FT_FIFO,
        S_IFCHR => FT_CHRDEV,
        S_IFBLK => FT_BLKDEV,
        _ => FT_REG,
    }
}

fn io(_: ()) -> i64 {
    EIO
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// The first 128 bytes of an on-disk inode (the part every ext2 has).
#[derive(Clone)]
pub struct RawInode([u8; 128]);

impl RawInode {
    pub fn mode(&self) -> u16 {
        le16(&self.0, 0)
    }
    fn set_mode(&mut self, v: u16) {
        put16(&mut self.0, 0, v)
    }
    fn is_dir(&self) -> bool {
        self.mode() as u32 & S_IFMT == S_IFDIR
    }
    fn is_reg(&self) -> bool {
        self.mode() as u32 & S_IFMT == S_IFREG
    }
    pub fn size(&self) -> u64 {
        let high = if self.is_reg() { le32(&self.0, 108) as u64 } else { 0 };
        le32(&self.0, 4) as u64 | high << 32
    }
    fn set_size(&mut self, v: u64) {
        put32(&mut self.0, 4, v as u32);
        if self.is_reg() {
            put32(&mut self.0, 108, (v >> 32) as u32);
        }
    }
    pub fn atime(&self) -> u32 {
        le32(&self.0, 8)
    }
    pub fn ctime(&self) -> u32 {
        le32(&self.0, 12)
    }
    pub fn mtime(&self) -> u32 {
        le32(&self.0, 16)
    }
    fn set_times(&mut self, atime: Option<u32>, mtime: Option<u32>, ctime: Option<u32>) {
        for (at, t) in [(8, atime), (16, mtime), (12, ctime)] {
            if let Some(t) = t {
                put32(&mut self.0, at, t);
            }
        }
    }
    fn touch(&mut self, now: u32, atime: bool, mtime: bool) {
        if atime {
            put32(&mut self.0, 8, now);
        }
        put32(&mut self.0, 12, now);
        if mtime {
            put32(&mut self.0, 16, now);
        }
    }
    pub fn links(&self) -> u16 {
        le16(&self.0, 26)
    }
    /// i_generation: a new one each time the inode number is given to a
    /// new file.
    pub fn generation(&self) -> u32 {
        le32(&self.0, 100)
    }
    fn set_generation(&mut self, v: u32) {
        put32(&mut self.0, 100, v)
    }
    fn set_links(&mut self, v: u16) {
        put16(&mut self.0, 26, v)
    }
    fn sectors(&self) -> u32 {
        le32(&self.0, 28)
    }
    fn add_sectors(&mut self, delta: i64) {
        let sectors = (self.sectors() as i64 + delta) as u32;
        put32(&mut self.0, 28, sectors)
    }
    fn block(&self, i: usize) -> u32 {
        le32(&self.0, 40 + 4 * i)
    }
    fn set_block(&mut self, i: usize, v: u32) {
        put32(&mut self.0, 40 + 4 * i, v)
    }
    fn fast_symlink(&self) -> bool {
        self.mode() as u32 & S_IFMT == S_IFLNK && self.sectors() == 0
    }
    /// Whether the block pointers point to blocks: a regular file's, a directory's, a
    /// slow symlink's. (A fast symlink's hold its target; a device's, its number; a
    /// socket's and a FIFO's, nothing.)
    fn has_blocks(&self) -> bool {
        match self.mode() as u32 & S_IFMT {
            S_IFREG | S_IFDIR => true,
            S_IFLNK => !self.fast_symlink(),
            _ => false,
        }
    }
    /// The deletion time (`i_dtime`); on the orphan list, the next orphan.
    fn dtime(&self) -> u32 {
        le32(&self.0, 20)
    }
}

#[derive(Clone, Copy)]
struct Group {
    block_bitmap: u32,
    inode_bitmap: u32,
    inode_table: u32,
    free_blocks: u16,
    free_inodes: u16,
    used_dirs: u16,
}

struct State<D: Device> {
    dev: D,
    block_size: usize,
    inode_size: usize,
    inodes_per_group: u32,
    blocks_per_group: u32,
    first_data_block: u32,
    blocks_count: u32,
    free_blocks: u32,
    free_inodes: u32,
    gdt_block: u32,
    groups: Vec<Group>,
    /// The blocks a group with a copy of the superblock starts with: the superblock, the
    /// group descriptors and the descriptor blocks reserved for growth (`resize_inode`).
    super_len: u32,
    /// The blocks of a group's inode table.
    table_blocks: u32,
    /// Only groups 0, 1 and powers of 3, 5 and 7 have a copy of the superblock.
    sparse_super: bool,
    /// The journal's blocks (none before it is opened): with each group's own (`super_len`,
    /// bitmaps, inode table), the blocks no file may have (ext4's "system zone").
    journal_blocks: BlockSet,
    /// Groups whose block or inode bitmap was found corrupt (it says a block of the system
    /// zone, or a reserved or used inode, is free): allocations skip them until e2fsck.
    bad_block_bitmaps: BTreeSet<usize>,
    bad_inode_bitmaps: BTreeSet<usize>,
    cache: BlockCache,
    /// What `cache` was made with (a mount after replay makes it again).
    cache_bytes: usize,
    /// The superblock as read at mount; the free counts change in it.
    sb: [u8; 1024],
    super_dirty: bool,
    /// The superblock as the disk has it (the last commit's).
    sb_on_disk: [u8; 1024],
    /// Something was written since the last flush.
    written: bool,
    /// Data blocks allocated without zeroing that were not written yet;
    /// the commit zeroes those still here before any metadata that points
    /// to them reaches the device, so no file shows a deleted file's data.
    fresh: BTreeSet<u32>,
    /// The inodes on the superblock's orphan list (see the module comment), each with the
    /// next one (0: the last): in use although their deletion time is set (it chains the
    /// list). And each one's predecessor (none for the head).
    orphans: BTreeMap<u32, u32>,
    orphan_prev: BTreeMap<u32, u32>,
    /// A commit failed (or an operation failed after part of it was committed): the
    /// cache holds changes that must never reach the disk; nothing is written any more
    /// (`Ext2::broken`), and the owner mounts again (the journal's replay).
    broken: bool,
    /// What the running operation changed, to undo it if it fails (`State::rollback`).
    undo: Option<Undo>,
    /// Operations do not commit: their owner commits a batch of them (`Ext2::batch`).
    batching: bool,
    /// A lower bound on transactions than the journal's (`Ext2::limit_transactions`).
    limit: Option<usize>,
    /// The superblock has the orphan list's field (revision 1).
    has_orphan_list: bool,
    /// `s_state` as it was at mount (`set_in_use`), with `STATE_ERROR` once corruption was
    /// found (`State::corrupt`); whether the filesystem is in use, and whether the disk
    /// says so now (not clean).
    mount_state: u16,
    in_use: bool,
    marked: bool,
    /// Data blocks reserved for writes in flight (`Ext2::reserve`): in no
    /// bitmap and no inode yet, but no allocation takes them.
    reserved: BTreeSet<u32>,
    /// The reserved blocks taken for a promised file block, by (inode,
    /// file block): until the write links them the promise still counts
    /// them (`promised`), so `avail` must not count them twice.
    reserved_promised: BTreeMap<(u32, u64), u32>,
    /// Blocks allocated for metadata (indirect, directory and symlink
    /// blocks) whose contents have not reached the device yet. Without a
    /// journal they are written and flushed before any metadata that may
    /// point to them (`barrier`): after a crash, no pointer leads to a
    /// block that still holds what a deleted file left there. (With one,
    /// they are logged with the rest of their transaction.)
    new_meta: BTreeSet<u32>,
    /// Blocks freed since the last successful commit: the metadata on the
    /// disk may still point to them, so no allocation takes them before a
    /// commit wrote the change (else a crash shows the new owner's data in
    /// the old file). Ranges: a big file frees millions of blocks at once.
    freed: BlockSet,
    /// Blocks were written that metadata may point to (file data, zeroed
    /// fresh blocks, new metadata blocks, and the data of the caller's own
    /// writes that `Ext2::link` records) and the device has not been
    /// flushed since: it is, before the transaction that points to them
    /// is committed.
    unflushed: bool,
    /// The journal (ext3's: `journal.rs`, docs/design/ext3-journal.md); None only before
    /// one is added or on a read-only mount without one.
    journal: Option<Journal>,
    /// Mounted read-only (`Device::read_only`): every change fails (EROFS).
    read_only: bool,
    /// Blocks promised to writes to come (`Ext2::promise`), by inode and
    /// owner, and their total.
    promises: BTreeMap<u32, BTreeMap<u64, Promise>>,
    promised: u64,
    /// An allocation spends a promise (`spend`): it may take promised
    /// blocks.
    spending: bool,
}

/// Writes blocks to the device, those that follow one another on the disk gathered into
/// one request (up to `Runs::MAX` bytes).
struct Runs {
    block_size: usize,
    sectors: u64,
    /// The first block of the run gathered, and its bytes.
    first: u32,
    buf: Vec<u8>,
}

impl Runs {
    const MAX: usize = 64 * 1024;

    fn new(block_size: usize, sectors: u64) -> Runs {
        Runs { block_size, sectors, first: 0, buf: Vec::new() }
    }

    fn push<D: Device + ?Sized>(&mut self, dev: &mut D, block: u32, data: &[u8]) -> Result<(), ()> {
        let count = (self.buf.len() / self.block_size) as u32;
        if !self.buf.is_empty() && (self.first.checked_add(count) != Some(block) || self.buf.len() + data.len() > Self::MAX) {
            self.finish(dev)?;
        }
        if self.buf.is_empty() {
            self.first = block;
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    fn finish<D: Device + ?Sized>(&mut self, dev: &mut D) -> Result<(), ()> {
        if !self.buf.is_empty() {
            dev.write(self.first as u64 * self.sectors, &self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// An indirect block of a file by its place in the file's tree: the
/// inode's block slot it hangs from (12, 13, 14), its height (1: it points
/// to data blocks) and its index among the tables of that height.
type TableId = (u8, u8, u64);

/// What one owner promised for one file (`Ext2::promise`).
#[derive(Default, Clone)]
struct Promise {
    /// File blocks the file has no block for yet, as ranges start -> end.
    blocks: BTreeMap<u64, u64>,
    count: u64,
    /// Indirect blocks those need that the file does not have yet.
    tables: BTreeSet<TableId>,
}

impl Promise {
    fn has(&self, fb: u64) -> bool {
        self.blocks.range(..=fb).next_back().is_some_and(|(_, &end)| fb < end)
    }

    /// Adds file block `fb` (not in it yet).
    fn add(&mut self, fb: u64) {
        let mut start = fb;
        let mut end = fb + 1;
        if let Some((&s, &e)) = self.blocks.range(..fb).next_back() {
            if e == fb {
                start = s;
            }
        }
        if let Some(&e) = self.blocks.get(&(fb + 1)) {
            self.blocks.remove(&(fb + 1));
            end = e;
        }
        self.blocks.insert(start, end);
        self.count += 1;
    }

    /// Removes file block `fb`; whether it was in it.
    fn remove(&mut self, fb: u64) -> bool {
        let Some((&s, &e)) = self.blocks.range(..=fb).next_back() else { return false };
        if fb >= e {
            return false;
        }
        self.blocks.remove(&s);
        if s < fb {
            self.blocks.insert(s, fb);
        }
        if fb + 1 < e {
            self.blocks.insert(fb + 1, e);
        }
        self.count -= 1;
        true
    }

    /// Removes the file blocks from `keep` on; how many there were.
    fn remove_from(&mut self, keep: u64) -> u64 {
        let mut gone = 0;
        if let Some((&s, &e)) = self.blocks.range(..keep).next_back() {
            if e > keep {
                self.blocks.insert(s, keep);
                gone += e - keep;
            }
        }
        let tail: Vec<(u64, u64)> = self.blocks.range(keep..).map(|(&s, &e)| (s, e)).collect();
        for (s, e) in tail {
            self.blocks.remove(&s);
            gone += e - s;
        }
        self.count -= gone;
        gone
    }

    /// Blocks it holds back.
    fn total(&self) -> u64 {
        self.count + self.tables.len() as u64
    }
}

/// What an operation changed in memory, as it was before (`State::begin`): if the
/// operation fails, all of it is undone (`State::rollback`), so that no transaction holds
/// part of an operation.
struct Undo {
    /// Metadata blocks as they were before the operation's first change of each (None: not
    /// cached), and whether they were dirty.
    blocks: BTreeMap<u32, Option<(Vec<u8>, bool)>>,
    sb: [u8; 1024],
    super_dirty: bool,
    marked: bool,
    groups: Vec<Group>,
    free_blocks: u32,
    free_inodes: u32,
    orphans: BTreeMap<u32, u32>,
    orphan_prev: BTreeMap<u32, u32>,
    fresh: BTreeSet<u32>,
    new_meta: BTreeSet<u32>,
    /// The promises of each inode whose promises changed, as they were.
    promises: BTreeMap<u32, Option<BTreeMap<u64, Promise>>>,
    promised: u64,
    reserved_promised: Vec<((u32, u64), u32)>,
    /// Blocks whose copies in the log the operation revoked.
    revoked: Vec<u32>,
    /// Part of the operation was committed (`State::pause`): its failure stops the
    /// filesystem (the part before stays, consistent, on the disk; the operation's caller
    /// is told it failed).
    paused: bool,
}

/// A truncation in progress (`State::cut_blocks`).
struct Cut {
    ino: u32,
    /// Blocks freed since the inode was last written.
    freed: i64,
    /// The inode is on the orphan list (so that a crash's recovery finishes the cut).
    listed: bool,
}

/// The journal as the mounted filesystem keeps it (docs/design/ext3-journal.md).
struct Journal {
    sb: journal::Superblock,
    /// The filesystem block of each journal block.
    map: Vec<u32>,
    /// Where the next transaction goes, and its sequence.
    head: u32,
    sequence: u32,
    /// Log blocks in use since the tail (`sb.start`; 0: none).
    used: u32,
    /// Blocks the live log has copies of: freeing one revokes them.
    logged: BTreeSet<u32>,
    /// Revokes for the running transaction.
    revokes: Vec<u32>,
    /// The superblock on the disk says `needs_recovery`.
    recovering: bool,
    /// The block holding the superblock (1 with 1 KiB blocks, else 0), and the rest of
    /// that block's bytes (the superblock is at byte 1024 of the disk).
    sb_block: u32,
    sb_block_data: Vec<u8>,
}


struct Entry {
    pos: usize,
    inode: u32,
    rec_len: usize,
    name_len: usize,
    ftype: u8,
}

/// The smallest directory record (`EXT2_DIR_REC_LEN(1)`): its header and a name's 4 bytes.
const MIN_REC_LEN: usize = 12;

/// The entries of directory block `block`, checked as ext4's `ext4_check_dir_entry` checks
/// them, and more: each record at least `MIN_REC_LEN` bytes, a multiple of 4, inside the
/// block and long enough for its name, the records covering the block exactly; a used
/// one with a name (no '/', no NUL) naming the root or an inode from `first_ino` to
/// `inodes` (no reserved one: the journal's, say). EUCLEAN otherwise (the caller notes the
/// corruption: `State::entries`).
fn parse_entries(block: &[u8], first_ino: u32, inodes: u32) -> Result<Vec<Entry>, i64> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < block.len() {
        if pos + MIN_REC_LEN > block.len() {
            return Err(EUCLEAN);
        }
        let rec_len = le16(block, pos + 4) as usize;
        let name_len = block[pos + 6] as usize;
        // (A multiple of 4 holds the name's padding too: `dir_add` takes the rest.)
        if rec_len < MIN_REC_LEN || rec_len % 4 != 0 || pos + rec_len > block.len() || 8 + name_len > rec_len {
            return Err(EUCLEAN);
        }
        let inode = le32(block, pos);
        if inode != 0 {
            let name = &block[pos + 8..pos + 8 + name_len];
            let numbered = inode == ROOT_INO || (inode >= first_ino && inode <= inodes);
            if name.is_empty() || name.iter().any(|&c| c == b'/' || c == 0) || !numbered {
                return Err(EUCLEAN);
            }
        }
        out.push(Entry { pos, inode, rec_len, name_len, ftype: block[pos + 7] });
        pos += rec_len;
    }
    Ok(out)
}

/// A name a new entry may have (what `parse_entries` takes): 1 to `NAME_MAX` bytes, no '/'
/// or NUL (EINVAL; ENOENT for an empty one, as Linux says for an empty path).
fn check_name(name: &str) -> Result<(), i64> {
    if name.is_empty() {
        return Err(ENOENT);
    }
    if name.len() > NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    if name.bytes().any(|c| c == b'/' || c == 0) {
        return Err(EINVAL);
    }
    Ok(())
}

/// Whether group `g` has a copy of the superblock and the group descriptors: every group,
/// or with `sparse_super` groups 0, 1 and the powers of 3, 5 and 7.
fn has_super(sparse: bool, g: usize) -> bool {
    let power = |p: usize| {
        let mut n = p;
        while n < g {
            n *= p;
        }
        n == g
    };
    !sparse || g <= 1 || power(3) || power(5) || power(7)
}

fn write_entry(block: &mut [u8], pos: usize, inode: u32, rec_len: usize, name: &[u8], ftype: u8) {
    put32(block, pos, inode);
    put16(block, pos + 4, rec_len as u16);
    block[pos + 6] = name.len() as u8;
    block[pos + 7] = ftype;
    block[pos + 8..pos + 8 + name.len()].copy_from_slice(name);
}

impl<D: Device> State<D> {
    fn sectors_per_block(&self) -> u64 {
        (self.block_size / SECTOR_SIZE) as u64
    }

    fn checks(&self) -> bool {
        self.dev.checks()
    }

    fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    /// The invariant every free keeps (`Device::checks`): an inode is freed only in a
    /// transaction (a cache, as it is to be committed) in which it has no links, is on no
    /// orphan list, and no directory entry (".." included) names it. Panics otherwise.
    fn check_free(&mut self, ino: u32) {
        // (A test device's failure of a read is not the check's: asked again.)
        let inode = self.read_inode(ino).or_else(|_| self.read_inode(ino)).expect("the inode");
        assert_eq!(inode.links(), 0, "freeing inode {ino} that has links");
        assert!(!self.orphans.contains_key(&ino), "freeing inode {ino} that is on the orphan list");
        // Every directory, from the root, the way lookups go.
        let (mut dirs, mut seen) = (vec![ROOT_INO], BTreeSet::new());
        while let Some(dir) = dirs.pop() {
            if !seen.insert(dir) {
                continue;
            }
            let entries = self.list(dir).or_else(|_| self.list(dir)).expect("a directory");
            for (name, entry, _) in entries {
                // ("." is the directory itself, which a name already led to.)
                if name == "." {
                    continue;
                }
                // A name, and a directory's "..": neither may point to a freed inode.
                assert_ne!(entry, ino, "freeing inode {ino} that {name} in directory {dir} names");
                // (By the inode's own type: without the filetype feature entries say none.)
                if name != ".." && self.read_inode(entry).or_else(|_| self.read_inode(entry)).is_ok_and(|i| i.is_dir()) {
                    dirs.push(entry);
                }
            }
        }
    }

    // ------------------------------------------------- operations as a unit

    /// An operation starts: what it changes in memory can be undone from here.
    fn begin(&mut self) {
        self.undo = Some(Undo {
            blocks: BTreeMap::new(),
            sb: self.sb,
            super_dirty: self.super_dirty,
            marked: self.marked,
            groups: self.groups.clone(),
            free_blocks: self.free_blocks,
            free_inodes: self.free_inodes,
            orphans: self.orphans.clone(),
            orphan_prev: self.orphan_prev.clone(),
            fresh: self.fresh.clone(),
            new_meta: self.new_meta.clone(),
            promises: BTreeMap::new(),
            promised: self.promised,
            reserved_promised: Vec::new(),
            revoked: Vec::new(),
            paused: false,
        });
    }

    /// Block `n` is about to change: the running operation keeps it as it was.
    fn save_block(&mut self, n: u32) {
        if let Some(u) = self.undo.as_mut() {
            if !u.blocks.contains_key(&n) {
                let old = self.cache.peek(n).map(|d| (d.to_vec(), self.cache.is_dirty(n)));
                u.blocks.insert(n, old);
            }
        }
    }

    /// The promises of `ino` are about to change: the running operation keeps them.
    fn save_promises(&mut self, ino: u32) {
        if let Some(u) = self.undo.as_mut() {
            if !u.promises.contains_key(&ino) {
                u.promises.insert(ino, self.promises.get(&ino).cloned());
            }
        }
    }

    /// The running operation failed: everything it changed in memory is as it was before
    /// (its blocks, counts, lists and promises), so that none of it is committed. (Blocks it
    /// freed stay out of allocations until the next commit, as any freed block; data it
    /// wrote to blocks it allocated is in free blocks again.)
    fn rollback(&mut self) {
        let Some(u) = self.undo.take() else { return };
        for (n, old) in u.blocks {
            self.cache.remove(n);
            if let Some((data, dirty)) = old {
                self.cache.insert(n, data, dirty);
            }
        }
        self.sb = u.sb;
        self.super_dirty = u.super_dirty;
        self.marked = u.marked;
        self.groups = u.groups;
        self.free_blocks = u.free_blocks;
        self.free_inodes = u.free_inodes;
        self.orphans = u.orphans;
        self.orphan_prev = u.orphan_prev;
        self.fresh = u.fresh;
        self.new_meta = u.new_meta;
        for (ino, m) in u.promises {
            match m {
                Some(m) => self.promises.insert(ino, m),
                None => self.promises.remove(&ino),
            };
        }
        self.promised = u.promised;
        self.reserved_promised.extend(u.reserved_promised);
        if let Some(j) = self.journal.as_mut() {
            for n in u.revoked {
                j.revokes.retain(|&r| r != n);
                j.logged.insert(n);
            }
        }
    }

    /// The most log blocks a transaction may take (a quarter of the log, as JBD2's
    /// `j_max_transaction_buffers`, and half the cache's capacity); without a journal, the
    /// dirty blocks the cache holds before they are written. Operations stop to commit (`pause`) when the running
    /// transaction reaches it, so a transaction, and the dirty blocks the cache holds for
    /// it, stay bounded whatever the operations do.
    fn transaction_limit(&self) -> usize {
        // (Half the cache: its dirty blocks never take more than that, so the cache stays
        // within its memory.)
        let cache = (self.cache.capacity() / 2).max(16);
        let limit = match &self.journal {
            Some(j) => ((j.sb.log_blocks() / 4) as usize).min(cache),
            None => cache,
        };
        limit.min(self.limit.unwrap_or(usize::MAX))
    }

    /// Whether the running transaction reached its limit (`transaction_limit`).
    fn over_limit(&self) -> bool {
        let revokes = self.journal.as_ref().map_or(0, |j| j.revokes.len());
        // (The superblock's block counts as one.)
        journal::transaction_len(self.block_size, self.cache.dirty_count() + 1, revokes) >= self.transaction_limit()
    }

    /// Commits the running transaction in the middle of an operation that is at a
    /// consistent state (a truncation, a long write): the operation goes on in a new one,
    /// and its failure from here on stops the filesystem (`Undo::paused`).
    fn pause(&mut self) -> Result<(), i64> {
        self.commit()?;
        if self.undo.is_some() {
            self.begin();
            self.undo.as_mut().expect("an operation").paused = true;
        }
        Ok(())
    }

    fn lba(&self, block: u32) -> u64 {
        block as u64 * self.sectors_per_block()
    }

    fn dev_write(&mut self, block: u32, buf: &[u8]) -> Result<(), i64> {
        self.written = true;
        self.dev.write(self.lba(block), buf).map_err(io)
    }

    /// Metadata block `n`, through the cache.
    fn read_block(&mut self, n: u32) -> Result<Vec<u8>, i64> {
        if let Some(data) = self.cache.get(n) {
            return Ok(data.to_vec());
        }
        let mut buf = vec![0u8; self.block_size];
        self.dev.read(self.lba(n), &mut buf).map_err(io)?;
        self.cache_insert(n, buf.clone(), false)?;
        Ok(buf)
    }

    /// Changes metadata block `n`; it reaches the device at the commit.
    fn write_block(&mut self, n: u32, buf: &[u8]) -> Result<(), i64> {
        self.save_block(n);
        self.cache_insert(n, buf.to_vec(), true)
    }

    /// Caches block `n`; the least recently used clean block gives way if the cache is
    /// full (dirty ones stay until their transaction commits, and a transaction is bounded:
    /// `transaction_limit`).
    fn cache_insert(&mut self, n: u32, data: Vec<u8>, dirty: bool) -> Result<(), i64> {
        if let Some(old) = self.cache.victim(n) {
            self.cache.remove(old);
        }
        self.cache.insert(n, data, dirty);
        Ok(())
    }

    /// Writes zeros to the fresh data blocks (see `fresh`).
    fn zero_fresh(&mut self) -> Result<(), i64> {
        while let Some(&first) = self.fresh.first() {
            let mut count = 1;
            while self.fresh.contains(&(first + count)) {
                count += 1;
            }
            let zeros = vec![0u8; count as usize * self.block_size];
            self.dev_write(first, &zeros)?;
            self.unflushed = true;
            for n in first..first + count {
                self.fresh.remove(&n);
            }
        }
        Ok(())
    }

    // ------------------------------------------- corruption (see the module comment)

    /// Corruption was found: the filesystem has errors from now on (`STATE_ERROR`, on the
    /// disk with the next commit, so that e2fsck checks it, as ext2's `ext2_error` marks
    /// it). EUCLEAN, for the operation that needed what is corrupt.
    fn corrupt(&mut self) -> i64 {
        self.mount_state |= STATE_ERROR;
        EUCLEAN
    }

    /// The first block of group `g`, and how many it has (the last may have fewer).
    fn group_first(&self, g: usize) -> u32 {
        self.first_data_block + g as u32 * self.blocks_per_group
    }

    fn group_len(&self, g: usize) -> u32 {
        self.blocks_per_group.min(self.blocks_count - self.group_first(g))
    }

    /// Whether block `b` is the filesystem's own (ext4's "system zone"): before the first
    /// data block or beyond the last, a group's copy of the superblock and descriptors,
    /// its bitmaps, its inode table, or the journal's. No file may point to one, and no
    /// allocation may take one.
    fn is_system(&self, b: u32) -> bool {
        if b < self.first_data_block || b >= self.blocks_count {
            return true;
        }
        let g = ((b - self.first_data_block) / self.blocks_per_group) as usize;
        let gr = &self.groups[g];
        (has_super(self.sparse_super, g) && b - self.group_first(g) < self.super_len)
            || b == gr.block_bitmap
            || b == gr.inode_bitmap
            || (b >= gr.inode_table && b - gr.inode_table < self.table_blocks)
            || self.journal_blocks.contains(b)
    }

    /// Block pointer `b` read from the disk: 0 (a hole) or a block a file may have (not
    /// the system zone's: `is_system`); anything else is corruption (as ext4's
    /// `block_validity`), so that no change through a pointer reaches metadata.
    fn check_block(&mut self, b: u32) -> Result<u32, i64> {
        if b != 0 && self.is_system(b) {
            return Err(self.corrupt());
        }
        Ok(b)
    }

    /// Whether block `b` (of the filesystem) is allocated in its group's bitmap.
    fn block_in_use(&mut self, b: u32) -> Result<bool, i64> {
        let rel = b - self.first_data_block;
        let g = (rel / self.blocks_per_group) as usize;
        self.test_bit(self.groups[g].block_bitmap, rel % self.blocks_per_group)
    }

    /// Checks inode `inode` (in use) before anything is done by it, as ext4's
    /// `__ext4_iget` does: a known type, and a size its type allows (a regular file's
    /// within `max_file_size`; a directory's whole blocks, at least one; a fast symlink's
    /// within its block pointers, a slow one's within its block, short of it as Linux and
    /// e2fsck want). EUCLEAN otherwise.
    fn check_inode(&mut self, inode: &RawInode) -> Result<(), i64> {
        let (size, bs) = (inode.size(), self.block_size as u64);
        let sound = match inode.mode() as u32 & S_IFMT {
            S_IFREG => size <= self.max_file_size(),
            S_IFDIR => size > 0 && size % bs == 0,
            S_IFLNK if inode.fast_symlink() => size < FAST_SYMLINK_MAX as u64,
            S_IFLNK => size > 0 && size < bs,
            S_IFSOCK | S_IFCHR | S_IFBLK | S_IFIFO => true,
            _ => false,
        };
        if !sound {
            return Err(self.corrupt());
        }
        Ok(())
    }

    /// Directory `dir`'s inode, checked (`check_inode`); ENOTDIR if it is none.
    fn dir_inode(&mut self, dir: u32) -> Result<RawInode, i64> {
        let inode = self.read_inode(dir)?;
        if !inode.is_dir() {
            return Err(ENOTDIR);
        }
        self.check_inode(&inode)?;
        Ok(inode)
    }

    /// The blocks of directory `dir` (as `inode`), in order: no hole (ext2 makes none in a
    /// directory, as ext4 checks), none twice, none of the system zone (`check_block`);
    /// EUCLEAN otherwise. So the work and memory a directory takes are bounded by blocks
    /// the disk has, each once, whatever its size says.
    fn dir_block_list(&mut self, dir: u32, inode: &mut RawInode) -> Result<Vec<u32>, i64> {
        let (mut blocks, mut seen) = (Vec::new(), BlockSet::new());
        for fb in 0..self.dir_blocks(inode) {
            let b = self.bmap(dir, inode, fb, false)?;
            // (A repeat is found at once: tables shared down a size of gigabytes end the
            // walk at the first.)
            if b == 0 || seen.contains(b) {
                return Err(self.corrupt());
            }
            seen.insert(b);
            blocks.push(b);
        }
        Ok(blocks)
    }

    /// The entries of directory block `block` (`parse_entries`: corruption noted).
    fn entries(&mut self, block: &[u8]) -> Result<Vec<Entry>, i64> {
        let (first, count) = (self.first_ino(), self.inode_count());
        parse_entries(block, first, count).map_err(|_| self.corrupt())
    }

    fn write_super(&mut self) -> Result<(), i64> {
        put32(&mut self.sb, 12, self.free_blocks);
        put32(&mut self.sb, 16, self.free_inodes);
        let now = self.dev.now();
        put32(&mut self.sb, 48, now);
        self.super_dirty = true;
        Ok(())
    }

    /// Without a journal: makes what metadata may point to durable before metadata is written:
    /// the new metadata blocks' contents (see `new_meta`), then a flush if
    /// anything such was written (see `unflushed`).
    fn barrier(&mut self) -> Result<(), i64> {
        let blocks: Vec<u32> = self.new_meta.iter().copied().collect();
        for n in blocks {
            // A block allocated but not filled yet (its allocation's own
            // metadata updates may land here) stays new until it is.
            if !self.cache.contains(n) {
                continue;
            }
            if self.cache.is_dirty(n) {
                let contents = self.cache.range(n..n + 1).next().map(|(_, d)| d.to_vec()).unwrap_or_default();
                self.dev_write(n, &contents)?;
                self.cache.mark_clean(n);
                self.unflushed = true;
            }
            self.new_meta.remove(&n);
        }
        if self.unflushed {
            self.dev.flush().map_err(io)?;
            self.unflushed = false;
        }
        Ok(())
    }

    /// Commits the running transaction (`journal_commit`; without a journal, `write_out`).
    /// A failure stops the filesystem (`broken`).
    fn commit(&mut self) -> Result<(), i64> {
        if self.broken {
            return Err(EIO);
        }
        // Corruption found (`corrupt`) goes to the disk's state with this transaction.
        let state = le16(&self.sb, 58);
        if self.mount_state & STATE_ERROR != 0 && state & STATE_ERROR == 0 {
            put16(&mut self.sb, 58, state | STATE_ERROR);
            self.super_dirty = true;
        }
        if self.journal.is_some() {
            // A transaction that cannot be written stops the filesystem (ADR 0012).
            let result = self.journal_commit();
            if result.is_err() {
                self.broken = true;
            }
            return result;
        }
        // (Without a journal, ext2's: written in place, whatever a crash leaves to e2fsck.)
        let result = self.write_out();
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    // ------------------------------------------------------------- the journal

    /// The block holding the superblock, as it is to be written.
    fn sb_block_contents(&self) -> Option<(u32, Vec<u8>)> {
        let j = self.journal.as_ref()?;
        let mut b = j.sb_block_data.clone();
        let at = if self.block_size == 1024 { 0 } else { 1024 };
        b[at..at + 1024].copy_from_slice(&self.sb);
        Some((j.sb_block, b))
    }

    /// The running transaction committed (docs/design/ext3-journal.md, "A transaction"):
    /// the data its metadata points to flushed first, then its log blocks and its commit
    /// block, one flush, then its blocks written home (the next commit's flush covers them).
    fn journal_commit(&mut self) -> Result<(), i64> {
        // Ordered data: fresh blocks zeroed and data written since the last commit reach
        // the disk before the metadata that points to them is committed.
        self.zero_fresh()?;
        // (New metadata blocks are logged with the rest: nothing to write ahead.)
        self.new_meta.clear();
        if self.unflushed {
            self.dev.flush().map_err(io)?;
            self.unflushed = false;
        }
        let blocks_dirty = self.cache.has_dirty() || self.super_dirty;
        let revokes = self.journal.as_ref().map_or(0, |j| j.revokes.len());
        if !blocks_dirty && revokes == 0 {
            self.freed.clear();
            return Ok(());
        }
        if !self.journal.as_ref().is_some_and(|j| j.recovering) {
            self.set_recovering()?;
        }
        let sb_block = if self.super_dirty { self.sb_block_contents() } else { None };
        let bs = self.block_size;
        let count = self.cache.dirty_count() + sb_block.is_some() as usize;
        let len = journal::transaction_len(bs, count, revokes) as u32;
        self.make_room(len)?;
        let now = self.dev.now() as u64;
        let sectors = self.sectors_per_block();
        // The log blocks straight from the cache (nothing copied but a block that must be
        // escaped), then its blocks home from there too.
        let State { cache, dev, journal, .. } = self;
        let j = journal.as_mut().expect("a journal");
        let mut blocks: Vec<(u32, &[u8])> = cache.dirty_blocks().collect();
        if let Some((n, data)) = &sb_block {
            blocks.push((*n, &data[..]));
        }
        let (start, starting) = (j.head, j.sb.start == 0);
        let mut at = j.head;
        // (Blocks that lie one after the other on the disk go out as one request.)
        let mut runs = Runs::new(bs, sectors);
        let mut write = |b: &[u8]| -> Result<(), ()> {
            runs.push(&mut *dev, j.map[at as usize], b)?;
            at = j.sb.next(at);
            Ok(())
        };
        journal::write_transaction(bs, &j.sb.uuid, j.sequence, &blocks, &j.revokes, now, &mut write).map_err(io)?;
        runs.finish(&mut *dev).map_err(io)?;
        if starting {
            // The log was empty: it starts at this transaction (in the same flush: a torn
            // transaction is not replayed, whatever the superblock says).
            j.sb.start = start;
            j.sb.sequence = j.sequence;
            dev.write(j.map[0] as u64 * sectors, &j.sb.encode()).map_err(io)?;
        }
        dev.flush().map_err(io)?;
        // Committed: home now (no flush of their own).
        for &(n, data) in &blocks {
            runs.push(&mut *dev, n, data).map_err(io)?;
        }
        runs.finish(&mut *dev).map_err(io)?;
        let homes: Vec<u32> = blocks.iter().map(|&(n, _)| n).collect();
        for &n in &homes {
            self.cache.mark_clean(n);
        }
        if self.super_dirty {
            self.sb_on_disk = self.sb;
        }
        self.super_dirty = false;
        self.written = true;
        let j = self.journal.as_mut().expect("a journal");
        j.head = at;
        j.sequence = j.sequence.wrapping_add(1);
        j.used += len;
        j.logged.extend(homes);
        j.revokes.clear();
        self.freed.clear();
        Ok(())
    }

    /// Room for a transaction of `len` log blocks: if the log lacks it, everything written
    /// home so far is flushed and the log starts again at the head (the journal's
    /// superblock says so, flushed, before the space is reused).
    fn make_room(&mut self, len: u32) -> Result<(), i64> {
        let j = self.journal.as_ref().expect("a journal");
        if len >= j.sb.log_blocks() {
            return Err(EFBIG);
        }
        if j.used + len < j.sb.log_blocks() {
            return Ok(());
        }
        self.dev.flush().map_err(io)?;
        self.empty_log()
    }

    /// Every transaction is at its place (the caller flushed): the log is empty from here,
    /// its superblock says so (flushed).
    fn empty_log(&mut self) -> Result<(), i64> {
        let j = self.journal.as_mut().expect("a journal");
        j.sb.start = 0;
        j.sb.sequence = j.sequence;
        j.used = 0;
        j.logged.clear();
        let jsb = j.sb.encode();
        let home = j.map[0];
        self.dev.write(self.lba(home), &jsb).map_err(io)?;
        self.dev.flush().map_err(io)
    }

    /// The superblock on the disk says `needs_recovery` before the first transaction (so
    /// that e2fsck and Linux replay the journal after a crash).
    fn set_recovering(&mut self) -> Result<(), i64> {
        let incompat = le32(&self.sb, 96);
        put32(&mut self.sb, 96, incompat | INCOMPAT_RECOVER);
        // The superblock as the last commit left it, with the flag: what the running
        // transaction changed in it must not go out before its commit.
        let mut on_disk = self.sb_on_disk;
        let incompat = le32(&on_disk, 96);
        put32(&mut on_disk, 96, incompat | INCOMPAT_RECOVER);
        self.dev.write(2, &on_disk).map_err(io)?;
        self.dev.flush().map_err(io)?;
        self.sb_on_disk = on_disk;
        self.journal.as_mut().expect("a journal").recovering = true;
        Ok(())
    }

    /// A clean stop (the last user gone, or an unmount): everything committed and flushed,
    /// the log empty, and the superblock without `needs_recovery`.
    fn quiesce(&mut self) -> Result<(), i64> {
        self.commit()?;
        if !self.journal.as_ref().is_some_and(|j| j.recovering) {
            return Ok(());
        }
        self.dev.flush().map_err(io)?;
        self.empty_log()?;
        let incompat = le32(&self.sb, 96);
        put32(&mut self.sb, 96, incompat & !INCOMPAT_RECOVER);
        let (n, b) = self.sb_block_contents().expect("a journal");
        self.dev.write(self.lba(n), &b).map_err(io)?;
        self.dev.flush().map_err(io)?;
        self.sb_on_disk = self.sb;
        self.journal.as_mut().expect("a journal").recovering = false;
        Ok(())
    }

    /// Block `n` is freed: copies of it in the live log are revoked.
    fn revoke(&mut self, n: u32) {
        if let Some(j) = self.journal.as_mut() {
            if j.logged.remove(&n) {
                j.revokes.push(n);
                if let Some(u) = self.undo.as_mut() {
                    u.revoked.push(n);
                }
            }
        }
    }

    /// The journal size mke2fs would choose for this filesystem, in blocks (None: too small
    /// for one).
    fn journal_size(&self) -> Option<u32> {
        let n = self.blocks_count;
        match n {
            0..2048 => None,
            2048..32768 => Some(1024),
            32768..262144 => Some(4096),
            262144..524288 => Some(8192),
            524288..4194304 => Some(16384),
            _ => Some(32768),
        }
    }

    /// Adds a journal (as `tune2fs -j`, docs/design/ext3-journal.md, "Adding a journal"):
    /// inode 8's blocks allocated, zeroed, the journal superblock written, inode 8 and the
    /// bitmaps committed and flushed; then the superblock's journal fields, flushed. A crash
    /// before the last step leaves ext2 with leaked blocks, never half a journal. A
    /// filesystem too small for one (or of revision 0) stays without.
    fn add_journal(&mut self) -> Result<(), &'static str> {
        let Some(len) = self.journal_size().filter(|_| self.has_orphan_list) else { return Ok(()) };
        if (self.free_blocks as u64) < len as u64 + len as u64 / 16 + 64 {
            return Ok(());
        }
        let fail = |_| "cannot add a journal";
        let old = self.read_inode(JOURNAL_INO).map_err(fail)?;
        if old.mode() != 0 {
            return Err("inode 8 is in use but the filesystem has no journal");
        }
        let mut inode = RawInode([0; 128]);
        inode.set_mode((S_IFREG | 0o600) as u16);
        inode.set_links(1);
        inode.touch(self.dev.now(), true, true);
        let bs = self.block_size;
        // Its blocks (data blocks: not zeroed in the cache, which holds only metadata),
        // each run zeroed on the device as it is allocated (a stale block could read as a
        // log block), then its superblock.
        const RUN: usize = 64;
        let zeros = vec![0u8; RUN * bs];
        let (mut first, mut run) = (0, (0u32, 0usize));
        for fb in 0..len as u64 {
            let (b, _) = self.bmap_data(JOURNAL_INO, &mut inode, fb).map_err(fail)?;
            if fb == 0 {
                first = b;
            }
            if run.1 > 0 && (b != run.0 + run.1 as u32 || run.1 == RUN) {
                self.write_data(run.0, &zeros[..run.1 * bs]).map_err(fail)?;
                run.1 = 0;
            }
            if run.1 == 0 {
                run.0 = b;
            }
            run.1 += 1;
        }
        if run.1 > 0 {
            self.write_data(run.0, &zeros[..run.1 * bs]).map_err(fail)?;
        }
        inode.set_size(len as u64 * bs as u64);
        self.write_inode(JOURNAL_INO, &inode).map_err(fail)?;
        let uuid: [u8; 16] = self.sb[104..120].try_into().unwrap();
        let jsb = journal::Superblock::new(bs as u32, len, uuid).encode();
        self.write_data(first, &jsb).map_err(fail)?;
        self.write_out().map_err(fail)?;
        // The superblock last: the journal's fields, a copy of inode 8's block map.
        let compat = le32(&self.sb, 92);
        put32(&mut self.sb, 92, compat | COMPAT_HAS_JOURNAL);
        put32(&mut self.sb, 224, JOURNAL_INO);
        put32(&mut self.sb, 228, 0);
        self.sb[208..224].fill(0);
        self.sb[0xFD] = 1;
        for i in 0..15 {
            put32(&mut self.sb, 0x10C + i * 4, le32(&inode.0, 40 + i * 4));
        }
        put32(&mut self.sb, 0x10C + 15 * 4, le32(&inode.0, 108));
        put32(&mut self.sb, 0x10C + 16 * 4, le32(&inode.0, 4));
        self.super_dirty = true;
        self.write_out().map_err(fail)?;
        self.journal = Some(self.open_journal()?);
        Ok(())
    }

    /// The features this journal is written with (`journal.rs`: checksummed, asynchronous
    /// commits, revokes), given to one made without them (mke2fs makes it so) while its
    /// log is empty.
    fn adopt_journal_features(&mut self) -> Result<(), i64> {
        let j = self.journal.as_mut().expect("a journal");
        let (compat, incompat) = (journal::COMPAT_CHECKSUM, journal::INCOMPAT_REVOKE | journal::INCOMPAT_ASYNC_COMMIT);
        if j.sb.compat & compat == compat && j.sb.incompat & incompat == incompat {
            return Ok(());
        }
        j.sb.compat |= compat;
        j.sb.incompat |= incompat;
        let (jsb, home) = (j.sb.encode(), j.map[0]);
        self.dev.write(self.lba(home), &jsb).map_err(io)?;
        self.dev.flush().map_err(io)
    }

    /// The journal of inode 8: its block map and superblock.
    fn open_journal(&mut self) -> Result<Journal, &'static str> {
        let mut inode = self.read_inode(JOURNAL_INO).map_err(|_| "cannot read the journal inode")?;
        let bs = self.block_size as u64;
        let blocks = inode.size().div_ceil(bs);
        // (Its size is the disk's word: bounded, as e2fsprogs makes journals, by half the
        // filesystem and their largest, before anything is allocated by it.)
        let most = (self.blocks_count as u64 / 2).min(MAX_JOURNAL_BLOCKS);
        if !inode.is_reg() || blocks < journal::MIN_BLOCKS as u64 || blocks > most {
            return Err("the journal inode is not a journal");
        }
        let mut map = Vec::with_capacity(blocks as usize);
        for fb in 0..blocks {
            // (Outside the system zone, the journal's own not counted yet: `check_block`.)
            let b = self.bmap(JOURNAL_INO, &mut inode, fb, false).map_err(|_| "cannot map the journal")?;
            if b == 0 {
                return Err("the journal has holes");
            }
            map.push(b);
        }
        // Its blocks join the system zone; one twice would be two places of the log.
        let mut zone = BlockSet::new();
        for &b in &map {
            if zone.contains(b) {
                return Err("the journal's blocks repeat");
            }
            zone.insert(b);
        }
        self.journal_blocks = zone;
        let mut raw = vec![0u8; self.block_size];
        self.dev.read(self.lba(map[0]), &mut raw).map_err(|_| "cannot read the journal")?;
        let sb = journal::Superblock::parse(&raw)?;
        if sb.block_size as usize != self.block_size || sb.len as u64 > blocks {
            return Err("the journal does not fit its inode");
        }
        let sb_block = if self.block_size == 1024 { 1 } else { 0 };
        let mut sb_block_data = vec![0u8; self.block_size];
        self.dev.read(self.lba(sb_block), &mut sb_block_data).map_err(|_| "cannot read the superblock")?;
        let head = if sb.start == 0 { sb.first } else { sb.start };
        Ok(Journal {
            sequence: sb.sequence,
            head,
            used: 0,
            logged: BTreeSet::new(),
            revokes: Vec::new(),
            recovering: le32(&self.sb, 96) & INCOMPAT_RECOVER != 0,
            sb_block,
            sb_block_data,
            map,
            sb,
        })
    }

    /// Replays the journal's complete transactions (their blocks home, flushed) and leaves
    /// the log empty. Whether anything was replayed.
    fn replay(&mut self, mut j: Journal) -> Result<bool, &'static str> {
        /// The journal's blocks by its map, the filesystem's home blocks, on the device.
        struct OnDevice<'a, D: Device> {
            dev: &'a mut D,
            map: &'a [u32],
            sectors: u64,
            block_size: usize,
        }
        impl<D: Device> journal::Log for OnDevice<'_, D> {
            fn read(&mut self, n: u32) -> Result<Vec<u8>, ()> {
                let mut b = vec![0u8; self.block_size];
                let at = *self.map.get(n as usize).ok_or(())? as u64 * self.sectors;
                self.dev.read(at, &mut b)?;
                Ok(b)
            }
            fn write_home(&mut self, home: u32, data: &[u8]) -> Result<(), ()> {
                self.dev.write(home as u64 * self.sectors, data)
            }
        }
        let sectors = self.sectors_per_block();
        let mut log = OnDevice { dev: &mut self.dev, map: &j.map, sectors, block_size: self.block_size };
        let rec = journal::recover(&j.sb, self.blocks_count, &mut log)?;
        self.dev.flush().map_err(|_| "cannot flush the replayed blocks")?;
        j.sb.start = 0;
        // One sequence skipped (as JBD2's recovery): blocks of a torn or stale transaction
        // the scan stopped at may carry `next_sequence` and lie where the new log starts, so
        // only a checksum would tell them from the next transaction's.
        j.sb.sequence = rec.next_sequence.wrapping_add(1);
        let jsb = j.sb.encode();
        self.dev.write(self.lba(j.map[0]), &jsb).map_err(|_| "cannot write the journal superblock")?;
        self.dev.flush().map_err(|_| "cannot flush the journal superblock")?;
        Ok(rec.transactions > 0)
    }

    /// Without a journal (and while one is added): zeroes fresh data blocks, writes the
    /// changed metadata in place (adjacent blocks in one request), then flushes the device
    /// if anything was written. The data and new blocks the metadata points to are flushed
    /// before it (`barrier`).
    fn write_out(&mut self) -> Result<(), i64> {
        self.zero_fresh()?;
        if self.super_dirty || self.cache.has_dirty() {
            self.barrier()?;
        }
        let dirty = self.cache.dirty();
        let mut i = 0;
        while i < dirty.len() {
            let mut j = i + 1;
            while j < dirty.len() && dirty[j].0 == dirty[j - 1].0 + 1 {
                j += 1;
            }
            let run: Vec<u8> = dirty[i..j].iter().flat_map(|(_, d)| d.iter().copied()).collect();
            self.dev_write(dirty[i].0, &run)?;
            for (n, _) in &dirty[i..j] {
                self.cache.mark_clean(*n);
            }
            i = j;
        }
        if self.super_dirty {
            self.written = true;
            self.dev.write(2, &self.sb).map_err(io)?;
            self.sb_on_disk = self.sb;
            self.super_dirty = false;
        }
        if self.written {
            self.dev.flush().map_err(io)?;
            self.written = false;
        }
        // The bitmaps on the disk now say what the cache says.
        self.freed.clear();
        Ok(())
    }

    /// File data block `n`: the cache's version if it has one (a block
    /// just allocated, or a tail zeroed by truncation), else the device's.
    fn data_block(&mut self, n: u32) -> Result<Vec<u8>, i64> {
        let mut buf = vec![0u8; self.block_size];
        self.read_data(n, &mut buf)?;
        Ok(buf)
    }

    /// Reads the contiguous data blocks from `first` into `buf`.
    fn read_data(&mut self, first: u32, buf: &mut [u8]) -> Result<(), i64> {
        self.dev.read(self.lba(first), buf).map_err(io)?;
        let bs = self.block_size;
        let count = (buf.len() / bs) as u32;
        for (n, data) in self.cache.range(first..first + count) {
            let at = (n - first) as usize * bs;
            buf[at..at + bs].copy_from_slice(data);
        }
        // A fresh block holds a deleted file's data until it is written or
        // zeroed (at the commit): it reads as zeros.
        for &n in self.fresh.range(first..first + count) {
            let at = (n - first) as usize * bs;
            buf[at..at + bs].fill(0);
        }
        Ok(())
    }

    /// Writes the contiguous data blocks from `first`; the device's copy
    /// is now the one that counts.
    fn write_data(&mut self, first: u32, buf: &[u8]) -> Result<(), i64> {
        let blocks = first..first + (buf.len() / self.block_size) as u32;
        self.cache.remove_range(blocks.clone());
        self.dev_write(first, buf)?;
        self.unflushed = true;
        for n in blocks {
            self.fresh.remove(&n);
        }
        Ok(())
    }

    fn write_group(&mut self, g: usize) -> Result<(), i64> {
        let byte = g * 32;
        let block = self.gdt_block + (byte / self.block_size) as u32;
        let off = byte % self.block_size;
        let mut buf = self.read_block(block)?;
        let gr = self.groups[g];
        put32(&mut buf, off, gr.block_bitmap);
        put32(&mut buf, off + 4, gr.inode_bitmap);
        put32(&mut buf, off + 8, gr.inode_table);
        put16(&mut buf, off + 12, gr.free_blocks);
        put16(&mut buf, off + 14, gr.free_inodes);
        put16(&mut buf, off + 16, gr.used_dirs);
        self.write_block(block, &buf)
    }

    fn inode_location(&self, ino: u32) -> Result<(u32, usize), i64> {
        let index = ino.checked_sub(1).ok_or(EIO)?;
        let g = (index / self.inodes_per_group) as usize;
        let group = self.groups.get(g).ok_or(EIO)?;
        let byte = (index % self.inodes_per_group) as usize * self.inode_size;
        Ok((group.inode_table + (byte / self.block_size) as u32, byte % self.block_size))
    }

    fn read_inode(&mut self, ino: u32) -> Result<RawInode, i64> {
        let (block, off) = self.inode_location(ino)?;
        let buf = self.read_block(block)?;
        Ok(RawInode(buf[off..off + 128].try_into().unwrap()))
    }

    fn write_inode(&mut self, ino: u32, inode: &RawInode) -> Result<(), i64> {
        let (block, off) = self.inode_location(ino)?;
        let mut buf = self.read_block(block)?;
        buf[off..off + 128].copy_from_slice(&inode.0);
        self.write_block(block, &buf)
    }

    /// The group of inode `ino` (a number from 1 to `inode_count`; for others an allocation
    /// goal at most, which the allocators clamp).
    fn group_of(&self, ino: u32) -> usize {
        (ino.saturating_sub(1) / self.inodes_per_group) as usize
    }

    /// A clear bit of a bitmap block below `limit`, searched from `start`
    /// on (wrapping around). For a block bitmap, `first` is the group's
    /// first block: bits of reserved blocks count as taken.
    fn find_bit(&mut self, bitmap_block: u32, limit: u32, start: u32, first: Option<u32>) -> Result<Option<u32>, i64> {
        let bitmap = self.read_block(bitmap_block)?;
        let start = if start < limit { start } else { 0 };
        for bit in (start..limit).chain(0..start) {
            let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
            let taken = first.is_some_and(|f| self.reserved.contains(&(f + bit)) || self.freed.contains(f + bit));
            if bitmap[byte] & mask == 0 && !taken {
                return Ok(Some(bit));
            }
        }
        Ok(None)
    }

    /// Sets bit `bit` of a bitmap block.
    fn set_bit(&mut self, bitmap_block: u32, bit: u32) -> Result<(), i64> {
        let mut bitmap = self.read_block(bitmap_block)?;
        bitmap[(bit / 8) as usize] |= 1u8 << (bit % 8);
        self.write_block(bitmap_block, &bitmap)
    }

    /// Clears a bit; whether it was set (a free counts only then: a bit
    /// cleared twice, which is corruption, never makes the free counts drift).
    fn clear_bit(&mut self, bitmap_block: u32, bit: u32) -> Result<bool, i64> {
        let mut bitmap = self.read_block(bitmap_block)?;
        let mask = 1u8 << (bit % 8);
        let was = bitmap[(bit / 8) as usize] & mask != 0;
        if was {
            bitmap[(bit / 8) as usize] &= !mask;
            self.write_block(bitmap_block, &bitmap)?;
        }
        Ok(was)
    }

    fn test_bit(&mut self, bitmap_block: u32, bit: u32) -> Result<bool, i64> {
        Ok(self.read_block(bitmap_block)?[(bit / 8) as usize] & 1u8 << (bit % 8) != 0)
    }

    /// Free blocks no promise and no write in flight holds (a block in
    /// flight for a promised file block counts once, as promised).
    fn avail(&self) -> u64 {
        let unpromised = self.reserved.len().saturating_sub(self.reserved_promised.len()) as u64;
        (self.free_blocks as u64).saturating_sub(unpromised + self.promised)
    }

    /// Reserved block `block` (for file block `fb` of `ino`) is linked or
    /// given back: it leaves the reserved blocks.
    fn unreserve_block(&mut self, ino: u32, fb: u64, block: u32) -> bool {
        if self.reserved_promised.get(&(ino, fb)) == Some(&block) {
            self.reserved_promised.remove(&(ino, fb));
        }
        self.reserved.remove(&block)
    }

    /// Runs an allocation that spends a promise if `promised` (it may take
    /// the promised blocks then; others only take what `avail` counts).
    fn spend<T>(&mut self, promised: bool, f: impl FnOnce(&mut Self) -> Result<T, i64>) -> Result<T, i64> {
        let before = self.spending;
        self.spending = promised;
        let result = f(self);
        self.spending = before;
        result
    }

    fn data_promised(&self, ino: u32, fb: u64) -> bool {
        self.promises.get(&ino).is_some_and(|m| m.values().any(|p| p.has(fb)))
    }

    fn table_promised(&self, ino: u32, id: TableId) -> bool {
        self.promises.get(&ino).is_some_and(|m| m.values().any(|p| p.tables.contains(&id)))
    }

    /// Drops the promises of `ino` that hold nothing any more.
    fn tidy_promises(&mut self, ino: u32) {
        if let Some(m) = self.promises.get_mut(&ino) {
            m.retain(|_, p| p.count > 0);
            if m.is_empty() {
                self.promises.remove(&ino);
            }
        }
        self.promised = self.promises.values().flat_map(|m| m.values()).map(Promise::total).sum();
    }

    /// File block `fb` of `ino` got its block: promises of it are kept.
    fn data_allocated(&mut self, ino: u32, fb: u64) {
        if !self.promises.contains_key(&ino) {
            return;
        }
        self.save_promises(ino);
        let Some(m) = self.promises.get_mut(&ino) else { return };
        let mut any = false;
        for p in m.values_mut() {
            any |= p.remove(fb);
        }
        if any {
            self.tidy_promises(ino);
        }
    }

    /// Indirect block `id` of `ino` was allocated: promises of it are kept.
    fn table_allocated(&mut self, ino: u32, id: TableId) {
        if !self.promises.contains_key(&ino) {
            return;
        }
        self.save_promises(ino);
        let Some(m) = self.promises.get_mut(&ino) else { return };
        let mut any = false;
        for p in m.values_mut() {
            any |= p.tables.remove(&id);
        }
        if any {
            self.tidy_promises(ino);
        }
    }

    /// The first file block an indirect block covers.
    fn table_start(&self, id: TableId) -> u64 {
        let p = self.ptrs_per_block();
        let base = match id.0 as usize {
            r if r == DIRECT => DIRECT as u64,
            r if r == DIRECT + 1 => DIRECT as u64 + p,
            _ => DIRECT as u64 + p + p * p,
        };
        base + id.2 * p.pow(id.1 as u32)
    }

    /// `ino` was cut to `keep` blocks: promises beyond go.
    fn unpromise_from(&mut self, ino: u32, keep: u64) {
        if !self.promises.contains_key(&ino) {
            return;
        }
        self.save_promises(ino);
        let Some(mut m) = self.promises.remove(&ino) else { return };
        for p in m.values_mut() {
            p.remove_from(keep);
            p.tables.retain(|&id| self.table_start(id) < keep);
        }
        self.promises.insert(ino, m);
        self.tidy_promises(ino);
        // Blocks in flight for what went count as reserved now (only a
        // promise's end leaves such blocks: a link takes its block out of
        // `reserved_promised` before it spends the promise).
        let gone: Vec<(u32, u64)> = self
            .reserved_promised
            .range((ino, keep)..=(ino, u64::MAX))
            .map(|(&k, _)| k)
            .filter(|&(i, fb)| !self.data_promised(i, fb))
            .collect();
        for k in gone {
            if let Some(b) = self.reserved_promised.remove(&k) {
                if let Some(u) = self.undo.as_mut() {
                    u.reserved_promised.push((k, b));
                }
            }
        }
    }

    /// Whether file block `fb` has a data block, and the indirect blocks
    /// missing on its way (top first).
    fn probe(&mut self, inode: &RawInode, fb: u64) -> Result<(bool, Vec<TableId>), i64> {
        if (fb as usize) < DIRECT {
            return Ok((self.check_block(inode.block(fb as usize))? != 0, Vec::new()));
        }
        let p = self.ptrs_per_block();
        let mut rel = fb - DIRECT as u64;
        let (root, depth) = if rel < p {
            (DIRECT, 1)
        } else if rel - p < p * p {
            rel -= p;
            (DIRECT + 1, 2)
        } else if rel - p - p * p < p * p * p {
            rel -= p + p * p;
            (DIRECT + 2, 3)
        } else {
            return Err(EFBIG);
        };
        let mut missing = Vec::new();
        let mut blk = self.check_block(inode.block(root))?;
        for h in (1..=depth).rev() {
            if blk == 0 {
                missing.push((root as u8, h as u8, rel / p.pow(h)));
                continue;
            }
            let table = self.read_block(blk)?;
            blk = self.check_block(le32(&table, ((rel / p.pow(h - 1)) % p) as usize * 4))?;
        }
        Ok((blk != 0, missing))
    }

    fn promise(&mut self, owner: u64, ino: u32, off: u64, len: u64) -> Result<(), i64> {
        let inode = self.live_inode(ino)?;
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        if len == 0 {
            return Ok(());
        }
        let end = off.checked_add(len).filter(|&end| end <= self.max_file_size()).ok_or(EFBIG)?;
        let bs = self.block_size as u64;
        let mut data = Vec::new();
        let mut tables = BTreeSet::new();
        for fb in off / bs..=(end - 1) / bs {
            // (Promised by any owner: one promise per block, `pass_promises` keeps it.)
            if self.data_promised(ino, fb) {
                continue;
            }
            let (allocated, missing) = self.probe(&inode, fb)?;
            if allocated {
                continue;
            }
            for id in missing {
                if !self.table_promised(ino, id) {
                    tables.insert(id);
                }
            }
            data.try_reserve(1).map_err(|_| ENOSPC)?;
            data.push(fb);
        }
        if (data.len() + tables.len()) as u64 > self.avail() {
            return Err(ENOSPC);
        }
        let p = self.promises.entry(ino).or_default().entry(owner).or_default();
        for fb in data {
            p.add(fb);
        }
        p.tables.extend(tables);
        self.tidy_promises(ino);
        Ok(())
    }

    /// Allocates a block, preferring group `goal`; zeroed unless it is for
    /// file data the caller writes. Only free blocks no promise holds,
    /// unless the allocation spends one (`spend`).
    fn alloc_block(&mut self, goal: usize, zero: bool) -> Result<u32, i64> {
        if !self.spending && self.avail() == 0 {
            return Err(ENOSPC);
        }
        let n = self.groups.len();
        let goal = if goal < n { goal } else { 0 };
        for g in (goal..n).chain(0..goal) {
            if self.groups[g].free_blocks == 0 || self.bad_block_bitmaps.contains(&g) {
                continue;
            }
            let (first, limit) = (self.group_first(g), self.group_len(g));
            let bitmap = self.groups[g].block_bitmap;
            let Some(bit) = self.find_bit(bitmap, limit, 0, Some(first))? else { continue };
            let block = first + bit;
            if self.system_free(g, block) {
                continue;
            }
            self.set_bit(bitmap, bit)?;
            self.groups[g].free_blocks -= 1;
            self.free_blocks = self.free_blocks.saturating_sub(1);
            self.write_group(g)?;
            self.write_super()?;
            // Metadata unless `alloc_data` makes it a fresh data block.
            self.new_meta.insert(block);
            if zero {
                self.write_block(block, &vec![0u8; self.block_size])?;
            }
            return Ok(block);
        }
        Err(ENOSPC)
    }

    /// Whether free block `block` of group `g`'s bitmap is the system zone's: the bitmap is
    /// corrupt, and the group takes no more allocations (as ext4 marks a block bitmap
    /// corrupt), so that no file is given metadata.
    fn system_free(&mut self, g: usize, block: u32) -> bool {
        if !self.is_system(block) {
            return false;
        }
        self.bad_block_bitmaps.insert(g);
        self.corrupt();
        true
    }

    /// Frees block `block`; whether it was the file's (allocated). One of the system zone
    /// (`is_system`), or not allocated (freed twice: two pointers to it, or never
    /// allocated), is corruption: noted (`corrupt`), nothing freed or counted (as ext4
    /// frees no block outside the data zone), so that a corrupt file can still be removed.
    fn free_block(&mut self, block: u32) -> Result<bool, i64> {
        if self.is_system(block) {
            self.corrupt();
            return Ok(false);
        }
        self.revoke(block);
        // (A data block is not cached: nothing to keep.)
        if self.cache.contains(block) {
            self.save_block(block);
            self.cache.remove(block);
        }
        self.fresh.remove(&block);
        self.new_meta.remove(&block);
        // Still pointed to on the disk until the next commit: not reused before.
        self.freed.insert(block);
        let rel = block - self.first_data_block;
        let g = (rel / self.blocks_per_group) as usize;
        if !self.clear_bit(self.groups[g].block_bitmap, rel % self.blocks_per_group)? {
            self.corrupt();
            return Ok(false);
        }
        // (Counts the disk had wrong stay within their group and the filesystem.)
        let len = self.group_len(g) as u16;
        self.groups[g].free_blocks = self.groups[g].free_blocks.saturating_add(1).min(len);
        self.free_blocks = self.free_blocks.saturating_add(1).min(self.blocks_count);
        self.write_group(g)?;
        self.write_super()?;
        Ok(true)
    }

    /// Allocates an inode, preferring group `goal`. A bitmap that says a reserved inode or
    /// one in use (a mode, links and no deletion time, or on the orphan list) is free is
    /// corrupt (as ext4's "reserved inode found cleared"): its group gives no more inodes,
    /// so that no file's inode is given to another.
    fn alloc_inode(&mut self, dir: bool, goal: usize) -> Result<u32, i64> {
        let n = self.groups.len();
        let goal = if goal < n { goal } else { 0 };
        for g in (goal..n).chain(0..goal) {
            if self.groups[g].free_inodes == 0 || self.bad_inode_bitmaps.contains(&g) {
                continue;
            }
            let bitmap = self.groups[g].inode_bitmap;
            let Some(bit) = self.find_bit(bitmap, self.inodes_per_group, 0, None)? else { continue };
            let ino = g as u32 * self.inodes_per_group + bit + 1;
            let old = self.read_inode(ino)?;
            let used = old.mode() != 0 && old.links() != 0 && old.dtime() == 0;
            if ino < self.first_ino() || used || self.orphans.contains_key(&ino) {
                self.bad_inode_bitmaps.insert(g);
                self.corrupt();
                continue;
            }
            self.set_bit(bitmap, bit)?;
            self.groups[g].free_inodes -= 1;
            if dir {
                self.groups[g].used_dirs = self.groups[g].used_dirs.saturating_add(1).min(self.inodes_per_group as u16);
            }
            self.free_inodes = self.free_inodes.saturating_sub(1);
            self.write_group(g)?;
            self.write_super()?;
            return Ok(ino);
        }
        Err(ENOSPC)
    }

    fn free_inode(&mut self, ino: u32, dir: bool) -> Result<(), i64> {
        let g = self.group_of(ino);
        if !self.clear_bit(self.groups[g].inode_bitmap, (ino - 1) % self.inodes_per_group)? {
            self.corrupt();
            return Ok(());
        }
        let ipg = self.inodes_per_group as u16;
        self.groups[g].free_inodes = self.groups[g].free_inodes.saturating_add(1).min(ipg);
        if dir {
            self.groups[g].used_dirs = self.groups[g].used_dirs.saturating_sub(1);
        }
        self.free_inodes = self.free_inodes.saturating_add(1).min(self.inode_count());
        self.write_group(g)?;
        self.write_super()
    }

    fn ptrs_per_block(&self) -> u64 {
        (self.block_size / 4) as u64
    }

    /// Largest size the direct and indirect block pointers can address.
    fn max_file_size(&self) -> u64 {
        let p = self.ptrs_per_block();
        (DIRECT as u64 + p + p * p + p * p * p) * self.block_size as u64
    }

    /// Disk block holding file block `fb`, allocating it (zeroed, and any
    /// indirect blocks on the way) if `alloc`. Returns 0 for a hole when not
    /// allocating.
    fn bmap(&mut self, ino: u32, inode: &mut RawInode, fb: u64, alloc: bool) -> Result<u32, i64> {
        let leaf = if alloc { Some(true) } else { None };
        self.map(ino, inode, fb, leaf).map(|(b, _)| b)
    }

    /// Like `bmap` with allocation, for file data the caller writes: a new
    /// block is not zeroed. Returns (block, whether it is new).
    fn bmap_data(&mut self, ino: u32, inode: &mut RawInode, fb: u64) -> Result<(u32, bool), i64> {
        self.map(ino, inode, fb, Some(false))
    }

    /// `leaf`: allocate a missing data block (zeroed if `Some(true)`).
    fn map(&mut self, ino: u32, inode: &mut RawInode, fb: u64, leaf: Option<bool>) -> Result<(u32, bool), i64> {
        let alloc = leaf.is_some();
        let zero_leaf = leaf == Some(true);
        let goal = self.group_of(ino);
        let spb = self.sectors_per_block() as i64;
        let p = self.ptrs_per_block();
        if (fb as usize) < DIRECT {
            let mut b = self.check_block(inode.block(fb as usize))?;
            if b == 0 && alloc {
                let promised = self.data_promised(ino, fb);
                b = self.spend(promised, |s| s.alloc_data(goal, zero_leaf))?;
                self.data_allocated(ino, fb);
                inode.set_block(fb as usize, b);
                inode.add_sectors(spb);
                return Ok((b, true));
            }
            return Ok((b, false));
        }
        let mut rel = fb - DIRECT as u64;
        let (root, depth) = if rel < p {
            (DIRECT, 1)
        } else if rel - p < p * p {
            rel -= p;
            (DIRECT + 1, 2)
        } else if rel - p - p * p < p * p * p {
            rel -= p + p * p;
            (DIRECT + 2, 3)
        } else {
            return Err(EFBIG);
        };
        let mut blk = self.check_block(inode.block(root))?;
        if blk == 0 {
            if !alloc {
                return Ok((0, false));
            }
            blk = self.alloc_table(ino, (root as u8, depth as u8, 0), goal)?;
            inode.set_block(root, blk);
            inode.add_sectors(spb);
        }
        let mut new = false;
        for level in (0..depth).rev() {
            let idx = ((rel / p.pow(level)) % p) as usize;
            let mut table = self.read_block(blk)?;
            let mut next = self.check_block(le32(&table, idx * 4))?;
            if next == 0 {
                if !alloc {
                    return Ok((0, false));
                }
                // Level 0 is the data block itself.
                next = if level > 0 {
                    self.alloc_table(ino, (root as u8, level as u8, rel / p.pow(level)), goal)?
                } else {
                    let promised = self.data_promised(ino, fb);
                    let b = self.spend(promised, |s| s.alloc_data(goal, zero_leaf))?;
                    self.data_allocated(ino, fb);
                    b
                };
                new = level == 0;
                put32(&mut table, idx * 4, next);
                self.write_block(blk, &table)?;
                inode.add_sectors(spb);
            }
            blk = next;
        }
        Ok((blk, new))
    }

    /// Indirect block `id` of `ino` (zeroed), spending its promise if
    /// there is one.
    fn alloc_table(&mut self, ino: u32, id: TableId, goal: usize) -> Result<u32, i64> {
        let promised = self.table_promised(ino, id);
        let b = self.spend(promised, |s| s.alloc_block(goal, true))?;
        self.table_allocated(ino, id);
        Ok(b)
    }

    /// A data block; one not zeroed is fresh until written (see `fresh`).
    fn alloc_data(&mut self, goal: usize, zero: bool) -> Result<u32, i64> {
        let b = self.alloc_block(goal, zero)?;
        if !zero {
            self.new_meta.remove(&b);
            self.fresh.insert(b);
        }
        Ok(b)
    }

    /// Frees the data blocks with relative index >= `from` below `blk`
    /// (`depth` 0 is a data block). Returns whether `blk` itself was freed. A long cut
    /// stops to commit at consistent states (`cut_pause`): a table's pointers to what was
    /// freed are cleared before.
    ///
    /// A table of the system zone, or one the bitmap does not have allocated (freed already
    /// through another pointer to it, or never allocated), is not the file's: corruption,
    /// noted; it counts as freed (its pointer goes) and nothing below it is. So a cut walks
    /// each table once at most (twice: once in part), however tables are shared: linear in
    /// the blocks freed, where a triple indirect block whose pointers all lead to one table
    /// would take p³ steps. (Data blocks: `free_block`.)
    fn trunc_tree(&mut self, cut: &mut Cut, inode: &mut RawInode, blk: u32, depth: u32, from: u64) -> Result<bool, i64> {
        if blk == 0 {
            return Ok(true);
        }
        if depth > 0 && (self.is_system(blk) || !self.block_in_use(blk)?) {
            self.corrupt();
            return Ok(true);
        }
        if depth == 0 {
            if from == 0 {
                cut.freed += self.free_block(blk)? as i64;
                return Ok(true);
            }
            return Ok(false);
        }
        let p = self.ptrs_per_block();
        let per = p.pow(depth - 1);
        let mut table = self.read_block(blk)?;
        let (mut changed, mut empty) = (false, true);
        for i in 0..p {
            // (Checked below: a pointer to the system zone is dropped, not followed.)
            let entry = le32(&table, i as usize * 4);
            if entry == 0 {
                continue;
            }
            if (i + 1) * per <= from {
                empty = false;
                continue;
            }
            if self.trunc_tree(cut, inode, entry, depth - 1, from.saturating_sub(i * per))? {
                put32(&mut table, i as usize * 4, 0);
                changed = true;
                if self.over_limit() {
                    self.write_block(blk, &table)?;
                    changed = false;
                    self.cut_pause(cut, inode)?;
                }
            } else {
                empty = false;
            }
        }
        if empty {
            cut.freed += self.free_block(blk)? as i64;
            return Ok(true);
        }
        if changed {
            self.write_block(blk, &table)?;
        }
        Ok(false)
    }

    /// A cut reached the transaction's limit: the inode as it is now (on the orphan list,
    /// so that a crash's recovery finishes the cut: freed with no links, else cut to its
    /// size, as ext3 does) committed, and the cut goes on in a new transaction.
    fn cut_pause(&mut self, cut: &mut Cut, inode: &mut RawInode) -> Result<(), i64> {
        if !cut.listed && self.has_orphan_list {
            self.list_orphan(cut.ino, inode)?;
            cut.listed = true;
        }
        inode.add_sectors(-cut.freed * self.sectors_per_block() as i64);
        cut.freed = 0;
        self.write_inode(cut.ino, inode)?;
        self.pause()
    }

    /// Frees the blocks of `inode` from file block `keep` on (its size says `keep` blocks
    /// or fewer already), in as many transactions as it takes; `inode` is written after.
    fn cut_blocks(&mut self, ino: u32, inode: &mut RawInode, keep: u64) -> Result<(), i64> {
        let listed = self.orphans.contains_key(&ino);
        let mut cut = Cut { ino, freed: 0, listed };
        for i in keep.min(DIRECT as u64)..DIRECT as u64 {
            let b = inode.block(i as usize);
            if b != 0 {
                cut.freed += self.free_block(b)? as i64;
                inode.set_block(i as usize, 0);
                if self.over_limit() {
                    self.cut_pause(&mut cut, inode)?;
                }
            }
        }
        let p = self.ptrs_per_block();
        let mut base = DIRECT as u64;
        for (root, depth) in [(DIRECT, 1), (DIRECT + 1, 2), (DIRECT + 2, 3)] {
            let top = inode.block(root);
            if self.trunc_tree(&mut cut, inode, top, depth, keep.saturating_sub(base))? {
                inode.set_block(root, 0);
            }
            base += p.pow(depth);
        }
        inode.add_sectors(-cut.freed * self.sectors_per_block() as i64);
        // Listed for the cut alone: off the list again, with the cut's last transaction.
        if cut.listed && !listed {
            self.remove_orphan(ino)?;
            put32(&mut inode.0, 20, 0);
        }
        self.write_inode(ino, inode)
    }

    /// Zeroes the bytes of the block holding byte `len` of the file from there on.
    fn zero_tail(&mut self, ino: u32, inode: &mut RawInode, len: u64) -> Result<(), i64> {
        let bs = self.block_size as u64;
        if len % bs == 0 {
            return Ok(());
        }
        let blk = self.bmap(ino, inode, len / bs, false)?;
        if blk != 0 {
            let mut buf = self.data_block(blk)?;
            buf[(len % bs) as usize..].fill(0);
            self.write_data(blk, &buf)?;
        }
        Ok(())
    }

    fn truncate(&mut self, ino: u32, inode: &mut RawInode, len: u64) -> Result<(), i64> {
        let bs = self.block_size as u64;
        if len < inode.size() && !inode.fast_symlink() {
            // The size before any block goes: a cut that spans transactions has its size
            // from the first (recovery cuts to it).
            inode.set_size(len);
            self.cut_blocks(ino, inode, len.div_ceil(bs))?;
            // The tail of the last kept block zeroed, so a later extension reads zeros (the
            // last step that can fail: a cut that fails before leaves the data as it was).
            self.zero_tail(ino, inode, len)?;
        }
        // Promised blocks beyond go with them.
        self.unpromise_from(ino, len.div_ceil(bs));
        inode.set_size(len);
        inode.touch(self.dev.now(), false, true);
        self.write_inode(ino, inode)
    }

    fn read(&mut self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let mut inode = self.live_inode(ino)?;
        // Only regular files have data blocks; a fast symlink's block
        // pointers hold characters, not block numbers.
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        let size = inode.size();
        if off >= size {
            return Ok(0);
        }
        let n = buf.len().min((size - off) as usize);
        let bs = self.block_size;
        let mut done = 0;
        while done < n {
            let pos = off + done as u64;
            let fb = pos / bs as u64;
            let bo = (pos % bs as u64) as usize;
            if bo != 0 || n - done < bs {
                // Part of a block.
                let chunk = (bs - bo).min(n - done);
                let blk = self.bmap(ino, &mut inode, fb, false)?;
                if blk == 0 {
                    buf[done..done + chunk].fill(0);
                } else {
                    let data = self.data_block(blk)?;
                    buf[done..done + chunk].copy_from_slice(&data[bo..bo + chunk]);
                }
                done += chunk;
                continue;
            }
            // Whole blocks: a run of contiguous ones (or of holes) is one read.
            let first = self.bmap(ino, &mut inode, fb, false)?;
            let mut count = 1;
            while count < (n - done) / bs {
                let next = self.bmap(ino, &mut inode, fb + count as u64, false)?;
                let follows = if first == 0 { next == 0 } else { next.checked_sub(first) == Some(count as u32) };
                if !follows {
                    break;
                }
                count += 1;
            }
            let run = &mut buf[done..done + count * bs];
            if first == 0 {
                run.fill(0);
            } else {
                self.read_data(first, run)?;
            }
            done += count * bs;
        }
        Ok(n)
    }

    fn write(&mut self, ino: u32, off: u64, data: &[u8]) -> Result<usize, i64> {
        let mut inode = self.live_inode(ino)?;
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        off.checked_add(data.len() as u64)
            .filter(|&end| end <= self.max_file_size())
            .ok_or(EFBIG)?;
        let bs = self.block_size;
        let mut done = 0;
        let mut result = Ok(());
        // A block the last run allocated but did not write (it was not
        // contiguous): (file block, disk block, new).
        let mut next_run: Option<(u64, u32, bool)> = None;
        while done < data.len() {
            let pos = off + done as u64;
            let fb = pos / bs as u64;
            let bo = (pos % bs as u64) as usize;
            let mapped = match next_run.take() {
                Some((at, blk, new)) if at == fb => Ok((blk, new)),
                _ => self.bmap_data(ino, &mut inode, fb),
            };
            let (first, new) = match mapped {
                Ok(b) => b,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            if bo != 0 || data.len() - done < bs {
                // Part of a block: read, change, write back (a new one is zero).
                let chunk = (bs - bo).min(data.len() - done);
                let block = if new { Ok(vec![0u8; bs]) } else { self.data_block(first) };
                let written = block.and_then(|mut block| {
                    block[bo..bo + chunk].copy_from_slice(&data[done..done + chunk]);
                    self.write_data(first, &block)
                });
                if let Err(e) = written {
                    result = Err(e);
                    break;
                }
                done += chunk;
                continue;
            }
            // Whole blocks: allocate a contiguous run, write it at once.
            let mut count = 1;
            while count < (data.len() - done) / bs {
                match self.bmap_data(ino, &mut inode, fb + count as u64) {
                    Ok((next, _)) if next.checked_sub(first) == Some(count as u32) => count += 1,
                    Ok((next, new)) => {
                        next_run = Some((fb + count as u64, next, new));
                        break;
                    }
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            if let Err(e) = self.write_data(first, &data[done..done + count * bs]) {
                result = Err(e);
                break;
            }
            done += count * bs;
            if result.is_err() {
                break;
            }
            // A long write commits what it did so far at the transaction's limit.
            if self.over_limit() && done < data.len() {
                if off + done as u64 > inode.size() {
                    inode.set_size(off + done as u64);
                }
                self.write_inode(ino, &inode)?;
                self.pause()?;
            }
        }
        if off + done as u64 > inode.size() {
            inode.set_size(off + done as u64);
        }
        // Also after an error: the inode records the blocks it got.
        inode.touch(self.dev.now(), false, true);
        self.write_inode(ino, &inode)?;
        match result {
            Err(e) if done == 0 => Err(e),
            _ => Ok(done),
        }
    }

    fn dir_blocks(&self, inode: &RawInode) -> u64 {
        inode.size() / self.block_size as u64
    }

    /// The entries of directory `dir` (its inode, blocks and records checked:
    /// `dir_inode`, `dir_block_list`, `parse_entries`).
    fn list(&mut self, dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
        let mut inode = self.dir_inode(dir)?;
        let mut out = Vec::new();
        for blk in self.dir_block_list(dir, &mut inode)? {
            let block = self.read_block(blk)?;
            for e in self.entries(&block)? {
                if e.inode != 0 {
                    let name = String::from_utf8_lossy(&block[e.pos + 8..e.pos + 8 + e.name_len]).into_owned();
                    out.push((name, e.inode, e.ftype));
                }
            }
        }
        Ok(out)
    }

    /// The inode `name` in `dir` names. It must be in use, with a link, and sound (as
    /// ext4's lookup checks the inode it gets): a name of a freed inode is corruption, which
    /// must not lead an unlink to free its blocks (another file's now) again.
    fn lookup(&mut self, dir: u32, name: &str) -> Result<u32, i64> {
        let ino = self.list(dir)?.into_iter().find(|(n, _, _)| n == name).map(|(_, ino, _)| ino).ok_or(ENOENT)?;
        match self.live_inode(ino) {
            Ok(inode) if inode.links() != 0 => Ok(ino),
            Ok(_) | Err(ENOENT) => Err(self.corrupt()),
            Err(e) => Err(e),
        }
    }

    fn dir_add(&mut self, dir: u32, name: &str, ino: u32, ftype: u8) -> Result<(), i64> {
        let name = name.as_bytes();
        let need = 8 + align4(name.len());
        let mut inode = self.dir_inode(dir)?;
        let blocks = self.dir_block_list(dir, &mut inode)?;
        for &blk in &blocks {
            let mut block = self.read_block(blk)?;
            for e in self.entries(&block)? {
                let used = if e.inode == 0 { 0 } else { 8 + align4(e.name_len) };
                if e.rec_len - used < need {
                    continue;
                }
                if e.inode == 0 {
                    write_entry(&mut block, e.pos, ino, e.rec_len, name, ftype);
                } else {
                    put16(&mut block, e.pos + 4, used as u16);
                    write_entry(&mut block, e.pos + used, ino, e.rec_len - used, name, ftype);
                }
                self.write_block(blk, &block)?;
                // A new name changes the directory (its mtime and ctime).
                inode.touch(self.dev.now(), false, true);
                return self.write_inode(dir, &inode);
            }
        }
        // No room: append a block holding just the new entry (a directory's size has 32
        // bits: ENOSPC at 4 GiB, as ext4's limit on a directory's size).
        let fb = blocks.len() as u64;
        let size = (fb + 1) * self.block_size as u64;
        if size > u32::MAX as u64 {
            return Err(ENOSPC);
        }
        let (blk, new) = self.map(dir, &mut inode, fb, Some(true))?;
        if !new {
            // A block past the directory's end: not the directory's to write over.
            return Err(self.corrupt());
        }
        let mut block = vec![0u8; self.block_size];
        write_entry(&mut block, 0, ino, self.block_size, name, ftype);
        self.write_block(blk, &block)?;
        inode.set_size(size);
        inode.touch(self.dev.now(), false, true);
        self.write_inode(dir, &inode)
    }

    fn dir_remove(&mut self, dir: u32, name: &str) -> Result<(), i64> {
        let mut inode = self.dir_inode(dir)?;
        for blk in self.dir_block_list(dir, &mut inode)? {
            let mut block = self.read_block(blk)?;
            let entries = self.entries(&block)?;
            for (i, e) in entries.iter().enumerate() {
                if e.inode == 0 || &block[e.pos + 8..e.pos + 8 + e.name_len] != name.as_bytes() {
                    continue;
                }
                match i.checked_sub(1).map(|p| &entries[p]) {
                    // Merge into the previous entry of the same block.
                    Some(prev) => put16(&mut block, prev.pos + 4, (prev.rec_len + e.rec_len) as u16),
                    None => put32(&mut block, e.pos, 0),
                }
                self.write_block(blk, &block)?;
                inode.touch(self.dev.now(), false, true);
                return self.write_inode(dir, &inode);
            }
        }
        Err(ENOENT)
    }

    /// Points the entry `name` of `dir` at `ino` (of type `ftype`), in place.
    fn dir_set(&mut self, dir: u32, name: &str, ino: u32, ftype: u8) -> Result<(), i64> {
        let mut inode = self.dir_inode(dir)?;
        for blk in self.dir_block_list(dir, &mut inode)? {
            let mut block = self.read_block(blk)?;
            let entries = self.entries(&block)?;
            if let Some(e) = entries.iter().find(|e| e.inode != 0 && &block[e.pos + 8..e.pos + 8 + e.name_len] == name.as_bytes()) {
                let pos = e.pos;
                put32(&mut block, pos, ino);
                block[pos + 7] = ftype;
                self.write_block(blk, &block)?;
                inode.touch(self.dev.now(), false, true);
                return self.write_inode(dir, &inode);
            }
        }
        Err(ENOENT)
    }

    fn dir_is_empty(&mut self, dir: u32) -> Result<bool, i64> {
        Ok(self.list(dir)?.iter().all(|(n, _, _)| n == "." || n == ".."))
    }

    /// Points the ".." entry of directory `dir` (in its first block) at `parent`; a
    /// directory without one is corrupt.
    fn set_dotdot(&mut self, dir: u32, parent: u32) -> Result<(), i64> {
        let mut inode = self.dir_inode(dir)?;
        let blk = self.bmap(dir, &mut inode, 0, false)?;
        if blk == 0 {
            return Err(self.corrupt());
        }
        let mut block = self.read_block(blk)?;
        for e in self.entries(&block)? {
            if e.inode != 0 && &block[e.pos + 8..e.pos + 8 + e.name_len] == b".." {
                put32(&mut block, e.pos, parent);
                return self.write_block(blk, &block);
            }
        }
        Err(self.corrupt())
    }

    fn adjust_links(&mut self, ino: u32, delta: i32) -> Result<(), i64> {
        let mut inode = self.read_inode(ino)?;
        inode.set_links((inode.links() as i32 + delta) as u16);
        inode.touch(self.dev.now(), false, false);
        self.write_inode(ino, &inode)
    }

    /// A new inode named `name` in `dir`: the inode (a directory's parent counts its ".."),
    /// and its name, in the running transaction. Its number.
    fn create(&mut self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        check_name(name)?;
        if let NewNode::CharDev(rdev) | NewNode::BlockDev(rdev) = kind {
            dev_to_blocks(*rdev)?;
        }
        // (Only "no such name" lets it be made: a failed lookup is no proof of none.)
        match self.lookup(dir, name) {
            Ok(_) => return Err(EEXIST),
            Err(ENOENT) => {}
            Err(e) => return Err(e),
        }
        let is_dir = matches!(kind, NewNode::Dir);
        if is_dir && self.read_inode(dir)?.links() >= LINK_MAX {
            return Err(EMLINK);
        }
        let ino = self.alloc_inode(is_dir, self.group_of(dir))?;
        let ftype = self.init_inode(dir, ino, kind, perm)?;
        self.dir_add(dir, name, ino, ftype)?;
        Ok(ino)
    }

    /// Writes new inode `ino` (in `dir`) and what it holds; its type for its entry.
    fn init_inode(&mut self, dir: u32, ino: u32, kind: &NewNode, perm: u32) -> Result<u8, i64> {
        let goal = self.group_of(dir);
        let is_dir = matches!(kind, NewNode::Dir);
        // A new generation for the number, unlike its last (a freed inode keeps that), and
        // random (`Device::random`).
        let old = self.read_inode(ino).map(|old| old.generation()).unwrap_or(0);
        let generation = old.wrapping_add(1 + self.dev.random() % u32::MAX);
        let mut inode = RawInode([0; 128]);
        inode.set_generation(generation);
        inode.touch(self.dev.now(), true, true);
        let ftype = match kind {
            NewNode::File => {
                inode.set_mode((S_IFREG | perm & 0o7777) as u16);
                inode.set_links(1);
                FT_REG
            }
            NewNode::Dir => {
                inode.set_mode((S_IFDIR | perm & 0o7777) as u16);
                inode.set_links(2);
                let blk = self.alloc_block(goal, false)?;
                let mut block = vec![0u8; self.block_size];
                write_entry(&mut block, 0, ino, 12, b".", FT_DIR);
                write_entry(&mut block, 12, dir, self.block_size - 12, b"..", FT_DIR);
                self.write_block(blk, &block)?;
                inode.set_block(0, blk);
                inode.add_sectors(self.sectors_per_block() as i64);
                inode.set_size(self.block_size as u64);
                FT_DIR
            }
            NewNode::Symlink(target) => {
                inode.set_mode((S_IFLNK | 0o777) as u16);
                inode.set_links(1);
                if target.len() < FAST_SYMLINK_MAX {
                    inode.0[40..40 + target.len()].copy_from_slice(target.as_bytes());
                } else {
                    // (Short of the block, as Linux's ext2 and e2fsck want: `check_inode`.)
                    if target.len() >= self.block_size {
                        return Err(ENAMETOOLONG);
                    }
                    let blk = self.alloc_block(goal, false)?;
                    let mut block = vec![0u8; self.block_size];
                    block[..target.len()].copy_from_slice(target.as_bytes());
                    self.write_block(blk, &block)?;
                    inode.set_block(0, blk);
                    inode.add_sectors(self.sectors_per_block() as i64);
                }
                inode.set_size(target.len() as u64);
                FT_SYMLINK
            }
            NewNode::Socket | NewNode::Fifo | NewNode::CharDev(_) | NewNode::BlockDev(_) => {
                let (kind, rdev) = match *kind {
                    NewNode::Fifo => (S_IFIFO, None),
                    NewNode::CharDev(rdev) => (S_IFCHR, Some(rdev)),
                    NewNode::BlockDev(rdev) => (S_IFBLK, Some(rdev)),
                    _ => (S_IFSOCK, None),
                };
                if let Some(rdev) = rdev {
                    let [b0, b1] = dev_to_blocks(rdev)?;
                    inode.set_block(0, b0);
                    inode.set_block(1, b1);
                }
                inode.set_mode((kind | perm & 0o7777) as u16);
                inode.set_links(1);
                ftype(kind)
            }
        };
        self.write_inode(ino, &inode)?;
        // A directory's parent counts its "..".
        if is_dir {
            self.adjust_links(dir, 1)?;
        }
        Ok(ftype)
    }

    /// Frees an inode whose last link is gone, with all its blocks (a big one in several
    /// transactions, on the orphan list until the last: `cut_blocks`), and takes it off the
    /// orphan list. (Its generation goes up: a handle of the file is stale, also before the
    /// number is reused.)
    fn release(&mut self, ino: u32, mut inode: RawInode) -> Result<(), i64> {
        let dir = inode.is_dir();
        // (Only pointers to blocks: a device's hold its number.)
        if inode.has_blocks() {
            self.cut_blocks(ino, &mut inode, 0)?;
        }
        // Its promises go with it.
        self.unpromise_from(ino, 0);
        self.remove_orphan(ino)?;
        inode.set_links(0);
        inode.set_generation(inode.generation().wrapping_add(1));
        let now = self.dev.now().max(1);
        put32(&mut inode.0, 20, now); // dtime (never 0: that is "in use")
        self.write_inode(ino, &inode)?;
        if self.checks() {
            self.check_free(ino);
        }
        self.free_inode(ino, dir)
    }

    /// Removes `name` from `dir`; the inodes to hold until their release (see `gone`).
    fn unlink(&mut self, dir: u32, name: &str, want_dir: bool, in_use: &dyn Fn(u32) -> bool) -> Result<Vec<(u32, u32)>, i64> {
        if name == "." || name == ".." {
            return Err(EINVAL);
        }
        let ino = self.lookup(dir, name)?;
        let mut inode = self.read_inode(ino)?;
        match (inode.is_dir(), want_dir) {
            (true, false) => return Err(EISDIR),
            (false, true) => return Err(ENOTDIR),
            (true, true) if inode.links() > 2 || !self.dir_is_empty(ino)? => return Err(ENOTEMPTY),
            _ => {}
        }
        self.dir_remove(dir, name)?;
        let mut held = Vec::new();
        if self.drop_link(dir, ino, &mut inode)? {
            held.extend(self.gone(ino, inode, in_use)?);
        }
        Ok(held)
    }

    /// `ino` (as `inode`) lost its name in `dir`: one link less (a directory has none
    /// left, and `dir` loses the link of its ".."), written. Whether that was its last.
    fn drop_link(&mut self, dir: u32, ino: u32, inode: &mut RawInode) -> Result<bool, i64> {
        let last = inode.is_dir() || inode.links() <= 1;
        if inode.is_dir() {
            self.adjust_links(dir, -1)?;
            *inode = self.read_inode(ino)?;
            inode.set_links(0);
        } else {
            inode.set_links(inode.links().saturating_sub(1));
        }
        inode.touch(self.dev.now(), false, false);
        self.write_inode(ino, inode)?;
        Ok(last)
    }

    /// `ino` (as `inode`, no links) lost its last name: if `in_use` says someone uses it, it
    /// goes on the orphan list and is returned with its generation (freed by `release`);
    /// else it is freed now. Both in the running transaction with the name's removal.
    fn gone(&mut self, ino: u32, mut inode: RawInode, in_use: &dyn Fn(u32) -> bool) -> Result<Option<(u32, u32)>, i64> {
        if in_use(ino) {
            self.list_orphan(ino, &mut inode)?;
            return Ok(Some((ino, inode.generation())));
        }
        self.release(ino, inode)?;
        Ok(None)
    }

    /// Whether `ancestor` is `dir` or one of its parents (via "..").
    fn is_ancestor(&mut self, ancestor: u32, mut dir: u32) -> Result<bool, i64> {
        for _ in 0..4096 {
            if dir == ancestor {
                return Ok(true);
            }
            if dir == ROOT_INO {
                return Ok(false);
            }
            // (A directory without "..", or whose ".." is no directory, is corrupt.)
            dir = match self.lookup(dir, "..") {
                Err(ENOENT | ENOTDIR) => return Err(self.corrupt()),
                r => r?,
            };
        }
        Err(ELOOP)
    }

    /// Renames `oname` in `odir` to `nname` in `ndir`, in the running transaction: the new
    /// name (over an existing one: its entry takes the file in place), the old name's
    /// removal, a moved directory's ".." and its parents' counts, and the replaced inode's
    /// lost link (`gone` if its last). The inodes to hold until their release.
    fn rename(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str, in_use: &dyn Fn(u32) -> bool) -> Result<Vec<(u32, u32)>, i64> {
        check_name(nname)?;
        if [oname, nname].iter().any(|n| *n == "." || *n == "..") {
            return Err(EINVAL);
        }
        let ino = self.lookup(odir, oname)?;
        let inode = self.read_inode(ino)?;
        let is_dir = inode.is_dir();
        if is_dir && self.is_ancestor(ino, ndir)? {
            return Err(EINVAL);
        }
        let ftype = ftype(inode.mode() as u32);
        // (Only "no such name" means none: another failure is the rename's.)
        let existing = match self.lookup(ndir, nname) {
            Ok(existing) => Some(existing),
            Err(ENOENT) => None,
            Err(e) => return Err(e),
        };
        match existing {
            // Two names of one file (hard links, or the same name): nothing to do, as
            // POSIX says.
            Some(e) if e == ino => return Ok(Vec::new()),
            Some(e) => {
                let ex = self.read_inode(e)?;
                match (ex.is_dir(), is_dir) {
                    (true, false) => return Err(EISDIR),
                    (false, true) => return Err(ENOTDIR),
                    (true, true) if ex.links() > 2 || !self.dir_is_empty(e)? => return Err(ENOTEMPTY),
                    _ => {}
                }
                self.dir_set(ndir, nname, ino, ftype)?;
            }
            None => {
                if is_dir && odir != ndir && self.read_inode(ndir)?.links() >= LINK_MAX {
                    return Err(EMLINK);
                }
                self.dir_add(ndir, nname, ino, ftype)?
            }
        }
        self.dir_remove(odir, oname)?;
        if is_dir && odir != ndir {
            self.set_dotdot(ino, ndir)?;
            self.adjust_links(odir, -1)?;
            self.adjust_links(ndir, 1)?;
        }
        // (A rename changes the inode: its ctime.)
        self.adjust_links(ino, 0)?;
        let mut held = Vec::new();
        if let Some(e) = existing {
            let mut ex = self.read_inode(e)?;
            if self.drop_link(ndir, e, &mut ex)? {
                held.extend(self.gone(e, ex, in_use)?);
            }
        }
        Ok(held)
    }

    fn readlink(&mut self, ino: u32) -> Result<String, i64> {
        let mut inode = self.read_inode(ino)?;
        if inode.mode() as u32 & S_IFMT != S_IFLNK {
            return Err(EINVAL);
        }
        // (Its size within its form, `check_inode`; a slow one without its block is corrupt.)
        self.check_inode(&inode)?;
        let len = inode.size() as usize;
        let bytes = if inode.fast_symlink() {
            inode.0[40..40 + len].to_vec()
        } else {
            let blk = self.bmap(ino, &mut inode, 0, false)?;
            if blk == 0 {
                return Err(self.corrupt());
            }
            self.read_block(blk)?[..len].to_vec()
        };
        String::from_utf8(bytes).map_err(|_| EIO)
    }

    // ------------------------------------- the ring path (see `Ext2::reserve`)

    /// Inode `ino` if it is in use: in range, allocated (it has a mode) and
    /// not freed (no deletion time; an unlinked inode still open is in
    /// use). ENOENT otherwise; EUCLEAN if it is corrupt (`check_inode`).
    fn live_inode(&mut self, ino: u32) -> Result<RawInode, i64> {
        let count = self.inodes_per_group as u64 * self.groups.len() as u64;
        if ino == 0 || ino as u64 > count {
            return Err(ENOENT);
        }
        let inode = self.read_inode(ino)?;
        let listed = self.orphans.contains_key(&ino);
        if inode.mode() == 0 || (le32(&inode.0, 20) != 0 && !listed) {
            return Err(ENOENT);
        }
        let g = self.group_of(ino);
        if !self.test_bit(self.groups[g].inode_bitmap, (ino - 1) % self.inodes_per_group)? {
            return Err(ENOENT);
        }
        self.check_inode(&inode)?;
        Ok(inode)
    }

    // ------------------------------------- the orphan list (see the module comment)

    const LAST_ORPHAN: usize = 232;

    fn last_orphan(&self) -> u32 {
        if self.has_orphan_list { le32(&self.sb, Self::LAST_ORPHAN) } else { 0 }
    }

    fn set_last_orphan(&mut self, ino: u32) {
        if self.has_orphan_list {
            put32(&mut self.sb, Self::LAST_ORPHAN, ino);
            self.super_dirty = true;
        }
    }

    /// The first inode number files get (`s_first_ino`; below it the reserved ones).
    fn first_ino(&self) -> u32 {
        if self.has_orphan_list { le32(&self.sb, 84) } else { 11 }
    }

    fn inode_count(&self) -> u32 {
        (self.inodes_per_group as u64 * self.groups.len() as u64).min(u32::MAX as u64) as u32
    }

    /// Whether `ino` may be on the orphan list: a file's number, allocated in the inode
    /// bitmap, with a mode, and no links unless a regular file (a cut's, `cut_pause`: cut to
    /// its size by recovery), as e2fsck and ext4 check an entry; and sound (`check_inode`).
    fn may_be_orphan(&mut self, ino: u32) -> Result<bool, i64> {
        if ino < self.first_ino().max(1) || ino > self.inode_count() {
            return Ok(false);
        }
        let g = self.group_of(ino);
        if !self.test_bit(self.groups[g].inode_bitmap, (ino - 1) % self.inodes_per_group)? {
            return Ok(false);
        }
        let inode = self.read_inode(ino)?;
        Ok(inode.mode() != 0 && (inode.links() == 0 || inode.is_reg()) && self.check_inode(&inode).is_ok())
    }

    /// At mount: reads the list into `orphans`. The list is cut before the first entry
    /// that cannot be one (see `may_be_orphan`; or seen before: a cycle): nothing after it
    /// is trusted. Whether it was cut (the change is to be committed).
    fn load_orphans(&mut self) -> Result<bool, i64> {
        let mut at = self.last_orphan();
        let mut prev = 0;
        while at != 0 {
            if self.orphans.contains_key(&at) || !self.may_be_orphan(at)? {
                if prev == 0 {
                    self.set_last_orphan(0);
                } else {
                    let mut p = self.read_inode(prev)?;
                    put32(&mut p.0, 20, 0);
                    self.write_inode(prev, &p)?;
                    self.orphans.insert(prev, 0);
                }
                return Ok(true);
            }
            let next = le32(&self.read_inode(at)?.0, 20);
            self.orphans.insert(at, next);
            if prev != 0 {
                self.orphan_prev.insert(at, prev);
            }
            prev = at;
            at = next;
        }
        Ok(false)
    }

    /// Puts `ino` at the head of the orphan list (`inode` written: its deletion time is
    /// the old head), unless it is on it (or there is no list: then only written).
    fn list_orphan(&mut self, ino: u32, inode: &mut RawInode) -> Result<(), i64> {
        if !self.has_orphan_list || self.orphans.contains_key(&ino) {
            return self.write_inode(ino, inode);
        }
        let head = self.last_orphan();
        put32(&mut inode.0, 20, head);
        self.write_inode(ino, inode)?;
        self.orphans.insert(ino, head);
        if head != 0 {
            self.orphan_prev.insert(head, ino);
        }
        self.set_last_orphan(ino);
        Ok(())
    }

    /// Takes `ino` off the list: the head moves on, or its predecessor points past it (the
    /// caller writes `ino` itself). Whether it was on the list.
    fn remove_orphan(&mut self, ino: u32) -> Result<bool, i64> {
        let Some(&next) = self.orphans.get(&ino) else { return Ok(false) };
        let prev = self.orphan_prev.get(&ino).copied();
        match prev {
            None => self.set_last_orphan(next),
            Some(p) => {
                let mut pi = self.read_inode(p)?;
                put32(&mut pi.0, 20, next);
                self.write_inode(p, &pi)?;
                self.orphans.insert(p, next);
            }
        }
        self.orphans.remove(&ino);
        self.orphan_prev.remove(&ino);
        if next != 0 {
            match prev {
                Some(p) => self.orphan_prev.insert(next, p),
                None => self.orphan_prev.remove(&next),
            };
        }
        Ok(true)
    }

    /// Where the pointer to the data block of file block `fb` lives; with
    /// `alloc`, missing indirect blocks on the way are allocated (zeroed)
    /// and linked. None if one is missing and not `alloc`.
    fn leaf_slot(&mut self, ino: u32, inode: &mut RawInode, fb: u64, alloc: bool) -> Result<Option<Slot>, i64> {
        if (fb as usize) < DIRECT {
            return Ok(Some(Slot::Inode(fb as usize)));
        }
        let goal = self.group_of(ino);
        let spb = self.sectors_per_block() as i64;
        let p = self.ptrs_per_block();
        let mut rel = fb - DIRECT as u64;
        let (root, depth) = if rel < p {
            (DIRECT, 1)
        } else if rel - p < p * p {
            rel -= p;
            (DIRECT + 1, 2)
        } else if rel - p - p * p < p * p * p {
            rel -= p + p * p;
            (DIRECT + 2, 3)
        } else {
            return Err(EFBIG);
        };
        let mut blk = self.check_block(inode.block(root))?;
        if blk == 0 {
            if !alloc {
                return Ok(None);
            }
            blk = self.alloc_table(ino, (root as u8, depth as u8, 0), goal)?;
            inode.set_block(root, blk);
            inode.add_sectors(spb);
        }
        // Down to the table that points to data blocks (level 0).
        for level in (1..depth).rev() {
            let idx = ((rel / p.pow(level)) % p) as usize;
            let mut table = self.read_block(blk)?;
            let mut next = self.check_block(le32(&table, idx * 4))?;
            if next == 0 {
                if !alloc {
                    return Ok(None);
                }
                next = self.alloc_table(ino, (root as u8, level as u8, rel / p.pow(level)), goal)?;
                put32(&mut table, idx * 4, next);
                self.write_block(blk, &table)?;
                inode.add_sectors(spb);
            }
            blk = next;
        }
        Ok(Some(Slot::Table(blk, (rel % p) as usize)))
    }

    fn slot_get(&mut self, inode: &RawInode, slot: Slot) -> Result<u32, i64> {
        match slot {
            Slot::Inode(i) => self.check_block(inode.block(i)),
            Slot::Table(t, i) => {
                let table = self.read_block(t)?;
                self.check_block(le32(&table, i * 4))
            }
        }
    }

    fn slot_set(&mut self, inode: &mut RawInode, slot: Slot, block: u32) -> Result<(), i64> {
        match slot {
            Slot::Inode(i) => {
                inode.set_block(i, block);
                Ok(())
            }
            Slot::Table(t, i) => {
                let mut table = self.read_block(t)?;
                put32(&mut table, i * 4, block);
                self.write_block(t, &table)
            }
        }
    }

    /// Reserves a free data block (see `reserved`): the one after `after`
    /// if that is free (runs stay contiguous), else the first free one from
    /// group `goal` on.
    fn reserve_block(&mut self, goal: usize, after: Option<u32>) -> Result<u32, i64> {
        if !self.spending && self.avail() == 0 {
            return Err(ENOSPC);
        }
        let (fdb, bpg) = (self.first_data_block, self.blocks_per_group);
        let next = after.and_then(|b| b.checked_add(1)).filter(|&b| b >= fdb && b < self.blocks_count);
        let (goal, start) = match next {
            Some(b) => (((b - fdb) / bpg) as usize, (b - fdb) % bpg),
            None => (goal.min(self.groups.len() - 1), 0),
        };
        let n = self.groups.len();
        for (i, g) in (goal..n).chain(0..goal).enumerate() {
            if self.groups[g].free_blocks == 0 || self.bad_block_bitmaps.contains(&g) {
                continue;
            }
            let (first, limit) = (self.group_first(g), self.group_len(g));
            let from = if i == 0 { start } else { 0 };
            let Some(bit) = self.find_bit(self.groups[g].block_bitmap, limit, from, Some(first))? else { continue };
            if self.system_free(g, first + bit) {
                continue;
            }
            self.reserved.insert(first + bit);
            return Ok(first + bit);
        }
        Err(ENOSPC)
    }

    /// Takes reserved block `block` into its group's bitmap and counts.
    fn mark_used(&mut self, block: u32) -> Result<(), i64> {
        let rel = block - self.first_data_block;
        let g = (rel / self.blocks_per_group) as usize;
        self.set_bit(self.groups[g].block_bitmap, rel % self.blocks_per_group)?;
        self.groups[g].free_blocks = self.groups[g].free_blocks.saturating_sub(1);
        self.free_blocks = self.free_blocks.saturating_sub(1);
        self.write_group(g)?;
        self.write_super()
    }

    fn read_map(&mut self, ino: u32, off: u64, len: u64) -> Result<(u64, Vec<Extent>), i64> {
        let mut inode = self.live_inode(ino)?;
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        let size = inode.size();
        let end = off.saturating_add(len).min(size);
        let bs = self.block_size as u64;
        let mut out: Vec<Extent> = Vec::new();
        let mut pos = off;
        while pos < end {
            let in_block = pos % bs;
            let chunk = (bs - in_block).min(end - pos);
            let blk = self.bmap(ino, &mut inode, pos / bs, false)?;
            // A fresh block (not zeroed until the commit) reads as zeros, a
            // cached one may be newer than the device's: the caller copies.
            if blk != 0 && (self.fresh.contains(&blk) || self.cache.contains(blk)) {
                return Err(EAGAIN);
            }
            let disk = (blk != 0).then(|| blk as u64 * bs + in_block);
            match out.last_mut() {
                Some(e) if e.disk.is_none() && disk.is_none() => e.len += chunk,
                Some(e) if e.disk.is_some() && e.disk.map(|d| d + e.len) == disk => e.len += chunk,
                _ => out.push(Extent { disk, len: chunk }),
            }
            pos += chunk;
        }
        Ok((size, out))
    }

    /// The data block for file block `fb` of a write: (block, new).
    fn reserve_one(&mut self, ino: u32, inode: &mut RawInode, fb: u64, after: Option<u32>) -> Result<(u32, bool), i64> {
        let slot = self.leaf_slot(ino, inode, fb, true)?.ok_or(EIO)?;
        match self.slot_get(inode, slot)? {
            0 => {
                // Spends its promise when linked (`link_block`): a write
                // that fails keeps it.
                let promised = self.data_promised(ino, fb);
                let goal = self.group_of(ino);
                let block = self.spend(promised, |s| s.reserve_block(goal, after))?;
                if promised {
                    self.reserved_promised.insert((ino, fb), block);
                }
                Ok((block, true))
            }
            // As in `read_map`: the device must hold the block's current data.
            b if self.fresh.contains(&b) || self.cache.contains(b) => Err(EAGAIN),
            b => Ok((b, false)),
        }
    }

    fn reserve(&mut self, ino: u32, off: u64, len: u64) -> Result<Reservation, i64> {
        let mut inode = self.live_inode(ino)?;
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        let end = off.checked_add(len).filter(|&end| end <= self.max_file_size()).ok_or(EFBIG)?;
        let mut r = Reservation { ino, runs: Vec::new() };
        if len == 0 {
            return Ok(r);
        }
        let bs = self.block_size as u64;
        let mut result = Ok(());
        let mut last_new = None;
        for fb in off / bs..=(end - 1) / bs {
            let (block, new) = match self.reserve_one(ino, &mut inode, fb, last_new) {
                Ok(b) => b,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            if new {
                last_new = Some(block);
            }
            // A long reservation commits the indirect blocks it linked so far at the
            // transaction's limit.
            if self.over_limit() {
                if let Err(e) = self.write_inode(ino, &inode).and_then(|_| self.pause()) {
                    result = Err(e);
                    break;
                }
            }
            match r.runs.last_mut() {
                Some(run) if run.new == new && run.block.checked_add(run.count) == Some(block) => run.count += 1,
                _ => r.runs.push(Run { file_block: fb, block, count: 1, new }),
            }
        }
        // The inode keeps the indirect blocks it got, also after an error.
        let written = self.write_inode(ino, &inode);
        if let Err(e) = result.and(written) {
            self.unreserve(&r);
            return Err(e);
        }
        Ok(r)
    }

    fn unreserve(&mut self, r: &Reservation) {
        for run in r.runs.iter().filter(|run| run.new) {
            for i in 0..run.count {
                self.unreserve_block(r.ino, run.file_block + i as u64, run.block + i);
            }
        }
    }

    /// Links reserved block `block` as file block `fb`.
    fn link_block(&mut self, ino: u32, inode: &mut RawInode, fb: u64, block: u32) -> Result<(), i64> {
        let slot = self.leaf_slot(ino, inode, fb, false)?.ok_or(EIO)?;
        if self.slot_get(inode, slot)? != 0 {
            // Linked meanwhile (writes to a block are serialized, so this
            // never happens): ours stays free.
            return Ok(());
        }
        self.mark_used(block)?;
        self.data_allocated(ino, fb);
        self.slot_set(inode, slot, block)?;
        inode.add_sectors(self.sectors_per_block() as i64);
        Ok(())
    }

    fn link(&mut self, r: &Reservation, end: u64) -> Result<u64, i64> {
        // The data went to the device outside this filesystem: from here on
        // it is flushed before any metadata reaches the device (see
        // `unflushed`), also a block an eviction writes while the pointers
        // below are set.
        self.unflushed = true;
        let mut inode = match self.live_inode(r.ino) {
            Ok(inode) => inode,
            Err(e) => {
                self.unreserve(r);
                return Err(e);
            }
        };
        let mut result = Ok(());
        for run in r.runs.iter().filter(|run| run.new) {
            for i in 0..run.count {
                let block = run.block + i;
                if self.unreserve_block(r.ino, run.file_block + i as u64, block) && result.is_ok() {
                    let fb = run.file_block + i as u64;
                    result = self.link_block(r.ino, &mut inode, fb, block);
                    // A long link commits what it linked so far at the transaction's limit
                    // (the file as long as that: its data is on the device).
                    if result.is_ok() && self.over_limit() {
                        let linked = ((fb + 1) * self.block_size as u64).min(end);
                        if linked > inode.size() {
                            inode.set_size(linked);
                        }
                        result = self.write_inode(r.ino, &inode).and_then(|_| self.pause());
                    }
                }
            }
        }
        if result.is_ok() && end > inode.size() {
            inode.set_size(end);
        }
        inode.touch(self.dev.now(), false, true);
        self.write_inode(r.ino, &inode)?;
        result.map(|_| inode.size())
    }
}

/// Where a pointer to a data block lives: the inode's block array, or a
/// slot of an indirect block (block, index).
#[derive(Clone, Copy)]
enum Slot {
    Inode(usize),
    Table(u32, usize),
}

/// A stretch of a file's bytes (`Ext2::read_map`): `len` bytes from byte
/// `disk` of the device, or a hole (None), which reads as zeros.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub disk: Option<u64>,
    pub len: u64,
}

/// `count` contiguous data blocks from `block` on the device, which hold
/// the file blocks from `file_block` on; `new` ones are reserved for the
/// write and linked by `Ext2::link`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub file_block: u64,
    pub block: u32,
    pub count: u32,
    pub new: bool,
}

/// The blocks a write goes to (`Ext2::reserve`).
#[derive(Debug, Default)]
pub struct Reservation {
    pub ino: u32,
    pub runs: Vec<Run>,
}

/// Metadata of one inode.
#[derive(Clone, Copy)]
pub struct Stat {
    pub mode: u32,
    pub size: u64,
    pub links: u32,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    /// i_generation (a new one for each new file of the number).
    pub generation: u32,
    /// A character or block device's number (as `st_rdev`, `dev_make`); 0 for any other.
    pub rdev: u64,
}

/// A mounted ext2 filesystem on device `D`. Single-threaded: the owner
/// serializes access.
pub struct Ext2<D: Device> {
    st: State<D>,
}

impl<D: Device> Ext2<D> {
    pub fn mount(dev: D) -> Result<Self, &'static str> {
        Self::mount_with_cache(dev, CACHE_BYTES)
    }

    /// `mount` with a metadata cache of `cache_bytes` (the tests make it
    /// small, so that blocks are evicted all the time).
    pub fn mount_with_cache(dev: D, cache_bytes: usize) -> Result<Self, &'static str> {
        Self::mount_replayed(dev, cache_bytes, false)
    }

    /// `mount_with_cache`; `replayed`: the journal was just replayed (a device that does
    /// not keep what it is given cannot make the mount replay for ever).
    fn mount_replayed(mut dev: D, cache_bytes: usize, replayed: bool) -> Result<Self, &'static str> {
        let mut sb = [0u8; 1024];
        dev.read(2, &mut sb).map_err(|_| "cannot read the superblock")?;
        if le16(&sb, 56) != MAGIC {
            return Err("not an ext2 filesystem");
        }
        let rev = le32(&sb, 76);
        if rev > 1 {
            return Err("unsupported ext2 revision");
        }
        let (inode_size, first_ino, compat, incompat, ro_compat) = if rev == 1 {
            (le16(&sb, 88) as usize, le32(&sb, 84), le32(&sb, 92), le32(&sb, 96), le32(&sb, 100))
        } else {
            (128, GOOD_OLD_FIRST_INO, 0, 0, 0)
        };
        if incompat & !(INCOMPAT_FILETYPE | INCOMPAT_RECOVER) != 0 || ro_compat & !RO_COMPAT_SUPPORTED != 0 {
            return Err("unsupported ext2 features");
        }
        let log = le32(&sb, 24);
        if log > 2 || inode_size < 128 {
            return Err("unsupported block or inode size");
        }
        let block_size = 1024usize << log;
        let first_data_block = le32(&sb, 20);
        let blocks_count = le32(&sb, 4);
        let blocks_per_group = le32(&sb, 32);
        let inodes_per_group = le32(&sb, 40);
        // (The superblock is at byte 1024: the descriptors follow it, in block 2 with 1 KiB
        // blocks, where the first data block is 1, else in block 1, after block 0.)
        if first_data_block != (block_size == 1024) as u32 || blocks_count <= first_data_block {
            return Err("corrupt superblock: its first data block or block count");
        }
        // A group's bitmaps are one block each: its sizes must fit one, and
        // an inode must fit a block (or the bitmap and table arithmetic
        // would index past them).
        let bits = 8 * block_size as u32;
        if blocks_per_group == 0 || inodes_per_group == 0 {
            return Err("corrupt superblock");
        }
        if blocks_per_group > bits || inodes_per_group > bits || inode_size > block_size || !inode_size.is_power_of_two() {
            return Err("corrupt superblock: group or inode sizes do not fit a block");
        }
        // The groups the blocks make have the inodes the superblock counts (as ext4 checks),
        // from `s_first_ino` (past the reserved ones) on.
        let count = (blocks_count - first_data_block).div_ceil(blocks_per_group) as usize;
        let inodes = count as u64 * inodes_per_group as u64;
        if inodes != le32(&sb, 0) as u64 {
            return Err("corrupt superblock: the inode count does not match the groups");
        }
        if first_ino < GOOD_OLD_FIRST_INO || first_ino as u64 > inodes {
            return Err("corrupt superblock: the first inode");
        }
        // Every group begins with a copy of the superblock and the descriptors, or none
        // (`has_super`); the first group holds that, two bitmaps and an inode table (no
        // meta_bg, no flex_bg): so the descriptors, and the memory they take, are bounded
        // by a group's blocks.
        let reserved_gdt = if compat & COMPAT_RESIZE_INODE != 0 { le16(&sb, 206) as u64 } else { 0 };
        let super_len = 1 + (count as u64 * 32).div_ceil(block_size as u64) + reserved_gdt;
        let table_blocks = (inodes_per_group as u64 * inode_size as u64).div_ceil(block_size as u64);
        if super_len + 2 + table_blocks > blocks_per_group.min(blocks_count - first_data_block) as u64 {
            return Err("corrupt superblock: the group descriptors do not fit the first group");
        }
        // The device has the filesystem's last block: nothing the counts drive goes beyond
        // the disk there is.
        let mut last = vec![0u8; block_size];
        dev.read((blocks_count - 1) as u64 * (block_size / SECTOR_SIZE) as u64, &mut last)
            .map_err(|_| "the filesystem is larger than the device")?;
        drop(last);
        let mut groups = Vec::new();
        groups.try_reserve_exact(count).map_err(|_| "no memory for the group descriptors")?;
        let mut st = State {
            dev,
            block_size,
            inode_size,
            inodes_per_group,
            blocks_per_group,
            first_data_block,
            blocks_count,
            free_blocks: le32(&sb, 12),
            free_inodes: le32(&sb, 16),
            gdt_block: first_data_block + 1,
            groups,
            super_len: super_len as u32,
            table_blocks: table_blocks as u32,
            sparse_super: ro_compat & RO_COMPAT_SPARSE_SUPER != 0,
            journal_blocks: BlockSet::new(),
            bad_block_bitmaps: BTreeSet::new(),
            bad_inode_bitmaps: BTreeSet::new(),
            cache: BlockCache::new(cache_bytes / block_size),
            cache_bytes,
            sb,
            sb_on_disk: sb,
            super_dirty: false,
            written: false,
            fresh: BTreeSet::new(),
            orphans: BTreeMap::new(),
            orphan_prev: BTreeMap::new(),
            broken: false,
            undo: None,
            batching: false,
            limit: None,
            has_orphan_list: rev >= 1,
            mount_state: le16(&sb, 58),
            in_use: false,
            marked: false,
            reserved: BTreeSet::new(),
            reserved_promised: BTreeMap::new(),
            promises: BTreeMap::new(),
            promised: 0,
            spending: false,
            new_meta: BTreeSet::new(),
            freed: BlockSet::new(),
            unflushed: false,
            journal: None,
            read_only: false,
        };
        let (mut free_blocks, mut free_inodes) = (0u64, 0u64);
        for g in 0..count {
            let byte = g * 32;
            let block = st.gdt_block + (byte / block_size) as u32;
            let buf = st.read_block(block).map_err(|_| "cannot read the group descriptors")?;
            let o = byte % block_size;
            let group = Group {
                block_bitmap: le32(&buf, o),
                inode_bitmap: le32(&buf, o + 4),
                inode_table: le32(&buf, o + 8),
                free_blocks: le16(&buf, o + 12),
                free_inodes: le16(&buf, o + 14),
                used_dirs: le16(&buf, o + 16),
            };
            // Its bitmaps and inode table inside the group, after its copy of the superblock
            // and descriptors, apart (as ext4's `ext4_check_descriptors`); its counts within
            // its blocks and inodes.
            let first = st.group_first(g) as u64 + if has_super(st.sparse_super, g) { super_len } else { 0 };
            let end = st.group_first(g) as u64 + st.group_len(g) as u64;
            let inside = |b: u32, len: u64| b as u64 >= first && b as u64 + len <= end;
            let table = group.inode_table as u64..group.inode_table as u64 + table_blocks;
            let apart = group.block_bitmap != group.inode_bitmap
                && !table.contains(&(group.block_bitmap as u64))
                && !table.contains(&(group.inode_bitmap as u64));
            if !inside(group.block_bitmap, 1) || !inside(group.inode_bitmap, 1) || !inside(group.inode_table, table_blocks) || !apart {
                return Err("corrupt group descriptor: its bitmaps or inode table are not its group's");
            }
            let ipg = inodes_per_group as u16;
            if group.free_blocks as u32 > st.group_len(g) || group.free_inodes > ipg || group.used_dirs > ipg {
                return Err("corrupt group descriptor: its counts exceed its group");
            }
            free_blocks += group.free_blocks as u64;
            free_inodes += group.free_inodes as u64;
            st.groups.push(group);
        }
        // The superblock's totals are the descriptors' (as Linux counts them at mount: the
        // superblock's may be stale), written with the next commit if they differ.
        let (free_blocks, free_inodes) = (free_blocks as u32, free_inodes as u32);
        if (st.free_blocks, st.free_inodes) != (free_blocks, free_inodes) {
            (st.free_blocks, st.free_inodes) = (free_blocks, free_inodes);
            put32(&mut st.sb, 12, free_blocks);
            put32(&mut st.sb, 16, free_inodes);
            st.super_dirty = true;
        }
        st.read_only = st.dev.read_only();
        let journaled = le32(&st.sb, 92) & COMPAT_HAS_JOURNAL != 0;
        if journaled {
            if le32(&st.sb, 224) != JOURNAL_INO {
                return Err("an external journal");
            }
            let j = st.open_journal()?;
            if j.sb.start != 0 {
                if st.read_only {
                    return Err("the journal needs recovery, the device is read-only");
                }
                if replayed {
                    return Err("the journal needs recovery again after its replay");
                }
                // Replayed (the log empty after), then mounted again from what the disk
                // has now.
                st.replay(j)?;
                let cache_bytes = st.cache_bytes();
                return Self::mount_replayed(st.dev, cache_bytes, true);
            }
            st.journal = Some(j);
        }
        // The root: a sound directory in use (as ext4 refuses a root that is none), before
        // anything is written.
        match st.live_inode(ROOT_INO) {
            Ok(root) if root.is_dir() && root.links() != 0 => {}
            _ => return Err("corrupt root directory"),
        }
        if journaled && !st.read_only {
            st.adopt_journal_features().map_err(|_| "cannot write the journal superblock")?;
        } else if !st.read_only {
            st.add_journal()?;
        }
        let mut fs = Ext2 { st };
        if fs.st.read_only {
            // (Read as it is: nothing is written.)
            let _ = fs.st.load_orphans();
            fs.st.super_dirty = false;
            return Ok(fs);
        }
        // The orphan list as it is (freed by `recover_orphans`, if the mounter says so), but
        // cuts a crash interrupted are finished now (a file with links stays a file).
        fs.st.load_orphans().map_err(|_| "cannot read the orphan list")?;
        fs.st.commit().map_err(|_| "cannot read the orphan list")?;
        fs.finish_cuts().map_err(|_| "cannot finish an interrupted truncation")?;
        Ok(fs)
    }

    /// Inode `ino`'s metadata; EUCLEAN if it is in use (a mode) and corrupt.
    pub fn stat(&mut self, ino: u32) -> Result<Stat, i64> {
        let i = self.st.read_inode(ino)?;
        if i.mode() != 0 {
            self.st.check_inode(&i)?;
        }
        Ok(Stat {
            mode: i.mode() as u32,
            size: i.size(),
            links: i.links() as u32,
            atime: i.atime(),
            mtime: i.mtime(),
            ctime: i.ctime(),
            generation: i.generation(),
            rdev: match i.mode() as u32 & S_IFMT {
                S_IFCHR | S_IFBLK => dev_from_blocks(i.block(0), i.block(1)),
                _ => 0,
            },
        })
    }

    pub fn read(&mut self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        self.st.read(ino, off, buf)
    }

    /// Before an operation that changes anything: EIO once the filesystem is `broken`,
    /// EROFS on a read-only mount. A running transaction that reached its limit (the ring
    /// path's links add to it without committing) is committed first. In use, the disk is
    /// to say "not clean" ("Clean"), with the operation's transaction.
    fn ready(&mut self) -> Result<(), i64> {
        if self.st.broken {
            return Err(EIO);
        }
        if self.st.read_only {
            return Err(EROFS);
        }
        if self.st.over_limit() {
            self.st.commit()?;
        }
        if self.st.in_use && !self.st.marked {
            let state = self.st.mount_state & !STATE_VALID;
            if le16(&self.st.sb, 58) != state {
                put16(&mut self.st.sb, 58, state);
                self.st.super_dirty = true;
            }
            self.st.marked = true;
        }
        Ok(())
    }

    /// Runs one operation as a unit: all of it joins the running transaction, or (it
    /// failed) none of it (`State::rollback`). The transaction is committed now, unless
    /// operations are batched (`batch`). An operation that failed after part of it was
    /// committed (a long one stopped at the transaction's limit) stops the filesystem.
    fn op<T>(&mut self, f: impl FnOnce(&mut State<D>) -> Result<T, i64>) -> Result<T, i64> {
        self.ready()?;
        self.st.begin();
        match f(&mut self.st) {
            Ok(value) => {
                self.st.undo = None;
                if !self.st.batching {
                    self.st.commit()?;
                }
                Ok(value)
            }
            Err(e) => {
                if self.st.undo.as_ref().is_some_and(|u| u.paused) {
                    self.st.broken = true;
                }
                self.st.rollback();
                Err(e)
            }
        }
    }

    /// Bounds transactions to `blocks` log blocks (at least 4), below the journal's own
    /// bound (a quarter of it): the dirty metadata the cache holds for a transaction is
    /// bounded so. (The tests make it small, so that long operations commit part way.)
    pub fn limit_transactions(&mut self, blocks: usize) {
        self.st.limit = Some(blocks.max(4));
    }

    /// Batches operations (diskfs's group commit): while on, they do not commit; `sync`
    /// commits all of them as one transaction (the caller answers them after that). A
    /// transaction that reaches its limit is committed all the same. Off: each commits.
    pub fn batch(&mut self, on: bool) {
        self.st.batching = on;
    }

    /// The filesystem is in use (its owner has clients) or not (module comment, "Clean").
    /// In use: the disk says "not clean" from the first change on (nothing is written
    /// now: a read-only disk serves reads all the same). Not in use: the state it had at
    /// mount is written back (unless an orphan waits to be freed), everything is committed,
    /// and the journal is emptied (`State::quiesce`). Whether the disk says "clean"
    /// (cleanly unmounted) now.
    pub fn set_in_use(&mut self, in_use: bool) -> Result<bool, i64> {
        self.st.in_use = in_use;
        if in_use || self.st.read_only {
            return Ok(le16(&self.st.sb, 58) & STATE_VALID != 0);
        }
        if self.st.broken {
            return Err(EIO);
        }
        if self.st.marked && !self.owed() {
            put16(&mut self.st.sb, 58, self.st.mount_state);
            self.st.super_dirty = true;
            self.st.marked = false;
        }
        self.st.quiesce()?;
        Ok(le16(&self.st.sb, 58) & STATE_VALID != 0)
    }

    /// Whether repairs are owed (the disk is not clean even when everything is written):
    /// inodes on the orphan list.
    pub fn owed(&self) -> bool {
        !self.st.orphans.is_empty()
    }

    /// Whether the filesystem stopped (a commit failed, or an operation failed after part
    /// of it was committed): every change fails from now on; the owner mounts again
    /// (diskfs restarts), which replays the journal.
    pub fn broken(&self) -> bool {
        self.st.broken
    }

    /// Whether it is mounted read-only (`Device::read_only`): every change fails (EROFS).
    pub fn read_only(&self) -> bool {
        self.st.read_only
    }

    /// Whether the filesystem has errors: corruption was found (an operation failed with
    /// EUCLEAN, or went on past it: "Corruption"), or the disk said so at mount. The disk
    /// says so too once a commit wrote it, until e2fsck.
    pub fn errors(&self) -> bool {
        self.st.mount_state & STATE_ERROR != 0
    }

    pub fn write(&mut self, ino: u32, off: u64, data: &[u8]) -> Result<usize, i64> {
        self.op(|st| st.write(ino, off, data))
    }

    pub fn truncate(&mut self, ino: u32, len: u64) -> Result<(), i64> {
        self.op(|st| {
            let mut inode = st.live_inode(ino)?;
            if !inode.is_reg() {
                return Err(EINVAL);
            }
            if len > st.max_file_size() {
                return Err(EFBIG);
            }
            st.truncate(ino, &mut inode, len)
        })
    }

    pub fn list(&mut self, dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
        self.st.list(dir)
    }

    pub fn lookup(&mut self, dir: u32, name: &str) -> Result<u32, i64> {
        self.st.lookup(dir, name)
    }

    /// Makes a file, directory, symlink, socket, FIFO or device named `name` in `dir`: one transaction.
    pub fn create(&mut self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        self.op(|st| st.create(dir, name, kind, perm))
    }

    /// Removes `name`; returns the inodes whose last link went away. They
    /// stay allocated (on the orphan list) until `release`, so open files keep working.
    pub fn unlink(&mut self, dir: u32, name: &str, want_dir: bool) -> Result<Vec<u32>, i64> {
        self.unlink_unless(dir, name, want_dir, |_| true).map(|gone| gone.into_iter().map(|(ino, _)| ino).collect())
    }

    /// `unlink`, but an inode whose last link went is freed at once unless `in_use` says
    /// someone uses it (in the same transaction as the name's removal). The inodes to
    /// release, with their generations.
    pub fn unlink_unless(&mut self, dir: u32, name: &str, want_dir: bool, in_use: impl Fn(u32) -> bool) -> Result<Vec<(u32, u32)>, i64> {
        self.op(|st| st.unlink(dir, name, want_dir, &in_use))
    }

    pub fn rename(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str) -> Result<Vec<u32>, i64> {
        self.rename_unless(odir, oname, ndir, nname, |_| true).map(|gone| gone.into_iter().map(|(ino, _)| ino).collect())
    }

    /// `rename`, one transaction; a replaced inode is freed at once unless `in_use` (see
    /// `unlink_unless`).
    pub fn rename_unless(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str, in_use: impl Fn(u32) -> bool) -> Result<Vec<(u32, u32)>, i64> {
        self.op(|st| st.rename(odir, oname, ndir, nname, &in_use))
    }

    /// Frees an inode returned by `unlink` or `rename` (no links left), with all its
    /// blocks, and takes it off the orphan list: one transaction (a big file's blocks in
    /// several, on the list until the last).
    pub fn release(&mut self, ino: u32) -> Result<(), i64> {
        self.op(|st| {
            let inode = st.live_inode(ino)?;
            if inode.links() != 0 {
                return Err(EINVAL);
            }
            st.release(ino, inode)
        })
    }

    /// Whether `ino` is in use and has no name left: `release` frees it.
    pub fn releasable(&mut self, ino: u32) -> bool {
        self.st.live_inode(ino).is_ok_and(|i| i.links() == 0)
    }

    /// Frees every inode on the orphan list (with no links) but those `keep` names. The
    /// first mount since boot frees them all (their users went with the machine); a
    /// server restarted while its clients live keeps those they may still use. How many
    /// were freed; an error stops it (the rest stay listed).
    pub fn recover_orphans(&mut self, keep: impl Fn(u32) -> bool) -> Result<u32, i64> {
        let listed: Vec<u32> = self.st.orphans.keys().copied().filter(|&i| !keep(i)).collect();
        let mut freed = 0;
        for ino in listed {
            // (One may have gone meanwhile: its owner freed it.)
            if !self.releasable(ino) {
                continue;
            }
            self.release(ino)?;
            freed += 1;
        }
        Ok(freed)
    }

    /// At mount: the orphans with links (cuts a crash interrupted, `State::cut_pause`) are
    /// cut to their sizes and taken off the list.
    fn finish_cuts(&mut self) -> Result<(), i64> {
        if self.st.read_only {
            return Ok(());
        }
        let listed: Vec<u32> = self.st.orphans.keys().copied().collect();
        for ino in listed {
            self.op(|st| {
                let mut inode = st.read_inode(ino)?;
                if inode.links() == 0 {
                    return Ok(());
                }
                let size = inode.size();
                st.cut_blocks(ino, &mut inode, size.div_ceil(st.block_size as u64))?;
                st.zero_tail(ino, &mut inode, size)?;
                st.remove_orphan(ino)?;
                put32(&mut inode.0, 20, 0);
                st.write_inode(ino, &inode)
            })?;
        }
        Ok(())
    }

    /// The inodes on the orphan list now.
    pub fn orphans(&self) -> Vec<u32> {
        self.st.orphans.keys().copied().collect()
    }

    /// Whether `ino` is on the orphan list.
    pub fn is_orphan(&self, ino: u32) -> bool {
        self.st.orphans.contains_key(&ino)
    }

    pub fn readlink(&mut self, ino: u32) -> Result<String, i64> {
        self.st.readlink(ino)
    }

    pub fn set_perm(&mut self, ino: u32, perm: u32) -> Result<(), i64> {
        self.op(|st| {
            let mut inode = st.live_inode(ino)?;
            inode.set_mode((inode.mode() as u32 & S_IFMT | perm & 0o7777) as u16);
            inode.touch(st.dev.now(), false, false);
            st.write_inode(ino, &inode)
        })
    }

    /// Sets the times given (seconds since 1970); the others stay.
    pub fn set_times(&mut self, ino: u32, atime: Option<u32>, mtime: Option<u32>, ctime: Option<u32>) -> Result<(), i64> {
        self.op(|st| {
            let mut inode = st.live_inode(ino)?;
            inode.set_times(atime, mtime, ctime);
            st.write_inode(ino, &inode)
        })
    }

    pub fn device(&self) -> &D {
        &self.st.dev
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.st.dev
    }

    /// Unmounts: everything committed, the journal emptied and the superblock clean of
    /// `needs_recovery` (a clean stop).
    pub fn into_device(mut self) -> D {
        if !self.st.read_only && !self.st.broken {
            let _ = self.st.quiesce();
        }
        self.st.dev
    }

    /// (block size, total blocks, free blocks, total inodes, free inodes);
    /// promised blocks do not count as free.
    pub fn usage(&self) -> (u64, u64, u64, u64, u64) {
        let st = &self.st;
        let inodes = st.inodes_per_group as u64 * st.groups.len() as u64;
        let free = (st.free_blocks as u64).saturating_sub(st.promised);
        (st.block_size as u64, st.blocks_count as u64, free, inodes, st.free_inodes as u64)
    }

    /// Promises the blocks a later write of `off..off + len` to regular
    /// file `ino` needs (see "Promises"), for `owner` (the caller's name
    /// for whoever caches the write): the data blocks the file lacks there
    /// and the indirect blocks they need, unless `owner` promised them
    /// already. ENOSPC (and nothing promised) if the free blocks less
    /// those promised and in flight do not cover them.
    pub fn promise(&mut self, owner: u64, ino: u32, off: u64, len: u64) -> Result<(), i64> {
        self.ready()?;
        self.st.promise(owner, ino, off, len)
    }

    /// `owner` goes: its promises end.
    pub fn forget_promises(&mut self, owner: u64) {
        self.pass_promises(owner, |_| None);
    }

    /// `owner` goes: its promises of each file pass to `heir(ino)` (another
    /// owner that holds the file: with a cache the owners share, its
    /// write-back writes those pages now), or end.
    pub fn pass_promises(&mut self, owner: u64, heir: impl Fn(u32) -> Option<u64>) {
        let st = &mut self.st;
        for (&ino, m) in st.promises.iter_mut() {
            let Some(p) = m.remove(&owner) else { continue };
            let Some(h) = heir(ino).filter(|&h| h != owner) else { continue };
            let to = m.entry(h).or_default();
            for (&s, &e) in &p.blocks {
                for fb in s..e {
                    if !to.has(fb) {
                        to.add(fb);
                    }
                }
            }
            to.tables.extend(p.tables);
        }
        st.promises.retain(|_, m| !m.is_empty());
        st.promised = st.promises.values().flat_map(|m| m.values()).map(Promise::total).sum();
        // Blocks in flight for what it promised count as reserved now.
        let gone: Vec<(u32, u64)> = st.reserved_promised.keys().copied().filter(|&(ino, fb)| !st.data_promised(ino, fb)).collect();
        for k in gone {
            st.reserved_promised.remove(&k);
        }
    }

    /// Blocks promised now.
    pub fn promised(&self) -> u64 {
        self.st.promised
    }

    pub fn block_size(&self) -> usize {
        self.st.block_size
    }

    /// The largest file this filesystem can hold (EFBIG beyond).
    pub fn max_file_size(&self) -> u64 {
        self.st.max_file_size()
    }

    // The ring path (diskfs's data plane, docs/design/io-rings.md): the
    // caller moves file data between the device and its client's pages
    // itself, with several requests in flight; these calls say where the
    // data lies and record what the writes did. They do not commit: the
    // metadata they change reaches the device with the next commit (`sync`,
    // or any other operation), after the data (see `State::unflushed`).

    /// ENOENT unless inode `ino` is in use (allocated and not freed).
    pub fn check(&mut self, ino: u32) -> Result<(), i64> {
        self.st.live_inode(ino).map(|_| ())
    }

    /// The handle (`ino`, `generation`) names a file in use: ESTALE if the inode is not
    /// in use (freed) or is another file of the number (another generation).
    pub fn check_handle(&mut self, ino: u32, generation: u32) -> Result<(), i64> {
        // Only the root and files' numbers (not the reserved inodes below `s_first_ino`).
        if ino != ROOT_INO && ino < self.st.first_ino() {
            return Err(ESTALE);
        }
        match self.st.live_inode(ino) {
            Ok(inode) if inode.generation() == generation => Ok(()),
            Ok(_) | Err(ENOENT) => Err(ESTALE),
            Err(e) => Err(e),
        }
    }

    /// Where the bytes `off..off + len` of regular file `ino` lie (clipped
    /// to its size): (file size, extents). EAGAIN if a block's current
    /// data is not (only) on the device (a block cached, or fresh until the
    /// commit, which `read` handles): the caller reads through `read` then.
    pub fn read_map(&mut self, ino: u32, off: u64, len: u64) -> Result<(u64, Vec<Extent>), i64> {
        self.st.read_map(ino, off, len)
    }

    /// The blocks a write of `off..off + len` to regular file `ino` goes
    /// to: the file's blocks where it has them, and blocks reserved for
    /// it in its holes (with the indirect blocks to point to them, which
    /// are allocated and linked now). The caller writes the data to the
    /// device, then `link`s the reservation (or `unreserve`s it if the
    /// write failed); a reserved block is in no bitmap and no inode until
    /// then, so a crash before leaves the filesystem as it was. A new
    /// block must be written whole (zeros where the write has no data).
    /// Writes to the same blocks must not overlap in time (the caller
    /// serializes them), nor may a truncation or release of the file come
    /// in between. EAGAIN as for `read_map`: the caller writes through
    /// `write` then.
    pub fn reserve(&mut self, ino: u32, off: u64, len: u64) -> Result<Reservation, i64> {
        self.ready()?;
        match self.st.reserve(ino, off, len) {
            // Blocks freed since the last commit are taken only after one:
            // commit, and try again.
            Err(ENOSPC) if !self.st.freed.is_empty() => {
                self.st.commit()?;
                self.st.reserve(ino, off, len)
            }
            r => r,
        }
    }

    /// Links the reserved blocks of a write whose data is on the device,
    /// and makes the file at least `end` bytes long; its new size.
    ///
    /// (No `ready` here or in `unreserve`: they finish a write `reserve`, which was
    /// ready, let start. Their changes go out with the next commit, after the data. A link
    /// that fails part way is undone: the write's blocks are free again.)
    pub fn link(&mut self, r: &Reservation, end: u64) -> Result<u64, i64> {
        if self.st.broken {
            self.st.unreserve(r);
            return Err(EIO);
        }
        self.st.begin();
        let result = self.st.link(r, end);
        match result {
            Ok(_) => self.st.undo = None,
            Err(_) => {
                if self.st.undo.as_ref().is_some_and(|u| u.paused) {
                    self.st.broken = true;
                }
                self.st.rollback();
            }
        }
        result
    }

    /// Gives the reserved blocks of a write that failed back.
    pub fn unreserve(&mut self, r: &Reservation) {
        self.st.unreserve(r)
    }

    /// Makes every linked write durable: its data first (a device flush),
    /// then the metadata (a commit, flushed).
    pub fn sync(&mut self) -> Result<(), i64> {
        if self.st.broken {
            return Err(EIO);
        }
        if self.st.read_only {
            return Ok(());
        }
        self.st.commit()
    }
}
