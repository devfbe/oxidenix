//! ext2 on the ATA data disk: revision 1 with the `filetype` feature,
//! 1/2/4 KiB blocks, direct and single/double/triple indirect blocks.
//!
//! Every change goes straight to disk (no cache), and metadata is kept
//! consistent enough for `e2fsck` on the host to accept the filesystem.

use super::{Inode, NewNode, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use crate::drivers::{ata, rtc};
use crate::process::errno::*;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

pub const ROOT_INO: u32 = 2;
const MAGIC: u16 = 0xef53;
const INCOMPAT_FILETYPE: u32 = 0x2;
const RO_COMPAT_SUPPORTED: u32 = 0x1 | 0x2; // sparse_super, large_file
const DIRECT: usize = 12;
/// Symlink targets shorter than this live in the block pointers ("fast").
const FAST_SYMLINK_MAX: usize = 60;

const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_SYMLINK: u8 = 7;

fn io(_: ata::Error) -> i64 {
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
    fn touch(&mut self, atime: bool, mtime: bool) {
        let now = rtc::now() as u32;
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

struct State {
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

impl State {
    fn sectors_per_block(&self) -> u64 {
        (self.block_size / ata::SECTOR_SIZE) as u64
    }

    fn read_block(&self, n: u32) -> Result<Vec<u8>, i64> {
        let mut buf = vec![0u8; self.block_size];
        ata::read(n as u64 * self.sectors_per_block(), &mut buf).map_err(io)?;
        Ok(buf)
    }

    fn write_block(&self, n: u32, buf: &[u8]) -> Result<(), i64> {
        ata::write(n as u64 * self.sectors_per_block(), buf).map_err(io)
    }

    fn write_super(&self) -> Result<(), i64> {
        let mut sb = [0u8; 1024];
        ata::read(2, &mut sb).map_err(io)?;
        put32(&mut sb, 12, self.free_blocks);
        put32(&mut sb, 16, self.free_inodes);
        put32(&mut sb, 48, rtc::now() as u32);
        ata::write(2, &sb).map_err(io)
    }

    fn write_group(&self, g: usize) -> Result<(), i64> {
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

    fn read_inode(&self, ino: u32) -> Result<RawInode, i64> {
        let (block, off) = self.inode_location(ino)?;
        let buf = self.read_block(block)?;
        Ok(RawInode(buf[off..off + 128].try_into().unwrap()))
    }

    fn write_inode(&self, ino: u32, inode: &RawInode) -> Result<(), i64> {
        let (block, off) = self.inode_location(ino)?;
        let mut buf = self.read_block(block)?;
        buf[off..off + 128].copy_from_slice(&inode.0);
        self.write_block(block, &buf)
    }

    fn group_of(&self, ino: u32) -> usize {
        ((ino - 1) / self.inodes_per_group) as usize
    }

    /// Finds and sets a clear bit in a bitmap block; returns its index.
    fn take_bit(&self, bitmap_block: u32, limit: u32) -> Result<Option<u32>, i64> {
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

    fn clear_bit(&self, bitmap_block: u32, bit: u32) -> Result<(), i64> {
        let mut bitmap = self.read_block(bitmap_block)?;
        bitmap[(bit / 8) as usize] &= !(1u8 << (bit % 8));
        self.write_block(bitmap_block, &bitmap)
    }

    /// Allocates a zeroed block, preferring group `goal`.
    fn alloc_block(&mut self, goal: usize) -> Result<u32, i64> {
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
                self.write_block(block, &vec![0u8; self.block_size])?;
                return Ok(block);
            }
        }
        Err(ENOSPC)
    }

    fn free_block(&mut self, block: u32) -> Result<(), i64> {
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

    /// Disk block holding file block `fb`, allocating it (and any indirect
    /// blocks on the way) if `alloc`. Returns 0 for a hole when not allocating.
    fn bmap(&mut self, ino: u32, inode: &mut RawInode, fb: u64, alloc: bool) -> Result<u32, i64> {
        let goal = self.group_of(ino);
        let spb = self.sectors_per_block() as i64;
        let p = self.ptrs_per_block();
        if (fb as usize) < DIRECT {
            let mut b = inode.block(fb as usize);
            if b == 0 && alloc {
                b = self.alloc_block(goal)?;
                inode.set_block(fb as usize, b);
                inode.add_sectors(spb);
            }
            return Ok(b);
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
                return Ok(0);
            }
            blk = self.alloc_block(goal)?;
            inode.set_block(root, blk);
            inode.add_sectors(spb);
        }
        for level in (0..depth).rev() {
            let idx = ((rel / p.pow(level)) % p) as usize;
            let mut table = self.read_block(blk)?;
            let mut next = le32(&table, idx * 4);
            if next == 0 {
                if !alloc {
                    return Ok(0);
                }
                next = self.alloc_block(goal)?;
                put32(&mut table, idx * 4, next);
                self.write_block(blk, &table)?;
                inode.add_sectors(spb);
            }
            blk = next;
        }
        Ok(blk)
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
                    let mut buf = self.read_block(blk)?;
                    buf[(len % bs) as usize..].fill(0);
                    self.write_block(blk, &buf)?;
                }
            }
        }
        inode.set_size(len);
        inode.touch(false, true);
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
        let bs = self.block_size as u64;
        let mut done = 0;
        while done < n {
            let pos = off + done as u64;
            let bo = (pos % bs) as usize;
            let chunk = (self.block_size - bo).min(n - done);
            let blk = self.bmap(ino, &mut inode, pos / bs, false)?;
            if blk == 0 {
                buf[done..done + chunk].fill(0);
            } else {
                let data = self.read_block(blk)?;
                buf[done..done + chunk].copy_from_slice(&data[bo..bo + chunk]);
            }
            done += chunk;
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
        let bs = self.block_size as u64;
        let mut done = 0;
        let mut result = Ok(());
        while done < data.len() {
            let pos = off + done as u64;
            let bo = (pos % bs) as usize;
            let chunk = (self.block_size - bo).min(data.len() - done);
            let blk = match self.bmap(ino, &mut inode, pos / bs, true) {
                Ok(b) => b,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            let mut block = if chunk == self.block_size { vec![0u8; self.block_size] } else { self.read_block(blk)? };
            block[bo..bo + chunk].copy_from_slice(&data[done..done + chunk]);
            self.write_block(blk, &block)?;
            done += chunk;
        }
        if off + done as u64 > inode.size() {
            inode.set_size(off + done as u64);
        }
        inode.touch(false, true);
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
        inode.touch(false, true);
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
                inode.touch(false, true);
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
        inode.touch(false, false);
        self.write_inode(ino, &inode)
    }

    fn create(&mut self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        if name.len() > super::NAME_MAX {
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
        inode.touch(true, true);
        let ftype = match kind {
            NewNode::File => {
                inode.set_mode((S_IFREG | perm & 0o7777) as u16);
                inode.set_links(1);
                FT_REG
            }
            NewNode::Dir => {
                inode.set_mode((S_IFDIR | perm & 0o7777) as u16);
                inode.set_links(2);
                let blk = self.alloc_block(goal)?;
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
                    let blk = self.alloc_block(goal)?;
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
        put32(&mut inode.0, 20, rtc::now() as u32); // dtime
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
        inode.touch(false, false);
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
        if nname.len() > super::NAME_MAX {
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

pub struct Ext2 {
    state: Mutex<State>,
    cache: Mutex<BTreeMap<u32, Weak<Inode>>>,
    /// Unlinked inodes still open somewhere; freed when the last VFS
    /// reference goes away, so their numbers cannot be reused meanwhile.
    orphans: Mutex<BTreeSet<u32>>,
}

impl Ext2 {
    pub fn mount() -> Result<Arc<Ext2>, &'static str> {
        ata::init().ok_or("no data disk")?;
        let mut sb = [0u8; 1024];
        ata::read(2, &mut sb).map_err(|_| "cannot read the superblock")?;
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
        let mut state = State {
            block_size,
            inode_size,
            inodes_per_group: le32(&sb, 40),
            blocks_per_group,
            first_data_block,
            blocks_count,
            free_blocks: le32(&sb, 12),
            free_inodes: le32(&sb, 16),
            gdt_block: first_data_block + 1,
            groups: Vec::new(),
            unlinked: Vec::new(),
        };
        let count = (blocks_count - first_data_block).div_ceil(blocks_per_group) as usize;
        for g in 0..count {
            let byte = g * 32;
            let buf = state
                .read_block(state.gdt_block + (byte / block_size) as u32)
                .map_err(|_| "cannot read the group descriptors")?;
            let o = byte % block_size;
            state.groups.push(Group {
                block_bitmap: le32(&buf, o),
                inode_bitmap: le32(&buf, o + 4),
                inode_table: le32(&buf, o + 8),
                free_blocks: le16(&buf, o + 12),
                free_inodes: le16(&buf, o + 14),
                used_dirs: le16(&buf, o + 16),
            });
        }
        Ok(Arc::new(Ext2 {
            state: Mutex::new(state),
            cache: Mutex::new(BTreeMap::new()),
            orphans: Mutex::new(BTreeSet::new()),
        }))
    }

    /// The VFS inode for `ino`; the same disk inode always maps to the same
    /// `Arc<Inode>` while it is in use.
    pub fn inode(self: &Arc<Self>, ino: u32) -> Arc<Inode> {
        let mut cache = self.cache.lock();
        if let Some(i) = cache.get(&ino).and_then(Weak::upgrade) {
            return i;
        }
        cache.retain(|_, w| w.strong_count() > 0);
        let inode = Inode::disk(self.clone(), ino);
        cache.insert(ino, Arc::downgrade(&inode));
        inode
    }

    pub fn stat(&self, ino: u32) -> Result<RawInode, i64> {
        self.state.lock().read_inode(ino)
    }
    pub fn read(&self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        self.state.lock().read(ino, off, buf)
    }
    pub fn write(&self, ino: u32, off: u64, data: &[u8]) -> Result<usize, i64> {
        self.state.lock().write(ino, off, data)
    }
    pub fn truncate(&self, ino: u32, len: u64) -> Result<(), i64> {
        let mut st = self.state.lock();
        let mut inode = st.read_inode(ino)?;
        if !inode.is_reg() {
            return Err(EINVAL);
        }
        if len > st.max_file_size() {
            return Err(EFBIG);
        }
        st.truncate(ino, &mut inode, len)
    }

    /// Frees inodes that lost their last link, unless an open file still
    /// refers to them; those become orphans until `forget`.
    fn settle_unlinked(&self, st: &mut State) -> Result<(), i64> {
        for (ino, inode) in core::mem::take(&mut st.unlinked) {
            let open = self.cache.lock().get(&ino).is_some_and(|w| w.strong_count() > 0);
            if open {
                self.orphans.lock().insert(ino);
            } else {
                st.release(ino, inode)?;
            }
        }
        Ok(())
    }

    /// Called when the last VFS reference to `ino` is dropped.
    pub fn forget(&self, ino: u32) {
        if self.orphans.lock().remove(&ino) {
            let mut st = self.state.lock();
            if let Ok(inode) = st.read_inode(ino) {
                let _ = st.release(ino, inode);
            }
        }
    }
    pub fn list(&self, dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
        self.state.lock().list(dir)
    }
    pub fn lookup(&self, dir: u32, name: &str) -> Result<u32, i64> {
        self.state.lock().lookup(dir, name)
    }
    pub fn create(&self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        self.state.lock().create(dir, name, kind, perm)
    }
    pub fn unlink(&self, dir: u32, name: &str, want_dir: bool) -> Result<(), i64> {
        let mut st = self.state.lock();
        let result = st.unlink(dir, name, want_dir);
        self.settle_unlinked(&mut st)?;
        result
    }
    pub fn rename(&self, odir: u32, oname: &str, ndir: u32, nname: &str) -> Result<(), i64> {
        let mut st = self.state.lock();
        let result = st.rename(odir, oname, ndir, nname);
        self.settle_unlinked(&mut st)?;
        result
    }
    pub fn readlink(&self, ino: u32) -> Result<String, i64> {
        self.state.lock().readlink(ino)
    }
    pub fn set_perm(&self, ino: u32, perm: u32) -> Result<(), i64> {
        let st = self.state.lock();
        let mut inode = st.read_inode(ino)?;
        inode.set_mode((inode.mode() as u32 & S_IFMT | perm & 0o7777) as u16);
        inode.touch(false, false);
        st.write_inode(ino, &inode)
    }
    /// (block size, total blocks, free blocks, total inodes, free inodes)
    pub fn usage(&self) -> (u64, u64, u64, u64, u64) {
        let st = self.state.lock();
        let inodes = st.inodes_per_group as u64 * st.groups.len() as u64;
        (st.block_size as u64, st.blocks_count as u64, st.free_blocks as u64, inodes, st.free_inodes as u64)
    }
}
