//! Standard descriptors (USB 2.0, 9.6; USB 3.2, 9.6): the device's, a
//! configuration's with everything that follows it (interfaces, their
//! alternate settings and endpoints, the class-specific descriptors in
//! between, interface associations), strings. A device writes these: the
//! parsers take any bytes, check every length against what is there, and
//! keep only what they understood.

use alloc::string::String;
use alloc::vec::Vec;

pub const DEVICE: u8 = 1;
pub const CONFIGURATION: u8 = 2;
pub const STRING: u8 = 3;
pub const INTERFACE: u8 = 4;
pub const ENDPOINT: u8 = 5;
pub const INTERFACE_ASSOCIATION: u8 = 11;
pub const BOS: u8 = 15;
/// Class-specific interface descriptors (CDC's functional descriptors).
pub const CS_INTERFACE: u8 = 0x24;
pub const SS_ENDPOINT_COMPANION: u8 = 48;
/// Hub descriptors (USB 2.0, 11.23.2.1; USB 3: 10.15.2.1).
pub const HUB: u8 = 0x29;
pub const SS_HUB: u8 = 0x2a;

/// The device descriptor (18 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    pub usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// The control endpoint's packet size: bytes, or for USB 3 the
    /// exponent (512 = 2^9).
    pub max_packet0: u8,
    pub vendor: u16,
    pub product: u16,
    pub release: u16,
    pub manufacturer: u8,
    pub product_string: u8,
    pub serial: u8,
    pub configurations: u8,
}

impl Device {
    pub const SIZE: usize = 18;

    pub fn parse(b: &[u8]) -> Option<Device> {
        if b.len() < Self::SIZE || (b[0] as usize) < Self::SIZE || b[1] != DEVICE {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Some(Device {
            usb: u16_at(2),
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet0: b[7],
            vendor: u16_at(8),
            product: u16_at(10),
            release: u16_at(12),
            manufacturer: b[14],
            product_string: b[15],
            serial: b[16],
            configurations: b[17],
        })
    }

    /// The control endpoint's packet size in bytes (None if the value is
    /// not one USB allows for the speed).
    pub fn max_packet0_bytes(&self, speed: crate::Speed) -> Option<u16> {
        if speed.is_super() {
            return (self.max_packet0 == 9).then_some(512);
        }
        match (speed, self.max_packet0) {
            (crate::Speed::Low, 8) => Some(8),
            (crate::Speed::Full, 8 | 16 | 32 | 64) => Some(self.max_packet0 as u16),
            (crate::Speed::High, 64) => Some(64),
            _ => None,
        }
    }
}

/// An endpoint of an interface's alternate setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// Number and direction (bit 7: IN).
    pub address: u8,
    /// Transfer type in bits 1:0 (`CONTROL` .. `INTERRUPT`).
    pub attributes: u8,
    /// Bytes per packet in bits 10:0; high speed's extra transactions per
    /// microframe in bits 12:11.
    pub max_packet: u16,
    pub interval: u8,
    /// USB 3: the companion's bMaxBurst (packets per burst, less one).
    pub max_burst: u8,
}

pub const CONTROL: u8 = 0;
pub const ISOCHRONOUS: u8 = 1;
pub const BULK: u8 = 2;
pub const INTERRUPT: u8 = 3;

impl Endpoint {
    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    pub fn kind(&self) -> u8 {
        self.attributes & 0b11
    }

    /// Bytes per packet.
    pub fn packet_size(&self) -> u16 {
        self.max_packet & 0x7ff
    }

    /// High speed's additional transactions per microframe (0-2).
    pub fn mult(&self) -> u8 {
        (self.max_packet >> 11 & 0b11) as u8
    }
}

/// A class-specific descriptor inside an interface (CDC's functional
/// descriptors): its type, subtype and the bytes after them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Functional {
    pub kind: u8,
    pub subtype: u8,
    pub data: Vec<u8>,
}

/// One alternate setting of an interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub alternate: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub string: u8,
    pub endpoints: Vec<Endpoint>,
    pub functional: Vec<Functional>,
}

impl Interface {
    /// The first endpoint of this transfer type and direction.
    pub fn endpoint(&self, kind: u8, input: bool) -> Option<Endpoint> {
        self.endpoints.iter().copied().find(|e| e.kind() == kind && e.is_in() == input)
    }

    /// The first functional descriptor of `subtype` (CS_INTERFACE).
    pub fn functional(&self, subtype: u8) -> Option<&Functional> {
        self.functional.iter().find(|f| f.kind == CS_INTERFACE && f.subtype == subtype)
    }
}

/// An interface association: interfaces `first..first + count` are one
/// function of this class (a composite device's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Association {
    pub first: u8,
    pub count: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// A configuration with everything its descriptor set held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub value: u8,
    pub string: u8,
    pub attributes: u8,
    /// In 2 mA (USB 2) or 8 mA (USB 3) units.
    pub max_power: u8,
    /// Every alternate setting of every interface, in descriptor order.
    pub interfaces: Vec<Interface>,
    pub associations: Vec<Association>,
}

/// Most alternate settings and endpoints kept of one configuration (a
/// device that claims more is cut off: what it would add is ignored).
const MAX_INTERFACES: usize = 64;
const MAX_ENDPOINTS: usize = 32;
const MAX_FUNCTIONAL: usize = 16;

impl Configuration {
    pub const HEADER: usize = 9;

    /// The total length the configuration descriptor's first 9 bytes
    /// announce.
    pub fn total_length(header: &[u8]) -> Option<u16> {
        (header.len() >= Self::HEADER && header[0] as usize >= Self::HEADER && header[1] == CONFIGURATION).then(|| u16::from_le_bytes([header[2], header[3]]))
    }

    /// The whole descriptor set (wTotalLength bytes, or what the device
    /// sent of them). Descriptors after a malformed one (a length of 0 or
    /// one beyond the end) are ignored; so are unknown kinds.
    pub fn parse(b: &[u8]) -> Option<Configuration> {
        if Self::total_length(b).is_none() {
            return None;
        }
        let mut config = Configuration { value: b[5], string: b[6], attributes: b[7], max_power: b[8], interfaces: Vec::new(), associations: Vec::new() };
        let total = (u16::from_le_bytes([b[2], b[3]]) as usize).min(b.len());
        let mut at = b[0] as usize;
        while at + 2 <= total {
            let len = b[at] as usize;
            if len < 2 || at + len > total {
                break;
            }
            let d = &b[at..at + len];
            match d[1] {
                INTERFACE if len >= 9 => {
                    if config.interfaces.len() < MAX_INTERFACES {
                        config.interfaces.push(Interface {
                            number: d[2],
                            alternate: d[3],
                            class: d[5],
                            subclass: d[6],
                            protocol: d[7],
                            string: d[8],
                            endpoints: Vec::new(),
                            functional: Vec::new(),
                        });
                    }
                }
                ENDPOINT if len >= 7 => {
                    if let Some(i) = config.interfaces.last_mut().filter(|i| i.endpoints.len() < MAX_ENDPOINTS) {
                        i.endpoints.push(Endpoint { address: d[2], attributes: d[3], max_packet: u16::from_le_bytes([d[4], d[5]]), interval: d[6], max_burst: 0 });
                    }
                }
                SS_ENDPOINT_COMPANION if len >= 6 => {
                    if let Some(e) = config.interfaces.last_mut().and_then(|i| i.endpoints.last_mut()) {
                        e.max_burst = d[2].min(15);
                    }
                }
                INTERFACE_ASSOCIATION if len >= 8 => {
                    if config.associations.len() < MAX_INTERFACES {
                        config.associations.push(Association { first: d[2], count: d[3], class: d[4], subclass: d[5], protocol: d[6] });
                    }
                }
                CS_INTERFACE if len >= 3 => {
                    if let Some(i) = config.interfaces.last_mut().filter(|i| i.functional.len() < MAX_FUNCTIONAL) {
                        i.functional.push(Functional { kind: d[1], subtype: d[2], data: d[3..].to_vec() });
                    }
                }
                _ => {}
            }
            at += len;
        }
        Some(config)
    }

    /// Interface `number`'s alternate setting `alternate`.
    pub fn interface(&self, number: u8, alternate: u8) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.number == number && i.alternate == alternate)
    }

    /// The numbers of the interfaces, each once, in order of appearance.
    pub fn interface_numbers(&self) -> impl Iterator<Item = u8> + '_ {
        self.interfaces.iter().enumerate().filter(|(n, i)| !self.interfaces[..*n].iter().any(|o| o.number == i.number)).map(|(_, i)| i.number)
    }
}

/// The languages a device's string descriptor 0 lists.
pub fn languages(b: &[u8]) -> Vec<u16> {
    if b.len() < 2 || b[1] != STRING {
        return Vec::new();
    }
    let len = (b[0] as usize).min(b.len());
    b[2..len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

/// A string descriptor's text (UTF-16LE; what does not decode becomes
/// U+FFFD).
pub fn string(b: &[u8]) -> Option<String> {
    if b.len() < 2 || b[1] != STRING || (b[0] as usize) < 2 {
        return None;
    }
    let len = (b[0] as usize).min(b.len());
    let units = b[2..len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    Some(char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect())
}
