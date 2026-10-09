//! The server's end of the file protocol (`fsring`, docs/design/io-rings.md)
//! to a filesystem service: diskfs for /data (`datafs`), procfs for /proc's
//! system-wide files and /sys (`procfs`). One channel per service for the
//! instance, shared by every thread of the tree's processes (and, for
//! diskfs, by the pager thread). Its slots, the reaper and the handling of
//! a hostile or dead service are `ringclient`'s: a dead client (the service
//! gone, or a request unanswered for a minute) fails its requests with
//! EIO, and its owner makes a new one (the service is started again if it
//! died).
//!
//! **Scratch.** Names, directory entries, link targets, generated file
//! contents and O_DIRECT reads travel in a scratch buffer: a memory object
//! mapped into the server's region (`SYS_MO_MAP_SERVER`) and granted to the
//! service once, handed out in page runs (`scratch`).

use crate::ringclient::{self, futex_wait, futex_wake, RingClient};
use crate::sync::Mutex;
use crate::syscall;
use core::ops::Deref;
use core::sync::atomic::{AtomicU32, Ordering};
use fsring::{Buf, Completion};
use restricted::*;

pub use crate::ringclient::Next;

const N: usize = fsring::SLOTS as usize;
pub const PAGE: u64 = 4096;
/// The most pages a scratch buffer has (one bit each in a u64).
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
    pages: u64,
    /// One bit per page in use.
    used: Mutex<u64>,
    freed: AtomicU32,
}

/// The bits of `pages` pages from bit 0.
fn mask(pages: u64) -> u64 {
    if pages >= 64 { u64::MAX } else { (1u64 << pages) - 1 }
}

impl Client {
    /// A new channel to `service` (started again if it died), connected,
    /// with a scratch buffer of `scratch_pages` pages (at most
    /// `SCRATCH_PAGES`) mapped and granted.
    pub fn connect(service: &str, generation: u64, scratch_pages: u64) -> Result<Client, i64> {
        let ring = RingClient::connect(service, 0)?;
        let scratch = ScratchArea::new(ring.handle(), scratch_pages.clamp(1, SCRATCH_PAGES))?;
        Ok(Client { ring, generation, scratch })
    }

    /// The scratch buffer's size in pages.
    pub fn scratch_pages(&self) -> u64 {
        self.scratch.pages
    }

    /// `pages` pages of the scratch buffer (at most all of them), waiting
    /// until they are free.
    pub fn scratch(&self, pages: u64) -> Scratch<'_> {
        let area = &self.scratch;
        let pages = pages.clamp(1, area.pages);
        loop {
            let seen = area.freed.load(Ordering::Acquire);
            {
                let mut used = area.used.lock();
                if let Some(first) = (0..=area.pages - pages).find(|&f| *used & (mask(pages) << f) == 0) {
                    *used |= mask(pages) << first;
                    return Scratch { client: self, first, pages };
                }
            }
            futex_wait(&area.freed, seen);
        }
    }
}

impl ScratchArea {
    fn new(channel: u64, pages: u64) -> Result<ScratchArea, i64> {
        let object = syscall(SYS_MO_CREATE, [pages, 0, 0, 0, 0, 0]);
        if object < 0 {
            return Err(-object);
        }
        let object = object as u64;
        let addr = syscall(SYS_MO_MAP_SERVER, [object, pages, 0, 0, 0, 0]);
        if addr < 0 {
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(-addr);
        }
        let grant = syscall(SYS_GRANT, [channel, object, 0, pages, GRANT_WRITE, 0]);
        if grant <= 0 {
            syscall(SYS_MO_UNMAP_SERVER, [addr as u64, 0, 0, 0, 0, 0]);
            syscall(SYS_HANDLE_CLOSE, [object, 0, 0, 0, 0, 0]);
            return Err(if grant < 0 { -grant } else { EIO });
        }
        Ok(ScratchArea { object, addr: addr as u64, grant: grant as u32, pages, used: Mutex::new(0), freed: AtomicU32::new(0) })
    }
}

impl Drop for ScratchArea {
    fn drop(&mut self) {
        syscall(SYS_MO_UNMAP_SERVER, [self.addr, 0, 0, 0, 0, 0]);
        syscall(SYS_HANDLE_CLOSE, [self.object, 0, 0, 0, 0, 0]);
    }
}

/// Pages of the scratch buffer, free again when dropped. The service may
/// write them at any time while it has the grant: what is read back is
/// copied out once and checked.
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
        *area.used.lock() &= !(mask(self.pages) << self.first);
        area.freed.fetch_add(1, Ordering::Release);
        futex_wake(&area.freed, u32::MAX as u64);
    }
}

/// `ringclient::run` on a file service's channel.
pub fn run<T>(client: &Client, next: impl FnMut() -> Next<T>, done: impl FnMut(T, Completion) -> Option<(ring::Desc, T)>) {
    ringclient::run(&client.ring, next, done)
}
