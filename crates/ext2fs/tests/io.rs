//! ext2fs on a RAM disk that counts device requests: large reads and writes
//! take few requests, a write flushes twice (data, then its transaction), and the
//! filesystem stays consistent for e2fsck; on the ring path, reserved
//! blocks stay out of the metadata until linked and `sync` flushes data
//! before metadata; a crash at any point never exposes a deleted file's
//! blocks; promised blocks are kept for the writes that promised them;
//! orphans outlive a mount until `recover_orphans`; after a crash at any point, or a
//! device request failing anywhere, the journal's replay leaves a filesystem e2fsck finds
//! nothing wrong with; and a corrupt or hostile disk is refused at mount or fails the
//! operations that need what is corrupt (EUCLEAN), never panicking, hanging, allocating
//! beyond the disk, or giving metadata to a file. Needs mke2fs, e2fsck and debugfs (e2fsprogs) in PATH:
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
    /// The flush that fails: the n-th from now on (once; a failed flush is no barrier for
    /// the crash images).
    fail_flush: Option<usize>,
    /// The read that fails: the n-th from now on (once).
    fail_read: Option<usize>,
    /// Every write and flush from some point on, for crash images.
    log: Option<Vec<Event>>,
    /// The device says it takes no writes (`Device::read_only`).
    read_only: bool,
    /// Writes "succeed" without changing anything (a device that lies).
    lose_writes: bool,
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
        match self.fail_read {
            Some(0) => {
                self.fail_read = None;
                return Err(());
            }
            Some(n) => self.fail_read = Some(n - 1),
            None => {}
        }
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
        if self.lose_writes {
            return Ok(());
        }
        self.data.get_mut(at..at + buf.len()).ok_or(())?.copy_from_slice(buf);
        self.counts.writes += 1;
        if let Some(log) = &mut self.log {
            log.push(Event::Write(at, buf.to_vec()));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        match self.fail_flush {
            Some(0) => {
                self.fail_flush = None;
                return Err(());
            }
            Some(n) => self.fail_flush = Some(n - 1),
            None => {}
        }
        self.counts.flushes += 1;
        if let Some(log) = &mut self.log {
            log.push(Event::Flush);
        }
        Ok(())
    }

    fn now(&self) -> u32 {
        1_700_000_000
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    /// Every free checks that no name and no orphan list points to the inode and it has
    /// no links (`Device::checks`).
    fn checks(&self) -> bool {
        true
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
    RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false }
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

fn write_file<D: Device>(fs: &mut Ext2<D>, ino: u32, len: usize) {
    let mut buf = vec![0u8; CHUNK];
    for off in (0..len).step_by(CHUNK) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = pattern(off + i);
        }
        assert_eq!(fs.write(ino, off as u64, &buf).unwrap(), CHUNK);
    }
}

fn check_file<D: Device>(fs: &mut Ext2<D>, ino: u32, len: usize) {
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
    // the transaction (its log blocks and commit).
    assert_eq!(after.flushes - before.flushes, 2 * requests, "two flushes per write");
    // Per request: the data, and twice (the journal, then home) the inode, the bitmap, the
    // group descriptor, the superblock and the indirect blocks it touched, with a
    // descriptor and a commit block.
    assert!(after.writes - before.writes <= requests * 16, "{} device writes for 1 MiB", after.writes - before.writes);
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
/// the end of the 32-bit range) is corruption (EUCLEAN), never a crash.
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
        assert_eq!(fs.read(ino, 0, &mut buf), Err(ext2fs::errno::EUCLEAN), "direct pointer {:#x}", bad);
        assert_eq!(fs.read(ino, 16 * 1024, &mut buf), Err(ext2fs::errno::EUCLEAN), "indirect pointer {:#x}", bad);
        assert!(fs.write(ino, 0, &buf).is_err());
        assert!(fs.write(ino, 20 * 1024, &buf).is_err());
        assert!(fs.truncate(ino, 0).is_err() || fs.truncate(ino, 0).is_ok());
    }
}

/// A commit whose writes fail stops the filesystem (broken: nothing more is written); the
/// next mount replays the journal: what was committed is there, the operation that failed
/// is there whole or not at all, and e2fsck finds nothing to fix.
#[test]
fn a_failed_commit_stops_and_the_journal_keeps_what_was_committed() {
    for skip in 0.. {
        let mut fs = Ext2::mount(mkfs("retry", 2 * 1024)).unwrap();
        fs.create(ROOT_INO, "before", &NewNode::File, 0o644).unwrap();
        fs.device_mut().fail_writes(skip, 1);
        if fs.create(ROOT_INO, "during", &NewNode::Dir, 0o755).is_ok() {
            assert!(skip > 2, "an operation that writes only {} times", skip);
            break;
        }
        assert!(fs.broken(), "write {skip}: a failed commit stops the filesystem");
        assert!(fs.create(ROOT_INO, "after", &NewNode::File, 0o644).is_err());
        let mut disk = take(fs);
        disk.fail_at = None;
        let mut fs = Ext2::mount(disk).unwrap();
        let names: Vec<String> = fs.list(ROOT_INO).unwrap().into_iter().map(|(n, _, _)| n).collect();
        assert!(names.contains(&"before".to_string()) && !names.contains(&"after".to_string()), "{:?}", names);
        if names.contains(&"during".to_string()) {
            let d = fs.lookup(ROOT_INO, "during").unwrap();
            assert!(fs.list(d).is_ok());
        }
        assert_eq!(fsck_findings(&take(fs), &format!("write {skip} failing")), None);
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
    let disk = RamDisk { data: image, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
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
        let mut disk = RamDisk { data: good.data.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
        disk.data[1024 + offset..1024 + offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(Ext2::mount(disk).is_err(), "superblock field {offset} = {value}");
    }
    let mut disk = RamDisk { data: good.data.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
    disk.data[1024 + 88..1024 + 90].copy_from_slice(&2048u16.to_le_bytes());
    assert!(Ext2::mount(disk).is_err(), "inodes larger than a block");
    assert!(Ext2::mount(good).is_ok());
}

/// A truncation whose commit fails stops the filesystem: nothing more is written (no
/// reservation, no write), and the next mount has the file as it was or truncated, never
/// its blocks given to another file.
#[test]
fn a_truncation_whose_commit_fails_frees_nothing_twice() {
    let mut fs = Ext2::mount(mkfs("freed", 2 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, a, 64 * 1024);
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    let c = fs.create(ROOT_INO, "c", &NewNode::File, 0o644).unwrap();
    fs.device_mut().fail_writes(0, 1000);
    assert!(fs.truncate(a, 0).is_err());
    assert!(fs.broken());
    assert!(fs.reserve(b, 0, 64 * 1024).is_err());
    assert!(fs.write(c, 0, &[7u8; 16 * 1024]).is_err());
    let mut disk = take(fs);
    disk.fail_at = None;
    let mut fs = Ext2::mount(disk).unwrap();
    let size = fs.stat(a).unwrap().size;
    assert!(size == 0 || size == 64 * 1024, "{size}");
    if size > 0 {
        check_file(&mut fs, a, 64 * 1024);
    }
    fs.write(c, 0, &[7u8; 16 * 1024]).unwrap();
    assert_eq!(fsck_findings(&take(fs), "freed"), None);
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

/// Times set by the client stay as set (each on its own), and the
/// filesystem checks clean; a freed inode takes none.
#[test]
fn times_are_set_as_given() {
    let mut fs = Ext2::mount(mkfs("times", 2 * 1024)).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    fs.set_times(f, Some(1_000_000_000), Some(1_500_000_000), None).unwrap();
    let s = fs.stat(f).unwrap();
    assert_eq!((s.atime, s.mtime), (1_000_000_000, 1_500_000_000));
    let ctime = s.ctime;
    fs.set_times(f, None, None, Some(u32::MAX)).unwrap();
    let s = fs.stat(f).unwrap();
    assert_eq!((s.atime, s.mtime, s.ctime), (1_000_000_000, 1_500_000_000, u32::MAX));
    assert_ne!(ctime, u32::MAX);
    for ino in fs.unlink(ROOT_INO, "f", false).unwrap() {
        fs.release(ino).unwrap();
    }
    assert_eq!(fs.set_times(f, Some(1), None, None), Err(ext2fs::errno::ENOENT));
    // A new name in room the directory has, and a removed one, change the
    // directory's modification and change times (to the device's now).
    let d = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    for (i, name) in ["a", "b"].into_iter().enumerate() {
        fs.set_times(d, Some(5), Some(5), Some(5)).unwrap();
        if i == 0 {
            fs.create(d, name, &NewNode::File, 0o644).unwrap();
        } else {
            for ino in fs.unlink(d, "a", false).unwrap() {
                fs.release(ino).unwrap();
            }
        }
        let s = fs.stat(d).unwrap();
        assert_eq!((s.atime, s.mtime, s.ctime), (5, 1_700_000_000, 1_700_000_000), "{name}");
    }
    fsck("times", &take(fs));
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

/// Free blocks no promise holds (`usage` reports them).
fn free(fs: &Ext2<RamDisk>) -> u64 {
    fs.usage().2
}

/// Fills the disk with file `name` until fewer than `leave` blocks are
/// free; its inode.
fn fill_to(fs: &mut Ext2<RamDisk>, name: &str, leave: u64) -> u32 {
    let f = fs.create(ROOT_INO, name, &NewNode::File, 0o644).unwrap();
    let mut off = 0u64;
    while free(fs) >= leave + 16 {
        let n = ((free(fs) - leave) / 2).clamp(1, 256) * 1024;
        fs.write(f, off, &vec![1u8; n as usize]).unwrap();
        off += n;
    }
    f
}

/// A promise counts the data blocks a range lacks and the indirect blocks
/// they need, once whoever promises them; what it covers is written even
/// when the disk is otherwise full, and what it does not cover cannot take
/// its blocks.
#[test]
fn promised_blocks_are_kept_for_their_write() {
    let mut fs = Ext2::mount(mkfs("promise", 4 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    let before = free(&fs);
    // 12 direct blocks, then 20 behind the single indirect block.
    fs.promise(1, a, 0, 32 * 1024).unwrap();
    assert_eq!(fs.promised(), 32 + 1);
    assert_eq!(before - free(&fs), 33);
    // Again, by the same owner or another (the owners share a cache):
    // nothing more; another's end leaves it.
    fs.promise(1, a, 4096, 8192).unwrap();
    assert_eq!(fs.promised(), 33);
    fs.promise(2, a, 0, 4096).unwrap();
    assert_eq!(fs.promised(), 33);
    fs.forget_promises(2);
    assert_eq!(fs.promised(), 33);
    // Everything else fills the disk: allocations not promised fail.
    let _filler = fill_to(&mut fs, "filler", 0);
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    let mut left = 0;
    while fs.write(b, left * 1024, &[2u8; 1024]).is_ok() {
        left += 1;
    }
    assert_eq!(fs.write(b, left * 1024, &[2u8; 1024]), Err(28));
    assert_eq!(fs.promise(3, b, left * 1024, 1024), Err(28));
    assert_eq!(fs.promised(), 33);
    // The promised write goes through: half the ring way, half the other.
    ring_write(&mut fs, a, 0, &[3u8; 16 * 1024]);
    // (The indirect block came with blocks 12 to 15.)
    assert_eq!(fs.promised(), 16);
    fs.write(a, 16 * 1024, &[3u8; 16 * 1024]).unwrap();
    assert_eq!(fs.promised(), 0);
    fs.sync().unwrap();
    let mut buf = vec![0u8; 32 * 1024];
    assert_eq!(fs.read(a, 0, &mut buf).unwrap(), 32 * 1024);
    assert!(buf.iter().all(|&x| x == 3));
    fsck("promise", &take(fs));
}

/// A promise the free blocks do not cover fails whole; truncation, the
/// file's release and the owner's end give promised blocks back.
#[test]
fn promises_end_with_truncation_release_and_owner() {
    let mut fs = Ext2::mount(mkfs("promise-end", 4 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    let total = free(&fs);
    assert_eq!(fs.promise(1, a, 0, (total + 1) * 1024), Err(28));
    assert_eq!(fs.promised(), 0);
    // A file with a hole: only the hole is promised.
    fs.write(a, 0, &[1u8; 4096]).unwrap();
    fs.truncate(a, 64 * 1024).unwrap();
    fs.promise(1, a, 0, 8192).unwrap();
    assert_eq!(fs.promised(), 4);
    // Shrinking drops what lies beyond; growing keeps what lies within.
    fs.promise(1, a, 8192, 56 * 1024).unwrap();
    assert_eq!(fs.promised(), 60 + 1);
    // (Blocks 4 to 15 stay, and the indirect block 12 to 15 need.)
    fs.truncate(a, 16 * 1024).unwrap();
    assert_eq!(fs.promised(), 13);
    fs.truncate(a, 32 * 1024).unwrap();
    assert_eq!(fs.promised(), 13);
    // Gone with the file.
    for ino in fs.unlink(ROOT_INO, "a", false).unwrap() {
        fs.release(ino).unwrap();
    }
    assert_eq!(fs.promised(), 0);
    // And with its owner.
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    fs.promise(7, b, 0, 300 * 1024).unwrap();
    // 300 data blocks; the single indirect one; the double indirect one
    // with one table below it.
    assert_eq!(fs.promised(), 300 + 1 + 2);
    fs.forget_promises(7);
    assert_eq!(fs.promised(), 0);
    assert_eq!(free(&fs), total);
    fsck("promise-end", &take(fs));
}

/// An owner that goes passes its promises of a file to an heir (another
/// owner that holds the file: diskfs's clients share their cache), merged
/// with the heir's own; without an heir they end.
#[test]
fn promises_pass_to_an_heir() {
    let mut fs = Ext2::mount(mkfs("promise-pass", 4 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    fs.promise(1, a, 0, 8 * 1024).unwrap();
    fs.promise(2, a, 8 * 1024, 4 * 1024).unwrap();
    fs.promise(1, b, 0, 4 * 1024).unwrap();
    assert_eq!(fs.promised(), 16);
    // a's pass to owner 2, b's (no heir) end.
    fs.pass_promises(1, |ino| (ino == a).then_some(2));
    assert_eq!(fs.promised(), 12);
    // Owner 2 holds all of a's: promising them again costs nothing, its end ends them.
    fs.promise(2, a, 0, 12 * 1024).unwrap();
    assert_eq!(fs.promised(), 12);
    fs.forget_promises(2);
    assert_eq!(fs.promised(), 0);
    fsck("promise-pass", &take(fs));
}

/// A write in flight for promised blocks (reserved, not linked yet) holds
/// them once: the promise still counts them until the write links them, so
/// the rest of the disk stays promisable to the last block. (Write-back of
/// a big file keeps megabytes in flight; counted twice, they made write()
/// fail with ENOSPC early.)
#[test]
fn blocks_in_flight_for_a_promise_count_once() {
    let mut fs = Ext2::mount(mkfs("promise-flight", 4 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    fs.promise(1, a, 0, 64 * 1024).unwrap();
    let promised = free(&fs);
    let r = fs.reserve(a, 0, 64 * 1024).unwrap();
    assert_eq!(free(&fs), promised);
    // Everything else can be promised to another file, block by block.
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    let mut fb = 0;
    while fs.promise(2, b, fb * 1024, 1024).is_ok() {
        fb += 1;
    }
    assert!(free(&fs) <= 2, "{} blocks left unpromisable", free(&fs));
    // The write in flight lands; so do the other owner's.
    dma_write(&mut fs, &r, 0, &[4u8; 64 * 1024]);
    fs.link(&r, 64 * 1024).unwrap();
    fs.write(b, 0, &vec![5u8; fb as usize * 1024]).unwrap();
    assert_eq!(fs.promised(), 0);
    fs.sync().unwrap();
    // A promise that ends while its blocks are in flight leaves them
    // counted as reserved.
    let c = fs.create(ROOT_INO, "c", &NewNode::File, 0o644).unwrap();
    for ino in fs.unlink(ROOT_INO, "b", false).unwrap() {
        fs.release(ino).unwrap();
    }
    fs.sync().unwrap();
    let before = free(&fs);
    fs.promise(3, c, 0, 8 * 1024).unwrap();
    let r = fs.reserve(c, 0, 8 * 1024).unwrap();
    assert_eq!(free(&fs), before - 8);
    fs.forget_promises(3);
    assert_eq!(fs.promised(), 0);
    let mut d = 0;
    let e = fs.create(ROOT_INO, "e", &NewNode::File, 0o644).unwrap();
    while fs.promise(4, e, d * 1024, 1024).is_ok() {
        d += 1;
    }
    fs.forget_promises(4);
    // Only the 8 blocks in flight were kept from the promises (and the
    // indirect blocks of e's: one single, one double with its tables).
    let tables = |n: u64| (n > 12) as u64 + if n > 268 { 1 + (n - 268).div_ceil(256) } else { 0 };
    let used = d + tables(d) + 8;
    assert!(used <= before && used + 2 >= before, "{d} promised of {before}");
    fs.unreserve(&r);
    assert_eq!(free(&fs), before);
    fsck("promise-flight", &take(fs));
}

/// Many blocks in flight at once (a write-back burst of a big file): each
/// counts once while in flight, linking them costs no rescans, and a
/// truncation that ends part of the promise leaves exactly those blocks
/// counted as reserved.
#[test]
fn many_blocks_in_flight_count_once() {
    let mut fs = Ext2::mount(mkfs("promise-many", 32 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    const LEN: u64 = 8 * 1024 * 1024;
    const CHUNK: u64 = 64 * 1024;
    let total = free(&fs);
    fs.promise(1, a, 0, LEN).unwrap();
    let promised = fs.promised();
    let started = std::time::Instant::now();
    let reservations: Vec<_> = (0..LEN / CHUNK).map(|i| fs.reserve(a, i * CHUNK, CHUNK).unwrap()).collect();
    assert_eq!(free(&fs), total - promised);
    // The rest of the disk can be promised to another file.
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    let rest = free(&fs);
    assert!(fs.promise(2, b, 0, (rest - rest / 64 - 8) * 1024).is_ok());
    fs.forget_promises(2);
    // The first half lands.
    let half = reservations.len() / 2;
    for (i, r) in reservations[..half].iter().enumerate() {
        dma_write(&mut fs, r, i as u64 * CHUNK, &vec![6u8; CHUNK as usize]);
        fs.link(r, (i as u64 + 1) * CHUNK).unwrap();
    }
    // The file is cut there: the promise of the second half ends, and its
    // blocks in flight count as reserved until given back.
    fs.truncate(a, half as u64 * CHUNK).unwrap();
    assert_eq!(fs.promised(), 0);
    let in_flight = (reservations.len() - half) as u64 * (CHUNK / 1024);
    let c = fs.create(ROOT_INO, "c", &NewNode::File, 0o644).unwrap();
    let left = free(&fs) - in_flight;
    assert!(fs.promise(3, c, 0, (left + 1) * 1024).is_err());
    fs.forget_promises(3);
    for r in &reservations[half..] {
        fs.unreserve(r);
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    fs.sync().unwrap();
    fsck("promise-many", &take(fs));
}

/// A socket's name (bind(2) on /data): an inode without data, listed as a
/// socket, that renames keep a socket and that goes like a file.
#[test]
fn socket_inodes() {
    let mut fs = Ext2::mount(mkfs("socket", 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "sock", &NewNode::Socket, 0o755).unwrap();
    let st = fs.stat(ino).unwrap();
    assert_eq!((st.mode, st.size, st.links), (0o140755, 0, 1));
    assert!(fs.create(ROOT_INO, "sock", &NewNode::Socket, 0o755).is_err());
    fs.rename(ROOT_INO, "sock", ROOT_INO, "moved").unwrap();
    let listed = fs.list(ROOT_INO).unwrap();
    let entry = listed.iter().find(|e| e.0 == "moved").expect("listed");
    assert_eq!((entry.1, entry.2), (ino, 6));
    fs.sync().unwrap();
    let disk = take(fs);
    fsck("socket", &disk);
    let mut fs = Ext2::mount(disk).unwrap();
    for gone in fs.unlink(ROOT_INO, "moved", false).unwrap() {
        fs.release(gone).unwrap();
    }
    fs.sync().unwrap();
    fsck("socket-gone", &take(fs));
}

/// FIFOs and devices (mknod(2) on /data): inodes without data, listed with their types,
/// a device's number kept as Linux's ext2 keeps it (the old encoding for numbers below
/// 256, the new one beyond), which debugfs reads back and whose nodes made by debugfs read
/// here; renames keep the type, e2fsck finds nothing wrong, and they go like files.
#[test]
fn fifos_and_device_nodes() {
    use ext2fs::dev_make;
    let mut fs = Ext2::mount(mkfs("special", 1024)).unwrap();
    let nodes = [
        ("fifo", NewNode::Fifo, 0o010640, 0, 5u8),
        ("null", NewNode::CharDev(dev_make(1, 3)), 0o020640, dev_make(1, 3), 3),
        ("sda", NewNode::BlockDev(dev_make(8, 300)), 0o060640, dev_make(8, 300), 4),
        ("wide", NewNode::CharDev(dev_make(4095, (1 << 20) - 1)), 0o020640, dev_make(4095, (1 << 20) - 1), 3),
        ("loop", NewNode::BlockDev(dev_make(259, 7)), 0o060640, dev_make(259, 7), 4),
    ];
    let mut inos = Vec::new();
    for (name, kind, mode, rdev, _) in &nodes {
        let ino = fs.create(ROOT_INO, name, kind, 0o640).unwrap();
        let st = fs.stat(ino).unwrap();
        assert_eq!((st.mode, st.size, st.links, st.rdev), (*mode, 0, 1, *rdev), "{name}");
        // No data: reads, writes and truncations are refused.
        assert_eq!(fs.read(ino, 0, &mut [0u8; 16]), Err(22), "{name}");
        assert_eq!(fs.write(ino, 0, b"x").map(|_| ()), Err(22), "{name}");
        assert_eq!(fs.truncate(ino, 100), Err(22), "{name}");
        inos.push(ino);
    }
    // Numbers ext2 cannot keep.
    assert_eq!(fs.create(ROOT_INO, "big-major", &NewNode::CharDev(dev_make(4096, 0)), 0o600), Err(22));
    assert_eq!(fs.create(ROOT_INO, "big-minor", &NewNode::BlockDev(dev_make(1, 1 << 20)), 0o600), Err(22));
    assert_eq!(fs.lookup(ROOT_INO, "big-major"), Err(2));
    assert_eq!(fs.create(ROOT_INO, "null", &NewNode::Fifo, 0o600), Err(17));
    fs.rename(ROOT_INO, "fifo", ROOT_INO, "moved").unwrap();
    let listed = fs.list(ROOT_INO).unwrap();
    for ((name, _, _, _, ftype), ino) in nodes.iter().zip(&inos) {
        let name = if *name == "fifo" { "moved" } else { name };
        let entry = listed.iter().find(|e| e.0 == name).expect("listed");
        assert_eq!((entry.1, entry.2), (*ino, *ftype), "{name}");
    }
    fs.sync().unwrap();
    let disk = take(fs);
    fsck("special", &disk);
    // debugfs reads the numbers as Linux would, and makes nodes this reads back.
    let (data, out) = debugfs(&disk.data, "special-debugfs", "stat null\nstat sda\nstat wide\nstat moved\nmknod dfifo p\nmknod dchr c 10 200\nmknod dblk b 300 5\n");
    for want in ["major/minor number: 01:03", "major/minor number: 08:300", "major/minor number: 4095:1048575", "Type: FIFO"] {
        assert!(out.contains(want), "debugfs: no '{want}' in {out}");
    }
    let disk = RamDisk { data, ..disk };
    fsck("special-debugfs", &disk);
    let mut fs = Ext2::mount(disk).unwrap();
    for (name, mode, rdev) in [("dfifo", 0o010000, 0), ("dchr", 0o020000, dev_make(10, 200)), ("dblk", 0o060000, dev_make(300, 5))] {
        let ino = fs.lookup(ROOT_INO, name).unwrap();
        let st = fs.stat(ino).unwrap();
        assert_eq!((st.mode & 0o170000, st.rdev), (mode, rdev), "{name}");
    }
    // They go like files: nothing of theirs is taken for a block.
    let free_before = fs.usage().2;
    for name in ["moved", "null", "sda", "wide", "loop", "dfifo", "dchr", "dblk"] {
        for gone in fs.unlink(ROOT_INO, name, false).unwrap() {
            fs.release(gone).unwrap();
        }
    }
    assert_eq!(fs.usage().2, free_before);
    fs.sync().unwrap();
    fsck("special-gone", &take(fs));
}

#[test]
fn orphans_outlive_a_mount_kept_on_restart_and_freed_at_boot() {
    let mut fs = Ext2::mount(mkfs("orphans", 16 * 1024)).unwrap();
    let (_, _, free_before, _, inodes_before) = fs.usage();
    let mut files = Vec::new();
    for i in 0..3 {
        let f = fs.create(ROOT_INO, &format!("open-{i}"), &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, f, MIB);
        files.push(f);
    }
    // Unlinked while still in use (not released): on the orphan list, still readable.
    for i in 0..3 {
        assert_eq!(fs.unlink(ROOT_INO, &format!("open-{i}"), false).unwrap(), vec![files[i]]);
    }
    let mut on_list = fs.orphans();
    on_list.sort_unstable();
    assert_eq!(on_list, files);
    check_file(&mut fs, files[1], MIB);
    // The middle one is released as usual: off the list (wherever it is in the chain).
    fs.release(files[1]).unwrap();
    assert_eq!(fs.orphans().len(), 2);
    let generation = fs.stat(files[0]).unwrap().generation;
    // A restart (the server's users live on): the mount keeps the list, the files are
    // still there under their handles, and a restarted server frees only what no user
    // keeps.
    let mut fs = Ext2::mount(take(fs)).unwrap();
    let mut on_list = fs.orphans();
    on_list.sort_unstable();
    assert_eq!(on_list, vec![files[0], files[2]]);
    fs.check_handle(files[0], generation).unwrap();
    assert_eq!(fs.check_handle(files[0], generation + 1), Err(ext2fs::errno::ESTALE));
    check_file(&mut fs, files[0], MIB);
    check_file(&mut fs, files[2], MIB);
    write_file(&mut fs, files[0], MIB);
    assert_eq!(fs.recover_orphans(|ino| ino == files[0]).unwrap(), 1);
    assert_eq!(fs.orphans(), vec![files[0]]);
    // Freed: its handle is stale (also once the number is a new file's).
    assert_eq!(fs.check_handle(files[2], fs_generation_unknown()), Err(ext2fs::errno::ESTALE));
    // The machine goes down without the last one's release: the first mount after boot
    // frees it, and the filesystem is as before and clean.
    let mut fs = Ext2::mount(take(fs)).unwrap();
    assert_eq!(fs.recover_orphans(|_| false).unwrap(), 1);
    assert!(fs.orphans().is_empty());
    assert_eq!(fs.check_handle(files[0], generation), Err(ext2fs::errno::ESTALE));
    let (_, _, free_after, _, inodes_after) = fs.usage();
    assert_eq!((free_after, inodes_after), (free_before, inodes_before));
    // A new file of a freed number has another generation.
    let again = fs.create(ROOT_INO, "again", &NewNode::File, 0o644).unwrap();
    if again == files[0] {
        assert_ne!(fs.stat(again).unwrap().generation, generation);
        assert_eq!(fs.check_handle(again, generation), Err(ext2fs::errno::ESTALE));
    }
    fsck("orphans", &take(fs));
}

/// A generation no inode of these tests has.
fn fs_generation_unknown() -> u32 {
    0xdead_beef
}

/// Crashes while orphans come and go (unlinks of open files, releases, a boot's recovery),
/// at every flush, with any of the writes after it lost: after the journal's replay and the
/// next boot's recovery no name points to a freed inode, and e2fsck finds nothing at all.
#[test]
fn crashes_never_free_a_named_or_a_free_inode() {
    let mut fs = Ext2::mount(mkfs("orphan-crash", 4 * 1024)).unwrap();
    let mut files = Vec::new();
    for i in 0..6 {
        let f = fs.create(ROOT_INO, &format!("held-{i}"), &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, f, 64 * 1024);
        files.push(f);
    }
    fs.unlink(ROOT_INO, "held-0", false).unwrap();
    fs.unlink(ROOT_INO, "held-1", false).unwrap();
    let mut disk = take(fs);
    let base = disk.data.clone();
    disk.log = Some(Vec::new());
    // Logged: a restart that keeps one orphan and frees the other, unlinks, a release, a
    // removal from the middle of the list, and an unlink that frees at once.
    let mut fs = Ext2::mount(disk).unwrap();
    assert_eq!(fs.recover_orphans(|ino| ino == files[0]).unwrap(), 1);
    fs.unlink(ROOT_INO, "held-2", false).unwrap();
    fs.release(files[2]).unwrap();
    fs.unlink(ROOT_INO, "held-3", false).unwrap();
    fs.unlink(ROOT_INO, "held-4", false).unwrap();
    fs.release(files[3]).unwrap();
    assert!(fs.unlink_unless(ROOT_INO, "held-5", false, |_| false).unwrap().is_empty());
    assert!(fs.check(files[5]).is_err());
    let mut disk = take(fs);
    let log = disk.log.take().unwrap();
    let epochs = log.iter().filter(|e| matches!(e, Event::Flush)).count();
    let mut problems = std::collections::BTreeSet::new();
    for epoch in 0..=epochs {
        for seed in 0..32 {
            let image = crash_image(&base, &log, epoch, seed);
            let d = RamDisk { data: image, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
            let mut fs = Ext2::mount(d).unwrap();
            fs.recover_orphans(|_| false).unwrap();
            assert!(fs.orphans().is_empty());
            for (name, ino, _) in fs.list(ROOT_INO).unwrap() {
                if name != "." && name != ".." {
                    assert!(fs.check(ino).is_ok(), "epoch {epoch} seed {seed}: {name} names freed inode {ino}");
                    assert!(fs.stat(ino).unwrap().links > 0, "epoch {epoch} seed {seed}: {name} names an inode without links");
                }
            }
            if let Some(f) = fsck_findings(&take(fs), &format!("epoch {epoch} seed {seed}")) {
                problems.insert(f);
            }
        }
    }
    assert!(problems.is_empty(), "{:#?}", problems);
}

#[test]
fn released_orphans_leave_an_empty_list() {
    let mut fs = Ext2::mount(mkfs("orphans-released", 4 * 1024)).unwrap();
    for round in 0..50 {
        let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, f, 64 * 1024);
        let gone = fs.unlink(ROOT_INO, "f", false).unwrap();
        assert_eq!(gone, vec![f], "round {round}");
        fs.release(f).unwrap();
    }
    assert!(fs.orphans().is_empty());
    // A clean filesystem: e2fsck -fn has nothing to say about the list.
    fsck("orphans-released", &take(fs));
}

/// What may never be seen after a crash or a failed commit, once the journal is replayed
/// (and the orphans recovered): a name of a freed inode or of one without enough links, or
/// anything at all e2fsck would fix (a leak, a link count too high, a bitmap difference).
fn unsafe_findings(fs: Ext2<RamDisk>, what: &str) -> Vec<String> {
    let mut fs = fs;
    let mut found = names_within_links(&mut fs, what);
    found.extend(fsck_findings(&take(fs), what));
    found
}

/// e2fsck -fn's report on `disk` unless it finds nothing at all.
fn fsck_findings(disk: &RamDisk, what: &str) -> Option<String> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = scratch(&format!("strict-fsck-{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::write(&path, &disk.data).unwrap();
    let out = Command::new("e2fsck").arg("-fn").arg(&path).output().expect("e2fsck not found");
    std::fs::remove_file(&path).unwrap();
    (!out.status.success()).then(|| format!("{what}: {}", String::from_utf8_lossy(&out.stdout)))
}

/// The orphans' sequence of `failed_commits_never_free_a_named_or_a_free_inode`, errors
/// ignored (a failed device request fails some step); the inodes the caller holds (it
/// releases them when it goes on).
fn orphan_sequence(fs: &mut Ext2<RamDisk>, files: &[u32]) -> Vec<u32> {
    let mut held = vec![files[0]];
    let _ = fs.recover_orphans(|ino| ino == files[0]);
    if let Ok(g) = fs.unlink(ROOT_INO, "held-2", false) {
        held.extend(g);
    }
    if fs.release(files[2]).is_ok() {
        held.retain(|&i| i != files[2]);
    }
    if let Ok(g) = fs.unlink_unless(ROOT_INO, "held-3", false, |_| false) {
        held.extend(g.into_iter().map(|(ino, _)| ino));
    }
    if let Ok(g) = fs.rename(ROOT_INO, "held-5", ROOT_INO, "held-4") {
        held.extend(g);
    }
    if fs.release(files[0]).is_ok() {
        held.retain(|&i| i != files[0]);
    }
    held
}

/// A device write or flush that fails anywhere in the orphans' sequence (unlinks of open
/// files, releases, a restart's recovery, an immediate free, a rename over a file): the
/// failed commit stops the filesystem (`broken`), and the next mount (the journal's
/// replay, then the boot's recovery) has a filesystem e2fsck finds nothing wrong with;
/// where nothing failed, everything the caller holds is freed as usual, and a crash at any
/// point of that run (any writes after a flush lost) leaves nothing either.
#[test]
fn failed_commits_never_free_a_named_or_a_free_inode() {
    let mut fs = Ext2::mount(mkfs("orphan-fail", 4 * 1024)).unwrap();
    let mut files = Vec::new();
    for i in 0..6 {
        let f = fs.create(ROOT_INO, &format!("held-{i}"), &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, f, 16 * 1024);
        files.push(f);
    }
    fs.unlink(ROOT_INO, "held-0", false).unwrap();
    fs.unlink(ROOT_INO, "held-1", false).unwrap();
    let base = take(fs).data;
    // How many writes and flushes the sequence makes when nothing fails.
    let mut fs = Ext2::mount(RamDisk { data: base.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false }).unwrap();
    // The handles the caller holds: (number, generation).
    let handles: Vec<(u32, u32)> = files.iter().map(|&f| (f, fs.stat(f).unwrap().generation)).collect();
    let before = fs.device().counts.clone();
    orphan_sequence(&mut fs, &files);
    let (writes, flushes) = (fs.device().counts.writes - before.writes, fs.device().counts.flushes - before.flushes);
    assert!(writes > 10 && flushes > 5, "{writes} writes, {flushes} flushes");
    let mut problems = Vec::new();
    // One write failing, one flush failing, or the device failing every write from one
    // on (until the sequence is over).
    let modes = (0..writes).map(|n| (0, n)).chain((0..flushes).map(|n| (1, n))).chain((0..writes).map(|n| (2, n)));
    for (mode, n) in modes {
        let what = format!("{} {n} failing", ["write", "flush", "every write from"][mode]);
        let mut disk = RamDisk { data: base.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: Some(Vec::new()), read_only: false, lose_writes: false };
        match mode {
            0 => disk.fail_writes(n, 1),
            1 => disk.fail_flush = Some(n),
            _ => disk.fail_writes(n, usize::MAX),
        }
        let mut fs = Ext2::mount(disk).unwrap();
        let held = orphan_sequence(&mut fs, &files);
        fs.device_mut().fail_at = None;
        // After every failure: names within links, and what the caller holds still the
        // very files of its handles.
        if !fs.broken() {
            problems.extend(names_within_links(&mut fs, &format!("{what}, held")));
            for &(ino, generation) in handles.iter().filter(|(ino, _)| held.contains(ino)) {
                if let Err(e) = fs.check_handle(ino, generation) {
                    problems.push(format!("{what}: the held handle ({ino}, {generation}): {e}"));
                }
            }
        }
        if fs.broken() {
            // What the disk has: the next mount (a restart, then the boot's recovery).
            let mut disk = fs.into_device();
            disk.log = None;
            let mut fs = Ext2::mount(disk).unwrap();
            fs.recover_orphans(|_| false).unwrap();
            problems.extend(unsafe_findings(fs, &format!("{what}, remounted")));
        } else {
            // The device works again: the caller releases what it holds, and goes down
            // cleanly: e2fsck finds nothing.
            for &ino in held.iter() {
                if fs.releasable(ino) {
                    if let Err(e) = fs.release(ino) {
                        problems.push(format!("{what}: releasing {ino}: {e} (held {held:?}, broken {})", fs.broken()));
                    }
                }
            }
            fs.sync().unwrap();
            let mut disk = take(fs);
            problems.extend(fsck_findings(&disk, &format!("{what}, shut down")));
            let log = disk.log.take().unwrap();
            // At any crash of that run (or a restart: files are made during its grace,
            // before the recovery), the recovery leaves nothing unsafe.
            let epochs = log.iter().filter(|e| matches!(e, Event::Flush)).count();
            for epoch in 0..=epochs {
                for seed in 0..6 {
                    let image = crash_image(&base, &log, epoch, seed);
                    let d = RamDisk { data: image, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
                    let mut fs = Ext2::mount(d).unwrap();
                    let fresh = fs.create(ROOT_INO, "fresh", &NewNode::File, 0o644).unwrap();
                    write_file(&mut fs, fresh, 64 * 1024);
                    fs.recover_orphans(|_| false).unwrap();
                    check_file(&mut fs, fresh, 64 * 1024);
                    problems.extend(unsafe_findings(fs, &format!("{what}, crash at {epoch}/{seed}")));
                }
            }
            let mut fs = Ext2::mount(disk).unwrap();
            fs.recover_orphans(|_| false).unwrap();
            problems.extend(unsafe_findings(fs, &format!("{what}, at the end")));
        }
    }
    assert!(problems.is_empty(), "{:#?}", problems);
}

/// An unlink whose commit fails stops the filesystem; the next mount has the name or not
/// (whole), and an inode whose last link went is on the orphan list until it is freed (the
/// next boot's recovery): nothing leaks, e2fsck finds nothing.
#[test]
fn an_unlink_whose_commit_fails_leaves_nothing_behind() {
    for skip in 0..12 {
        let mut fs = Ext2::mount(mkfs("unlisted", 4 * 1024)).unwrap();
        let (_, _, free_before, _, inodes_before) = fs.usage();
        let held = fs.create(ROOT_INO, "held", &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, held, 64 * 1024);
        fs.device_mut().fail_writes(skip, 1);
        let unlinked = fs.unlink(ROOT_INO, "held", false);
        if unlinked.is_ok() {
            assert!(!fs.broken());
            // (The release's own commit may be the one that fails.)
            if fs.release(held).is_err() {
                assert!(fs.broken());
            }
        } else {
            assert!(fs.broken());
        }
        let mut disk = take(fs);
        disk.fail_at = None;
        let mut fs = Ext2::mount(disk).unwrap();
        fs.recover_orphans(|_| false).unwrap();
        if fs.lookup(ROOT_INO, "held").is_ok() {
            fs.unlink_unless(ROOT_INO, "held", false, |_| false).unwrap();
        }
        let (_, _, free_after, _, inodes_after) = fs.usage();
        assert_eq!((free_after, inodes_after), (free_before, inodes_before), "write {skip} failing");
        assert_eq!(fsck_findings(&take(fs), &format!("unlink, write {skip} failing")), None);
    }
}

/// A truncation that fails part way (a read of an indirect block fails) is undone whole:
/// the file as it was, nothing of it committed, and the filesystem goes on (the next
/// truncation goes through); e2fsck finds nothing after either.
#[test]
fn a_truncation_failing_part_way_is_undone() {
    let mut fs = Ext2::mount_with_cache(mkfs("trunc-fail", 8 * 1024), 16 * 1024).unwrap();
    let f = fs.create(ROOT_INO, "big", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, f, 2 * MIB);
    fs.sync().unwrap();
    let base = take(fs).data;
    let mut failed = 0;
    for n in 0..64 {
        let disk = RamDisk { data: base.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
        let mut fs = Ext2::mount_with_cache(disk, 16 * 1024).unwrap();
        let (_, _, free_before, _, _) = fs.usage();
        fs.device_mut().fail_read = Some(n);
        let result = fs.truncate(f, 1000);
        fs.device_mut().fail_read = None;
        assert!(!fs.broken(), "read {n}");
        if result.is_err() {
            failed += 1;
            assert_eq!(fs.stat(f).unwrap().size, 2 * MIB as u64, "read {n}");
            assert_eq!(fs.usage().2, free_before, "read {n}");
            check_file(&mut fs, f, 2 * MIB);
            let mut fs = Ext2::mount(take(fs)).unwrap();
            assert_eq!(fs.stat(f).unwrap().size, 2 * MIB as u64, "read {n}");
            check_file(&mut fs, f, 2 * MIB);
            fs.truncate(f, 1000).unwrap();
            assert_eq!(fsck_findings(&take(fs), &format!("read {n} failing")), None);
        } else {
            let mut fs = Ext2::mount(take(fs)).unwrap();
            assert_eq!(fs.stat(f).unwrap().size, 1000, "read {n}");
            assert_eq!(fsck_findings(&take(fs), &format!("read {n} failing")), None);
        }
    }
    assert!(failed > 0, "no read of the truncation failed");
}

/// `s_state` of the disk (the superblock at byte 1024, the field at 58).
fn state(disk: &RamDisk) -> u16 {
    u16::from_le_bytes([disk.data[1024 + 58], disk.data[1024 + 59]])
}

/// While in use, the disk says "not cleanly unmounted" (a crash then makes e2fsck -p check
/// it); afterwards the state it had at mount: clean again, or still not clean when it was
/// not at mount.
#[test]
fn the_state_says_in_use_until_let_go() {
    let disk = mkfs("state", 2 * 1024);
    assert_eq!(state(&disk), 1);
    let mut fs = Ext2::mount(disk).unwrap();
    // In use: nothing written until the first change, then "not clean" before it.
    fs.set_in_use(true).unwrap();
    assert_eq!(state(fs.device()), 1);
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    assert_eq!(state(fs.device()), 0);
    write_file(&mut fs, f, 64 * 1024);
    // A crash now: the disk is not clean, and e2fsck -p checks it.
    let crashed = RamDisk { data: fs.device().data.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
    let path = scratch("state-crashed");
    std::fs::write(&path, &crashed.data).unwrap();
    let out = Command::new("e2fsck").arg("-p").arg(&path).output().expect("e2fsck not found");
    std::fs::remove_file(&path).unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("was not cleanly unmounted"), "e2fsck -p: {said}");
    // An inode still to be freed (an open file unlinked): not clean while it is owed.
    fs.unlink(ROOT_INO, "f", false).unwrap();
    assert_eq!(fs.set_in_use(false), Ok(false));
    assert_eq!(state(fs.device()), 0);
    fs.release(f).unwrap();
    assert_eq!(fs.set_in_use(false), Ok(true));
    assert_eq!(state(fs.device()), 1);
    fsck("state", &take(fs));
    // Mounted after that crash: it stays not clean when let go of (e2fsck's to clear).
    let mut fs = Ext2::mount(crashed).unwrap();
    fs.set_in_use(true).unwrap();
    fs.create(ROOT_INO, "g", &NewNode::File, 0o644).unwrap();
    fs.set_in_use(false).unwrap();
    assert_eq!(state(fs.device()), 0);
}

/// A read-only device: mounted read-only, it serves reads and refuses changes (EROFS), and
/// nothing is ever written (not even the state, nor a journal). A disk whose writes fail
/// (not saying it is read-only) serves reads too: its first change stops it.
#[test]
fn a_read_only_disk_serves_reads() {
    let mut fs = Ext2::mount(mkfs("readonly", 2 * 1024)).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, f, 64 * 1024);
    let mut disk = take(fs);
    let before = disk.data.clone();
    disk.read_only = true;
    let mut fs = Ext2::mount(disk).unwrap();
    fs.set_in_use(true).unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "f"), Ok(f));
    check_file(&mut fs, f, 64 * 1024);
    assert_eq!(fs.create(ROOT_INO, "g", &NewNode::File, 0o644), Err(ext2fs::errno::EROFS));
    assert_eq!(fs.write(f, 0, b"x"), Err(ext2fs::errno::EROFS));
    check_file(&mut fs, f, 64 * 1024);
    let _ = fs.set_in_use(false);
    let mut disk = take(fs);
    assert!(disk.data == before, "a read-only disk was written");
    // Writes failing: reads go on, the first change fails and stops the filesystem.
    disk.read_only = false;
    disk.fail_writes(0, usize::MAX);
    let mut fs = Ext2::mount(disk).unwrap();
    fs.set_in_use(true).unwrap();
    assert!(fs.create(ROOT_INO, "g", &NewNode::File, 0o644).is_err());
    assert!(fs.broken());
    check_file(&mut fs, f, 64 * 1024);
    assert!(take(fs).data == before, "a failing disk was written");
}

/// A plain ext2 disk (no journal) gets one at its first read-write mount (as `tune2fs
/// -j`), which e2fsck accepts; mounted read-only it stays without.
#[test]
fn a_journal_is_added_at_the_first_mount() {
    let mut disk = mkfs("add-journal", 4 * 1024);
    let plain = disk.data.clone();
    disk.read_only = true;
    let fs = Ext2::mount(disk).unwrap();
    assert!(take(fs).data == plain, "a read-only mount added a journal");
    let mut fs = Ext2::mount(RamDisk { data: plain, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false }).unwrap();
    fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    let disk = take(fs);
    let path = scratch("add-journal-dump");
    std::fs::write(&path, &disk.data).unwrap();
    let out = Command::new("dumpe2fs").arg("-h").arg(&path).output().expect("dumpe2fs not found");
    std::fs::remove_file(&path).unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("has_journal") && said.contains("Journal inode:            8"), "{said}");
    assert!(!said.contains("needs_recovery"), "{said}");
    fsck("add-journal", &disk);
}

/// A rename over an existing name takes the file into that name's entry in place (nothing
/// to allocate), so it works on a full disk with a full directory; a rename to a new name
/// that needs room fails with ENOSPC and leaves both names as they were.
#[test]
fn a_rename_over_a_name_needs_no_room() {
    let mut fs = Ext2::mount(mkfs("rename-full", 2 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, a, 32 * 1024);
    let b = fs.create(ROOT_INO, "b", &NewNode::File, 0o644).unwrap();
    // The disk full: a file takes every free block, then names take the directory's room.
    let big = fs.create(ROOT_INO, "big", &NewNode::File, 0o644).unwrap();
    let chunk = vec![1u8; 1024];
    let mut off = 0u64;
    while fs.write(big, off, &chunk).is_ok() {
        off += 1024;
    }
    let mut n = 0;
    while fs.create(ROOT_INO, &format!("filler-with-a-long-name-{n:05}"), &NewNode::File, 0o644).is_ok() {
        n += 1;
    }
    assert_eq!(fs.create(ROOT_INO, "one-more-name-that-needs-room", &NewNode::File, 0o644), Err(ext2fs::errno::ENOSPC));
    // To a new name: ENOSPC, both names as they were.
    assert_eq!(fs.rename(ROOT_INO, "a", ROOT_INO, "a-new-name-that-needs-room-too"), Err(ext2fs::errno::ENOSPC));
    assert_eq!(fs.lookup(ROOT_INO, "a"), Ok(a));
    // Over an existing name: in place.
    assert_eq!(fs.rename(ROOT_INO, "a", ROOT_INO, "b").unwrap(), vec![b]);
    assert_eq!(fs.lookup(ROOT_INO, "b"), Ok(a));
    assert!(fs.lookup(ROOT_INO, "a").is_err());
    check_file(&mut fs, a, 32 * 1024);
    fs.release(b).unwrap();
    fsck("rename-full", &take(fs));
}

/// A rename over a name that fails part way (a read fails at each point in turn) is undone
/// whole: both names as they were.
#[test]
fn a_rename_failing_part_way_leaves_every_file_a_name() {
    let mut fs = Ext2::mount(mkfs("rename-fail", 2 * 1024)).unwrap();
    let dir = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    let mut names = Vec::new();
    for i in 0..60 {
        let f = fs.create(dir, &format!("entry-{i:03}-with-some-length"), &NewNode::File, 0o644).unwrap();
        names.push(f);
    }
    let moved = fs.create(ROOT_INO, "moved", &NewNode::File, 0o644).unwrap();
    let target = names[45];
    let base = take(fs).data;
    let mut failed = 0;
    for n in 0..40 {
        let disk = RamDisk { data: base.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
        let mut fs = Ext2::mount_with_cache(disk, 2 * 1024).unwrap();
        fs.device_mut().fail_read = Some(n);
        let result = fs.rename(ROOT_INO, "moved", dir, "entry-045-with-some-length");
        fs.device_mut().fail_read = None;
        failed += result.is_err() as u32;
        let new = fs.lookup(dir, "entry-045-with-some-length");
        let old = fs.lookup(ROOT_INO, "moved");
        // Undone whole, or done whole.
        match result {
            Ok(_) => assert!(new == Ok(moved) && old.is_err(), "read {n}: {new:?} {old:?}"),
            Err(_) => assert!(new == Ok(target) && old == Ok(moved), "read {n}: {new:?} {old:?}"),
        }
        assert!(new == Ok(moved) || new == Ok(target), "read {n}: the new name is {new:?}");
        assert!(old.is_err() || old == Ok(moved), "read {n}: the old name is {old:?}");
        assert!(old == Ok(moved) || new == Ok(moved), "read {n}: the moved file has no name");
        for ino in [moved, target] {
            if fs.lookup(dir, "entry-045-with-some-length") == Ok(ino) || fs.lookup(ROOT_INO, "moved") == Ok(ino) {
                assert!(fs.stat(ino).unwrap().links > 0, "read {n}: a named inode without links: {ino} (moved {moved}, target {target}), result {result:?}, new {:?}, old {:?}", fs.lookup(dir, "entry-045-with-some-length"), fs.lookup(ROOT_INO, "moved"));
            }
        }
    }
    assert!(failed > 0, "no read of the rename failed");
}

/// A commit that fails while in use stops the filesystem with the disk saying "not
/// clean"; the next mount, once let go of, says "clean".
#[test]
fn a_failed_commit_in_use_leaves_the_disk_not_clean() {
    let mut fs = Ext2::mount(mkfs("state-fail", 2 * 1024)).unwrap();
    fs.set_in_use(true).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    assert_eq!(state(fs.device()), 0);
    fs.device_mut().fail_writes(0, usize::MAX);
    assert!(fs.write(f, 0, &[1u8; 4096]).is_err() || fs.sync().is_err());
    assert!(fs.set_in_use(false).is_err());
    assert_eq!(state(fs.device()), 0);
    let mut disk = take(fs);
    disk.fail_at = None;
    let mut fs = Ext2::mount(disk).unwrap();
    assert_eq!(state(fs.device()), 0);
    fs.set_in_use(true).unwrap();
    fs.create(ROOT_INO, "g", &NewNode::File, 0o644).unwrap();
    assert_eq!(fs.set_in_use(false), Ok(false), "it was not clean at mount");
    assert_eq!(state(fs.device()), 0);
    fsck("state-fail", &take(fs));
}

/// The disk as a clean unmount would leave it now (committed, the journal emptied), the
/// filesystem mounted on as before.
fn take_copy(fs: &mut Ext2<RamDisk>) -> RamDisk {
    fs.sync().unwrap();
    let copy = fs.device().data.clone();
    let d = RamDisk { data: copy, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
    // (Its journal is replayed by a mount, which empties it: what e2fsck is to see.)
    take(Ext2::mount(d).unwrap())
}

/// Every name in the tree and how many name each inode (`.` and `..` aside).
fn names(fs: &mut Ext2<RamDisk>) -> std::collections::BTreeMap<u32, (u32, Vec<String>)> {
    let mut out: std::collections::BTreeMap<u32, (u32, Vec<String>)> = Default::default();
    let mut dirs = vec![(ROOT_INO, String::from(""))];
    let mut seen = std::collections::BTreeSet::new();
    while let Some((dir, path)) = dirs.pop() {
        if !seen.insert(dir) {
            continue;
        }
        for (name, ino, kind) in fs.list(dir).unwrap_or_default() {
            if name == "." || name == ".." {
                continue;
            }
            let full = format!("{path}/{name}");
            let e = out.entry(ino).or_default();
            e.0 += 1;
            e.1.push(full.clone());
            if kind == 2 {
                dirs.push((ino, full));
            }
        }
    }
    out
}

/// No inode has more names than links, and every named one is in use.
fn names_within_links(fs: &mut Ext2<RamDisk>, what: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (ino, (count, paths)) in names(fs) {
        match fs.stat(ino) {
            Ok(s) if fs.check(ino).is_ok() && s.links >= count => {}
            Ok(s) => found.push(format!("{what}: inode {ino} named {paths:?} has {} links", s.links)),
            Err(e) => found.push(format!("{what}: inode {ino} named {paths:?}: {e}")),
        }
    }
    found
}

/// Every directory with one name has its ".." at the directory that names it (a rename
/// that broke off and was finished leaves none behind).
fn dotdots_at_parents(fs: &mut Ext2<RamDisk>, what: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (ino, (count, paths)) in names(fs) {
        if count != 1 || !fs.stat(ino).is_ok_and(|s| s.mode & 0o170000 == 0o040000) {
            continue;
        }
        let parent_path = paths[0].rsplit_once('/').unwrap().0;
        let parent = parent_path.split('/').filter(|n| !n.is_empty()).try_fold(ROOT_INO, |d, n| fs.lookup(d, n));
        let dotdot = fs.lookup(ino, "..");
        if parent.is_err() || dotdot != parent {
            found.push(format!("{what}: {} has its \"..\" at {dotdot:?}, its parent is {parent:?}", paths[0]));
        }
    }
    found
}

/// Renames of every kind (a file to a new name, over a file, a directory to another
/// directory, over an empty directory), each one transaction: a crash at any point, or a
/// write, flush or read failing anywhere (one, or every one from some point on), leaves
/// (after the journal's replay) every rename done or not, and a filesystem e2fsck finds
/// nothing wrong with; removing every name afterwards (which frees what loses its last
/// link) leaves nothing either.
#[test]
fn renames_never_leave_more_names_than_links() {
    let mut fs = Ext2::mount(mkfs("rename-crash", 4 * 1024)).unwrap();
    let a = fs.create(ROOT_INO, "a", &NewNode::Dir, 0o755).unwrap();
    let b = fs.create(ROOT_INO, "b", &NewNode::Dir, 0o755).unwrap();
    for name in ["f1", "f2", "f3"] {
        let f = fs.create(a, name, &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, f, 16 * 1024);
    }
    fs.create(a, "sub", &NewNode::Dir, 0o755).unwrap();
    fs.create(b, "empty", &NewNode::Dir, 0o755).unwrap();
    fs.create(b, "f2", &NewNode::File, 0o644).unwrap();
    let base = take(fs).data;
    // (What a rename replaced is released, as its caller does.)
    let sequence = |fs: &mut Ext2<RamDisk>| {
        for (from, to, dir) in [("f1", "f1-renamed", a), ("f2", "f2", b), ("sub", "empty", b), ("f3", "f3", b)] {
            if let Ok(gone) = fs.rename(a, from, dir, to) {
                for ino in gone {
                    let _ = fs.release(ino);
                }
            }
        }
    };
    // Every name removed (files, then directories deepest first), each freed.
    let remove_all = |fs: &mut Ext2<RamDisk>| {
        let all = names(fs);
        let mut paths: Vec<(String, u32)> = all.iter().flat_map(|(&ino, (_, p))| p.iter().map(move |p| (p.clone(), ino))).collect();
        paths.sort_by_key(|(p, _)| std::cmp::Reverse(p.matches('/').count()));
        for (path, _) in paths {
            let (parent, name) = path.rsplit_once('/').unwrap();
            let dir = if parent.is_empty() { Ok(ROOT_INO) } else { parent[1..].split('/').try_fold(ROOT_INO, |d, n| fs.lookup(d, n)) };
            let Ok(dir) = dir else { continue };
            let is_dir = fs.lookup(dir, name).ok().and_then(|i| fs.stat(i).ok()).is_some_and(|s| s.mode & 0o170000 == 0o040000);
            if let Ok(gone) = fs.unlink(dir, name, is_dir) {
                for ino in gone {
                    let _ = fs.release(ino);
                }
            }
        }
    };
    let mut problems = Vec::new();
    // How many writes and flushes the renames make.
    let mut fs = Ext2::mount(RamDisk { data: base.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: Some(Vec::new()), read_only: false, lose_writes: false }).unwrap();
    sequence(&mut fs);
    problems.extend(names_within_links(&mut fs, "no failure"));
    let (writes, flushes) = (fs.device().counts.writes, fs.device().counts.flushes);
    let mut disk = take(fs);
    let log = disk.log.take().unwrap();
    // Crashes at every flush, any of the writes after it lost.
    let epochs = log.iter().filter(|e| matches!(e, Event::Flush)).count();
    for epoch in 0..=epochs {
        for seed in 0..8 {
            let d = RamDisk { data: crash_image(&base, &log, epoch, seed), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
            let mut fs = Ext2::mount(d).unwrap();
            fs.recover_orphans(|_| false).unwrap();
            let what = format!("crash at {epoch}/{seed}");
            problems.extend(names_within_links(&mut fs, &what));
            problems.extend(fsck_findings(&take_copy(&mut fs), &what));
            // The renames again (those done find nothing to do).
            sequence(&mut fs);
            problems.extend(names_within_links(&mut fs, &format!("{what}, again")));
            problems.extend(dotdots_at_parents(&mut fs, &format!("{what}, again")));
            remove_all(&mut fs);
            problems.extend(unsafe_findings(fs, &format!("{what}, all removed")));
        }
    }
    // A write or flush failing, once or from some point on; or a read failing in the
    // middle of a step (a small cache: every step reads), so that a step breaks off with
    // part of it in the cache (a name added, its directory's inode not written).
    let reads = {
        let d = RamDisk { data: base.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
        let mut fs = Ext2::mount_with_cache(d, 1024).unwrap();
        let before = fs.device().counts.reads;
        sequence(&mut fs);
        fs.device().counts.reads - before
    };
    let modes = (0..writes).map(|n| (0, n)).chain((0..flushes).map(|n| (1, n))).chain((0..writes).map(|n| (2, n))).chain((0..reads).map(|n| (3, n)));
    for (mode, n) in modes {
        let what = format!("{} {n} failing", ["write", "flush", "every write from", "read"][mode]);
        let mut d = RamDisk { data: base.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
        match mode {
            0 => d.fail_writes(n, 1),
            1 => d.fail_flush = Some(n),
            2 => d.fail_writes(n, usize::MAX),
            _ => {}
        }
        let mut fs = if mode == 3 { Ext2::mount_with_cache(d, 1024).unwrap() } else { Ext2::mount(d).unwrap() };
        if mode == 3 {
            fs.device_mut().fail_read = Some(n);
        }
        sequence(&mut fs);
        fs.device_mut().fail_at = None;
        fs.device_mut().fail_read = None;
        if fs.broken() {
            // The next mount (the journal's replay) goes on from the disk.
            let mut d = fs.into_device();
            d.fail_at = None;
            let mut fs = Ext2::mount(d).unwrap();
            fs.recover_orphans(|_| false).unwrap();
            problems.extend(names_within_links(&mut fs, &format!("{what}, remounted")));
            problems.extend(fsck_findings(&take_copy(&mut fs), &format!("{what}, remounted")));
            sequence(&mut fs);
            remove_all(&mut fs);
            problems.extend(unsafe_findings(fs, &format!("{what}, remounted, all removed")));
            continue;
        }
        problems.extend(names_within_links(&mut fs, &what));
        sequence(&mut fs);
        problems.extend(names_within_links(&mut fs, &format!("{what}, again")));
        problems.extend(dotdots_at_parents(&mut fs, &format!("{what}, again")));
        remove_all(&mut fs);
        // (A release that failed left its inode on the orphan list: the boot's recovery
        // frees it.)
        fs.recover_orphans(|_| false).unwrap();
        problems.extend(unsafe_findings(fs, &format!("{what}, all removed")));
    }
    assert!(problems.is_empty(), "{:#?}", problems);
}

/// Two names of one file made outside (debugfs's `ln`, a hard link): a rename of one onto
/// the other does nothing (POSIX), both names stay; a rename of one elsewhere moves only
/// that name, and the file keeps its other one and its link count.
#[test]
fn a_rename_between_hard_links_does_nothing() {
    let mut fs = Ext2::mount(mkfs("hardlinks", 2 * 1024)).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, f, 4096);
    let disk = take(fs);
    let path = scratch("hardlinks-debugfs");
    std::fs::write(&path, &disk.data).unwrap();
    let ok = Command::new("debugfs").args(["-w", "-R", "ln f g"]).arg(&path).status().unwrap().success()
        && Command::new("debugfs").args(["-w", "-R", "sif f links_count 2"]).arg(&path).status().unwrap().success();
    assert!(ok, "debugfs");
    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let mut fs = Ext2::mount(RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false }).unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "g"), Ok(f));
    assert_eq!(fs.rename(ROOT_INO, "f", ROOT_INO, "g").unwrap(), Vec::<u32>::new());
    assert_eq!((fs.lookup(ROOT_INO, "f"), fs.lookup(ROOT_INO, "g")), (Ok(f), Ok(f)));
    assert_eq!(fs.stat(f).unwrap().links, 2);
    fs.rename(ROOT_INO, "g", ROOT_INO, "h").unwrap();
    assert_eq!((fs.lookup(ROOT_INO, "f"), fs.lookup(ROOT_INO, "h")), (Ok(f), Ok(f)));
    assert!(fs.lookup(ROOT_INO, "g").is_err());
    assert_eq!(fs.stat(f).unwrap().links, 2);
    fsck("hardlinks", &take(fs));
}

/// The flushes each kind of operation costs (printed: `--nocapture`): one transaction,
/// one flush, whatever it does.
#[test]
fn what_operations_flush() {
    let mut fs = Ext2::mount(mkfs("flushes", 2 * 1024)).unwrap();
    let d = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    let count = |fs: &mut Ext2<RamDisk>, what: &str, op: &mut dyn FnMut(&mut Ext2<RamDisk>)| {
        let before = fs.device().counts.flushes;
        op(fs);
        let n = fs.device().counts.flushes - before;
        eprintln!("    {what}: {n} flushes");
        n
    };
    assert_eq!(count(&mut fs, "create", &mut |fs| {
        fs.create(d, "f", &NewNode::File, 0o644).unwrap();
    }), 1);
    assert_eq!(count(&mut fs, "mkdir", &mut |fs| {
        fs.create(d, "sub", &NewNode::Dir, 0o755).unwrap();
    }), 1);
    assert_eq!(count(&mut fs, "rename to a new name", &mut |fs| {
        fs.rename(d, "f", d, "g").unwrap();
    }), 1);
    fs.create(d, "h", &NewNode::File, 0o644).unwrap();
    assert_eq!(count(&mut fs, "rename over a name (the replaced file freed at once)", &mut |fs| {
        fs.rename_unless(d, "g", d, "h", |_| false).unwrap();
    }), 1);
    fs.create(d, "i", &NewNode::File, 0o644).unwrap();
    assert_eq!(count(&mut fs, "rename over a name, then the replaced file's release", &mut |fs| {
        for ino in fs.rename(d, "h", d, "i").unwrap() {
            fs.release(ino).unwrap();
        }
    }), 2);
    assert_eq!(count(&mut fs, "rename of a directory to another one", &mut |fs| {
        fs.rename(d, "sub", ROOT_INO, "sub").unwrap();
    }), 1);
    assert_eq!(count(&mut fs, "unlink (freed at once)", &mut |fs| {
        fs.unlink_unless(d, "i", false, |_| false).unwrap();
    }), 1);
    assert_eq!(count(&mut fs, "rmdir (freed at once)", &mut |fs| {
        fs.unlink_unless(ROOT_INO, "sub", true, |_| false).unwrap();
    }), 1);
    // Batched (diskfs's group commit): one flush for all.
    fs.batch(true);
    assert_eq!(count(&mut fs, "a batch of 20 creations and removals", &mut |fs| {
        for i in 0..10 {
            fs.create(d, &format!("b{i}"), &NewNode::File, 0o644).unwrap();
        }
        for i in 0..10 {
            fs.unlink_unless(d, &format!("b{i}"), false, |_| false).unwrap();
        }
        fs.sync().unwrap();
    }), 1);
    fs.batch(false);
    fsck("flushes", &take(fs));
}

/// A fresh disk made with a journal by mke2fs (as the builder's data disk).
fn mkfs_journal(name: &str, kib: usize) -> RamDisk {
    let path = scratch(name);
    let ok = Command::new("mke2fs")
        .args(["-q", "-t", "ext2", "-b", "1024", "-I", "128", "-O", "none,has_journal,filetype,sparse_super,large_file", "-F"])
        .arg(&path)
        .arg(kib.to_string())
        .env_remove("SOURCE_DATE_EPOCH")
        .status()
        .expect("mke2fs not found (run under nix-shell -p e2fsprogs)")
        .success();
    assert!(ok, "mke2fs failed");
    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false }
}

/// Runs debugfs (`-w`) with `commands` on a copy of `data`; the copy afterwards, and what
/// debugfs said.
fn debugfs(data: &[u8], name: &str, commands: &str) -> (Vec<u8>, String) {
    let path = scratch(name);
    std::fs::write(&path, data).unwrap();
    let mut child = Command::new("debugfs")
        .args(["-w", "-f", "-"])
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("debugfs not found");
    use std::io::Write;
    child.stdin.take().unwrap().write_all(commands.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    (data, String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
}

/// The disk as it is (journal and all), not unmounted: what a crash now leaves.
fn crashed(fs: &Ext2<RamDisk>) -> RamDisk {
    RamDisk { data: fs.device().data.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false }
}

/// Our journal is ext3's: debugfs's `logdump` reads its transactions, e2fsck replays a
/// crashed disk's journal (`-fy`) and then finds nothing, and the journal e2fsprogs writes
/// (debugfs's `journal_write`, with a revoke) is replayed by our mount as ext3 would.
#[test]
fn the_journal_is_ext3s() {
    let mut fs = Ext2::mount(mkfs_journal("ext3", 8 * 1024)).unwrap();
    let d = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    let f = fs.create(d, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, f, 256 * 1024);
    fs.rename(d, "f", ROOT_INO, "g").unwrap();
    fs.unlink_unless(ROOT_INO, "d", true, |_| false).unwrap();
    let disk = crashed(&fs);
    let (_, said) = debugfs(&disk.data, "ext3-logdump", "logdump\n");
    assert!(said.contains("descriptor block") && said.contains("commit block") && !said.contains("Invalid"), "{said}");
    // e2fsck replays it and then finds the filesystem whole.
    let path = scratch("ext3-e2fsck");
    std::fs::write(&path, &disk.data).unwrap();
    let out = Command::new("e2fsck").arg("-fy").arg(&path).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    let replayed = RamDisk { data: std::fs::read(&path).unwrap(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
    std::fs::remove_file(&path).unwrap();
    assert!(said.contains("recovering journal"), "{said}");
    assert_eq!(fsck_findings(&replayed, "replayed by e2fsck"), None);
    let mut fs2 = Ext2::mount(replayed).unwrap();
    check_file(&mut fs2, f, 256 * 1024);
    assert_eq!(fs2.lookup(ROOT_INO, "g"), Ok(f));
    drop(fs);

    // A transaction e2fsprogs wrote: block 3000 replayed home; one whose copy a newer
    // transaction revokes: not.
    let base = mkfs_journal("ext3-foreign", 8 * 1024).data;
    let block: Vec<u8> = (0..1024).map(|i| (i * 7 + 3) as u8).collect();
    let other: Vec<u8> = (0..1024).map(|i| (i * 5 + 1) as u8).collect();
    let (a, b) = (scratch("ext3-block-a"), scratch("ext3-block-b"));
    std::fs::write(&a, &block).unwrap();
    std::fs::write(&b, &other).unwrap();
    let commands = format!("jo\njw -b 3000 {}\njc\njo\njw -b 3001 {}\njc\njo\njw -r 3001\njc\n", a.display(), b.display());
    let (data, said) = debugfs(&base, "ext3-foreign-img", &commands);
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    let (_, dump) = debugfs(&data, "ext3-foreign-dump", "logdump\n");
    assert!(dump.contains("revoke table"), "{said}{dump}");
    // Read-only, a journal that needs recovery cannot be mounted.
    let disk = RamDisk { data: data.clone(), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: true, lose_writes: false };
    assert!(Ext2::mount(disk).is_err());
    let disk = RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
    let fs = Ext2::mount(disk).unwrap();
    let disk = take(fs);
    assert!(disk.data[3000 * 1024..3001 * 1024] == block[..], "the transaction was not replayed");
    assert!(disk.data[3001 * 1024..3002 * 1024] != other[..], "a revoked copy was replayed");
    assert_eq!(fsck_findings(&disk, "replayed"), None);
}

/// The sequence a journal superblock names, as debugfs's `logdump` reads it.
fn journal_sequence(disk: &RamDisk, name: &str) -> u32 {
    let (_, dump) = debugfs(&disk.data, name, "logdump\n");
    let at = dump.find("transaction ").unwrap_or_else(|| panic!("{dump}")) + "transaction ".len();
    dump[at..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

/// After a replay the log restarts one sequence past the last complete transaction (as
/// JBD2's recovery): a torn transaction of that next sequence left in the log is never
/// taken for the first new one.
#[test]
fn a_replay_skips_one_sequence() {
    let mut fs = Ext2::mount(mkfs_journal("seq-skip", 8 * 1024)).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, f, 64 * 1024);
    let disk = crashed(&fs);
    drop(fs);
    let (_, dump) = debugfs(&disk.data, "seq-skip-dump", "logdump\n");
    let last = dump
        .lines()
        .filter(|l| l.contains("(commit block)"))
        .filter_map(|l| l.split("sequence ").nth(1))
        .map(|s| s.split(|c: char| !c.is_ascii_digit()).next().unwrap().parse::<u32>().unwrap())
        .max()
        .unwrap_or_else(|| panic!("{dump}"));
    let fs = Ext2::mount(disk).unwrap();
    let disk = take(fs);
    assert_eq!(journal_sequence(&disk, "seq-skip-after"), last + 2);
    assert_eq!(fsck_findings(&disk, "seq-skip"), None);
}

/// Reservations that need many indirect blocks commit part way at the transaction's limit,
/// one larger than the free space fails whole: the filesystem goes on, e2fsck finds nothing.
#[test]
fn long_reservations_stay_within_a_transaction() {
    for limit in [None, Some(4)] {
        let mut fs = Ext2::mount(mkfs_journal("reserve-long", 8 * 1024)).unwrap();
        if let Some(limit) = limit {
            fs.limit_transactions(limit);
        }
        let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
        // Deep in the double-indirect range: two dozen new indirect blocks.
        let base = 300 * 1024 * 1024;
        let r = fs.reserve(f, base, 6 * 1024 * 1024).unwrap();
        fs.unreserve(&r);
        // More than the disk has.
        assert_eq!(fs.reserve(f, base, 64 * 1024 * 1024).map(|_| ()), Err(ext2fs::errno::ENOSPC));
        let r = fs.reserve(f, 0, 1024 * 1024).unwrap();
        dma_write(&mut fs, &r, 0, &vec![b'r'; 1024 * 1024]);
        fs.link(&r, 1024 * 1024).unwrap();
        fs.sync().unwrap();
        let mut back = vec![0u8; 1024 * 1024];
        assert_eq!(fs.read(f, 0, &mut back).unwrap(), back.len());
        assert!(back.iter().all(|&b| b == b'r'));
        let disk = take(fs);
        assert_eq!(fsck_findings(&disk, "reserve-long"), None, "limit {limit:?}");
    }
}

/// A tiny xorshift generator for the fuzz tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The journal superblock's fields are checked before anything is read by them.
#[test]
fn journal_superblocks_are_checked() {
    use ext2fs::journal::Superblock;
    let good = Superblock::new(1024, 1024, [7; 16]).encode();
    assert!(Superblock::parse(&good).is_ok());
    let field = |at: usize, v: u32| {
        let mut b = good.clone();
        b[at..at + 4].copy_from_slice(&v.to_be_bytes());
        Superblock::parse(&b)
    };
    // Block size, length, first block, start, magic, type, features.
    for (at, v) in [(12, 4096), (16, 100), (16, 0), (20, 0), (20, 1024), (28, 1024), (28, u32::MAX), (0, 0), (4, 3), (40, 0x10), (44, 1)] {
        assert!(field(at, v).is_err(), "field {at} = {v:#x} accepted");
    }
    assert!(Superblock::parse(&good[..512]).is_err());
}

/// A journal as an attacker writes it: random blocks with valid headers (no checksums to
/// stop the scan), any counts, block numbers, flags. Recovery never panics, reads each log
/// block a bounded number of times, writes only the filesystem's blocks, and never more
/// copies than the log has blocks.
#[test]
fn hostile_logs_are_scanned_safely() {
    use ext2fs::journal::{recover, Log, Superblock};
    struct VecLog {
        blocks: Vec<Vec<u8>>,
        reads: usize,
        writes: usize,
        fs_blocks: u32,
    }
    impl Log for VecLog {
        fn read(&mut self, n: u32) -> Result<Vec<u8>, ()> {
            self.reads += 1;
            self.blocks.get(n as usize).cloned().ok_or(())
        }
        fn write_home(&mut self, home: u32, data: &[u8]) -> Result<(), ()> {
            assert!(home < self.fs_blocks, "a block beyond the filesystem written");
            assert_eq!(data.len(), 1024);
            self.writes += 1;
            Ok(())
        }
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..400 {
        let len = 1024u32;
        let mut sb = Superblock::new(1024, len, [1; 16]);
        sb.compat = if round % 2 == 0 { 0 } else { ext2fs::journal::COMPAT_CHECKSUM };
        sb.start = 1 + rng.below(len as u64 - 1) as u32;
        sb.sequence = rng.next() as u32;
        let mut blocks = vec![vec![0u8; 1024]; len as usize];
        let mut seq = sb.sequence;
        for b in blocks.iter_mut().skip(1) {
            for x in b.iter_mut() {
                *x = rng.next() as u8;
            }
            if rng.below(4) != 0 {
                b[0..4].copy_from_slice(&0xC03B_3998u32.to_be_bytes());
                b[4..8].copy_from_slice(&(1 + rng.below(5) as u32).to_be_bytes());
                b[8..12].copy_from_slice(&seq.to_be_bytes());
                if rng.below(3) == 0 {
                    seq = seq.wrapping_add(1);
                }
                // Revoke counts and tag flags as they come; sometimes the last tag soon.
                if rng.below(2) == 0 {
                    b[12..16].copy_from_slice(&(rng.below(2048) as u32).to_be_bytes());
                }
            }
        }
        let fs_blocks = 1 + rng.below(1 << 20) as u32;
        let mut log = VecLog { blocks, reads: 0, writes: 0, fs_blocks };
        let result = recover(&sb, fs_blocks, &mut log);
        // (At most every block for the scan, every block again for checksums, and every
        // copy once for the replay.)
        assert!(log.reads <= 3 * len as usize, "round {round}: {} reads", log.reads);
        assert!(log.writes <= len as usize, "round {round}: {} writes", log.writes);
        if let Ok(rec) = result {
            assert_eq!(rec.replayed as usize, log.writes);
        }
    }
}

/// A crashed disk whose journal was corrupted at random (its superblock and log blocks):
/// mounting it never panics or hangs; it mounts or is refused.
#[test]
fn corrupted_journals_never_panic() {
    let mut fs = Ext2::mount(mkfs_journal("corrupt-journal", 4 * 1024)).unwrap();
    for i in 0..8 {
        let f = fs.create(ROOT_INO, &format!("f{i}"), &NewNode::File, 0o644).unwrap();
        write_file(&mut fs, f, 32 * 1024);
        if i % 3 == 0 {
            fs.unlink_unless(ROOT_INO, &format!("f{i}"), false, |_| false).unwrap();
        }
    }
    let image = crashed(&fs).data;
    // The journal's blocks: those that start with its magic number.
    let magic = 0xC03B_3998u32.to_be_bytes();
    let marked: Vec<usize> = (0..image.len() / 1024).filter(|&b| image[b * 1024..b * 1024 + 4] == magic).collect();
    assert!(marked.len() > 3, "no journal found");
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut mounted, mut refused) = (0, 0);
    for _ in 0..300 {
        let mut data = image.clone();
        for _ in 0..1 + rng.below(4) {
            let b = marked[rng.below(marked.len() as u64) as usize];
            // A word at one of the fields' places, or anywhere.
            let at = if rng.below(2) == 0 { [4, 8, 12, 16, 20, 24, 28, 36, 40][rng.below(9) as usize] } else { rng.below(1020) as usize & !3 };
            let v = match rng.below(4) {
                0 => 0,
                1 => u32::MAX,
                2 => rng.below(4096) as u32,
                _ => rng.next() as u32,
            };
            data[b * 1024 + at..b * 1024 + at + 4].copy_from_slice(&v.to_be_bytes());
        }
        let disk = RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
        match Ext2::mount(disk) {
            Ok(fs) => {
                mounted += 1;
                drop(take(fs));
            }
            Err(_) => refused += 1,
        }
    }
    assert!(mounted > 0 && refused > 0, "{mounted} mounted, {refused} refused");
}

/// Long operations (a truncation, a removal of a big file, a long write, a ring write)
/// commit part way when their transaction reaches its limit (a small one here): a crash
/// at any point leaves, after the journal's replay and recovery (which finishes an
/// interrupted cut), each file whole in its old or new size with its data, and a
/// filesystem e2fsck finds nothing wrong with.
#[test]
fn long_operations_commit_part_way_and_crashes_finish_them() {
    let mut fs = Ext2::mount(mkfs_journal("long-ops", 8 * 1024)).unwrap();
    let big = fs.create(ROOT_INO, "big", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, big, 320 * 1024);
    let gone = fs.create(ROOT_INO, "gone", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, gone, 160 * 1024);
    let w = fs.create(ROOT_INO, "w", &NewNode::File, 0o644).unwrap();
    let r = fs.create(ROOT_INO, "r", &NewNode::File, 0o644).unwrap();
    let mut disk = take(fs);
    let base = disk.data.clone();
    disk.log = Some(Vec::new());
    let mut fs = Ext2::mount(disk).unwrap();
    fs.limit_transactions(4);
    let before = fs.device().counts.flushes;
    fs.truncate(big, 100_000).unwrap();
    fs.unlink_unless(ROOT_INO, "gone", false, |_| false).unwrap();
    let data: Vec<u8> = (0..160 * 1024).map(pattern).collect();
    assert_eq!(fs.write(w, 0, &data).unwrap(), data.len());
    ring_write(&mut fs, r, 0, &data);
    fs.sync().unwrap();
    assert!(fs.device().counts.flushes - before > 20, "the operations did not commit part way");
    let mut disk = take(fs);
    let log = disk.log.take().unwrap();
    assert_eq!(fsck_findings(&disk, "long operations"), None);
    let epochs = log.iter().filter(|e| matches!(e, Event::Flush)).count();
    let mut problems = Vec::new();
    for epoch in 0..=epochs {
        for seed in 0..2 {
            let d = RamDisk { data: crash_image(&base, &log, epoch, seed), counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false };
            let mut fs = Ext2::mount(d).unwrap();
            fs.recover_orphans(|_| false).unwrap();
            let what = format!("crash at {epoch}/{seed}");
            let size = fs.stat(big).unwrap().size;
            if size != 100_000 && size != 320 * 1024 {
                problems.push(format!("{what}: big is {size} bytes"));
            }
            let mut buf = vec![0u8; 100_000];
            fs.read(big, 0, &mut buf).unwrap();
            if buf.iter().enumerate().any(|(i, &b)| b != pattern(i)) {
                problems.push(format!("{what}: big's data changed"));
            }
            // The removed file whole or gone.
            match fs.lookup(ROOT_INO, "gone") {
                Ok(_) => check_file(&mut fs, gone, 160 * 1024),
                Err(_) => assert!(fs.check(gone).is_err(), "{what}: gone's inode left"),
            }
            problems.extend(unsafe_findings(fs, &what));
        }
    }
    assert!(problems.is_empty(), "{:#?}", problems);
}

/// Targeted hostile logs: a descriptor whose tags never end, revoke blocks claiming huge
/// counts, a ring full of transactions of one sequence (as if the log ran in a circle), a
/// wrong sequence, short (truncated) blocks, a committed tag naming a block beyond the
/// filesystem. Each scan ends within the ring's blocks, in bounded time, writing nothing
/// beyond the filesystem; and a device that loses the replay's writes makes the mount fail
/// instead of replaying for ever.
#[test]
fn targeted_hostile_journals_end() {
    use ext2fs::journal::{recover, Log, Superblock, COMPAT_CHECKSUM};
    struct VecLog {
        blocks: Vec<Vec<u8>>,
        reads: usize,
        writes: usize,
    }
    impl Log for VecLog {
        fn read(&mut self, n: u32) -> Result<Vec<u8>, ()> {
            self.reads += 1;
            self.blocks.get(n as usize).cloned().ok_or(())
        }
        fn write_home(&mut self, home: u32, _: &[u8]) -> Result<(), ()> {
            assert!(home < 4096);
            self.writes += 1;
            Ok(())
        }
    }
    let len = 1024u32;
    let header = |b: &mut Vec<u8>, kind: u32, seq: u32| {
        b[0..4].copy_from_slice(&0xC03B_3998u32.to_be_bytes());
        b[4..8].copy_from_slice(&kind.to_be_bytes());
        b[8..12].copy_from_slice(&seq.to_be_bytes());
    };
    let started = std::time::Instant::now();
    for case in 0..6 {
        for checksums in [false, true] {
            let mut sb = Superblock::new(1024, len, [3; 16]);
            sb.compat = if checksums { COMPAT_CHECKSUM } else { 0 };
            sb.start = 1;
            sb.sequence = 7;
            let mut blocks = vec![vec![0u8; 1024]; len as usize];
            match case {
                // Every block a descriptor of sequence 7 whose tags (block 5, SAME_UUID, no
                // LAST_TAG) fill it: tags run on through the whole ring.
                0 => {
                    for b in blocks.iter_mut().skip(1) {
                        header(b, 1, 7);
                        for t in (12..1024 - 8).step_by(8) {
                            b[t..t + 4].copy_from_slice(&5u32.to_be_bytes());
                            b[t + 6..t + 8].copy_from_slice(&2u16.to_be_bytes());
                        }
                    }
                }
                // Revoke blocks claiming 4 GiB of records, each followed by a commit: the
                // ring round of transactions.
                1 => {
                    for (i, b) in blocks.iter_mut().enumerate().skip(1) {
                        let seq = 7 + (i as u32 - 1) / 2;
                        if i % 2 == 1 {
                            header(b, 5, seq);
                            b[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
                        } else {
                            header(b, 2, seq);
                        }
                    }
                }
                // A ring of commit blocks of one sequence (a cycle of empty transactions,
                // if the scan did not check sequences).
                2 => {
                    for b in blocks.iter_mut().skip(1) {
                        header(b, 2, 7);
                    }
                }
                // The wrong sequence at the start: an empty log.
                3 => {
                    for b in blocks.iter_mut().skip(1) {
                        header(b, 1, 8);
                    }
                }
                // Short blocks (a device that returns less than asked).
                4 => {
                    for b in blocks.iter_mut().skip(1) {
                        header(b, 2, 7);
                        b.truncate(100);
                    }
                }
                // A descriptor naming a block beyond the filesystem, committed.
                _ => {
                    header(&mut blocks[1], 1, 7);
                    blocks[1][12..16].copy_from_slice(&u32::MAX.to_be_bytes());
                    blocks[1][18..20].copy_from_slice(&8u16.to_be_bytes());
                    header(&mut blocks[3], 2, 7);
                }
            }
            let mut log = VecLog { blocks, reads: 0, writes: 0 };
            let result = recover(&sb, 4096, &mut log);
            assert!(log.reads <= 3 * len as usize, "case {case}: {} reads", log.reads);
            assert!(log.writes <= len as usize, "case {case}: {} writes", log.writes);
            if case == 5 && !checksums {
                assert!(result.is_err(), "a block beyond the filesystem accepted");
            }
        }
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());

    // A device that loses the replay's writes: the mount fails once, it does not loop.
    let mut fs = Ext2::mount(mkfs_journal("lying", 4 * 1024)).unwrap();
    fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    let mut disk = crashed(&fs);
    disk.lose_writes = true;
    assert!(Ext2::mount(disk).is_err());
}

// ------------------------------------------------------- corrupt and hostile disks

/// Counts each thread's heap bytes and their peak, so that the corruption tests can bound
/// what the filesystem allocates for a hostile disk (an allocation sized by a count read
/// from the disk shows as a peak far beyond the disk's own size).
struct CountingHeap;

thread_local! {
    /// (bytes now, peak) of this thread's allocations.
    static HEAP: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

fn heap_count(delta: isize) {
    // (No allocation and no destructor here: safe inside the allocator.)
    let _ = HEAP.try_with(|h| {
        let (now, peak) = h.get();
        let now = (now as isize + delta).max(0) as usize;
        h.set((now, peak.max(now)));
    });
}

/// Starts a new peak; this thread's heap bytes now.
fn heap_reset() -> usize {
    HEAP.with(|h| {
        let (now, _) = h.get();
        h.set((now, now));
        now
    })
}

fn heap_peak() -> usize {
    HEAP.with(|h| h.get().1)
}

unsafe impl std::alloc::GlobalAlloc for CountingHeap {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc(layout) };
        if !p.is_null() {
            heap_count(layout.size() as isize);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !p.is_null() {
            heap_count(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, layout) };
        heap_count(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, p: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        let q = unsafe { std::alloc::System.realloc(p, layout, size) };
        if !q.is_null() {
            heap_count(size as isize - layout.size() as isize);
        }
        q
    }
}

#[global_allocator]
static HEAP_COUNTER: CountingHeap = CountingHeap;

fn ram(data: Vec<u8>) -> RamDisk {
    RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0, fail_flush: None, fail_read: None, log: None, read_only: false, lose_writes: false }
}

/// A RAM disk for corrupt filesystems: without the free invariant's checks
/// (`Device::checks`), which a corrupt disk breaks by itself and which panic by design.
struct Unchecked(RamDisk);

impl Device for Unchecked {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.0.read(lba, buf)
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        self.0.write(lba, buf)
    }

    fn flush(&mut self) -> Result<(), ()> {
        self.0.flush()
    }

    fn now(&self) -> u32 {
        self.0.now()
    }
}

/// An image made by mke2fs (1 KiB blocks, 128-byte inodes) with `features` and `extra`
/// arguments.
fn mke2fs(name: &str, kib: usize, features: &str, extra: &[&str]) -> RamDisk {
    let path = scratch(name);
    let ok = Command::new("mke2fs")
        .args(["-q", "-t", "ext2", "-b", "1024", "-I", "128", "-O", features, "-F"])
        .args(extra)
        .arg(&path)
        .arg(kib.to_string())
        .env_remove("SOURCE_DATE_EPOCH")
        .status()
        .expect("mke2fs not found (run under nix-shell -p e2fsprogs)")
        .success();
    assert!(ok, "mke2fs failed");
    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    ram(data)
}

fn le16_at(data: &[u8], o: usize) -> usize {
    u16::from_le_bytes([data[o], data[o + 1]]) as usize
}

fn le32_at(data: &[u8], o: usize) -> usize {
    u32::from_le_bytes(data[o..o + 4].try_into().unwrap()) as usize
}

fn put32_at(data: &mut [u8], o: usize, v: u32) {
    data[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// The metadata of a 1 KiB-block image, read from the image itself: the blocks of each
/// kind, and the inodes in use (number, byte offset on the disk).
#[derive(Default)]
struct Metadata {
    superblocks: Vec<usize>,
    descriptors: Vec<usize>,
    bitmaps: Vec<usize>,
    tables: Vec<usize>,
    dirs: Vec<usize>,
    indirect: Vec<usize>,
    symlinks: Vec<usize>,
    journal: Vec<usize>,
    inodes: Vec<(u32, usize)>,
}

impl Metadata {
    fn read(data: &[u8]) -> Metadata {
        let sb = 1024;
        let (blocks, fdb, bpg, ipg, isz) = (le32_at(data, sb + 4), le32_at(data, sb + 20), le32_at(data, sb + 32), le32_at(data, sb + 40), le16_at(data, sb + 88));
        let groups = (blocks - fdb).div_ceil(bpg);
        let gdt_blocks = (groups * 32).div_ceil(1024);
        let power = |g: usize, p: usize| {
            let mut n = p;
            while n < g {
                n *= p;
            }
            n == g
        };
        let mut m = Metadata::default();
        let children = |t: usize| (0..256).map(|k| le32_at(data, t * 1024 + 4 * k)).filter(|&b| b != 0).collect::<Vec<_>>();
        for g in 0..groups {
            let first = fdb + g * bpg;
            if g <= 1 || power(g, 3) || power(g, 5) || power(g, 7) {
                m.superblocks.push(first);
                m.descriptors.extend(first + 1..first + 1 + gdt_blocks);
            }
            let d = (fdb + 1) * 1024 + g * 32;
            let (bb, ib, it) = (le32_at(data, d), le32_at(data, d + 4), le32_at(data, d + 8));
            m.bitmaps.extend([bb, ib]);
            m.tables.extend(it..it + (ipg * isz).div_ceil(1024));
            for i in 0..ipg {
                let at = it * 1024 + i * isz;
                let ino = (g * ipg + i + 1) as u32;
                let used = data[ib * 1024 + i / 8] & (1 << (i % 8)) != 0;
                let mode = le16_at(data, at);
                if mode == 0 || !used || (ino < 11 && ino != 2 && ino != 8) {
                    continue;
                }
                m.inodes.push((ino, at));
                let ptr = |k: usize| le32_at(data, at + 40 + 4 * k);
                match mode & 0o170000 {
                    0o040000 => m.dirs.extend((0..12).map(ptr).filter(|&b| b != 0)),
                    0o120000 if le32_at(data, at + 28) != 0 => m.symlinks.push(ptr(0)),
                    0o100000 => {
                        if ino == 8 {
                            m.journal.push(ptr(0));
                        }
                        for (root, depth) in [(12, 1), (13, 2), (14, 3)] {
                            let mut level = if ptr(root) != 0 { vec![ptr(root)] } else { Vec::new() };
                            for h in 0..depth {
                                m.indirect.extend(&level);
                                if h + 1 < depth {
                                    level = level.iter().flat_map(|&t| children(t)).collect();
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        m
    }

    /// Every metadata block.
    fn all(&self) -> Vec<usize> {
        [&self.superblocks, &self.descriptors, &self.bitmaps, &self.tables, &self.dirs, &self.indirect, &self.symlinks, &self.journal]
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }
}

/// The image the corruption tests start from: four groups (backups in groups 1 and 3), a
/// journal, directories of several blocks and levels, small files, a file with double
/// indirect blocks, a sparse one with triple indirect blocks, fast and slow symlinks, a
/// socket, a FIFO and devices, and an orphan (an unlinked file still open). `crash`: the disk as a crash
/// leaves it (the journal to replay).
fn hostile_base(crash: bool) -> Vec<u8> {
    let disk = mke2fs("hostile", 8 * 1024, "none,has_journal,filetype,sparse_super,large_file", &["-g", "2048", "-N", "512"]);
    let mut fs = Ext2::mount(disk).unwrap();
    let d = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    let e = fs.create(d, "e", &NewNode::Dir, 0o755).unwrap();
    let small = fs.create(ROOT_INO, "small", &NewNode::File, 0o644).unwrap();
    fs.write(small, 0, &[7u8; 3000]).unwrap();
    let big = fs.create(d, "big", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, big, 320 * 1024);
    let sparse = fs.create(e, "sparse", &NewNode::File, 0o644).unwrap();
    fs.write(sparse, 80 * MIB as u64, b"far away").unwrap();
    fs.create(ROOT_INO, "fast", &NewNode::Symlink("small".into()), 0).unwrap();
    fs.create(e, "slow", &NewNode::Symlink("s".repeat(200)), 0).unwrap();
    fs.create(ROOT_INO, "sock", &NewNode::Socket, 0o644).unwrap();
    // A FIFO, and devices whose numbers in the block pointers would read as block numbers
    // (259 and 2049, in use; the new encoding's (8,300), 0x10082c, beyond the disk).
    fs.create(ROOT_INO, "fifo", &NewNode::Fifo, 0o644).unwrap();
    fs.create(ROOT_INO, "null", &NewNode::CharDev(ext2fs::dev_make(1, 3)), 0o644).unwrap();
    fs.create(e, "sda1", &NewNode::BlockDev(ext2fs::dev_make(8, 1)), 0o644).unwrap();
    fs.create(e, "wide", &NewNode::BlockDev(ext2fs::dev_make(8, 300)), 0o644).unwrap();
    for i in 0..60 {
        fs.create(d, &format!("name-{i:02}-padding"), &NewNode::File, 0o644).unwrap();
    }
    let open = fs.create(ROOT_INO, "open", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, open, 32 * 1024);
    fs.unlink(ROOT_INO, "open", false).unwrap();
    if crash {
        fs.create(e, "late", &NewNode::Dir, 0o755).unwrap();
        fs.truncate(big, 100_000).unwrap();
        return crashed(&fs).data;
    }
    fs.into_device().data
}

/// A value for a corrupted field: the edges, small numbers, block and inode numbers (of
/// metadata, of the end of the filesystem), powers of two, anything.
fn hostile_value(rng: &mut Rng, m: &Metadata, blocks: u32) -> u32 {
    match rng.below(11) {
        0 => 0,
        1 => 1,
        2 => u32::MAX,
        3 => rng.below(16) as u32,
        4 => blocks.wrapping_sub(rng.below(3) as u32),
        5 => {
            let all = m.all();
            all[rng.below(all.len() as u64) as usize] as u32
        }
        6 => rng.below(blocks as u64) as u32,
        7 => 1 << rng.below(32),
        8 => rng.below(4096) as u32,
        9 => m.inodes[rng.below(m.inodes.len() as u64) as usize].0,
        _ => rng.next() as u32,
    }
}

/// Corrupts one place of the image's metadata: an inode's field, a superblock or
/// descriptor field, a directory entry's, an indirect pointer, a bitmap or inode table
/// byte, a journal superblock field.
fn corrupt(data: &mut [u8], m: &Metadata, rng: &mut Rng) {
    let blocks = le32_at(data, 1024 + 4) as u32;
    let pick = |rng: &mut Rng, v: &Vec<usize>| v[rng.below(v.len() as u64) as usize];
    let v = hostile_value(rng, m, blocks);
    // (Mostly where a mount still succeeds: inodes, directories, indirect blocks.)
    match [0, 0, 0, 0, 1, 1, 2, 2, 3, 4, 5, 6, 7][rng.below(13) as usize] {
        0 => {
            let (_, at) = m.inodes[rng.below(m.inodes.len() as u64) as usize];
            // mode, size, dtime, links, blocks, the pointers, size_high.
            let fields = [0usize, 4, 20, 26, 28, 40, 44, 48, 84, 88, 92, 96, 108];
            let o = at + fields[rng.below(fields.len() as u64) as usize];
            if o == at || o == at + 26 {
                data[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
            } else {
                put32_at(data, o, v);
            }
        }
        5 => {
            let fields = [4usize, 8, 12, 16, 20, 24, 32, 40, 58, 76, 84, 88, 92, 96, 100, 224, 232];
            let o = 1024 + fields[rng.below(fields.len() as u64) as usize];
            if o == 1024 + 58 || o == 1024 + 88 {
                data[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
            } else {
                put32_at(data, o, v);
            }
        }
        6 => {
            let groups = m.bitmaps.len() / 2;
            let o = 2 * 1024 + rng.below(groups as u64) as usize * 32 + [0, 4, 8, 12, 14, 16][rng.below(6) as usize];
            if o % 32 >= 12 {
                data[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
            } else {
                put32_at(data, o, v);
            }
        }
        1 => {
            let b = pick(rng, &m.dirs);
            let o = b * 1024 + (rng.below(1024) as usize & !3);
            match rng.below(3) {
                0 => put32_at(data, o, v),
                1 => data[o + 4..o + 6].copy_from_slice(&(v as u16).to_le_bytes()),
                _ => data[o + 6] = v as u8,
            }
        }
        2 => {
            let kind = [&m.indirect, &m.symlinks][rng.below(2) as usize];
            let b = pick(rng, kind);
            put32_at(data, b * 1024 + (rng.below(1024) as usize & !3), v);
        }
        3 | 4 => {
            let b = pick(rng, &[&m.bitmaps[..], &m.tables[..]].concat());
            data[b * 1024 + rng.below(1024) as usize] ^= 1 << rng.below(8);
        }
        _ => {
            let b = pick(rng, &m.journal);
            let o = b * 1024 + [12usize, 16, 20, 24, 28, 36, 40][rng.below(7) as usize];
            data[o..o + 4].copy_from_slice(&v.to_be_bytes());
        }
    }
}

/// Every kind of operation on a (possibly corrupt) filesystem, errors ignored: a walk of
/// the tree (lists, lookups, stats, reads, read maps, readlinks), creations, writes,
/// truncations, renames and removals of new and found names, the ring path, orphans'
/// recovery and release, commits.
fn sweep(fs: &mut Ext2<Unchecked>, rng: &mut Rng) {
    use ext2fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
    let mut buf = vec![0u8; 64 * 1024];
    let (mut dirs, mut found) = (vec![ROOT_INO], Vec::new());
    let mut listed = 0;
    while let Some(d) = dirs.pop() {
        listed += 1;
        if listed > 32 {
            break;
        }
        let Ok(entries) = fs.list(d) else { continue };
        for (name, _, _) in entries.into_iter().take(80) {
            if name == "." || name == ".." {
                continue;
            }
            let Ok(ino) = fs.lookup(d, &name) else { continue };
            let Ok(st) = fs.stat(ino) else { continue };
            match st.mode & S_IFMT {
                S_IFDIR => dirs.push(ino),
                S_IFREG => {
                    let _ = fs.read(ino, 0, &mut buf);
                    let _ = fs.read(ino, st.size.saturating_sub(100), &mut buf[..4096]);
                    let _ = fs.read_map(ino, 0, 1 << 20);
                }
                S_IFLNK => {
                    let _ = fs.readlink(ino);
                }
                _ => {}
            }
            found.push((d, name, ino, st.mode & S_IFMT));
        }
    }
    let data: Vec<u8> = (0..70 * 1024).map(|i| i as u8).collect();
    let found_dirs: Vec<u32> = found.iter().filter(|f| f.3 == S_IFDIR).map(|f| f.2).take(2).collect();
    for dir in [ROOT_INO].into_iter().chain(found_dirs) {
        if let Ok(f) = fs.create(dir, "fz", &NewNode::File, 0o644) {
            let _ = fs.write(f, 0, &data[..3000]);
            let _ = fs.write(f, 200 * 1024, &data);
            let _ = fs.truncate(f, 1000);
            let _ = fs.rename(dir, "fz", ROOT_INO, "fz2");
        }
        let _ = fs.create(dir, "fzd", &NewNode::Dir, 0o755);
        let _ = fs.create(dir, "fzl", &NewNode::Symlink("x".repeat(100)), 0);
        let _ = fs.unlink(dir, "fzd", true);
    }
    for (d, name, ino, kind) in found.iter().take(40) {
        match rng.below(7) {
            0 => {
                let _ = fs.write(*ino, rng.below(1 << 20), &data[..1 + rng.below(8192) as usize]);
            }
            1 => {
                let _ = fs.truncate(*ino, rng.below(1 << 19));
            }
            2 => {
                let _ = fs.rename(*d, name, ROOT_INO, &format!("r{}", rng.below(4)));
            }
            3 => {
                let keep = rng.below(2) == 0;
                let _ = fs.unlink_unless(*d, name, *kind == S_IFDIR, |_| keep);
            }
            4 => {
                let _ = fs.promise(1, *ino, 0, 8192);
                if let Ok(r) = fs.reserve(*ino, 0, 4096) {
                    let _ = fs.link(&r, 4096);
                }
                fs.forget_promises(1);
            }
            5 if *kind == S_IFDIR => {
                let _ = fs.rename(ROOT_INO, "d", *ino, "moved");
            }
            _ => {
                let _ = fs.set_perm(*ino, 0o600);
            }
        }
    }
    let _ = fs.recover_orphans(|_| false);
    for o in fs.orphans() {
        let _ = fs.release(o);
    }
    let _ = fs.set_in_use(true);
    let _ = fs.sync();
    let _ = fs.set_in_use(false);
    let _ = fs.usage();
}

/// One corrupted image (by `seed`) mounted, swept, unmounted, mounted and swept again.
/// Whether it mounted; panics if what the filesystem allocated went far beyond the disk.
fn hostile_round(image: &[u8], m: &Metadata, seed: u64) -> bool {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut data = image.to_vec();
    for _ in 0..1 + rng.below(6) {
        corrupt(&mut data, m, &mut rng);
    }
    let base = heap_reset();
    let Ok(mut fs) = Ext2::mount(Unchecked(ram(data))) else { return false };
    sweep(&mut fs, &mut rng);
    let disk = fs.into_device();
    if let Ok(mut fs) = Ext2::mount(disk) {
        sweep(&mut fs, &mut rng);
        drop(fs.into_device());
    }
    let peak = heap_peak() - base;
    assert!(peak < 48 * MIB, "{} MiB allocated", peak / MIB);
    true
}

/// Disks whose metadata was corrupted at random (superblock, group descriptors, bitmaps,
/// inode tables, directories, indirect blocks, symlinks, the journal's superblock; clean
/// and needing a replay), many seeds: a mount is refused or succeeds, and then no
/// operation panics, none runs long, and the filesystem never allocates far beyond the
/// disk's size. (`EXT2FS_HOSTILE_SEEDS` asks for more seeds.)
#[test]
fn corrupt_disks_never_panic_hang_or_balloon() {
    let seeds: u64 = std::env::var("EXT2FS_HOSTILE_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    let mut failures = Vec::new();
    let (mut mounted, mut refused) = (0, 0);
    let started = std::time::Instant::now();
    for crash in [false, true] {
        let image = std::sync::Arc::new(hostile_base(crash));
        let m = std::sync::Arc::new(Metadata::read(&image));
        assert!(!m.dirs.is_empty() && !m.indirect.is_empty() && !m.symlinks.is_empty() && !m.journal.is_empty());
        for seed in 0..seeds / 2 {
            let seed = seed * 2 + crash as u64;
            let (tx, rx) = std::sync::mpsc::channel();
            let (image, m) = (image.clone(), m.clone());
            std::thread::spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hostile_round(&image, &m, seed)));
                let _ = tx.send(r.map_err(|e| e.downcast_ref::<String>().cloned().or(e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()));
            });
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(Ok(true)) => mounted += 1,
                Ok(Ok(false)) => refused += 1,
                Ok(Err(why)) => failures.push(format!("seed {seed}: panic: {why}")),
                Err(_) => {
                    failures.push(format!("seed {seed}: still running after 30 s"));
                    break;
                }
            }
        }
    }
    eprintln!("{mounted} mounted, {refused} refused, {:?}", started.elapsed());
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
    // (Both happen: the operations get their share of corrupt disks.)
    assert!(mounted > refused && refused > 0, "{mounted} mounted, {refused} refused");
}

use ext2fs::errno::{EINVAL, EMLINK, ENAMETOOLONG, ENOENT, EUCLEAN};

/// Byte offset of inode `ino` on a 1 KiB-block image with 128-byte inodes.
fn inode_offset(data: &[u8], ino: u32) -> usize {
    let ipg = le32_at(data, 1024 + 40);
    let g = (ino as usize - 1) / ipg;
    let table = le32_at(data, 2 * 1024 + g * 32 + 8);
    table * 1024 + (ino as usize - 1) % ipg * 128
}

/// Inode `ino`'s block pointer `i`.
fn ptr_of(data: &[u8], ino: u32, i: usize) -> usize {
    le32_at(data, inode_offset(data, ino) + 40 + 4 * i)
}

fn set_ptr(data: &mut [u8], ino: u32, i: usize, v: u32) {
    let at = inode_offset(data, ino) + 40 + 4 * i;
    put32_at(data, at, v);
}

/// Sets inode `ino`'s mode, or its size (low 32 bits).
fn set_mode(data: &mut [u8], ino: u32, mode: u16) {
    let at = inode_offset(data, ino);
    data[at..at + 2].copy_from_slice(&mode.to_le_bytes());
}

fn set_size(data: &mut [u8], ino: u32, size: u32) {
    let at = inode_offset(data, ino) + 4;
    put32_at(data, at, size);
}

/// Whether the disk's superblock says the filesystem has errors (`EXT2_ERROR_FS`).
fn disk_errors(data: &[u8]) -> bool {
    le16_at(data, 1024 + 58) & 2 != 0
}

/// Mounts a (corrupt) image on a device without the free invariant's checks.
fn mount_unchecked(data: Vec<u8>) -> Ext2<Unchecked> {
    Ext2::mount(Unchecked(ram(data))).unwrap_or_else(|e| panic!("refused: {e}"))
}

/// Superblocks and group descriptors that contradict themselves or the disk are refused at
/// mount, each by its own check: a revision we do not know, a wrong first data block, an
/// inode count the groups do not have, a first inode among the reserved ones or beyond
/// the inodes, descriptors that do not fit the first group, a filesystem larger than its
/// device, bitmaps or an inode table outside their group or on each other or the
/// descriptors, counts beyond a group, a root that is no directory, a journal larger than
/// half the filesystem, with a block twice or in an inode table.
#[test]
fn inconsistent_geometry_is_refused_at_mount() {
    let good = mkfs_journal("geometry", 4 * 1024).data;
    assert!(Ext2::mount(ram(good.clone())).is_ok());
    let sb = 1024;
    let (inodes, blocks, ipg) = (le32_at(&good, sb) as u32, le32_at(&good, sb + 4) as u32, le32_at(&good, sb + 40) as u32);
    let gd = 2 * 1024;
    let (bb, it) = (le32_at(&good, gd) as u32, le32_at(&good, gd + 8) as u32);
    let (journal, journal0) = (inode_offset(&good, 8), ptr_of(&good, 8, 0) as u32);
    let root = inode_offset(&good, 2);
    let w32 = |o: usize, v: u32| (o, v.to_le_bytes().to_vec());
    let w16 = |o: usize, v: u16| (o, v.to_le_bytes().to_vec());
    let outside = "corrupt group descriptor: its bitmaps or inode table are not its group's";
    let cases: Vec<(&str, Vec<(usize, Vec<u8>)>)> = vec![
        ("unsupported ext2 revision", vec![w32(sb + 76, 2)]),
        ("corrupt superblock: its first data block or block count", vec![w32(sb + 20, 0)]),
        ("corrupt superblock: the inode count does not match the groups", vec![w32(sb, inodes + ipg)]),
        ("corrupt superblock: the first inode", vec![w32(sb + 84, 3)]),
        ("corrupt superblock: the first inode", vec![w32(sb + 84, inodes + 1)]),
        // Groups of 16 blocks: 256 of them, whose descriptors and one inode table do not fit
        // 16 blocks.
        ("corrupt superblock: the group descriptors do not fit the first group", vec![w32(sb + 32, 16), w32(sb, (blocks - 1).div_ceil(16) * ipg)]),
        ("the filesystem is larger than the device", vec![w32(sb + 4, blocks * 2)]),
        (outside, vec![w32(gd, 1)]),
        (outside, vec![w32(gd, 2)]),
        (outside, vec![w32(gd + 4, bb)]),
        (outside, vec![w32(gd + 4, it + 1)]),
        (outside, vec![w32(gd + 8, blocks - 4)]),
        (outside, vec![w32(gd + 8, bb)]),
        ("corrupt group descriptor: its counts exceed its group", vec![w16(gd + 12, u16::MAX)]),
        ("corrupt group descriptor: its counts exceed its group", vec![w16(gd + 14, ipg as u16 + 1)]),
        ("corrupt group descriptor: its counts exceed its group", vec![w16(gd + 16, ipg as u16 + 1)]),
        ("corrupt root directory", vec![w16(root, 0o100644)]),
        ("corrupt root directory", vec![w32(root + 4, 1000)]),
        ("the journal inode is not a journal", vec![w32(journal + 4, blocks / 2 * 1024 + 1024)]),
        ("the journal's blocks repeat", vec![w32(journal + 44, journal0)]),
        ("cannot map the journal", vec![w32(journal + 40, it)]),
    ];
    for (want, patches) in cases {
        let mut data = good.clone();
        for (at, bytes) in &patches {
            data[*at..*at + bytes.len()].copy_from_slice(bytes);
        }
        assert_eq!(Ext2::mount(ram(data)).err(), Some(want), "{patches:?}");
    }
}

/// The superblock's free counts are the descriptors' sums (as Linux counts them): a stale
/// one is corrected, and e2fsck finds nothing after.
#[test]
fn stale_superblock_counts_are_the_descriptors() {
    let mut data = mkfs_journal("stale-counts", 4 * 1024).data;
    let (free, free_inodes) = (le32_at(&data, 1024 + 12), le32_at(&data, 1024 + 16));
    put32_at(&mut data, 1024 + 12, 5);
    put32_at(&mut data, 1024 + 16, 1_000_000);
    let mut fs = Ext2::mount(ram(data)).unwrap();
    let (_, _, f, _, fi) = fs.usage();
    assert_eq!((f, fi), (free as u64, free_inodes as u64));
    let ino = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, ino, 64 * 1024);
    assert!(!fs.errors());
    fsck("stale-counts", &take(fs));
}

/// An image with directory `d` holding "a", "b" and "c" (one block: ".", "..", then 12
/// bytes each, "c" the rest), file `f` of 32 KiB (an indirect block), a fast and a slow
/// symlink, a socket, all in the root. (disk, d, f)
fn small_tree(name: &str) -> (Vec<u8>, u32, u32) {
    let mut fs = Ext2::mount(mkfs_journal(name, 4 * 1024)).unwrap();
    let d = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    for n in ["a", "b", "c"] {
        fs.create(d, n, &NewNode::File, 0o644).unwrap();
    }
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, f, 32 * 1024);
    fs.create(ROOT_INO, "fast", &NewNode::Symlink("abc".into()), 0).unwrap();
    fs.create(ROOT_INO, "slow", &NewNode::Symlink("s".repeat(200)), 0).unwrap();
    fs.create(ROOT_INO, "sock", &NewNode::Socket, 0o644).unwrap();
    (take(fs).data, d, f)
}

/// Corrupt directories (as ext4's `ext4_check_dir_entry` and its lookup see them): a
/// record length not a multiple of 4, too short, past the block, a name past its record,
/// an inode number beyond the inodes or a reserved one (the journal's), a '/' in a name,
/// a size that is no number of blocks, a hole, a block twice, a block of the inode table.
/// Every operation on the directory fails with EUCLEAN, the rest of the filesystem works,
/// and the disk says it has errors after.
#[test]
fn corrupt_directories_fail_with_euclean() {
    let (good, d, f) = small_tree("corrupt-dirs");
    let blk = ptr_of(&good, d, 0);
    let entry = blk * 1024 + 24; // "a"
    let inodes = le32_at(&good, 1024) as u32;
    let it = le32_at(&good, 2 * 1024 + 8) as u32;
    let cases: Vec<(&str, Box<dyn Fn(&mut Vec<u8>)>)> = vec![
        ("record length 13", Box::new(move |b| b[entry + 4..entry + 6].copy_from_slice(&13u16.to_le_bytes()))),
        ("record length 8", Box::new(move |b| b[entry + 4..entry + 6].copy_from_slice(&8u16.to_le_bytes()))),
        ("record past the block", Box::new(move |b| b[entry + 4..entry + 6].copy_from_slice(&2000u16.to_le_bytes()))),
        ("name past the record", Box::new(move |b| b[entry + 6] = 200)),
        ("inode beyond the inodes", Box::new(move |b| put32_at(b, entry, inodes + 1))),
        ("the journal's inode", Box::new(move |b| put32_at(b, entry, 8))),
        ("a slash", Box::new(move |b| b[entry + 8] = b'/')),
        ("size of no blocks", Box::new(move |b| set_size(b, d, 1500))),
        ("a hole", Box::new(move |b| set_size(b, d, 2048))),
        ("a block twice", Box::new(move |b| {
            set_size(b, d, 2048);
            set_ptr(b, d, 1, blk as u32);
        })),
        ("the inode table", Box::new(move |b| set_ptr(b, d, 0, it))),
    ];
    for (what, corrupt) in cases {
        let mut data = good.clone();
        corrupt(&mut data);
        let mut fs = mount_unchecked(data);
        assert_eq!(fs.list(d).err(), Some(EUCLEAN), "{what}: list");
        assert_eq!(fs.lookup(d, "b").err(), Some(EUCLEAN), "{what}: lookup");
        assert_eq!(fs.create(d, "new", &NewNode::File, 0o644).err(), Some(EUCLEAN), "{what}: create");
        assert!(fs.unlink(ROOT_INO, "d", true).is_err(), "{what}: rmdir");
        assert!(fs.errors(), "{what}");
        // The rest works.
        assert!(fs.list(ROOT_INO).is_ok(), "{what}");
        let g = fs.create(ROOT_INO, "g", &NewNode::File, 0o644).unwrap();
        fs.write(g, 0, b"fine").unwrap();
        let mut buf = vec![0u8; 32 * 1024];
        assert_eq!(fs.read(f, 0, &mut buf).unwrap(), buf.len(), "{what}");
        let data = fs.into_device().0.data;
        assert!(disk_errors(&data), "{what}: the disk does not say it has errors");
        let fs = mount_unchecked(data);
        assert!(fs.errors());
    }
}

/// A name of a freed inode (its bitmap bit clear) is corruption: lookups and an unlink
/// fail with EUCLEAN, and nothing is freed a second time.
#[test]
fn a_name_of_a_freed_inode_frees_nothing() {
    let (mut data, _, f) = small_tree("freed-name");
    let ib = le32_at(&data, 2 * 1024 + 4);
    data[ib * 1024 + (f as usize - 1) / 8] &= !(1 << ((f - 1) % 8));
    let mut fs = mount_unchecked(data);
    let free = fs.usage().2;
    assert_eq!(fs.lookup(ROOT_INO, "f").err(), Some(EUCLEAN));
    assert_eq!(fs.unlink(ROOT_INO, "f", false).err(), Some(EUCLEAN));
    assert_eq!(fs.rename(ROOT_INO, "fast", ROOT_INO, "f").err(), Some(EUCLEAN));
    assert_eq!(fs.check(f).err(), Some(ENOENT));
    assert_eq!(fs.usage().2, free);
    assert!(fs.errors());
}

/// Block pointers into the system zone (an inode table, a bitmap): reads and writes through
/// them fail with EUCLEAN, and removing the file frees the rest of its blocks but never
/// those (the filesystem stays usable).
#[test]
fn pointers_into_metadata_are_never_followed() {
    let (mut data, _, f) = small_tree("system-zone");
    let (bb, it) = (le32_at(&data, 2 * 1024) as u32, le32_at(&data, 2 * 1024 + 8) as u32);
    set_ptr(&mut data, f, 0, it);
    set_ptr(&mut data, f, 12, bb);
    let mut fs = mount_unchecked(data);
    let mut buf = vec![0u8; 1024];
    assert_eq!(fs.read(f, 0, &mut buf).err(), Some(EUCLEAN));
    assert_eq!(fs.read(f, 13 * 1024, &mut buf).err(), Some(EUCLEAN));
    assert_eq!(fs.write(f, 0, &buf).err(), Some(EUCLEAN));
    assert_eq!(fs.read_map(f, 0, 1024).err(), Some(EUCLEAN));
    let free = fs.usage().2;
    fs.unlink_unless(ROOT_INO, "f", false, |_| false).unwrap();
    // Its 11 other direct blocks; not the inode table's, the bitmap, or what was below it.
    assert_eq!(fs.usage().2, free + 11);
    assert!(fs.errors());
    let g = fs.create(ROOT_INO, "g", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, g, 64 * 1024);
    let disk = fs.into_device().0;
    let mut fs = mount_unchecked(disk.data);
    check_file(&mut fs, g, 64 * 1024);
    assert!(fs.list(ROOT_INO).unwrap().iter().any(|(n, _, _)| n == "slow"));
}

/// Bitmaps that say a block of the system zone (an inode table's), a reserved inode (the
/// journal's) or an inode in use is free: allocations skip the group (as ext4 marks its
/// bitmap corrupt) and go elsewhere, so no file gets metadata or another file's inode.
#[test]
fn lying_bitmaps_give_out_no_metadata_and_no_inode_in_use() {
    let disk = mke2fs("lying-bitmaps", 8 * 1024, "none,has_journal,filetype,sparse_super,large_file", &["-g", "2048", "-N", "512"]);
    let mut fs = Ext2::mount(disk).unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    let h = fs.create(ROOT_INO, "h", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, h, 32 * 1024);
    let good = take(fs).data;
    let (bb, ib, it) = (le32_at(&good, 2 * 1024), le32_at(&good, 2 * 1024 + 4), le32_at(&good, 2 * 1024 + 8));
    // Group 0's first inode table block (group-relative bit) free, and inode 8 or f.
    for free in [8, f] {
        let mut data = good.clone();
        let bit = it - 1;
        data[bb * 1024 + bit / 8] &= !(1 << (bit % 8));
        data[ib * 1024 + (free as usize - 1) / 8] &= !(1 << ((free - 1) % 8));
        let mut fs = mount_unchecked(data);
        let g = fs.create(ROOT_INO, "g", &NewNode::File, 0o644).unwrap();
        assert!(g != free && g >= 11, "inode {g} given out");
        // h (in group 0) grows: its new blocks come from elsewhere.
        let mut buf = vec![0u8; CHUNK];
        for off in (32 * 1024..96 * 1024).step_by(CHUNK) {
            buf.iter_mut().enumerate().for_each(|(i, b)| *b = pattern(off + i));
            fs.write(h, off as u64, &buf).unwrap();
        }
        let (_, extents) = fs.read_map(h, 0, 96 * 1024).unwrap();
        for e in extents {
            let disk = e.disk.expect("written");
            assert!(!(disk..disk + e.len).contains(&(it as u64 * 1024)), "the inode table given out");
        }
        assert!(fs.errors());
        let disk = fs.into_device().0;
        let mut fs = mount_unchecked(disk.data);
        check_file(&mut fs, h, 96 * 1024);
        assert_eq!(fs.stat(f).unwrap().mode, 0o100644);
        assert!(fs.lookup(ROOT_INO, "g").is_ok());
    }
}

/// Indirect blocks shared at every level (a triple indirect block whose pointers all lead
/// to one double one, whose pointers all lead to one single one, whose pointers all lead
/// to one block): removing the file takes linear time, not 256³ steps; a directory whose
/// size says 4 GiB through shared tables ends at the first repeat.
#[test]
fn shared_indirect_blocks_take_linear_time() {
    let mut fs = Ext2::mount(mkfs_journal("shared-tables", 8 * 1024)).unwrap();
    let s = fs.create(ROOT_INO, "s", &NewNode::File, 0o644).unwrap();
    fs.write(s, 80 * MIB as u64, b"far away").unwrap();
    let f = fs.create(ROOT_INO, "f", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, f, 32 * 1024);
    let d = fs.create(ROOT_INO, "d", &NewNode::Dir, 0o755).unwrap();
    let mut data = take(fs).data;
    let below = |data: &[u8], t: usize| (0..256).map(|k| le32_at(data, t * 1024 + 4 * k)).find(|&b| b != 0).unwrap() as u32;
    let t3 = ptr_of(&data, s, 14);
    let t2 = below(&data, t3);
    let t1 = below(&data, t2 as usize);
    let x = below(&data, t1 as usize);
    for (table, to) in [(t3, t2), (t2 as usize, t1), (t1 as usize, x)] {
        for k in 0..256 {
            put32_at(&mut data, table * 1024 + 4 * k, to);
        }
    }
    // d: f's direct blocks, then f's indirect block whose pointers all lead to one block.
    for i in 1..13 {
        let p = ptr_of(&data, f, i) as u32;
        set_ptr(&mut data, d, i, p);
    }
    let fi = ptr_of(&data, f, 12);
    let first = le32_at(&data, fi * 1024) as u32;
    for k in 0..256 {
        put32_at(&mut data, fi * 1024 + 4 * k, first);
    }
    set_size(&mut data, d, u32::MAX & !1023);
    let mut fs = mount_unchecked(data);
    let (started, reads) = (std::time::Instant::now(), fs.device().0.counts.reads);
    assert_eq!(fs.list(d).err(), Some(EUCLEAN));
    fs.unlink_unless(ROOT_INO, "s", false, |_| false).unwrap();
    // (A freed table read again for each pointer to it: 65 536 reads of the single one.)
    let reads = fs.device().0.counts.reads - reads;
    assert!(reads < 100, "{reads} reads");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(fs.errors());
}

/// A device's block pointers hold its number, not blocks: removing a device inode frees
/// nothing (here its "pointer" is another file's block, which stays that file's).
#[test]
fn removing_a_device_frees_no_blocks() {
    let (mut data, _, f) = small_tree("device");
    let mut fs = mount_unchecked(data.clone());
    let sock = fs.lookup(ROOT_INO, "sock").unwrap();
    drop(fs);
    set_mode(&mut data, sock, 0o020644);
    let block = ptr_of(&data, f, 0) as u32;
    set_ptr(&mut data, sock, 0, block);
    let mut fs = mount_unchecked(data);
    let free = fs.usage().2;
    fs.unlink_unless(ROOT_INO, "sock", false, |_| false).unwrap();
    assert_eq!(fs.usage().2, free);
    let g = fs.create(ROOT_INO, "g", &NewNode::File, 0o644).unwrap();
    write_file(&mut fs, g, 32 * 1024);
    check_file(&mut fs, f, 32 * 1024);
    assert!(!fs.errors());
    fsck("device", &fs.into_device().0);
}

/// Symlinks whose size does not fit their form are corrupt; a slow one without its block
/// too. A new symlink's target is shorter than a block (as Linux and e2fsck want).
#[test]
fn symlink_sizes_are_checked() {
    let (good, _, _) = small_tree("symlinks");
    let mut fs = mount_unchecked(good.clone());
    let (fast, slow) = (fs.lookup(ROOT_INO, "fast").unwrap(), fs.lookup(ROOT_INO, "slow").unwrap());
    assert_eq!(fs.readlink(slow).unwrap(), "s".repeat(200));
    assert_eq!(fs.create(ROOT_INO, "long", &NewNode::Symlink("l".repeat(1024)), 0).err(), Some(ENAMETOOLONG));
    let l = fs.create(ROOT_INO, "long", &NewNode::Symlink("l".repeat(1023)), 0).unwrap();
    assert_eq!(fs.readlink(l).unwrap().len(), 1023);
    drop(fs);
    let cases: [(&str, u32, fn(&mut Vec<u8>, u32)); 3] = [
        ("fast, 100 bytes", fast, |b, i| set_size(b, i, 100)),
        ("slow, a block's", slow, |b, i| set_size(b, i, 1024)),
        ("slow, no block", slow, |b, i| set_ptr(b, i, 0, 0)),
    ];
    for (what, ino, corrupt) in cases {
        let mut data = good.clone();
        corrupt(&mut data, ino);
        let mut fs = mount_unchecked(data);
        assert_eq!(fs.readlink(ino).err(), Some(EUCLEAN), "{what}");
        assert!(fs.errors(), "{what}");
    }
}

/// Names a directory entry cannot hold are refused (empty, '/', NUL), and a directory with
/// `LINK_MAX` links takes no subdirectory (EMLINK), neither made nor moved in.
#[test]
fn names_and_link_counts_are_bounded() {
    let (mut data, d, _) = small_tree("names");
    let root = inode_offset(&data, ROOT_INO);
    data[root + 26..root + 28].copy_from_slice(&ext2fs::LINK_MAX.to_le_bytes());
    let mut fs = mount_unchecked(data);
    assert_eq!(fs.create(ROOT_INO, "", &NewNode::File, 0o644).err(), Some(ENOENT));
    assert_eq!(fs.create(ROOT_INO, "a/b", &NewNode::File, 0o644).err(), Some(EINVAL));
    assert_eq!(fs.create(ROOT_INO, "a\0b", &NewNode::File, 0o644).err(), Some(EINVAL));
    assert_eq!(fs.rename(ROOT_INO, "f", ROOT_INO, "x/y").err(), Some(EINVAL));
    assert_eq!(fs.create(ROOT_INO, "sub", &NewNode::Dir, 0o755).err(), Some(EMLINK));
    fs.create(ROOT_INO, "file", &NewNode::File, 0o644).unwrap();
    fs.create(d, "sub", &NewNode::Dir, 0o755).unwrap();
    assert_eq!(fs.rename(d, "sub", ROOT_INO, "sub").err(), Some(EMLINK));
    assert!(!fs.errors());
}

/// A directory without ".." cannot be moved (its ".." is to be set): EUCLEAN, and nothing
/// moved.
#[test]
fn a_directory_without_dotdot_is_not_moved() {
    let (mut data, d, _) = small_tree("dotdot");
    let blk = ptr_of(&data, d, 0);
    data[blk * 1024 + 12 + 8] = b'x';
    let mut fs = mount_unchecked(data);
    let e = fs.create(ROOT_INO, "e", &NewNode::Dir, 0o755).unwrap();
    assert_eq!(fs.rename(ROOT_INO, "d", e, "d").err(), Some(EUCLEAN));
    assert_eq!(fs.lookup(ROOT_INO, "d"), Ok(d));
    assert_eq!(fs.lookup(e, "d").err(), Some(ENOENT));
}

/// A directory whose next block (past its size) is another file's: a new name that needs
/// a new block fails (EUCLEAN) instead of writing over that block.
#[test]
fn a_directory_never_grows_onto_a_block_past_its_end() {
    let (mut data, d, f) = small_tree("past-end");
    let block = ptr_of(&data, f, 0) as u32;
    set_ptr(&mut data, d, 1, block);
    let mut fs = mount_unchecked(data);
    let long = |i: usize| format!("{i}{}", "n".repeat(199));
    let mut failed = None;
    for i in 0..8 {
        if let Err(e) = fs.create(d, &long(i), &NewNode::File, 0o644) {
            failed = Some(e);
            break;
        }
    }
    assert_eq!(failed, Some(EUCLEAN));
    check_file(&mut fs, f, 32 * 1024);
}
