//! A USB device on the controller: its slot (enabled, addressed: xHCI
//! 1.2, 4.3), its control endpoint's transfers (waited for), its other
//! endpoints (configured together, 4.6.6) and their transfers (queued,
//! completed by the event loop), and its release.

use crate::dma::{Buf, PAGE};
use crate::xhci::context::{self, Input, Output};
use crate::xhci::ring::Ring;
use crate::xhci::trb::{self, Event, Trb};
use crate::xhci::{deadline, Error, Xhci, COMMAND_TIMEOUT_MS};
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use usb::descriptor::{self, Endpoint};
use usb::{SetupPacket, Speed};

/// Where a device hangs: its root hub port, the route string of the hubs
/// below it (4 bits a tier: xHCI 1.2, 8.9), and for a low- or full-speed
/// device behind a high-speed hub, that hub's slot and port (its
/// transaction translator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub root_port: u8,
    pub route: u32,
    pub tt: Option<(u8, u8)>,
    /// Hubs between it and the root (0: on a root port).
    pub depth: u8,
}

impl Position {
    /// "port" or "port.port.port", as Linux names a device's place.
    pub fn name(&self) -> alloc::string::String {
        let mut s = alloc::format!("{}", self.root_port);
        for tier in 0..self.depth {
            s += &alloc::format!(".{}", self.route >> (4 * tier as u32) & 0xf);
        }
        s
    }
}

/// A transfer queued on an endpoint.
struct Td {
    first: u64,
    last: u64,
    trbs: usize,
    cookie: u32,
    /// Bytes asked for.
    len: usize,
}

/// One of the device's endpoints other than EP0, with its ring.
struct EndpointRing {
    ring: Ring,
    endpoint: Endpoint,
    queued: VecDeque<Td>,
}

/// A transfer the event loop learned is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completed {
    pub dci: u8,
    pub cookie: u32,
    /// Bytes transferred.
    pub len: usize,
    /// Success or a short packet (`Ok`), or the error.
    pub result: Result<(), Error>,
}

pub struct Device {
    pub slot: u8,
    pub speed: Speed,
    pub position: Position,
    /// What the hub fields of the slot context say (ports, think time,
    /// multiple TTs), once the device is known to be a hub.
    pub hub: Option<(u8, u8, bool)>,
    input: Input,
    output: Output,
    ep0: Ring,
    max_packet0: u16,
    /// The control transfers' data (one page).
    buf: Buf,
    pub descriptor: descriptor::Device,
    endpoints: [Option<EndpointRing>; 32],
}

impl Device {
    /// Enables a slot for the device just reset at `position` and gives
    /// it an address; reads its device descriptor.
    pub fn attach(xhci: &mut Xhci, position: Position, speed: Speed) -> Result<Device, Error> {
        let slot = xhci.command(Trb::enable_slot())?;
        if slot == 0 || slot > xhci.max_slots {
            return Err(Error::Protocol("the controller gave an invalid slot"));
        }
        let wide = xhci.wide;
        let parts = (|| {
            let output = Output::new(&mut xhci.dma, wide)?;
            let input = Input::new(&mut xhci.dma, wide)?;
            let ep0 = Ring::new(&mut xhci.dma)?;
            let buf = xhci.dma.alloc(1)?;
            Some((output, input, ep0, buf))
        })();
        let Some((output, input, ep0, buf)) = parts else {
            let _ = xhci.command(Trb::disable_slot(slot));
            return Err(Error::NoRoom);
        };
        xhci.set_device_context(slot, output.phys());
        let unknown = descriptor::Device { usb: 0, class: 0, subclass: 0, protocol: 0, max_packet0: 0, vendor: 0, product: 0, release: 0, manufacturer: 0, product_string: 0, serial: 0, configurations: 0 };
        let mut dev = Device {
            slot,
            speed,
            position,
            hub: None,
            input,
            output,
            ep0,
            max_packet0: speed.default_max_packet0(),
            buf,
            descriptor: unknown,
            endpoints: Default::default(),
        };
        match dev.address(xhci) {
            Ok(()) => Ok(dev),
            Err(e) => {
                dev.release(xhci);
                Err(e)
            }
        }
    }

    fn address(&mut self, xhci: &mut Xhci) -> Result<(), Error> {
        self.input.clear();
        self.input.add(0);
        self.input.add(1);
        self.write_slot(1);
        self.input.control_endpoint(self.max_packet0, self.ep0.base());
        xhci.command(Trb::address_device(self.input.phys(), self.slot, false))?;
        // The device's recovery after SET_ADDRESS (USB 2.0, 9.2.6.3).
        pause(2);
        // The first 8 bytes tell the control endpoint's packet size.
        let head = self.control_in(xhci, SetupPacket::get_descriptor(descriptor::DEVICE, 0, 0, 8))?;
        if head.len() < 8 || head[1] != descriptor::DEVICE {
            return Err(Error::Protocol("no device descriptor"));
        }
        let probe = descriptor::Device::parse(&{
            let mut d = [0u8; descriptor::Device::SIZE];
            d[..8].copy_from_slice(&head[..8]);
            d[0] = descriptor::Device::SIZE as u8;
            d
        });
        let mp0 = probe.and_then(|d| d.max_packet0_bytes(self.speed)).ok_or(Error::Protocol("an invalid control packet size"))?;
        if mp0 != self.max_packet0 {
            self.max_packet0 = mp0;
            self.input.clear();
            self.input.add(1);
            self.input.control_packet(mp0);
            xhci.command(Trb::evaluate_context(self.input.phys(), self.slot))?;
        }
        let full = self.control_in(xhci, SetupPacket::get_descriptor(descriptor::DEVICE, 0, 0, descriptor::Device::SIZE as u16))?;
        self.descriptor = descriptor::Device::parse(full).ok_or(Error::Protocol("a malformed device descriptor"))?;
        Ok(())
    }

    /// The slot context with `entries` contexts, from what is known.
    fn write_slot(&mut self, entries: u8) {
        let p = self.position;
        self.input.slot(p.route, self.speed, entries, p.root_port, self.hub, p.tt);
    }

    /// The highest DCI configured (the slot context's entries).
    fn entries(&self) -> u8 {
        (1..32u8).rev().find(|&d| self.endpoints[d as usize].is_some()).unwrap_or(1)
    }

    // ------------------------------------------------- control transfers

    /// A control transfer that reads (at most a page): the bytes.
    pub fn control_in(&mut self, xhci: &mut Xhci, setup: SetupPacket) -> Result<&[u8], Error> {
        let n = self.control(xhci, setup, None)?;
        crate::dma::acquire();
        Ok(&self.buf.bytes()[..n])
    }

    /// A control transfer that writes `data` (at most a page), or nothing.
    pub fn control_out(&mut self, xhci: &mut Xhci, setup: SetupPacket, data: &[u8]) -> Result<(), Error> {
        self.control(xhci, setup, Some(data)).map(|_| ())
    }

    fn control(&mut self, xhci: &mut Xhci, mut setup: SetupPacket, out: Option<&[u8]>) -> Result<usize, Error> {
        let input = setup.is_in();
        if let Some(data) = out {
            setup.length = data.len() as u16;
            self.buf.bytes_mut()[..data.len()].copy_from_slice(data);
        }
        let len = (setup.length as usize).min(PAGE);
        setup.length = len as u16;
        let transfer = match (len, input) {
            (0, _) => trb::TRT_NONE,
            (_, true) => trb::TRT_IN,
            (_, false) => trb::TRT_OUT,
        };
        let mut td = Vec::with_capacity(3);
        td.push(Trb::setup(setup.to_u64(), transfer));
        if len > 0 {
            td.push(Trb::data(self.buf.phys(), len as u32, input));
        }
        td.push(Trb::status(input && len > 0));
        let (first, last, trbs) = self.ep0.push(&td).ok_or(Error::NoRoom)?;
        xhci.ring_doorbell(self.slot, 1);
        let slot = self.slot;
        let mut done = len;
        let result = loop {
            let event = xhci.wait(deadline(COMMAND_TIMEOUT_MS), |e| matches!(e, Event::Transfer { slot: s, dci: 1, trb, .. } if *s == slot && (first..=last).contains(trb)));
            match event {
                Ok(Event::Transfer { trb, code, residual, .. }) => match code {
                    trb::SUCCESS if trb == last => break Ok(()),
                    // The data stage ended short; the status stage follows.
                    trb::SHORT_PACKET if trb != last => done = len.saturating_sub(residual as usize),
                    trb::SUCCESS => {}
                    trb::STALL => break Err(Error::Stall),
                    code => break Err(Error::Code(code)),
                },
                Ok(_) => unreachable!("the predicate took a transfer event"),
                Err(e) => break Err(e),
            }
        };
        match result {
            Ok(()) => {
                self.ep0.completed(trbs);
                Ok(done)
            }
            Err(e) => {
                // EP0 halted (or the TD still waits): it continues after it.
                self.recover(xhci, 1, matches!(e, Error::Timeout));
                Err(e)
            }
        }
    }

    // ----------------------------------------------------- the endpoints

    /// Configures the endpoints `endpoints` (an interface's alternate
    /// setting, or several): each gets a ring, and the controller its
    /// context (one Configure Endpoint command).
    pub fn configure(&mut self, xhci: &mut Xhci, endpoints: &[Endpoint]) -> Result<(), Error> {
        let mut rings = Vec::new();
        for _ in endpoints {
            match Ring::new(&mut xhci.dma) {
                Some(r) => rings.push(r),
                None => {
                    rings.into_iter().for_each(|r: Ring| r.free(&mut xhci.dma));
                    return Err(Error::NoRoom);
                }
            }
        }
        self.input.clear();
        self.input.add(0);
        let speed = self.speed;
        let mut entries = self.entries();
        for (e, r) in endpoints.iter().zip(&rings) {
            let dci = context::dci(e);
            if dci < 2 || self.endpoints[dci as usize].is_some() {
                rings.into_iter().for_each(|r| r.free(&mut xhci.dma));
                return Err(Error::Protocol("an endpoint configured twice"));
            }
            self.input.endpoint(e, speed, r.base());
            self.input.add(dci);
            entries = entries.max(dci);
        }
        self.write_slot(entries);
        if let Err(e) = xhci.command(Trb::configure_endpoint(self.input.phys(), self.slot)) {
            rings.into_iter().for_each(|r| r.free(&mut xhci.dma));
            return Err(e);
        }
        for (e, r) in endpoints.iter().zip(rings) {
            self.endpoints[context::dci(e) as usize] = Some(EndpointRing { ring: r, endpoint: *e, queued: VecDeque::new() });
        }
        Ok(())
    }

    /// Queues a transfer of `len` bytes at `phys` on endpoint `dci`
    /// (interrupt on completion and on a short packet), told back with
    /// `cookie`. The caller rings the doorbell (`kick`) after a batch.
    pub fn submit(&mut self, dci: u8, phys: u64, len: usize, cookie: u32) -> Result<(), Error> {
        let ep = self.endpoints.get_mut(dci as usize).and_then(Option::as_mut).ok_or(Error::Protocol("no such endpoint"))?;
        let flags = trb::IOC | if ep.endpoint.is_in() { trb::ISP } else { 0 };
        let (first, last, trbs) = ep.ring.push(&[Trb::normal(phys, len as u32, flags)]).ok_or(Error::NoRoom)?;
        ep.queued.push_back(Td { first, last, trbs, cookie, len });
        Ok(())
    }

    /// Room on endpoint `dci` for `n` more transfers.
    pub fn has_room(&self, dci: u8, n: usize) -> bool {
        self.endpoints.get(dci as usize).and_then(Option::as_ref).is_some_and(|ep| ep.ring.has_room(n))
    }

    pub fn kick(&self, xhci: &Xhci, dci: u8) {
        xhci.ring_doorbell(self.slot, dci);
    }

    /// A transfer event of this device's (not of EP0: those are waited
    /// for): which queued transfer it completes. An event of a halted
    /// endpoint resets it (`recover`): its other transfers are dropped
    /// (None for them; the caller queues new ones).
    pub fn complete(&mut self, xhci: &mut Xhci, dci: u8, trb: u64, code: u8, residual: u32) -> Option<Completed> {
        let ep = self.endpoints.get_mut(dci as usize).and_then(Option::as_mut)?;
        let i = ep.queued.iter().position(|td| (td.first..=td.last).contains(&trb))?;
        // TDs complete in order: one before it that got no event is gone.
        for td in ep.queued.drain(..i) {
            ep.ring.completed(td.trbs);
        }
        let td = ep.queued.pop_front().expect("found above");
        ep.ring.completed(td.trbs);
        let len = td.len.saturating_sub(residual as usize);
        let result = match code {
            trb::SUCCESS | trb::SHORT_PACKET => Ok(()),
            trb::STALL => Err(Error::Stall),
            code => Err(Error::Code(code)),
        };
        let completed = Completed { dci, cookie: td.cookie, len, result };
        if result.is_err() {
            self.recover(xhci, dci, false);
        }
        Some(completed)
    }

    /// Endpoint `dci` halted (a stall or an error) or a transfer on it
    /// hangs (`stop`): it is reset and continues after everything queued,
    /// which is dropped; a stalled bulk or interrupt endpoint's halt is
    /// cleared on the device too.
    pub fn recover(&mut self, xhci: &mut Xhci, dci: u8, stop: bool) {
        let state = self.output.endpoint_state(dci);
        if stop && state == 1 {
            let _ = xhci.command(Trb::stop_endpoint(self.slot, dci));
        } else if state == 2 || state == 4 {
            let _ = xhci.command(Trb::reset_endpoint(self.slot, dci));
        }
        let (ring, address) = if dci == 1 {
            (&mut self.ep0, None)
        } else {
            match self.endpoints.get_mut(dci as usize).and_then(Option::as_mut) {
                Some(ep) => {
                    ep.queued.clear();
                    (&mut ep.ring, Some(ep.endpoint.address))
                }
                None => return,
            }
        };
        let (dequeue, cycle) = ring.enqueue_pointer();
        ring.reset_used();
        let _ = xhci.command(Trb::set_tr_dequeue(self.slot, dci, dequeue, cycle));
        if let (Some(address), false) = (address, stop) {
            let _ = self.control_out(xhci, SetupPacket::clear_halt(address), &[]);
        }
    }

    /// Disables the slot and gives its memory back. (Its transfers are
    /// gone with it: the caller forgets their buffers.)
    pub fn release(mut self, xhci: &mut Xhci) {
        let _ = xhci.command(Trb::disable_slot(self.slot));
        xhci.set_device_context(self.slot, 0);
        for ep in self.endpoints.iter_mut().filter_map(Option::take) {
            ep.ring.free(&mut xhci.dma);
        }
        self.ep0.free(&mut xhci.dma);
        self.input.free(&mut xhci.dma);
        self.output.free(&mut xhci.dma);
        xhci.dma.free(self.buf);
    }

    // ------------------------------------------------------ descriptors

    /// Configuration descriptor `index`, whole.
    pub fn configuration(&mut self, xhci: &mut Xhci, index: u8) -> Result<descriptor::Configuration, Error> {
        let head = self.control_in(xhci, SetupPacket::get_descriptor(descriptor::CONFIGURATION, index, 0, descriptor::Configuration::HEADER as u16))?;
        let total = descriptor::Configuration::total_length(head).ok_or(Error::Protocol("a malformed configuration descriptor"))?;
        let all = self.control_in(xhci, SetupPacket::get_descriptor(descriptor::CONFIGURATION, index, 0, total.min(PAGE as u16)))?;
        descriptor::Configuration::parse(all).ok_or(Error::Protocol("a malformed configuration descriptor"))
    }

    /// String descriptor `index` in the device's first language.
    pub fn string(&mut self, xhci: &mut Xhci, index: u8) -> Result<alloc::string::String, Error> {
        if index == 0 {
            return Err(Error::Protocol("no string"));
        }
        let languages = descriptor::languages(self.control_in(xhci, SetupPacket::get_descriptor(descriptor::STRING, 0, 0, 255))?);
        let language = languages.first().copied().unwrap_or(0x0409);
        let text = self.control_in(xhci, SetupPacket::get_descriptor(descriptor::STRING, index, language, 255))?;
        descriptor::string(text).ok_or(Error::Protocol("a malformed string descriptor"))
    }

    /// The device's name for messages: "vendor:product" and the product
    /// string if it has one.
    pub fn name(&mut self, xhci: &mut Xhci) -> alloc::string::String {
        let d = self.descriptor;
        let product = if d.product_string != 0 { self.string(xhci, d.product_string).ok() } else { None };
        match product {
            Some(p) => alloc::format!("{:04x}:{:04x} \"{}\"", d.vendor, d.product, p.trim()),
            None => alloc::format!("{:04x}:{:04x}", d.vendor, d.product),
        }
    }
}

/// Waits `ms` milliseconds (yielding).
pub fn pause(ms: u64) {
    let until = oxrt::uptime_ms() + ms;
    while oxrt::uptime_ms() < until {
        oxrt::sched_yield();
    }
}
