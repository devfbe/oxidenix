//! The link protocol (ADR 0025): what a link driver (a network function
//! usbd found on USB, as the client) and netd (the service) share over a
//! channel (`ring::channel`) for one Ethernet link. Kept apart from both so
//! that its encodings and memory ordering are tested on the host.
//!
//! **The channel.** The driver makes it (a native server's `chan_create`)
//! with `SLOTS` slots per ring and `SHARED_PAGES` pages of shared area,
//! writes the link's description (`Info`) into the shared area's header
//! and offers it to netd's service (`SERVICE`), as a native server: netd
//! tells link offers from the Linux server instances' by
//! `Offer::instance` 0. One channel is one link: it comes with the offer
//! and goes with the channel (the driver closes it when the device goes;
//! netd detaches it when it finds the protocol broken).
//!
//! **Frames** travel in two byte rings of the shared area, each
//! `RING_BYTES` long: the receive ring (`rx`, frames from the wire: the
//! driver produces, netd consumes) and the send ring (`tx`, the other
//! way). A frame is an Ethernet frame without its frame check sequence, at
//! most `MAX_FRAME` bytes, stored as a *record*: an 8-byte header (its
//! length, `KIND_FRAME`) and the bytes, the next record 8-byte aligned. A
//! record never wraps around the ring's end: where the rest of the ring is
//! too short, the producer leaves a `KIND_PAD` record over it and starts
//! again at the front. Positions run freely modulo 2^32: `tail` is the
//! producer's (the bytes it put in), `head` the consumer's (the bytes it
//! took); each side keeps its own privately and reads only the other's. A
//! producer writes the record, then publishes `tail` (Release); a
//! consumer reads `tail` (Acquire), copies the record out once, then
//! publishes `head` (Release). A full ring drops the frame (the driver
//! counts it in `rx_dropped`; netd's device has no room to send: smoltcp
//! tries again later).
//!
//! **Doorbells.** The channel's rings carry no descriptors: their `tail`
//! words are the two ends' doorbell counters, the words the kernel's
//! doorbell watches compare (the submission ring's is netd's: `chan_watch`;
//! the completion ring's the driver's: `chan_client_watch`). A side that
//! sleeps announces it in its `sleeping` word, fences, reads its counter,
//! looks for work once more and sleeps (a watch armed with the value read)
//! only if there is none; it clears `sleeping` once it runs again. A side
//! that made work for the other (records, a carrier change) fences after
//! publishing it and, if the other announced a sleep, advances the other's
//! counter and wakes it (a futex wake on it): either the sleeper sees the
//! work, or the waker sees the announcement and the counter it moves
//! makes a watch armed with the old value fail (EAGAIN) or fire. No wakeup
//! is lost (`tests/protocol.rs`).
//!
//! **State** besides the frames: `carrier` (the driver's: whether the
//! medium is there, from the function's notifications) and the counters
//! of dropped frames. The peer may be hostile: every length, position and
//! record is checked (`Violation`); netd lets go of a link whose driver
//! broke the protocol.

#![no_std]

use core::sync::atomic::{fence, AtomicU32, Ordering};
use ring::channel::Layout;
pub use ring::Wait;

/// The service's IPC name (netd's, `netring::SERVICE`).
pub const SERVICE: &str = "net";
/// Slots per ring: the rings are doorbells only.
pub const SLOTS: u32 = 2;
pub const PAGE: usize = 4096;
/// Bytes of each frame ring (a power of two).
pub const RING_BYTES: usize = 256 * 1024;
/// Pages of the shared area: the header page, the receive ring, the send
/// ring.
pub const SHARED_PAGES: u32 = (1 + 2 * RING_BYTES / PAGE) as u32;
/// The longest frame (Ethernet header and payload, no FCS) either side
/// may put in.
pub const MAX_FRAME: usize = 2048;
/// The shortest frame (an Ethernet header).
pub const MIN_FRAME: usize = 14;
/// Bytes of a record's header.
pub const RECORD_HEADER: usize = 8;
pub const KIND_FRAME: u32 = 1;
pub const KIND_PAD: u32 = 2;

pub const MAGIC: u32 = u32::from_le_bytes(*b"LINK");
pub const VERSION: u32 = 1;
/// An Ethernet link (the only kind).
pub const KIND_ETHERNET: u16 = 1;
/// Bytes of `Info::driver`.
pub const DRIVER_BYTES: usize = 24;

/// The link as its driver describes it, written once before the offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Its hardware address (unicast, not zero).
    pub mac: [u8; 6],
    /// The largest IP packet it carries (576..=`MAX_FRAME` - 14).
    pub mtu: u32,
    /// What drives it, for messages: printable ASCII, NUL padded (e.g.
    /// "rndis", "cdc_ether", "cdc_ncm", "ipheth").
    pub driver: [u8; DRIVER_BYTES],
}

impl Info {
    /// `driver` as a string (what is valid of it).
    pub fn driver_name(&self) -> &str {
        let len = self.driver.iter().position(|&b| b == 0).unwrap_or(DRIVER_BYTES);
        core::str::from_utf8(&self.driver[..len]).unwrap_or("?")
    }

    /// `name` as a `driver` field (cut to fit).
    pub fn driver_field(name: &str) -> [u8; DRIVER_BYTES] {
        let mut out = [0u8; DRIVER_BYTES];
        let n = name.len().min(DRIVER_BYTES);
        out[..n].copy_from_slice(&name.as_bytes()[..n]);
        out
    }
}

/// The peer broke the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Violation {
    /// The description is malformed (magic, version, kind, MTU, address,
    /// driver name).
    Info,
    /// A ring position beyond what the ring can hold.
    Position,
    /// A record that is malformed, too long, or reaches past what was put
    /// in or past the ring's end.
    Record,
}

/// One cache line of its own per word a side writes often.
#[repr(C, align(64))]
struct Line(AtomicU32);

/// The shared area's header page.
#[repr(C)]
struct Header {
    magic: AtomicU32,
    version: AtomicU32,
    /// `KIND_ETHERNET` (low half); the high half is 0.
    kind: AtomicU32,
    mtu: AtomicU32,
    /// The address's bytes 0-3 and 4-5 (little endian).
    mac_low: AtomicU32,
    mac_high: AtomicU32,
    driver: [AtomicU32; DRIVER_BYTES / 4],
    _reserved: [u32; 2],
    /// The driver's: 1 while the medium is there.
    carrier: Line,
    /// The driver's: frames from the wire dropped for want of room.
    rx_dropped: Line,
    rx_tail: Line,
    rx_head: Line,
    tx_tail: Line,
    tx_head: Line,
    /// Each side's announcement of its sleep.
    driver_sleeping: Line,
    netd_sleeping: Line,
}

const _: () = assert!(core::mem::size_of::<Header>() <= PAGE);
const _: () = assert!(RING_BYTES.is_power_of_two() && RING_BYTES % 8 == 0 && RING_BYTES >= 2 * (RECORD_HEADER + MAX_FRAME));

/// Bytes a record of `len` bytes of frame takes.
pub const fn span(len: usize) -> usize {
    RECORD_HEADER + len.div_ceil(8) * 8
}

/// Either side's view of a link channel mapped in its address space.
#[derive(Clone, Copy)]
pub struct Area {
    header: *const Header,
    rx: *mut u8,
    tx: *mut u8,
    /// The doorbell counters: netd's (the submission ring's tail) and the
    /// driver's (the completion ring's).
    netd_bell: *const AtomicU32,
    driver_bell: *const AtomicU32,
}

// The area is shared memory: every access to it is atomic or a copy the
// protocol above orders.
unsafe impl Send for Area {}

impl Area {
    /// The view of the channel mapped at `base` with `layout` (None for a
    /// layout that is not a link channel's).
    ///
    /// # Safety
    /// `base` maps the whole channel, readable and writable from the
    /// submission ring on, for as long as the view and what it hands out
    /// are used, and `layout` is the channel's.
    pub unsafe fn new(base: *mut u8, layout: &Layout) -> Option<Area> {
        if layout.slots != SLOTS || layout.shared_pages != SHARED_PAGES as usize {
            return None;
        }
        let shared = unsafe { base.add(layout.shared) };
        Some(Area {
            header: shared as *const Header,
            rx: unsafe { shared.add(PAGE) },
            tx: unsafe { shared.add(PAGE + RING_BYTES) },
            netd_bell: unsafe { base.add(layout.submission + ring::TAIL_OFFSET) } as *const AtomicU32,
            driver_bell: unsafe { base.add(layout.completion + ring::TAIL_OFFSET) } as *const AtomicU32,
        })
    }

    fn h(&self) -> &Header {
        unsafe { &*self.header }
    }

    // ------------------------------------------------------------ driver

    /// The driver describes the link (before it offers the channel).
    pub fn init(&self, info: &Info) {
        let h = self.h();
        h.kind.store(KIND_ETHERNET as u32, Ordering::Relaxed);
        h.mtu.store(info.mtu, Ordering::Relaxed);
        h.mac_low.store(u32::from_le_bytes(info.mac[..4].try_into().expect("4 bytes")), Ordering::Relaxed);
        h.mac_high.store(u16::from_le_bytes([info.mac[4], info.mac[5]]) as u32, Ordering::Relaxed);
        for (i, word) in h.driver.iter().enumerate() {
            word.store(u32::from_le_bytes(info.driver[4 * i..4 * i + 4].try_into().expect("4 bytes")), Ordering::Relaxed);
        }
        h.version.store(VERSION, Ordering::Relaxed);
        h.magic.store(MAGIC, Ordering::Release);
    }

    /// The driver's word on the medium: netd hears of it (rung if asleep).
    pub fn set_carrier(&self, up: bool, w: &impl Wait) {
        if self.h().carrier.0.swap(up as u32, Ordering::SeqCst) != up as u32 {
            self.ring_netd(w);
        }
    }

    /// A frame from the wire was dropped for want of room.
    pub fn count_rx_dropped(&self) {
        self.h().rx_dropped.0.fetch_add(1, Ordering::Relaxed);
    }

    /// The receive ring's producing end (the driver's, one).
    pub fn rx_producer(&self) -> Producer {
        Producer::new(self.rx, &self.h().rx_tail.0, &self.h().rx_head.0)
    }

    /// The send ring's consuming end (the driver's, one).
    pub fn tx_consumer(&self) -> Consumer {
        Consumer::new(self.tx, &self.h().tx_tail.0, &self.h().tx_head.0)
    }

    /// The driver made work for netd (records it published, room it made
    /// in the send ring, a carrier change): rings netd's doorbell if netd
    /// announced a sleep.
    pub fn ring_netd(&self, w: &impl Wait) {
        fence(Ordering::SeqCst);
        if self.h().netd_sleeping.0.load(Ordering::Relaxed) != 0 {
            let bell = unsafe { &*self.netd_bell };
            bell.fetch_add(1, Ordering::SeqCst);
            w.wake(bell);
        }
    }

    /// The driver announces its sleep: the value of its doorbell counter
    /// to arm its watch with, or None if netd put frames in meanwhile
    /// (`tx` is the driver's consumer: it looks at it once more). The
    /// driver calls `driver_awake` once it runs again.
    pub fn prepare_driver_sleep(&self, tx: &Consumer) -> Option<u32> {
        self.h().driver_sleeping.0.store(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let value = unsafe { &*self.driver_bell }.load(Ordering::SeqCst);
        if !tx.is_empty() {
            self.driver_awake();
            return None;
        }
        Some(value)
    }

    pub fn driver_awake(&self) {
        self.h().driver_sleeping.0.store(0, Ordering::Relaxed);
    }

    // -------------------------------------------------------------- netd

    /// The link's description, checked (netd, after it attached).
    pub fn info(&self) -> Result<Info, Violation> {
        let h = self.h();
        if h.magic.load(Ordering::Acquire) != MAGIC || h.version.load(Ordering::Relaxed) != VERSION || h.kind.load(Ordering::Relaxed) != KIND_ETHERNET as u32 {
            return Err(Violation::Info);
        }
        let mtu = h.mtu.load(Ordering::Relaxed);
        let (low, high) = (h.mac_low.load(Ordering::Relaxed).to_le_bytes(), h.mac_high.load(Ordering::Relaxed));
        if high > 0xffff {
            return Err(Violation::Info);
        }
        let high = (high as u16).to_le_bytes();
        let mac = [low[0], low[1], low[2], low[3], high[0], high[1]];
        let mut driver = [0u8; DRIVER_BYTES];
        for (i, word) in h.driver.iter().enumerate() {
            driver[4 * i..4 * i + 4].copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        let len = driver.iter().position(|&b| b == 0).unwrap_or(DRIVER_BYTES);
        let printable = driver[..len].iter().all(|b| b.is_ascii_graphic()) && driver[len..].iter().all(|&b| b == 0);
        if !(576..=(MAX_FRAME - MIN_FRAME) as u32).contains(&mtu) || mac[0] & 1 != 0 || mac == [0; 6] || !printable {
            return Err(Violation::Info);
        }
        Ok(Info { mac, mtu, driver })
    }

    /// Whether the medium is there, as the driver says.
    pub fn carrier(&self) -> bool {
        self.h().carrier.0.load(Ordering::Acquire) != 0
    }

    /// Frames the driver dropped for want of room.
    pub fn rx_dropped(&self) -> u32 {
        self.h().rx_dropped.0.load(Ordering::Relaxed)
    }

    /// The receive ring's consuming end (netd's, one).
    pub fn rx_consumer(&self) -> Consumer {
        Consumer::new(self.rx, &self.h().rx_tail.0, &self.h().rx_head.0)
    }

    /// The send ring's producing end (netd's, one).
    pub fn tx_producer(&self) -> Producer {
        Producer::new(self.tx, &self.h().tx_tail.0, &self.h().tx_head.0)
    }

    /// netd made work for the driver (records it published): rings the
    /// driver's doorbell if it announced a sleep. (Room in the receive
    /// ring needs no doorbell: the driver drops what does not fit.)
    pub fn ring_driver(&self, w: &impl Wait) {
        fence(Ordering::SeqCst);
        if self.h().driver_sleeping.0.load(Ordering::Relaxed) != 0 {
            let bell = unsafe { &*self.driver_bell };
            bell.fetch_add(1, Ordering::SeqCst);
            w.wake(bell);
        }
    }

    /// netd announces its sleep: the value of its doorbell counter to arm
    /// its watch with (`chan_watch`), or None if the driver put frames in
    /// or changed the carrier meanwhile (`rx`: netd's consumer; `carrier`:
    /// what netd knows of it). netd calls `netd_awake` once it runs again.
    pub fn prepare_netd_sleep(&self, rx: &Consumer, carrier: bool) -> Option<u32> {
        self.h().netd_sleeping.0.store(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let value = unsafe { &*self.netd_bell }.load(Ordering::SeqCst);
        if !rx.is_empty() || self.carrier() != carrier {
            self.netd_awake();
            return None;
        }
        Some(value)
    }

    pub fn netd_awake(&self) {
        self.h().netd_sleeping.0.store(0, Ordering::Relaxed);
    }
}

/// The producing end of a frame ring.
pub struct Producer {
    ring: *mut u8,
    tail_word: *const AtomicU32,
    head_word: *const AtomicU32,
    /// Ours: never read back from shared memory.
    tail: u32,
}

unsafe impl Send for Producer {}

impl Producer {
    fn new(ring: *mut u8, tail_word: &AtomicU32, head_word: &AtomicU32) -> Producer {
        Producer { ring, tail_word, head_word, tail: tail_word.load(Ordering::Relaxed) }
    }

    /// Whether a frame of `len` bytes fits now.
    pub fn has_room(&self, len: usize) -> bool {
        self.room_for(len).is_some()
    }

    /// The bytes to skip at the end (a pad record) and the record's span,
    /// if a frame of `len` bytes fits.
    fn room_for(&self, len: usize) -> Option<(usize, usize)> {
        if !(MIN_FRAME..=MAX_FRAME).contains(&len) {
            return None;
        }
        let head = unsafe { &*self.head_word }.load(Ordering::Acquire);
        // (A consumer that published a head beyond ours broke the
        // protocol: no room then, and no frame is lost to it.)
        let fill = self.tail.wrapping_sub(head) as usize;
        if fill > RING_BYTES {
            return None;
        }
        let span = span(len);
        let offset = self.tail as usize % RING_BYTES;
        let pad = if RING_BYTES - offset < span { RING_BYTES - offset } else { 0 };
        (RING_BYTES - fill >= pad + span).then_some((pad, span))
    }

    /// Puts a frame of `len` bytes in, written by `fill` into its place:
    /// false (nothing put in, `fill` not called) without room or for a
    /// length beyond `MIN_FRAME..=MAX_FRAME`. The record is published
    /// after `fill` returns; the caller rings the consumer's doorbell
    /// (after a batch).
    pub fn push_with(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        let Some((pad, span)) = self.room_for(len) else { return false };
        if pad > 0 {
            self.write_header(self.tail as usize % RING_BYTES, 0, KIND_PAD);
            self.tail = self.tail.wrapping_add(pad as u32);
        }
        let offset = self.tail as usize % RING_BYTES;
        self.write_header(offset, len as u32, KIND_FRAME);
        let data = unsafe { core::slice::from_raw_parts_mut(self.ring.add(offset + RECORD_HEADER), len) };
        fill(data);
        self.tail = self.tail.wrapping_add(span as u32);
        unsafe { &*self.tail_word }.store(self.tail, Ordering::Release);
        true
    }

    /// `push_with` of a copy of `frame`.
    pub fn push(&mut self, frame: &[u8]) -> bool {
        self.push_with(frame.len(), |dst| dst.copy_from_slice(frame))
    }

    fn write_header(&self, offset: usize, len: u32, kind: u32) {
        let mut header = [0u8; RECORD_HEADER];
        header[..4].copy_from_slice(&len.to_le_bytes());
        header[4..].copy_from_slice(&kind.to_le_bytes());
        unsafe { core::ptr::copy_nonoverlapping(header.as_ptr(), self.ring.add(offset), RECORD_HEADER) };
    }
}

/// The consuming end of a frame ring.
pub struct Consumer {
    ring: *const u8,
    tail_word: *const AtomicU32,
    head_word: *const AtomicU32,
    /// Ours: never read back from shared memory.
    head: u32,
}

unsafe impl Send for Consumer {}

impl Consumer {
    fn new(ring: *const u8, tail_word: &AtomicU32, head_word: &AtomicU32) -> Consumer {
        Consumer { ring, tail_word, head_word, head: head_word.load(Ordering::Relaxed) }
    }

    /// Whether the ring holds nothing (as far as the producer published).
    pub fn is_empty(&self) -> bool {
        unsafe { &*self.tail_word }.load(Ordering::Acquire) == self.head
    }

    /// Takes the next frame out, copied into `buf` (`MAX_FRAME` bytes
    /// hold any): its length, or None if the ring is empty. The room it
    /// leaves is published at once; the caller rings the producer's
    /// doorbell if the producer may wait for room (after a batch).
    pub fn pop(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Violation> {
        loop {
            let tail = unsafe { &*self.tail_word }.load(Ordering::Acquire);
            let fill = tail.wrapping_sub(self.head) as usize;
            if fill == 0 {
                return Ok(None);
            }
            if fill > RING_BYTES || fill % 8 != 0 {
                return Err(Violation::Position);
            }
            let offset = self.head as usize % RING_BYTES;
            let mut header = [0u8; RECORD_HEADER];
            unsafe { core::ptr::copy_nonoverlapping(self.ring.add(offset), header.as_mut_ptr(), RECORD_HEADER) };
            let len = u32::from_le_bytes(header[..4].try_into().expect("4 bytes")) as usize;
            let kind = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
            match kind {
                KIND_PAD => {
                    // Over the rest of the ring, and only where a record of
                    // the longest frame would not fit.
                    let rest = RING_BYTES - offset;
                    if rest > fill || rest >= span(MAX_FRAME) {
                        return Err(Violation::Record);
                    }
                    self.head = self.head.wrapping_add(rest as u32);
                    unsafe { &*self.head_word }.store(self.head, Ordering::Release);
                }
                KIND_FRAME => {
                    let span = span(len);
                    if !(MIN_FRAME..=MAX_FRAME).contains(&len) || span > fill || offset + span > RING_BYTES {
                        return Err(Violation::Record);
                    }
                    let dst = buf.get_mut(..len).ok_or(Violation::Record)?;
                    unsafe { core::ptr::copy_nonoverlapping(self.ring.add(offset + RECORD_HEADER), dst.as_mut_ptr(), len) };
                    self.head = self.head.wrapping_add(span as u32);
                    unsafe { &*self.head_word }.store(self.head, Ordering::Release);
                    return Ok(Some(len));
                }
                _ => return Err(Violation::Record),
            }
        }
    }
}
