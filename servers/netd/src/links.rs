//! netd's links: the device under its one smoltcp interface (`Medium::Ip`)
//! that is all of them. smoltcp cannot route between several interfaces
//! sharing one socket set, so it sees IP packets only and this module does
//! the rest, with `linklayer` (routing, each Ethernet link's ARP, DHCP and
//! filters, tested on the host):
//!
//! - the loopback: packets to an address of this host (any link's, or
//!   127.0.0.0/8) come back as received packets;
//! - the virtio card, if there is one (index 2, metric 100: `virtio_net`);
//! - USB links: the network functions usbd drives, each offered over a
//!   channel of its own (`linkring`), attached here and gone with the
//!   channel or when its driver breaks the protocol (metric 200).
//!
//! **Indexes** are Linux's interface indexes: the loopback 1, the card 2,
//! then each link that comes the next one, never reused while netd runs.
//! **Names** are the Linux server's (`netdev`): eth0, ... for PCI links,
//! usb0, ... for USB ones, by index; netd's messages use them too.
//!
//! **Each round** takes at most `RX_PER_ROUND` frames from each link,
//! starting after the link the last round's ingress stopped at (no link
//! starves another), and `poll` runs the links' timers, notices carriers
//! and gone links and applies what changed: smoltcp's addresses (the
//! default link's first: smoltcp sends from its first address where no
//! subnet decides; the loopback's last), its default route (the default
//! link's router: `Medium::Ip` emits nothing without a route), its MTU (the
//! smallest link's), the records the instances read (`LINKS`).
//!
//! **Sending** (`transmit`) needs room on the loopback path and on the
//! card's queue, as before links came: smoltcp then keeps what it has
//! instead of losing it. A USB link's full send ring drops the packet (as
//! a full queue drops at the tail): a driver that stopped consuming must
//! not stop the other links.

use crate::virtio_net::VirtioNet;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use linklayer::dhcp::{Event, Loss};
use linklayer::link::{EthernetLink, Input, Output};
use linklayer::route::{self, Candidate, Route};
use linklayer::{Dotted, Mac, Wire, ETHERNET_HEADER, MAX_MTU};
use oxrt::println;
use ring::channel::{Header, Layout, Offer, CLIENT_GONE};
use smoltcp::iface::Interface;
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr};

/// The loopback's address.
pub const LOOPBACK: Ipv4Cidr = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
/// Ethernet links at most: smoltcp holds 8 addresses (its
/// `iface-max-addr-count`), one of them the loopback's.
pub const MAX_LINKS: usize = 7;
/// Packets waiting to be "received" from the loopback path.
const MAX_LOOPED: usize = 64;
/// Frames taken from one link per round.
const RX_PER_ROUND: usize = 64;
const INDEX_LOOPBACK: u32 = 1;
const INDEX_CARD: u32 = 2;
/// The largest packet smoltcp may hand over (it keeps to the MTU).
const MAX_PACKET: usize = linkring::MAX_FRAME - ETHERNET_HEADER;
const ENOBUFS: i64 = 105;
const EINVAL: i64 = 22;

/// What the links take of netd's heap at most: the loopback queue (twice
/// `MAX_LOOPED` at most), the frames waiting for ARP and the ARP caches,
/// the buffers, the records.
pub const HEAP_WORST: usize = {
    use core::mem::size_of;
    let looped = 2 * MAX_LOOPED * (MAX_MTU + 32);
    let link = size_of::<Link>() + linklayer::arp::PENDING * (ETHERNET_HEADER + MAX_MTU + 48) + linklayer::arp::ENTRIES * 32 + linklayer::link::MAX_EVENTS * 16;
    looped + MAX_LINKS * link + 2 * linkring::MAX_FRAME + (1 + MAX_LINKS) * (size_of::<netring::Link>() + size_of::<Candidate>())
};

/// netd's random numbers for the links (DHCP's transaction ids and
/// jitter).
struct Random(csprng::ChaCha);

impl linklayer::Rng for Random {
    fn next_u32(&mut self) -> u32 {
        self.0.u64() as u32
    }
}

/// A futex wake of the driver (its doorbell).
struct WakeOne;

impl linkring::Wait for WakeOne {
    fn wait(&self, word: &core::sync::atomic::AtomicU32, value: u32) {
        let _ = oxrt::futex_wait(word, value, None);
    }

    fn wake(&self, word: &core::sync::atomic::AtomicU32) {
        let _ = oxrt::futex_wake(word, 1);
    }
}

/// Where a link's frames go and come from.
enum Port {
    Card(VirtioNet),
    Ring(RingPort),
}

/// A USB link's channel.
struct RingPort {
    channel: u64,
    /// Mapped until `chan_detach`, which comes after the port is dropped.
    header: &'static Header,
    area: linkring::Area,
    rx: linkring::Consumer,
    tx: linkring::Producer,
    /// Records went into the send ring this round (the driver's doorbell).
    rang: bool,
    /// The driver broke the protocol: the link goes.
    broken: bool,
}

impl Wire for Port {
    fn send(&mut self, frame: &[u8]) -> bool {
        match self {
            Port::Card(card) => {
                if !card.can_send() {
                    return false;
                }
                card.send(frame.len(), |buf| buf.copy_from_slice(frame));
                true
            }
            Port::Ring(r) => {
                let sent = r.tx.push(frame);
                r.rang |= sent;
                sent
            }
        }
    }
}

impl Port {
    /// The next frame from the wire, copied into `buf`: its length.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self {
            Port::Card(card) => card.receive(|f| {
                let n = f.len().min(buf.len());
                buf[..n].copy_from_slice(&f[..n]);
                n
            }),
            Port::Ring(r) if r.broken => None,
            Port::Ring(r) => match r.rx.pop(buf) {
                Ok(n) => n,
                Err(_) => {
                    r.broken = true;
                    None
                }
            },
        }
    }

    fn has_received(&mut self) -> bool {
        match self {
            Port::Card(card) => card.has_received(),
            Port::Ring(r) => !r.broken && !r.rx.is_empty(),
        }
    }
}

struct Link {
    index: u32,
    bus: u8,
    metric: u32,
    /// What drives it, for messages.
    driver: String,
    eth: EthernetLink,
    port: Port,
    /// Frames it may still give this round.
    budget: usize,
}

pub struct Links {
    links: Vec<Link>,
    /// The links as routing sees them, in the same order.
    routes: Vec<Candidate>,
    /// The host's addresses (the links' leased ones).
    locals: Vec<u32>,
    looped: VecDeque<Vec<u8>>,
    next_index: u32,
    rng: Random,
    /// A packet on its way out, after room for its Ethernet header.
    scratch: Vec<u8>,
    /// A frame on its way in.
    rx: Vec<u8>,
    /// Where the next round's ingress starts.
    cursor: usize,
    /// The interface's MTU: the smallest link's.
    mtu: usize,
    /// Something the instances or smoltcp see changed (`take_changed`).
    changed: bool,
}

pub struct RxToken(Vec<u8>);
pub struct TxToken<'a>(&'a mut Links);

fn now_ms() -> u64 {
    oxrt::uptime_ms()
}

impl Links {
    /// The loopback, and the card if there is one (its carrier is always
    /// there: DHCP starts at once).
    pub fn new(card: Option<VirtioNet>) -> Links {
        let mut seed = [0u8; 32];
        oxrt::getrandom(&mut seed);
        let mut links = Links {
            links: Vec::with_capacity(MAX_LINKS),
            routes: Vec::with_capacity(MAX_LINKS),
            locals: Vec::with_capacity(MAX_LINKS),
            looped: VecDeque::with_capacity(2 * MAX_LOOPED),
            next_index: INDEX_CARD,
            rng: Random(csprng::ChaCha::new(seed)),
            scratch: vec![0; ETHERNET_HEADER + MAX_PACKET],
            rx: vec![0; linkring::MAX_FRAME],
            cursor: 0,
            mtu: MAX_MTU,
            changed: true,
        };
        if let Some(card) = card {
            let mut eth = EthernetLink::new(card.mac, MAX_MTU);
            eth.set_carrier(true, now_ms());
            links.add(linklayer::route::METRIC_PCI, netring::LINK_BUS_PCI, String::from("virtio-net"), eth, Port::Card(card));
        }
        links
    }

    fn add(&mut self, metric: u32, bus: u8, driver: String, eth: EthernetLink, port: Port) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        self.links.push(Link { index, bus, metric, driver, eth, port, budget: RX_PER_ROUND });
        self.refresh();
        index
    }

    pub fn has_card(&self) -> bool {
        self.links.iter().any(|l| matches!(l.port, Port::Card(_)))
    }

    /// The card's lease, if it has one.
    pub fn card_configured(&self) -> bool {
        self.links.iter().any(|l| matches!(l.port, Port::Card(_)) && l.eth.config().is_some())
    }

    /// The card interrupted: acknowledged (the frames are taken in the
    /// round).
    pub fn card_interrupt(&mut self) {
        for l in &mut self.links {
            if let Port::Card(card) = &mut l.port {
                card.ack_interrupt();
            }
        }
    }

    /// The Linux server's name of link `i` (eth0, usb0, ...: by bus, in
    /// the order of the indexes).
    fn name(&self, i: usize) -> String {
        let l = &self.links[i];
        let n = self.links[..i].iter().filter(|o| o.bus == l.bus).count();
        alloc::format!("{}{}", if l.bus == netring::LINK_BUS_USB { "usb" } else { "eth" }, n)
    }

    /// The routing table, the host's addresses and the MTU after a change.
    fn refresh(&mut self) {
        self.routes.clear();
        self.locals.clear();
        for l in &self.links {
            let config = l.eth.config().filter(|_| l.eth.carrier());
            self.routes.push(Candidate { config, metric: l.metric });
            if let Some(c) = config {
                self.locals.push(c.address);
            }
        }
        self.mtu = self.links.iter().map(|l| l.eth.mtu()).min().unwrap_or(MAX_MTU).min(MAX_MTU);
        self.changed = true;
    }

    /// Whether something changed that smoltcp and the instances must hear
    /// of (`apply`, `records`), since the last call.
    pub fn take_changed(&mut self) -> bool {
        core::mem::take(&mut self.changed)
    }

    /// smoltcp's addresses, default route and MTU as the links have them.
    pub fn apply(&self, iface: &mut Interface) {
        let default = route::default_link(&self.routes);
        let ordered = default.into_iter().chain((0..self.routes.len()).filter(|&i| Some(i) != default));
        iface.update_ip_addrs(|addrs| {
            addrs.clear();
            for i in ordered {
                if let Some(c) = self.routes[i].config {
                    let _ = addrs.push(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::from_bits(c.address), c.prefix)));
                }
            }
            let _ = addrs.push(IpCidr::Ipv4(LOOPBACK));
        });
        match default.and_then(|i| self.routes[i].config) {
            Some(c) => {
                let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::from_bits(c.gateway));
            }
            None => {
                iface.routes_mut().remove_default_ipv4_route();
            }
        }
        iface.set_max_transmission_unit(self.mtu);
    }

    /// The interfaces as the instances read them (`LINKS`): the loopback,
    /// then the links.
    pub fn records(&self, out: &mut Vec<netring::Link>) {
        out.clear();
        let up = netring::LINK_UP | netring::LINK_RUNNING;
        out.push(netring::Link {
            index: INDEX_LOOPBACK,
            kind: netring::LINK_LOOPBACK,
            state: up,
            mtu: self.mtu as u32,
            prefix: LOOPBACK.prefix_len(),
            address: LOOPBACK.address().to_bits(),
            ..Default::default()
        });
        let default = route::default_link(&self.routes);
        for (i, l) in self.links.iter().enumerate() {
            let c = self.routes[i].config.unwrap_or_default();
            let mut state = netring::LINK_UP;
            if l.eth.carrier() {
                state |= netring::LINK_RUNNING;
            }
            if default == Some(i) {
                state |= netring::LINK_DEFAULT;
            }
            out.push(netring::Link {
                index: l.index,
                kind: netring::LINK_ETHERNET,
                state,
                mtu: l.eth.mtu() as u32,
                mac: l.eth.mac(),
                prefix: c.prefix,
                bus: l.bus,
                address: c.address,
                gateway: c.gateway,
                dns: c.dns,
            });
        }
    }

    // ---------------------------------------------------------- lifecycle

    /// A link driver's offer (`Offer::instance` 0): the status to answer
    /// with. The channel is attached and its description checked.
    pub fn offer(&mut self, offer: &Offer) -> i64 {
        let want = Layout::with_shared(linkring::SLOTS, linkring::SHARED_PAGES).expect("a valid layout");
        if offer.layout() != Some(want) {
            return -EINVAL;
        }
        if self.links.len() >= MAX_LINKS {
            println!("netd: a link offered beyond the {} netd keeps: refused", MAX_LINKS);
            return -ENOBUFS;
        }
        let base = match oxrt::chan_attach(offer.channel) {
            Ok(base) => base,
            Err(e) => return e,
        };
        // Mapped until chan_detach (`remove`).
        let (area, header) = unsafe { (linkring::Area::new(base, &want).expect("a link channel's layout"), Header::at(base)) };
        let info = match area.info() {
            Ok(info) => info,
            Err(e) => {
                println!("netd: a link offer of process {} with a malformed description ({:?}): refused", offer.client, e);
                let _ = oxrt::chan_detach(offer.channel);
                return -EINVAL;
            }
        };
        let port = RingPort { channel: offer.channel, header, area, rx: area.rx_consumer(), tx: area.tx_producer(), rang: false, broken: false };
        // The carrier comes with the next `poll`.
        let eth = EthernetLink::new(info.mac, (info.mtu as usize).min(MAX_MTU));
        self.add(route::METRIC_USB, netring::LINK_BUS_USB, String::from(info.driver_name()), eth, Port::Ring(port));
        let i = self.links.len() - 1;
        println!("netd: {} ({} {}) attached", self.name(i), info.driver_name(), Mac(info.mac));
        0
    }

    /// Link `i` goes (its channel closed, or its driver broke the
    /// protocol): with it its lease and its routes.
    fn remove(&mut self, i: usize) {
        println!("netd: {} gone", self.name(i));
        let l = self.links.remove(i);
        if let Port::Ring(r) = l.port {
            let channel = r.channel;
            drop(r);
            let _ = oxrt::chan_detach(channel);
        }
        self.cursor = 0;
        self.refresh();
    }

    /// The links' timers, carriers and lease changes, gone links; their
    /// messages. True if anything happened.
    pub fn poll(&mut self) -> bool {
        let now = now_ms();
        let mut progress = false;
        let mut i = 0;
        while i < self.links.len() {
            let (broken, gone, carrier) = match &self.links[i].port {
                Port::Ring(r) => (r.broken, r.header.state() & CLIENT_GONE != 0, r.area.carrier()),
                Port::Card(_) => (false, false, true),
            };
            if broken || gone {
                if broken {
                    println!("netd: {}'s driver broke the link protocol", self.name(i));
                }
                self.remove(i);
                progress = true;
                continue;
            }
            let l = &mut self.links[i];
            if carrier != l.eth.carrier() {
                l.eth.set_carrier(carrier, now);
                progress = true;
            }
            if l.eth.deadline().is_some_and(|at| at <= now) {
                l.eth.poll(now, &mut self.rng, &mut l.port);
            }
            while let Some(e) = self.links[i].eth.take_event() {
                self.report(i, e);
                progress = true;
            }
            i += 1;
        }
        if progress {
            self.refresh();
        }
        progress
    }

    /// The message for a lease change of link `i`.
    fn report(&self, i: usize, e: Event) {
        let l = &self.links[i];
        let card = matches!(l.port, Port::Card(_));
        let mac = Mac(l.eth.mac());
        match e {
            Event::Configured(c) => {
                let at = alloc::format!("{}/{} via DHCP", Dotted(c.address), c.prefix);
                let gateway = match c.gateway {
                    0 => String::from("no gateway"),
                    g => alloc::format!("gateway {}", Dotted(g)),
                };
                if card {
                    println!("netd: {}, {} (virtio-net {})", at, gateway, mac);
                } else {
                    println!("netd: {} ({} {}): {}, {}", self.name(i), l.driver, mac, at, gateway);
                }
            }
            Event::Lost(Loss::Expired | Loss::Nak) if card => println!("netd: lost the DHCP lease"),
            Event::Lost(Loss::Expired | Loss::Nak) => println!("netd: {} lost the DHCP lease", self.name(i)),
            Event::Lost(Loss::Stopped) => println!("netd: {}: no carrier", self.name(i)),
            // The caller said so (`renew`).
            Event::Lost(Loss::Restart) => {}
        }
    }

    /// The self-tests' renewal: the first link's lease (the card's, which
    /// comes first) goes now, DHCP asks again `delay_ms` later. False
    /// without a link.
    pub fn renew(&mut self, delay_ms: u64) -> bool {
        let Some(l) = self.links.first_mut() else { return false };
        l.eth.renew(now_ms() + delay_ms);
        self.refresh();
        true
    }

    /// When `poll` has something to do.
    pub fn deadline(&self) -> Option<u64> {
        self.links.iter().filter_map(|l| l.eth.deadline()).min()
    }

    // -------------------------------------------------------------- rounds

    /// A new round: each link may give `RX_PER_ROUND` frames.
    pub fn round_start(&mut self) {
        for l in &mut self.links {
            l.budget = RX_PER_ROUND;
        }
    }

    /// Whether frames wait (smoltcp's ingress takes them next round).
    pub fn has_received(&mut self) -> bool {
        !self.looped.is_empty() || self.links.iter_mut().any(|l| l.port.has_received())
    }

    /// The end of a round: the drivers hear of the frames they got.
    pub fn flush(&mut self) {
        for l in &mut self.links {
            if let Port::Ring(r) = &mut l.port {
                if core::mem::take(&mut r.rang) {
                    r.area.ring_driver(&WakeOne);
                }
            }
        }
    }

    /// Announces the sleep in every link's channel and arms its doorbell:
    /// false if work came meanwhile (then nothing sleeps).
    pub fn prepare_sleep(&mut self) -> bool {
        if !self.looped.is_empty() {
            return false;
        }
        for l in &mut self.links {
            match &mut l.port {
                Port::Card(card) => {
                    if card.has_received() {
                        return false;
                    }
                }
                Port::Ring(r) => {
                    if r.broken || r.header.state() != 0 {
                        return false;
                    }
                    let Some(value) = r.area.prepare_netd_sleep(&r.rx, l.eth.carrier()) else { return false };
                    match oxrt::chan_watch(r.channel, value) {
                        Ok(true) => {}
                        Ok(false) | Err(_) => return false,
                    }
                }
            }
        }
        true
    }

    /// Ends a sleep: the drivers stop ringing while netd polls.
    pub fn awake(&mut self) {
        for l in &mut self.links {
            if let Port::Ring(r) = &l.port {
                r.area.netd_awake();
            }
        }
    }

    // ------------------------------------------------------------ packets

    /// Sends smoltcp's packet in `scratch[ETHERNET_HEADER..end]` where
    /// routing says.
    fn output(&mut self, end: usize, now: u64) {
        let packet = &self.scratch[ETHERNET_HEADER..end];
        if packet.len() < 20 {
            return;
        }
        let src = u32::from_be_bytes(packet[12..16].try_into().expect("4 bytes"));
        let dst = u32::from_be_bytes(packet[16..20].try_into().expect("4 bytes"));
        match route::route(&self.routes, src, dst) {
            Route::Loopback => {
                // `transmit` hands out a token only while there is room; a
                // reply smoltcp makes while it takes a packet (an ACK, a
                // reset) may go beyond, up to twice as many.
                if self.looped.len() < 2 * MAX_LOOPED {
                    self.looped.push_back(packet.to_vec());
                }
            }
            Route::Link { link, next_hop } => {
                let l = &mut self.links[link];
                // (A full ring or queue drops it, as a busy wire would.)
                let _: Output = l.eth.output(&mut self.scratch[..end], next_hop, now, &mut l.port);
            }
            Route::Unreachable => {}
        }
    }
}

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let links = self.0;
        if len > MAX_PACKET {
            // Beyond any link's MTU (smoltcp keeps to the MTU): built and
            // dropped.
            return f(&mut vec![0u8; len]);
        }
        let end = ETHERNET_HEADER + len;
        let result = f(&mut links.scratch[ETHERNET_HEADER..end]);
        links.output(end, now_ms());
        result
    }
}

impl Device for Links {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, timestamp: Instant) -> Option<(RxToken, TxToken<'_>)> {
        if let Some(packet) = self.looped.pop_front() {
            return Some((RxToken(packet), TxToken(self)));
        }
        let now = timestamp.total_millis() as u64;
        let n = self.links.len();
        for k in 0..n {
            let i = (self.cursor + k) % n;
            let (locals, rx, rng) = (&self.locals, &mut self.rx, &mut self.rng);
            let l = &mut self.links[i];
            while l.budget > 0 {
                let Some(len) = l.port.receive(rx) else { break };
                l.budget -= 1;
                let is_local = |ip: u32| linklayer::is_loopback(ip) || locals.contains(&ip);
                if let Input::Packet(range) = l.eth.input(&rx[..len], now, rng, is_local, &mut l.port) {
                    // The next call starts after this link.
                    self.cursor = (i + 1) % n;
                    let packet = rx[range].to_vec();
                    return Some((RxToken(packet), TxToken(self)));
                }
            }
        }
        None
    }

    /// Room on the loopback path and on the card's queue, or none: smoltcp
    /// then keeps its segments and sends them in a later round, instead of
    /// their being dropped (a TCP window larger than the queues, as grown
    /// buffers give, would otherwise lose frames and wait for a
    /// retransmission).
    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        let card = self.links.iter_mut().all(|l| match &mut l.port {
            Port::Card(card) => card.can_send(),
            Port::Ring(_) => true,
        });
        (self.looped.len() < MAX_LOOPED && card).then_some(TxToken(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}
