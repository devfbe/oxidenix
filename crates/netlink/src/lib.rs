//! NETLINK_ROUTE's messages (netlink(7), rtnetlink(7)): the requests a
//! program sends to the kernel's end of a netlink socket and the answers,
//! for the Linux server's netlink sockets; kept apart from the server so
//! they can be tested on the host. The server owns the socket (ports,
//! queue, waiting); this crate turns one datagram of requests into the
//! datagrams that answer it.
//!
//! What it answers: the interfaces (`RTM_GETLINK`, dumped or one by index
//! or name) and their IPv4 addresses (`RTM_GETADDR`, dumped), which is
//! what getifaddrs(3) asks, and with it libuv's interface list (Node.js's
//! `os.networkInterfaces()`). Every other routing request is refused with
//! `EOPNOTSUPP`, as Linux refuses requests a family does not implement.
//! Nothing changes the configuration (that is netd's, by DHCP).

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// `struct nlmsghdr`: length, type, flags, sequence number, port id.
pub const NLMSG_HDRLEN: usize = 16;

// Control messages.
pub const NLMSG_NOOP: u16 = 1;
pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
/// Types below are control messages, never requests of a family.
pub const NLMSG_MIN_TYPE: u16 = 0x10;

// rtnetlink's types.
pub const RTM_NEWLINK: u16 = 16;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_GETADDR: u16 = 22;

// Flags.
pub const NLM_F_REQUEST: u16 = 0x1;
pub const NLM_F_MULTI: u16 = 0x2;
pub const NLM_F_ACK: u16 = 0x4;
pub const NLM_F_ROOT: u16 = 0x100;
pub const NLM_F_MATCH: u16 = 0x200;
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;
/// In an acknowledgement: the request's payload is left out.
pub const NLM_F_CAPPED: u16 = 0x100;

// Interface flags (`IFF_*`, netdevice(7)).
pub const IFF_UP: u32 = 0x1;
pub const IFF_BROADCAST: u32 = 0x2;
pub const IFF_LOOPBACK: u32 = 0x8;
pub const IFF_RUNNING: u32 = 0x40;
pub const IFF_MULTICAST: u32 = 0x1000;
pub const IFF_LOWER_UP: u32 = 0x10000;

// Hardware types (`ARPHRD_*`).
pub const ARPHRD_ETHER: u16 = 1;
pub const ARPHRD_LOOPBACK: u16 = 772;

// Link attributes (`IFLA_*`).
pub const IFLA_ADDRESS: u16 = 1;
pub const IFLA_BROADCAST: u16 = 2;
pub const IFLA_IFNAME: u16 = 3;
pub const IFLA_MTU: u16 = 4;
pub const IFLA_OPERSTATE: u16 = 16;
pub const IFLA_LINKMODE: u16 = 17;
pub const IFLA_CARRIER: u16 = 33;

// Operational states (RFC 2863, `IF_OPER_*`).
pub const IF_OPER_UNKNOWN: u8 = 0;
pub const IF_OPER_DOWN: u8 = 2;
pub const IF_OPER_UP: u8 = 6;

// Address attributes (`IFA_*`) and flags.
pub const IFA_ADDRESS: u16 = 1;
pub const IFA_LOCAL: u16 = 2;
pub const IFA_LABEL: u16 = 3;
pub const IFA_BROADCAST: u16 = 4;
pub const IFA_FLAGS: u16 = 8;
pub const IFA_F_PERMANENT: u32 = 0x80;

// Scopes.
pub const RT_SCOPE_UNIVERSE: u8 = 0;
pub const RT_SCOPE_HOST: u8 = 254;

pub const AF_UNSPEC: u8 = 0;
pub const AF_INET: u8 = 2;
pub const AF_PACKET: u8 = 17;

const EINVAL: i32 = 22;
const ENODEV: i32 = 19;
const EOPNOTSUPP: i32 = 95;

/// How many bytes of messages one datagram of a dump carries at most (as
/// Linux's dumps fill a page-sized buffer at a time).
pub const DUMP_DATAGRAM: usize = 4096;

/// An IPv4 address of an interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4 {
    /// In host order (127.0.0.1 is 0x7f000001).
    pub address: u32,
    pub prefix: u8,
    /// `RT_SCOPE_HOST` for the loopback's, else `RT_SCOPE_UNIVERSE`.
    pub scope: u8,
    /// Configured for good (the loopback's), not leased (DHCP's).
    pub permanent: bool,
}

/// A network interface as rtnetlink describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub index: u32,
    pub name: String,
    /// `ARPHRD_*`.
    pub hw_type: u16,
    /// `IFF_*`.
    pub flags: u32,
    pub mtu: u32,
    pub mac: [u8; 6],
    pub broadcast_mac: [u8; 6],
    pub ipv4: Option<Ipv4>,
}

/// A message being built: header first, its length set by `finish`.
struct Message(Vec<u8>);

impl Message {
    fn new(ty: u16, flags: u16, seq: u32, port: u32) -> Message {
        let mut m = Vec::with_capacity(256);
        m.extend_from_slice(&0u32.to_le_bytes());
        m.extend_from_slice(&ty.to_le_bytes());
        m.extend_from_slice(&flags.to_le_bytes());
        m.extend_from_slice(&seq.to_le_bytes());
        m.extend_from_slice(&port.to_le_bytes());
        Message(m)
    }

    fn put(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    /// An attribute (`struct rtattr`), padded to 4 bytes.
    fn attr(&mut self, ty: u16, data: &[u8]) {
        let len = 4 + data.len();
        self.put(&(len as u16).to_le_bytes());
        self.put(&ty.to_le_bytes());
        self.put(data);
        self.pad();
    }

    fn pad(&mut self) {
        while self.0.len() % 4 != 0 {
            self.0.push(0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.pad();
        let len = self.0.len() as u32;
        self.0[0..4].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

/// The header of a request.
#[derive(Clone, Copy, Debug)]
struct Header {
    len: u32,
    ty: u16,
    flags: u16,
    seq: u32,
}

fn header(b: &[u8]) -> Header {
    Header {
        len: u32::from_le_bytes(b[0..4].try_into().expect("4 bytes")),
        ty: u16::from_le_bytes([b[4], b[5]]),
        flags: u16::from_le_bytes([b[6], b[7]]),
        seq: u32::from_le_bytes(b[8..12].try_into().expect("4 bytes")),
    }
}

/// The attributes after a fixed part: (type, data) pairs; stops at a
/// malformed one.
fn attributes(mut b: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    core::iter::from_fn(move || {
        if b.len() < 4 {
            return None;
        }
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        let ty = u16::from_le_bytes([b[2], b[3]]) & 0x3fff;
        if len < 4 || len > b.len() {
            return None;
        }
        let data = &b[4..len];
        b = &b[(len + 3 & !3).min(b.len())..];
        Some((ty, data))
    })
}

/// `RTM_NEWLINK` describing `i`.
fn link_message(i: &Interface, flags: u16, seq: u32, port: u32) -> Vec<u8> {
    let mut m = Message::new(RTM_NEWLINK, flags, seq, port);
    // struct ifinfomsg: family, pad, type, index, flags, change.
    m.put(&[AF_UNSPEC, 0]);
    m.put(&i.hw_type.to_le_bytes());
    m.put(&i.index.to_le_bytes());
    m.put(&i.flags.to_le_bytes());
    m.put(&0u32.to_le_bytes());
    let mut name = i.name.clone().into_bytes();
    name.push(0);
    m.attr(IFLA_IFNAME, &name);
    let running = i.flags & IFF_RUNNING != 0;
    let oper = match (i.flags & IFF_LOOPBACK != 0, running) {
        (true, _) => IF_OPER_UNKNOWN,
        (false, true) => IF_OPER_UP,
        (false, false) => IF_OPER_DOWN,
    };
    m.attr(IFLA_OPERSTATE, &[oper]);
    m.attr(IFLA_LINKMODE, &[0]);
    m.attr(IFLA_MTU, &i.mtu.to_le_bytes());
    m.attr(IFLA_CARRIER, &[running as u8]);
    m.attr(IFLA_ADDRESS, &i.mac);
    m.attr(IFLA_BROADCAST, &i.broadcast_mac);
    m.finish()
}

/// `RTM_NEWADDR` describing `i`'s address `a`.
fn address_message(i: &Interface, a: &Ipv4, flags: u16, seq: u32, port: u32) -> Vec<u8> {
    let mut m = Message::new(RTM_NEWADDR, flags, seq, port);
    let ifa_flags = if a.permanent { IFA_F_PERMANENT } else { 0 };
    // struct ifaddrmsg: family, prefix length, flags, scope, index.
    m.put(&[AF_INET, a.prefix, ifa_flags as u8, a.scope]);
    m.put(&i.index.to_le_bytes());
    let addr = a.address.to_be_bytes();
    m.attr(IFA_ADDRESS, &addr);
    m.attr(IFA_LOCAL, &addr);
    if i.flags & IFF_BROADCAST != 0 && a.prefix < 31 {
        let host_bits = u32::MAX >> a.prefix;
        m.attr(IFA_BROADCAST, &(a.address | host_bits).to_be_bytes());
    }
    let mut name = i.name.clone().into_bytes();
    name.push(0);
    m.attr(IFA_LABEL, &name);
    m.attr(IFA_FLAGS, &ifa_flags.to_le_bytes());
    m.finish()
}

/// `NLMSG_ERROR` for request `req` (its bytes, header included): `error`
/// 0 acknowledges it. An error carries the whole request, an
/// acknowledgement (or any answer with `cap`, NETLINK_CAP_ACK) only its
/// header.
fn error_message(req: &[u8], h: &Header, error: i32, port: u32, cap: bool) -> Vec<u8> {
    let capped = error == 0 || cap;
    let mut m = Message::new(NLMSG_ERROR, if capped { NLM_F_CAPPED } else { 0 }, h.seq, port);
    m.put(&(-error).to_le_bytes());
    m.put(if capped { &req[..NLMSG_HDRLEN] } else { req });
    m.finish()
}

fn done_message(seq: u32, port: u32) -> Vec<u8> {
    let mut m = Message::new(NLMSG_DONE, NLM_F_MULTI, seq, port);
    m.put(&0i32.to_le_bytes());
    m.finish()
}

/// Packs a dump's messages, then `NLMSG_DONE`, into datagrams of at most
/// `DUMP_DATAGRAM` bytes (a message larger than that goes alone).
fn pack(messages: Vec<Vec<u8>>, done: Vec<u8>, out: &mut Vec<Vec<u8>>) {
    let mut current: Vec<u8> = Vec::new();
    for m in messages.into_iter().chain(core::iter::once(done)) {
        if !current.is_empty() && current.len() + m.len() > DUMP_DATAGRAM {
            out.push(core::mem::take(&mut current));
        }
        current.extend_from_slice(&m);
    }
    out.push(current);
}

/// Answers one request into `out`: Ok(true) for a dump (which ends with
/// `NLMSG_DONE` and is not acknowledged), Ok(false) for an answered
/// request, or the error to report (nothing added).
fn request(req: &[u8], h: &Header, port: u32, interfaces: &[Interface], out: &mut Vec<Vec<u8>>) -> Result<bool, i32> {
    let body = &req[NLMSG_HDRLEN..];
    let dump = h.flags & NLM_F_DUMP != 0;
    match h.ty {
        RTM_GETLINK if dump => {
            // Every interface, whatever the family asked for (a link
            // belongs to none).
            let all = interfaces.iter().map(|i| link_message(i, NLM_F_MULTI, h.seq, port)).collect();
            pack(all, done_message(h.seq, port), out);
            Ok(true)
        }
        RTM_GETLINK => {
            // struct ifinfomsg, then attributes: by index, else by name.
            if body.len() < 16 {
                return Err(EINVAL);
            }
            let index = i32::from_le_bytes(body[4..8].try_into().expect("4 bytes"));
            let found = if index > 0 {
                interfaces.iter().find(|i| i.index == index as u32)
            } else {
                let name = attributes(&body[16..]).find(|&(ty, _)| ty == IFLA_IFNAME).map(|(_, d)| d.split(|&b| b == 0).next().unwrap_or(d));
                match name {
                    Some(name) => interfaces.iter().find(|i| i.name.as_bytes() == name),
                    None => return Err(EINVAL),
                }
            };
            let i = found.ok_or(ENODEV)?;
            out.push(link_message(i, 0, h.seq, port));
            Ok(false)
        }
        RTM_GETADDR if dump => {
            // struct ifaddrmsg's (or rtgenmsg's) family: IPv4 or any.
            let family = body.first().copied().unwrap_or(AF_UNSPEC);
            let all = interfaces
                .iter()
                .filter(|_| family == AF_UNSPEC || family == AF_INET)
                .filter_map(|i| i.ipv4.as_ref().map(|a| address_message(i, a, NLM_F_MULTI, h.seq, port)))
                .collect();
            pack(all, done_message(h.seq, port), out);
            Ok(true)
        }
        _ => Err(EOPNOTSUPP),
    }
}

/// The datagrams that answer `datagram`, the requests a socket with port
/// id `port` sent to the kernel's end, given the `interfaces`; `cap_ack`:
/// the socket asked for capped acknowledgements (NETLINK_CAP_ACK).
///
/// As Linux: requests are taken one after the other until one is
/// malformed; control messages and messages that are not requests only
/// get an acknowledgement if they ask for one (`NLM_F_ACK`); a dump ends
/// with `NLMSG_DONE` and is not acknowledged; any other request is
/// acknowledged if it asks, and a failed one always answers its error.
pub fn answer(datagram: &[u8], port: u32, interfaces: &[Interface], cap_ack: bool) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + NLMSG_HDRLEN <= datagram.len() {
        let h = header(&datagram[at..]);
        let len = h.len as usize;
        if len < NLMSG_HDRLEN || len > datagram.len() - at {
            break;
        }
        let req = &datagram[at..at + len];
        let result = if h.flags & NLM_F_REQUEST == 0 || h.ty < NLMSG_MIN_TYPE {
            Ok(false)
        } else {
            request(req, &h, port, interfaces, &mut out)
        };
        match result {
            Err(e) => out.push(error_message(req, &h, e, port, cap_ack)),
            Ok(false) if h.flags & NLM_F_ACK != 0 => out.push(error_message(req, &h, 0, port, cap_ack)),
            Ok(_) => {}
        }
        at += (len + 3) & !3;
    }
    out
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn interfaces() -> Vec<Interface> {
        vec![
            Interface {
                index: 1,
                name: "lo".to_string(),
                hw_type: ARPHRD_LOOPBACK,
                flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP,
                mtu: 1500,
                mac: [0; 6],
                broadcast_mac: [0; 6],
                ipv4: Some(Ipv4 { address: 0x7f00_0001, prefix: 8, scope: RT_SCOPE_HOST, permanent: true }),
            },
            Interface {
                index: 2,
                name: "eth0".to_string(),
                hw_type: ARPHRD_ETHER,
                flags: IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_LOWER_UP,
                mtu: 1500,
                mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
                broadcast_mac: [0xff; 6],
                ipv4: Some(Ipv4 { address: 0x0a00_020f, prefix: 24, scope: RT_SCOPE_UNIVERSE, permanent: false }),
            },
        ]
    }

    /// A request as musl's getifaddrs sends it: header and rtgenmsg.
    fn request(ty: u16, flags: u16, seq: u32, body: &[u8]) -> Vec<u8> {
        let mut m = Message::new(ty, flags, seq, 0);
        m.put(body);
        let mut v = m.0;
        let len = v.len() as u32;
        v[0..4].copy_from_slice(&len.to_le_bytes());
        v
    }

    /// The messages of a datagram: (header, payload).
    fn messages(d: &[u8]) -> Vec<(Header, Vec<u8>, u32)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + NLMSG_HDRLEN <= d.len() {
            let h = header(&d[at..]);
            let port = u32::from_le_bytes(d[at + 12..at + 16].try_into().unwrap());
            out.push((h, d[at + NLMSG_HDRLEN..at + h.len as usize].to_vec(), port));
            assert_eq!(h.len % 4, 0, "messages are aligned");
            at += h.len as usize;
        }
        assert_eq!(at, d.len());
        out
    }

    fn attr(payload: &[u8], fixed: usize, ty: u16) -> Option<Vec<u8>> {
        attributes(&payload[fixed..]).find(|&(t, _)| t == ty).map(|(_, d)| d.to_vec())
    }

    #[test]
    fn link_dump() {
        let req = request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 7, &[AF_UNSPEC]);
        let out = answer(&req, 4242, &interfaces(), false);
        assert_eq!(out.len(), 1);
        let msgs = messages(&out[0]);
        assert_eq!(msgs.len(), 3);
        for (h, _, port) in &msgs {
            assert_eq!((h.seq, *port, h.flags & NLM_F_MULTI), (7, 4242, NLM_F_MULTI));
        }
        let (h, p, _) = &msgs[1];
        assert_eq!(h.ty, RTM_NEWLINK);
        assert_eq!(u16::from_le_bytes([p[2], p[3]]), ARPHRD_ETHER);
        assert_eq!(i32::from_le_bytes(p[4..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(p[8..12].try_into().unwrap()), IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_LOWER_UP);
        assert_eq!(attr(p, 16, IFLA_IFNAME).unwrap(), b"eth0\0");
        assert_eq!(attr(p, 16, IFLA_ADDRESS).unwrap(), [0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        assert_eq!(attr(p, 16, IFLA_MTU).unwrap(), 1500u32.to_le_bytes());
        assert_eq!(attr(p, 16, IFLA_OPERSTATE).unwrap(), [IF_OPER_UP]);
        assert_eq!(msgs[2].0.ty, NLMSG_DONE);
        assert_eq!(msgs[2].1, 0i32.to_le_bytes());
    }

    #[test]
    fn address_dump() {
        let req = request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 8, &[AF_UNSPEC]);
        let msgs = messages(&answer(&req, 1, &interfaces(), false)[0]);
        assert_eq!(msgs.len(), 3);
        let (h, p, _) = &msgs[0];
        assert_eq!(h.ty, RTM_NEWADDR);
        assert_eq!(&p[..4], &[AF_INET, 8, IFA_F_PERMANENT as u8, RT_SCOPE_HOST]);
        assert_eq!(attr(p, 8, IFA_LOCAL).unwrap(), [127, 0, 0, 1]);
        assert_eq!(attr(p, 8, IFA_BROADCAST), None, "the loopback has no broadcast address");
        assert_eq!(attr(p, 8, IFA_LABEL).unwrap(), b"lo\0");
        let (_, p, _) = &msgs[1];
        assert_eq!(&p[..4], &[AF_INET, 24, 0, RT_SCOPE_UNIVERSE]);
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 2);
        assert_eq!(attr(p, 8, IFA_ADDRESS).unwrap(), [10, 0, 2, 15]);
        assert_eq!(attr(p, 8, IFA_BROADCAST).unwrap(), [10, 0, 2, 255]);
        // IPv6 only: an empty dump.
        let req = request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 9, &[10]);
        let msgs = messages(&answer(&req, 1, &interfaces(), false)[0]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0.ty, NLMSG_DONE);
    }

    #[test]
    fn one_link() {
        let mut body = vec![0u8; 16];
        body[4..8].copy_from_slice(&1i32.to_le_bytes());
        let req = request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 3, &body);
        let out = answer(&req, 5, &interfaces(), false);
        assert_eq!(out.len(), 2, "the link, then the acknowledgement");
        let link = messages(&out[0]);
        assert_eq!((link[0].0.ty, link[0].0.flags), (RTM_NEWLINK, 0));
        assert_eq!(attr(&link[0].1, 16, IFLA_IFNAME).unwrap(), b"lo\0");
        let ack = messages(&out[1]);
        assert_eq!((ack[0].0.ty, ack[0].0.flags), (NLMSG_ERROR, NLM_F_CAPPED));
        assert_eq!(&ack[0].1[..4], &0i32.to_le_bytes());
        assert_eq!(ack[0].1.len(), 4 + NLMSG_HDRLEN);
        // By name.
        let mut m = Message::new(RTM_GETLINK, NLM_F_REQUEST, 4, 0);
        m.put(&[0u8; 16]);
        m.attr(IFLA_IFNAME, b"eth0\0");
        let out = answer(&m.finish(), 5, &interfaces(), false);
        assert_eq!(i32::from_le_bytes(messages(&out[0])[0].1[4..8].try_into().unwrap()), 2);
        // No such interface.
        body[4..8].copy_from_slice(&9i32.to_le_bytes());
        let req = request(RTM_GETLINK, NLM_F_REQUEST, 5, &body);
        let out = answer(&req, 5, &interfaces(), false);
        let err = messages(&out[0]);
        assert_eq!(err[0].0.ty, NLMSG_ERROR);
        assert_eq!(&err[0].1[..4], &(-ENODEV).to_le_bytes());
        assert_eq!(&err[0].1[4..], &req[..], "an error carries the request");
    }

    #[test]
    fn refused_and_ignored() {
        // A request of a type it does not implement.
        let req = request(24, NLM_F_REQUEST, 1, &[0u8; 12]);
        let out = answer(&req, 5, &interfaces(), true);
        let err = messages(&out[0]);
        assert_eq!(&err[0].1[..4], &(-EOPNOTSUPP).to_le_bytes());
        assert_eq!(err[0].1.len(), 4 + NLMSG_HDRLEN, "capped by NETLINK_CAP_ACK");
        // Not a request, a control message: nothing, unless acknowledged.
        assert!(answer(&request(RTM_GETLINK, 0, 1, &[0]), 5, &interfaces(), false).is_empty());
        assert!(answer(&request(NLMSG_NOOP, NLM_F_REQUEST, 1, &[]), 5, &interfaces(), false).is_empty());
        assert_eq!(answer(&request(NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 1, &[]), 5, &interfaces(), false).len(), 1);
        // Malformed: a length beyond the datagram ends the processing.
        let mut bad = request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, &[0]);
        bad[0] = 200;
        assert!(answer(&bad, 5, &interfaces(), false).is_empty());
        // A short GETLINK.
        let out = answer(&request(RTM_GETLINK, NLM_F_REQUEST, 1, &[0]), 5, &interfaces(), false);
        assert_eq!(&messages(&out[0])[0].1[..4], &(-EINVAL).to_le_bytes());
    }

    #[test]
    fn two_requests_in_one_datagram() {
        let mut d = request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, &[0, 0, 0, 0]);
        d.extend(request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 2, &[0, 0, 0, 0]));
        let out = answer(&d, 5, &interfaces(), false);
        assert_eq!(out.len(), 2);
        assert_eq!(messages(&out[1])[0].0.seq, 2);
    }

    #[test]
    fn large_dumps_split() {
        let many: Vec<Interface> = (1..=100)
            .map(|n| Interface { index: n, name: alloc::format!("eth{n}"), ..interfaces()[1].clone() })
            .collect();
        let out = answer(&request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, &[0]), 5, &many, false);
        assert!(out.len() > 1);
        let total: usize = out.iter().map(|d| messages(d).len()).sum();
        assert_eq!(total, 101);
        assert!(out.iter().all(|d| d.len() <= DUMP_DATAGRAM));
        assert_eq!(messages(out.last().unwrap()).last().unwrap().0.ty, NLMSG_DONE);
    }
}
