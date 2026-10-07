//! ext2 on the data disk (any `Device`): revision 1 with the `filetype` feature,
//! 1/2/4 KiB blocks, direct and single/double/triple indirect blocks.
//!
//! Metadata goes through a block cache (`cache.rs`); every public operation
//! commits its changes before it returns (written together, then one
//! flush), so a completed operation is durable. File data is read and
//! written straight from and to the device, in runs of contiguous blocks.
//! Metadata is kept consistent enough for `e2fsck` on the host to accept
//! the filesystem.

#![no_std]

extern crate alloc;

mod cache;

use cache::BlockCache;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Linux errno values used in results.
pub mod errno {
    pub const ENOENT: i64 = 2;
    pub const EIO: i64 = 5;
    pub const EEXIST: i64 = 17;
    pub const ENOTDIR: i64 = 20;
    pub const EISDIR: i64 = 21;
    pub const EINVAL: i64 = 22;
    pub const EFBIG: i64 = 27;
    pub const ENOSPC: i64 = 28;
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOTEMPTY: i64 = 39;
    pub const ELOOP: i64 = 40;
}
use errno::*;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
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
}

/// What `Ext2::create` makes.
pub enum NewNode {
    File,
    Dir,
    Symlink(String),
}

pub const ROOT_INO: u32 = 2;
const MAGIC: u16 = 0xef53;
const INCOMPAT_FILETYPE: u32 = 0x2;
const RO_COMPAT_SUPPORTED: u32 = 0x1 | 0x2; // sparse_super, large_file
const DIRECT: usize = 12;
/// Symlink targets shorter than this live in the block pointers ("fast").
const FAST_SYMLINK_MAX: usize = 60;

/// Memory for cached metadata blocks.
const CACHE_BYTES: usize = 1024 * 1024;

const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
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
    /// Inodes whose last link went away; the Ext2 wrapper decides whether
    /// to free them now or when the last open reference is dropped.
    unlinked: Vec<(u32, RawInode)>,
}

/// One directory entry as found on disk.
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
        match self.cache.insert(n, data, dirty) {
            Some((old, data)) => self.dev_write(old, &data),
            None => Ok(()),
        }
    }

    fn write_super(&mut self) -> Result<(), i64> {
        put32(&mut self.sb, 12, self.free_blocks);
        put32(&mut self.sb, 16, self.free_inodes);
        let now = self.dev.now();
        put32(&mut self.sb, 48, now);
        self.super_dirty = true;
        Ok(())
    }

    /// Writes the changed metadata (adjacent blocks in one request), then
    /// flushes the device if anything was written.
    fn commit(&mut self) -> Result<(), i64> {
        let dirty = self.cache.take_dirty();
        let mut i = 0;
        while i < dirty.len() {
            let mut j = i + 1;
            while j < dirty.len() && dirty[j].0 == dirty[j - 1].0 + 1 {
                j += 1;
            }
            let run: Vec<u8> = dirty[i..j].iter().flat_map(|(_, d)| d.iter().copied()).collect();
            self.dev_write(dirty[i].0, &run)?;
            i = j;
        }
        if self.super_dirty {
            self.super_dirty = false;
            self.written = true;
            self.dev.write(2, &self.sb).map_err(io)?;
        }
        if self.written {
            self.written = false;
            self.dev.flush().map_err(io)?;
        }
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
        Ok(())
    }

    /// Writes the contiguous data blocks from `first`; the device's copy
    /// is now the one that counts.
    fn write_data(&mut self, first: u32, buf: &[u8]) -> Result<(), i64> {
        self.cache.remove_range(first..first + (buf.len() / self.block_size) as u32);
        self.dev_write(first, buf)
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
        let g = ((ino - 1) / self.inodes_per_group) as usize;
        let group = self.groups.get(g).ok_or(EIO)?;
        let byte = ((ino - 1) % self.inodes_per_group) as usize * self.inode_size;
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

    /// Finds and sets a clear bit in a bitmap block; returns its index.
    fn take_bit(&mut self, bitmap_block: u32, limit: u32) -> Result<Option<u32>, i64> {
        let mut bitmap = self.read_block(bitmap_block)?;
        for bit in 0..limit {
            let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
            if bitmap[byte] & mask == 0 {
                bitmap[byte] |= mask;
                self.write_block(bitmap_block, &bitmap)?;
                return Ok(Some(bit));
            }
        }
        Ok(None)
    }

    fn clear_bit(&mut self, bitmap_block: u32, bit: u32) -> Result<(), i64> {
        let mut bitmap = self.read_block(bitmap_block)?;
        bitmap[(bit / 8) as usize] &= !(1u8 << (bit % 8));
        self.write_block(bitmap_block, &bitmap)
    }

    /// Allocates a block, preferring group `goal`; zeroed unless it is for
    /// file data the caller writes.
    fn alloc_block(&mut self, goal: usize, zero: bool) -> Result<u32, i64> {
        let n = self.groups.len();
        for g in (goal..n).chain(0..goal) {
            if self.groups[g].free_blocks == 0 {
                continue;
            }
            let first = self.first_data_block + g as u32 * self.blocks_per_group;
            let limit = self.blocks_per_group.min(self.blocks_count - first);
            if let Some(bit) = self.take_bit(self.groups[g].block_bitmap, limit)? {
                self.groups[g].free_blocks -= 1;
                self.free_blocks -= 1;
                self.write_group(g)?;
                self.write_super()?;
                let block = first + bit;
                if zero {
                    self.write_block(block, &vec![0u8; self.block_size])?;
                }
                return Ok(block);
            }
        }
        Err(ENOSPC)
    }

    fn free_block(&mut self, block: u32) -> Result<(), i64> {
        self.cache.remove(block);
        let rel = block - self.first_data_block;
        let g = (rel / self.blocks_per_group) as usize;
        self.clear_bit(self.groups[g].block_bitmap, rel % self.blocks_per_group)?;
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
            if let Some(bit) = self.take_bit(self.groups[g].inode_bitmap, self.inodes_per_group)? {
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
        let g = self.group_of(ino);
        self.clear_bit(self.groups[g].inode_bitmap, (ino - 1) % self.inodes_per_group)?;
        self.groups[g].free_inodes += 1;
        if dir {
            self.groups[g].used_dirs -= 1;
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
            let mut b = inode.block(fb as usize);
            if b == 0 && alloc {
                b = self.alloc_block(goal, zero_leaf)?;
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
        let mut blk = inode.block(root);
        if blk == 0 {
            if !alloc {
                return Ok((0, false));
            }
            blk = self.alloc_block(goal, true)?;
            inode.set_block(root, blk);
            inode.add_sectors(spb);
        }
        let mut new = false;
        for level in (0..depth).rev() {
            let idx = ((rel / p.pow(level)) % p) as usize;
            let mut table = self.read_block(blk)?;
            let mut next = le32(&table, idx * 4);
            if next == 0 {
                if !alloc {
                    return Ok((0, false));
                }
                // Level 0 is the data block itself.
                next = self.alloc_block(goal, level > 0 || zero_leaf)?;
                new = level == 0;
                put32(&mut table, idx * 4, next);
                self.write_block(blk, &table)?;
                inode.add_sectors(spb);
            }
            blk = next;
        }
        Ok((blk, new))
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
            let entry = le32(&table, i as usize * 4);
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
        inode.set_size(len);
        inode.touch(self.dev.now(), false, true);
        self.write_inode(ino, inode)
    }

    fn read(&mut self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let mut inode = self.read_inode(ino)?;
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
                let follows = if first == 0 { next == 0 } else { next == first + count as u32 };
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
        let mut inode = self.read_inode(ino)?;
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
                let mut block = if new { vec![0u8; bs] } else { self.data_block(first)? };
                block[bo..bo + chunk].copy_from_slice(&data[done..done + chunk]);
                self.write_data(first, &block)?;
                done += chunk;
                continue;
            }
            // Whole blocks: allocate a contiguous run, write it at once.
            let mut count = 1;
            while count < (data.len() - done) / bs {
                match self.bmap_data(ino, &mut inode, fb + count as u64) {
                    Ok((next, _)) if next == first + count as u32 => count += 1,
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
            self.write_data(first, &data[done..done + count * bs])?;
            done += count * bs;
            if result.is_err() {
                break;
            }
        }
        if off + done as u64 > inode.size() {
            inode.set_size(off + done as u64);
        }
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
                return self.write_block(blk, &block);
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

    fn create(&mut self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        if name.len() > NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        if self.lookup(dir, name).is_ok() {
            return Err(EEXIST);
        }
        let is_dir = matches!(kind, NewNode::Dir);
        let ino = self.alloc_inode(is_dir, self.group_of(dir))?;
        let result = self.init_inode(dir, ino, name, kind, perm);
        if result.is_err() {
            let _ = self.free_inode(ino, is_dir);
        }
        result.map(|_| ino)
    }

    fn init_inode(&mut self, dir: u32, ino: u32, name: &str, kind: &NewNode, perm: u32) -> Result<(), i64> {
        let goal = self.group_of(dir);
        let is_dir = matches!(kind, NewNode::Dir);
        let mut inode = RawInode([0; 128]);
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
        };
        self.write_inode(ino, &inode)?;
        self.dir_add(dir, name, ino, ftype)?;
        if is_dir {
            self.adjust_links(dir, 1)?;
        }
        Ok(())
    }

    /// Frees an inode whose last link is gone, with all its blocks.
    fn release(&mut self, ino: u32, mut inode: RawInode) -> Result<(), i64> {
        let dir = inode.is_dir();
        self.truncate(ino, &mut inode, 0)?;
        inode.set_links(0);
        let now = self.dev.now();
        put32(&mut inode.0, 20, now); // dtime
        self.write_inode(ino, &inode)?;
        self.free_inode(ino, dir)
    }

    fn unlink(&mut self, dir: u32, name: &str, want_dir: bool) -> Result<(), i64> {
        if name == "." || name == ".." {
            return Err(EINVAL);
        }
        let ino = self.lookup(dir, name)?;
        let mut inode = self.read_inode(ino)?;
        match (inode.is_dir(), want_dir) {
            (true, false) => return Err(EISDIR),
            (false, true) => return Err(ENOTDIR),
            (true, true) if !self.dir_is_empty(ino)? => return Err(ENOTEMPTY),
            _ => {}
        }
        self.dir_remove(dir, name)?;
        if inode.is_dir() {
            self.adjust_links(dir, -1)?;
            inode.set_links(0);
        } else {
            inode.set_links(inode.links().saturating_sub(1));
        }
        inode.touch(self.dev.now(), false, false);
        self.write_inode(ino, &inode)?;
        if inode.links() == 0 {
            self.unlinked.push((ino, inode));
        }
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

    fn rename(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str) -> Result<(), i64> {
        if nname.len() > NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        let ino = self.lookup(odir, oname)?;
        let inode = self.read_inode(ino)?;
        if inode.is_dir() && self.is_ancestor(ino, ndir)? {
            return Err(EINVAL);
        }
        if let Ok(existing) = self.lookup(ndir, nname) {
            if existing == ino {
                return Ok(());
            }
            let ex = self.read_inode(existing)?;
            match (ex.is_dir(), inode.is_dir()) {
                (true, false) => return Err(EISDIR),
                (false, true) => return Err(ENOTDIR),
                _ => {}
            }
            self.unlink(ndir, nname, ex.is_dir())?;
        }
        let ftype = if inode.is_dir() {
            FT_DIR
        } else if inode.mode() as u32 & S_IFMT == S_IFLNK {
            FT_SYMLINK
        } else {
            FT_REG
        };
        self.dir_add(ndir, nname, ino, ftype)?;
        self.dir_remove(odir, oname)?;
        if inode.is_dir() && odir != ndir {
            self.set_dotdot(ino, ndir)?;
            self.adjust_links(odir, -1)?;
            self.adjust_links(ndir, 1)?;
        }
        Ok(())
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
}

/// A mounted ext2 filesystem on device `D`. Single-threaded: the owner
/// serializes access.
pub struct Ext2<D: Device> {
    st: State<D>,
}

impl<D: Device> Ext2<D> {
    pub fn mount(mut dev: D) -> Result<Self, &'static str> {
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
        if blocks_per_group == 0 || inodes_per_group == 0 || blocks_count <= first_data_block {
            return Err("corrupt superblock");
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
            cache: BlockCache::new(CACHE_BYTES / block_size),
            sb,
            super_dirty: false,
            written: false,
            unlinked: Vec::new(),
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
        Ok(Ext2 { st })
    }

    pub fn stat(&mut self, ino: u32) -> Result<Stat, i64> {
        let i = self.st.read_inode(ino)?;
        Ok(Stat {
            mode: i.mode() as u32,
            size: i.size(),
            links: i.links() as u32,
            atime: i.atime(),
            mtime: i.mtime(),
            ctime: i.ctime(),
        })
    }

    pub fn read(&mut self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        self.st.read(ino, off, buf)
    }

    /// Commits what `result`'s operation changed; its error wins.
    fn commit<T>(&mut self, result: Result<T, i64>) -> Result<T, i64> {
        let committed = self.st.commit();
        let value = result?;
        committed.map(|_| value)
    }

    pub fn write(&mut self, ino: u32, off: u64, data: &[u8]) -> Result<usize, i64> {
        let result = self.st.write(ino, off, data);
        self.commit(result)
    }

    pub fn truncate(&mut self, ino: u32, len: u64) -> Result<(), i64> {
        let mut inode = self.st.read_inode(ino)?;
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        if len > self.st.max_file_size() {
            return Err(EFBIG);
        }
        let result = self.st.truncate(ino, &mut inode, len);
        self.commit(result)
    }

    pub fn list(&mut self, dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
        self.st.list(dir)
    }

    pub fn lookup(&mut self, dir: u32, name: &str) -> Result<u32, i64> {
        self.st.lookup(dir, name)
    }

    pub fn create(&mut self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        let result = self.st.create(dir, name, kind, perm);
        self.commit(result)
    }

    /// Inodes that lost their last link during the last operation. If the
    /// operation failed, nobody else can know about them, so they are freed.
    fn take_unlinked(&mut self, ok: bool) -> Vec<u32> {
        let gone: Vec<(u32, RawInode)> = core::mem::take(&mut self.st.unlinked);
        if ok {
            return gone.into_iter().map(|(ino, _)| ino).collect();
        }
        for (ino, inode) in gone {
            let _ = self.st.release(ino, inode);
        }
        Vec::new()
    }

    /// Removes `name`; returns the inodes whose last link went away. They
    /// stay allocated until `release`, so open files keep working.
    pub fn unlink(&mut self, dir: u32, name: &str, want_dir: bool) -> Result<Vec<u32>, i64> {
        let result = self.st.unlink(dir, name, want_dir);
        let gone = self.take_unlinked(result.is_ok());
        self.commit(result.map(|_| gone))
    }

    pub fn rename(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str) -> Result<Vec<u32>, i64> {
        let result = self.st.rename(odir, oname, ndir, nname);
        let gone = self.take_unlinked(result.is_ok());
        self.commit(result.map(|_| gone))
    }

    /// Frees an inode returned by `unlink` or `rename`, with all its blocks.
    pub fn release(&mut self, ino: u32) -> Result<(), i64> {
        let inode = self.st.read_inode(ino)?;
        if inode.links() != 0 {
            return Err(EINVAL);
        }
        let result = self.st.release(ino, inode);
        self.commit(result)
    }

    pub fn readlink(&mut self, ino: u32) -> Result<String, i64> {
        self.st.readlink(ino)
    }

    pub fn set_perm(&mut self, ino: u32, perm: u32) -> Result<(), i64> {
        let mut inode = self.st.read_inode(ino)?;
        inode.set_mode((inode.mode() as u32 & S_IFMT | perm & 0o7777) as u16);
        let now = self.st.dev.now();
        inode.touch(now, false, false);
        let result = self.st.write_inode(ino, &inode);
        self.commit(result)
    }

    pub fn device(&self) -> &D {
        &self.st.dev
    }

    /// Unmounts the filesystem and returns its device. Every operation
    /// committed its changes, so there is nothing left to write.
    pub fn into_device(self) -> D {
        self.st.dev
    }

    /// (block size, total blocks, free blocks, total inodes, free inodes)
    pub fn usage(&self) -> (u64, u64, u64, u64, u64) {
        let st = &self.st;
        let inodes = st.inodes_per_group as u64 * st.groups.len() as u64;
        (st.block_size as u64, st.blocks_count as u64, st.free_blocks as u64, inodes, st.free_inodes as u64)
    }
}
