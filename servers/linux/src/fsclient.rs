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
//!
//! **Notes.** A channel to diskfs has one page of shared area with the
//! ring of diskfs's notes about inodes the instance holds (`fsring`,
//! "Notes"): the worker thread takes them (`note`) and sleeps on the ring
//! beside its own requests (`notes_sleep`, `notes_awake`).

use crate::ringclient::{self, futex_wait, futex_wake, RingClient};
use crate::sync::Mutex;
use crate::syscall;
use core::ops::Deref;
use core::sync::atomic::{AtomicU32, Ordering};
use fsring::{Buf, Completion};
use restricted::*;
use ring::{Consumer, Ring};

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
    /// The service's notes about inodes the instance holds (`fsring`,
    /// "Notes"), in the channel's shared area; taken by one thread (the
    /// worker's `datafs::take_notes`). None for a channel without them.
    notes: Option<Mutex<Consumer<'static, { fsring::NOTE_SLOTS }>>>,
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
    /// `SCRATCH_PAGES`) mapped and granted, and with `notes` the notes'
    /// ring (diskfs's).
    pub fn connect(service: &str, generation: u64, scratch_pages: u64, notes: bool) -> Result<Client, i64> {
        let ring = RingClient::connect(service, if notes { fsring::SHARED_PAGES } else { 0 })?;
        let scratch = ScratchArea::new(ring.handle(), scratch_pages.clamp(1, SCRATCH_PAGES))?;
        // (The shared area is mapped while the client lives.)
        let notes = notes.then(|| Mutex::new(Ring::new(unsafe { fsring::notes(ring.shared()) }).consumer()));
        Ok(Client { ring, generation, scratch, notes })
    }

    /// The next note, if one came.
    pub fn note(&self) -> Option<ring::Desc> {
        self.notes.as_ref()?.lock().pop()
    }

    /// Announces a sleep on the notes' ring (`ring::Consumer::prepare_sleep`):
    /// the word and the value to sleep on, None if a note came meanwhile (or
    /// the channel has none). `notes_awake` once the thread runs again.
    pub fn notes_sleep(&self) -> Option<(u64, u32)> {
        let notes = self.notes.as_ref()?;
        let value = notes.lock().prepare_sleep()?;
        Some((self.ring.shared() as u64 + ring::TAIL_OFFSET as u64, value))
    }

    pub fn notes_awake(&self) {
        if let Some(notes) = self.notes.as_ref() {
            notes.lock().awake();
        }
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

    /// A copy of `len` bytes at `offset` (as many as there are; ENOMEM
    /// without memory for it: what a program reads).
    pub fn get(&self, offset: u64, len: u64) -> Result<alloc::vec::Vec<u8>, crate::heap::NoMem> {
        let n = len.min(self.len().saturating_sub(offset)) as usize;
        let mut out = crate::heap::try_vec(n)?;
        unsafe {
            core::ptr::copy_nonoverlapping(self.base().add(offset as usize), out.as_mut_ptr(), n);
            out.set_len(n);
        }
        Ok(out)
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
