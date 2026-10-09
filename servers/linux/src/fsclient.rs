//! The server's end of the file protocol (`fsring`, docs/design/io-rings.md)
//! to diskfs: one channel for the instance, shared by every thread of the
//! tree's processes and by the pager thread. Its slots, the reaper and the
//! handling of a hostile or dead diskfs are `ringclient`'s: a dead client
//! (diskfs gone, or a request unanswered for a minute) fails its requests
//! with EIO, and `datafs` makes a new one (diskfs is started again if it
//! died).
//!
//! **Scratch.** Names, directory entries, link targets and O_DIRECT reads
//! travel in a scratch buffer: a memory object mapped into the server's
//! region (`SYS_MO_MAP_SERVER`) and granted to diskfs once, handed out in
//! page runs (`scratch`).

use crate::ringclient::{self, futex_wait, futex_wake, RingClient};
use crate::sync::Mutex;
use crate::syscall;
use core::ops::Deref;
use core::sync::atomic::{AtomicU32, Ordering};
use fsring::{Buf, Completion, SERVICE};
use restricted::*;

pub use crate::ringclient::Next;

const N: usize = fsring::SLOTS as usize;
pub const PAGE: u64 = 4096;
/// Pages of the scratch buffer.
pub const SCRATCH_PAGES: u64 = 64;

pub const EIO: i64 = ringclient::EIO;

pub struct Client {
    /// First: dropped first, so the channel (and its grant of the scratch
    /// buffer) goes before the scratch buffer's memory.
    ring: RingClient<N>,
    /// Which connection this is (`datafs` counts them).
    pub generation: u64,
    scratch: ScratchArea,
}

impl Deref for Client {
    type Target = RingClient<N>;

    fn deref(&self) -> &RingClient<N> {
        &self.ring
    }
}

struct ScratchArea {
    object: u64,
    addr: u64,
    grant: u32,
    /// One bit per page in use.
    used: Mutex<u64>,
    freed: AtomicU32,
}

impl Client {
    /// A new channel to diskfs (started again if it died), connected, with
    /// its scratch buffer mapped and granted.
    pub fn connect(generation: u64) -> Result<Client, i64> {
        let ring = RingClient::connect(SERVICE, 0)?;
        let scratch = ScratchArea::new(ring.handle())?;
        Ok(Client { ring, generation, scratch })
    }

    /// `pages` pages of the scratch buffer (at most `SCRATCH_PAGES`), waiting
    /// until they are free.
    pub fn scratch(&self, pages: u64) -> Scratch<'_> {
        let pages = pages.clamp(1, SCRATCH_PAGES);
        let mask = if pages == 64 { u64::MAX } else { (1u64 << pages) - 1 };
        let area = &self.scratch;
        loop {
            let seen = area.freed.load(Ordering::Acquire);
            {
                let mut used = area.used.lock();
                if let Some(first) = (0..=SCRATCH_PAGES - pages).find(|&f| *used & (mask << f) == 0) {
                    *used |= mask << first;
                    return Scratch { client: self, first, pages };
                }
            }
            futex_wait(&area.freed, seen);
        }
    }
}

impl ScratchArea {
    fn new(channel: u64) -> Result<ScratchArea, i64> {
        let object = syscall(SYS_MO_CREATE, [SCRATCH_PAGES, 0, 0, 0, 0, 0]);
        if object < 0 {
            return Err(-object);
        }
        let object = object as u64;
        let addr = syscall(SYS_MO_MAP_SERVER, [object, SCRATCH_PAGES, 0, 0, 0, 0]);
        if addr < 0 {
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(-addr);
        }
        let grant = syscall(SYS_GRANT, [channel, object, 0, SCRATCH_PAGES, GRANT_WRITE, 0]);
        if grant <= 0 {
            syscall(SYS_MO_UNMAP_SERVER, [addr as u64, 0, 0, 0, 0, 0]);
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(if grant < 0 { -grant } else { EIO });
        }
        Ok(ScratchArea { object, addr: addr as u64, grant: grant as u32, used: Mutex::new(0), freed: AtomicU32::new(0) })
    }
}

impl Drop for ScratchArea {
    fn drop(&mut self) {
        syscall(SYS_MO_UNMAP_SERVER, [self.addr, 0, 0, 0, 0, 0]);
        syscall(SYS_HANDLE_CLOSE, [self.object, 0, 0, 0, 0, 0]);
    }
}

/// Pages of the scratch buffer, free again when dropped. diskfs may write
/// them at any time while it has the grant: what is read back is copied
/// out once and checked.
pub struct Scratch<'a> {
    client: &'a Client,
    first: u64,
    pages: u64,
}

impl Scratch<'_> {
    pub fn len(&self) -> u64 {
        self.pages * PAGE
    }

    /// The range at `offset` (within these pages) as a buffer of the grant.
    pub fn buf(&self, offset: u64, len: u64) -> Buf {
        let at = (self.first * PAGE + offset.min(self.len())) as u32;
        Buf { grant: self.client.scratch.grant, offset: at, len: len.min(self.len() - offset.min(self.len())) as u32 }
    }

    fn base(&self) -> *mut u8 {
        (self.client.scratch.addr + self.first * PAGE) as *mut u8
    }

    /// Copies `data` in at `offset` (as much as fits).
    pub fn put(&self, offset: u64, data: &[u8]) {
        let n = (data.len() as u64).min(self.len().saturating_sub(offset)) as usize;
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.base().add(offset as usize), n) };
    }

    /// A copy of `len` bytes at `offset` (as many as there are).
    pub fn get(&self, offset: u64, len: u64) -> alloc::vec::Vec<u8> {
        let n = len.min(self.len().saturating_sub(offset)) as usize;
        let mut out = alloc::vec![0u8; n];
        unsafe { core::ptr::copy_nonoverlapping(self.base().add(offset as usize), out.as_mut_ptr(), n) };
        out
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        let area = &self.client.scratch;
        let mask = if self.pages == 64 { u64::MAX } else { (1u64 << self.pages) - 1 };
        *area.used.lock() &= !(mask << self.first);
        area.freed.fetch_add(1, Ordering::Release);
        futex_wake(&area.freed, u32::MAX as u64);
    }
}

/// `ringclient::run` on diskfs's channel.
pub fn run<T>(client: &Client, next: impl FnMut() -> Next<T>, done: impl FnMut(T, Completion) -> Option<(ring::Desc, T)>) {
    ringclient::run(&client.ring, next, done)
}
