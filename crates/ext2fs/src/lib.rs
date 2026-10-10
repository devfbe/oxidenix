//! ext2 on the data disk (any `Device`): revision 1 with the `filetype` feature,
//! 1/2/4 KiB blocks, direct and single/double/triple indirect blocks.
//!
//! Metadata goes through a block cache (`cache.rs`); every public operation
//! commits its changes before it returns (written together, then a
//! flush), so a completed operation is durable. File data is read and
//! written straight from and to the device, in runs of contiguous blocks.
//! Metadata is kept consistent enough for `e2fsck` on the host to accept
//! the filesystem.
//!
//! **Ordering.** Whatever metadata may point to reaches the disk first:
//! file data, zeroed fresh blocks and the contents of newly allocated
//! metadata blocks (indirect, directory, symlink blocks) are written and
//! flushed before any metadata is written, by a commit or a cache eviction
//! (`State::barrier`). A crash at any moment, whatever the device's cache
//! wrote back of what came after its last flush, therefore never leaves a
//! pointer to a block that still holds a deleted file's data (tested by
//! replaying every write and flush with arbitrary losses). A write thus
//! costs two flushes: data, then metadata.
//!
//! **Orphans.** An inode whose last link went while a client still uses it (an open file
//! unlinked) stays allocated until `release`. So that a crash or a diskfs that dies before
//! the release does not leave it allocated for good, it is on the superblock's orphan list
//! (`s_last_orphan`, chained through the inodes' deletion-time field, as ext3's) from the
//! operation that took its last link until its release. While on the list an inode's
//! deletion time is the next orphan's number, so it is in use despite a nonzero one.
//! `mount` reads the list (trusting an entry only as e2fsck and ext4 do: a number from
//! `s_first_ino` on, allocated in the inode bitmap, a mode, no links, not seen before; the
//! list is cut before the first one that is not) but frees nothing: whoever mounts decides
//! with `recover_orphans`. The first mount since boot frees them all (no user of them
//! survived); a restarted server keeps those its clients may still have open (diskfs,
//! "Holds").
//!
//! The list is kept in an order a crash cannot hurt (no journal: each step is flushed
//! before the next, as soft updates do). Unlinking the last name first makes the
//! directory's change durable while the inode still has its links on the disk, then writes
//! the inode (no links, its deletion time the old head) and flushes, then the superblock's
//! head: a name never points to an inode the list has (a crash in between leaves an inode
//! without a name, which e2fsck takes, never a name whose inode recovery would free and
//! another file reuse). Freeing takes the inode off the list (the head, or its
//! predecessor's link) and flushes before it clears the inode and its bits: a crash in
//! between leaves an unlisted inode for e2fsck, never one listed and freed (which the next
//! recovery would free again). Freeing an inode bumps its generation (as reusing its
//! number does), so stale handles stay stale.
//!
//! **Names and links.** No state of the disk has more names for an inode than its link
//! count says, and no name or ".." points to an inode the disk does not have (a crash or
//! a failed commit may leave a link too many, e2fsck's; never a name too many, which
//! would let the inode be freed while a name still points to it). Each step below is
//! committed before the next:
//! - A creation writes the inode (a directory's parent counts its ".." then), and only
//!   then its name; one whose name cannot be added deletes the inode, frees it, and then
//!   the parent's count goes.
//! - A name's removal goes before the lower link count (`drop_link`; the count with
//!   `Ext2::take_unlinked`, applied to the inode as it is then, or after the retry of a
//!   failed commit: `after_commit`); a removed directory's parent stops counting its ".."
//!   only once the directory is freed (`Ext2::freed_dir`).
//! - A rename: the file gains the link its new name takes (a moved directory's new
//!   parent: the link of its ".."), the ".." moves, the new name comes (over an existing
//!   name its entry takes the file in place: nothing to allocate), the old name goes, the
//!   extra links go. A step that fails is undone only when provably not done (the new
//!   name absent); else the rename stops with its extra links, and the same rename again
//!   finishes it (also a directory's ".." on the disk after a crash). Two names of one
//!   regular file are taken for hard links (rename does nothing then, as POSIX says)
//!   unless a rename of this mount broke off between them: after a crash a broken rename
//!   of a file looks like hard links and keeps both names (a link count that matches).
//! - A directory is removed only with no more links than "." and its name, so none whose
//!   count another directory's ".." is still in is freed.
//!
//! So an inode is freed only when its last link is gone on the disk and no directory entry
//! (".." included) names it (and, the owner's part, no client holds it): every free
//! (`Ext2::release`, an immediate free, a recovery's, an undone creation's) starts from a
//! durable state, and on a device that asks for it (`Device::checks`, the tests) checks
//! exactly that first (`State::check_free`: the inode on the disk, every entry of the
//! tree). The flushes this costs: a creation 2, a directory's 3, a rename 4 (over a name
//! 9, the replaced inode's free included), an unlink that frees at once 3, a removed
//! directory 4.
//!
//! **Failed commits.** A commit that fails (a device write or flush did) leaves what it
//! had to write in the cache. Before any later operation changes anything, that commit is
//! tried again, alone (`Ext2::ready`; EIO while it keeps failing): steps meant to reach the
//! disk one after the other never go out together. A sequence of steps (an unlink's, a
//! release's) ends at its first failed commit: an inode that cannot be put on the list
//! keeps its links (`unlisted`; `releasable` says it may be released, and its release
//! deletes it on the disk before freeing it), one whose removal from the list or deletion
//! did not commit waits to be freed (`releasing`) and is released again, never re-listed.
//! An unlink or rename whose final commit fails still happened (it is in the cache, and the
//! retry writes it): its caller gets the inodes to hold all the same. Inodes nobody holds
//! that a failed commit kept from being freed (an immediate free's, a recovery's) wait for
//! their owner to free them (`take_deferred`: it knows who holds what). An unlink whose
//! commits failed has happened in the cache only: if the filesystem breaks (or its owner
//! dies) before the retry writes it, the unlink is lost although it was acknowledged. An operation
//! that fails part way through a free (a release, a truncation: blocks freed in the cache
//! while the inode on the disk still points to them) leaves changes that must never reach
//! the disk: the filesystem is `broken` (every change and `sync` fail, nothing is written,
//! not even an evicted block); its owner mounts it again and goes on from the disk's state
//! (diskfs exits and is restarted). What failures can leave for e2fsck only (allocated,
//! unreachable, never freed twice): an inode without a name when a crash comes before its
//! release, an unlinked inode that could not be put on the list (`orphan_failures`, until
//! it is freed), the inodes of an operation that failed part way (unlink or rename: kept
//! with their links), and an `unlisted` or `releasing` inode when the filesystem is mounted
//! again before its release (a crash, a diskfs restart: it is on no list the next mount
//! reads).
//!
//! **Clean.** The superblock's state (`s_state`) says "not cleanly unmounted" while the
//! filesystem is in use (`set_in_use`: diskfs, while any client is connected) and anything
//! has been changed, so after a crash a host's `e2fsck -p` (and Linux's mount) knows to
//! check it; it is written before the first change (a read-only disk is never written).
//! When it is no longer in use, and nothing is owed (`owed`: inodes still to be freed, or
//! that failed commits left), the state it had at mount is written back, after everything
//! else reached the disk (a filesystem that was not clean then stays so until e2fsck).
//!
//! On a host: Linux's ext2 driver ignores `s_last_orphan`; its ext4 driver frees the
//! listed inodes at mount (also read-only, unless the device itself is read-only);
//! `e2fsck -fy` frees them, `e2fsck -fn` reports a non-empty list. A cleanly unmounted
//! disk has an empty list.
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
//! promised blocks; one it covers spends its promise. A promise ends when
//! its blocks are allocated, truncated away or freed with the file, or
//! when its owner goes (`forget_promises`); free space reported by `usage`
//! excludes it.

#![no_std]

extern crate alloc;

mod blockset;
mod cache;

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
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOTEMPTY: i64 = 39;
    pub const ELOOP: i64 = 40;
    /// A handle (number and generation) of an inode that is gone, or whose
    /// number is another file's now.
    pub const ESTALE: i64 = 116;
}
use errno::*;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;
pub const NAME_MAX: usize = 255;
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
}

pub const ROOT_INO: u32 = 2;
const MAGIC: u16 = 0xef53;
/// `s_state`: cleanly unmounted (`EXT2_VALID_FS`).
const STATE_VALID: u16 = 1;
const INCOMPAT_FILETYPE: u32 = 0x2;
const RO_COMPAT_SUPPORTED: u32 = 0x1 | 0x2; // sparse_super, large_file
const DIRECT: usize = 12;
/// Symlink targets shorter than this live in the block pointers ("fast").
const FAST_SYMLINK_MAX: usize = 60;

/// Memory for cached metadata blocks.
const CACHE_BYTES: usize = 1024 * 1024;

const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

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
    cache: BlockCache,
    /// The superblock as read at mount; the free counts change in it.
    sb: [u8; 1024],
    super_dirty: bool,
    /// Something was written since the last flush.
    written: bool,
    /// Data blocks allocated without zeroing that were not written yet;
    /// the commit zeroes those still here before any metadata that points
    /// to them reaches the device, so no file shows a deleted file's data.
    fresh: BTreeSet<u32>,
    /// Inodes whose last link went away; the Ext2 wrapper decides whether
    /// to free them now or when the last open reference is dropped.
    unlinked: Vec<Dropped>,
    /// The inodes on the superblock's orphan list (see the module comment), each with the
    /// next one (0: the last): in use although their deletion time is set (it chains the
    /// list). And each one's predecessor (none for the head): taking one off is a step.
    orphans: BTreeMap<u32, u32>,
    orphan_prev: BTreeMap<u32, u32>,
    /// Inodes whose last link went that could not be put on the list (an I/O error): they
    /// are freed at release all the same, but a crash before leaks them (`orphan_failures`).
    orphan_failures: u32,
    /// Inodes off the orphan list (or deleted) whose free waits: in use until `release`.
    releasing: BTreeSet<u32>,
    /// Inodes whose last link went that are not on the list (a commit failed: `unlisted`).
    unlisted: BTreeSet<u32>,
    /// Files whose rename broke off after their new name (this mount): a rename of the
    /// two names finishes it (else two names of a file are hard links: nothing to do).
    broken_renames: BTreeSet<u32>,
    /// Link drops whose names' removal a failed commit holds back: done once its retry
    /// went through (`Ext2::ready`), never before (no state of the disk with more names
    /// than links).
    after_commit: Vec<AfterCommit>,
    /// Inodes nobody holds that a failed commit kept from being freed, for the owner to
    /// free once no one holds them (`Ext2::take_deferred`).
    deferred: Vec<u32>,
    /// The last commit failed: what it had to write is still in the cache, and goes out
    /// alone before anything new is changed (`Ext2::ready`).
    pending: bool,
    /// An operation failed part way through freeing: the cache holds changes that must
    /// never reach the disk; nothing is written any more (`Ext2::broken`).
    broken: bool,
    /// The superblock has the orphan list's field (revision 1).
    has_orphan_list: bool,
    /// `s_state` as it was at mount (`set_in_use`); whether the filesystem is in use, and
    /// whether the disk says so now (not clean).
    mount_state: u16,
    in_use: bool,
    marked: bool,
    /// Blocks and inodes freed since the mount: an operation that fails after it freed
    /// something leaves the cache untrustworthy (`broken`), one that fails before does not.
    frees: u64,
    /// Data blocks reserved for writes in flight (`Ext2::reserve`): in no
    /// bitmap and no inode yet, but no allocation takes them.
    reserved: BTreeSet<u32>,
    /// The reserved blocks taken for a promised file block, by (inode,
    /// file block): until the write links them the promise still counts
    /// them (`promised`), so `avail` must not count them twice.
    reserved_promised: BTreeMap<(u32, u64), u32>,
    /// Blocks allocated for metadata (indirect, directory and symlink
    /// blocks) whose contents have not reached the device yet. Like file
    /// data, they are written and flushed before any metadata that may
    /// point to them (`barrier`): after a crash, no pointer leads to a
    /// block that still holds what a deleted file left there.
    new_meta: BTreeSet<u32>,
    /// Blocks freed since the last successful commit: the metadata on the
    /// disk may still point to them, so no allocation takes them before a
    /// commit wrote the change (else a crash shows the new owner's data in
    /// the old file). Ranges: a big file frees millions of blocks at once.
    freed: BlockSet,
    /// Blocks were written that metadata may point to (file data, zeroed
    /// fresh blocks, new metadata blocks, and the data of the caller's own
    /// writes that `Ext2::link` records) and the device has not been
    /// flushed since: it is, before any metadata reaches the device.
    unflushed: bool,
    /// Blocks promised to writes to come (`Ext2::promise`), by inode and
    /// owner, and their total.
    promises: BTreeMap<u32, BTreeMap<u64, Promise>>,
    promised: u64,
    /// An allocation spends a promise (`spend`): it may take promised
    /// blocks.
    spending: bool,
}

/// An indirect block of a file by its place in the file's tree: the
/// inode's block slot it hangs from (12, 13, 14), its height (1: it points
/// to data blocks) and its index among the tables of that height.
type TableId = (u8, u8, u64);

/// What one owner promised for one file (`Ext2::promise`).
#[derive(Default)]
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

/// One directory entry as found on disk.
/// A name an inode lost (`State::drop_link`), to be applied once that is durable.
struct Dropped {
    ino: u32,
    /// Its last: it goes (an orphan, or freed); else one link less. (A directory's
    /// parent stops counting its ".." when it is freed: `Ext2::freed_dir`.)
    last: bool,
}

/// A link drop waiting for a commit (`State::after_commit`).
#[derive(Clone, Copy)]
enum AfterCommit {
    /// `ino` lost its name in `dir` (`State::drop_link`).
    Drop { dir: u32, ino: u32 },
    /// `ino`'s links change by `delta`.
    Links { ino: u32, delta: i32 },
}

/// A rename's steps (`State::rename_plan`).
struct RenamePlan {
    ino: u32,
    is_dir: bool,
    ftype: u8,
    /// The inode the new name had (it loses that link).
    replaced: Option<u32>,
    /// The new name is the file's already (a rename that broke off).
    named: bool,
}

struct Entry {
    pos: usize,
    inode: u32,
    rec_len: usize,
    name_len: usize,
    ftype: u8,
}

fn parse_entries(block: &[u8]) -> Result<Vec<Entry>, i64> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= block.len() {
        let rec_len = le16(block, pos + 4) as usize;
        let name_len = block[pos + 6] as usize;
        if rec_len < 8 || pos + rec_len > block.len() || 8 + name_len > rec_len {
            return Err(EIO);
        }
        out.push(Entry { pos, inode: le32(block, pos), rec_len, name_len, ftype: block[pos + 7] });
        pos += rec_len;
    }
    Ok(out)
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

    /// The parent a directory's ".." names (None for another inode, or the root).
    fn dotdot(&mut self, ino: u32, inode: &RawInode) -> Option<u32> {
        if !inode.is_dir() {
            return None;
        }
        self.lookup(ino, "..").ok().filter(|&p| p != ino)
    }

    /// The invariant every free keeps (`Device::checks`): the inode is freed only when its
    /// last link is gone durably (no change waits in the cache, and the inode on the disk
    /// has no links) and no directory entry names it. Panics otherwise.
    fn check_free(&mut self, ino: u32) {
        assert!(!self.cache.has_dirty() && !self.super_dirty, "freeing inode {ino} with changes not on the disk");
        let (block, off) = self.inode_location(ino).expect("an inode in range");
        let mut buf = vec![0u8; self.block_size];
        // (A test device's failure of a read is not the check's: asked again.)
        let lba = self.lba(block);
        self.dev.read(lba, &mut buf).or_else(|_| self.dev.read(lba, &mut buf)).expect("the inode's block");
        assert_eq!(le16(&buf, off + 26), 0, "freeing inode {ino} whose links on the disk are not 0");
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
        self.cache_insert(n, buf.to_vec(), true)
    }

    fn cache_insert(&mut self, n: u32, data: Vec<u8>, dirty: bool) -> Result<(), i64> {
        let victim = self.cache.victim(n).map(|(old, contents)| (old, contents.map(<[u8]>::to_vec)));
        if let Some((old, contents)) = victim {
            if contents.is_some() && self.broken {
                // (Nothing reaches the disk any more: `broken`.)
                return Err(EIO);
            }
            if let Some(contents) = contents {
                // Data before the metadata that may point to it.
                self.zero_fresh()?;
                if self.new_meta.remove(&old) {
                    // A new block's contents: what points to it waits.
                    self.dev_write(old, &contents)?;
                    self.unflushed = true;
                } else {
                    self.barrier()?;
                    self.dev_write(old, &contents)?;
                }
            }
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

    /// Block pointer `b` read from the disk: 0 (a hole) or a block of the
    /// filesystem; anything else is corruption.
    fn check_block(&self, b: u32) -> Result<u32, i64> {
        if b != 0 && (b < self.first_data_block || b >= self.blocks_count) {
            return Err(EIO);
        }
        Ok(b)
    }

    fn write_super(&mut self) -> Result<(), i64> {
        put32(&mut self.sb, 12, self.free_blocks);
        put32(&mut self.sb, 16, self.free_inodes);
        let now = self.dev.now();
        put32(&mut self.sb, 48, now);
        self.super_dirty = true;
        Ok(())
    }

    /// Makes what metadata may point to durable before metadata is written:
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

    /// Zeroes fresh data blocks, writes the changed metadata (adjacent
    /// blocks in one request), then flushes the device if anything was
    /// written. The data and new blocks the metadata points to are flushed
    /// before it (`barrier`). What fails stays to be written by the next
    /// commit.
    fn commit(&mut self) -> Result<(), i64> {
        if self.broken {
            return Err(EIO);
        }
        let result = self.write_out();
        self.pending = result.is_err();
        result
    }

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
        // zeroed (a failed commit leaves it so): it reads as zeros.
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

    fn group_of(&self, ino: u32) -> usize {
        ((ino - 1) / self.inodes_per_group) as usize
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

    /// Finds and sets a clear bit in a bitmap block (see `find_bit`);
    /// returns its index.
    fn take_bit(&mut self, bitmap_block: u32, limit: u32, first: Option<u32>) -> Result<Option<u32>, i64> {
        let bit = self.find_bit(bitmap_block, limit, 0, first)?;
        if let Some(bit) = bit {
            self.set_bit(bitmap_block, bit)?;
        }
        Ok(bit)
    }

    /// Clears a bit; whether it was set (a free counts only then: a bit
    /// cleared twice never makes the free counts drift).
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
            self.reserved_promised.remove(&k);
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
            let mine = self.promises.get(&ino).and_then(|m| m.get(&owner));
            if mine.is_some_and(|p| p.has(fb)) {
                continue;
            }
            let (allocated, missing) = self.probe(&inode, fb)?;
            if allocated {
                continue;
            }
            let mine = self.promises.get(&ino).and_then(|m| m.get(&owner));
            for id in missing {
                if !mine.is_some_and(|p| p.tables.contains(&id)) {
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
        for g in (goal..n).chain(0..goal) {
            if self.groups[g].free_blocks == 0 {
                continue;
            }
            let first = self.first_data_block + g as u32 * self.blocks_per_group;
            let limit = self.blocks_per_group.min(self.blocks_count - first);
            if let Some(bit) = self.take_bit(self.groups[g].block_bitmap, limit, Some(first))? {
                self.groups[g].free_blocks -= 1;
                self.free_blocks -= 1;
                self.write_group(g)?;
                self.write_super()?;
                let block = first + bit;
                // Metadata unless `alloc_data` makes it a fresh data block.
                self.new_meta.insert(block);
                if zero {
                    self.write_block(block, &vec![0u8; self.block_size])?;
                }
                return Ok(block);
            }
        }
        Err(ENOSPC)
    }

    fn free_block(&mut self, block: u32) -> Result<(), i64> {
        self.check_block(block)?;
        self.frees += 1;
        self.cache.remove(block);
        self.fresh.remove(&block);
        self.new_meta.remove(&block);
        // Still pointed to on the disk until the next commit: not reused before.
        self.freed.insert(block);
        let rel = block - self.first_data_block;
        let g = (rel / self.blocks_per_group) as usize;
        if !self.clear_bit(self.groups[g].block_bitmap, rel % self.blocks_per_group)? {
            return Ok(());
        }
        self.groups[g].free_blocks += 1;
        self.free_blocks += 1;
        self.write_group(g)?;
        self.write_super()
    }

    fn alloc_inode(&mut self, dir: bool, goal: usize) -> Result<u32, i64> {
        let n = self.groups.len();
        for g in (goal..n).chain(0..goal) {
            if self.groups[g].free_inodes == 0 {
                continue;
            }
            if let Some(bit) = self.take_bit(self.groups[g].inode_bitmap, self.inodes_per_group, None)? {
                self.groups[g].free_inodes -= 1;
                if dir {
                    self.groups[g].used_dirs += 1;
                }
                self.free_inodes -= 1;
                self.write_group(g)?;
                self.write_super()?;
                return Ok(g as u32 * self.inodes_per_group + bit + 1);
            }
        }
        Err(ENOSPC)
    }

    fn free_inode(&mut self, ino: u32, dir: bool) -> Result<(), i64> {
        self.frees += 1;
        let g = self.group_of(ino);
        if !self.clear_bit(self.groups[g].inode_bitmap, (ino - 1) % self.inodes_per_group)? {
            return Ok(());
        }
        self.groups[g].free_inodes += 1;
        if dir {
            self.groups[g].used_dirs = self.groups[g].used_dirs.saturating_sub(1);
        }
        self.free_inodes += 1;
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
    /// (`depth` 0 is a data block). Returns whether `blk` itself was freed.
    fn trunc_tree(&mut self, blk: u32, depth: u32, from: u64, freed: &mut i64) -> Result<bool, i64> {
        if blk == 0 {
            return Ok(true);
        }
        if depth == 0 {
            if from == 0 {
                self.free_block(blk)?;
                *freed += 1;
                return Ok(true);
            }
            return Ok(false);
        }
        let p = self.ptrs_per_block();
        let per = p.pow(depth - 1);
        let mut table = self.read_block(blk)?;
        let (mut changed, mut empty) = (false, true);
        for i in 0..p {
            let entry = self.check_block(le32(&table, i as usize * 4))?;
            if entry == 0 {
                continue;
            }
            if (i + 1) * per <= from {
                empty = false;
                continue;
            }
            if self.trunc_tree(entry, depth - 1, from.saturating_sub(i * per), freed)? {
                put32(&mut table, i as usize * 4, 0);
                changed = true;
            } else {
                empty = false;
            }
        }
        if empty {
            self.free_block(blk)?;
            *freed += 1;
            return Ok(true);
        }
        if changed {
            self.write_block(blk, &table)?;
        }
        Ok(false)
    }

    fn truncate(&mut self, ino: u32, inode: &mut RawInode, len: u64) -> Result<(), i64> {
        let bs = self.block_size as u64;
        if len < inode.size() && !inode.fast_symlink() {
            let keep = len.div_ceil(bs);
            let mut freed = 0;
            for i in keep.min(DIRECT as u64)..DIRECT as u64 {
                let b = inode.block(i as usize);
                if b != 0 {
                    self.free_block(b)?;
                    freed += 1;
                    inode.set_block(i as usize, 0);
                }
            }
            let p = self.ptrs_per_block();
            let mut base = DIRECT as u64;
            for (root, depth) in [(DIRECT, 1), (DIRECT + 1, 2), (DIRECT + 2, 3)] {
                if self.trunc_tree(inode.block(root), depth, keep.saturating_sub(base), &mut freed)? {
                    inode.set_block(root, 0);
                }
                base += p.pow(depth);
            }
            inode.add_sectors(-freed * self.sectors_per_block() as i64);
            // Zero the tail of the last kept block so a later extension reads zeros.
            if len % bs != 0 {
                let blk = self.bmap(ino, inode, len / bs, false)?;
                if blk != 0 {
                    let mut buf = self.data_block(blk)?;
                    buf[(len % bs) as usize..].fill(0);
                    self.write_data(blk, &buf)?;
                }
            }
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

    fn list(&mut self, dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
        let mut inode = self.read_inode(dir)?;
        if !inode.is_dir() {
            return Err(ENOTDIR);
        }
        let mut out = Vec::new();
        for fb in 0..self.dir_blocks(&inode) {
            let blk = self.bmap(dir, &mut inode, fb, false)?;
            if blk == 0 {
                continue;
            }
            let block = self.read_block(blk)?;
            for e in parse_entries(&block)? {
                if e.inode != 0 {
                    let name = String::from_utf8_lossy(&block[e.pos + 8..e.pos + 8 + e.name_len]).into_owned();
                    out.push((name, e.inode, e.ftype));
                }
            }
        }
        Ok(out)
    }

    fn lookup(&mut self, dir: u32, name: &str) -> Result<u32, i64> {
        self.list(dir)?
            .into_iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, ino, _)| ino)
            .ok_or(ENOENT)
    }

    fn dir_add(&mut self, dir: u32, name: &str, ino: u32, ftype: u8) -> Result<(), i64> {
        let name = name.as_bytes();
        let need = 8 + align4(name.len());
        let mut inode = self.read_inode(dir)?;
        for fb in 0..self.dir_blocks(&inode) {
            let blk = self.bmap(dir, &mut inode, fb, false)?;
            if blk == 0 {
                continue;
            }
            let mut block = self.read_block(blk)?;
            for e in parse_entries(&block)? {
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
        // No room: append a block holding just the new entry.
        let fb = self.dir_blocks(&inode);
        let blk = self.bmap(dir, &mut inode, fb, true)?;
        let mut block = vec![0u8; self.block_size];
        write_entry(&mut block, 0, ino, self.block_size, name, ftype);
        self.write_block(blk, &block)?;
        inode.set_size((fb + 1) * self.block_size as u64);
        inode.touch(self.dev.now(), false, true);
        self.write_inode(dir, &inode)
    }

    fn dir_remove(&mut self, dir: u32, name: &str) -> Result<(), i64> {
        let mut inode = self.read_inode(dir)?;
        for fb in 0..self.dir_blocks(&inode) {
            let blk = self.bmap(dir, &mut inode, fb, false)?;
            if blk == 0 {
                continue;
            }
            let mut block = self.read_block(blk)?;
            let entries = parse_entries(&block)?;
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
        let mut inode = self.read_inode(dir)?;
        for fb in 0..self.dir_blocks(&inode) {
            let blk = self.bmap(dir, &mut inode, fb, false)?;
            if blk == 0 {
                continue;
            }
            let mut block = self.read_block(blk)?;
            let entries = parse_entries(&block)?;
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

    /// Points the ".." entry of directory `dir` at `parent`.
    fn set_dotdot(&mut self, dir: u32, parent: u32) -> Result<(), i64> {
        let mut inode = self.read_inode(dir)?;
        let blk = self.bmap(dir, &mut inode, 0, false)?;
        let mut block = self.read_block(blk)?;
        for e in parse_entries(&block)? {
            if &block[e.pos + 8..e.pos + 8 + e.name_len] == b".." {
                put32(&mut block, e.pos, parent);
                return self.write_block(blk, &block);
            }
        }
        Err(EIO)
    }

    fn adjust_links(&mut self, ino: u32, delta: i32) -> Result<(), i64> {
        let mut inode = self.read_inode(ino)?;
        inode.set_links((inode.links() as i32 + delta) as u16);
        inode.touch(self.dev.now(), false, false);
        self.write_inode(ino, &inode)
    }

    /// A new inode for `name` in `dir` (none there yet), written, without the name: its
    /// number and type. One that cannot be made whole is freed again (nothing names it).
    fn create_inode(&mut self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<(u32, u8), i64> {
        if name.len() > NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        // (Only "no such name" lets it be made: a failed lookup is no proof of none.)
        match self.lookup(dir, name) {
            Ok(_) => return Err(EEXIST),
            Err(ENOENT) => {}
            Err(e) => return Err(e),
        }
        let is_dir = matches!(kind, NewNode::Dir);
        let ino = self.alloc_inode(is_dir, self.group_of(dir))?;
        match self.init_inode(dir, ino, kind, perm) {
            Ok(ftype) => Ok((ino, ftype)),
            Err(e) => {
                // (The parent's count is the last step: not reached.)
                self.unmake(ino, dir, is_dir, false);
                Err(e)
            }
        }
    }

    /// Frees inode `ino` that nothing names and nothing on the disk counts on (a creation
    /// that did not go through), with a directory's link in its parent if it got it.
    fn unmake(&mut self, ino: u32, dir: u32, is_dir: bool, counted: bool) {
        match self.read_inode(ino) {
            Ok(inode) if inode.mode() != 0 => {
                if is_dir && counted {
                    let _ = self.adjust_links(dir, -1);
                }
                let _ = self.release(ino, inode);
            }
            _ => {
                let _ = self.free_inode(ino, is_dir);
            }
        }
    }

    /// Writes new inode `ino` (in `dir`) and what it holds; its type for its entry. The
    /// name comes later (`Ext2::create`: after the inode is durable).
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
                    if target.len() > self.block_size {
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
            NewNode::Socket => {
                inode.set_mode((S_IFSOCK | perm & 0o7777) as u16);
                inode.set_links(1);
                FT_SOCK
            }
        };
        self.write_inode(ino, &inode)?;
        // A directory's parent counts its ".." from now on (before the name exists).
        if is_dir {
            self.adjust_links(dir, 1)?;
        }
        Ok(ftype)
    }

    /// Frees an inode whose last link is gone, with all its blocks.
    /// (Its generation goes up: a handle of the file is stale, also before
    /// the number is reused.)
    fn release(&mut self, ino: u32, mut inode: RawInode) -> Result<(), i64> {
        let dir = inode.is_dir();
        self.truncate(ino, &mut inode, 0)?;
        inode.set_links(0);
        inode.set_generation(inode.generation().wrapping_add(1));
        let now = self.dev.now().max(1);
        put32(&mut inode.0, 20, now); // dtime (never 0: that is "in use")
        self.write_inode(ino, &inode)?;
        self.free_inode(ino, dir)
    }

    fn unlink(&mut self, dir: u32, name: &str, want_dir: bool) -> Result<(), i64> {
        if name == "." || name == ".." {
            return Err(EINVAL);
        }
        let ino = self.lookup(dir, name)?;
        let inode = self.read_inode(ino)?;
        match (inode.is_dir(), want_dir) {
            (true, false) => return Err(EISDIR),
            (false, true) => return Err(ENOTDIR),
            // (Empty, and no more links than "." and its name: a directory whose link
            // count another directory's ".." is still in, or that a rename which broke off
            // named twice, stays, so that nothing points to it once it is freed.)
            (true, true) if inode.links() > 2 || !self.dir_is_empty(ino)? => return Err(ENOTEMPTY),
            _ => {}
        }
        self.dir_remove(dir, name)?;
        self.drop_link(dir, ino, inode)
    }

    /// `ino` (as `inode`) lost a name in `dir`: one link less (a directory with one name:
    /// none, and `dir` loses its ".." link). Nothing of that is written now: the inode keeps
    /// its links (on the disk, too) and `dir` its count until the name's removal is
    /// durable; then `Ext2::take_unlinked` applies the changes to the inodes as they are
    /// then (`unlinked`), so no state of the disk has more names than links.
    fn drop_link(&mut self, dir: u32, ino: u32, mut inode: RawInode) -> Result<(), i64> {
        inode.touch(self.dev.now(), false, false);
        self.write_inode(ino, &inode)?;
        // (A directory has "." and its name; one with more, a rename's that broke off,
        // keeps a name, and its ".." stays.)
        let _ = dir;
        let last = if inode.is_dir() { inode.links() <= 2 } else { inode.links() <= 1 };
        self.unlinked.push(Dropped { ino, last });
        Ok(())
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
            dir = self.lookup(dir, "..")?;
        }
        Err(ELOOP)
    }

    /// What a rename of `oname` in `odir` to `nname` in `ndir` is to do (`Ext2::rename_unless`
    /// does it, step by step): None if nothing (the names are one file's already, the old
    /// one gone).
    fn rename_plan(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str) -> Result<Option<RenamePlan>, i64> {
        if nname.len() > NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        let ino = self.lookup(odir, oname)?;
        let inode = self.read_inode(ino)?;
        if inode.is_dir() && self.is_ancestor(ino, ndir)? {
            return Err(EINVAL);
        }
        let ftype = match inode.mode() as u32 & S_IFMT {
            S_IFDIR => FT_DIR,
            S_IFLNK => FT_SYMLINK,
            S_IFSOCK => FT_SOCK,
            _ => FT_REG,
        };
        // (Only "no such name" means none: another failure is the rename's.)
        let existing = match self.lookup(ndir, nname) {
            Ok(existing) => Some(existing),
            Err(ENOENT) => None,
            Err(e) => return Err(e),
        };
        let mut plan = RenamePlan { ino, is_dir: inode.is_dir(), ftype, replaced: None, named: false };
        match existing {
            // Both names the file's: a rename that broke off after its new name (there are
            // no hard links otherwise) is finished; the same name is nothing to do.
            Some(e) if e == ino => {
                if odir == ndir && oname == nname {
                    return Ok(None);
                }
                // Two names of one file: hard links (rename does nothing then, as POSIX
                // says), or a rename of this mount that broke off after its new name (it is
                // finished). A directory has no hard links: two names are a broken rename.
                // (After a crash a file's broken rename looks like hard links: kept.)
                if !inode.is_dir() && !self.broken_renames.contains(&ino) {
                    return Ok(None);
                }
                plan.named = true;
            }
            Some(e) => {
                let ex = self.read_inode(e)?;
                match (ex.is_dir(), inode.is_dir()) {
                    (true, false) => return Err(EISDIR),
                    (false, true) => return Err(ENOTDIR),
                    (true, true) if ex.links() > 2 || !self.dir_is_empty(e)? => return Err(ENOTEMPTY),
                    _ => {}
                }
                plan.replaced = Some(e);
            }
            None => {}
        }
        Ok(Some(plan))
    }

    fn readlink(&mut self, ino: u32) -> Result<String, i64> {
        let mut inode = self.read_inode(ino)?;
        if inode.mode() as u32 & S_IFMT != S_IFLNK {
            return Err(EINVAL);
        }
        let len = inode.size() as usize;
        let bytes = if inode.fast_symlink() {
            inode.0[40..40 + len.min(FAST_SYMLINK_MAX)].to_vec()
        } else {
            let blk = self.bmap(ino, &mut inode, 0, false)?;
            self.read_block(blk)?[..len.min(self.block_size)].to_vec()
        };
        String::from_utf8(bytes).map_err(|_| EIO)
    }

    // ------------------------------------- the ring path (see `Ext2::reserve`)

    /// Inode `ino` if it is in use: in range, allocated (it has a mode) and
    /// not freed (no deletion time; an unlinked inode still open is in
    /// use). ENOENT otherwise.
    fn live_inode(&mut self, ino: u32) -> Result<RawInode, i64> {
        let count = self.inodes_per_group as u64 * self.groups.len() as u64;
        if ino == 0 || ino as u64 > count {
            return Err(ENOENT);
        }
        let inode = self.read_inode(ino)?;
        let listed = self.orphans.contains_key(&ino) || self.releasing.contains(&ino);
        if inode.mode() == 0 || (le32(&inode.0, 20) != 0 && !listed) {
            return Err(ENOENT);
        }
        let g = self.group_of(ino);
        if !self.test_bit(self.groups[g].inode_bitmap, (ino - 1) % self.inodes_per_group)? {
            return Err(ENOENT);
        }
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
    /// bitmap, with a mode and no links (as e2fsck and ext4 check an entry).
    fn may_be_orphan(&mut self, ino: u32) -> Result<bool, i64> {
        if ino < self.first_ino().max(1) || ino > self.inode_count() {
            return Ok(false);
        }
        let g = self.group_of(ino);
        if !self.test_bit(self.groups[g].inode_bitmap, (ino - 1) % self.inodes_per_group)? {
            return Ok(false);
        }
        let inode = self.read_inode(ino)?;
        Ok(inode.mode() != 0 && inode.links() == 0)
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

    /// Writes `inode` (its last link just went; the name's removal is durable) with no
    /// links and, with an orphan list, at the list's head: its deletion time is the old
    /// head. The superblock's head is set by `set_last_orphan` after this reached the disk.
    fn add_orphan(&mut self, ino: u32, inode: &mut RawInode) -> Result<(), i64> {
        inode.set_links(0);
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
        Ok(())
    }

    /// Takes `ino` off the list (before it is freed): the head moves on, or its
    /// predecessor points past it. Whether it was on the list.
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
            if self.groups[g].free_blocks == 0 {
                continue;
            }
            let first = fdb + g as u32 * bpg;
            let limit = bpg.min(self.blocks_count - first);
            let from = if i == 0 { start } else { 0 };
            if let Some(bit) = self.find_bit(self.groups[g].block_bitmap, limit, from, Some(first))? {
                self.reserved.insert(first + bit);
                return Ok(first + bit);
            }
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
            // A fresh block (a failed commit's) reads as zeros, a cached
            // one may be newer than the device's: the caller copies.
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
                    result = self.link_block(r.ino, &mut inode, run.file_block + i as u64, block);
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
    pub fn mount_with_cache(mut dev: D, cache_bytes: usize) -> Result<Self, &'static str> {
        let mut sb = [0u8; 1024];
        dev.read(2, &mut sb).map_err(|_| "cannot read the superblock")?;
        if le16(&sb, 56) != MAGIC {
            return Err("not an ext2 filesystem");
        }
        let rev = le32(&sb, 76);
        let (inode_size, incompat, ro_compat) = if rev >= 1 {
            (le16(&sb, 88) as usize, le32(&sb, 96), le32(&sb, 100))
        } else {
            (128, 0, 0)
        };
        if incompat & !INCOMPAT_FILETYPE != 0 || ro_compat & !RO_COMPAT_SUPPORTED != 0 {
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
        // A group's bitmaps are one block each: its sizes must fit one, and
        // an inode must fit a block (or the bitmap and table arithmetic
        // would index past them).
        let bits = 8 * block_size as u32;
        if blocks_per_group == 0 || inodes_per_group == 0 || blocks_count <= first_data_block {
            return Err("corrupt superblock");
        }
        if blocks_per_group > bits || inodes_per_group > bits || inode_size > block_size || !inode_size.is_power_of_two() {
            return Err("corrupt superblock: group or inode sizes do not fit a block");
        }
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
            groups: Vec::new(),
            cache: BlockCache::new(cache_bytes / block_size),
            sb,
            super_dirty: false,
            written: false,
            fresh: BTreeSet::new(),
            unlinked: Vec::new(),
            orphans: BTreeMap::new(),
            orphan_prev: BTreeMap::new(),
            orphan_failures: 0,
            releasing: BTreeSet::new(),
            unlisted: BTreeSet::new(),
            deferred: Vec::new(),
            after_commit: Vec::new(),
            broken_renames: BTreeSet::new(),
            pending: false,
            broken: false,
            has_orphan_list: rev >= 1,
            mount_state: le16(&sb, 58),
            in_use: false,
            marked: false,
            frees: 0,
            reserved: BTreeSet::new(),
            reserved_promised: BTreeMap::new(),
            promises: BTreeMap::new(),
            promised: 0,
            spending: false,
            new_meta: BTreeSet::new(),
            freed: BlockSet::new(),
            unflushed: false,
        };
        let count = (blocks_count - first_data_block).div_ceil(blocks_per_group) as usize;
        for g in 0..count {
            let byte = g * 32;
            let block = st.gdt_block + (byte / block_size) as u32;
            let buf = st.read_block(block).map_err(|_| "cannot read the group descriptors")?;
            let o = byte % block_size;
            st.groups.push(Group {
                block_bitmap: le32(&buf, o),
                inode_bitmap: le32(&buf, o + 4),
                inode_table: le32(&buf, o + 8),
                free_blocks: le16(&buf, o + 12),
                free_inodes: le16(&buf, o + 14),
                used_dirs: le16(&buf, o + 16),
            });
        }
        let mut fs = Ext2 { st };
        // The orphan list as it is (freed by `recover_orphans`, if the mounter says so).
        let cut = fs.st.load_orphans();
        fs.commit(cut).map_err(|_| "cannot read the orphan list")?;
        Ok(fs)
    }

    pub fn stat(&mut self, ino: u32) -> Result<Stat, i64> {
        let i = self.st.read_inode(ino);
        let i = self.looked(i)?;
        Ok(Stat {
            mode: i.mode() as u32,
            size: i.size(),
            links: i.links() as u32,
            atime: i.atime(),
            mtime: i.mtime(),
            ctime: i.ctime(),
            generation: i.generation(),
        })
    }

    pub fn read(&mut self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let result = self.st.read(ino, off, buf);
        self.looked(result)
    }

    /// Before an operation that changes anything: a commit that failed is retried alone
    /// first (module comment, "Failed commits"); EIO if it fails again, or if the cache
    /// cannot be trusted any more (`broken`). In use, the disk then says "not clean"
    /// ("Clean"), whatever the superblock said before (also after a failed attempt to
    /// say "clean").
    fn ready(&mut self) -> Result<(), i64> {
        if self.st.broken {
            return Err(EIO);
        }
        if self.st.pending {
            self.st.commit()?;
        }
        // Link drops that waited for that commit (it removed their names).
        if !self.st.after_commit.is_empty() {
            let mut todo = core::mem::take(&mut self.st.after_commit).into_iter();
            let mut failed = None;
            for a in todo.by_ref() {
                let done = match a {
                    AfterCommit::Drop { dir, ino } => self.st.read_inode(ino).and_then(|inode| self.st.drop_link(dir, ino, inode)),
                    AfterCommit::Links { ino, delta } => self.st.adjust_links(ino, delta),
                };
                if let Err(e) = done {
                    // (It and the rest stay to do.)
                    self.st.after_commit.push(a);
                    failed = Some(e);
                    break;
                }
            }
            self.st.after_commit.extend(todo);
            // (Who holds an inode whose last link went is the owner's to know.)
            let gone = self.take_unlinked(true, &|_| true);
            self.st.deferred.extend(gone.into_iter().map(|(ino, _)| ino));
            if let Some(e) = failed {
                return Err(e);
            }
        }
        // (A commit of that failed: nothing new before its retry.)
        if self.st.pending {
            return Err(EIO);
        }
        if self.st.in_use {
            let state = self.st.mount_state & !STATE_VALID;
            if le16(&self.st.sb, 58) != state {
                put16(&mut self.st.sb, 58, state);
                self.st.super_dirty = true;
                self.st.commit()?;
            }
            self.st.marked = true;
        }
        if self.st.pending {
            return Err(EIO);
        }
        Ok(())
    }

    /// The inodes nobody holds that a failed commit kept from being freed (an immediate
    /// free's, a recovery's), for the owner to free (`release`) when no one holds them: it
    /// knows who holds what. Taken: asked again, they are not listed again.
    pub fn take_deferred(&mut self) -> Vec<u32> {
        core::mem::take(&mut self.st.deferred)
    }

    /// The filesystem is in use (its owner has clients) or not (module comment, "Clean").
    /// In use: the disk is marked "not clean" before the first change goes out (nothing is
    /// written now: a read-only disk serves reads all the same). Not in use: once every
    /// change is on the disk (committed and flushed first) and nothing is owed that only
    /// e2fsck or a later release would repair (`owed`), the state it had at mount is
    /// written back, in a commit of its own (if that commit fails, the superblock says
    /// "not clean" again: the retry writes that, never "clean"). Whether the disk says
    /// "clean" (cleanly unmounted) now.
    pub fn set_in_use(&mut self, in_use: bool) -> Result<bool, i64> {
        self.st.in_use = in_use;
        if !in_use && self.st.marked && !self.owed() {
            self.ready()?;
            // Everything first, then the state, each flushed: the state never says
            // "clean" over metadata that did not reach the disk.
            self.st.commit()?;
            put16(&mut self.st.sb, 58, self.st.mount_state);
            self.st.super_dirty = true;
            if let Err(e) = self.st.commit() {
                put16(&mut self.st.sb, 58, self.st.mount_state & !STATE_VALID);
                return Err(e);
            }
            self.st.marked = false;
        }
        Ok(!self.st.pending && le16(&self.st.sb, 58) & STATE_VALID != 0)
    }

    /// Whether repairs are owed (the disk is not clean even when everything is written):
    /// inodes whose last link went that are still allocated (listed or not), and those a
    /// failed commit could not list.
    pub fn owed(&self) -> bool {
        let st = &self.st;
        !st.orphans.is_empty()
            || !st.unlisted.is_empty()
            || !st.releasing.is_empty()
            || !st.deferred.is_empty()
            || !st.after_commit.is_empty()
            || st.orphan_failures > 0
    }

    /// Whether the cache holds changes that must never reach the disk (an operation
    /// failed part way through a free): every change fails from now on; the owner mounts
    /// again (diskfs restarts) to go on from the disk's state.
    pub fn broken(&self) -> bool {
        self.st.broken
    }

    /// Ends an operation that changes nothing: what a failed commit left is tried again,
    /// but its failure is not the reader's (a read-only or failing disk still serves
    /// reads).
    fn looked<T>(&mut self, result: Result<T, i64>) -> Result<T, i64> {
        if self.st.pending && !self.st.broken {
            let _ = self.st.commit();
        }
        result
    }

    /// Commits what `result`'s operation changed; its error wins. Every operation that
    /// changes something ends here, so changes a failed commit left are retried with the
    /// next one (reading ones end in `looked`).
    fn commit<T>(&mut self, result: Result<T, i64>) -> Result<T, i64> {
        let committed = self.st.commit();
        let value = result?;
        committed.map(|_| value)
    }

    pub fn write(&mut self, ino: u32, off: u64, data: &[u8]) -> Result<usize, i64> {
        self.ready()?;
        let result = self.st.write(ino, off, data);
        self.commit(result)
    }

    pub fn truncate(&mut self, ino: u32, len: u64) -> Result<(), i64> {
        self.ready()?;
        let mut inode = self.st.live_inode(ino)?;
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        if len > self.st.max_file_size() {
            return Err(EFBIG);
        }
        // A truncation that fails after it freed blocks the inode on the disk still points
        // to: none of it may reach the disk (`broken`). One that fails before changed
        // nothing that matters.
        let frees = self.st.frees;
        if let Err(e) = self.st.truncate(ino, &mut inode, len) {
            if self.st.frees != frees {
                self.st.broken = true;
            }
            return Err(e);
        }
        self.commit(Ok(()))
    }

    pub fn list(&mut self, dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
        let result = self.st.list(dir);
        self.looked(result)
    }

    pub fn lookup(&mut self, dir: u32, name: &str) -> Result<u32, i64> {
        let result = self.st.lookup(dir, name);
        self.looked(result)
    }

    /// The inode is made durable before its name (each committed: no name ever points to
    /// an inode the disk does not have), and a creation whose name cannot be added deletes
    /// its durable inode in order (no links, then freed) before its parent stops counting
    /// a directory's "..".
    pub fn create(&mut self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        self.ready()?;
        let is_dir = matches!(kind, NewNode::Dir);
        let (ino, ftype) = match self.st.create_inode(dir, name, kind, perm) {
            Ok(made) => made,
            Err(e) => {
                let _ = self.st.commit();
                return Err(e);
            }
        };
        if let Err(e) = self.st.commit() {
            // (Nothing names it: allocated and freed in the cache, the retry writes both.)
            self.st.unmake(ino, dir, is_dir, true);
            return Err(e);
        }
        let named = self.st.dir_add(dir, name, ino, ftype);
        if named.is_ok() || self.st.lookup(dir, name) == Ok(ino) {
            // (Named, in the cache at least: the retry of a failed commit writes it.)
            let _ = self.st.commit();
            return Ok(ino);
        }
        // Not named: the inode deleted on the disk, then freed, then the parent's count.
        if let Ok(mut inode) = self.st.read_inode(ino) {
            inode.set_links(0);
            put32(&mut inode.0, 20, self.st.dev.now().max(1));
            if self.st.write_inode(ino, &inode).is_ok() && self.st.commit().is_ok() {
                if self.st.checks() {
                    self.st.check_free(ino);
                }
                if self.st.release(ino, inode).is_ok() && self.st.commit().is_ok() && is_dir {
                    let _ = self.st.adjust_links(dir, -1);
                    let _ = self.st.commit();
                }
            }
        }
        named.map(|_| ino)
    }

    /// Inodes that lost their last link during the last operation, with their generations:
    /// those `in_use` says someone uses go on the orphan list (returned), the others are
    /// freed once the names' removal is durable. Every step is committed before the next
    /// (module comment, "Failed commits"); one whose commit fails ends the sequence there:
    /// the used inodes it could not list are returned all the same (`unlisted`), the unused
    /// ones it could not free are the filesystem's to free once it can (`deferred`). If the operation itself failed part
    /// way (names may be gone in the cache), they keep their links and are left alone:
    /// freeing them could go out with the names unordered (e2fsck takes them).
    fn take_unlinked(&mut self, ok: bool, in_use: &dyn Fn(u32) -> bool) -> Vec<(u32, u32)> {
        let dropped: Vec<Dropped> = core::mem::take(&mut self.st.unlinked);
        if !ok {
            // (They keep their links: names may be gone in the cache, unordered.)
            self.st.orphan_failures += dropped.iter().filter(|d| d.last).count() as u32;
            return Vec::new();
        }
        // Once the names' removal is durable (after a failed commit, once its retry wrote
        // it: `after_commit`): one link less for an inode that keeps a name, applied to the
        // inode as it is then.
        let deltas: Vec<u32> = dropped.iter().filter(|d| !d.last).map(|d| d.ino).collect();
        let durable = self.st.commit().is_ok();
        if !deltas.is_empty() {
            for ino in deltas {
                if !durable || self.st.adjust_links(ino, -1).is_err() {
                    self.st.after_commit.push(AfterCommit::Links { ino, delta: -1 });
                }
            }
            if durable {
                let _ = self.st.commit();
            }
        }
        // The inodes whose last link went, as they are now (no links from here on).
        let mut gone = Vec::new();
        for d in dropped.into_iter().filter(|d| d.last) {
            match self.st.read_inode(d.ino) {
                Ok(mut inode) => {
                    inode.set_links(0);
                    gone.push((d.ino, inode));
                }
                // (Unreadable: it keeps its links; e2fsck's.)
                Err(_) => self.st.orphan_failures += 1,
            }
        }
        let (used, unused): (Vec<_>, Vec<_>) = gone.into_iter().partition(|&(ino, _)| in_use(ino));
        let out = self.orphan_unlinked(used);
        if unused.is_empty() {
            return out;
        }
        // The names' removal first (the inodes keep their links on the disk until then),
        // then the inodes deleted (no links, a deletion time), then their bits and blocks
        // freed: a crash in between leaves space for e2fsck to give back, never a free block
        // or inode that an inode on the disk still has.
        if self.st.commit().is_err() {
            // Nobody holds these: the owner frees them once it can (`take_deferred`).
            let held = self.unlisted(unused);
            self.st.deferred.extend(held.into_iter().map(|(ino, _)| ino));
            return out;
        }
        let now = self.st.dev.now().max(1);
        for (ino, inode) in &unused {
            let mut gone = inode.clone();
            gone.set_links(0);
            put32(&mut gone.0, 20, now);
            let _ = self.st.write_inode(*ino, &gone);
        }
        if self.st.commit().is_err() {
            // Deleted in the cache (the next commit writes that alone): the owner frees
            // them once it can (`take_deferred`).
            for (ino, _) in unused {
                self.st.releasing.insert(ino);
                self.st.deferred.push(ino);
            }
            return out;
        }
        // One at a time, each committed: every free starts from a durable state. What a
        // failure leaves waits to be freed (deleted on the disk already), as any a commit
        // kept back.
        let mut rest = unused.into_iter();
        while let Some((ino, mut inode)) = rest.next() {
            inode.set_links(0);
            put32(&mut inode.0, 20, now);
            let frees = self.st.frees;
            let parent = self.st.dotdot(ino, &inode);
            if self.st.checks() {
                self.st.check_free(ino);
            }
            if self.st.release(ino, inode).is_err() {
                if self.st.frees != frees {
                    self.st.broken = true;
                    break;
                }
                self.st.releasing.insert(ino);
                self.st.deferred.push(ino);
                continue;
            }
            self.freed_dir(parent);
            if self.st.pending {
                for (ino, _) in rest {
                    self.st.releasing.insert(ino);
                    self.st.deferred.push(ino);
                }
                break;
            }
        }
        out
    }

    /// Inodes whose last link went that cannot be put on the orphan list now (a commit
    /// failed): they keep the links they have in the cache (no name may ever point to an
    /// inode without links), stay allocated until their release (which deletes them on the
    /// disk before it frees them), and are counted (`orphan_failures`): a crash before
    /// their release leaves them to e2fsck.
    fn unlisted(&mut self, gone: Vec<(u32, RawInode)>) -> Vec<(u32, u32)> {
        let mut out = Vec::with_capacity(gone.len());
        for (ino, inode) in gone {
            self.st.unlisted.insert(ino);
            self.st.orphan_failures += 1;
            out.push((ino, inode.generation()));
        }
        out
    }

    /// Puts the inodes whose last link an operation took on the orphan list, in the order
    /// of the module comment ("Orphans"): the operation's changes (the names' removal; the
    /// inodes keep their links on the disk) are committed first, then each inode (no links,
    /// chained to the old head), then the head. After a failed commit the rest are
    /// `unlisted`: still usable until their release.
    fn orphan_unlinked(&mut self, gone: Vec<(u32, RawInode)>) -> Vec<(u32, u32)> {
        if gone.is_empty() {
            return Vec::new();
        }
        if self.st.commit().is_err() {
            return self.unlisted(gone);
        }
        let mut out = Vec::with_capacity(gone.len());
        let mut rest = gone.into_iter();
        while let Some((ino, mut inode)) = rest.next() {
            let listed = self.st.add_orphan(ino, &mut inode).and_then(|_| self.st.commit());
            if listed.is_err() {
                // Off the list again (the pending write of the inode says so too: no
                // deletion time; no links, its name's removal is durable).
                if let Some(head) = self.st.orphans.remove(&ino) {
                    self.st.orphan_prev.remove(&head);
                }
                put32(&mut inode.0, 20, 0);
                let _ = self.st.write_inode(ino, &inode);
                out.extend(self.unlisted(core::iter::once((ino, inode)).chain(rest).collect()));
                return out;
            }
            out.push((ino, inode.generation()));
            self.st.set_last_orphan(ino);
            if self.st.commit().is_err() {
                // (Listed: the next commit writes the head alone.)
                out.extend(self.unlisted(rest.collect()));
                return out;
            }
        }
        out
    }

    /// Removes `name`; returns the inodes whose last link went away. They
    /// stay allocated (on the orphan list) until `release`, so open files keep working.
    pub fn unlink(&mut self, dir: u32, name: &str, want_dir: bool) -> Result<Vec<u32>, i64> {
        self.unlink_unless(dir, name, want_dir, |_| true).map(|gone| gone.into_iter().map(|(ino, _)| ino).collect())
    }

    /// `unlink`, but an inode whose last link went is freed at once unless `in_use` says
    /// someone uses it (no orphan list to go through, and nothing to release). The inodes
    /// to release, with their generations.
    pub fn unlink_unless(&mut self, dir: u32, name: &str, want_dir: bool, in_use: impl Fn(u32) -> bool) -> Result<Vec<(u32, u32)>, i64> {
        self.ready()?;
        let result = self.st.unlink(dir, name, want_dir);
        let gone = self.take_unlinked(result.is_ok(), &in_use);
        self.done(result.map(|_| gone))
    }

    pub fn rename(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str) -> Result<Vec<u32>, i64> {
        self.rename_unless(odir, oname, ndir, nname, |_| true).map(|gone| gone.into_iter().map(|(ino, _)| ino).collect())
    }

    /// `rename`; a replaced inode is freed at once unless `in_use` (see `unlink_unless`).
    ///
    /// The steps, each committed before the next, so that no state of the disk has a name
    /// more than its inode has links (a crash, or a failed commit that ends the steps,
    /// leaves at most a link too many: e2fsck's): the file gains the link its new name
    /// takes (a moved directory's new parent the one of its ".."), the new name comes (over
    /// an existing name: its entry takes the file in place, nothing to allocate), the old
    /// name goes, and then the links the old name and the replaced inode had go (an
    /// inode's last, as an unlink's). A rename that broke off after its new name is
    /// finished by the next rename of the same names.
    pub fn rename_unless(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str, in_use: impl Fn(u32) -> bool) -> Result<Vec<(u32, u32)>, i64> {
        self.ready()?;
        let Some(p) = self.st.rename_plan(odir, oname, ndir, nname)? else { return Ok(Vec::new()) };
        let moves_dir = p.is_dir && odir != ndir;
        let mut gone = Vec::new();
        if !p.named {
            // The link the new name takes (a moved directory's new parent: the link of its
            // ".."). Undone as far as it went if it cannot be committed.
            self.st.adjust_links(p.ino, 1)?;
            if moves_dir {
                if let Err(e) = self.st.adjust_links(ndir, 1) {
                    let _ = self.st.adjust_links(p.ino, -1);
                    return Err(e);
                }
            }
            if let Err(e) = self.st.commit() {
                let _ = self.st.adjust_links(p.ino, -1);
                if moves_dir {
                    let _ = self.st.adjust_links(ndir, -1);
                }
                return Err(e);
            }
            // A moved directory's ".." first (its new parent counts it already), then the
            // new name. Undone only when the new name is provably not there.
            let dotdot = if moves_dir { self.st.set_dotdot(p.ino, ndir) } else { Ok(()) };
            let named = dotdot.and_then(|_| match p.replaced {
                Some(_) => self.st.dir_set(ndir, nname, p.ino, p.ftype),
                None => self.st.dir_add(ndir, nname, p.ino, p.ftype),
            });
            if let Err(e) = named {
                match (p.replaced, self.st.lookup(ndir, nname)) {
                    // (It is there, in the cache at least: the rename goes on.)
                    (_, Ok(i)) if i == p.ino => {}
                    // Provably not named: no such name, or the replaced inode's still.
                    (None, Err(ENOENT)) | (Some(_), Ok(_)) => {
                        // The ".." back, and the links.
                        if moves_dir {
                            let _ = self.st.set_dotdot(p.ino, odir);
                        }
                        let _ = self.st.adjust_links(p.ino, -1);
                        if moves_dir {
                            let _ = self.st.adjust_links(ndir, -1);
                        }
                        let _ = self.st.commit();
                        return Err(e);
                    }
                    // Not known: the extra links stay (a link too many, never a name).
                    _ => {
                        self.st.broken_renames.insert(p.ino);
                        return Err(e);
                    }
                }
            }
            if self.st.commit().is_err() {
                // (Written by the retry, alone: the replaced inode's link goes after.)
                if let Some(e) = p.replaced {
                    self.st.after_commit.push(AfterCommit::Drop { dir: ndir, ino: e });
                }
                self.st.broken_renames.insert(p.ino);
                return Err(EIO);
            }
            // The new name is durable: the replaced inode loses its link (an unlink's).
            if let Some(e) = p.replaced {
                let dropped = self.st.read_inode(e).and_then(|ex| self.st.drop_link(ndir, e, ex));
                if dropped.is_err() {
                    self.st.after_commit.push(AfterCommit::Drop { dir: ndir, ino: e });
                }
                gone = self.take_unlinked(true, &in_use);
            }
        } else if moves_dir && self.st.lookup(p.ino, "..") != Ok(ndir) {
            // Finishing a broken rename of a directory: its ".." to the new parent first
            // (which counts it since the rename's first step).
            let dotdot = self.st.set_dotdot(p.ino, ndir).and_then(|_| self.st.commit());
            if let Err(e) = dotdot {
                return Err(e);
            }
        }
        // The old name, then the links it (a moved directory's "..": its old parent's) had.
        // Whatever cannot be done now is queued (after the retry of a failed commit) or left
        // to a rename of the same names again; what the caller was to hold is the owner's.
        self.st.broken_renames.insert(p.ino);
        let removed = self.st.dir_remove(odir, oname);
        let gone_old = match self.st.lookup(odir, oname) {
            Err(ENOENT) => true,
            _ => false,
        };
        if removed.is_err() && !gone_old {
            self.st.deferred.extend(gone.iter().map(|&(ino, _)| ino));
            return Err(removed.err().unwrap_or(EIO));
        }
        let mut drops = vec![(p.ino, -1)];
        if moves_dir {
            drops.push((odir, -1));
        }
        if self.st.commit().is_ok() {
            drops.retain(|&(ino, delta)| self.st.adjust_links(ino, delta).is_err());
        }
        for (ino, delta) in drops {
            self.st.after_commit.push(AfterCommit::Links { ino, delta });
        }
        self.st.broken_renames.remove(&p.ino);
        if let Err(e) = removed {
            self.st.deferred.extend(gone.iter().map(|&(ino, _)| ino));
            return Err(e);
        }
        self.done(Ok(gone))
    }

    /// Ends an unlink or rename: committed, but if only the commit fails the operation
    /// still happened (it is in the cache, and the next commit writes it, alone: `ready`),
    /// and the caller gets the inodes to hold and release: none is lost.
    fn done(&mut self, result: Result<Vec<(u32, u32)>, i64>) -> Result<Vec<(u32, u32)>, i64> {
        let _ = self.st.commit();
        result
    }

    /// Frees an inode returned by `unlink` or `rename`, with all its blocks. It leaves the
    /// orphan list first (committed: a crash then leaks it, never frees it twice). If that
    /// commit fails, nothing is freed: the next commit writes the removal alone (`ready`),
    /// and the release can be asked again. If the free's own commit fails, the inode is
    /// freed all the same (in the cache; the next commit writes it, alone): Ok.
    pub fn release(&mut self, ino: u32) -> Result<(), i64> {
        self.ready()?;
        self.free(ino)
    }

    /// Whether `ino` is in use and has no name left: `release` frees it (its last link
    /// went, also one kept with its links after a failed commit: `unlisted`).
    pub fn releasable(&mut self, ino: u32) -> bool {
        self.st.live_inode(ino).is_ok_and(|i| i.links() == 0 || self.st.unlisted.contains(&ino))
    }

    /// `release` once the filesystem is ready.
    fn free(&mut self, ino: u32) -> Result<(), i64> {
        let inode = self.st.live_inode(ino)?;
        let unlisted = self.st.unlisted.contains(&ino);
        if inode.links() != 0 && !unlisted {
            return Err(EINVAL);
        }
        if self.st.remove_orphan(ino)? {
            self.st.releasing.insert(ino);
            self.st.commit()?;
        } else if unlisted {
            // Deleted on the disk (no links, a deletion time) before anything is freed.
            let mut gone = inode;
            gone.set_links(0);
            put32(&mut gone.0, 20, self.st.dev.now().max(1));
            self.st.write_inode(ino, &gone)?;
            self.st.unlisted.remove(&ino);
            self.st.orphan_failures = self.st.orphan_failures.saturating_sub(1);
            self.st.releasing.insert(ino);
            self.st.commit()?;
        }
        let inode = self.st.read_inode(ino)?;
        let parent = self.st.dotdot(ino, &inode);
        let frees = self.st.frees;
        if self.st.checks() {
            self.st.check_free(ino);
        }
        if let Err(e) = self.st.release(ino, inode) {
            // Partly freed in the cache: none of it may reach the disk.
            if self.st.frees != frees {
                self.st.broken = true;
            }
            return Err(e);
        }
        self.st.releasing.remove(&ino);
        // (Freed: a commit that fails leaves only the durability to its retry, `ready`.)
        self.freed_dir(parent);
        Ok(())
    }

    /// After a directory's free: its parent stops counting its ".." once the free is
    /// durable (the count stays high until then: nothing that points to the parent is
    /// left uncounted).
    fn freed_dir(&mut self, parent: Option<u32>) {
        let durable = self.st.commit().is_ok();
        let Some(parent) = parent else { return };
        if !durable || self.st.adjust_links(parent, -1).is_err() {
            self.st.after_commit.push(AfterCommit::Links { ino: parent, delta: -1 });
            return;
        }
        let _ = self.st.commit();
    }

    /// Frees every inode on the orphan list but those `keep` names, one at a time: each
    /// leaves the list (committed) before it is freed (see `release`). The first mount
    /// since boot frees them all (their users went with the machine); a server restarted
    /// while its clients live keeps those they may still use. How many were freed; an
    /// error stops it, and the rest are freed by the filesystem itself once commits go
    /// through again (`deferred`).
    pub fn recover_orphans(&mut self, keep: impl Fn(u32) -> bool) -> Result<u32, i64> {
        self.ready()?;
        // The listed ones, and those a failed commit kept off the list (`unlisted`,
        // `releasing`).
        let mut listed: Vec<u32> = self.st.orphans.keys().copied().collect();
        listed.extend(self.st.unlisted.iter().chain(self.st.releasing.iter()).copied());
        let mut freed = 0;
        let mut rest = listed.into_iter().filter(|&i| !keep(i));
        while let Some(ino) = rest.next() {
            // (One may have gone meanwhile: its owner freed it.)
            if !self.releasable(ino) {
                continue;
            }
            if let Err(e) = self.release(ino) {
                // Nobody holds these: the filesystem frees them itself once it can.
                if !self.st.broken {
                    self.st.deferred.push(ino);
                    self.st.deferred.extend(rest);
                    // (One whose free is done, with a commit that failed, is skipped.)
                }
                return Err(e);
            }
            freed += 1;
        }
        Ok(freed)
    }

    /// The inodes on the orphan list now.
    pub fn orphans(&self) -> Vec<u32> {
        self.st.orphans.keys().copied().collect()
    }

    /// Whether `ino` is on the orphan list.
    pub fn is_orphan(&self, ino: u32) -> bool {
        self.st.orphans.contains_key(&ino)
    }

    /// Inodes whose last link went that could not be put on the orphan list (I/O errors),
    /// since the mount.
    pub fn orphan_failures(&self) -> u32 {
        self.st.orphan_failures
    }

    pub fn readlink(&mut self, ino: u32) -> Result<String, i64> {
        let result = self.st.readlink(ino);
        self.looked(result)
    }

    pub fn set_perm(&mut self, ino: u32, perm: u32) -> Result<(), i64> {
        self.ready()?;
        let mut inode = self.st.live_inode(ino)?;
        inode.set_mode((inode.mode() as u32 & S_IFMT | perm & 0o7777) as u16);
        let now = self.st.dev.now();
        inode.touch(now, false, false);
        let result = self.st.write_inode(ino, &inode);
        self.commit(result)
    }

    /// Sets the times given (seconds since 1970); the others stay.
    pub fn set_times(&mut self, ino: u32, atime: Option<u32>, mtime: Option<u32>, ctime: Option<u32>) -> Result<(), i64> {
        self.ready()?;
        let mut inode = self.st.live_inode(ino)?;
        inode.set_times(atime, mtime, ctime);
        let result = self.st.write_inode(ino, &inode);
        self.commit(result)
    }

    pub fn device(&self) -> &D {
        &self.st.dev
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.st.dev
    }

    /// Unmounts the filesystem and returns its device, after writing what
    /// a failed commit left (if the device lets it).
    pub fn into_device(mut self) -> D {
        let _ = self.st.commit();
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
        let st = &mut self.st;
        for m in st.promises.values_mut() {
            m.remove(&owner);
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
    /// data is not (only) on the device (a block cached or fresh after a
    /// failed commit, which `read` handles): the caller reads through
    /// `read` then.
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
    /// ready, let start; failing them would lose the write's blocks. Their changes go out
    /// with the next commit, after the data, as on any day.)
    pub fn link(&mut self, r: &Reservation, end: u64) -> Result<u64, i64> {
        self.st.link(r, end)
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
        let barrier = self.st.barrier();
        self.commit(barrier)
    }
}
