//! Microsoft's Remote NDIS (RNDIS 1.0 over USB) as a host speaks it:
//! control messages travel as CDC encapsulated commands and responses on
//! the control endpoint, frames as `PACKET_MSG`s on the bulk endpoints
//! (several per transfer where the device says so). All fields are
//! little-endian 32-bit words.

use alloc::vec::Vec;

/// CDC's requests that carry the control messages.
pub const SEND_ENCAPSULATED_COMMAND: u8 = 0x00;
pub const GET_ENCAPSULATED_RESPONSE: u8 = 0x01;
/// The most a response may be: what usbd asks for (Linux asks 1025 bytes;
/// a query of a long list could need more).
pub const RESPONSE_BYTES: usize = 4096;

pub const PACKET_MSG: u32 = 0x0000_0001;
pub const INITIALIZE_MSG: u32 = 0x0000_0002;
pub const HALT_MSG: u32 = 0x0000_0003;
pub const QUERY_MSG: u32 = 0x0000_0004;
pub const SET_MSG: u32 = 0x0000_0005;
pub const RESET_MSG: u32 = 0x0000_0006;
pub const INDICATE_STATUS_MSG: u32 = 0x0000_0007;
pub const KEEPALIVE_MSG: u32 = 0x0000_0008;
/// A completion's type: its request's with this bit.
pub const COMPLETION: u32 = 0x8000_0000;

pub const STATUS_SUCCESS: u32 = 0;
pub const STATUS_MEDIA_CONNECT: u32 = 0x4001_000b;
pub const STATUS_MEDIA_DISCONNECT: u32 = 0x4001_000c;

pub const OID_GEN_MAXIMUM_FRAME_SIZE: u32 = 0x0001_0106;
pub const OID_GEN_CURRENT_PACKET_FILTER: u32 = 0x0001_010e;
pub const OID_GEN_MEDIA_CONNECT_STATUS: u32 = 0x0001_0114;
pub const OID_802_3_PERMANENT_ADDRESS: u32 = 0x0101_0101;

/// `OID_GEN_CURRENT_PACKET_FILTER`'s bits.
pub const PACKET_TYPE_DIRECTED: u32 = 0x01;
pub const PACKET_TYPE_MULTICAST: u32 = 0x02;
pub const PACKET_TYPE_ALL_MULTICAST: u32 = 0x04;
pub const PACKET_TYPE_BROADCAST: u32 = 0x08;

/// `OID_GEN_MEDIA_CONNECT_STATUS`'s values.
pub const MEDIA_CONNECTED: u32 = 0;

/// Bytes of a `PACKET_MSG`'s header.
pub const PACKET_HEADER: usize = 44;
/// The transfer size usbd announces in `INITIALIZE_MSG`: what it takes in
/// one bulk transfer.
pub const MAX_TRANSFER: u32 = 16384;

fn put(out: &mut Vec<u8>, words: &[u32]) {
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
}

fn word(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// `INITIALIZE_MSG`: version 1.0, at most `max_transfer` bytes per
/// transfer from the device.
pub fn initialize(request: u32, max_transfer: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(24);
    put(&mut m, &[INITIALIZE_MSG, 24, request, 1, 0, max_transfer]);
    m
}

/// `QUERY_MSG` of `oid`, without an input buffer.
pub fn query(request: u32, oid: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(28);
    put(&mut m, &[QUERY_MSG, 28, request, oid, 0, 0, 0]);
    m
}

/// `SET_MSG` of `oid` to `value` (the buffer right after the header: its
/// offset counts from the request id, at byte 8).
pub fn set(request: u32, oid: u32, value: &[u8]) -> Vec<u8> {
    let len = 28 + value.len() as u32;
    let mut m = Vec::with_capacity(len as usize);
    put(&mut m, &[SET_MSG, len, request, oid, value.len() as u32, 20, 0]);
    m.extend_from_slice(value);
    m
}

/// `KEEPALIVE_CMPLT` for the device's `KEEPALIVE_MSG` `request`.
pub fn keepalive_complete(request: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(16);
    put(&mut m, &[KEEPALIVE_MSG | COMPLETION, 16, request, STATUS_SUCCESS]);
    m
}

/// What `INITIALIZE_CMPLT` said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Initialized {
    pub max_packets_per_transfer: u32,
    /// The most bytes the device takes in one transfer from the host.
    pub max_transfer: u32,
    /// Packets in one transfer start at multiples of 2^this.
    pub alignment: u32,
}

/// A message from the device (a response to a command, or one of its own).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Initialized { request: u32, status: u32, info: Initialized },
    /// A query's answer: the information buffer.
    Queried { request: u32, status: u32, info: Vec<u8> },
    Set { request: u32, status: u32 },
    Reset { status: u32 },
    /// The device's word on its status (`STATUS_MEDIA_CONNECT`, ...).
    Indication { status: u32 },
    /// The device checks the host is alive: it wants a `KEEPALIVE_CMPLT`.
    KeepAlive { request: u32 },
    /// The answer to a keep-alive of the host's.
    KeptAlive { request: u32, status: u32 },
}

impl Response {
    /// One message (its length checked against the bytes given); None for
    /// a malformed or unknown one.
    pub fn parse(b: &[u8]) -> Option<Response> {
        let kind = word(b, 0)?;
        let len = word(b, 4)? as usize;
        if len < 8 || len > b.len() {
            return None;
        }
        let b = &b[..len];
        Some(match kind {
            k if k == INITIALIZE_MSG | COMPLETION => {
                if len < 52 {
                    return None;
                }
                Response::Initialized {
                    request: word(b, 8)?,
                    status: word(b, 12)?,
                    info: Initialized { max_packets_per_transfer: word(b, 32)?, max_transfer: word(b, 36)?, alignment: word(b, 40)? },
                }
            }
            k if k == QUERY_MSG | COMPLETION => {
                let (request, status) = (word(b, 8)?, word(b, 12)?);
                let (info_len, offset) = (word(b, 16)? as usize, word(b, 20)? as usize);
                // (The offset counts from the request id, at byte 8.)
                let info = if info_len == 0 { &[][..] } else { b.get(8usize.checked_add(offset)?..8 + offset + info_len)? };
                Response::Queried { request, status, info: info.to_vec() }
            }
            k if k == SET_MSG | COMPLETION => Response::Set { request: word(b, 8)?, status: word(b, 12)? },
            k if k == RESET_MSG | COMPLETION => Response::Reset { status: word(b, 8)? },
            INDICATE_STATUS_MSG => Response::Indication { status: word(b, 8)? },
            KEEPALIVE_MSG => Response::KeepAlive { request: word(b, 8)? },
            k if k == KEEPALIVE_MSG | COMPLETION => Response::KeptAlive { request: word(b, 8)?, status: word(b, 12)? },
            _ => return None,
        })
    }
}

/// Whether an interrupt-endpoint notification of an RNDIS function says a
/// response waits: its own 8 bytes (the 32-bit 1, a reserved word), or
/// CDC's RESPONSE_AVAILABLE some devices send instead.
pub fn is_response_available(b: &[u8]) -> bool {
    word(b, 0) == Some(1) || matches!(crate::cdc::Notification::parse(b), crate::cdc::Notification::ResponseAvailable)
}

/// The `PACKET_MSG` header for a frame of `len` bytes (the frame follows
/// it at once).
pub fn packet_header(len: usize) -> [u8; PACKET_HEADER] {
    let mut h = [0u8; PACKET_HEADER];
    h[0..4].copy_from_slice(&PACKET_MSG.to_le_bytes());
    h[4..8].copy_from_slice(&((PACKET_HEADER + len) as u32).to_le_bytes());
    // The data's offset counts from byte 8.
    h[8..12].copy_from_slice(&((PACKET_HEADER - 8) as u32).to_le_bytes());
    h[12..16].copy_from_slice(&(len as u32).to_le_bytes());
    h
}

/// The frames of one bulk IN transfer (`PACKET_MSG`s back to back):
/// each a range of `b`. A malformed message ends the transfer (what came
/// before it stands); messages of other kinds are skipped.
pub fn packets(b: &[u8]) -> Packets<'_> {
    Packets { b, at: 0 }
}

pub struct Packets<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Packets<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let rest = self.b.get(self.at..)?;
            let (kind, len) = (word(rest, 0)?, word(rest, 4)? as usize);
            if len < 8 || len > rest.len() {
                self.at = self.b.len();
                return None;
            }
            self.at += len;
            if kind != PACKET_MSG {
                continue;
            }
            let msg = &rest[..len];
            let (offset, data_len) = (word(msg, 8)? as usize, word(msg, 12)? as usize);
            match 8usize.checked_add(offset).and_then(|start| Some((start, start.checked_add(data_len)?))) {
                Some((start, end)) if len >= PACKET_HEADER && end <= len && data_len > 0 => return Some(&msg[start..end]),
                _ => {
                    self.at = self.b.len();
                    return None;
                }
            }
        }
    }
}
