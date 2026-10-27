//! Device and input contexts (xHCI 1.2, 6.2): the slot context and the
//! endpoint contexts the controller keeps per device, and the input
//! context software hands it in commands. Contexts are 32 or 64 bytes
//! (HCCPARAMS1.CSZ); the fields used are in their first 32.

use crate::dma::{Buf, Dma};
use usb::descriptor::{Endpoint, BULK, CONTROL, INTERRUPT, ISOCHRONOUS};
use usb::Speed;

/// Endpoint types (the endpoint context's EP Type).
pub const EP_ISOCH_OUT: u32 = 1;
pub const EP_BULK_OUT: u32 = 2;
pub const EP_INTERRUPT_OUT: u32 = 3;
pub const EP_CONTROL: u32 = 4;
pub const EP_ISOCH_IN: u32 = 5;
pub const EP_BULK_IN: u32 = 6;
pub const EP_INTERRUPT_IN: u32 = 7;

/// The device context index of an endpoint (EP0: 1; OUT n: 2n; IN n:
/// 2n + 1).
pub fn dci(endpoint: &Endpoint) -> u8 {
    2 * endpoint.number() + endpoint.is_in() as u8
}

/// A slot's input context: the input control context, the slot context,
/// 31 endpoint contexts.
pub struct Input {
    pub buf: Buf,
    size: usize,
}

impl Input {
    pub fn new(dma: &mut Dma, wide: bool) -> Option<Input> {
        Some(Input { buf: dma.alloc(1)?, size: if wide { 64 } else { 32 } })
    }

    pub fn phys(&self) -> u64 {
        self.buf.phys()
    }

    /// Starts a new command's input: nothing to add or drop, the contexts
    /// zero.
    pub fn clear(&mut self) {
        self.buf.zero();
    }

    /// Adds context `index` (0: slot, else a DCI) to the command.
    pub fn add(&mut self, index: u8) {
        let add = self.buf.read32(4);
        self.buf.write32(4, add | 1 << index);
    }

    /// Dword `n` of context `index` (0: slot, else a DCI).
    fn offset(&self, index: u8, n: usize) -> usize {
        self.size * (1 + index as usize) + 4 * n
    }

    pub fn set(&mut self, index: u8, n: usize, value: u32) {
        let at = self.offset(index, n);
        self.buf.write32(at, value);
    }

    /// The slot context: `route` (the route string below the root port),
    /// `speed`, `entries` (the highest DCI in use), the root hub port, and
    /// for a hub its ports and, for a high-speed hub, its TT think time;
    /// `tt`: the high-speed hub slot and port a low- or full-speed device
    /// behind a high-speed hub reaches the root through.
    #[allow(clippy::too_many_arguments)]
    pub fn slot(&mut self, route: u32, speed: Speed, entries: u8, root_port: u8, hub: Option<(u8, u8, bool)>, tt: Option<(u8, u8)>) {
        let mut d0 = route & 0xf_ffff | (speed.psiv() as u32) << 20 | (entries as u32) << 27;
        let mut d1 = (root_port as u32) << 16;
        let mut d2 = 0;
        if let Some((ports, think, multi_tt)) = hub {
            d0 |= 1 << 26 | (multi_tt as u32) << 25;
            d1 |= (ports as u32) << 24;
            d2 |= (think as u32 & 0b11) << 16;
        }
        if let Some((hub_slot, hub_port)) = tt {
            d2 |= hub_slot as u32 | (hub_port as u32) << 8;
        }
        self.set(0, 0, d0);
        self.set(0, 1, d1);
        self.set(0, 2, d2);
        self.set(0, 3, 0);
    }

    /// Endpoint context `dci` for EP0 with packets of `max_packet` bytes,
    /// its ring at `ring` (cycle state 1).
    pub fn control_endpoint(&mut self, max_packet: u16, ring: u64) {
        self.endpoint_raw(1, EP_CONTROL, max_packet, 0, 0, 0, 0, ring, 8);
    }

    /// The endpoint context of `endpoint` of a device at `speed`, its ring
    /// at `ring`.
    pub fn endpoint(&mut self, endpoint: &Endpoint, speed: Speed, ring: u64) {
        let kind = match (endpoint.kind(), endpoint.is_in()) {
            (BULK, true) => EP_BULK_IN,
            (BULK, false) => EP_BULK_OUT,
            (INTERRUPT, true) => EP_INTERRUPT_IN,
            (INTERRUPT, false) => EP_INTERRUPT_OUT,
            (ISOCHRONOUS, true) => EP_ISOCH_IN,
            (ISOCHRONOUS, false) => EP_ISOCH_OUT,
            (CONTROL, _) => EP_CONTROL,
            _ => unreachable!("a two-bit field"),
        };
        let packet = endpoint.packet_size();
        // Bursts: USB 3's companion, high speed's extra transactions of a
        // periodic endpoint; none for full and low speed.
        let burst = match speed {
            Speed::Super | Speed::SuperPlus => endpoint.max_burst,
            Speed::High if matches!(endpoint.kind(), INTERRUPT | ISOCHRONOUS) => endpoint.mult(),
            _ => 0,
        };
        let periodic = matches!(endpoint.kind(), INTERRUPT | ISOCHRONOUS);
        let interval = if periodic { interval(endpoint, speed) } else { 0 };
        let esit = if periodic { packet as u32 * (burst as u32 + 1) } else { 0 };
        let average = if periodic { esit.min(1024) as u16 } else { 3072 };
        self.endpoint_raw(dci(endpoint), kind, packet, burst, interval, esit, 0, ring, average);
    }

    #[allow(clippy::too_many_arguments)]
    fn endpoint_raw(&mut self, dci: u8, kind: u32, max_packet: u16, burst: u8, interval: u8, esit: u32, mult: u8, ring: u64, average: u16) {
        let d0 = (mult as u32) << 8 | (interval as u32) << 16 | (esit >> 16) << 24;
        // Three retries on errors (CErr), as the specification recommends.
        let d1 = 3 << 1 | kind << 3 | (burst as u32) << 8 | (max_packet as u32) << 16;
        self.set(dci, 0, d0);
        self.set(dci, 1, d1);
        self.set(dci, 2, ring as u32 | 1);
        self.set(dci, 3, (ring >> 32) as u32);
        self.set(dci, 4, average as u32 | (esit & 0xffff) << 16);
    }

    /// Changes EP0's packet size (for Evaluate Context, after the device
    /// descriptor's first 8 bytes).
    pub fn control_packet(&mut self, max_packet: u16) {
        let d1 = 3 << 1 | EP_CONTROL << 3 | (max_packet as u32) << 16;
        self.set(1, 1, d1);
    }

    pub fn free(self, dma: &mut Dma) {
        dma.free(self.buf);
    }
}

/// An endpoint's polling interval as the endpoint context encodes it
/// (2^interval units of 125 µs, xHCI 1.2, 6.2.3.6).
fn interval(endpoint: &Endpoint, speed: Speed) -> u8 {
    let b = endpoint.interval.max(1);
    match (speed, endpoint.kind()) {
        // bInterval is already an exponent (1-16).
        (Speed::High | Speed::Super | Speed::SuperPlus, _) | (Speed::Full, ISOCHRONOUS) => {
            let base = b.min(16) - 1;
            if speed == Speed::Full { base + 3 } else { base }
        }
        // Frames (milliseconds): the largest power of two of microframes
        // not above them, at least 8 (one frame).
        _ => {
            let micro = b as u32 * 8;
            (31 - micro.leading_zeros()).clamp(3, 10) as u8
        }
    }
}

/// A slot's output (device) context, which the controller owns.
pub struct Output {
    pub buf: Buf,
    size: usize,
}

impl Output {
    pub fn new(dma: &mut Dma, wide: bool) -> Option<Output> {
        Some(Output { buf: dma.alloc(1)?, size: if wide { 64 } else { 32 } })
    }

    pub fn phys(&self) -> u64 {
        self.buf.phys()
    }

    /// Endpoint `dci`'s state (dword 0, bits 2:0: 1 running, 2 halted,
    /// 3 stopped, 4 error).
    pub fn endpoint_state(&self, dci: u8) -> u8 {
        (self.buf.read32(self.size * dci as usize) & 0b111) as u8
    }

    pub fn free(self, dma: &mut Dma) {
        dma.free(self.buf);
    }
}
