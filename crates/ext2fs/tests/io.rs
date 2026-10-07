//! ext2fs on a RAM disk that counts device requests: large reads and writes
//! take few requests, every operation flushes once, and the filesystem
//! stays consistent for e2fsck. Needs mke2fs and e2fsck (e2fsprogs) in PATH:
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
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        self.counts.flushes += 1;
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
    RamDisk { data, counts: Counts::default(), fail_at: None, fail_count: 0 }
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
fn large_writes_take_few_requests_and_one_flush() {
    let mut fs = Ext2::mount(mkfs("writes", 16 * 1024)).unwrap();
    let ino = fs.create(ROOT_INO, "big", &NewNode::File, 0o644).unwrap();
    let before = fs.device().counts.clone();
    write_file(&mut fs, ino, MIB);
    let after = fs.device().counts.clone();
    let requests = 32;
    assert_eq!(after.flushes - before.flushes, requests, "one flush per write");
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
