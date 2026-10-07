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
}

impl Device for RamDisk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        let at = lba as usize * 512;
        buf.copy_from_slice(self.data.get(at..at + buf.len()).ok_or(())?);
        self.counts.reads += 1;
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
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
        1_800_000_000
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
    RamDisk { data, counts: Counts::default() }
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
