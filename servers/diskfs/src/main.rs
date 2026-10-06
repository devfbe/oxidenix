//! diskfs: the ext2 filesystem server. It drives the ATA disk from user
//! space and answers the kernel's filesystem requests (see `fsproto`).

#![no_std]
#![no_main]

extern crate alloc;

mod ata;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use ext2fs::{Ext2, NewNode};
use fsproto::*;
use oxrt::println;

oxrt::entry!(main);

const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;

fn main(_args: Vec<&'static str>) -> i32 {
    // Only the primary bus's command block and its control register.
    if oxrt::ioperm(0x1f0, 8).and(oxrt::ioperm(0x3f6, 1)).is_err() {
        println!("diskfs: no permission for the ATA ports");
        return 1;
    }
    let Some(disk) = ata::Ata::probe() else {
        println!("diskfs: no data disk (primary slave)");
        return 1;
    };
    let sectors = disk.sectors();
    let mut fs = match Ext2::mount(disk) {
        Ok(fs) => fs,
        Err(e) => {
            println!("diskfs: cannot mount: {}", e);
            return 1;
        }
    };
    if let Err(e) = oxrt::ipc_register("diskfs", ext2fs::ROOT_INO as u64) {
        println!("diskfs: cannot register: {}", e);
        return 1;
    }
    println!("diskfs: serving ext2 from a {} MiB disk (pid {})", sectors / 2048, oxrt::getpid());

    let mut request = vec![0u8; MAX_MESSAGE];
    let mut response = vec![0u8; MAX_MESSAGE];
    loop {
        let Ok(oxrt::Event::Request(id, len)) = oxrt::ipc_receive(&mut request, None) else { continue };
        let n = match decode_request(&request[..len]) {
            Some(req) => handle(&mut fs, &req, &mut response),
            None => encode_response(&mut response, -EINVAL, [0; 6], &[]),
        };
        let _ = oxrt::ipc_reply(id, &response[..n]);
    }
}

fn name(bytes: &[u8]) -> Result<&str, i64> {
    core::str::from_utf8(bytes).map_err(|_| -EINVAL)
}

fn inodes_payload(inodes: &[u32]) -> Vec<u8> {
    inodes.iter().flat_map(|i| i.to_le_bytes()).collect()
}

/// Executes one request and writes the response; returns its length.
fn handle(fs: &mut Ext2<ata::Ata>, req: &Request, out: &mut [u8]) -> usize {
    let [a0, a1, a2, _] = req.args;
    let ino = a0 as u32;
    let result: Result<(i64, [u64; 6], Vec<u8>), i64> = (|| {
        let op = req.op.ok_or(-ENOSYS)?;
        let ok = |status: i64| Ok((status, [0; 6], Vec::new()));
        match op {
            Op::Stat => {
                let s = fs.stat(ino).map_err(|e| -e)?;
                let v = [s.mode as u64, s.size, s.links as u64, s.atime as u64, s.mtime as u64, s.ctime as u64];
                Ok((0, v, Vec::new()))
            }
            Op::Read => {
                let mut buf = vec![0u8; (a2 as usize).min(MAX_DATA)];
                let n = fs.read(ino, a1, &mut buf).map_err(|e| -e)?;
                buf.truncate(n);
                Ok((n as i64, [0; 6], buf))
            }
            Op::Write => ok(fs.write(ino, a1, req.payload).map_err(|e| -e)? as i64),
            Op::Truncate => fs.truncate(ino, a1).map_err(|e| -e).and_then(|_| ok(0)),
            Op::List => {
                let entries = fs.list(ino).map_err(|e| -e)?;
                let mut payload = Vec::new();
                let mut next = a1 as usize;
                for (n, i, t) in entries.iter().skip(next) {
                    if payload.len() + 6 + n.len() > MAX_DATA {
                        break;
                    }
                    push_entry(&mut payload, *i, *t, n.as_bytes());
                    next += 1;
                }
                let cursor = if next >= entries.len() { 0 } else { next as u64 };
                Ok((0, [cursor, 0, 0, 0, 0, 0], payload))
            }
            Op::Lookup => {
                let child = fs.lookup(ino, name(req.payload)?).map_err(|e| -e)?;
                Ok((0, [child as u64, 0, 0, 0, 0, 0], Vec::new()))
            }
            Op::Create => {
                let (n, target) = match req.payload.iter().position(|&b| b == 0) {
                    Some(p) => (&req.payload[..p], &req.payload[p + 1..]),
                    None => (req.payload, &[][..]),
                };
                let kind = match a1 {
                    KIND_FILE => NewNode::File,
                    KIND_DIR => NewNode::Dir,
                    KIND_SYMLINK => NewNode::Symlink(String::from(name(target)?)),
                    _ => return Err(-EINVAL),
                };
                let child = fs.create(ino, name(n)?, &kind, a2 as u32).map_err(|e| -e)?;
                Ok((0, [child as u64, 0, 0, 0, 0, 0], Vec::new()))
            }
            Op::Unlink => {
                let gone = fs.unlink(ino, name(req.payload)?, a1 != 0).map_err(|e| -e)?;
                Ok((0, [0; 6], inodes_payload(&gone)))
            }
            Op::Rename => {
                let split = (a2 as usize).min(req.payload.len());
                let (old, new) = req.payload.split_at(split);
                let gone = fs.rename(ino, name(old)?, a1 as u32, name(new)?).map_err(|e| -e)?;
                Ok((0, [0; 6], inodes_payload(&gone)))
            }
            Op::Release => fs.release(ino).map_err(|e| -e).and_then(|_| ok(0)),
            Op::Readlink => {
                let target = fs.readlink(ino).map_err(|e| -e)?;
                Ok((0, [0; 6], target.into_bytes()))
            }
            Op::SetPerm => fs.set_perm(ino, a1 as u32).map_err(|e| -e).and_then(|_| ok(0)),
            Op::Usage => {
                let (bs, blocks, free, inodes, free_inodes) = fs.usage();
                Ok((0, [bs, blocks, free, inodes, free_inodes, 0], Vec::new()))
            }
        }
    })();
    match result {
        Ok((status, values, payload)) => encode_response(out, status, values, &payload),
        Err(e) => encode_response(out, e, [0; 6], &[]),
    }
}
