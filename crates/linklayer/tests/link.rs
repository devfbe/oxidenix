//! An Ethernet link on a simulated wire: DHCP and ARP as the frames go,
//! what comes in and what goes out.

use linklayer::arp;
use linklayer::dhcp::{Event, Loss};
use linklayer::link::{udp_frame, Drop, EthernetLink, Input, Output};
use linklayer::{Config, Rng, Wire, BROADCAST, BROADCAST_MAC, ETHERNET_HEADER};
use smoltcp::wire::{DhcpMessageType, DhcpOption, DhcpPacket, DhcpRepr, EthernetAddress, IpAddress, Ipv4Address, Ipv4Packet, UdpPacket};

const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const SERVER_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
const NEIGHBOUR_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x03];
const SERVER: u32 = 0x0a00_0202;
const ADDRESS: u32 = 0x0a00_020f;
const NEIGHBOUR: u32 = 0x0a00_0203;
const MTU: usize = 1500;
static DNS: [DhcpOption<'static>; 1] = [DhcpOption { kind: 6, data: &[10, 0, 2, 3] }];

struct Counter(u32);

impl Rng for Counter {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(7919);
        self.0
    }
}

/// The wire: what was sent, and whether there is room.
#[derive(Default)]
struct Tape {
    frames: Vec<Vec<u8>>,
    full: bool,
}

impl Wire for Tape {
    fn send(&mut self, frame: &[u8]) -> bool {
        if !self.full {
            self.frames.push(frame.to_vec());
        }
        !self.full
    }
}

impl Tape {
    fn take(&mut self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.frames)
    }
}

fn local(ip: u32) -> bool {
    ip == ADDRESS || ip >> 24 == 127
}

/// A frame's DHCP message: (IP source, IP destination, type, xid), its
/// IP and UDP headers checked.
fn dhcp_of(frame: &[u8]) -> (u32, u32, DhcpMessageType, u32) {
    assert_eq!(&frame[6..12], &MAC);
    assert_eq!(&frame[12..14], &[0x08, 0x00]);
    let ip = Ipv4Packet::new_checked(&frame[ETHERNET_HEADER..]).unwrap();
    assert!(ip.verify_checksum());
    assert_eq!(ip.hop_limit(), 64);
    let udp = UdpPacket::new_checked(ip.payload()).unwrap();
    assert!(udp.verify_checksum(&IpAddress::Ipv4(ip.src_addr()), &IpAddress::Ipv4(ip.dst_addr())));
    assert_eq!((udp.src_port(), udp.dst_port()), (68, 67));
    let p = DhcpPacket::new_checked(udp.payload()).unwrap();
    let r = DhcpRepr::parse(&p).unwrap();
    (ip.src_addr().to_bits(), ip.dst_addr().to_bits(), r.message_type, r.transaction_id)
}

/// The server's reply in a frame to `dst_mac`/`dst_ip`.
fn server_frame(ty: DhcpMessageType, xid: u32, dst_mac: [u8; 6], dst_ip: u32, edit: impl FnOnce(&mut DhcpRepr)) -> Vec<u8> {
    let mut r = DhcpRepr {
        message_type: ty,
        transaction_id: xid,
        secs: 0,
        client_hardware_address: EthernetAddress(MAC),
        client_ip: Ipv4Address::UNSPECIFIED,
        your_ip: Ipv4Address::from_bits(ADDRESS),
        server_ip: Ipv4Address::UNSPECIFIED,
        router: Some(Ipv4Address::from_bits(SERVER)),
        subnet_mask: Some(Ipv4Address::new(255, 255, 255, 0)),
        relay_agent_ip: Ipv4Address::UNSPECIFIED,
        broadcast: false,
        requested_ip: None,
        client_identifier: None,
        server_identifier: Some(Ipv4Address::from_bits(SERVER)),
        parameter_request_list: None,
        dns_servers: None,
        max_size: None,
        lease_duration: Some(1000),
        renew_duration: None,
        rebind_duration: None,
        additional_options: &DNS,
    };
    edit(&mut r);
    let mut payload = vec![0u8; r.buffer_len()];
    r.emit(&mut DhcpPacket::new_unchecked(&mut payload[..])).unwrap();
    let mut frame = vec![0u8; 42 + payload.len()];
    let len = udp_frame(&mut frame, SERVER, dst_ip, (67, 68), &payload);
    frame.truncate(len);
    ethernet(&mut frame, dst_mac, SERVER_MAC, 0x0800);
    frame
}

fn ethernet(frame: &mut [u8], dst: [u8; 6], src: [u8; 6], ty: u16) {
    frame[0..6].copy_from_slice(&dst);
    frame[6..12].copy_from_slice(&src);
    frame[12..14].copy_from_slice(&ty.to_be_bytes());
}

/// An IPv4 packet (UDP, port 9 to 9) from `src` to `dst` in a frame from
/// `src_mac` to `dst_mac`.
fn ip_frame(dst_mac: [u8; 6], src_mac: [u8; 6], src: u32, dst: u32) -> Vec<u8> {
    let mut frame = vec![0u8; 42 + 4];
    udp_frame(&mut frame, src, dst, (9, 9), b"ping");
    ethernet(&mut frame, dst_mac, src_mac, 0x0800);
    frame
}

/// A frame of ours for smoltcp's packet to `dst` (the header's room left).
fn outgoing(dst: u32, payload: usize) -> Vec<u8> {
    let mut frame = vec![0u8; 42 + payload];
    udp_frame(&mut frame, ADDRESS, dst, (9, 9), &vec![0x5a; payload]);
    frame[..ETHERNET_HEADER].fill(0);
    frame
}

/// A link that leased ADDRESS/24 from SERVER at time 0 (the server's
/// replies broadcast, so nothing is learnt about it).
fn configured(wire: &mut Tape, rng: &mut Counter) -> EthernetLink {
    let mut link = EthernetLink::new(MAC, MTU);
    link.set_carrier(true, 0);
    link.poll(0, rng, wire);
    let (_, _, _, xid) = dhcp_of(&wire.take()[0]);
    assert_eq!(link.input(&server_frame(DhcpMessageType::Offer, xid, BROADCAST_MAC, BROADCAST, |_| {}), 0, rng, local, wire), Input::Taken);
    link.poll(0, rng, wire);
    wire.take();
    assert_eq!(link.input(&server_frame(DhcpMessageType::Ack, xid, BROADCAST_MAC, BROADCAST, |_| {}), 0, rng, local, wire), Input::Taken);
    assert!(matches!(link.take_event(), Some(Event::Configured(_))));
    link
}

#[test]
fn dhcp_over_the_wire() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = EthernetLink::new(MAC, MTU);
    link.poll(0, &mut rng, &mut wire);
    assert!(wire.frames.is_empty(), "no carrier: nothing");
    assert_eq!(link.deadline(), None);
    link.set_carrier(true, 50);
    assert_eq!(link.deadline(), Some(50));
    link.poll(50, &mut rng, &mut wire);
    let sent = wire.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(&sent[0][0..6], &BROADCAST_MAC);
    let (src, dst, ty, xid) = dhcp_of(&sent[0]);
    assert_eq!((src, dst, ty), (0, BROADCAST, DhcpMessageType::Discover));
    // The offer comes by unicast to the address offered (some servers do
    // so): taken although that address is not ours yet.
    let offer = server_frame(DhcpMessageType::Offer, xid, MAC, ADDRESS, |_| {});
    assert_eq!(link.input(&offer, 60, &mut rng, |_| false, &mut wire), Input::Taken);
    link.poll(60, &mut rng, &mut wire);
    let sent = wire.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(dhcp_of(&sent[0]), (0, BROADCAST, DhcpMessageType::Request, xid));
    assert_eq!(link.config(), None);
    let ack = server_frame(DhcpMessageType::Ack, xid, MAC, ADDRESS, |_| {});
    assert_eq!(link.input(&ack, 70, &mut rng, |_| false, &mut wire), Input::Taken);
    let config = Config { address: ADDRESS, prefix: 24, gateway: SERVER, dns: 0x0a00_0203 };
    assert_eq!(link.take_event(), Some(Event::Configured(config)));
    assert_eq!(link.take_event(), None);
    assert_eq!(link.config(), Some(config));
}

#[test]
fn dhcp_replies_are_checked_and_taken_whatever_they_are() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = EthernetLink::new(MAC, MTU);
    link.set_carrier(true, 0);
    link.poll(0, &mut rng, &mut wire);
    let (_, _, _, xid) = dhcp_of(&wire.take()[0]);
    let offer = server_frame(DhcpMessageType::Offer, xid, BROADCAST_MAC, BROADCAST, |_| {});
    // A broken UDP checksum: taken (port 68 is the client's), not used.
    let mut bad = offer.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xff;
    assert_eq!(link.input(&bad, 1, &mut rng, local, &mut wire), Input::Taken);
    // From another port than the server's: the same.
    let mut other = offer.clone();
    other[34..36].copy_from_slice(&1067u16.to_be_bytes());
    assert_eq!(link.input(&other, 1, &mut rng, local, &mut wire), Input::Taken);
    // A broken IP header checksum: malformed.
    let mut broken = offer.clone();
    broken[24] ^= 1;
    assert_eq!(link.input(&broken, 1, &mut rng, local, &mut wire), Input::Dropped(Drop::Malformed));
    link.poll(1, &mut rng, &mut wire);
    assert!(wire.take().is_empty(), "still selecting: no REQUEST");
    // A fragment is no DHCP reply: like any packet it goes up if it is
    // for us (to ADDRESS: only once that is ours).
    let mut fragment = server_frame(DhcpMessageType::Offer, xid, MAC, ADDRESS, |_| {});
    {
        let mut ip = Ipv4Packet::new_unchecked(&mut fragment[ETHERNET_HEADER..]);
        ip.set_more_frags(true);
        ip.fill_checksum();
    }
    assert_eq!(link.input(&fragment, 1, &mut rng, |_| false, &mut wire), Input::Dropped(Drop::NotForUs));
    assert!(matches!(link.input(&fragment, 1, &mut rng, local, &mut wire), Input::Packet(_)));
    link.poll(1, &mut rng, &mut wire);
    assert!(wire.take().is_empty());
    // The real one.
    assert_eq!(link.input(&offer, 2, &mut rng, local, &mut wire), Input::Taken);
    link.poll(2, &mut rng, &mut wire);
    assert_eq!(dhcp_of(&wire.take()[0]).2, DhcpMessageType::Request);
}

#[test]
fn arp_requests_for_our_address_are_answered() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = configured(&mut wire, &mut rng);
    let request = arp::Packet { op: arp::REQUEST, sender_mac: NEIGHBOUR_MAC, sender_ip: NEIGHBOUR, target_mac: [0; 6], target_ip: ADDRESS };
    assert_eq!(link.input(&request.frame(BROADCAST_MAC, NEIGHBOUR_MAC), 10, &mut rng, local, &mut wire), Input::Taken);
    let sent = wire.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(&sent[0][0..6], &NEIGHBOUR_MAC);
    let reply = arp::Packet::parse(&sent[0][ETHERNET_HEADER..]).unwrap();
    assert_eq!(reply, arp::Packet { op: arp::REPLY, sender_mac: MAC, sender_ip: ADDRESS, target_mac: NEIGHBOUR_MAC, target_ip: NEIGHBOUR });
    // The asker was learnt: a packet to it goes at once.
    let mut frame = outgoing(NEIGHBOUR, 10);
    assert_eq!(link.output(&mut frame, NEIGHBOUR, 11, &mut wire), Output::Sent);
    let sent = wire.take();
    assert_eq!((&sent[0][0..6], &sent[0][6..12], &sent[0][12..14]), (&NEIGHBOUR_MAC[..], &MAC[..], &[8u8, 0][..]));
    // A probe (from 0.0.0.0) for our address is answered too, not learnt.
    let probe = arp::Packet { sender_ip: 0, sender_mac: [2, 9, 9, 9, 9, 9], ..request };
    link.input(&probe.frame(BROADCAST_MAC, [2, 9, 9, 9, 9, 9]), 12, &mut rng, local, &mut wire);
    let sent = wire.take();
    assert_eq!(arp::Packet::parse(&sent[0][ETHERNET_HEADER..]).unwrap().target_ip, 0);
    // A request for another address: no answer, nothing learnt.
    let other = arp::Packet { target_ip: 0x0a00_0209, sender_ip: 0x0a00_0208, sender_mac: [2, 8, 8, 8, 8, 8], ..request };
    assert_eq!(link.input(&other.frame(BROADCAST_MAC, [2, 8, 8, 8, 8, 8]), 13, &mut rng, local, &mut wire), Input::Taken);
    assert!(wire.take().is_empty());
    let mut frame = outgoing(0x0a00_0208, 10);
    assert_eq!(link.output(&mut frame, 0x0a00_0208, 14, &mut wire), Output::Queued);
    // Someone claiming our address teaches nothing.
    let claim = arp::Packet { op: arp::REPLY, sender_mac: [2, 7, 7, 7, 7, 7], sender_ip: ADDRESS, target_mac: MAC, target_ip: ADDRESS };
    assert_eq!(link.input(&claim.frame(MAC, [2, 7, 7, 7, 7, 7]), 15, &mut rng, local, &mut wire), Input::Dropped(Drop::Martian));
}

#[test]
fn resolution_on_the_wire() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = configured(&mut wire, &mut rng);
    let mut frame = outgoing(0x0101_0101, 100);
    assert_eq!(link.output(&mut frame, SERVER, 100, &mut wire), Output::Queued);
    let sent = wire.take();
    assert_eq!(sent.len(), 1, "the request at once");
    assert_eq!(&sent[0][0..6], &BROADCAST_MAC);
    let request = arp::Packet::parse(&sent[0][ETHERNET_HEADER..]).unwrap();
    assert_eq!(request, arp::Packet { op: arp::REQUEST, sender_mac: MAC, sender_ip: ADDRESS, target_mac: [0; 6], target_ip: SERVER });
    assert_eq!(link.deadline(), Some(100 + arp::RETRANSMIT_MS));
    let mut second = outgoing(0x0101_0102, 100);
    assert_eq!(link.output(&mut second, SERVER, 200, &mut wire), Output::Queued);
    assert!(wire.take().is_empty(), "one request at a time");
    let reply = arp::Packet { op: arp::REPLY, sender_mac: SERVER_MAC, sender_ip: SERVER, target_mac: MAC, target_ip: ADDRESS };
    assert_eq!(link.input(&reply.frame(MAC, SERVER_MAC), 300, &mut rng, local, &mut wire), Input::Taken);
    let sent = wire.take();
    assert_eq!(sent.len(), 2, "both waiting packets, in order");
    assert_eq!(&sent[0][0..6], &SERVER_MAC);
    assert_eq!(&sent[0][6..12], &MAC);
    assert_eq!(&sent[0][ETHERNET_HEADER + 16..ETHERNET_HEADER + 20], &[1, 1, 1, 1]);
    assert_eq!(&sent[1][ETHERNET_HEADER + 16..ETHERNET_HEADER + 20], &[1, 1, 1, 2]);
    // Unanswered: three requests, then the packet goes.
    let mut frame = outgoing(NEIGHBOUR, 10);
    assert_eq!(link.output(&mut frame, NEIGHBOUR, 1000, &mut wire), Output::Queued);
    for t in [2000, 3000, 4000] {
        link.poll(t, &mut rng, &mut wire);
    }
    let requests = wire.take().iter().filter(|f| f[12..14] == [8, 6]).count();
    assert_eq!(requests, arp::REQUESTS as usize);
    assert_eq!(link.arp().pending(), 0);
}

#[test]
fn broadcast_multicast_and_what_cannot_go() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = configured(&mut wire, &mut rng);
    for (hop, mac) in [(BROADCAST, BROADCAST_MAC), (0x0a00_02ff, BROADCAST_MAC), (0xe000_00fb, [1, 0, 0x5e, 0, 0, 0xfb])] {
        let mut frame = outgoing(hop, 10);
        assert_eq!(link.output(&mut frame, hop, 1, &mut wire), Output::Sent);
        assert_eq!(&wire.take()[0][0..6], &mac);
    }
    let mut big = outgoing(BROADCAST, MTU - 28 + 1);
    assert_eq!(link.output(&mut big, BROADCAST, 1, &mut wire), Output::Dropped(Drop::TooBig));
    let mut fits = outgoing(BROADCAST, MTU - 28);
    assert_eq!(link.output(&mut fits, BROADCAST, 1, &mut wire), Output::Sent);
    wire.full = true;
    let mut frame = outgoing(BROADCAST, 10);
    assert_eq!(link.output(&mut frame, BROADCAST, 1, &mut wire), Output::Dropped(Drop::NoRoom));
    wire.full = false;
    // Without a lease nobody can be asked for.
    let mut fresh = EthernetLink::new(MAC, MTU);
    fresh.set_carrier(true, 0);
    let mut frame = outgoing(SERVER, 10);
    assert_eq!(fresh.output(&mut frame, SERVER, 1, &mut wire), Output::Dropped(Drop::Unresolved));
    // Without a carrier nothing goes.
    let mut frame = outgoing(BROADCAST, 10);
    assert_eq!(EthernetLink::new(MAC, MTU).output(&mut frame, BROADCAST, 1, &mut wire), Output::Dropped(Drop::NoCarrier));
}

#[test]
fn what_comes_in() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = configured(&mut wire, &mut rng);
    let mut input = |frame: &[u8]| link.input(frame, 5, &mut rng, local, &mut wire);
    let ours = ip_frame(MAC, SERVER_MAC, SERVER, ADDRESS);
    assert_eq!(input(&ours), Input::Packet(ETHERNET_HEADER..ETHERNET_HEADER + 32));
    // Ethernet's padding is left out of the packet.
    let mut padded = ours.clone();
    padded.resize(60 + 10, 0);
    assert_eq!(input(&padded), Input::Packet(ETHERNET_HEADER..ETHERNET_HEADER + 32));
    assert!(matches!(input(&ip_frame(BROADCAST_MAC, SERVER_MAC, SERVER, BROADCAST)), Input::Packet(_)));
    assert!(matches!(input(&ip_frame(BROADCAST_MAC, SERVER_MAC, SERVER, 0x0a00_02ff)), Input::Packet(_)), "the subnet's broadcast");
    // To any address of the host (another link's).
    let mut any = |ip| link.input(&ip_frame(MAC, SERVER_MAC, SERVER, ip), 5, &mut Counter(0), |a| a == 0xc0a8_2a05 || local(a), &mut Tape::default());
    assert!(matches!(any(0xc0a8_2a05), Input::Packet(_)));
    assert_eq!(any(0x0a00_0209), Input::Dropped(Drop::NotForUs));
    let mut input = |frame: &[u8]| link.input(frame, 5, &mut rng, local, &mut wire);
    assert_eq!(input(&ip_frame([2, 1, 1, 1, 1, 1], SERVER_MAC, SERVER, ADDRESS)), Input::Dropped(Drop::NotForUs), "another host's frame");
    assert_eq!(input(&ip_frame([1, 0, 0x5e, 0, 0, 1], SERVER_MAC, SERVER, 0xe000_0001)), Input::Dropped(Drop::NotForUs), "no multicast");
    assert_eq!(input(&ip_frame(MAC, SERVER_MAC, SERVER, 0x0a00_0209)), Input::Dropped(Drop::NotForUs), "another address");
    for (src, dst) in [(0x7f00_0001, ADDRESS), (SERVER, 0x7f00_0001), (ADDRESS, ADDRESS), (0, ADDRESS), (BROADCAST, ADDRESS), (0xe000_0001, ADDRESS)] {
        assert_eq!(input(&ip_frame(MAC, SERVER_MAC, src, dst)), Input::Dropped(Drop::Martian), "{src:x} -> {dst:x}");
    }
    assert_eq!(input(&ip_frame(MAC, MAC, SERVER, ADDRESS)), Input::Dropped(Drop::Martian), "our own frame");
    assert_eq!(input(&ip_frame(MAC, [3, 0, 0, 0, 0, 1], SERVER, ADDRESS)), Input::Dropped(Drop::Martian), "a group source");
    let arp_from_loopback = arp::Packet { op: arp::REQUEST, sender_mac: SERVER_MAC, sender_ip: 0x7f00_0001, target_mac: [0; 6], target_ip: ADDRESS };
    assert_eq!(input(&arp_from_loopback.frame(BROADCAST_MAC, SERVER_MAC)), Input::Dropped(Drop::Martian));
    let mut ipv6 = ours.clone();
    ipv6[12..14].copy_from_slice(&[0x86, 0xdd]);
    assert_eq!(input(&ipv6), Input::Dropped(Drop::Protocol));
    assert_eq!(input(&ours[..13]), Input::Dropped(Drop::Malformed));
    assert_eq!(input(&ours[..ETHERNET_HEADER + 19]), Input::Dropped(Drop::Malformed));
    assert_eq!(input(&ours[..ours.len() - 1]), Input::Dropped(Drop::Malformed), "shorter than its total length");
    let mut v6 = ours.clone();
    v6[ETHERNET_HEADER] = 0x65;
    assert_eq!(input(&v6), Input::Dropped(Drop::Malformed));
    let mut arp_short = arp_from_loopback.frame(BROADCAST_MAC, SERVER_MAC).to_vec();
    arp_short.truncate(30);
    assert_eq!(input(&arp_short), Input::Dropped(Drop::Malformed));
}

#[test]
fn carrier_loss_and_return() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = configured(&mut wire, &mut rng);
    let reply = arp::Packet { op: arp::REPLY, sender_mac: SERVER_MAC, sender_ip: SERVER, target_mac: MAC, target_ip: ADDRESS };
    link.input(&reply.frame(MAC, SERVER_MAC), 1, &mut rng, local, &mut wire);
    assert_eq!(link.arp().len(), 1);
    link.set_carrier(false, 10);
    assert_eq!(link.take_event(), Some(Event::Lost(Loss::Stopped)));
    assert_eq!(link.config(), None);
    assert!(link.arp().is_empty(), "what it knew went with the lease");
    assert_eq!(link.deadline(), None);
    assert_eq!(link.input(&ip_frame(MAC, SERVER_MAC, SERVER, ADDRESS), 11, &mut rng, local, &mut wire), Input::Dropped(Drop::NoCarrier));
    link.poll(20, &mut rng, &mut wire);
    assert!(wire.take().is_empty());
    link.set_carrier(true, 30);
    link.poll(30, &mut rng, &mut wire);
    let sent = wire.take();
    assert_eq!(dhcp_of(&sent[0]).2, DhcpMessageType::Discover, "DHCP again at once");
}

#[test]
fn renewal_by_unicast_and_the_tests_restart() {
    let (mut wire, mut rng) = (Tape::default(), Counter(1));
    let mut link = configured(&mut wire, &mut rng);
    // T1 (500 s): the REQUEST goes to the server, resolved first.
    link.poll(500_000, &mut rng, &mut wire);
    let sent = wire.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(arp::Packet::parse(&sent[0][ETHERNET_HEADER..]).unwrap().target_ip, SERVER);
    let reply = arp::Packet { op: arp::REPLY, sender_mac: SERVER_MAC, sender_ip: SERVER, target_mac: MAC, target_ip: ADDRESS };
    link.input(&reply.frame(MAC, SERVER_MAC), 500_001, &mut rng, local, &mut wire);
    let sent = wire.take();
    assert_eq!(&sent[0][0..6], &SERVER_MAC);
    let (src, dst, ty, xid) = dhcp_of(&sent[0]);
    assert_eq!((src, dst, ty), (ADDRESS, SERVER, DhcpMessageType::Request));
    let ack = server_frame(DhcpMessageType::Ack, xid, MAC, ADDRESS, |_| {});
    assert_eq!(link.input(&ack, 500_002, &mut rng, local, &mut wire), Input::Taken);
    assert_eq!(link.take_event(), None, "renewed: nothing changed");
    assert_eq!(link.lease().unwrap().expires, Some(1_500_002));
    // The tests' renewal: gone now, DISCOVER a second later.
    link.renew(501_000);
    assert_eq!(link.take_event(), Some(Event::Lost(Loss::Restart)));
    assert!(link.arp().is_empty());
    link.poll(500_999, &mut rng, &mut wire);
    assert!(wire.take().is_empty());
    link.poll(501_000, &mut rng, &mut wire);
    assert_eq!(dhcp_of(&wire.take()[0]).2, DhcpMessageType::Discover);
}
