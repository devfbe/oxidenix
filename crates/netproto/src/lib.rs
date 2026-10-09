//! Message format between the kernel's socket layer and the network
//! server (netd). The framing is the one of `fsproto` (40-byte request
//! header, 56-byte response header, little-endian); only the operations
//! differ.
//!
//! Addresses are IPv4 addresses as `u32` in host order (10.0.2.15 is
//! 0x0a00020f) and ports in host order.

#![no_std]

pub use fsproto::{decode_response, encode_response, Response, MAX_DATA, MAX_MESSAGE, REQUEST_HEADER, RESPONSE_HEADER};

extern crate alloc;

use alloc::vec::Vec;

/// Operations. Arguments are listed as (a0, a1, a2, a3) + payload, results
/// as (v0..v5) + payload. Calls that would block are answered when they
/// can complete, unless `NONBLOCK` is set in their flags (then `EAGAIN`).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// (kind) -> v0 = handle
    Socket = 1,
    /// (handle, address, port)
    Bind = 2,
    /// (handle, backlog)
    Listen = 3,
    /// (handle, flags) -> v0 = new handle, v1 = peer address, v2 = peer port
    Accept = 4,
    /// (handle, address, port, flags); with NONBLOCK: EINPROGRESS
    Connect = 5,
    /// (handle, flags, address, port) + data -> status = bytes sent;
    /// address 0 sends to the connected peer
    Send = 6,
    /// (handle, max length, flags) -> status = bytes, payload = data,
    /// v0 = sender address, v1 = sender port
    Recv = 7,
    /// (handle, how: 0 read, 1 write, 2 both)
    Shutdown = 8,
    /// (handle, events) -> v0 = ready events (poll bits)
    Poll = 9,
    /// (handle, peer) -> v0 = address, v1 = port (local or peer)
    Name = 10,
    /// (handle): the last descriptor is gone. Sent without waiting.
    Close = 11,
    /// (ipc request id): the caller of that request was interrupted by a
    /// signal; drop it. Sent without waiting.
    Cancel = 12,
    /// (handle) -> status = pending error (SO_ERROR), cleared
    TakeError = 13,
    /// () -> v0 = address, v1 = prefix length, v2 = gateway, v3 = DNS server,
    /// payload = 6-byte MAC
    Info = 14,
    /// () -> payload = the network interfaces, `Link` records in the order
    /// of their indexes
    Links = 15,
}

impl Op {
    pub fn from_u32(v: u32) -> Option<Op> {
        use Op::*;
        [Socket, Bind, Listen, Accept, Connect, Send, Recv, Shutdown, Poll, Name, Close, Cancel, TakeError, Info, Links]
            .into_iter()
            .find(|op| *op as u32 == v)
    }
}

pub const KIND_TCP: u64 = 1;
pub const KIND_UDP: u64 = 2;
/// Raw ICMP (SOCK_RAW, IPPROTO_ICMP): sends ICMP messages, receives whole
/// IPv4 packets carrying ICMP, as Linux does.
pub const KIND_RAW_ICMP: u64 = 3;

/// Flag: answer EAGAIN instead of waiting.
pub const NONBLOCK: u64 = 1;
/// Flag (Recv): leave the data in the socket.
pub const PEEK: u64 = 2;

pub const POLLIN: u64 = 0x1;
pub const POLLOUT: u64 = 0x4;
pub const POLLERR: u64 = 0x8;
pub const POLLHUP: u64 = 0x10;

/// A link of the loopback kind: traffic to the host's own addresses.
pub const LINK_LOOPBACK: u16 = 1;
/// An Ethernet link (the network card).
pub const LINK_ETHERNET: u16 = 2;

/// Link state: configured up (it sends and receives).
pub const LINK_UP: u16 = 1;
/// Link state: the medium is there (a carrier).
pub const LINK_RUNNING: u16 = 2;

/// A network interface as netd describes it (`Op::Links`). netd speaks
/// IPv4 with one address per interface; the names are the Linux server's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Link {
    /// Its number, from 1, stable while netd runs.
    pub index: u32,
    /// `LINK_LOOPBACK` or `LINK_ETHERNET`.
    pub kind: u16,
    /// `LINK_UP`, `LINK_RUNNING`.
    pub state: u16,
    /// The largest IP packet it carries.
    pub mtu: u32,
    /// Its hardware address (zero for the loopback).
    pub mac: [u8; 6],
    /// The IPv4 address's prefix length (with `address` 0: none).
    pub prefix: u8,
    /// Its IPv4 address in host order, 0 for none (no DHCP lease yet).
    pub address: u32,
}

impl Link {
    /// The size of one encoded record.
    pub const SIZE: usize = 24;

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut r = [0u8; Self::SIZE];
        r[0..4].copy_from_slice(&self.index.to_le_bytes());
        r[4..6].copy_from_slice(&self.kind.to_le_bytes());
        r[6..8].copy_from_slice(&self.state.to_le_bytes());
        r[8..12].copy_from_slice(&self.mtu.to_le_bytes());
        r[12..18].copy_from_slice(&self.mac);
        r[18] = self.prefix;
        r[20..24].copy_from_slice(&self.address.to_le_bytes());
        r
    }

    /// The records of an `Op::Links` payload; a partial record at the end
    /// is ignored.
    pub fn decode_all(payload: &[u8]) -> impl Iterator<Item = Link> + '_ {
        payload.chunks_exact(Self::SIZE).map(|r| Link {
            index: u32::from_le_bytes(r[0..4].try_into().expect("4 bytes")),
            kind: u16::from_le_bytes([r[4], r[5]]),
            state: u16::from_le_bytes([r[6], r[7]]),
            mtu: u32::from_le_bytes(r[8..12].try_into().expect("4 bytes")),
            mac: r[12..18].try_into().expect("6 bytes"),
            prefix: r[18],
            address: u32::from_le_bytes(r[20..24].try_into().expect("4 bytes")),
        })
    }
}

pub fn encode_request(op: Op, args: [u64; 4], payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(REQUEST_HEADER + payload.len());
    m.extend_from_slice(&(op as u32).to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    for a in args {
        m.extend_from_slice(&a.to_le_bytes());
    }
    m.extend_from_slice(payload);
    m
}

pub struct Request<'a> {
    pub op: Option<Op>,
    pub args: [u64; 4],
    pub payload: &'a [u8],
}

pub fn decode_request(m: &[u8]) -> Option<Request<'_>> {
    let r = fsproto::decode_request(m)?;
    let op = u32::from_le_bytes(m[0..4].try_into().ok()?);
    Some(Request { op: Op::from_u32(op), args: r.args, payload: r.payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let links = [
            Link { index: 1, kind: LINK_LOOPBACK, state: LINK_UP | LINK_RUNNING, mtu: 1500, mac: [0; 6], prefix: 8, address: 0x7f00_0001 },
            Link { index: 2, kind: LINK_ETHERNET, state: LINK_UP, mtu: 1500, mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56], prefix: 24, address: 0x0a00_020f },
        ];
        let mut payload: Vec<u8> = links.iter().flat_map(|l| l.encode()).collect();
        payload.push(0xff);
        let back: Vec<Link> = Link::decode_all(&payload).collect();
        assert_eq!(back, links);
    }
}
