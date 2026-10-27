//! The Communications Device Class (CDC 1.2) as Ethernet functions use
//! it: the class codes, the functional descriptors (union, Ethernet
//! networking), the ECM requests and the notifications every CDC
//! Ethernet function (ECM, NCM, and RNDIS's control interface) sends on
//! its interrupt endpoint.

use crate::descriptor::Interface;

/// Interface classes.
pub const CLASS_COMM: u8 = 0x02;
pub const CLASS_DATA: u8 = 0x0a;
/// Communications subclasses.
pub const SUBCLASS_ACM: u8 = 0x02;
pub const SUBCLASS_ECM: u8 = 0x06;
pub const SUBCLASS_NCM: u8 = 0x0d;
/// The data interface's protocol for NCM (NTBs).
pub const PROTOCOL_NCM_DATA: u8 = 0x01;

/// Functional descriptor subtypes.
pub const FUNC_HEADER: u8 = 0x00;
pub const FUNC_UNION: u8 = 0x06;
pub const FUNC_ETHERNET: u8 = 0x0f;
pub const FUNC_NCM: u8 = 0x1a;

/// ECM's (and NCM's) request: which frames the function passes on.
pub const SET_ETHERNET_PACKET_FILTER: u8 = 0x43;
pub const PACKET_TYPE_PROMISCUOUS: u16 = 1 << 0;
pub const PACKET_TYPE_ALL_MULTICAST: u16 = 1 << 1;
pub const PACKET_TYPE_DIRECTED: u16 = 1 << 2;
pub const PACKET_TYPE_BROADCAST: u16 = 1 << 3;
pub const PACKET_TYPE_MULTICAST: u16 = 1 << 4;

/// Notifications (CDC 1.2, 6.3; PSTN and ECM).
pub const NETWORK_CONNECTION: u8 = 0x00;
pub const RESPONSE_AVAILABLE: u8 = 0x01;
pub const CONNECTION_SPEED_CHANGE: u8 = 0x2a;
/// A notification's header: bmRequestType, bNotification, wValue, wIndex,
/// wLength.
pub const NOTIFICATION_HEADER: usize = 8;

/// The union functional descriptor: the control interface and the
/// interfaces it controls (the data interface first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Union {
    pub control: u8,
    pub subordinate: u8,
}

pub fn union(control: &Interface) -> Option<Union> {
    let f = control.functional(FUNC_UNION)?;
    (f.data.len() >= 2).then(|| Union { control: f.data[0], subordinate: f.data[1] })
}

/// The Ethernet networking functional descriptor (CDC ECM 1.2, 5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ethernet {
    /// The string descriptor that holds the MAC address (12 hex digits).
    pub mac_string: u8,
    /// The largest frame the function takes, header included.
    pub max_segment: u16,
}

pub fn ethernet(control: &Interface) -> Option<Ethernet> {
    let f = control.functional(FUNC_ETHERNET)?;
    (f.data.len() >= 7).then(|| Ethernet { mac_string: f.data[0], max_segment: u16::from_le_bytes([f.data[5], f.data[6]]) })
}

/// A MAC address from its string descriptor's text (12 hex digits, as
/// iMACAddress holds it); None for anything else.
pub fn mac_from_string(text: &str) -> Option<[u8; 6]> {
    let digits = text.trim();
    if digits.len() != 12 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(mac)
}

/// What a notification on the interrupt endpoint said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification {
    /// The medium is connected (true) or not.
    Connection(bool),
    /// A response to an encapsulated command waits (RNDIS).
    ResponseAvailable,
    /// The link's speeds in bits per second: downstream, upstream.
    Speed(u32, u32),
    /// One of no interest, or malformed.
    Other,
}

impl Notification {
    pub fn parse(b: &[u8]) -> Notification {
        // (RNDIS functions send RESPONSE_AVAILABLE as 8 bytes of their own:
        // the 32-bit 1 and a reserved word; `rndis::is_response_available`.)
        if b.len() < NOTIFICATION_HEADER || b[0] != 0xa1 {
            return Notification::Other;
        }
        let value = u16::from_le_bytes([b[2], b[3]]);
        match b[1] {
            NETWORK_CONNECTION => Notification::Connection(value != 0),
            RESPONSE_AVAILABLE => Notification::ResponseAvailable,
            CONNECTION_SPEED_CHANGE if b.len() >= NOTIFICATION_HEADER + 8 => {
                let at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"));
                Notification::Speed(at(8), at(12))
            }
            _ => Notification::Other,
        }
    }
}
