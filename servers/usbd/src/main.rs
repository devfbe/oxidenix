//! usbd: the USB server. One process per xHCI controller (the kernel
//! starts "usb0", "usb1", ...: `kernel/src/main.rs::start_usbd`): it
//! drives the controller from user space (`xhci`), enumerates the devices
//! on its ports and below hubs (`device`, `hub`), and binds the Ethernet
//! functions it finds (USB tethering: CDC-ECM, CDC-NCM, RNDIS, `net`),
//! each offered to netd as a link over a channel of its own (the link
//! protocol, `linkring`). ADR 0025.
//!
//! One thread, one event loop: each round takes the controller's events
//! (transfers done, ports changed), moves frames from the links' send
//! rings into transfers, and rings netd's doorbells for what it put in the
//! links' receive rings. With nothing to do it arms each link's doorbell
//! watch and sleeps in `ipc_receive` until the controller's interrupt, a
//! doorbell, or a second (when it looks at the root ports again: a lost
//! port event costs a second, not a device).

#![no_std]
#![no_main]

extern crate alloc;

mod device;
mod dma;
mod hub;
mod kbd;
mod net;
mod xhci;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use device::{pause, Device, Position};
use hub::Hub;
use net::Net;
use oxrt::println;
use usb::function::{self, Kind};
use usb::{SetupPacket, Speed};
use xhci::regs::*;
use xhci::trb::Event;
use xhci::{Error, Xhci};

oxrt::entry!(main);

/// Value of `key=...` among the arguments the kernel passed.
fn arg<'a>(args: &[&'a str], key: &str) -> Option<&'a str> {
    args.iter().find_map(|a| a.strip_prefix(key)?.strip_prefix('='))
}

fn number(v: &str) -> Option<u64> {
    match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

/// How long a root port's connection must hold before the device is
/// reset (USB 2.0, 7.1.7.3: TATTDB, 100 ms).
const DEBOUNCE_MS: u64 = 100;
/// A root port's reset, and a USB 3 port's link training after a
/// connect, may take this long.
const PORT_RESET_MS: u64 = 500;
/// The most configurations of a device usbd looks at.
const MAX_CONFIGURATIONS: u8 = 8;
/// The sleep's limit: the root ports are looked at this often.
const RESCAN_MS: u64 = 1000;

/// What usbd does with a device.
enum Role {
    Net(Net),
    Hub(Hub),
    Keyboard(kbd::Keyboard),
    /// Nothing usbd drives: it keeps its slot (to notice it go).
    Unused,
}

struct Attached {
    dev: Device,
    role: Role,
    /// The hub it hangs on (None: a root port).
    parent: Option<u8>,
    name: String,
}

struct Usbd {
    xhci: Xhci,
    /// The attached devices, by slot.
    devices: BTreeMap<u8, Attached>,
    /// The device on each root port (by slot).
    roots: [Option<u8>; 256],
    /// Root ports whose device did not come up: tried again only when
    /// it is plugged in anew (a connect change), not at every look.
    failed: [bool; 256],
    /// The kernel runs its self-tests: a device whose serial number is
    /// "rndis" gets its RNDIS function where it offers another too (QEMU's
    /// usb-net offers ECM first), so that both are tested.
    test: bool,
}

fn main(args: Vec<&'static str>) -> i32 {
    let name = arg(&args, "name").unwrap_or("usbd");
    let irq = arg(&args, "irq").and_then(number).map(|l| l as u8);
    let (bar, len) = match oxrt::mmio_map(0) {
        Ok(b) => b,
        Err(e) => {
            println!("usbd: cannot map the controller's registers (errno {})", -e);
            return 1;
        }
    };
    let Ok((base, phys)) = oxrt::dma_map() else {
        println!("usbd: no DMA memory");
        return 1;
    };
    if let Some(line) = irq {
        if oxrt::irq_enable(line).is_err() {
            println!("usbd: cannot take interrupt line {}", line);
            return 1;
        }
    }
    let regs = xhci::regs::Mmio::new(bar, len as usize);
    let xhci = match Xhci::new(regs, dma::Dma::new(base, phys), irq) {
        Ok(x) => x,
        Err(e) => {
            println!("usbd: {} ({}): {}", name, arg(&args, "pci").unwrap_or("?"), e);
            return 1;
        }
    };
    let mut usbd = Usbd { xhci, devices: BTreeMap::new(), roots: [None; 256], failed: [false; 256], test: args.contains(&"test=1") };
    // The kernel waits for the registration: the devices there at boot
    // are enumerated first, so their links come before the programs.
    usbd.scan_roots();
    if let Err(e) = oxrt::ipc_register(name, 0) {
        println!("usbd: cannot register: {}", e);
        return 1;
    }
    usbd.run()
}

impl Usbd {
    fn run(&mut self) -> ! {
        let mut last_scan = oxrt::uptime_ms();
        loop {
            let mut progress = false;
            while let Some(e) = self.xhci.next_event() {
                self.event(e);
                progress = true;
            }
            let mut due = None::<u64>;
            for a in self.devices.values_mut() {
                match &mut a.role {
                    Role::Net(n) => {
                        n.check_link();
                        progress |= n.pump(&mut self.xhci, &mut a.dev);
                        n.flush();
                    }
                    Role::Keyboard(k) => {
                        k.tick(&mut self.xhci, &mut a.dev);
                        due = match (due, k.deadline()) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                    }
                    Role::Hub(_) | Role::Unused => {}
                }
            }
            if oxrt::uptime_ms() >= last_scan + RESCAN_MS {
                last_scan = oxrt::uptime_ms();
                self.scan_roots();
            }
            if progress || core::mem::take(&mut self.xhci.doorbell) {
                continue;
            }
            // Asleep only if no link has frames for us (each watch armed).
            let nets = self.devices.values().filter_map(|a| match &a.role {
                Role::Net(n) => Some(n),
                _ => None,
            });
            let mut armed = true;
            for n in nets {
                if n.has_work() || n.link().is_some_and(|l| !l.prepare_sleep()) {
                    armed = false;
                    break;
                }
            }
            if armed && !self.xhci.has_events() {
                // Until the next look at the ports, or a keyboard's repeat.
                let until = due.map_or(last_scan + RESCAN_MS, |d| d.min(last_scan + RESCAN_MS));
                self.xhci.sleep(Some(until.saturating_sub(oxrt::uptime_ms()).max(1)));
            }
            for a in self.devices.values() {
                if let Role::Net(n) = &a.role {
                    if let Some(l) = n.link() {
                        l.awake();
                    }
                }
            }
            if self.xhci.failed() {
                println!("usbd: the controller stopped; giving up its devices");
                oxrt::exit(1);
            }
        }
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::Transfer { slot, dci, trb, code, residual } => {
                let Some(a) = self.devices.get_mut(&slot) else { return };
                let Some(c) = a.dev.complete(&mut self.xhci, dci, trb, code, residual) else { return };
                match &mut a.role {
                    Role::Net(n) => n.completed(&mut self.xhci, &mut a.dev, c),
                    Role::Keyboard(k) if dci == k.dci() => k.completed(&mut self.xhci, &mut a.dev, c),
                    Role::Hub(h) if dci == h.status_dci() => {
                        let changed = h.changed(&mut self.xhci, &mut a.dev, c.result.is_ok());
                        self.hub_changed(slot, changed);
                    }
                    _ => {}
                }
            }
            Event::Port { port } => self.root_port(port),
            Event::Controller { code } => println!("usbd: controller event (completion code {})", code),
            // (A command's completion that came after it timed out.)
            Event::Command { .. } | Event::Other { .. } => {}
        }
    }

    // ------------------------------------------------------- root ports

    /// Looks at every root port.
    fn scan_roots(&mut self) {
        for port in 1..=self.xhci.ports {
            self.root_port(port);
        }
    }

    /// A root port changed (or is looked at): a device that came is
    /// attached, one that went is detached.
    fn root_port(&mut self, port: u8) {
        let sc = self.xhci.clear_port_changes(port);
        let present = self.roots[port as usize];
        let connected = sc & PORT_CCS != 0;
        let failed = &mut self.failed[port as usize];
        if !connected || sc & PORT_CSC != 0 {
            *failed = false;
        }
        if *failed {
            return;
        }
        match (connected, present) {
            (false, Some(slot)) => self.detach(slot),
            // A port that was reset or disabled under the device: it is
            // gone as far as usbd knows; a new one comes from the reset.
            (true, Some(slot)) if sc & PORT_PED == 0 || sc & PORT_CSC != 0 => {
                self.detach(slot);
                self.connect_root(port);
            }
            (true, None) => self.connect_root(port),
            _ => {}
        }
    }

    fn connect_root(&mut self, port: u8) {
        pause(DEBOUNCE_MS);
        if self.xhci.port(port) & PORT_CCS == 0 {
            return;
        }
        let speed = match self.reset_root(port) {
            Ok(speed) => speed,
            Err(e) => {
                println!("usbd: port {}: the device does not come up ({})", port, e);
                self.failed[port as usize] = true;
                return;
            }
        };
        let position = Position { root_port: port, route: 0, tt: None, depth: 0 };
        match self.attach(position, speed, None) {
            Some(slot) => self.roots[port as usize] = Some(slot),
            None => self.failed[port as usize] = true,
        }
    }

    /// Brings root port `port` up: a USB 2 port is reset; a USB 3 port
    /// trains its link by itself (a warm reset if it is stuck). The
    /// device's speed.
    fn reset_root(&mut self, port: u8) -> Result<Speed, Error> {
        let x = &self.xhci;
        let until = xhci::deadline(PORT_RESET_MS);
        if x.is_usb3(port) {
            loop {
                let sc = x.port(port);
                if sc & PORT_PED != 0 {
                    break;
                }
                let pls = (sc & PORT_PLS_MASK) >> PORT_PLS_SHIFT;
                if matches!(pls, PLS_INACTIVE | PLS_COMPLIANCE) {
                    x.set_port(port, (sc & PORT_NEUTRAL) | PORT_WPR);
                }
                if oxrt::uptime_ms() > until {
                    return Err(Error::Timeout);
                }
                pause(5);
            }
        } else {
            let sc = x.port(port);
            x.set_port(port, (sc & PORT_NEUTRAL) | PORT_PR);
            loop {
                pause(5);
                let sc = x.port(port);
                if sc & PORT_CCS == 0 {
                    return Err(Error::Protocol("the device went during its reset"));
                }
                if sc & PORT_PR == 0 && sc & PORT_PED != 0 {
                    break;
                }
                if oxrt::uptime_ms() > until {
                    return Err(Error::Timeout);
                }
            }
            pause(10);
        }
        let sc = x.clear_port_changes(port);
        Speed::from_psiv(((sc & PORT_SPEED_MASK) >> PORT_SPEED_SHIFT) as u8).ok_or(Error::Protocol("an unknown port speed"))
    }

    // --------------------------------------------------------- devices

    /// Attaches the device just reset at `position` and gives it its
    /// role; its slot.
    fn attach(&mut self, position: Position, speed: Speed, parent: Option<u8>) -> Option<u8> {
        let mut dev = match Device::attach(&mut self.xhci, position, speed) {
            Ok(d) => d,
            Err(e) => {
                println!("usbd: {}: cannot address the device ({})", position.name(), e);
                return None;
            }
        };
        let name = dev.name(&mut self.xhci);
        let slot = dev.slot;
        let role = match self.role(&mut dev, &name) {
            Ok(role) => role,
            Err(e) => {
                println!("usbd: {}: {}: {}", position.name(), name, e);
                Role::Unused
            }
        };
        self.devices.insert(slot, Attached { dev, role, parent, name });
        // Devices already on a new hub's ports.
        if matches!(self.devices.get(&slot).map(|a| &a.role), Some(Role::Hub(_))) {
            self.hub_changed(slot, u16::MAX);
        }
        Some(slot)
    }

    /// Chooses the device's configuration and binds what it offers.
    fn role(&mut self, dev: &mut Device, name: &str) -> Result<Role, Error> {
        let count = dev.descriptor.configurations.min(MAX_CONFIGURATIONS);
        let mut configs = Vec::new();
        for i in 0..count {
            configs.push(dev.configuration(&mut self.xhci, i)?);
        }
        let place = dev.position.name();
        if dev.descriptor.class == usb::hub::CLASS_HUB {
            let config = configs.first().ok_or(Error::Protocol("a hub without a configuration"))?;
            let hub = Hub::bind(&mut self.xhci, dev, config)?;
            println!("usbd: {}: {}: a {}-speed hub with {} ports", place, name, dev.speed.name(), hub.ports);
            return Ok(Role::Hub(hub));
        }
        let forced = if self.test && self.serial(dev).as_deref() == Some("rndis") { Some(Kind::Rndis) } else { None };
        let chosen = match forced {
            Some(kind) => configs.iter().enumerate().find_map(|(n, c)| Some((n, function::find_kind(c, kind)?))),
            None => function::choose(&configs),
        };
        let Some((n, f)) = chosen else {
            // A keyboard (in its first configuration, as every one has).
            if let Some(interface) = configs.first().and_then(kbd::find).cloned() {
                dev.control_out(&mut self.xhci, SetupPacket::set_configuration(configs[0].value), &[])?;
                let k = kbd::Keyboard::bind(&mut self.xhci, dev, &interface)?;
                println!("usbd: {}: {} ({}-speed): a keyboard", place, name, dev.speed.name());
                return Ok(Role::Keyboard(k));
            }
            println!("usbd: {}: {} ({}-speed): nothing usbd drives", place, name, dev.speed.name());
            return Ok(Role::Unused);
        };
        dev.control_out(&mut self.xhci, SetupPacket::set_configuration(configs[n].value), &[])?;
        let mut net = Net::bind(&mut self.xhci, dev, f)?;
        net.start(&mut self.xhci, dev)?;
        let mac = net.mac.map(|b| alloc::format!("{b:02x}")).join(":");
        println!("usbd: {}: {} ({}-speed): {} {}, configuration {}", place, name, dev.speed.name(), net.kind.driver(), mac, configs[n].value);
        Ok(Role::Net(net))
    }

    fn serial(&mut self, dev: &mut Device) -> Option<String> {
        let index = dev.descriptor.serial;
        (index != 0).then(|| dev.string(&mut self.xhci, index).ok()).flatten()
    }

    /// The device in `slot` went: its function and (a hub's) the devices
    /// below it go too.
    fn detach(&mut self, slot: u8) {
        let Some(a) = self.devices.remove(&slot) else { return };
        if let Some(port) = self.roots.iter().position(|&s| s == Some(slot)) {
            self.roots[port] = None;
        }
        if let Some(parent) = a.parent.and_then(|p| self.devices.get_mut(&p)) {
            if let Role::Hub(h) = &mut parent.role {
                h.children.iter_mut().filter(|c| **c == Some(slot)).for_each(|c| *c = None);
            }
        }
        println!("usbd: {}: {} gone", a.dev.position.name(), a.name);
        match a.role {
            Role::Net(n) => n.release(&mut self.xhci),
            Role::Keyboard(k) => k.release(&mut self.xhci),
            Role::Hub(h) => {
                for child in h.children.iter().flatten() {
                    self.detach(*child);
                }
                h.release(&mut self.xhci);
            }
            Role::Unused => {}
        }
        a.dev.release(&mut self.xhci);
    }

    // ------------------------------------------------------------- hubs

    /// Ports of the hub in `slot` changed (bit n: port n).
    fn hub_changed(&mut self, slot: u8, ports: u16) {
        let Some(count) = self.devices.get(&slot).and_then(|a| match &a.role {
            Role::Hub(h) => Some(h.ports),
            _ => None,
        }) else {
            return;
        };
        for port in (1..=count).filter(|p| ports & 1 << p != 0) {
            self.hub_port(slot, port);
        }
    }

    fn hub_port(&mut self, slot: u8, port: u8) {
        let Some(a) = self.devices.get_mut(&slot) else { return };
        let Role::Hub(h) = &mut a.role else { return };
        let status = match h.port_status(&mut self.xhci, &mut a.dev, port) {
            Ok(s) => s,
            Err(_) => return,
        };
        let child = h.children[port as usize];
        let changed = status.change & usb::hub::CHANGE_CONNECTION != 0;
        match (status.connected(), child) {
            (false, Some(c)) => self.detach(c),
            (true, Some(c)) if changed => {
                self.detach(c);
                self.connect_hub_port(slot, port);
            }
            (true, None) => self.connect_hub_port(slot, port),
            _ => {}
        }
    }

    fn connect_hub_port(&mut self, slot: u8, port: u8) {
        pause(DEBOUNCE_MS);
        let Some(a) = self.devices.get_mut(&slot) else { return };
        let Role::Hub(h) = &mut a.role else { return };
        let speed = match h.reset_port(&mut self.xhci, &mut a.dev, port) {
            Ok(s) => s,
            Err(e) => {
                println!("usbd: {}.{}: the device does not come up ({})", a.dev.position.name(), port, e);
                return;
            }
        };
        let position = h.child_position(&a.dev, port, speed);
        if let Some(child) = self.attach(position, speed, Some(slot)) {
            if let Some(Attached { role: Role::Hub(h), .. }) = self.devices.get_mut(&slot) {
                h.children[port as usize] = Some(child);
            }
        }
    }
}
