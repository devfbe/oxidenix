//! Hubs (USB 2.0, 11.23-11.24; USB 3.2, 10.14-10.16): the hub descriptor,
//! a port's status, the class requests on ports, and the status change
//! bitmap of the hub's interrupt endpoint.

use crate::descriptor::{HUB, SS_HUB};
use crate::{SetupPacket, Speed, DIR_IN, RECIPIENT_DEVICE, RECIPIENT_OTHER, TYPE_CLASS};

/// The hub class.
pub const CLASS_HUB: u8 = 0x09;

/// Port features (SET_FEATURE / CLEAR_FEATURE to a port).
pub const PORT_ENABLE: u16 = 1;
pub const PORT_RESET: u16 = 4;
pub const PORT_POWER: u16 = 8;
pub const C_PORT_CONNECTION: u16 = 16;
pub const C_PORT_ENABLE: u16 = 17;
pub const C_PORT_SUSPEND: u16 = 18;
pub const C_PORT_OVER_CURRENT: u16 = 19;
pub const C_PORT_RESET: u16 = 20;
/// USB 3 hubs' change features.
pub const C_PORT_LINK_STATE: u16 = 25;
pub const C_PORT_CONFIG_ERROR: u16 = 26;
pub const C_BH_PORT_RESET: u16 = 29;
pub const BH_PORT_RESET: u16 = 28;

/// USB 3 hubs: SET_HUB_DEPTH (the hub's depth below the root, from 0).
pub const SET_HUB_DEPTH: u8 = 12;

/// wPortStatus bits.
pub const STATUS_CONNECTION: u16 = 1 << 0;
pub const STATUS_ENABLE: u16 = 1 << 1;
pub const STATUS_OVER_CURRENT: u16 = 1 << 3;
pub const STATUS_RESET: u16 = 1 << 4;
/// USB 2 only.
pub const STATUS_POWER: u16 = 1 << 8;
pub const STATUS_LOW_SPEED: u16 = 1 << 9;
pub const STATUS_HIGH_SPEED: u16 = 1 << 10;
/// USB 3 only.
pub const STATUS_SS_POWER: u16 = 1 << 9;

/// wPortChange bits.
pub const CHANGE_CONNECTION: u16 = 1 << 0;
pub const CHANGE_ENABLE: u16 = 1 << 1;
pub const CHANGE_SUSPEND: u16 = 1 << 2;
pub const CHANGE_OVER_CURRENT: u16 = 1 << 3;
pub const CHANGE_RESET: u16 = 1 << 4;
pub const CHANGE_BH_RESET: u16 = 1 << 5;
pub const CHANGE_LINK_STATE: u16 = 1 << 6;
pub const CHANGE_CONFIG_ERROR: u16 = 1 << 7;

/// The most ports a hub may have (USB 2: 255 in theory; usbd's route
/// strings and status bitmaps take 15, which USB 3 allows and no real
/// USB 2 hub exceeds).
pub const MAX_PORTS: u8 = 15;

/// What usbd needs of a hub's descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub ports: u8,
    pub characteristics: u16,
    /// Time from power on to power good, in milliseconds.
    pub power_on_ms: u32,
}

impl Descriptor {
    /// A hub descriptor (USB 2's or USB 3's, `ss`).
    pub fn parse(b: &[u8], ss: bool) -> Option<Descriptor> {
        let kind = if ss { SS_HUB } else { HUB };
        if b.len() < 7 || (b[0] as usize) < 7 || b[1] != kind || b[2] == 0 {
            return None;
        }
        Some(Descriptor { ports: b[2].min(MAX_PORTS), characteristics: u16::from_le_bytes([b[3], b[4]]), power_on_ms: 2 * b[5] as u32 })
    }

    /// A high-speed hub's transaction translator think time in FS bit
    /// times, as xHCI's slot context encodes it (0: 8, 1: 16, ...).
    pub fn tt_think_time(&self) -> u8 {
        (self.characteristics >> 5 & 0b11) as u8
    }

    /// Whether it switches power per port (else all ports at once).
    pub fn per_port_power(&self) -> bool {
        self.characteristics & 0b11 == 0b01
    }
}

/// A port's status and changes (GET_STATUS to a port, 4 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PortStatus {
    pub status: u16,
    pub change: u16,
}

impl PortStatus {
    pub fn parse(b: &[u8]) -> Option<PortStatus> {
        (b.len() >= 4).then(|| PortStatus { status: u16::from_le_bytes([b[0], b[1]]), change: u16::from_le_bytes([b[2], b[3]]) })
    }

    pub fn connected(&self) -> bool {
        self.status & STATUS_CONNECTION != 0
    }

    pub fn enabled(&self) -> bool {
        self.status & STATUS_ENABLE != 0
    }

    pub fn resetting(&self) -> bool {
        self.status & STATUS_RESET != 0
    }

    /// The attached device's speed: a USB 3 hub's ports are SuperSpeed; a
    /// USB 2 hub's say low, high or (neither) full.
    pub fn speed(&self, ss: bool) -> Speed {
        if ss {
            Speed::Super
        } else if self.status & STATUS_LOW_SPEED != 0 {
            Speed::Low
        } else if self.status & STATUS_HIGH_SPEED != 0 {
            Speed::High
        } else {
            Speed::Full
        }
    }

    /// The change features to clear for what `change` reports.
    pub fn changes_to_clear(&self, ss: bool) -> impl Iterator<Item = u16> + '_ {
        let all: [(u16, u16); 8] = [
            (CHANGE_CONNECTION, C_PORT_CONNECTION),
            (CHANGE_ENABLE, C_PORT_ENABLE),
            (CHANGE_SUSPEND, C_PORT_SUSPEND),
            (CHANGE_OVER_CURRENT, C_PORT_OVER_CURRENT),
            (CHANGE_RESET, C_PORT_RESET),
            (CHANGE_BH_RESET, C_BH_PORT_RESET),
            (CHANGE_LINK_STATE, C_PORT_LINK_STATE),
            (CHANGE_CONFIG_ERROR, C_PORT_CONFIG_ERROR),
        ];
        let change = self.change;
        all.into_iter()
            // (USB 2 hubs have no USB 3 changes; their bit 2 is suspend.)
            .filter(move |&(bit, feature)| change & bit != 0 && (ss || feature <= C_PORT_RESET) && !(ss && feature == C_PORT_SUSPEND))
            .map(|(_, feature)| feature)
    }
}

pub fn get_port_status(port: u8) -> SetupPacket {
    SetupPacket { request_type: DIR_IN | TYPE_CLASS | RECIPIENT_OTHER, request: crate::GET_STATUS, value: 0, index: port as u16, length: 4 }
}

pub fn set_port_feature(port: u8, feature: u16) -> SetupPacket {
    SetupPacket { request_type: TYPE_CLASS | RECIPIENT_OTHER, request: crate::SET_FEATURE, value: feature, index: port as u16, length: 0 }
}

pub fn clear_port_feature(port: u8, feature: u16) -> SetupPacket {
    SetupPacket { request_type: TYPE_CLASS | RECIPIENT_OTHER, request: crate::CLEAR_FEATURE, value: feature, index: port as u16, length: 0 }
}

pub fn set_hub_depth(depth: u8) -> SetupPacket {
    SetupPacket { request_type: TYPE_CLASS | RECIPIENT_DEVICE, request: SET_HUB_DEPTH, value: depth as u16, index: 0, length: 0 }
}

/// The ports a status change report (the interrupt endpoint's bitmap: bit
/// 0 the hub, bit n port n) names, up to `ports`.
pub fn changed_ports(bitmap: &[u8], ports: u8) -> impl Iterator<Item = u8> + '_ {
    (1..=ports.min(MAX_PORTS)).filter(move |&p| bitmap.get(p as usize / 8).is_some_and(|b| b & 1 << (p % 8) != 0))
}
