//! smoltcp's view of the network card, with loopback: frames addressed to
//! this host (its own address or 127.0.0.0/8) never reach the wire but come
//! back as received frames, and ARP requests for those addresses are
//! answered here.

use crate::virtio::{self, VirtioNet};
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;
/// Frames waiting to be "received" from the loopback path.
const MAX_LOOPED: usize = 64;

pub struct Nic {
    pub card: VirtioNet,
    mac: [u8; 6],
    /// The interface's IPv4 address (0 while unconfigured).
    pub address: u32,
    looped: VecDeque<Vec<u8>>,
}

pub struct RxToken(Vec<u8>);
pub struct TxToken<'a>(&'a mut Nic);

fn u16_at(f: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([f[o], f[o + 1]])
}

fn u32_at(f: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([f[o], f[o + 1], f[o + 2], f[o + 3]])
}

impl Nic {
    pub fn new(card: VirtioNet) -> Nic {
        let mac = card.mac;
        Nic { card, mac, address: 0, looped: VecDeque::new() }
    }

    fn is_local(&self, ip: u32) -> bool {
        (ip != 0 && ip == self.address) || ip >> 24 == 127
    }

    pub fn has_received(&mut self) -> bool {
        !self.looped.is_empty() || self.card.has_received()
    }

    fn loop_back(&mut self, frame: Vec<u8>) {
        if self.looped.len() < MAX_LOOPED {
            self.looped.push_back(frame);
        }
    }

    /// Sends a frame smoltcp built, or loops it back.
    fn output(&mut self, mut frame: Vec<u8>) {
        if frame.len() >= 42 && u16_at(&frame, 12) == ETHERTYPE_ARP && u16_at(&frame, 20) == ARP_REQUEST {
            let target = u32_at(&frame, 38);
            if self.is_local(target) {
                // Answer for ourselves: "target is at our MAC".
                let mut reply = frame.clone();
                let (requester_mac, requester_ip) = (frame[22..28].to_vec(), frame[28..32].to_vec());
                reply[0..6].copy_from_slice(&requester_mac);
                reply[6..12].copy_from_slice(&self.mac);
                reply[20..22].copy_from_slice(&ARP_REPLY.to_be_bytes());
                reply[22..28].copy_from_slice(&self.mac);
                reply[28..32].copy_from_slice(&target.to_be_bytes());
                reply[32..38].copy_from_slice(&requester_mac);
                reply[38..42].copy_from_slice(&requester_ip);
                self.loop_back(reply);
                return;
            }
        }
        if frame.len() >= 34 && u16_at(&frame, 12) == ETHERTYPE_IPV4 && self.is_local(u32_at(&frame, 30)) {
            let mac = self.mac;
            frame[0..6].copy_from_slice(&mac);
            self.loop_back(frame);
            return;
        }
        // A full transmit queue drops the frame, as a busy wire would.
        if self.card.can_send() {
            self.card.send(frame.len(), |buf| buf.copy_from_slice(&frame));
        }
    }
}

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len.min(virtio::MTU)];
        let result = f(&mut frame);
        self.0.output(frame);
        result
    }
}

impl Device for Nic {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = match self.looped.pop_front() {
            Some(frame) => frame,
            None => self.card.receive(|f| f.to_vec())?,
        };
        Some((RxToken(frame), TxToken(self)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        Some(TxToken(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = virtio::MTU;
        caps
    }
}
