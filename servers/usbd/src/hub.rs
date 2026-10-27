//! Hubs (USB 2 and USB 3, `usb::hub`): a hub's slot is made one (its
//! ports in the slot context), its ports powered, its status change
//! endpoint watched; a port that changed is looked at, a device that came
//! is reset and attached below it, one that went is detached.

use crate::device::{pause, Device, Position};
use crate::dma::Buf;
use crate::xhci::{Error, Xhci};
use usb::descriptor::{self, Configuration, INTERRUPT};
use usb::hub::{self as h, PortStatus};
use usb::{SetupPacket, Speed};

/// How long a port reset may take, and the recovery after it (USB 2.0,
/// 7.1.7.5: TRSTRCY 10 ms).
const RESET_MS: u64 = 500;
const RECOVERY_MS: u64 = 10;
/// The status change report's bytes (bit 0 the hub, bits 1-15 its ports).
const STATUS_BYTES: usize = 2;
/// Hubs below the root at most (USB allows 5 tiers of hubs).
pub const MAX_DEPTH: u8 = 5;

pub struct Hub {
    pub ports: u8,
    ss: bool,
    status_dci: u8,
    status: Buf,
    /// The device on each port (by slot), from port 1.
    pub children: [Option<u8>; h::MAX_PORTS as usize + 1],
}

impl Hub {
    /// Makes `dev` (a hub, configuration `config` chosen) a hub: its
    /// descriptor, its slot context, its status endpoint, its ports
    /// powered.
    pub fn bind(xhci: &mut Xhci, dev: &mut Device, config: &Configuration) -> Result<Hub, Error> {
        if dev.position.depth >= MAX_DEPTH {
            return Err(Error::Protocol("a hub too deep below the root"));
        }
        let ss = dev.speed.is_super();
        let interface = config.interfaces.iter().find(|i| i.class == h::CLASS_HUB).ok_or(Error::Protocol("a hub without a hub interface"))?;
        let endpoint = interface.endpoint(INTERRUPT, true).ok_or(Error::Protocol("a hub without a status endpoint"))?;
        let kind = if ss { descriptor::SS_HUB } else { descriptor::HUB };
        let bytes = dev.control_in(xhci, SetupPacket::get_hub_descriptor(kind, 12))?;
        let d = h::Descriptor::parse(bytes, ss).ok_or(Error::Protocol("a malformed hub descriptor"))?;
        // A high-speed hub with a transaction translator per port says so
        // in its device protocol (2).
        let multi_tt = dev.speed == Speed::High && dev.descriptor.protocol == 2;
        dev.hub = Some((d.ports, if dev.speed == Speed::High { d.tt_think_time() } else { 0 }, multi_tt));
        dev.control_out(xhci, SetupPacket::set_configuration(config.value), &[])?;
        // (The slot's hub fields go in with the endpoint.)
        dev.configure(xhci, &[endpoint])?;
        if ss {
            dev.control_out(xhci, h::set_hub_depth(dev.position.depth), &[])?;
        }
        for port in 1..=d.ports {
            dev.control_out(xhci, h::set_port_feature(port, h::PORT_POWER), &[])?;
        }
        pause(d.power_on_ms.max(100) as u64);
        let status = xhci.dma.alloc(1).ok_or(Error::NoRoom)?;
        let dci = crate::xhci::context::dci(&endpoint);
        let hub = Hub { ports: d.ports, ss, status_dci: dci, status, children: [None; h::MAX_PORTS as usize + 1] };
        dev.submit(dci, hub.status.phys(), STATUS_BYTES, 0)?;
        dev.kick(xhci, dci);
        Ok(hub)
    }

    pub fn status_dci(&self) -> u8 {
        self.status_dci
    }

    /// The ports the status report names (bit n: port n), and the
    /// report queued again. All ports after an error (the endpoint was
    /// reset: look at every one).
    pub fn changed(&mut self, xhci: &mut Xhci, dev: &mut Device, ok: bool) -> u16 {
        crate::dma::acquire();
        let bitmap = if ok { u16::from_le_bytes([self.status.bytes()[0], self.status.bytes()[1]]) } else { u16::MAX };
        let _ = dev.submit(self.status_dci, self.status.phys(), STATUS_BYTES, 0);
        dev.kick(xhci, self.status_dci);
        let mask = if self.ports >= 15 { u16::MAX } else { (1u16 << (self.ports + 1)) - 2 };
        bitmap & mask
    }

    /// A port's status, its changes cleared.
    pub fn port_status(&self, xhci: &mut Xhci, dev: &mut Device, port: u8) -> Result<PortStatus, Error> {
        let s = PortStatus::parse(dev.control_in(xhci, h::get_port_status(port))?).ok_or(Error::Protocol("a short port status"))?;
        for feature in s.changes_to_clear(self.ss) {
            dev.control_out(xhci, h::clear_port_feature(port, feature), &[])?;
        }
        Ok(s)
    }

    /// Resets `port` of the hub (a device is on it): the device's speed
    /// once it is enabled.
    pub fn reset_port(&self, xhci: &mut Xhci, dev: &mut Device, port: u8) -> Result<Speed, Error> {
        dev.control_out(xhci, h::set_port_feature(port, h::PORT_RESET), &[])?;
        let until = crate::xhci::deadline(RESET_MS);
        loop {
            pause(10);
            let s = self.port_status(xhci, dev, port)?;
            if !s.connected() {
                return Err(Error::Protocol("the device went during its reset"));
            }
            if !s.resetting() && s.enabled() {
                pause(RECOVERY_MS);
                return Ok(s.speed(self.ss));
            }
            if oxrt::uptime_ms() > until {
                return Err(Error::Timeout);
            }
        }
    }

    /// Where a device on `port` hangs: the hub's position one tier down.
    pub fn child_position(&self, hub: &Device, port: u8, speed: Speed) -> Position {
        let p = hub.position;
        let tt = match (hub.speed, speed) {
            // Below a high-speed hub, a slow device's transactions go
            // through that hub's translator.
            (Speed::High, Speed::Low | Speed::Full) => Some((hub.slot, port)),
            // Below a slower hub: whatever translator that hub uses.
            _ => p.tt,
        };
        Position { root_port: p.root_port, route: p.route | ((port.min(15) as u32) << (4 * p.depth as u32)), tt, depth: p.depth + 1 }
    }

    pub fn release(self, xhci: &mut Xhci) {
        xhci.dma.free(self.status);
    }
}
