//! ext2fs on a RAM disk that counts device requests: large reads and writes
//! take few requests, a write flushes twice (data, then metadata), and the
//! filesystem stays consistent for e2fsck; on the ring path, reserved
//! blocks stay out of the metadata until linked and `sync` flushes data
//! before metadata; a crash at any point never exposes a deleted file's
//! blocks. Needs mke2fs and e2fsck (e2fsprogs) in PATH:
//! `nix-shell -p e2fsprogs --run "cargo test -p ext2fs"`.

use ext2fs::{Device, Ext2, NewNode, ROOT_INO};
use std::path::PathBuf;
use std::process::Command;

#[derive(Default, Clone)]
struct Counts {
    reads: usize,
    writes: usize,
    flushes: usize,
}

struct RamDisk {
    data: Vec<u8>,
    counts: Counts,
    /// Writes that fail: the n-th write from now on (0 = the next one) ...
    fail_at: Option<usize>,
    /// ... and how many in a row.
    fail_count: usize,
    /// Every write and flush from some point on, for crash images.
    log: Option<Vec<Event>>,
}

/// A device request, as the crash tests replay them.
#[derive(Clone)]
enum Event {
    Write(usize, Vec<u8>),
    Flush,
}

impl RamDisk {
    /// Makes `count` writes fail, starting with the `skip`-th from now.
    fn fail_writes(&mut self, skip: usize, count: usize) {
        self.fail_at = Some(skip);
        self.fail_count = count;
    }

    fn write_fails(&mut self) -> bool {
        match self.fail_at {
            Some(0) if self.fail_count > 0 => {
                self.fail_count -= 1;
                true
            }
            Some(0) => {
                self.fail_at = None;
                false
            }
            Some(n) => {
                self.fail_at = Some(n - 1);
                false
            }
            None => false,
        }
    }
}

impl Device for RamDisk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        let at = lba as usize * 512;
        buf.copy_from_slice(self.data.get(at..at + buf.len()).ok_or(())?);
        self.counts.reads += 1;
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        if self.write_fails() {
            return Err(());
        }
        let at = lba as usize * 512;
        self.data.get_mut(at..at + buf.len()).ok_or(())?.copy_from_slice(buf);
        self.counts.writes += 1;
        if let Some(log) = &mut self.log {
            log.push(Event::Write(at, buf.to_vec()));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        self.counts.flushes += 1;
        if let Some(log) = &mut self.log {
            log.push(Event::Flush);
        }
        Ok(())
    }

    fn now(&self) -> u32 {
        1_700_000_000
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    dir.join(format!("{}-{}.img", name, std::process::id()))
}

/// A fresh ext2 image like the builder's data disk: 1 KiB blocks.
fn mkfs(name: &str, kib: usize) -> RamDisk {
    let path = scratch(name);
    let ok = Command::new("mke2fs")
        .args(["-q", "-t", "ext2", "-b", "1024", "-I", "128", "-O", "none,filetype,sparse_super,large_file", "-F"])
        .arg(&path)
        .arg(kib.to_string())
        .env_remove("SOURCE_DATE_EPOCH")
        .status()
        .expect("mke2fs not found (run under nix-shell -p e2fsprogs)")
        .success();
    assert!(ok, "mke2fs failed");
    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0, log: None }
}

/// e2fsck -fn on the disk's contents.
fn fsck(name: &str, disk: &RamDisk) {
    let path = scratch(name);
    std::fs::write(&path, &disk.data).unwrap();
    let out = Command::new("e2fsck").arg("-fn").arg(&path).output().expect("e2fsck not found");
    std::fs::remove_file(&path).unwrap();
    assert!(out.status.success(), "e2fsck: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

fn pattern(off: usize) -> u8 {
    (off / 1024 * 7 + off) as u8
}

/// Unmounts (by dropping the filesystem) and returns the disk.
fn take(fs: Ext2<RamDisk>) -> RamDisk {
    fs.into_device()
}

const MIB: usize = 1024 * 1024;
/// The chunk size of the kernel's requests to diskfs.
const CHUNK: usize = 32 * 1024;

fn write_file(fs: &mut Ext2<RamDisk>, ino: u32, len: usize) {
    let mut buf = vec![0u8; CHUNK];
    for off in (0..len).step_by(CHUNK) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = pattern(off + i);
        }
        assert_eq!(fs.write(ino, off as u64, &buf).unwrap(), CHUNK);
    }
}

fn check_file(fs: &mut Ext2<RamDisk>, ino: u32, len: usize) {
    let mut buf = vec![0u8; CHUNK];
    for off in (0..len).step_by(CHUNK) {
        assert_eq!(fs.read(ino, off as u64, &mut buf).unwrap(), CHUNK);
        assert!(buf.iter().enumerate().all(|(i, &b)| b == pattern(off + i)), "data at {}", off);
    }
}

#[test]
fn large_reads_take_few_requests() {
    let mut fs = Ext2::mount(mkfs("reads", 16 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "big", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, ino, 4 * MIB);
    // Remount: nothing cached.
    let mut fs = Ext2::mount(take(fs)).unwrap();
    let before = fs.device().counts.reads;
    check_file(&mut fs, ino, 4 * MIB);
    let reads = fs.device().counts.reads - before;
    // 128 requests of 32 KiB, one device read each; one more where a
    // request crosses an indirect block (one per 256 KiB, between the data
    // blocks), plus the metadata (inode table, indirect blocks) once.
    assert!(reads <= 128 + 16 + 32, "{} device reads for 4 MiB", reads);
    fsck("reads", &take(fs));
}

#[test]
fn large_writes_take_few_requests_and_two_flushes() {
    let mut fs = Ext2::mount(mkfs("writes", 16 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "big", &NewNode::File, 0o644).unwrap();
    let before = fs.device().counts.clone();
    write_file(&mut fs, ino, MIB);
    let after = fs.device().counts.clone();
    let requests = 32;
    // One for the data (before the metadata that points to it), one for
    // the metadata.
    assert_eq!(after.flushes - before.flushes, 2 * requests, "two flushes per write");
    // Per request: the data, the inode, the bitmap, the group descriptor,
    // the superblock and the indirect blocks it touched.
    assert!(after.writes - before.writes <= requests * 8, "{} device writes for 1 MiB", after.writes - before.writes);
    check_file(&mut fs, ino, MIB);
    fsck("writes", &take(fs));
}

#[test]
fn reads_flush_nothing() {
    let mut fs = Ext2::mount(mkfs("ro", 4 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, ino, CHUNK);
    let before = fs.device().counts.clone();
    check_file(&mut fs, ino, CHUNK);
    fs.stat(ino).unwrap();
    fs.list(ROOT_INO).unwrap();
    let after = fs.device().counts.clone();
    assert_eq!((after.writes, after.flushes), (before.writes, before.flushes));
}

/// Blocks move between directories and files: freed directory blocks
/// become file data and back, partial writes land in fresh blocks, and
/// truncation zeroes a tail. The cached metadata must never shadow data.
#[test]
fn blocks_change_roles_consistently() {
    let mut fs = Ext2::mount(mkfs("roles", 2 * 1024)).unwrap();
    for round in 0..3 {
        let dir = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
        for i in 0..200 {
            fs.create(dir, &format!("a-rather-long-file-name-{:04}", i), &NewNode::File, 0o644).unwrap();
        }
        assert_eq!(fs.list(dir).unwrap().len(), 202);
        for i in 0..200 {
            let gone = fs.unlink(dir, &format!("a-rather-long-file-name-{:04}", i), false).unwrap();
            for ino in gone {
                fs.release(ino).unwrap();
            }
        }
        for ino in fs.unlink(ROOT_INO, "d", true).unwrap() {
            fs.release(ino).unwrap();
        }
        // The freed directory blocks are reused for file data.
        let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, f, 64 * 1024);
        fs.write(f, 64 * 1024 + 100, b"tail").unwrap();
        fs.truncate(f, 64 * 1024 + 102).unwrap();
        fs.truncate(f, 64 * 1024 + 2048).unwrap();
        let mut tail = [0xffu8; 2048];
        assert_eq!(fs.read(f, 64 * 1024, &mut tail).unwrap(), 2048);
        assert!(tail[..100].iter().all(|&b| b == 0) && &tail[100..102] == b"ta" && tail[102..].iter().all(|&b| b == 0), "round {}", round);
        check_file(&mut fs, f, 64 * 1024);
        // ... and back into a directory.
        let gone = fs.unlink(ROOT_INO, "f", false).unwrap();
        for ino in gone {
            fs.release(ino).unwrap();
        }
        let d2 = fs.create(ROOT_INO, "d2", &NewNode::Dir, 0o755).unwrap();
        for i in 0..100 {
            fs.create(d2, &format!("another-long-name-{:04}", i), &NewNode::Symlink("x".repeat(100)), 0).unwrap();
        }
        assert_eq!(fs.list(d2).unwrap().len(), 102);
        let link = fs.lookup(d2, "another-long-name-0042").unwrap();
        assert_eq!(fs.readlink(link).unwrap(), "x".repeat(100));
        for i in 0..100 {
            for ino in fs.unlink(d2, &format!("another-long-name-{:04}", i), false).unwrap() {
                fs.release(ino).unwrap();
            }
        }
        for ino in fs.unlink(ROOT_INO, "d2", true).unwrap() {
            fs.release(ino).unwrap();
        }
    }
    let disk = take(fs);
    fsck("roles", &disk);
    // Everything was written: a remount sees an empty root.
    let mut fs = Ext2::mount(disk).unwrap();
    assert_eq!(fs.list(ROOT_INO).unwrap().len(), 3); // ., .., lost+found
}

#[test]
fn a_file_larger_than_the_cache_is_consistent() {
    let mut fs = Ext2::mount(mkfs("large", 24 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "big", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, ino, 16 * MIB);
    check_file(&mut fs, ino, 16 * MIB);
    fs.truncate(ino, 3 * MIB as u64 + 5).unwrap();
    let disk = take(fs);
    fsck("large", &disk);
    let mut fs = Ext2::mount(disk).unwrap();
    check_file(&mut fs, ino, 3 * MIB);
}

/// Byte offset of inode `ino`'s block pointer `i` (1 KiB blocks, one group:
/// the inode table's block is in the group descriptor at block 2).
fn block_pointer(disk: &RamDisk, ino: u32, i: usize) -> usize {
    let gd = 2 * 1024;
    let table = u32::from_le_bytes(disk.data[gd + 8..gd + 12].try_into().unwrap()) as usize;
    table * 1024 + (ino as usize - 1) * 128 + 40 + 4 * i
}

/// A block pointer on the disk beyond the end of the filesystem (or near
/// the end of the 32-bit range) is an I/O error, never a crash.
#[test]
fn block_pointers_out_of_range_are_errors() {
    for bad in [u32::MAX, u32::MAX - 3, 1 << 30] {
        let mut fs = Ext2::mount(mkfs("badptr", 2 * 1024)).unwrap();
        let ino = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, ino, 64 * 1024);
        let mut disk = take(fs);
        let direct = block_pointer(&disk, ino, 0);
        disk.data[direct..direct + 4].copy_from_slice(&bad.to_le_bytes());
        let single = block_pointer(&disk, ino, 12);
        disk.data[single..single + 4].copy_from_slice(&bad.to_le_bytes());
        let mut fs = Ext2::mount(disk).unwrap();
        let mut buf = vec![0u8; CHUNK];
        assert_eq!(fs.read(ino, 0, &mut buf), Err(ext2fs::errno::EIO), "direct pointer {:#x}", bad);
        assert_eq!(fs.read(ino, 16 * 1024, &mut buf), Err(ext2fs::errno::EIO), "indirect pointer {:#x}", bad);
        assert!(fs.write(ino, 0, &buf).is_err());
        assert!(fs.write(ino, 20 * 1024, &buf).is_err());
        assert!(fs.truncate(ino, 0).is_err() || fs.truncate(ino, 0).is_ok());
    }
}

/// A commit whose writes fail keeps its changes and writes them with the
/// next one: once the device works again, nothing is lost.
#[test]
fn failed_commits_are_retried() {
    // Every write of the operation in turn fails, until it has fewer.
    for skip in 0.. {
        let mut fs = Ext2::mount(mkfs("retry", 2 * 1024)).unwrap();
        fs.create(ROOT_INO, "before", &NewNode::File, 0o644).unwrap();
        fs.device_mut().fail_writes(skip, 1);
        if fs.create(ROOT_INO, "during", &NewNode::Dir, 0o755).is_ok() {
            assert!(skip > 2, "an operation that writes only {} times", skip);
            break;
        }
        fs.create(ROOT_INO, "after", &NewNode::File, 0o644).unwrap();
        let disk = take(fs);
        fsck("retry", &disk);
        let mut fs = Ext2::mount(disk).unwrap();
        let names: Vec<String> = fs.list(ROOT_INO).unwrap().into_iter().map(|(n, _, _)| n).collect();
        assert!(names.contains(&"before".to_string()) && names.contains(&"after".to_string()), "{:?}", names);
    }
}

/// Blocks that held a deleted file's data are never visible through a
/// file whose write to them failed.
#[test]
fn failed_data_writes_never_expose_old_blocks() {
    for skip in 0..3 {
        let mut fs = Ext2::mount(mkfs("stale", 2 * 1024)).unwrap();
        let secret = fs.create(ROOT_INO, "secret", &NewNode::File, 0o644).unwrap();
        fs.write(secret, 0, &vec![b'S'; 256 * 1024]).unwrap();
        for ino in fs.unlink(ROOT_INO, "secret", false).unwrap() {
            fs.release(ino).unwrap();
        }
        // A sparse file: everything beyond the direct blocks is a hole.
        let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
        fs.truncate(f, 256 * 1024).unwrap();
        fs.device_mut().fail_writes(skip, 1);
        let _ = fs.write(f, 64 * 1024, &vec![b'n'; 64 * 1024]);
        let disk = take(fs);
        fsck("stale", &disk);
        let mut fs = Ext2::mount(disk).unwrap();
        let mut buf = vec![0u8; 256 * 1024];
        let n = fs.read(f, 0, &mut buf).unwrap();
        assert!(buf[..n].iter().all(|&b| b != b'S'), "old data visible (failed write {})", skip);
    }
}

/// While a failed commit waits to be retried, reads through the metadata
/// it left in memory must not show the unwritten blocks' old contents.
#[test]
fn reads_after_a_failed_commit_never_expose_old_blocks() {
    for skip in 0..4 {
        let mut fs = Ext2::mount(mkfs("pending", 2 * 1024)).unwrap();
        let secret = fs.create(ROOT_INO, "secret", &NewNode::File, 0o644).unwrap();
        fs.write(secret, 0, &vec![b'S'; 256 * 1024]).unwrap();
        for ino in fs.unlink(ROOT_INO, "secret", false).unwrap() {
            fs.release(ino).unwrap();
        }
        let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
        fs.truncate(f, 256 * 1024).unwrap();
        // The data write and everything after it fail for a while.
        fs.device_mut().fail_writes(skip, 1000);
        let _ = fs.write(f, 64 * 1024, &vec![b'n'; 64 * 1024]);
        // Not even in the buffer of a read that fails (the pending commit).
        let mut buf = vec![0u8; 256 * 1024];
        let _ = fs.read(f, 0, &mut buf);
        assert!(buf.iter().all(|&b| b != b'S'), "old data visible (writes failing from {})", skip);
    }
}

// ------------------------------------------------------------- the ring path

/// What diskfs's device does for a write on the ring path: the data of
/// `off..off + data.len()` straight into the reserved blocks (a new block
/// whole, zeros where the write has none).
fn dma_write(fs: &mut Ext2<RamDisk>, r: &ext2fs::Reservation, off: u64, data: &[u8]) {
    let bs = fs.block_size() as u64;
    for run in &r.runs {
        for i in 0..run.count as u64 {
            let fb = run.file_block + i;
            let disk = (run.block as u64 + i) * bs;
            let mut block = vec![0u8; bs as usize];
            if !run.new {
                block.copy_from_slice(&fs.device().data[disk as usize..(disk + bs) as usize]);
            }
            for (j, b) in block.iter_mut().enumerate() {
                let pos = fb * bs + j as u64;
                if pos >= off && pos < off + data.len() as u64 {
                    *b = data[(pos - off) as usize];
                }
            }
            fs.device_mut().write(disk / 512, &block).unwrap();
        }
    }
}

/// The bytes `read_map` says lie at `off..off + len`, read from the disk.
fn dma_read(fs: &mut Ext2<RamDisk>, ino: u32, off: u64, len: u64) -> (u64, Vec<u8>) {
    let (size, extents) = fs.read_map(ino, off, len).unwrap();
    let mut out = Vec::new();
    for e in extents {
        match e.disk {
            Some(d) => out.extend_from_slice(&fs.device().data[d as usize..(d + e.len) as usize]),
            None => out.extend(std::iter::repeat_n(0, e.len as usize)),
        }
    }
    (size, out)
}

fn ring_write(fs: &mut Ext2<RamDisk>, ino: u32, off: u64, data: &[u8]) -> u64 {
    let r = fs.reserve(ino, off, data.len() as u64).unwrap();
    dma_write(fs, &r, off, data);
    fs.link(&r, off + data.len() as u64).unwrap()
}

#[test]
fn ring_writes_land_in_reserved_blocks_and_read_back() {
    let mut fs = Ext2::mount(mkfs("ring", 4 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    let data: Vec<u8> = (0..300 * 1024).map(pattern).collect();
    // Whole blocks, into a file without any: all new, mostly contiguous
    // (one run per indirect block in between).
    let r = fs.reserve(ino, 0, data.len() as u64).unwrap();
    assert!(r.runs.iter().all(|run| run.new) && r.runs.len() <= 4, "{:?}", r.runs);
    assert_eq!(r.runs.iter().map(|run| run.count).sum::<u32>(), 300);
    dma_write(&mut fs, &r, 0, &data);
    assert_eq!(fs.link(&r, data.len() as u64).unwrap(), data.len() as u64);
    // Overwrites in place, unaligned, and past the end (a new block).
    let r = fs.reserve(ino, 1000, 3000).unwrap();
    assert!(r.runs.iter().all(|run| !run.new));
    dma_write(&mut fs, &r, 1000, &[0xaa; 3000]);
    fs.link(&r, 4000).unwrap();
    assert_eq!(ring_write(&mut fs, ino, 400 * 1024 + 10, b"tail"), 400 * 1024 + 14);
    let mut want = data.clone();
    want[1000..4000].fill(0xaa);
    want.resize(400 * 1024 + 10, 0);
    want.extend_from_slice(b"tail");
    // Both read paths see it: the ring's extents and the IPC path's read.
    let (size, got) = dma_read(&mut fs, ino, 0, 1 << 20);
    assert_eq!(size, want.len() as u64);
    assert!(got == want, "ring read differs");
    let mut buf = vec![0u8; want.len()];
    assert_eq!(fs.read(ino, 0, &mut buf).unwrap(), want.len());
    assert!(buf == want, "read differs");
    fs.sync().unwrap();
    fsck("ring", &take(fs));
}

#[test]
fn read_maps_clip_at_the_end_and_report_holes() {
    let mut fs = Ext2::mount(mkfs("map", 2 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    fs.write(ino, 0, &[1u8; 1024]).unwrap();
    fs.write(ino, 5 * 1024, &[2u8; 1000]).unwrap();
    let (size, extents) = fs.read_map(ino, 100, 1 << 20).unwrap();
    assert_eq!(size, 6120);
    let lens: Vec<(bool, u64)> = extents.iter().map(|e| (e.disk.is_some(), e.len)).collect();
    assert_eq!(lens, vec![(true, 924), (false, 4096), (true, 1000)]);
    assert_eq!(fs.read_map(ino, 6120, 10).unwrap().1, vec![]);
    assert_eq!(fs.read_map(ino, 1 << 40, 10).unwrap().1, vec![]);
    let (_, bytes) = dma_read(&mut fs, ino, 1020, 8);
    assert_eq!(bytes, [1, 1, 1, 1, 0, 0, 0, 0]);
    assert_eq!(fs.read_map(ROOT_INO, 0, 10), Err(ext2fs::errno::EINVAL));
}

/// A reserved block is in no bitmap and no inode: commits meanwhile leave
/// a consistent filesystem, other allocations do not take it, and giving
/// it back leaves no trace.
#[test]
fn reserved_blocks_stay_out_of_the_metadata_until_linked() {
    let mut fs = Ext2::mount(mkfs("reserved", 4 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    let free = fs.usage().2;
    let r = fs.reserve(a, 0, 64 * 1024).unwrap();
    // Another file allocates (and commits) while the reservation stands.
    write_file(&mut fs, b, 64 * 1024);
    let reserved: Vec<u32> = r.runs.iter().flat_map(|run| run.block..run.block + run.count).collect();
    let (_, extents) = fs.read_map(b, 0, 64 * 1024).unwrap();
    for e in extents {
        let first = (e.disk.unwrap() / 1024) as u32;
        assert!(reserved.iter().all(|&blk| blk < first || blk >= first + (e.len / 1024) as u32), "b got a reserved block");
    }
    // On the disk now: the bitmap without the reserved blocks.
    fsck("reserved-mid", fs.device());
    dma_write(&mut fs, &r, 0, &[7u8; 64 * 1024]);
    fs.link(&r, 64 * 1024).unwrap();
    // A reservation given back.
    let r = fs.reserve(a, 64 * 1024, 16 * 1024).unwrap();
    fs.unreserve(&r);
    fs.sync().unwrap();
    // 64 + 64 data blocks and an indirect block each (and the one the
    // given-back reservation made for a, which stays linked: it is valid).
    assert!(free - fs.usage().2 <= 128 + 3, "{} blocks used", free - fs.usage().2);
    check_file(&mut fs, b, 64 * 1024);
    let mut buf = vec![0u8; 64 * 1024];
    assert_eq!(fs.read(a, 0, &mut buf).unwrap(), 64 * 1024);
    assert!(buf.iter().all(|&x| x == 7));
    assert_eq!(fs.stat(a).unwrap().size, 64 * 1024);
    fsck("reserved", &take(fs));
}

#[test]
fn sync_flushes_the_data_before_the_metadata() {
    let mut fs = Ext2::mount(mkfs("order", 2 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    let r = fs.reserve(ino, 0, 4096).unwrap();
    dma_write(&mut fs, &r, 0, &[3u8; 4096]);
    fs.link(&r, 4096).unwrap();
    let before = fs.device().counts.clone();
    fs.sync().unwrap();
    let after = fs.device().counts.clone();
    // A flush for the data, the metadata, a flush for it.
    assert_eq!(after.flushes - before.flushes, 2);
    assert!(after.writes > before.writes);
    // Nothing left: the next sync writes nothing.
    fs.sync().unwrap();
    assert_eq!(fs.device().counts.flushes, after.flushes);
    fsck("order", &take(fs));
}

#[test]
fn the_ring_path_takes_only_inodes_in_use() {
    let mut fs = Ext2::mount(mkfs("live", 2 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    let enoent = Err(ext2fs::errno::ENOENT);
    for dead in [0, ino + 1, u32::MAX, 100_000] {
        assert_eq!(fs.check(dead), enoent, "{dead}");
        assert_eq!(fs.read_map(dead, 0, 1).map(|_| ()), enoent);
        assert_eq!(fs.reserve(dead, 0, 1).map(|_| ()), enoent);
    }
    assert_eq!(fs.check(ino), Ok(()));
    // Unlinked but not released: still in use (an open file).
    let gone = fs.unlink(ROOT_INO, "f", false).unwrap();
    assert_eq!(gone, vec![ino]);
    assert_eq!(fs.check(ino), Ok(()));
    fs.release(ino).unwrap();
    assert_eq!(fs.check(ino), enoent);
    assert_eq!(fs.reserve(ino, 0, 1).map(|_| ()), enoent);
    fsck("live", &take(fs));
}

// ------------------------------------------------------------------ crashes

/// The disk after a crash in flush epoch `epoch` of `log` (from `base`):
/// every write before that epoch's start reached the disk (flushes ended
/// the epochs before), of the epoch's own writes an arbitrary subset (the
/// device's cache may write them back in any order), chosen by `seed`.
fn crash_image(base: &[u8], log: &[Event], epoch: usize, seed: u64) -> Vec<u8> {
    let mut data = base.to_vec();
    let mut rng = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut current = 0;
    for e in log {
        match e {
            Event::Flush => {
                current += 1;
                if current > epoch {
                    break;
                }
            }
            Event::Write(at, bytes) => {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // Seed 0: none of the epoch's writes, 1: all of them.
                let keep = current < epoch || seed == 1 || (seed > 1 && rng >> 63 == 1);
                if keep {
                    data[*at..*at + bytes.len()].copy_from_slice(bytes);
                }
            }
        }
    }
    data
}

/// Mounts a crash image and reads every file and directory it can reach:
/// none may show the secret's bytes (`S`), which only freed blocks hold.
/// Errors are allowed (ext2 has no journal: a crash may leave an
/// inconsistency for e2fsck), stale data is not.
fn assert_no_secret(image: Vec<u8>, what: &str) {
    let disk = RamDisk { data: image, counts: Counts::default(), fail_at: None, fail_count: 0, log: None };
    let Ok(mut fs) = Ext2::mount(disk) else { return };
    let mut dirs = vec![ROOT_INO];
    let mut seen = std::collections::HashSet::new();
    while let Some(dir) = dirs.pop() {
        if !seen.insert(dir) || seen.len() > 1000 {
            continue;
        }
        let Ok(entries) = fs.list(dir) else { continue };
        for (name, ino, kind) in entries {
            assert!(!name.contains("SSSS"), "{what}: a directory shows freed data");
            match kind {
                2 if name != "." && name != ".." => dirs.push(ino),
                1 => {
                    let mut buf = vec![0u8; 2 * MIB];
                    if let Ok(n) = fs.read(ino, 0, &mut buf) {
                        assert!(!buf[..n].contains(&b'S'), "{what}: {name} shows freed data");
                    }
                }
                7 => {
                    if let Ok(target) = fs.readlink(ino) {
                        assert!(!target.contains('S'), "{what}: symlink {name} shows freed data");
                    }
                }
                _ => {}
            }
        }
    }
}

/// A crash at any point, with the device's cache writing back any subset
/// of what came since the last flush, never makes a file, directory or
/// symlink show what a deleted file left in its blocks: data and new
/// metadata blocks are flushed before the metadata that points to them,
/// on the IPC path and the ring path alike, also when a small cache
/// evicts dirty blocks in the middle of an operation.
#[test]
fn a_crash_never_exposes_freed_blocks() {
    let mut fs = Ext2::mount(mkfs("crash", 4 * 1024)).unwrap();
    let secret = fs.create(ROOT_INO, "secret", &NewNode::File, 0o644).unwrap();
    fs.write(secret, 0, &vec![b'S'; 2 * MIB]).unwrap();
    for ino in fs.unlink(ROOT_INO, "secret", false).unwrap() {
        fs.release(ino).unwrap();
    }
    let mut disk = take(fs);
    let base = disk.data.clone();
    disk.log = Some(Vec::new());
    // 2 blocks of cache: dirty blocks are evicted all the time, also in
    // the middle of linking a write.
    let mut fs = Ext2::mount_with_cache(disk, 2 * 1024).unwrap();
    // The IPC path: a file with indirect blocks, a directory with entries
    // in several blocks, a symlink with a block of its own.
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    for off in (0..300 * 1024).step_by(32 * 1024) {
        fs.write(a, off, &[b'n'; 32 * 1024]).unwrap();
    }
    let d = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    for i in 0..40 {
        fs.create(d, &format!("a-long-file-name-of-some-length-{i:03}"), &NewNode::File, 0o644).unwrap();
    }
    fs.create(ROOT_INO, "l", &NewNode::Symlink("t".repeat(200)), 0).unwrap();
    // The ring path: reserved blocks written, linked, other operations
    // committing in between, then synced; a hole left in the middle.
    // Direct blocks only first: no new indirect block whose barrier would
    // flush the data by the way.
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    for (i, (off, len)) in [(0u64, 10 * 1024u64), (0, 200 * 1024), (300 * 1024, 100 * 1024), (100 * 1024 + 7, 5000)].into_iter().enumerate() {
        let r = fs.reserve(b, off, len).unwrap();
        dma_write(&mut fs, &r, off, &vec![b'r'; len as usize]);
        fs.link(&r, off + len).unwrap();
        fs.create(d, &format!("x{i}"), &NewNode::File, 0o644).unwrap();
    }
    fs.sync().unwrap();
    // Into a hole under an indirect block already on the disk: linking
    // changes it (no new block's barrier flushes by the way), and the tiny
    // cache evicts it in the middle of the link.
    let r = fs.reserve(b, 210 * 1024, 20 * 1024).unwrap();
    assert!(r.runs.iter().all(|run| run.new));
    dma_write(&mut fs, &r, 210 * 1024, &[b'r'; 20 * 1024]);
    fs.link(&r, 230 * 1024).unwrap();
    fs.create(d, "last", &NewNode::File, 0o644).unwrap();
    let mut disk = take(fs);
    let log = disk.log.take().unwrap();
    let epochs = log.iter().filter(|e| matches!(e, Event::Flush)).count();
    assert!(epochs > 50, "{epochs} flushes");
    for epoch in 0..=epochs {
        for seed in 0..32 {
            assert_no_secret(crash_image(&base, &log, epoch, seed), &format!("crash in epoch {epoch} (seed {seed})"));
        }
    }
    // And without a crash, all is there and consistent.
    fsck("crash", &disk);
}

// ------------------------------------------------------- review follow-ups

/// A superblock whose group sizes do not fit a bitmap block (or whose
/// inodes are larger than a block) is refused at mount, not a panic later.
#[test]
fn impossible_group_sizes_are_refused_at_mount() {
    let good = mkfs("sizes", 2 * 1024);
    for (offset, value) in [(32usize, 8 * 1024 + 1u32), (40, 8 * 1024 + 1), (32, 0)] {
        let mut disk = RamDisk { data: good.data.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, log: None };
        disk.data[1024 + offset..1024 + offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(Ext2::mount(disk).is_err(), "superblock field {offset} = {value}");
    }
    let mut disk = RamDisk { data: good.data.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, log: None };
    disk.data[1024 + 88..1024 + 90].copy_from_slice(&2048u16.to_le_bytes());
    assert!(Ext2::mount(disk).is_err(), "inodes larger than a block");
    assert!(Ext2::mount(good).is_ok());
}

/// The blocks of `ino` (from its extents).
fn blocks_of(fs: &mut Ext2<RamDisk>, ino: u32, len: u64) -> Vec<u32> {
    let (_, extents) = fs.read_map(ino, 0, len).unwrap();
    extents.iter().filter_map(|e| e.disk.map(|d| (d / 1024) as u32..((d + e.len) / 1024) as u32)).flatten().collect()
}

/// Blocks freed by an operation whose commit failed are still pointed to
/// on the disk: no allocation (ring reservations, IPC writes) takes them
/// until a commit succeeded, so a crash never shows another file's data
/// in the old file.
#[test]
fn blocks_freed_before_a_failed_commit_are_not_reused() {
    let mut fs = Ext2::mount(mkfs("freed", 2 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, a, 64 * 1024);
    let old = blocks_of(&mut fs, a, 64 * 1024);
    assert_eq!(old.len(), 64);
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    let c = fs.create(ROOT_INO, "c", &NewNode::File, 0o644).unwrap();
    fs.device_mut().fail_writes(0, 1000);
    assert!(fs.truncate(a, 0).is_err());
    fs.device_mut().fail_at = None;
    // No commit has succeeded since: the freed blocks stay untouched.
    let r = fs.reserve(b, 0, 64 * 1024).unwrap();
    let taken: Vec<u32> = r.runs.iter().flat_map(|run| run.block..run.block + run.count).collect();
    assert!(taken.iter().all(|blk| !old.contains(blk)), "a reservation took a block freed before the failed commit");
    fs.unreserve(&r);
    // The IPC path: its allocation comes before its own commit.
    fs.write(c, 0, &[7u8; 16 * 1024]).unwrap();
    let got = blocks_of(&mut fs, c, 16 * 1024);
    assert!(got.iter().all(|blk| !old.contains(blk)), "a write took a block freed before the failed commit");
    fsck("freed", &take(fs));
}

/// An inode that was freed takes no more reads or writes from the IPC
/// path either (a client that still names it gets ENOENT, nothing is
/// allocated to a free inode).
#[test]
fn freed_inodes_take_no_operations() {
    let mut fs = Ext2::mount(mkfs("freedino", 2 * 1024)).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    fs.write(f, 0, b"data").unwrap();
    for ino in fs.unlink(ROOT_INO, "f", false).unwrap() {
        fs.release(ino).unwrap();
    }
    let enoent = Err(ext2fs::errno::ENOENT);
    assert_eq!(fs.write(f, 0, b"more").map(|_| ()), enoent);
    let mut buf = [0u8; 4];
    assert_eq!(fs.read(f, 0, &mut buf).map(|_| ()), enoent);
    assert_eq!(fs.truncate(f, 100), enoent);
    assert_eq!(fs.set_perm(f, 0o600), enoent);
    assert_eq!(fs.release(f), enoent);
    fsck("freedino", &take(fs));
}

/// An inode number given to a new file gets a new generation: a client
/// that held the old file tells them apart.
#[test]
fn a_reused_inode_number_has_a_new_generation() {
    let mut fs = Ext2::mount(mkfs("generation", 2 * 1024)).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    let old = fs.stat(f).unwrap().generation;
    for ino in fs.unlink(ROOT_INO, "f", false).unwrap() {
        fs.release(ino).unwrap();
    }
    let g = fs.create(ROOT_INO, "g", &NewNode::File, 0o644).unwrap();
    assert_eq!(g, f, "the freed number is taken again");
    assert_ne!(fs.stat(g).unwrap().generation, old);
    fsck("generation", &take(fs));
}
