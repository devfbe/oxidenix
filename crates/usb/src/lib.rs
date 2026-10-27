//! The USB protocols usbd speaks with devices, apart from the host
//! controller (the xHCI driver is usbd's): standard requests
//! (`SetupPacket`), descriptors (`descriptor`), which network function a
//! device offers and how it is wired (`function`), the CDC class's
//! notifications and requests (`cdc`), CDC-NCM's transfer blocks (`ncm`),
//! Microsoft's RNDIS (`rndis`), hubs (`hub`) and boot keyboards (`hid`).
//! Kept apart from usbd so
//! that every parser is tested on the host against real devices'
//! descriptors and malformed input: everything here comes from a device,
//! and a device may be hostile.

#![no_std]

extern crate alloc;

pub mod cdc;
pub mod descriptor;
pub mod function;
pub mod hid;
pub mod hub;
pub mod ncm;
pub mod rndis;

/// The 8-byte setup stage of a control transfer (USB 2.0, 9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

/// `bmRequestType`: direction, type and recipient.
pub const DIR_IN: u8 = 0x80;
pub const TYPE_STANDARD: u8 = 0x00;
pub const TYPE_CLASS: u8 = 0x20;
pub const TYPE_VENDOR: u8 = 0x40;
pub const RECIPIENT_DEVICE: u8 = 0x00;
pub const RECIPIENT_INTERFACE: u8 = 0x01;
pub const RECIPIENT_ENDPOINT: u8 = 0x02;
pub const RECIPIENT_OTHER: u8 = 0x03;

/// Standard requests (USB 2.0, table 9-4).
pub const GET_STATUS: u8 = 0;
pub const CLEAR_FEATURE: u8 = 1;
pub const SET_FEATURE: u8 = 3;
pub const SET_ADDRESS: u8 = 5;
pub const GET_DESCRIPTOR: u8 = 6;
pub const SET_CONFIGURATION: u8 = 9;
pub const SET_INTERFACE: u8 = 11;

/// The feature selector of an endpoint's halt (CLEAR_FEATURE).
pub const ENDPOINT_HALT: u16 = 0;

impl SetupPacket {
    pub fn to_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0] = self.request_type;
        b[1] = self.request;
        b[2..4].copy_from_slice(&self.value.to_le_bytes());
        b[4..6].copy_from_slice(&self.index.to_le_bytes());
        b[6..8].copy_from_slice(&self.length.to_le_bytes());
        b
    }

    /// The setup stage as the xHCI Setup Stage TRB carries it (its
    /// immediate data, little endian).
    pub fn to_u64(self) -> u64 {
        u64::from_le_bytes(self.to_bytes())
    }

    /// Whether the data stage (if any) is device-to-host.
    pub fn is_in(self) -> bool {
        self.request_type & DIR_IN != 0
    }

    /// GET_DESCRIPTOR of descriptor `kind`, number `index`, `length` bytes
    /// (`language` for strings).
    pub fn get_descriptor(kind: u8, index: u8, language: u16, length: u16) -> SetupPacket {
        SetupPacket { request_type: DIR_IN | TYPE_STANDARD | RECIPIENT_DEVICE, request: GET_DESCRIPTOR, value: (kind as u16) << 8 | index as u16, index: language, length }
    }

    /// A hub's class descriptor (USB 2.0, 11.24.2.10; USB 3: 10.14.2.10).
    pub fn get_hub_descriptor(kind: u8, length: u16) -> SetupPacket {
        SetupPacket { request_type: DIR_IN | TYPE_CLASS | RECIPIENT_DEVICE, request: GET_DESCRIPTOR, value: (kind as u16) << 8, index: 0, length }
    }

    pub fn set_configuration(value: u8) -> SetupPacket {
        SetupPacket { request_type: TYPE_STANDARD | RECIPIENT_DEVICE, request: SET_CONFIGURATION, value: value as u16, index: 0, length: 0 }
    }

    pub fn set_interface(interface: u8, alternate: u8) -> SetupPacket {
        SetupPacket { request_type: TYPE_STANDARD | RECIPIENT_INTERFACE, request: SET_INTERFACE, value: alternate as u16, index: interface as u16, length: 0 }
    }

    /// CLEAR_FEATURE(ENDPOINT_HALT) of endpoint `address` (after a stall).
    pub fn clear_halt(address: u8) -> SetupPacket {
        SetupPacket { request_type: TYPE_STANDARD | RECIPIENT_ENDPOINT, request: CLEAR_FEATURE, value: ENDPOINT_HALT, index: address as u16, length: 0 }
    }

    /// A class request to interface `interface`.
    pub fn class_interface(request: u8, value: u16, interface: u8, length: u16, device_to_host: bool) -> SetupPacket {
        let dir = if device_to_host { DIR_IN } else { 0 };
        SetupPacket { request_type: dir | TYPE_CLASS | RECIPIENT_INTERFACE, request, value, index: interface as u16, length }
    }
}

/// A device's or port's speed, as xHCI's default protocol speed IDs name
/// them (xHCI 1.2, 7.2.2.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Speed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
}

impl Speed {
    /// The default protocol speed ID of PORTSC's Port Speed field.
    pub fn from_psiv(psiv: u8) -> Option<Speed> {
        Some(match psiv {
            1 => Speed::Full,
            2 => Speed::Low,
            3 => Speed::High,
            4 => Speed::Super,
            5 => Speed::SuperPlus,
            _ => return None,
        })
    }

    pub fn psiv(self) -> u8 {
        match self {
            Speed::Full => 1,
            Speed::Low => 2,
            Speed::High => 3,
            Speed::Super => 4,
            Speed::SuperPlus => 5,
        }
    }

    /// The control endpoint's packet size before the device descriptor
    /// says (USB 2.0, 5.5.3; USB 3: 512 bytes, bMaxPacketSize0 = 9).
    pub fn default_max_packet0(self) -> u16 {
        match self {
            Speed::Low | Speed::Full => 8,
            Speed::High => 64,
            Speed::Super | Speed::SuperPlus => 512,
        }
    }

    pub fn is_super(self) -> bool {
        self >= Speed::Super
    }

    pub fn name(self) -> &'static str {
        match self {
            Speed::Low => "low",
            Speed::Full => "full",
            Speed::High => "high",
            Speed::Super => "super",
            Speed::SuperPlus => "super+",
        }
    }
}
