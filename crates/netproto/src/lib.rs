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
}

impl Op {
    pub fn from_u32(v: u32) -> Option<Op> {
        use Op::*;
        [Socket, Bind, Listen, Accept, Connect, Send, Recv, Shutdown, Poll, Name, Close, Cancel, TakeError, Info]
            .into_iter()
            .find(|op| *op as u32 == v)
    }
}

pub const KIND_TCP: u64 = 1;
pub const KIND_UDP: u64 = 2;

/// Flag: answer EAGAIN instead of waiting.
pub const NONBLOCK: u64 = 1;
/// Flag (Recv): leave the data in the socket.
pub const PEEK: u64 = 2;

pub const POLLIN: u64 = 0x1;
pub const POLLOUT: u64 = 0x4;
pub const POLLERR: u64 = 0x8;
pub const POLLHUP: u64 = 0x10;

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
