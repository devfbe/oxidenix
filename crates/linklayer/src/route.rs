//! Where an IP packet from smoltcp goes: netd's routing decision over its
//! links (one smoltcp interface above them all, `crate`'s comment).
//!
//! In this order:
//! 1. To an address of this host (a link's leased address, 127.0.0.0/8):
//!    the loopback.
//! 2. From the loopback's addresses anywhere else: nowhere (a martian).
//! 3. To the limited broadcast or a multicast group: the link of the
//!    source address, else the default link; the frame goes to the
//!    group's hardware address.
//! 4. To an address in a link's subnet (or the subnet's broadcast
//!    address): that link, the destination itself the next hop. Where
//!    subnets overlap, the source address's link, else the longest prefix,
//!    else the lowest metric.
//! 5. From the address of a link with a router: that link via its router
//!    (source-based policy routing: a socket bound to a link's address
//!    leaves by it).
//! 6. The default link via its router: of the links with a router the one
//!    of the lowest metric (the PCI card 100, USB links 200: as
//!    NetworkManager orders them), the first of equals.
//! 7. Else nowhere.
//!
//! Only links with a carrier and a lease take part.

use crate::{is_loopback, is_multicast, Config, BROADCAST};

/// A link as routing sees it, in netd's order of links (by index).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Candidate {
    /// Its lease, while it has a carrier and one.
    pub config: Option<Config>,
    pub metric: u32,
}

pub const METRIC_PCI: u32 = 100;
pub const METRIC_USB: u32 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Back up: the packet is for this host.
    Loopback,
    /// Out by link number `link` (a position in the candidates) to
    /// `next_hop` (the destination or a router).
    Link { link: usize, next_hop: u32 },
    /// No route: the packet is dropped.
    Unreachable,
}

/// Whether `ip` is an address of this host.
pub fn is_local(links: &[Candidate], ip: u32) -> bool {
    is_loopback(ip) || (ip != 0 && links.iter().any(|l| l.config.is_some_and(|c| c.address == ip)))
}

/// The link whose router is the default route's.
pub fn default_link(links: &[Candidate]) -> Option<usize> {
    links.iter().enumerate().filter(|(_, l)| l.config.is_some_and(|c| c.gateway != 0)).min_by_key(|(i, l)| (l.metric, *i)).map(|(i, _)| i)
}

/// Where a packet from `src` to `dst` goes.
pub fn route(links: &[Candidate], src: u32, dst: u32) -> Route {
    if is_local(links, dst) {
        return Route::Loopback;
    }
    if is_loopback(src) {
        return Route::Unreachable;
    }
    let source = links.iter().position(|l| src != 0 && l.config.is_some_and(|c| c.address == src));
    if dst == BROADCAST || is_multicast(dst) {
        return match source.or_else(|| default_link(links)) {
            Some(link) => Route::Link { link, next_hop: dst },
            None => Route::Unreachable,
        };
    }
    let connected = links
        .iter()
        .enumerate()
        .filter_map(|(i, l)| Some((i, l.metric, l.config.filter(|c| c.contains(dst) || c.broadcast() == Some(dst))?)))
        .min_by_key(|&(i, metric, c)| (Some(i) != source, u8::MAX - c.prefix, metric, i));
    if let Some((link, _, _)) = connected {
        return Route::Link { link, next_hop: dst };
    }
    if let Some(link) = source {
        let gateway = links[link].config.map_or(0, |c| c.gateway);
        if gateway != 0 {
            return Route::Link { link, next_hop: gateway };
        }
    }
    match default_link(links) {
        Some(link) => Route::Link { link, next_hop: links[link].config.expect("a default link has a lease").gateway },
        None => Route::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: Config = Config { address: 0x0a00_020f, prefix: 24, gateway: 0x0a00_0202, dns: 0 };
    const PHONE: Config = Config { address: 0xc0a8_2a05, prefix: 24, gateway: 0xc0a8_2a81, dns: 0 };
    const LAB: Config = Config { address: 0xac10_0005, prefix: 16, gateway: 0, dns: 0 };

    fn link(config: Option<Config>, metric: u32) -> Candidate {
        Candidate { config, metric }
    }

    #[test]
    fn own_addresses_loop_back() {
        let links = [link(Some(CARD), METRIC_PCI), link(Some(PHONE), METRIC_USB)];
        for dst in [0x7f00_0001, 0x7f12_3456, CARD.address, PHONE.address] {
            assert_eq!(route(&links, CARD.address, dst), Route::Loopback);
            assert_eq!(route(&links, 0x7f00_0001, dst), Route::Loopback);
        }
        assert!(is_local(&links, PHONE.address) && !is_local(&links, 0) && !is_local(&[], CARD.address));
        // Loopback sources stay home.
        assert_eq!(route(&links, 0x7f00_0001, 0x0101_0101), Route::Unreachable);
        assert_eq!(route(&links, 0x7f00_0001, 0x0a00_0203), Route::Unreachable);
    }

    #[test]
    fn the_subnet_then_the_source_then_the_default() {
        let links = [link(Some(CARD), METRIC_PCI), link(Some(PHONE), METRIC_USB), link(Some(LAB), METRIC_USB)];
        // In a subnet: that link, directly, whatever the source.
        assert_eq!(route(&links, CARD.address, 0xc0a8_2a81), Route::Link { link: 1, next_hop: 0xc0a8_2a81 });
        assert_eq!(route(&links, PHONE.address, 0x0a00_0203), Route::Link { link: 0, next_hop: 0x0a00_0203 });
        assert_eq!(route(&links, CARD.address, 0xac10_ff01), Route::Link { link: 2, next_hop: 0xac10_ff01 });
        // The subnet's broadcast address.
        assert_eq!(route(&links, CARD.address, 0xc0a8_2aff), Route::Link { link: 1, next_hop: 0xc0a8_2aff });
        // Elsewhere: the source's link via its router...
        assert_eq!(route(&links, PHONE.address, 0x0101_0101), Route::Link { link: 1, next_hop: PHONE.gateway });
        assert_eq!(route(&links, CARD.address, 0x0101_0101), Route::Link { link: 0, next_hop: CARD.gateway });
        // ... a source link without one, or no source of ours: the default.
        assert_eq!(route(&links, LAB.address, 0x0101_0101), Route::Link { link: 0, next_hop: CARD.gateway });
        assert_eq!(route(&links, 0x0909_0909, 0x0101_0101), Route::Link { link: 0, next_hop: CARD.gateway });
    }

    #[test]
    fn the_default_link_is_the_lowest_metric_with_a_router() {
        let card = link(Some(CARD), METRIC_PCI);
        let phone = link(Some(PHONE), METRIC_USB);
        let lab = link(Some(LAB), METRIC_USB);
        let down = link(None, METRIC_PCI);
        assert_eq!(default_link(&[card, phone]), Some(0));
        assert_eq!(default_link(&[phone, card]), Some(1), "the metric, not the order");
        assert_eq!(default_link(&[down, phone]), Some(1), "without a lease a link takes no part");
        assert_eq!(default_link(&[lab, phone]), Some(1), "without a router neither");
        assert_eq!(default_link(&[phone, phone]), Some(0), "the first of equals");
        assert_eq!(default_link(&[lab, down]), None);
        assert_eq!(route(&[down, phone], PHONE.address, 0x0101_0101), Route::Link { link: 1, next_hop: PHONE.gateway });
        assert_eq!(route(&[lab, down], LAB.address, 0x0101_0101), Route::Unreachable);
        assert_eq!(route(&[], 0, 0x0101_0101), Route::Unreachable);
        // A link without a lease has no subnet.
        assert_eq!(route(&[down], 0, 0x0a00_0203), Route::Unreachable);
    }

    #[test]
    fn broadcast_and_multicast_by_the_source_or_the_default() {
        let links = [link(Some(CARD), METRIC_PCI), link(Some(PHONE), METRIC_USB), link(Some(LAB), METRIC_USB)];
        assert_eq!(route(&links, PHONE.address, BROADCAST), Route::Link { link: 1, next_hop: BROADCAST });
        assert_eq!(route(&links, LAB.address, BROADCAST), Route::Link { link: 2, next_hop: BROADCAST });
        assert_eq!(route(&links, 0, BROADCAST), Route::Link { link: 0, next_hop: BROADCAST });
        assert_eq!(route(&links, PHONE.address, 0xe000_00fb), Route::Link { link: 1, next_hop: 0xe000_00fb });
        assert_eq!(route(&[link(Some(LAB), METRIC_USB)], 0, BROADCAST), Route::Unreachable, "no source, no default");
    }

    #[test]
    fn overlapping_subnets() {
        // Two phones that both lease from 192.168.42.0/24, and a wider net.
        let other = Config { address: 0xc0a8_2a07, ..PHONE };
        let wide = Config { address: 0xc0a8_0001, prefix: 16, gateway: 0, dns: 0 };
        let links = [link(Some(wide), METRIC_PCI), link(Some(PHONE), METRIC_USB), link(Some(other), METRIC_USB)];
        assert_eq!(route(&links, other.address, 0xc0a8_2a81), Route::Link { link: 2, next_hop: 0xc0a8_2a81 }, "the source's link");
        assert_eq!(route(&links, wide.address, 0xc0a8_2a81), Route::Link { link: 0, next_hop: 0xc0a8_2a81 }, "the source's link, though wider");
        assert_eq!(route(&links, 0, 0xc0a8_2a81), Route::Link { link: 1, next_hop: 0xc0a8_2a81 }, "the longest prefix, then the order");
        assert_eq!(route(&links, 0, 0xc0a8_0105), Route::Link { link: 0, next_hop: 0xc0a8_0105 });
    }
}
