//! ARP (RFC 826) for one Ethernet link: its packets and the cache of the
//! neighbours' hardware addresses.
//!
//! **The cache** holds an entry per neighbour, in one of three states, as
//! Linux's neighbour table does with fewer of them:
//! - *incomplete*: asked for by broadcast (`REQUESTS` times,
//!   `RETRANSMIT_MS` apart); the packets to it wait (`PENDING` of them per
//!   link, the oldest dropped first). Unanswered, the entry and its
//!   packets go.
//! - *reachable*: confirmed by an ARP packet from it within
//!   `REACHABLE_MS` (Linux's base_reachable_time). Past that it is stale:
//!   still used, but its first use starts a probe; a stale entry unused for
//!   `GC_MS` goes (or sooner, when the cache is full and room is needed).
//! - *probe*: stale and in use: asked for by unicast to the address it had,
//!   which is used meanwhile; unanswered `REQUESTS` times, it goes (and the
//!   next packet asks by broadcast again).
//!
//! Learning follows RFC 826's merge rule: an ARP packet from a neighbour
//! updates its entry if there is one, and makes one only if the packet is
//! meant for this host (its target is the link's address). So a
//! gratuitous ARP updates, but no one fills the cache from afar.

use crate::{is_group_mac, is_unicast, ETHERNET_HEADER, ETHERTYPE_ARP};
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Entries per link.
pub const ENTRIES: usize = 64;
/// Packets waiting for resolution, per link.
pub const PENDING: usize = 16;
pub const REACHABLE_MS: u64 = 30_000;
pub const RETRANSMIT_MS: u64 = 1_000;
/// Requests before an address counts as unreachable (Linux's mcast_solicit
/// and ucast_solicit).
pub const REQUESTS: u32 = 3;
pub const GC_MS: u64 = 60_000;

/// The bytes of an ARP packet for IPv4 over Ethernet, and of its frame.
pub const PACKET: usize = 28;
pub const FRAME: usize = ETHERNET_HEADER + PACKET;

pub const REQUEST: u16 = 1;
pub const REPLY: u16 = 2;

/// An ARP packet for IPv4 over Ethernet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    /// `REQUEST` or `REPLY`.
    pub op: u16,
    pub sender_mac: [u8; 6],
    pub sender_ip: u32,
    pub target_mac: [u8; 6],
    pub target_ip: u32,
}

impl Packet {
    /// The packet in an Ethernet frame's payload (trailing padding
    /// allowed): None unless it is IPv4 over Ethernet, a request or reply.
    pub fn parse(payload: &[u8]) -> Option<Packet> {
        let p = payload.get(..PACKET)?;
        let u16_at = |o: usize| u16::from_be_bytes([p[o], p[o + 1]]);
        let u32_at = |o: usize| u32::from_be_bytes(p[o..o + 4].try_into().expect("4 bytes"));
        if u16_at(0) != 1 || u16_at(2) != crate::ETHERTYPE_IPV4 || p[4] != 6 || p[5] != 4 {
            return None;
        }
        let op = u16_at(6);
        if op != REQUEST && op != REPLY {
            return None;
        }
        Some(Packet {
            op,
            sender_mac: p[8..14].try_into().expect("6 bytes"),
            sender_ip: u32_at(14),
            target_mac: p[18..24].try_into().expect("6 bytes"),
            target_ip: u32_at(24),
        })
    }

    /// The packet in a frame from `src` to `dst`.
    pub fn frame(&self, dst: [u8; 6], src: [u8; 6]) -> [u8; FRAME] {
        let mut f = [0u8; FRAME];
        f[0..6].copy_from_slice(&dst);
        f[6..12].copy_from_slice(&src);
        f[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
        let p = &mut f[ETHERNET_HEADER..];
        p[0..2].copy_from_slice(&1u16.to_be_bytes());
        p[2..4].copy_from_slice(&crate::ETHERTYPE_IPV4.to_be_bytes());
        p[4] = 6;
        p[5] = 4;
        p[6..8].copy_from_slice(&self.op.to_be_bytes());
        p[8..14].copy_from_slice(&self.sender_mac);
        p[14..18].copy_from_slice(&self.sender_ip.to_be_bytes());
        p[18..24].copy_from_slice(&self.target_mac);
        p[24..28].copy_from_slice(&self.target_ip.to_be_bytes());
        f
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Incomplete { sent: u32, next: u64 },
    Reachable { until: u64 },
    Probe { sent: u32, next: u64 },
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    ip: u32,
    mac: [u8; 6],
    state: State,
    /// When a packet last went to it (or it was made).
    used: u64,
}

/// A request to send: by broadcast, or by unicast to the address the
/// entry had (a probe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub target: u32,
    pub unicast: Option<[u8; 6]>,
}

/// One link's neighbours.
pub struct Cache {
    entries: Vec<Entry>,
    /// Frames (Ethernet header and IP packet) waiting for their next hop's
    /// address, oldest first.
    pending: VecDeque<(u32, Vec<u8>)>,
}

impl Default for Cache {
    fn default() -> Cache {
        Cache::new()
    }
}

impl Cache {
    pub fn new() -> Cache {
        Cache { entries: Vec::with_capacity(ENTRIES), pending: VecDeque::with_capacity(PENDING) }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Frames waiting for resolution.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    fn find(&mut self, ip: u32) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.ip == ip)
    }

    /// The hardware address of `ip` to send to now, if one is known (a
    /// stale one too: its use starts a probe).
    pub fn lookup(&mut self, ip: u32, now: u64) -> Option<[u8; 6]> {
        let e = self.find(ip)?;
        match e.state {
            State::Incomplete { .. } => return None,
            State::Reachable { until } if now >= until => e.state = State::Probe { sent: 0, next: now },
            _ => {}
        }
        e.used = now;
        Some(e.mac)
    }

    /// Room for a new entry: the least recently used one that is not being
    /// resolved goes if the cache is full. False if all are being resolved.
    fn make_room(&mut self) -> bool {
        if self.entries.len() < ENTRIES {
            return true;
        }
        let victim = self.entries.iter().enumerate().filter(|(_, e)| !matches!(e.state, State::Incomplete { .. })).min_by_key(|(_, e)| e.used).map(|(i, _)| i);
        match victim {
            Some(i) => {
                self.entries.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// Keeps `frame` until `ip` is resolved (asking for it if nobody asks
    /// yet: `poll` sends the request): false if it was dropped (the cache
    /// is full of addresses being resolved). The oldest waiting frame makes
    /// room if `PENDING` wait.
    pub fn queue(&mut self, ip: u32, frame: &[u8], now: u64) -> bool {
        match self.find(ip) {
            Some(e) => {
                debug_assert!(matches!(e.state, State::Incomplete { .. }), "lookup first");
                e.used = now;
            }
            None => {
                if !self.make_room() {
                    return false;
                }
                self.entries.push(Entry { ip, mac: [0; 6], state: State::Incomplete { sent: 0, next: now }, used: now });
            }
        }
        if self.pending.len() == PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back((ip, frame.to_vec()));
        true
    }

    /// What an ARP packet said: `ip` is at `mac`. An entry is made only if
    /// the packet was meant for us (`for_us`), else only an existing one
    /// is updated. The frames waiting for `ip` go to `release`, with their
    /// destination filled in. True if the cache knows `ip` now.
    pub fn learn(&mut self, ip: u32, mac: [u8; 6], for_us: bool, now: u64, mut release: impl FnMut(&[u8])) -> bool {
        if !is_unicast(ip) || is_group_mac(&mac) || mac == [0; 6] {
            return false;
        }
        let state = State::Reachable { until: now + REACHABLE_MS };
        if let Some(e) = self.find(ip) {
            e.mac = mac;
            e.state = state;
        } else if for_us && self.make_room() {
            self.entries.push(Entry { ip, mac, state, used: now });
        } else {
            return false;
        }
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].0 == ip {
                let (_, mut frame) = self.pending.remove(i).expect("in range");
                frame[0..6].copy_from_slice(&mac);
                release(&frame);
            } else {
                i += 1;
            }
        }
        true
    }

    /// The timers: requests due go to `request`; entries whose requests
    /// went unanswered go with their waiting frames, stale entries unused
    /// for `GC_MS` go.
    pub fn poll(&mut self, now: u64, mut request: impl FnMut(Request)) {
        let mut i = 0;
        while i < self.entries.len() {
            let e = &mut self.entries[i];
            let keep = match e.state {
                State::Incomplete { sent, next } | State::Probe { sent, next } if now >= next => {
                    let probe = matches!(e.state, State::Probe { .. });
                    if sent >= REQUESTS {
                        false
                    } else {
                        let next = now + RETRANSMIT_MS;
                        e.state = if probe { State::Probe { sent: sent + 1, next } } else { State::Incomplete { sent: sent + 1, next } };
                        request(Request { target: e.ip, unicast: probe.then_some(e.mac) });
                        true
                    }
                }
                State::Reachable { until } => now < until || now.saturating_sub(e.used) < GC_MS,
                _ => true,
            };
            if keep {
                i += 1;
            } else {
                let ip = self.entries.swap_remove(i).ip;
                self.pending.retain(|(to, _)| *to != ip);
            }
        }
    }

    /// When `poll` has a request to send (or an entry to give up).
    pub fn deadline(&self) -> Option<u64> {
        self.entries
            .iter()
            .filter_map(|e| match e.state {
                State::Incomplete { next, .. } | State::Probe { next, .. } => Some(next),
                State::Reachable { .. } => None,
            })
            .min()
    }

    /// Forgets everything (the link lost its carrier or its address).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const A: u32 = 0x0a00_0202;
    const B: u32 = 0x0a00_0203;
    const MAC_A: [u8; 6] = [2, 0, 0, 0, 0, 0xa];
    const MAC_B: [u8; 6] = [2, 0, 0, 0, 0, 0xb];

    fn requests(c: &mut Cache, now: u64) -> Vec<Request> {
        let mut out = Vec::new();
        c.poll(now, |r| out.push(r));
        out
    }

    #[test]
    fn packets_round_trip_and_malformed_ones_are_refused() {
        let p = Packet { op: REQUEST, sender_mac: MAC_A, sender_ip: A, target_mac: [0; 6], target_ip: B };
        let f = p.frame(crate::BROADCAST_MAC, MAC_A);
        assert_eq!(&f[12..14], &[0x08, 0x06]);
        assert_eq!(Packet::parse(&f[ETHERNET_HEADER..]), Some(p));
        // Padding after the packet (a frame of Ethernet's minimum size).
        let mut padded = f.to_vec();
        padded.resize(60, 0);
        assert_eq!(Packet::parse(&padded[ETHERNET_HEADER..]), Some(p));
        assert_eq!(Packet::parse(&f[ETHERNET_HEADER..FRAME - 1]), None, "short");
        for (at, value) in [(0, 6u8), (3, 0xdd), (4, 8), (5, 16), (7, 3)] {
            let mut bad = f;
            bad[ETHERNET_HEADER + at] = value;
            assert_eq!(Packet::parse(&bad[ETHERNET_HEADER..]), None, "byte {at}");
        }
    }

    #[test]
    fn resolution_queues_asks_and_releases() {
        let mut c = Cache::new();
        assert_eq!(c.lookup(A, 0), None);
        assert!(c.queue(A, &[0u8; 20], 0));
        assert!(c.queue(A, &[1u8; 20], 0));
        assert_eq!(c.deadline(), Some(0));
        assert_eq!(requests(&mut c, 0), vec![Request { target: A, unicast: None }]);
        assert_eq!(c.deadline(), Some(RETRANSMIT_MS));
        assert_eq!(requests(&mut c, 10), vec![], "not again before the retransmission");
        assert_eq!(c.lookup(A, 10), None, "still incomplete");
        let mut out = Vec::new();
        assert!(c.learn(A, MAC_A, true, 20, |f| out.push(f.to_vec())));
        assert_eq!(out.len(), 2, "both waiting frames go, in order");
        assert_eq!(&out[0][0..6], &MAC_A);
        assert_eq!(out[0][6], 0);
        assert_eq!(out[1][6], 1);
        assert_eq!(c.pending(), 0);
        assert_eq!(c.lookup(A, 30), Some(MAC_A));
        assert_eq!(c.deadline(), None);
    }

    #[test]
    fn unanswered_requests_give_up_and_drop_the_frames() {
        let mut c = Cache::new();
        c.queue(A, &[0u8; 20], 0);
        c.queue(B, &[0u8; 20], 500);
        let mut sent = 0;
        let mut now = 0;
        while now <= 10 * RETRANSMIT_MS {
            sent += requests(&mut c, now).iter().filter(|r| r.target == A).count();
            now += 100;
        }
        assert_eq!(sent, REQUESTS as usize);
        assert!(c.is_empty() && c.pending() == 0, "gone, with what waited");
        // The next packet asks again.
        c.queue(A, &[0u8; 20], now);
        assert_eq!(requests(&mut c, now), vec![Request { target: A, unicast: None }]);
    }

    #[test]
    fn the_queue_is_bounded_oldest_first() {
        let mut c = Cache::new();
        for i in 0..PENDING as u8 + 3 {
            assert!(c.queue(A, &[i; 20], 0));
        }
        assert_eq!(c.pending(), PENDING);
        let mut first = None;
        c.learn(A, MAC_A, true, 1, |f| {
            first.get_or_insert(f[6]);
        });
        assert_eq!(first, Some(3), "the three oldest were dropped");
    }

    #[test]
    fn aging_probes_by_unicast_then_forgets() {
        let mut c = Cache::new();
        c.learn(A, MAC_A, true, 0, |_| {});
        // Reachable: used without a question.
        assert_eq!(c.lookup(A, REACHABLE_MS - 1), Some(MAC_A));
        assert_eq!(requests(&mut c, REACHABLE_MS - 1), vec![]);
        // Stale and used: still used, and probed by unicast.
        let t = REACHABLE_MS + 5;
        assert_eq!(c.lookup(A, t), Some(MAC_A));
        assert_eq!(requests(&mut c, t), vec![Request { target: A, unicast: Some(MAC_A) }]);
        // A reply confirms it.
        c.learn(A, MAC_A, true, t + 10, |_| {});
        assert_eq!(c.deadline(), None);
        assert_eq!(requests(&mut c, t + 2 * RETRANSMIT_MS), vec![]);
        // Stale again, used, and never answers: it goes.
        let t = t + 10 + REACHABLE_MS;
        assert_eq!(c.lookup(A, t), Some(MAC_A));
        let mut probes = 0;
        for k in 0..=REQUESTS as u64 + 1 {
            probes += requests(&mut c, t + k * RETRANSMIT_MS).len();
        }
        assert_eq!(probes, REQUESTS as usize);
        assert_eq!(c.lookup(A, t + 10 * RETRANSMIT_MS), None);
        assert!(c.is_empty());
    }

    #[test]
    fn stale_unused_entries_are_collected() {
        let mut c = Cache::new();
        c.learn(A, MAC_A, true, 0, |_| {});
        c.learn(B, MAC_B, true, 0, |_| {});
        c.lookup(B, REACHABLE_MS + GC_MS - 10);
        requests(&mut c, REACHABLE_MS + GC_MS);
        assert_eq!(c.len(), 1, "A was unused for GC_MS after it went stale; B in use");
        assert_eq!(c.lookup(B, REACHABLE_MS + GC_MS), Some(MAC_B));
    }

    #[test]
    fn learning_follows_the_merge_rule() {
        let mut c = Cache::new();
        // Not for us, not known: nothing is made.
        assert!(!c.learn(A, MAC_A, false, 0, |_| {}));
        assert!(c.is_empty());
        // For us: made. Then a gratuitous update moves it.
        assert!(c.learn(A, MAC_A, true, 0, |_| {}));
        assert!(c.learn(A, MAC_B, false, 1, |_| {}));
        assert_eq!(c.lookup(A, 2), Some(MAC_B));
        // Nonsense is never learnt.
        assert!(!c.learn(0, MAC_A, true, 0, |_| {}), "a probe's sender");
        assert!(!c.learn(0xffff_ffff, MAC_A, true, 0, |_| {}));
        assert!(!c.learn(0x7f00_0001, MAC_A, true, 0, |_| {}));
        assert!(!c.learn(B, [0xff; 6], true, 0, |_| {}));
        assert!(!c.learn(B, [1, 0, 0x5e, 0, 0, 1], true, 0, |_| {}));
        assert!(!c.learn(B, [0; 6], true, 0, |_| {}));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn a_full_cache_evicts_the_least_recently_used_resolved_entry() {
        let mut c = Cache::new();
        for i in 0..ENTRIES as u32 {
            c.learn(0x0a00_0100 + i, MAC_A, true, i as u64, |_| {});
        }
        c.lookup(0x0a00_0100, 1000);
        assert!(c.learn(B, MAC_B, true, 1001, |_| {}));
        assert_eq!(c.len(), ENTRIES);
        assert_eq!(c.lookup(0x0a00_0101, 1002), None, "the least recently used went");
        assert!(c.lookup(0x0a00_0100, 1002).is_some(), "the one used lately stayed");
        // A cache full of addresses being resolved takes no more.
        let mut c = Cache::new();
        for i in 0..ENTRIES as u32 {
            assert!(c.queue(0x0a00_0100 + i, &[0; 20], 0));
        }
        assert!(!c.queue(B, &[0; 20], 0));
        assert!(!c.learn(B, MAC_B, true, 0, |_| {}));
    }

    #[test]
    fn clear_forgets_all() {
        let mut c = Cache::new();
        c.learn(A, MAC_A, true, 0, |_| {});
        c.queue(B, &[0; 20], 0);
        c.clear();
        assert!(c.is_empty() && c.pending() == 0 && c.deadline().is_none());
    }
}
