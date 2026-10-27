//! The DHCP client of one Ethernet link (RFC 2131), its messages in
//! smoltcp's wire format (`DhcpRepr`).
//!
//! **States** (RFC 2131 4.4): *init* (a DISCOVER is due), *selecting*
//! (DISCOVERs went: the first acceptable OFFER is taken), *requesting* (a
//! REQUEST for the offer went: an ACK binds, a NAK starts again, so does
//! silence after `REQUEST_TRIES` of them), *bound*, *renewing* (from T1:
//! REQUESTs by unicast to the server that leased), *rebinding* (from T2:
//! by broadcast, to any server); at the lease's end the address goes and
//! the client starts again. Without a carrier it is *stopped*.
//!
//! **Retransmission** (4.1): in selecting and requesting 4 s, doubling up
//! to 64 s, each randomized by up to a second either way; in renewing and
//! rebinding half the time left to T2 (to the lease's end), at least 60 s
//! (4.4.5). Each exchange has a transaction id of its own, from the
//! caller's generator. The client identifier is the hardware address.
//!
//! **What it takes** from a server: replies of its exchange (xid, the
//! hardware address) only; an address that a host may have and that is
//! neither its subnet's network nor broadcast address; a contiguous mask
//! (a classful one without); a router in the subnet; the lease time (an
//! hour without, as BusyBox's udhcpc assumes), T1 and T2 when they are
//! ordered (else RFC 2131's 1/2 and 7/8 of the lease). A NAK counts only
//! from the server of the exchange (any while rebinding).
//!
//! The caller moves the messages (`poll` hands them out with where they go,
//! `input` takes the UDP payloads that came to port 68) and applies the
//! lease (`Event`).

use crate::{is_unicast, netmask, Config, Rng, BROADCAST};
use smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, Ipv4Address};

pub use smoltcp::wire::{DHCP_CLIENT_PORT as CLIENT_PORT, DHCP_SERVER_PORT as SERVER_PORT};

pub const FIRST_RETRANSMIT_MS: u64 = 4_000;
pub const MAX_RETRANSMIT_MS: u64 = 64_000;
/// Each retransmission's randomization, either way.
pub const JITTER_MS: u64 = 1_000;
/// REQUESTs for an offer before the client starts again.
pub const REQUEST_TRIES: u32 = 4;
/// The shortest retransmission while renewing or rebinding.
pub const RENEW_MIN_MS: u64 = 60_000;
/// A lease without a lease time lasts this long.
pub const DEFAULT_LEASE_S: u32 = 3600;
/// After a NAK, the client starts again after this (randomized like a
/// retransmission): a server that refuses every request is not flooded.
pub const RESTART_MS: u64 = 2_000;
/// The largest message the client takes (RFC 2131's minimum: every
/// server keeps to it, and it fits every link's MTU).
pub const MAX_MESSAGE: u16 = 576;
/// The bytes of the payload a message takes at most here (padded to
/// BOOTP's 300 bytes, which some servers insist on).
pub const MAX_PAYLOAD: usize = 400;
const MIN_PAYLOAD: usize = 300;
/// What the client asks for: the mask, the router, DNS servers, the lease
/// time, T1, T2.
const PARAMETERS: &[u8] = &[1, 3, 6, 51, 58, 59];

/// A lease as the client holds it (times are absolute, None: infinite).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    pub config: Config,
    /// The server that leased it (its server identifier).
    pub server: u32,
    pub t1: Option<u64>,
    pub t2: Option<u64>,
    pub expires: Option<u64>,
}

/// Why a lease went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loss {
    /// The server refused to extend it.
    Nak,
    /// It ran out without a renewal.
    Expired,
    /// The link lost its carrier.
    Stopped,
    /// The caller asked for a new one (`restart`).
    Restart,
}

/// What changed for the caller to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A lease began, or a renewal changed it.
    Configured(Config),
    Lost(Loss),
}

/// A message to send: its payload's length (in the caller's buffer), its
/// IP destination (None: the limited broadcast) and source (0 before a
/// lease).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub len: usize,
    pub to: Option<u32>,
    pub from: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Offer {
    address: u32,
    server: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Stopped,
    /// A DISCOVER is due at `at`.
    Init { at: u64 },
    /// `sent` DISCOVERs went, the first at `started`; the next is due at
    /// `next`.
    Selecting { xid: u32, started: u64, sent: u32, next: u64 },
    /// `sent` REQUESTs for `offer` went; `secs` is the DISCOVER's.
    Requesting { xid: u32, offer: Offer, secs: u16, sent: u32, next: u64 },
    Bound,
    Renewing { xid: u32, started: u64, next: u64 },
    Rebinding { xid: u32, started: u64, next: u64 },
}

pub struct Client {
    mac: [u8; 6],
    state: State,
    lease: Option<Lease>,
    /// The last address leased, asked for again in DISCOVERs (0: none).
    previous: u32,
}

/// A delay randomized by up to `JITTER_MS` either way.
fn jittered(ms: u64, rng: &mut impl Rng) -> u64 {
    (ms + rng.next_u32() as u64 % (2 * JITTER_MS + 1)).saturating_sub(JITTER_MS)
}

/// The retransmission delay after the `sent`th message of an exchange.
fn backoff(sent: u32, rng: &mut impl Rng) -> u64 {
    let base = FIRST_RETRANSMIT_MS.checked_shl(sent.saturating_sub(1)).unwrap_or(MAX_RETRANSMIT_MS).min(MAX_RETRANSMIT_MS);
    jittered(base, rng)
}

fn secs(started: u64, now: u64) -> u16 {
    (now.saturating_sub(started) / 1000).min(u16::MAX as u64) as u16
}

/// The prefix length of a classful network (a lease without a mask).
fn classful(address: u32) -> u8 {
    match address >> 29 {
        0..=3 => 8,
        4 | 5 => 16,
        _ => 24,
    }
}

/// The lease a reply offers, if it is acceptable (see the module's
/// comment); `server` is the server it counts as from.
fn lease_of(repr: &DhcpRepr, server: u32, now: u64) -> Option<Lease> {
    let address = repr.your_ip.to_bits();
    if !is_unicast(address) {
        return None;
    }
    let prefix = match repr.subnet_mask.map(|m| m.to_bits()) {
        Some(m) if m != 0 && m.leading_ones() + m.trailing_zeros() == 32 => m.leading_ones() as u8,
        Some(_) => return None,
        None => classful(address),
    };
    let mask = netmask(prefix);
    if prefix <= 30 && (address & !mask == 0 || address & !mask == !mask) {
        return None;
    }
    let config = Config {
        address,
        prefix,
        gateway: repr.router.map(|r| r.to_bits()).filter(|&r| is_unicast(r) && r != address && r & mask == address & mask && (prefix > 30 || r & !mask != !mask)).unwrap_or(0),
        dns: repr.dns_servers.as_ref().and_then(|d| d.iter().map(|a| a.to_bits()).find(|&a| is_unicast(a))).unwrap_or(0),
    };
    let lease_s = repr.lease_duration.unwrap_or(DEFAULT_LEASE_S);
    let (t1, t2, expires) = if lease_s == u32::MAX {
        (None, None, None)
    } else {
        let lease = lease_s as u64 * 1000;
        let (mut t1, mut t2) = (repr.renew_duration.map(|t| t as u64 * 1000), repr.rebind_duration.map(|t| t as u64 * 1000));
        if !t2.is_some_and(|t2| t2 < lease) {
            t2 = Some(lease * 7 / 8);
        }
        if !t1.is_some_and(|t1| Some(t1) < t2) {
            t1 = Some(lease / 2);
        }
        (t1.map(|t| now + t), t2.map(|t| now + t), Some(now + lease))
    };
    Some(Lease { config, server, t1, t2, expires })
}

impl Client {
    /// A client for the link with hardware address `mac`, stopped until
    /// its link has a carrier.
    pub fn new(mac: [u8; 6]) -> Client {
        Client { mac, state: State::Stopped, lease: None, previous: 0 }
    }

    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    pub fn config(&self) -> Option<Config> {
        self.lease.map(|l| l.config)
    }

    pub fn is_stopped(&self) -> bool {
        self.state == State::Stopped
    }

    /// The link has a carrier: a DISCOVER goes at once.
    pub fn start(&mut self, now: u64) {
        if self.state == State::Stopped {
            self.state = State::Init { at: now };
        }
    }

    fn drop_lease(&mut self, why: Loss) -> Option<Event> {
        let lease = self.lease.take()?;
        self.previous = lease.config.address;
        Some(Event::Lost(why))
    }

    /// The link lost its carrier: the lease goes (no server can be asked
    /// for it), the client waits for `start`.
    pub fn stop(&mut self) -> Option<Event> {
        self.state = State::Stopped;
        self.drop_lease(Loss::Stopped)
    }

    /// The lease goes now and the client asks for a new one at `at` (the
    /// self-tests' renewal). Nothing while stopped but the lease's end.
    pub fn restart(&mut self, at: u64) -> Option<Event> {
        if self.state != State::Stopped {
            self.state = State::Init { at };
        }
        self.drop_lease(Loss::Restart)
    }

    /// When `poll` has something to do.
    pub fn deadline(&self) -> Option<u64> {
        let timer = match self.state {
            State::Stopped | State::Bound => None,
            State::Init { at } => Some(at),
            State::Selecting { next, .. } | State::Requesting { next, .. } | State::Renewing { next, .. } | State::Rebinding { next, .. } => Some(next),
        };
        let lease = self.lease.and_then(|l| match self.state {
            State::Bound => l.t1.into_iter().chain(l.expires).min(),
            State::Renewing { .. } => l.t2.into_iter().chain(l.expires).min(),
            _ => l.expires,
        });
        timer.into_iter().chain(lease).min()
    }

    /// The timers: the lease's end, T1 and T2, and the message that is due,
    /// written into `buf` (at least `MAX_PAYLOAD` bytes) and handed to
    /// `send`. Returns what changed.
    pub fn poll(&mut self, now: u64, rng: &mut impl Rng, buf: &mut [u8], mut send: impl FnMut(&Message, &[u8])) -> Option<Event> {
        let mut event = None;
        if let Some(l) = self.lease {
            if l.expires.is_some_and(|t| now >= t) {
                self.state = State::Init { at: now };
                event = self.drop_lease(Loss::Expired);
            } else if l.t2.is_some_and(|t| now >= t) && matches!(self.state, State::Bound | State::Renewing { .. }) {
                self.state = State::Rebinding { xid: rng.next_u32(), started: now, next: now };
            } else if l.t1.is_some_and(|t| now >= t) && self.state == State::Bound {
                self.state = State::Renewing { xid: rng.next_u32(), started: now, next: now };
            }
        }
        if let Some(m) = self.due(now, rng, buf) {
            send(&m, &buf[..m.len]);
        }
        event
    }

    /// The message due now, if any (the state moves on with it).
    fn due(&mut self, now: u64, rng: &mut impl Rng, buf: &mut [u8]) -> Option<Message> {
        let mac = EthernetAddress(self.mac);
        let mut repr = DhcpRepr {
            message_type: DhcpMessageType::Discover,
            transaction_id: 0,
            secs: 0,
            client_hardware_address: mac,
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: Ipv4Address::UNSPECIFIED,
            server_ip: Ipv4Address::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: false,
            requested_ip: None,
            client_identifier: Some(mac),
            server_identifier: None,
            parameter_request_list: Some(PARAMETERS),
            dns_servers: None,
            max_size: Some(MAX_MESSAGE),
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let (to, from) = match self.state {
            State::Init { at } if now >= at => {
                let xid = rng.next_u32();
                self.state = State::Selecting { xid, started: now, sent: 1, next: now + backoff(1, rng) };
                repr.transaction_id = xid;
                repr.requested_ip = (self.previous != 0).then(|| Ipv4Address::from_bits(self.previous));
                (None, 0)
            }
            State::Selecting { xid, started, sent, next } if now >= next => {
                self.state = State::Selecting { xid, started, sent: sent + 1, next: now + backoff(sent + 1, rng) };
                repr.transaction_id = xid;
                repr.secs = secs(started, now);
                repr.requested_ip = (self.previous != 0).then(|| Ipv4Address::from_bits(self.previous));
                (None, 0)
            }
            State::Requesting { sent, next, .. } if now >= next && sent >= REQUEST_TRIES => {
                self.state = State::Init { at: now };
                return self.due(now, rng, buf);
            }
            State::Requesting { xid, offer, secs, sent, next } if now >= next => {
                self.state = State::Requesting { xid, offer, secs, sent: sent + 1, next: now + backoff(sent + 1, rng) };
                repr.message_type = DhcpMessageType::Request;
                repr.transaction_id = xid;
                repr.secs = secs;
                repr.requested_ip = Some(Ipv4Address::from_bits(offer.address));
                repr.server_identifier = Some(Ipv4Address::from_bits(offer.server));
                (None, 0)
            }
            State::Renewing { xid, started, next } | State::Rebinding { xid, started, next } if now >= next => {
                let lease = self.lease.expect("renewing a lease");
                let renewing = matches!(self.state, State::Renewing { .. });
                // Half the time left to the next stage, at least a minute
                // (the stage's own timer comes first if it is sooner).
                let end = if renewing { lease.t2 } else { lease.expires }.unwrap_or(now);
                let next = now + (end.saturating_sub(now) / 2).max(RENEW_MIN_MS);
                self.state = if renewing { State::Renewing { xid, started, next } } else { State::Rebinding { xid, started, next } };
                repr.message_type = DhcpMessageType::Request;
                repr.transaction_id = xid;
                repr.secs = secs(started, now);
                repr.client_ip = Ipv4Address::from_bits(lease.config.address);
                (renewing.then_some(lease.server), lease.config.address)
            }
            _ => return None,
        };
        let len = repr.buffer_len().max(MIN_PAYLOAD);
        let out = &mut buf[..len];
        out.fill(0);
        repr.emit(&mut DhcpPacket::new_unchecked(&mut *out)).expect("room for the message");
        Some(Message { len, to, from })
    }

    /// A UDP payload that came to the client's port (from the server's).
    /// Returns what changed; a message may be due now (`poll`).
    pub fn input(&mut self, now: u64, rng: &mut impl Rng, payload: &[u8]) -> Option<Event> {
        let packet = DhcpPacket::new_checked(payload).ok()?;
        let repr = DhcpRepr::parse(&packet).ok()?;
        let xid = match self.state {
            State::Selecting { xid, .. } | State::Requesting { xid, .. } | State::Renewing { xid, .. } | State::Rebinding { xid, .. } => xid,
            _ => return None,
        };
        if repr.transaction_id != xid || repr.client_hardware_address.0 != self.mac {
            return None;
        }
        let server = repr.server_identifier.map(|s| s.to_bits());
        match (self.state, repr.message_type) {
            (State::Selecting { started, .. }, DhcpMessageType::Offer) => {
                let server = server.filter(|&s| is_unicast(s))?;
                // Checked as a lease would be: an offer that cannot become
                // one is not taken.
                lease_of(&repr, server, now)?;
                let offer = Offer { address: repr.your_ip.to_bits(), server };
                self.state = State::Requesting { xid, offer, secs: secs(started, now), sent: 0, next: now };
                None
            }
            (State::Requesting { offer, .. }, DhcpMessageType::Ack) => {
                if server.is_some_and(|s| s != offer.server) {
                    return None;
                }
                self.bind(lease_of(&repr, offer.server, now)?)
            }
            (State::Renewing { .. } | State::Rebinding { .. }, DhcpMessageType::Ack) => {
                let old = self.lease.expect("renewing a lease");
                if matches!(self.state, State::Renewing { .. }) && server.is_some_and(|s| s != old.server) {
                    return None;
                }
                self.bind(lease_of(&repr, server.filter(|&s| is_unicast(s)).unwrap_or(old.server), now)?)
            }
            (State::Requesting { offer, .. }, DhcpMessageType::Nak) => {
                if server.is_some_and(|s| s != offer.server) {
                    return None;
                }
                self.state = State::Init { at: now + jittered(RESTART_MS, rng) };
                None
            }
            (State::Renewing { .. } | State::Rebinding { .. }, DhcpMessageType::Nak) => {
                let old = self.lease.expect("renewing a lease");
                if matches!(self.state, State::Renewing { .. }) && server.is_some_and(|s| s != old.server) {
                    return None;
                }
                self.state = State::Init { at: now + jittered(RESTART_MS, rng) };
                self.drop_lease(Loss::Nak)
            }
            _ => None,
        }
    }

    fn bind(&mut self, lease: Lease) -> Option<Event> {
        let changed = self.lease.map(|l| l.config) != Some(lease.config);
        self.lease = Some(lease);
        self.previous = lease.config.address;
        self.state = State::Bound;
        changed.then_some(Event::Configured(lease.config))
    }
}

/// The limited broadcast, for messages without a destination.
pub fn destination(m: &Message) -> u32 {
    m.to.unwrap_or(BROADCAST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use smoltcp::wire::DhcpOption;

    const MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
    const SERVER: u32 = 0x0a00_0202;
    const ADDRESS: u32 = 0x0a00_020f;
    const DNS: u32 = 0x0a00_0203;
    static DNS_OPTION: [DhcpOption<'static>; 1] = [DhcpOption { kind: 6, data: &[10, 0, 2, 3] }];
    static TWO_DNS: [DhcpOption<'static>; 1] = [DhcpOption { kind: 6, data: &[0, 0, 0, 0, 1, 1, 1, 1] }];
    // (`DhcpRepr::emit` leaves T1 and T2 out: a server's come as options.)
    static T1_100_T2_200: [DhcpOption<'static>; 2] = [DhcpOption { kind: 58, data: &[0, 0, 0, 100] }, DhcpOption { kind: 59, data: &[0, 0, 0, 200] }];
    static T1_300_T2_200: [DhcpOption<'static>; 2] = [DhcpOption { kind: 58, data: &[0, 0, 1, 44] }, DhcpOption { kind: 59, data: &[0, 0, 0, 200] }];
    static T2_2000: [DhcpOption<'static>; 1] = [DhcpOption { kind: 59, data: &[0, 0, 7, 208] }];

    /// A generator that counts.
    struct Counter(u32);

    impl Rng for Counter {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_add(1);
            self.0
        }
    }

    /// A generator without jitter: distinct numbers that are all
    /// `JITTER_MS` modulo 2001 (no shift).
    struct Fixed;

    impl Rng for Fixed {
        fn next_u32(&mut self) -> u32 {
            static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            JITTER_MS as u32 + (2 * JITTER_MS as u32 + 1) * N.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        }
    }

    /// What a message of the client's says.
    #[derive(Debug)]
    struct Fields {
        ty: DhcpMessageType,
        xid: u32,
        secs: u16,
        client_ip: u32,
        requested: Option<u32>,
        server: Option<u32>,
        client_id: Option<[u8; 6]>,
        chaddr: [u8; 6],
        parameters: Vec<u8>,
        max_size: Option<u16>,
    }

    struct Sent {
        message: Message,
        len: usize,
        m: Fields,
    }

    fn fields(payload: &[u8]) -> Fields {
        let p = DhcpPacket::new_checked(payload).unwrap();
        let r = DhcpRepr::parse(&p).unwrap();
        Fields {
            ty: r.message_type,
            xid: r.transaction_id,
            secs: r.secs,
            client_ip: r.client_ip.to_bits(),
            requested: r.requested_ip.map(|a| a.to_bits()),
            server: r.server_identifier.map(|a| a.to_bits()),
            client_id: r.client_identifier.map(|a| a.0),
            chaddr: r.client_hardware_address.0,
            parameters: r.parameter_request_list.unwrap_or(&[]).to_vec(),
            max_size: r.max_size,
        }
    }

    fn poll(c: &mut Client, now: u64, rng: &mut impl Rng) -> (Option<Event>, Option<Sent>) {
        let mut buf = [0u8; MAX_PAYLOAD];
        let mut sent = None;
        let event = c.poll(now, rng, &mut buf, |m, p| sent = Some(Sent { message: *m, len: p.len(), m: fields(p) }));
        (event, sent)
    }

    /// A server's reply to `xid` of type `ty` (an OFFER or ACK leasing
    /// ADDRESS/24 via SERVER for 1000 s, unless `edit` changes it).
    fn reply(ty: DhcpMessageType, xid: u32, edit: impl FnOnce(&mut DhcpRepr)) -> Vec<u8> {
        let nak = ty == DhcpMessageType::Nak;
        let mut r = DhcpRepr {
            message_type: ty,
            transaction_id: xid,
            secs: 0,
            client_hardware_address: EthernetAddress(MAC),
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: if nak { Ipv4Address::UNSPECIFIED } else { Ipv4Address::from_bits(ADDRESS) },
            server_ip: Ipv4Address::UNSPECIFIED,
            router: (!nak).then(|| Ipv4Address::from_bits(SERVER)),
            subnet_mask: (!nak).then(|| Ipv4Address::new(255, 255, 255, 0)),
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: false,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(Ipv4Address::from_bits(SERVER)),
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            lease_duration: (!nak).then_some(1000),
            renew_duration: None,
            rebind_duration: None,
            additional_options: if nak { &[] } else { &DNS_OPTION },
        };
        edit(&mut r);
        let mut out = vec![0u8; r.buffer_len()];
        r.emit(&mut DhcpPacket::new_unchecked(&mut out[..])).unwrap();
        out
    }

    /// The exchange's id while it is under way.
    fn current_xid(c: &Client) -> u32 {
        match c.state {
            State::Selecting { xid, .. } | State::Requesting { xid, .. } | State::Renewing { xid, .. } | State::Rebinding { xid, .. } => xid,
            s => panic!("no exchange: {s:?}"),
        }
    }

    /// A client that took an offer at time 0 (its REQUEST due).
    fn requesting() -> Client {
        let mut c = Client::new(MAC);
        c.start(0);
        poll(&mut c, 0, &mut Fixed).1.expect("a DISCOVER");
        c.input(0, &mut Fixed, &reply(DhcpMessageType::Offer, current_xid(&c), |_| {}));
        c
    }

    /// A client bound at time 0 to the usual lease (1000 s).
    fn bound() -> Client {
        let mut c = requesting();
        poll(&mut c, 0, &mut Fixed).1.expect("a REQUEST");
        assert!(c.input(0, &mut Fixed, &reply(DhcpMessageType::Ack, current_xid(&c), |_| {})).is_some());
        c
    }

    #[test]
    fn discover_offer_request_ack() {
        let mut c = Client::new(MAC);
        assert!(poll(&mut c, 0, &mut Counter(0)).1.is_none(), "stopped without a carrier");
        assert_eq!(c.deadline(), None);
        c.start(100);
        assert_eq!(c.deadline(), Some(100));
        let mut rng = Counter(0);
        let (event, d) = poll(&mut c, 100, &mut rng);
        assert_eq!(event, None);
        let d = d.expect("a DISCOVER at once");
        assert_eq!(d.message, Message { len: d.len, to: None, from: 0 });
        assert!(d.len >= 300, "BOOTP's minimum");
        assert_eq!(d.m.ty, DhcpMessageType::Discover);
        assert_eq!((d.m.client_id, d.m.chaddr), (Some(MAC), MAC));
        assert_eq!(d.m.parameters, PARAMETERS);
        assert_eq!(d.m.max_size, Some(MAX_MESSAGE));
        assert_eq!(d.m.requested, None);
        let xid = d.m.xid;
        // Only replies of the exchange count.
        assert_eq!(c.input(110, &mut rng, &reply(DhcpMessageType::Offer, xid ^ 1, |_| {})), None);
        assert!(poll(&mut c, 110, &mut rng).1.is_none(), "still selecting");
        assert_eq!(c.input(110, &mut rng, &reply(DhcpMessageType::Offer, xid, |r| r.client_hardware_address = EthernetAddress([2; 6]))), None);
        assert!(poll(&mut c, 110, &mut rng).1.is_none());
        // An ACK while selecting is not an offer.
        assert_eq!(c.input(110, &mut rng, &reply(DhcpMessageType::Ack, xid, |_| {})), None);
        assert!(c.lease().is_none());
        assert_eq!(c.input(120, &mut rng, &reply(DhcpMessageType::Offer, xid, |_| {})), None);
        let q = poll(&mut c, 120, &mut rng).1.expect("a REQUEST at once");
        assert_eq!(q.message.to, None, "broadcast while requesting");
        assert_eq!(q.m.ty, DhcpMessageType::Request);
        assert_eq!(q.m.xid, xid, "the same exchange");
        assert_eq!((q.m.requested, q.m.server, q.m.client_ip), (Some(ADDRESS), Some(SERVER), 0));
        // An ACK from another server is not ours.
        assert_eq!(c.input(130, &mut rng, &reply(DhcpMessageType::Ack, xid, |r| r.server_identifier = Some(Ipv4Address::new(10, 0, 2, 9)))), None);
        let config = Config { address: ADDRESS, prefix: 24, gateway: SERVER, dns: DNS };
        assert_eq!(c.input(130, &mut rng, &reply(DhcpMessageType::Ack, xid, |_| {})), Some(Event::Configured(config)));
        let l = *c.lease().unwrap();
        assert_eq!((l.server, l.t1, l.t2, l.expires), (SERVER, Some(130 + 500_000), Some(130 + 875_000), Some(130 + 1_000_000)));
        assert_eq!(c.deadline(), Some(130 + 500_000), "T1");
        assert!(poll(&mut c, 140, &mut rng).1.is_none());
        // A late duplicate changes nothing.
        assert_eq!(c.input(150, &mut rng, &reply(DhcpMessageType::Ack, xid, |_| {})), None);
        assert_eq!(c.lease().unwrap().expires, Some(130 + 1_000_000));
    }

    #[test]
    fn retransmissions_back_off_with_jitter() {
        let mut c = Client::new(MAC);
        c.start(0);
        let mut times = Vec::new();
        let mut xids = Vec::new();
        let mut now = 0;
        while now <= 300_000 {
            if let (_, Some(s)) = poll(&mut c, now, &mut Fixed) {
                times.push(now);
                xids.push(s.m.xid);
                assert_eq!(s.m.secs as u64, now / 1000, "seconds since the first DISCOVER");
            }
            now += 500;
        }
        assert_eq!(times, vec![0, 4_000, 12_000, 28_000, 60_000, 124_000, 188_000, 252_000], "4, 8, 16, 32, 64, 64, ... s");
        assert!(xids.iter().all(|&x| x == xids[0]), "one exchange");
        // The jitter stays within a second either way, and covers it.
        let mut rng = Counter(0);
        let (mut early, mut late) = (false, false);
        for sent in 1..4000 {
            let d = backoff(sent % 10 + 1, &mut rng);
            let base = FIRST_RETRANSMIT_MS.checked_shl(sent % 10).unwrap_or(u64::MAX).min(MAX_RETRANSMIT_MS);
            assert!(d + JITTER_MS >= base && d <= base + JITTER_MS, "{sent}: {d}");
            early |= d + JITTER_MS == base;
            late |= d == base + JITTER_MS;
        }
        assert!(early && late);
    }

    #[test]
    fn unanswered_requests_start_again() {
        let mut c = requesting();
        let first = current_xid(&c);
        let mut requests = 0;
        let mut now = 0;
        let discover = loop {
            if let (_, Some(s)) = poll(&mut c, now, &mut Fixed) {
                match s.m.ty {
                    DhcpMessageType::Request => requests += 1,
                    DhcpMessageType::Discover => break s,
                    t => panic!("{t:?}"),
                }
            }
            now += 1000;
        };
        assert_eq!(requests, REQUEST_TRIES);
        assert_eq!(now, 4_000 + 8_000 + 16_000 + 32_000);
        assert_ne!(discover.m.xid, first, "a new exchange");
    }

    #[test]
    fn a_nak_while_requesting_starts_again_later() {
        let mut c = requesting();
        poll(&mut c, 0, &mut Fixed).1.expect("a REQUEST");
        let x = current_xid(&c);
        // Not from the server of the offer: ignored.
        c.input(5, &mut Fixed, &reply(DhcpMessageType::Nak, x, |r| r.server_identifier = Some(Ipv4Address::new(10, 0, 2, 9))));
        assert_eq!(c.deadline(), Some(FIRST_RETRANSMIT_MS), "still requesting");
        assert_eq!(c.input(5, &mut Fixed, &reply(DhcpMessageType::Nak, x, |_| {})), None, "no lease to lose");
        assert_eq!(c.deadline(), Some(5 + RESTART_MS));
        assert!(poll(&mut c, 5 + RESTART_MS - 1, &mut Fixed).1.is_none());
        assert_eq!(poll(&mut c, 5 + RESTART_MS, &mut Fixed).1.unwrap().m.ty, DhcpMessageType::Discover);
    }

    #[test]
    fn renewing_rebinding_and_expiry() {
        let mut c = bound();
        assert!(poll(&mut c, 499_999, &mut Fixed).1.is_none());
        // T1: unicast to the server, from the address.
        let (event, s) = poll(&mut c, 500_000, &mut Fixed);
        assert_eq!(event, None);
        let s = s.unwrap();
        assert_eq!((s.message.to, s.message.from), (Some(SERVER), ADDRESS));
        assert_eq!((s.m.ty, s.m.client_ip), (DhcpMessageType::Request, ADDRESS));
        assert_eq!((s.m.requested, s.m.server), (None, None), "RFC 2131 4.3.2: neither while renewing");
        let renew_xid = s.m.xid;
        // Half the time to T2 (187.5 s) later, and so on.
        assert_eq!(c.deadline(), Some(500_000 + 187_500));
        assert!(poll(&mut c, 687_499, &mut Fixed).1.is_none());
        assert_eq!(poll(&mut c, 687_500, &mut Fixed).1.unwrap().m.xid, renew_xid, "the same exchange");
        assert_eq!(c.deadline(), Some(687_500 + 93_750));
        assert!(poll(&mut c, 781_250, &mut Fixed).1.is_some());
        // Down to a minute.
        assert_eq!(c.deadline(), Some(781_250 + RENEW_MIN_MS));
        assert!(poll(&mut c, 841_250, &mut Fixed).1.is_some());
        assert_eq!(c.deadline(), Some(875_000), "T2 comes first");
        // T2: rebinding by broadcast, a new exchange.
        let s = poll(&mut c, 875_000, &mut Fixed).1.unwrap();
        assert_eq!((s.message.to, s.message.from), (None, ADDRESS));
        assert_ne!(s.m.xid, renew_xid);
        assert_eq!((s.m.client_ip, s.m.requested, s.m.server), (ADDRESS, None, None));
        assert_eq!(c.deadline(), Some(875_000 + 62_500), "half of what is left");
        assert!(poll(&mut c, 937_500, &mut Fixed).1.is_some());
        assert_eq!(c.deadline(), Some(937_500 + RENEW_MIN_MS), "a minute at least");
        assert!(poll(&mut c, 997_500, &mut Fixed).1.is_some());
        assert_eq!(c.deadline(), Some(1_000_000), "the lease's end comes first");
        // The end: the address goes, a DISCOVER asks for it again.
        let (event, s) = poll(&mut c, 1_000_000, &mut Fixed);
        assert_eq!(event, Some(Event::Lost(Loss::Expired)));
        assert!(c.lease().is_none());
        let s = s.unwrap();
        assert_eq!((s.m.ty, s.m.requested, s.message.from), (DhcpMessageType::Discover, Some(ADDRESS), 0), "the old address as a hint");
    }

    #[test]
    fn a_renewal_extends_the_lease_quietly_or_reports_a_change() {
        let mut c = bound();
        poll(&mut c, 500_000, &mut Fixed).1.expect("a REQUEST");
        let x = current_xid(&c);
        // From another server while renewing: not ours.
        assert_eq!(c.input(500_001, &mut Fixed, &reply(DhcpMessageType::Ack, x, |r| r.server_identifier = Some(Ipv4Address::new(10, 0, 2, 9)))), None);
        assert!(matches!(c.state, State::Renewing { .. }));
        assert_eq!(c.input(500_010, &mut Fixed, &reply(DhcpMessageType::Ack, x, |_| {})), None, "the same lease: nothing to apply");
        assert_eq!(c.lease().unwrap().expires, Some(1_500_010));
        assert_eq!(c.deadline(), Some(1_000_010));
        // A renewal that brings a new router reports it.
        poll(&mut c, 1_000_010, &mut Fixed).1.expect("a REQUEST");
        let x = current_xid(&c);
        let event = c.input(1_000_020, &mut Fixed, &reply(DhcpMessageType::Ack, x, |r| r.router = Some(Ipv4Address::new(10, 0, 2, 1))));
        assert_eq!(event, Some(Event::Configured(Config { address: ADDRESS, prefix: 24, gateway: 0x0a00_0201, dns: DNS })));
        // While rebinding another server may answer (and leases from now on).
        poll(&mut c, 1_000_020 + 875_000, &mut Fixed).1.expect("a REQUEST");
        assert!(matches!(c.state, State::Rebinding { .. }));
        let x = current_xid(&c);
        c.input(1_000_020 + 875_001, &mut Fixed, &reply(DhcpMessageType::Ack, x, |r| r.server_identifier = Some(Ipv4Address::new(10, 0, 2, 9))));
        assert_eq!(c.lease().unwrap().server, 0x0a00_0209);
        assert_eq!(c.state, State::Bound);
    }

    #[test]
    fn a_nak_while_renewing_loses_the_lease() {
        let mut c = bound();
        poll(&mut c, 500_000, &mut Fixed).1.expect("a REQUEST");
        let x = current_xid(&c);
        assert_eq!(c.input(500_001, &mut Fixed, &reply(DhcpMessageType::Nak, x, |r| r.server_identifier = Some(Ipv4Address::new(10, 0, 2, 9)))), None, "another server's");
        assert_eq!(c.input(500_001, &mut Fixed, &reply(DhcpMessageType::Nak, x, |_| {})), Some(Event::Lost(Loss::Nak)));
        assert!(c.lease().is_none());
        assert_eq!(c.deadline(), Some(500_001 + RESTART_MS));
    }

    #[test]
    fn carrier_loss_and_restart() {
        let mut c = bound();
        assert_eq!(c.stop(), Some(Event::Lost(Loss::Stopped)));
        assert_eq!(c.stop(), None);
        assert!(c.is_stopped() && c.deadline().is_none());
        assert!(poll(&mut c, 2_000_000, &mut Fixed).1.is_none(), "no messages without a carrier");
        assert_eq!(c.input(0, &mut Fixed, &reply(DhcpMessageType::Ack, 0, |_| {})), None, "nor replies");
        assert_eq!(c.restart(5), None, "stopped: nothing to restart");
        assert!(c.is_stopped());
        c.start(10);
        assert_eq!(poll(&mut c, 10, &mut Fixed).1.unwrap().m.requested, Some(ADDRESS));
        // The tests' renewal: the lease goes now, the DISCOVER at `at`.
        let mut c = bound();
        assert_eq!(c.restart(1000), Some(Event::Lost(Loss::Restart)));
        assert!(poll(&mut c, 999, &mut Fixed).1.is_none());
        assert_eq!(poll(&mut c, 1000, &mut Fixed).1.unwrap().m.ty, DhcpMessageType::Discover);
    }

    #[test]
    fn lease_times() {
        let lease = |edit: &dyn Fn(&mut DhcpRepr)| {
            let mut c = requesting();
            poll(&mut c, 0, &mut Fixed);
            c.input(0, &mut Fixed, &reply(DhcpMessageType::Ack, current_xid(&c), |r| edit(r)));
            c.lease().map(|l| (l.t1, l.t2, l.expires))
        };
        assert_eq!(lease(&|r| r.additional_options = &T1_100_T2_200), Some((Some(100_000), Some(200_000), Some(1_000_000))));
        assert_eq!(lease(&|r| r.additional_options = &T1_300_T2_200), Some((Some(500_000), Some(200_000), Some(1_000_000))), "T1 after T2");
        assert_eq!(lease(&|r| r.additional_options = &T2_2000), Some((Some(500_000), Some(875_000), Some(1_000_000))), "T2 after the end");
        assert_eq!(lease(&|r| r.lease_duration = None), Some((Some(1_800_000), Some(3_150_000), Some(3_600_000))), "an hour without");
        assert_eq!(lease(&|r| r.lease_duration = Some(u32::MAX)), Some((None, None, None)), "infinite");
        let mut c = requesting();
        poll(&mut c, 0, &mut Fixed);
        c.input(0, &mut Fixed, &reply(DhcpMessageType::Ack, current_xid(&c), |r| r.lease_duration = Some(u32::MAX)));
        assert_eq!(c.deadline(), None, "an infinite lease has no timers");
        assert!(poll(&mut c, u64::MAX / 2, &mut Fixed).1.is_none());
    }

    #[test]
    fn unacceptable_offers() {
        let check = |what: &str, edit: &dyn Fn(&mut DhcpRepr), taken: bool| {
            let mut c = Client::new(MAC);
            c.start(0);
            poll(&mut c, 0, &mut Fixed);
            c.input(0, &mut Fixed, &reply(DhcpMessageType::Offer, current_xid(&c), |r| edit(r)));
            assert_eq!(matches!(c.state, State::Requesting { .. }), taken, "{what}");
        };
        check("a good offer", &|_| {}, true);
        check("no address", &|r| r.your_ip = Ipv4Address::UNSPECIFIED, false);
        check("a loopback address", &|r| r.your_ip = Ipv4Address::new(127, 0, 0, 2), false);
        check("a multicast address", &|r| r.your_ip = Ipv4Address::new(224, 0, 0, 1), false);
        check("the broadcast address", &|r| r.your_ip = Ipv4Address::BROADCAST, false);
        check("the subnet's broadcast", &|r| r.your_ip = Ipv4Address::new(10, 0, 2, 255), false);
        check("the subnet's network", &|r| r.your_ip = Ipv4Address::new(10, 0, 2, 0), false);
        check("a mask with holes", &|r| r.subnet_mask = Some(Ipv4Address::new(255, 0, 255, 0)), false);
        check("a zero mask", &|r| r.subnet_mask = Some(Ipv4Address::UNSPECIFIED), false);
        check("no server identifier", &|r| r.server_identifier = None, false);
        check("a broadcast server identifier", &|r| r.server_identifier = Some(Ipv4Address::BROADCAST), false);
        check("no mask: classful", &|r| r.subnet_mask = None, true);
    }

    #[test]
    fn malformed_packets_are_nothing() {
        let mut c = Client::new(MAC);
        c.start(0);
        poll(&mut c, 0, &mut Fixed);
        let good = reply(DhcpMessageType::Offer, current_xid(&c), |_| {});
        let mut bad: Vec<Vec<u8>> = vec![good[..10].to_vec(), good[..239].to_vec(), Vec::new()];
        for (at, flip) in [(236, 1), (1, 7), (0, 3)] {
            // The magic cookie, the hardware type, the opcode (a request).
            let mut b = good.clone();
            b[at] ^= flip;
            bad.push(b);
        }
        // Without the message type (its option overwritten by padding).
        let mut b = good.clone();
        b[240..243].fill(0);
        bad.push(b);
        for b in bad {
            assert_eq!(c.input(0, &mut Fixed, &b), None);
            assert!(matches!(c.state, State::Selecting { .. }), "{b:?}");
        }
        // Truncated options end the list (what came before counts).
        assert!(c.input(0, &mut Fixed, &good[..good.len() - 1]).is_none());
    }

    #[test]
    fn what_the_lease_holds() {
        let config = |edit: &dyn Fn(&mut DhcpRepr)| {
            let r = reply(DhcpMessageType::Ack, 1, |r| edit(r));
            let p = DhcpPacket::new_checked(&r[..]).unwrap();
            lease_of(&DhcpRepr::parse(&p).unwrap(), SERVER, 0).map(|l| l.config)
        };
        assert_eq!(config(&|_| {}), Some(Config { address: ADDRESS, prefix: 24, gateway: SERVER, dns: DNS }));
        assert_eq!(config(&|r| (r.subnet_mask, r.router) = (None, None)).unwrap().prefix, 8, "class A");
        assert_eq!(config(&|r| (r.your_ip, r.subnet_mask) = (Ipv4Address::new(172, 16, 1, 2), None)).unwrap().prefix, 16, "class B");
        assert_eq!(config(&|r| (r.your_ip, r.subnet_mask) = (Ipv4Address::new(192, 168, 1, 2), None)).unwrap().prefix, 24, "class C");
        assert_eq!(config(&|r| r.router = Some(Ipv4Address::new(10, 0, 3, 1))).unwrap().gateway, 0, "a router outside the subnet");
        assert_eq!(config(&|r| r.router = Some(Ipv4Address::from_bits(ADDRESS))).unwrap().gateway, 0, "ourselves as the router");
        assert_eq!(config(&|r| r.router = Some(Ipv4Address::new(10, 0, 2, 255))).unwrap().gateway, 0, "the broadcast address as the router");
        assert_eq!(config(&|r| r.router = None).unwrap().gateway, 0);
        assert_eq!(config(&|r| r.additional_options = &TWO_DNS).unwrap().dns, 0x0101_0101, "the first usable one");
        assert_eq!(config(&|r| r.additional_options = &[]).unwrap().dns, 0);
        let host = config(&|r| (r.your_ip, r.subnet_mask, r.router) = (Ipv4Address::new(10, 0, 2, 255), Some(Ipv4Address::BROADCAST), None)).unwrap();
        assert_eq!(host.prefix, 32, "a /32 has no network or broadcast address");
    }
}
