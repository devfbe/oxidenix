//! netd's link layer: what netd does itself for each Ethernet link below
//! its one smoltcp interface (an IP medium), kept apart from netd so that
//! it is tested on the host. smoltcp cannot route between several
//! interfaces that share one socket set, so netd gives it one interface of
//! `Medium::Ip` over all its links and does the rest here:
//!
//! - `route`: which link an IP packet from smoltcp leaves by and to which
//!   next hop (the loopback for the host's own addresses; a link's subnet;
//!   the link of the source address; the default link by metric).
//! - `link`: one Ethernet link: its frames in (ARP, DHCP's replies, IP
//!   packets for this host, martians dropped) and out (the next hop's
//!   hardware address, broadcast and multicast ones), its ARP cache and
//!   its DHCP client.
//! - `arp`: the ARP cache (RFC 826): requests, replies for the link's own
//!   address, aging, a bounded queue of packets waiting for resolution.
//! - `dhcp`: the DHCP client (RFC 2131): the lease, its renewal and
//!   rebinding, retransmission with backoff and jitter.
//!
//! Time is netd's uptime in milliseconds; randomness (DHCP's transaction
//! ids, jitter) comes from the caller (`Rng`: netd's ChaCha). Addresses are
//! IPv4 addresses in host order (`u32`), as in `netring`.

#![no_std]

extern crate alloc;

pub mod arp;
pub mod dhcp;
pub mod link;
pub mod route;

/// The bytes of an Ethernet header (no VLAN tag).
pub const ETHERNET_HEADER: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const BROADCAST_MAC: [u8; 6] = [0xff; 6];
/// The limited broadcast address (255.255.255.255).
pub const BROADCAST: u32 = u32::MAX;
/// The largest IP packet netd sends on any link (Ethernet's).
pub const MAX_MTU: usize = 1500;

/// Random numbers for the protocols (netd's generator).
pub trait Rng {
    fn next_u32(&mut self) -> u32;
}

/// Where a link's frames go: its driver's send ring, the card's queue.
pub trait Wire {
    /// Sends one Ethernet frame (header and payload, no FCS): false if
    /// there was no room (the frame is dropped, as a busy wire would).
    fn send(&mut self, frame: &[u8]) -> bool;
}

/// The network mask of a prefix length.
pub const fn netmask(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        32.. => u32::MAX,
        p => u32::MAX << (32 - p as u32),
    }
}

pub const fn is_loopback(a: u32) -> bool {
    a >> 24 == 127
}

pub const fn is_multicast(a: u32) -> bool {
    a >> 28 == 0xe
}

/// An address a host may have and a unicast packet may come from or go
/// to: not 0.0.0.0/8 ("this network"), not the loopback's, not multicast
/// or reserved (class E) or the limited broadcast.
pub const fn is_unicast(a: u32) -> bool {
    a >> 24 != 0 && !is_loopback(a) && a >> 28 < 0xe
}

/// A multicast or broadcast hardware address (the group bit).
pub const fn is_group_mac(mac: &[u8; 6]) -> bool {
    mac[0] & 1 != 0
}

/// The Ethernet multicast address of an IPv4 multicast group (RFC 1112:
/// 01:00:5e and the group's low 23 bits).
pub fn multicast_mac(group: u32) -> [u8; 6] {
    let b = group.to_be_bytes();
    [0x01, 0x00, 0x5e, b[1] & 0x7f, b[2], b[3]]
}

/// A link's configuration: its DHCP lease's, as netd uses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub address: u32,
    pub prefix: u8,
    /// The router (0: none).
    pub gateway: u32,
    /// The first DNS server (0: none).
    pub dns: u32,
}

impl Config {
    /// Whether `ip` is in the link's subnet.
    pub fn contains(&self, ip: u32) -> bool {
        let mask = netmask(self.prefix);
        ip & mask == self.address & mask
    }

    /// The subnet's directed broadcast address (none for a /31 or /32).
    pub fn broadcast(&self) -> Option<u32> {
        (self.prefix < 31).then(|| self.address | !netmask(self.prefix))
    }

    /// Whether `ip` is a broadcast address on this link: the limited one
    /// or the subnet's.
    pub fn is_broadcast(&self, ip: u32) -> bool {
        ip == BROADCAST || self.broadcast() == Some(ip)
    }
}

/// An address as text (`a.b.c.d`), for messages.
pub struct Dotted(pub u32);

impl core::fmt::Display for Dotted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}

/// A hardware address as text (`aa:bb:cc:dd:ee:ff`), for messages.
pub struct Mac(pub [u8; 6]);

impl core::fmt::Display for Mac {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(netmask(0), 0);
        assert_eq!(netmask(24), 0xffff_ff00);
        assert_eq!(netmask(32), u32::MAX);
        assert!(is_unicast(0x0a00_020f) && !is_unicast(0) && !is_unicast(0x7f00_0001) && !is_unicast(0xe000_0001) && !is_unicast(BROADCAST));
        assert!(!is_unicast(0x00ff_0001), "0.0.0.0/8 is no host's");
        assert_eq!(multicast_mac(0xe0ff_0102), [0x01, 0, 0x5e, 0x7f, 0x01, 0x02]);
        let c = Config { address: 0x0a00_020f, prefix: 24, gateway: 0x0a00_0202, dns: 0 };
        assert!(c.contains(0x0a00_02ff) && !c.contains(0x0a00_0301));
        assert_eq!(c.broadcast(), Some(0x0a00_02ff));
        assert!(c.is_broadcast(BROADCAST) && c.is_broadcast(0x0a00_02ff) && !c.is_broadcast(0x0a00_0202));
        assert_eq!(Config { prefix: 31, ..c }.broadcast(), None);
        assert_eq!(alloc::format!("{} {}", Dotted(0x0a00_020f), Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56])), "10.0.2.15 52:54:00:12:34:56");
    }
}
