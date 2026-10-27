//! The Ethernet functions usbd drives (USB tethering, `usb::function`):
//! binding one (CDC-ECM, CDC-NCM, RNDIS: each kind's setup, `bind`), its
//! link to netd (`link`), and the data path: bulk IN transfers queued all
//! the time, each completed one's frames into the link's receive ring;
//! frames from the link's send ring into bulk OUT transfers (one frame a
//! transfer for ECM, an NTB of several for NCM, `PACKET_MSG`s for RNDIS);
//! the notifications of the interrupt endpoint (the carrier; RNDIS's
//! responses and indications).

pub mod link;
mod rndis;

use crate::device::{Completed, Device};
use crate::dma::{self, Buf};
use crate::xhci::context::dci;
use crate::xhci::{Error, Xhci};
use alloc::vec;
use alloc::vec::Vec;
use link::{Futex, Link};
use linkring::{Info, MAX_FRAME};
use oxrt::println;
use usb::function::{Kind, NetFunction};
use usb::{cdc, ncm, SetupPacket};

/// Bulk IN transfers kept queued, and bulk OUT buffers.
const RX_QUEUE: usize = 16;
const TX_QUEUE: usize = 16;
/// Bytes of a transfer: an ECM frame's buffer (one frame, a short packet
/// ends it), or what an NTB or RNDIS transfer may hold.
const ECM_BUFFER: usize = 2048;
const BLOCK_BUFFER: usize = 16384;
/// The notification buffer (CDC's longest: the speed change, 16 bytes).
const NOTIFY_BYTES: usize = 64;

/// Cookies: which buffer a transfer is.
const RX: u32 = 0;
const TX: u32 = 1 << 16;
const NOTIFY: u32 = 2 << 16;

pub struct Net {
    pub kind: Kind,
    pub mac: [u8; 6],
    pub mtu: u32,
    link: Option<Link>,
    /// Endpoint DCIs.
    rx_dci: u8,
    tx_dci: u8,
    notify_dci: Option<u8>,
    /// Bulk OUT's packet size: a transfer of a multiple of it gets a pad
    /// byte (as Linux's usbnet sends), so that a short packet ends it.
    tx_packet: usize,
    rx: Vec<Buf>,
    tx: Vec<Buf>,
    tx_free: Vec<usize>,
    notify: Option<Buf>,
    buffer: usize,
    carrier: bool,
    /// A frame taken from the send ring that did not fit the NTB being
    /// built: it goes first into the next one.
    carry: Option<usize>,
    scratch: Vec<u8>,
    ncm: Option<ncm::Parameters>,
    ncm_sequence: u16,
    rndis: Option<rndis::Rndis>,
    /// What netd should hear of (new frames, room in the send ring).
    ring_netd: bool,
    /// The send ring was malformed: the link is given up (`check_link`).
    broken: bool,
}

impl Net {
    /// Binds `function` of `dev` (its configuration set already): the
    /// kind's setup, its endpoints configured, its buffers made. The link
    /// to netd comes with `start`.
    pub fn bind(xhci: &mut Xhci, dev: &mut Device, function: NetFunction) -> Result<Net, Error> {
        let mut endpoints = vec![function.bulk_in, function.bulk_out];
        if let Some(n) = function.notify {
            endpoints.push(n);
        }
        let buffer = match function.kind {
            Kind::Ecm => ECM_BUFFER,
            Kind::Ncm | Kind::Rndis => BLOCK_BUFFER,
        };
        let mut net = Net {
            kind: function.kind,
            mac: [0; 6],
            mtu: 1500,
            link: None,
            rx_dci: dci(&function.bulk_in),
            tx_dci: dci(&function.bulk_out),
            notify_dci: function.notify.as_ref().map(dci),
            tx_packet: function.bulk_out.packet_size().max(1) as usize,
            rx: Vec::new(),
            tx: Vec::new(),
            tx_free: Vec::new(),
            notify: None,
            buffer,
            // (Until a notification says otherwise: not every function
            // sends one.)
            carrier: true,
            carry: None,
            scratch: vec![0u8; MAX_FRAME],
            ncm: None,
            ncm_sequence: 0,
            rndis: None,
            ring_netd: false,
            broken: false,
        };
        // NCM's input size is set while the data interface has no
        // endpoints (its setting 0).
        if function.kind == Kind::Ncm {
            net.ncm = Some(ncm_setup(xhci, dev, &function)?);
        }
        dev.configure(xhci, &endpoints)?;
        match function.kind {
            Kind::Ecm | Kind::Ncm => {
                net.mac = cdc_mac(xhci, dev, &function)?;
                if function.max_segment as usize > linkring::MIN_FRAME + 576 {
                    net.mtu = (function.max_segment as u32 - 14).min(1500);
                }
                dev.control_out(xhci, SetupPacket::set_interface(function.data, function.data_alternate), &[])?;
                let filter = cdc::PACKET_TYPE_DIRECTED | cdc::PACKET_TYPE_BROADCAST | cdc::PACKET_TYPE_ALL_MULTICAST;
                // (Optional for NCM and for some ECM functions: a stall is no failure.)
                match dev.control_out(xhci, SetupPacket::class_interface(cdc::SET_ETHERNET_PACKET_FILTER, filter, function.control, 0, false), &[]) {
                    Ok(()) | Err(Error::Stall) => {}
                    Err(e) => return Err(e),
                }
            }
            Kind::Rndis => {
                let (r, mac, up) = rndis::Rndis::init(xhci, dev, &function)?;
                net.mac = mac;
                net.carrier = up;
                net.mtu = r.mtu();
                net.rndis = Some(r);
            }
        }
        for _ in 0..RX_QUEUE {
            net.rx.push(xhci.dma.alloc_bytes(buffer).ok_or(Error::NoRoom)?);
        }
        for i in 0..TX_QUEUE {
            net.tx.push(xhci.dma.alloc_bytes(buffer).ok_or(Error::NoRoom)?);
            net.tx_free.push(i);
        }
        if net.notify_dci.is_some() {
            net.notify = Some(xhci.dma.alloc(1).ok_or(Error::NoRoom)?);
        }
        Ok(net)
    }

    /// Queues the receive and notification transfers and offers the link
    /// to netd.
    pub fn start(&mut self, xhci: &mut Xhci, dev: &mut Device) -> Result<(), Error> {
        for i in 0..self.rx.len() {
            dev.submit(self.rx_dci, self.rx[i].phys(), self.buffer, RX | i as u32)?;
        }
        dev.kick(xhci, self.rx_dci);
        if let (Some(d), Some(buf)) = (self.notify_dci, &self.notify) {
            dev.submit(d, buf.phys(), NOTIFY_BYTES, NOTIFY)?;
            dev.kick(xhci, d);
        }
        self.connect();
        Ok(())
    }

    /// Offers netd a link (again, after it lost the last one).
    fn connect(&mut self) {
        let info = Info { mac: self.mac, mtu: self.mtu, driver: Info::driver_field(self.kind.driver()) };
        match Link::open(&info) {
            Ok(link) => {
                link.area.set_carrier(self.carrier, &Futex);
                self.link = Some(link);
            }
            Err(e) => println!("usbd: netd did not take the link (errno {})", -e),
        }
    }

    /// The link went (netd died or let go): a new one is offered.
    pub fn check_link(&mut self) {
        if core::mem::take(&mut self.broken) || self.link.as_ref().is_some_and(Link::gone) {
            if let Some(link) = self.link.take() {
                link.close();
            }
            self.carry = None;
            self.connect();
        }
    }

    /// A transfer of the function's endpoints completed.
    pub fn completed(&mut self, xhci: &mut Xhci, dev: &mut Device, c: Completed) {
        match c.cookie & 0xffff_0000 {
            RX => self.received(xhci, dev, c),
            TX => {
                self.tx_free.push((c.cookie & 0xffff) as usize);
                if c.result.is_err() {
                    // The endpoint was reset: every other send is gone too.
                    self.tx_free = (0..self.tx.len()).collect();
                }
            }
            NOTIFY => self.notified(xhci, dev, c),
            _ => {}
        }
    }

    fn received(&mut self, xhci: &mut Xhci, dev: &mut Device, c: Completed) {
        let i = (c.cookie & 0xffff) as usize;
        if c.result.is_ok() && c.len > 0 {
            dma::acquire();
            let data = &self.rx[i].bytes()[..c.len.min(self.buffer)];
            if let Some(link) = &mut self.link {
                let mut push = |frame: &[u8]| {
                    if (linkring::MIN_FRAME..=MAX_FRAME).contains(&frame.len()) && !link.rx.push(frame) {
                        link.area.count_rx_dropped();
                    }
                };
                match self.kind {
                    Kind::Ecm => push(data),
                    Kind::Rndis => rndis::packets(data).for_each(push),
                    Kind::Ncm => {
                        let mut ranges = [(0, 0); ncm::MAX_DATAGRAMS];
                        let n = ncm::datagrams(data, &mut ranges).unwrap_or(0);
                        for &(s, e) in &ranges[..n] {
                            push(&data[s..e]);
                        }
                    }
                }
                self.ring_netd = true;
            }
        }
        if c.result.is_err() {
            // The endpoint was reset: every receive is queued again.
            for j in 0..self.rx.len() {
                let _ = dev.submit(self.rx_dci, self.rx[j].phys(), self.buffer, RX | j as u32);
            }
        } else {
            let _ = dev.submit(self.rx_dci, self.rx[i].phys(), self.buffer, RX | i as u32);
        }
        dev.kick(xhci, self.rx_dci);
    }

    fn notified(&mut self, xhci: &mut Xhci, dev: &mut Device, c: Completed) {
        if c.result.is_ok() {
            dma::acquire();
            let bytes = self.notify.as_ref().map(|b| b.bytes()[..c.len.min(NOTIFY_BYTES)].to_vec()).unwrap_or_default();
            match &mut self.rndis {
                Some(r) => {
                    if usb::rndis::is_response_available(&bytes) {
                        if let Some(up) = r.responses(xhci, dev) {
                            self.set_carrier(up);
                        }
                    }
                }
                None => match cdc::Notification::parse(&bytes) {
                    cdc::Notification::Connection(up) => self.set_carrier(up),
                    cdc::Notification::Speed(down, up) => println!("usbd: {} link at {} Mbit/s down, {} up", self.kind.driver(), down / 1_000_000, up / 1_000_000),
                    _ => {}
                },
            }
        }
        if let (Some(d), Some(buf)) = (self.notify_dci, &self.notify) {
            let _ = dev.submit(d, buf.phys(), NOTIFY_BYTES, NOTIFY);
            dev.kick(xhci, d);
        }
    }

    fn set_carrier(&mut self, up: bool) {
        if up != self.carrier {
            self.carrier = up;
            println!("usbd: {} {}", self.kind.driver(), if up { "carrier up" } else { "carrier lost" });
        }
        if let Some(link) = &self.link {
            link.area.set_carrier(up, &Futex);
        }
    }

    /// Moves frames from the link's send ring into bulk OUT transfers;
    /// whether it did.
    pub fn pump(&mut self, xhci: &mut Xhci, dev: &mut Device) -> bool {
        let mut sent = false;
        while let Some(&slot) = self.tx_free.last() {
            if !dev.has_room(self.tx_dci, 1) {
                break;
            }
            let Some(len) = self.fill(slot) else { break };
            let len = if len % self.tx_packet == 0 { len + 1 } else { len };
            self.tx_free.pop();
            if dev.submit(self.tx_dci, self.tx[slot].phys(), len, TX | slot as u32).is_err() {
                self.tx_free.push(slot);
                break;
            }
            sent = true;
        }
        if sent {
            dev.kick(xhci, self.tx_dci);
            // (Room in the send ring for netd.)
            self.ring_netd = true;
        }
        sent
    }

    /// Fills transfer buffer `slot` from the send ring: its length, or
    /// None if the ring had nothing. A malformed ring (netd broke the
    /// protocol) ends the link.
    fn fill(&mut self, slot: usize) -> Option<usize> {
        let link = self.link.as_mut()?;
        let buf = self.tx[slot].bytes_mut();
        let broken = &mut self.broken;
        let mut take = |scratch: &mut [u8]| match link.tx.pop(scratch) {
            Ok(n) => n,
            Err(e) => {
                println!("usbd: netd broke the link protocol ({:?}); offering a new link", e);
                *broken = true;
                None
            }
        };
        let len = match self.kind {
            Kind::Ecm => {
                let n = take(&mut self.scratch)?;
                buf[..n].copy_from_slice(&self.scratch[..n]);
                n
            }
            Kind::Rndis => {
                let n = take(&mut self.scratch)?;
                buf[..usb::rndis::PACKET_HEADER].copy_from_slice(&usb::rndis::packet_header(n));
                buf[usb::rndis::PACKET_HEADER..usb::rndis::PACKET_HEADER + n].copy_from_slice(&self.scratch[..n]);
                usb::rndis::PACKET_HEADER + n
            }
            Kind::Ncm => {
                let params = self.ncm.expect("an NCM function has its parameters");
                let mut b = ncm::Builder::new(buf, params, ncm::MAX_DATAGRAMS, self.ncm_sequence);
                loop {
                    let n = match self.carry.take() {
                        Some(n) => n,
                        None => match take(&mut self.scratch) {
                            Some(n) => n,
                            None => break,
                        },
                    };
                    if !b.push_with(n, |d| d.copy_from_slice(&self.scratch[..n])) {
                        self.carry = Some(n);
                        break;
                    }
                }
                if b.is_empty() {
                    return None;
                }
                self.ncm_sequence = self.ncm_sequence.wrapping_add(1);
                b.finish()
            }
        };
        Some(len)
    }

    /// Rings netd's doorbell for what this round did.
    pub fn flush(&mut self) {
        if core::mem::take(&mut self.ring_netd) {
            if let Some(link) = &self.link {
                link.area.ring_netd(&Futex);
            }
        }
    }

    /// Whether the send ring holds frames (or one is carried) and a
    /// buffer is free for them.
    pub fn has_work(&self) -> bool {
        !self.tx_free.is_empty() && (self.carry.is_some() || self.link.as_ref().is_some_and(|l| !l.tx.is_empty()))
    }

    pub fn link(&self) -> Option<&Link> {
        self.link.as_ref()
    }

    /// The device went (or the function is given up): the link closes,
    /// the buffers go back.
    pub fn release(mut self, xhci: &mut Xhci) {
        if let Some(link) = self.link.take() {
            link.close();
        }
        for b in self.rx.drain(..).chain(self.tx.drain(..)).chain(self.notify.take()) {
            xhci.dma.free(b);
        }
    }
}

/// The MAC address of an ECM or NCM function, from its string descriptor.
fn cdc_mac(xhci: &mut Xhci, dev: &mut Device, f: &NetFunction) -> Result<[u8; 6], Error> {
    let text = dev.string(xhci, f.mac_string)?;
    let mac = cdc::mac_from_string(&text).ok_or(Error::Protocol("a malformed MAC address string"))?;
    if mac[0] & 1 != 0 || mac == [0; 6] {
        return Err(Error::Protocol("a multicast or zero MAC address"));
    }
    Ok(mac)
}

/// NCM's setup: the parameters, the 16-bit format, the input size.
fn ncm_setup(xhci: &mut Xhci, dev: &mut Device, f: &NetFunction) -> Result<ncm::Parameters, Error> {
    let answer = dev.control_in(xhci, SetupPacket::class_interface(ncm::GET_NTB_PARAMETERS, 0, f.control, ncm::PARAMETERS_BYTES as u16, true))?;
    let params = ncm::Parameters::parse(answer).ok_or(Error::Protocol("NTB parameters usbd cannot honour"))?;
    if params.formats & 2 != 0 {
        dev.control_out(xhci, SetupPacket::class_interface(ncm::SET_NTB_FORMAT, ncm::FORMAT_NTB16, f.control, 0, false), &[])?;
    }
    let size = (BLOCK_BUFFER as u32).min(params.in_max_size);
    // (Optional: a function whose input size is its own already may stall.)
    match dev.control_out(xhci, SetupPacket::class_interface(ncm::SET_NTB_INPUT_SIZE, 0, f.control, 4, false), &size.to_le_bytes()) {
        Ok(()) | Err(Error::Stall) => {}
        Err(e) => return Err(e),
    }
    let mut params = params;
    params.out_max_size = params.out_max_size.min(BLOCK_BUFFER as u32);
    Ok(params)
}
