//! A channel: the memory object both ends of a data-plane connection map
//! (docs/design/io-rings.md). The kernel creates it (`chan_create`) and
//! lays it out; this module is that layout, shared by the kernel, the
//! client (the Linux server) and the service (diskfs, netd).
//!
//! ```text
//! page 0                 header: what the kernel wrote at creation, and
//!                        `state` on its own cache line
//! submission             RingMemory<slots>: client -> service
//! completion             RingMemory<slots>: service -> client
//! ```
//!
//! Each ring starts on a page of its own. The layout is a function of the
//! slot count alone (`Layout::new`), which each end knows from its own
//! system call. The header page is mapped read-only into both ends (only
//! the kernel writes it), the rings read and write.
//!
//! `state` is set by the kernel when an end is gone (`CLIENT_GONE`,
//! `SERVICE_GONE`); from then on every futex wait on the channel's memory
//! fails at once (EPIPE), so an end sleeping on a ring's doorbell wakes and
//! sees it (`Consumer::pop_wait_while` with `live` reading `state`).

use crate::{RingMemory, RING_HEADER};
use core::sync::atomic::{AtomicU32, Ordering};

pub const PAGE: usize = 4096;
/// Slots per ring: a power of two in this range.
pub const MIN_SLOTS: u32 = 2;
pub const MAX_SLOTS: u32 = 4096;

pub const MAGIC: u32 = u32::from_le_bytes(*b"OXCH");
pub const VERSION: u32 = 1;

/// Where the parts of a channel of `slots` slots per ring lie (bytes from
/// the start of its memory object).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub slots: u32,
    pub submission: usize,
    pub completion: usize,
    /// Pages of the whole object.
    pub pages: usize,
}

impl Layout {
    /// None unless `slots` is a power of two within MIN_SLOTS..=MAX_SLOTS.
    pub const fn new(slots: u32) -> Option<Layout> {
        if !slots.is_power_of_two() || slots < MIN_SLOTS || slots > MAX_SLOTS {
            return None;
        }
        let ring = (RING_HEADER + slots as usize * 64).div_ceil(PAGE) * PAGE;
        Some(Layout { slots, submission: PAGE, completion: PAGE + ring, pages: (PAGE + 2 * ring) / PAGE })
    }

    pub fn bytes(&self) -> usize {
        self.pages * PAGE
    }

    /// The ring at `offset` (`submission` or `completion`) of the channel
    /// mapped at `base`, as a `RingMemory<N>`.
    ///
    /// # Safety
    /// `base` is the start of a mapping of the whole channel that lives for
    /// `'a`, and `N == self.slots`.
    pub unsafe fn ring<'a, const N: usize>(&self, base: *const u8, offset: usize) -> &'a RingMemory<N> {
        assert!(N == self.slots as usize && (offset == self.submission || offset == self.completion));
        unsafe { RingMemory::from_ptr(base.add(offset) as *const RingMemory<N>) }
    }
}

/// The header at offset 0. The kernel writes it once at creation (and
/// `state` when an end goes); both ends map it read-only. They may read it
/// for diagnostics, but use their own `Layout`.
#[repr(C, align(64))]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub slots: u32,
    pub pages: u32,
    pub submission: u32,
    pub completion: u32,
    _pad: [u32; 10],
    /// `CLIENT_GONE` | `SERVICE_GONE`: written by the kernel only (no end
    /// can forge it: the page is read-only to both).
    pub state: AtomicU32,
}

/// Byte offset of `Header::state`.
pub const STATE_OFFSET: usize = 64;
const _: () = assert!(core::mem::offset_of!(Header, state) == STATE_OFFSET);

/// The client closed its end or its Linux server instance ended.
pub const CLIENT_GONE: u32 = 1;
/// The service detached or its process ended.
pub const SERVICE_GONE: u32 = 2;

impl Header {
    pub fn new(layout: &Layout) -> Header {
        Header {
            magic: MAGIC,
            version: VERSION,
            slots: layout.slots,
            pages: layout.pages as u32,
            submission: layout.submission as u32,
            completion: layout.completion as u32,
            _pad: [0; 10],
            state: AtomicU32::new(0),
        }
    }

    /// The header of the channel mapped at `base`.
    ///
    /// # Safety
    /// `base` is the start of a channel mapping that lives for `'a`.
    pub unsafe fn at<'a>(base: *const u8) -> &'a Header {
        unsafe { &*(base as *const Header) }
    }

    /// The gone bits (Acquire: what the kernel did before is visible).
    pub fn state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }
}

/// The kernel's offer of a channel to a service: the payload of a control
/// request (`ipc_receive` reports it as such) to a service registered to
/// accept channels. The service maps it with `chan_attach(channel)` and
/// answers with an 8-byte status (0, or a negative errno to refuse);
/// whether the channel is attached is the kernel's to say, not the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The channel's id, for the service's channel calls.
    pub channel: u64,
    pub slots: u32,
    /// The process (pid) whose server instance connects, for the service's
    /// diagnostics.
    pub client: u64,
}

pub const OFFER_MAGIC: u32 = u32::from_le_bytes(*b"OFFR");
pub const OFFER_BYTES: usize = 32;

impl Offer {
    pub fn encode(&self) -> [u8; OFFER_BYTES] {
        let mut b = [0u8; OFFER_BYTES];
        b[0..4].copy_from_slice(&OFFER_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&VERSION.to_le_bytes());
        b[8..16].copy_from_slice(&self.channel.to_le_bytes());
        b[16..20].copy_from_slice(&self.slots.to_le_bytes());
        b[24..32].copy_from_slice(&self.client.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Offer> {
        let b: &[u8; OFFER_BYTES] = b.try_into().ok()?;
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        if u32_at(0) != OFFER_MAGIC || u32_at(4) != VERSION {
            return None;
        }
        let offer = Offer { channel: u64_at(8), slots: u32_at(16), client: u64_at(24) };
        Layout::new(offer.slots).map(|_| offer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_put_each_ring_on_its_own_pages() {
        assert_eq!(Layout::new(1), None);
        assert_eq!(Layout::new(3), None);
        assert_eq!(Layout::new(MAX_SLOTS * 2), None);
        let l = Layout::new(2).unwrap();
        assert_eq!((l.submission, l.completion, l.pages), (PAGE, 2 * PAGE, 3));
        let l = Layout::new(64).unwrap();
        // 192 + 64 * 64 bytes: two pages per ring.
        assert_eq!((l.submission, l.completion, l.pages), (PAGE, 3 * PAGE, 5));
        let l = Layout::new(MAX_SLOTS).unwrap();
        assert!(l.completion >= l.submission + RING_HEADER + MAX_SLOTS as usize * 64);
        assert_eq!(l.bytes(), l.completion + (l.completion - l.submission));
    }

    #[test]
    fn offers_round_trip_and_reject_garbage() {
        let o = Offer { channel: 0x1234_5678_9abc, slots: 64, client: 7 };
        assert_eq!(Offer::decode(&o.encode()), Some(o));
        let mut bad = o.encode();
        bad[0] ^= 1;
        assert_eq!(Offer::decode(&bad), None);
        assert_eq!(Offer::decode(&o.encode()[..31]), None);
        let odd = Offer { slots: 3, ..o };
        assert_eq!(Offer::decode(&odd.encode()), None);
    }
}
