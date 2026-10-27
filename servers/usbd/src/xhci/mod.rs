//! The xHCI host controller (eXtensible Host Controller Interface 1.2):
//! taking it over from the firmware, resetting and starting it, its
//! command ring, its event ring and interrupter 0, its root hub's ports
//! and doorbells. Devices and their transfers are `device`'s.
//!
//! usbd is one thread: a call that needs an answer (a command, a control
//! transfer) waits for its event and keeps every other event it takes
//! meanwhile (`pending`) for the event loop, which handles them in order
//! after the call returns.

pub mod context;
pub mod regs;
pub mod ring;
pub mod trb;

use crate::dma::{self, Buf, Dma};
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use oxrt::println;
use regs::*;
use ring::{EventRing, Ring};
use trb::{Event, Trb};

/// Device slots usbd enables at most (of the controller's MaxSlots).
const MAX_SLOTS: u8 = 64;
/// How long a command or a control transfer may take.
pub const COMMAND_TIMEOUT_MS: u64 = 5000;
/// Interrupt moderation: at most one interrupt per this many 250 ns
/// (250 µs).
const IMOD_INTERVAL: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No answer in time (the device or the controller hangs, or the
    /// device went).
    Timeout,
    /// The controller completed it with this code (`trb::code_name`).
    Code(u8),
    /// The device stalled the request.
    Stall,
    /// No DMA memory, ring room or slot left.
    NoRoom,
    /// The device answered something it must not.
    Protocol(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Error::Timeout => write!(f, "timed out"),
            Error::Code(c) => write!(f, "{} (completion code {})", trb::code_name(*c), c),
            Error::Stall => write!(f, "stalled"),
            Error::NoRoom => write!(f, "out of DMA memory or ring room"),
            Error::Protocol(what) => write!(f, "{}", what),
        }
    }
}

pub struct Xhci {
    op: Mmio,
    rt: Mmio,
    db: Mmio,
    pub dma: Dma,
    /// 64-byte contexts.
    pub wide: bool,
    /// Root hub ports, and which are USB 3's (Supported Protocol, major
    /// revision 3).
    pub ports: u8,
    usb3: [bool; 256],
    pub max_slots: u8,
    dcbaa: Buf,
    _scratchpad: Option<(Buf, Vec<Buf>)>,
    commands: Ring,
    events: EventRing,
    /// Events taken while a call waited for another one, oldest first.
    pending: VecDeque<Event>,
    /// The interrupt line (None: polled).
    irq: Option<u8>,
    /// A doorbell (a link channel's) rang while a call waited.
    pub doorbell: bool,
}

/// Value of `deadline` milliseconds from now.
pub fn deadline(ms: u64) -> u64 {
    oxrt::uptime_ms() + ms
}

impl Xhci {
    /// Takes the controller in `bar` over from the firmware, resets it and
    /// starts it, with everything it needs from `dma`.
    pub fn new(bar: Mmio, mut dma: Dma, irq: Option<u8>) -> Result<Xhci, &'static str> {
        let caplength = bar.read32(CAPLENGTH) as u8 as usize;
        let hcs1 = bar.read32(HCSPARAMS1);
        let hcs2 = bar.read32(HCSPARAMS2);
        let hcc1 = bar.read32(HCCPARAMS1);
        let op = bar.at(caplength).ok_or("the operational registers are outside the BAR")?;
        let rt = bar.at(bar.read32(RTSOFF) as usize & !0x1f).ok_or("the runtime registers are outside the BAR")?;
        let db = bar.at(bar.read32(DBOFF) as usize & !0x3).ok_or("the doorbells are outside the BAR")?;
        let (ac64, wide) = (hcc1 & HCC_AC64 != 0, hcc1 & HCC_CSZ != 0);
        if !ac64 && dma.end() > 1 << 32 {
            return Err("the controller reaches only 4 GiB and the DMA area is above");
        }
        let ports = (hcs1 >> 24) as u8;
        let max_slots = (hcs1 as u8).min(MAX_SLOTS);
        if op.read32(PAGESIZE) & 1 == 0 {
            return Err("the controller does not take 4 KiB pages");
        }
        let mut usb3 = [false; 256];
        take_over(&bar, hcc1, &mut usb3);

        // Stop it (the firmware may have left it running), then reset it.
        op.write32(USBCMD, op.read32(USBCMD) & !(CMD_RUN | CMD_INTE));
        wait_for(|| op.read32(USBSTS) & STS_HALTED != 0, 32).ok_or("the controller does not halt")?;
        op.write32(USBCMD, CMD_RESET);
        // (Some Intel controllers hang if read too soon after the reset.)
        let start = oxrt::uptime_ms();
        while oxrt::uptime_ms() < start + 2 {
            oxrt::sched_yield();
        }
        wait_for(|| op.read32(USBCMD) & CMD_RESET == 0 && op.read32(USBSTS) & STS_CNR == 0, 1000).ok_or("the controller does not come out of reset")?;

        op.write32(CONFIG, (op.read32(CONFIG) & !0xff) | max_slots as u32);
        let mut dcbaa = dma.alloc(1).ok_or("no DMA memory for the device context table")?;
        // Scratchpad buffers the controller asked for (pointed to by entry 0).
        let scratch_count = ((hcs2 >> 21 & 0x1f) << 5 | hcs2 >> 27) as usize;
        let scratchpad = if scratch_count > 0 {
            let mut array = dma.alloc_bytes(8 * scratch_count).ok_or("no DMA memory for the scratchpad array")?;
            let mut pages = Vec::new();
            for i in 0..scratch_count {
                let page = dma.alloc(1).ok_or("no DMA memory for the scratchpad")?;
                array.write64(8 * i, page.phys());
                pages.push(page);
            }
            dcbaa.write64(0, array.phys());
            Some((array, pages))
        } else {
            None
        };
        dma::release();
        op.write64(DCBAAP, dcbaa.phys());
        let commands = Ring::new(&mut dma).ok_or("no DMA memory for the command ring")?;
        op.write64(CRCR, commands.base() | CRCR_RCS);
        let events = EventRing::new(&mut dma).ok_or("no DMA memory for the event ring")?;
        let ir = rt.at(IR0).expect("interrupter 0 is in the runtime registers");
        ir.write32(ERSTSZ, 1);
        ir.write64(ERDP, events.dequeue_pointer());
        ir.write64(ERSTBA, events.table());
        ir.write32(IMOD, IMOD_INTERVAL);
        ir.write32(IMAN, IMAN_IE | IMAN_IP);
        op.write32(USBCMD, CMD_RUN | CMD_INTE | CMD_HSEE);
        wait_for(|| op.read32(USBSTS) & STS_HALTED == 0, 100).ok_or("the controller does not run")?;
        let xhci = Xhci {
            op,
            rt,
            db,
            dma,
            wide,
            ports,
            usb3,
            max_slots,
            dcbaa,
            _scratchpad: scratchpad,
            commands,
            events,
            pending: VecDeque::new(),
            irq,
            doorbell: false,
        };
        if hcc1 & HCC_PPC != 0 {
            // Ports with power switches: on (each, and their power-on time).
            for port in 1..=ports {
                let sc = xhci.port(port);
                if sc & PORT_PP == 0 {
                    xhci.set_port(port, (sc & PORT_NEUTRAL) | PORT_PP);
                }
            }
            let at = oxrt::uptime_ms();
            while oxrt::uptime_ms() < at + 20 {
                oxrt::sched_yield();
            }
        }
        let version = bar.read32(CAPLENGTH) >> 16;
        let interrupt = match irq {
            Some(line) if line >= 16 => alloc::format!("message-signalled interrupt (line {})", line),
            Some(line) => alloc::format!("interrupt line {}", line),
            None => alloc::string::String::from("no interrupt (polled)"),
        };
        println!(
            "usbd: xHCI {}.{:02x}, {} ports ({} USB 3), {} slots, {}-byte contexts, {} scratchpad pages, {}",
            version >> 8,
            version & 0xff,
            ports,
            (1..=ports).filter(|&p| xhci.usb3[p as usize]).count(),
            max_slots,
            if wide { 64 } else { 32 },
            scratch_count,
            interrupt
        );
        Ok(xhci)
    }

    // ------------------------------------------------------------- ports

    pub fn port(&self, port: u8) -> u32 {
        self.op.read32(PORTS + PORT_STRIDE * (port as usize - 1))
    }

    /// Writes PORTSC (the caller starts from `PORT_NEUTRAL` of a read).
    pub fn set_port(&self, port: u8, value: u32) {
        self.op.write32(PORTS + PORT_STRIDE * (port as usize - 1), value);
    }

    /// Clears the port's change bits that are set (and only those).
    pub fn clear_port_changes(&self, port: u8) -> u32 {
        let sc = self.port(port);
        self.set_port(port, (sc & PORT_NEUTRAL) | (sc & PORT_CHANGES));
        sc
    }

    pub fn is_usb3(&self, port: u8) -> bool {
        self.usb3[port as usize]
    }

    // ------------------------------------------------------- the commands

    /// Device context `slot`'s entry in the table (0 to clear it).
    pub fn set_device_context(&mut self, slot: u8, phys: u64) {
        self.dcbaa.write64(8 * slot as usize, phys);
        dma::release();
    }

    pub fn ring_doorbell(&self, slot: u8, target: u8) {
        dma::release();
        self.db.write32(4 * slot as usize, target as u32);
    }

    /// Runs a command and waits for its completion: the completion's slot
    /// (Enable Slot's new one). A code other than success is an error.
    pub fn command(&mut self, command: Trb) -> Result<u8, Error> {
        let (_, trb, _) = self.commands.push(&[command]).ok_or(Error::NoRoom)?;
        self.ring_doorbell(0, 0);
        let event = self.wait(deadline(COMMAND_TIMEOUT_MS), |e| matches!(e, Event::Command { trb: t, .. } if *t == trb))?;
        self.commands.completed(1);
        match event {
            Event::Command { code: trb::SUCCESS, slot, .. } => Ok(slot),
            Event::Command { code, .. } => Err(Error::Code(code)),
            _ => unreachable!("the predicate took a command completion"),
        }
    }

    // ---------------------------------------------------------- the events

    /// The next event: those a call kept first, then the ring's.
    pub fn next_event(&mut self) -> Option<Event> {
        if let Some(e) = self.pending.pop_front() {
            return Some(e);
        }
        self.take_event()
    }

    fn take_event(&mut self) -> Option<Event> {
        let t = self.events.next()?;
        let ir = self.rt.at(IR0).expect("interrupter 0");
        ir.write64(ERDP, self.events.dequeue_pointer() | ERDP_EHB);
        Some(Event::decode(&t))
    }

    /// Waits until an event `want` takes comes, at most until `deadline`
    /// (uptime in milliseconds), keeping the others for the loop.
    pub fn wait(&mut self, deadline: u64, mut want: impl FnMut(&Event) -> bool) -> Result<Event, Error> {
        loop {
            while let Some(e) = self.take_event() {
                if want(&e) {
                    return Ok(e);
                }
                self.pending.push_back(e);
            }
            if oxrt::uptime_ms() >= deadline {
                return Err(Error::Timeout);
            }
            self.sleep(Some(1));
        }
    }

    /// Whether events wait (kept ones or the ring's).
    pub fn has_events(&mut self) -> bool {
        if !self.pending.is_empty() {
            return true;
        }
        match self.take_event() {
            Some(e) => {
                self.pending.push_back(e);
                true
            }
            None => false,
        }
    }

    /// Sleeps until the controller's interrupt, a doorbell, or `timeout_ms`
    /// (None: no limit; a controller without an interrupt line is polled
    /// every 10 ms).
    pub fn sleep(&mut self, timeout_ms: Option<u64>) {
        let timeout = match self.irq {
            Some(_) => timeout_ms,
            None => Some(timeout_ms.unwrap_or(10).min(10)),
        };
        let mut message = [0u8; 64];
        match oxrt::ipc_receive(&mut message, timeout) {
            Ok(oxrt::Event::Interrupt(_)) => self.ack_interrupt(),
            Ok(oxrt::Event::Doorbell) => self.doorbell = true,
            // usbd takes no requests.
            Ok(oxrt::Event::Request(id, _) | oxrt::Event::Control(id, _)) => {
                let _ = oxrt::ipc_reply(id, &(-38i64).to_le_bytes());
            }
            Ok(oxrt::Event::Timeout) | Err(_) => {}
        }
    }

    /// The interrupt was taken: its pending bits clear (IMAN.IP, then
    /// USBSTS.EINT), a legacy line opens again.
    fn ack_interrupt(&mut self) {
        let ir = self.rt.at(IR0).expect("interrupter 0");
        ir.write32(IMAN, IMAN_IE | IMAN_IP);
        self.op.write32(USBSTS, STS_EINT);
        if let Some(line) = self.irq {
            let _ = oxrt::irq_enable(line);
        }
        let sts = self.op.read32(USBSTS);
        if sts & (STS_HSE | STS_HCE) != 0 {
            println!("usbd: the controller reports a {} error", if sts & STS_HCE != 0 { "host controller" } else { "host system" });
        }
    }

    /// Whether the controller stopped working (a host system or controller
    /// error, or halted).
    pub fn failed(&self) -> bool {
        self.op.read32(USBSTS) & (STS_HSE | STS_HCE | STS_HALTED) != 0
    }
}

/// Spins (yielding) until `done` or `ms` milliseconds passed.
fn wait_for(mut done: impl FnMut() -> bool, ms: u64) -> Option<()> {
    let until = oxrt::uptime_ms() + ms;
    loop {
        if done() {
            return Some(());
        }
        if oxrt::uptime_ms() > until {
            return None;
        }
        oxrt::sched_yield();
    }
}

/// Walks the extended capabilities: the firmware's ownership is handed
/// over (USB Legacy Support: the OS-owned semaphore set, at most a second
/// for the firmware to let go, its SMIs off), and the ports of each
/// Supported Protocol are noted (USB 3's in `usb3`).
fn take_over(bar: &Mmio, hcc1: u32, usb3: &mut [bool; 256]) {
    let mut at = (hcc1 >> 16) as usize * 4;
    for _ in 0..64 {
        if at == 0 || at + 16 > bar.len() {
            break;
        }
        let head = bar.read32(at);
        match head as u8 {
            EXT_LEGACY => {
                if head & LEGACY_BIOS_OWNED != 0 {
                    bar.write32(at, head | LEGACY_OS_OWNED);
                    if wait_for(|| bar.read32(at) & LEGACY_BIOS_OWNED == 0, 1000).is_none() {
                        println!("usbd: the firmware does not let go of the controller; taking it anyway");
                        bar.write32(at, (bar.read32(at) & !LEGACY_BIOS_OWNED) | LEGACY_OS_OWNED);
                    }
                }
                let ctl = bar.read32(at + 4);
                bar.write32(at + 4, (ctl & !LEGACY_SMI_ENABLES) | LEGACY_SMI_STATUS);
            }
            EXT_PROTOCOL => {
                let major = head >> 24;
                let ports = bar.read32(at + 8);
                let (first, count) = (ports as u8 as usize, (ports >> 8) as u8 as usize);
                if major == 3 {
                    for p in first..(first + count).min(256) {
                        usb3[p] = true;
                    }
                }
            }
            _ => {}
        }
        let next = (head >> 8 & 0xff) as usize * 4;
        if next == 0 {
            break;
        }
        at += next;
    }
}
