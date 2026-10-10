//! diskfs: the ext2 filesystem server. It drives the virtio block device
//! from user space and serves the file protocol over the rings of the
//! channels clients (the Linux server instances) offer it (`service`,
//! `fsring`).
//!
//! One thread, one event loop: `ipc_receive` brings channel offers and
//! doorbells; the ring service polls its rings and the device while there
//! is work and sleeps there when there is none.

#![no_std]
#![no_main]

extern crate alloc;

mod blk;
mod service;

use alloc::vec;
use alloc::vec::Vec;
use ext2fs::Ext2;
use oxrt::println;

oxrt::entry!(main);

const ENOSYS: i64 = 38;
/// The only messages diskfs takes are the kernel's channel offers (a
/// longer one fails in the kernel).
const MAX_MESSAGE: usize = ring::channel::OFFER_BYTES;

/// Value of `key=...` among the arguments the kernel passed.
fn arg(args: &[&str], key: &str) -> Option<u64> {
    let v = args.iter().find_map(|a| a.strip_prefix(key)?.strip_prefix('='))?;
    match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

fn main(args: Vec<&'static str>) -> i32 {
    let (Some(io), Some(iolen)) = (arg(&args, "io"), arg(&args, "iolen")) else {
        println!("diskfs: started without a disk (io=, iolen=)");
        return 1;
    };
    if oxrt::ioperm(io as u16, iolen as u16).is_err() {
        println!("diskfs: no permission for the disk's I/O ports");
        return 1;
    }
    let Ok((dma, phys)) = oxrt::dma_map() else {
        println!("diskfs: no DMA memory");
        return 1;
    };
    let disk = match blk::VirtioBlk::new(io as u16, dma, phys) {
        Ok(disk) => disk,
        Err(e) => {
            println!("diskfs: {}", e);
            return 1;
        }
    };
    let (sectors, read_only) = (disk.sectors(), disk.read_only());
    let mut fs = match Ext2::mount(disk) {
        Ok(fs) => fs,
        Err(e) => {
            println!("diskfs: cannot mount: {}", e);
            return 1;
        }
    };
    // What a crash left on the orphan list goes now; what a diskfs that died left there
    // waits for its clients (`service`, "Restarts").
    let inherited = oxrt::chan_predecessors().unwrap_or(0);
    if inherited == 0 {
        match fs.recover_orphans(|_| false) {
            Ok(0) => {}
            Ok(n) => println!("diskfs: freed {} orphaned inodes", n),
            Err(e) => println!("diskfs: cannot free the orphaned inodes (errno {})", e),
        }
    } else {
        let orphans = fs.orphans().len();
        println!("diskfs: {} clients of the diskfs before; keeping {} orphaned inodes for them", inherited, orphans);
    }
    let inodes = fs.usage().3;
    let mut rings = match service::Service::new(fs.device(), fs.block_size(), inodes, inherited > 0) {
        Ok(rings) => rings,
        Err(e) => {
            println!("diskfs: cannot serve this disk: {}", e);
            return 1;
        }
    };
    // Copies to and from grants fail instead of killing diskfs when a
    // client revokes one meanwhile.
    if let Err(e) = oxrt::copy::register() {
        println!("diskfs: cannot register the copy fixup: {}", e);
        return 1;
    }
    if let Err(e) = oxrt::ipc_register_with(fsring::SERVICE, ext2fs::ROOT_INO as u64, oxrt::IPC_CHANNELS) {
        println!("diskfs: cannot register: {}", e);
        return 1;
    }
    let mode = if read_only { ", read-only" } else { "" };
    println!("diskfs: serving ext2 from a {} MiB virtio disk{}", sectors / 2048, mode);

    let mut request = vec![0u8; MAX_MESSAGE];
    loop {
        // Sleep only with nothing to do and every doorbell armed.
        let sleep = !rings.busy() && rings.prepare_sleep();
        let event = oxrt::ipc_receive(&mut request, if sleep { rings.sleep_limit() } else { Some(0) });
        rings.awake();
        match event {
            // No protocol besides the rings.
            Ok(oxrt::Event::Request(id, _)) => {
                let _ = oxrt::ipc_reply(id, &(-ENOSYS).to_le_bytes());
            }
            Ok(oxrt::Event::Control(id, len)) => {
                let status = rings.offer(&request[..len]);
                let _ = oxrt::ipc_reply(id, &status.to_le_bytes());
            }
            // A doorbell (also when a predecessor's channel went), or the end
            // of the predecessors' grace.
            Ok(oxrt::Event::Doorbell) => rings.settle(&mut fs, true),
            Ok(oxrt::Event::Timeout) => rings.settle(&mut fs, false),
            _ => {}
        }
        rings.run(&mut fs);
    }
}
