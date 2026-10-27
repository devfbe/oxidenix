//! One Ethernet link below netd's IP interface: its frames in and out,
//! its ARP cache (`arp`) and its DHCP client (`dhcp`).
//!
//! **In** (`input`), a frame is taken only from a link with a carrier, to
//! the link's hardware address or the broadcast one, from a unicast
//! hardware address not the link's own. ARP packets go to the cache (and a
//! request for the link's address is answered); an IPv4 packet with a
//! valid header that is no martian (from or to the loopback's addresses,
//! from an address that no host has or that is this host's own) either is
//! a DHCP reply (UDP to port 68, unfragmented: the client takes it, nobody
//! else sees it) or goes up if it is for this host: to one of its
//! addresses (any link's: the weak host model, as Linux), to the limited
//! broadcast or this link's subnet broadcast. Everything else is dropped
//! (`Drop` says why).
//!
//! **Out** (`output`), an IP packet from smoltcp in a frame whose header
//! is left to fill, with the next hop routing chose: the broadcast address
//! for a broadcast, the group's for a multicast, else the next hop's from
//! the ARP cache; one being resolved keeps the frame waiting (the request
//! goes at once). A frame larger than the link's MTU, one for a link
//! without a carrier and one the wire has no room for are dropped.
//!
//! **Lease changes** come out as `dhcp::Event`s (`take_event`), for the
//! caller's messages; what the link has is always `config`. A lost lease
//! or a new address empties the ARP cache.

use crate::arp::{self, Cache};
use crate::dhcp::{self, Client, Event};
use crate::{is_group_mac, is_loopback, is_multicast, is_unicast, multicast_mac, Config, Rng, Wire, BROADCAST, BROADCAST_MAC, ETHERNET_HEADER, ETHERTYPE_ARP, ETHERTYPE_IPV4};
use alloc::collections::VecDeque;
use core::ops::Range;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{IpAddress, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr};

/// Lease changes kept for the caller (older ones go first: they are for
/// messages only).
pub const MAX_EVENTS: usize = 8;
const IPV4_HEADER: usize = 20;
const UDP_HEADER: usize = 8;
/// A DHCP message's frame: the headers and the payload.
const DHCP_FRAME: usize = ETHERNET_HEADER + IPV4_HEADER + UDP_HEADER + dhcp::MAX_PAYLOAD;
/// The time to live of the client's messages (Linux's default).
const TTL: u8 = 64;

/// Why a frame was not taken or not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    NoCarrier,
    /// Too short, a bad header or checksum.
    Malformed,
    /// To another host.
    NotForUs,
    Martian,
    /// Neither ARP nor IPv4.
    Protocol,
    /// Larger than the link's MTU.
    TooBig,
    /// The next hop cannot be resolved (no address to ask from, or the
    /// cache is full of addresses being resolved).
    Unresolved,
    /// The wire had no room.
    NoRoom,
}

/// What became of a frame from the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// An IP packet for smoltcp: these bytes of the frame.
    Packet(Range<usize>),
    /// The link took it (ARP, DHCP).
    Taken,
    Dropped(Drop),
}

/// What became of a packet to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    Sent,
    /// Waiting for its next hop's hardware address.
    Queued,
    Dropped(Drop),
}

pub struct EthernetLink {
    mac: [u8; 6],
    /// The largest IP packet it carries.
    mtu: usize,
    carrier: bool,
    arp: Cache,
    /// The address the ARP cache's entries were learnt with (0: none).
    arp_address: u32,
    dhcp: Client,
    events: VecDeque<Event>,
}

/// Sends a frame (its source and type written) to `next_hop`, resolving it
/// first: `frame` is the Ethernet header's room and the IP packet.
fn deliver(arp: &mut Cache, mac: [u8; 6], config: Option<Config>, frame: &mut [u8], next_hop: u32, now: u64, wire: &mut impl Wire) -> Output {
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let dst = if next_hop == BROADCAST || config.is_some_and(|c| c.broadcast() == Some(next_hop)) {
        BROADCAST_MAC
    } else if is_multicast(next_hop) {
        multicast_mac(next_hop)
    } else if !is_unicast(next_hop) {
        // (Routing never chooses one: nobody to ask for.)
        return Output::Dropped(Drop::Unresolved);
    } else {
        match arp.lookup(next_hop, now) {
            Some(dst) => dst,
            // Without an address there is nobody to ask for.
            None if config.is_none() => return Output::Dropped(Drop::Unresolved),
            None if arp.queue(next_hop, frame, now) => {
                ask(arp, mac, config, now, wire);
                return Output::Queued;
            }
            None => return Output::Dropped(Drop::Unresolved),
        }
    };
    frame[0..6].copy_from_slice(&dst);
    if wire.send(frame) { Output::Sent } else { Output::Dropped(Drop::NoRoom) }
}

/// Sends the ARP cache's requests that are due (and lets it give up on
/// the unanswered ones).
fn ask(arp: &mut Cache, mac: [u8; 6], config: Option<Config>, now: u64, wire: &mut impl Wire) {
    let sender_ip = config.map_or(0, |c| c.address);
    arp.poll(now, |r| {
        let p = arp::Packet { op: arp::REQUEST, sender_mac: mac, sender_ip, target_mac: [0; 6], target_ip: r.target };
        wire.send(&p.frame(r.unicast.unwrap_or(BROADCAST_MAC), mac));
    });
}

/// Writes a UDP datagram from `from` to `to` (ports `ports`) with its IP
/// header into `frame` after the Ethernet header's room: the frame's
/// length.
pub fn udp_frame(frame: &mut [u8], from: u32, to: u32, ports: (u16, u16), payload: &[u8]) -> usize {
    let caps = ChecksumCapabilities::default();
    let (src, dst) = (Ipv4Address::from_bits(from), Ipv4Address::from_bits(to));
    let len = ETHERNET_HEADER + IPV4_HEADER + UDP_HEADER + payload.len();
    let ip = Ipv4Repr { src_addr: src, dst_addr: dst, next_header: IpProtocol::Udp, payload_len: UDP_HEADER + payload.len(), hop_limit: TTL };
    ip.emit(&mut Ipv4Packet::new_unchecked(&mut frame[ETHERNET_HEADER..ETHERNET_HEADER + IPV4_HEADER]), &caps);
    let udp = UdpRepr { src_port: ports.0, dst_port: ports.1 };
    let mut packet = UdpPacket::new_unchecked(&mut frame[ETHERNET_HEADER + IPV4_HEADER..len]);
    udp.emit(&mut packet, &IpAddress::Ipv4(src), &IpAddress::Ipv4(dst), payload.len(), |b| b.copy_from_slice(payload), &caps);
    len
}

impl EthernetLink {
    /// A link with hardware address `mac` that carries IP packets of up to
    /// `mtu` bytes, without a carrier yet.
    pub fn new(mac: [u8; 6], mtu: usize) -> EthernetLink {
        EthernetLink { mac, mtu, carrier: false, arp: Cache::new(), arp_address: 0, dhcp: Client::new(mac), events: VecDeque::with_capacity(MAX_EVENTS) }
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn carrier(&self) -> bool {
        self.carrier
    }

    /// Its lease (none without a carrier).
    pub fn config(&self) -> Option<Config> {
        self.dhcp.config()
    }

    pub fn lease(&self) -> Option<&dhcp::Lease> {
        self.dhcp.lease()
    }

    pub fn arp(&self) -> &Cache {
        &self.arp
    }

    /// The next lease change, oldest first.
    pub fn take_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// After DHCP had its say: the ARP cache is for the lease's address
    /// only, and the event is kept.
    fn note(&mut self, event: Option<Event>) {
        let address = self.config().map_or(0, |c| c.address);
        if address != self.arp_address {
            self.arp.clear();
            self.arp_address = address;
        }
        let Some(event) = event else { return };
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// The medium came or went: DHCP starts at once, or the lease goes
    /// (`Loss::Stopped`) with what the ARP cache knew.
    pub fn set_carrier(&mut self, up: bool, now: u64) {
        if up == self.carrier {
            return;
        }
        self.carrier = up;
        if up {
            self.dhcp.start(now);
        } else {
            let e = self.dhcp.stop();
            self.note(e);
        }
    }

    /// The lease goes now (`Loss::Restart`) and DHCP asks again at `at`.
    pub fn renew(&mut self, at: u64) {
        let e = self.dhcp.restart(at);
        self.note(e);
    }

    /// When `poll` has something to do.
    pub fn deadline(&self) -> Option<u64> {
        if !self.carrier {
            return None;
        }
        self.arp.deadline().into_iter().chain(self.dhcp.deadline()).min()
    }

    /// The timers: ARP's requests, DHCP's messages and lease.
    pub fn poll(&mut self, now: u64, rng: &mut impl Rng, wire: &mut impl Wire) {
        if !self.carrier {
            return;
        }
        let config = self.config();
        ask(&mut self.arp, self.mac, config, now, wire);
        let mut payload = [0u8; dhcp::MAX_PAYLOAD];
        let mut frame = [0u8; DHCP_FRAME];
        let (arp, mac) = (&mut self.arp, self.mac);
        let event = self.dhcp.poll(now, rng, &mut payload, |m, payload| {
            let to = dhcp::destination(m);
            let len = udp_frame(&mut frame, m.from, to, (dhcp::CLIENT_PORT, dhcp::SERVER_PORT), payload);
            // A renewal goes to the server, or to the router if the server
            // is beyond it.
            let next_hop = match (m.to, config) {
                (Some(server), Some(c)) if !c.contains(server) && c.gateway != 0 => c.gateway,
                _ => to,
            };
            deliver(arp, mac, config, &mut frame[..len], next_hop, now, wire);
        });
        self.note(event);
    }

    /// Takes a frame from the wire (see the module's comment). `is_local`
    /// says whether an address is one of this host's.
    pub fn input(&mut self, frame: &[u8], now: u64, rng: &mut impl Rng, is_local: impl Fn(u32) -> bool, wire: &mut impl Wire) -> Input {
        if !self.carrier {
            return Input::Dropped(Drop::NoCarrier);
        }
        if frame.len() < ETHERNET_HEADER {
            return Input::Dropped(Drop::Malformed);
        }
        let dst: [u8; 6] = frame[0..6].try_into().expect("6 bytes");
        let src: [u8; 6] = frame[6..12].try_into().expect("6 bytes");
        if dst != self.mac && dst != BROADCAST_MAC {
            return Input::Dropped(Drop::NotForUs);
        }
        if src == self.mac || is_group_mac(&src) {
            return Input::Dropped(Drop::Martian);
        }
        match u16::from_be_bytes([frame[12], frame[13]]) {
            ETHERTYPE_ARP => self.arp_input(&frame[ETHERNET_HEADER..], now, wire),
            ETHERTYPE_IPV4 => self.ipv4_input(frame, now, rng, is_local),
            _ => Input::Dropped(Drop::Protocol),
        }
    }

    fn arp_input(&mut self, payload: &[u8], now: u64, wire: &mut impl Wire) -> Input {
        let Some(p) = arp::Packet::parse(payload) else { return Input::Dropped(Drop::Malformed) };
        if is_loopback(p.sender_ip) || is_loopback(p.target_ip) {
            return Input::Dropped(Drop::Martian);
        }
        let address = self.config().map(|c| c.address);
        if address.is_some_and(|a| a == p.sender_ip) {
            // Someone else claims our address: nothing to learn from it.
            return Input::Dropped(Drop::Martian);
        }
        let for_us = address.is_some_and(|a| a == p.target_ip);
        self.arp.learn(p.sender_ip, p.sender_mac, for_us, now, |f| {
            wire.send(f);
        });
        // (A probe's, from 0.0.0.0, is answered too, as Linux does: the
        // prober learns that the address is taken.)
        if p.op == arp::REQUEST && for_us && (p.sender_ip == 0 || is_unicast(p.sender_ip)) && !is_group_mac(&p.sender_mac) {
            let reply = arp::Packet { op: arp::REPLY, sender_mac: self.mac, sender_ip: p.target_ip, target_mac: p.sender_mac, target_ip: p.sender_ip };
            wire.send(&reply.frame(p.sender_mac, self.mac));
        }
        Input::Taken
    }

    fn ipv4_input(&mut self, frame: &[u8], now: u64, rng: &mut impl Rng, is_local: impl Fn(u32) -> bool) -> Input {
        let ip = &frame[ETHERNET_HEADER..];
        let Ok(packet) = Ipv4Packet::new_checked(ip) else { return Input::Dropped(Drop::Malformed) };
        if packet.version() != 4 || !packet.verify_checksum() {
            return Input::Dropped(Drop::Malformed);
        }
        let (src, dst) = (packet.src_addr().to_bits(), packet.dst_addr().to_bits());
        if is_loopback(src) || is_loopback(dst) || !is_unicast(src) || is_local(src) {
            return Input::Dropped(Drop::Martian);
        }
        let (header, total) = (packet.header_len() as usize, packet.total_len() as usize);
        if packet.next_header() == IpProtocol::Udp && !packet.more_frags() && packet.frag_offset() == 0 {
            if let Ok(udp) = UdpPacket::new_checked(&ip[header..total]) {
                if udp.dst_port() == dhcp::CLIENT_PORT {
                    // The client's port: DHCP's replies, nobody else's.
                    let sound = udp.checksum() == 0 || udp.verify_checksum(&IpAddress::Ipv4(packet.src_addr()), &IpAddress::Ipv4(packet.dst_addr()));
                    if udp.src_port() == dhcp::SERVER_PORT && sound {
                        let e = self.dhcp.input(now, rng, udp.payload());
                        self.note(e);
                    }
                    return Input::Taken;
                }
            }
        }
        let config = self.config();
        if !(is_local(dst) || dst == BROADCAST || config.is_some_and(|c| c.broadcast() == Some(dst))) {
            return Input::Dropped(Drop::NotForUs);
        }
        Input::Packet(ETHERNET_HEADER..ETHERNET_HEADER + total)
    }

    /// Sends an IP packet to `next_hop` (see the module's comment):
    /// `frame` is `ETHERNET_HEADER` bytes of room for the header, then the
    /// packet.
    pub fn output(&mut self, frame: &mut [u8], next_hop: u32, now: u64, wire: &mut impl Wire) -> Output {
        if !self.carrier {
            return Output::Dropped(Drop::NoCarrier);
        }
        if frame.len() < ETHERNET_HEADER + IPV4_HEADER {
            return Output::Dropped(Drop::Malformed);
        }
        if frame.len() - ETHERNET_HEADER > self.mtu {
            return Output::Dropped(Drop::TooBig);
        }
        let config = self.config();
        deliver(&mut self.arp, self.mac, config, frame, next_hop, now, wire)
    }
}
