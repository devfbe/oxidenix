//! USB keyboards in the HID boot protocol (`usb::hid`): their reports
//! become the PS/2 keyboard's scancodes on the console (the kernel's
//! `kbd_input`), with the autorepeat a PS/2 keyboard makes itself and the
//! LEDs the console's keyboard shows (`kbd_state`). Without this a USB
//! keyboard would stop working once usbd takes the controller from the
//! firmware (whose legacy emulation made it a PS/2 one).

use crate::device::{Completed, Device};
use crate::dma::{self, Buf};
use crate::xhci::{Error, Xhci};
use usb::descriptor::{Configuration, Interface, INTERRUPT};
use usb::hid;
use usb::SetupPacket;

pub struct Keyboard {
    interface: u8,
    dci: u8,
    report: Buf,
    state: hid::Keyboard,
    repeat: hid::Repeat,
    /// The LEDs last set (the output report's bits).
    leds: Option<u8>,
    /// The typematic settings: delay, period (ms).
    typematic: (u64, u64),
}

/// The configuration's boot keyboard interface, with its report endpoint.
pub fn find(config: &Configuration) -> Option<&Interface> {
    config
        .interfaces
        .iter()
        .find(|i| (i.class, i.subclass, i.protocol) == (hid::CLASS_HID, hid::SUBCLASS_BOOT, hid::PROTOCOL_KEYBOARD) && i.endpoint(INTERRUPT, true).is_some())
}

impl Keyboard {
    /// Binds the boot keyboard `interface` of `dev` (its configuration set
    /// already): its endpoint, the boot protocol, no idle reports, the
    /// first report queued.
    pub fn bind(xhci: &mut Xhci, dev: &mut Device, interface: &Interface) -> Result<Keyboard, Error> {
        let endpoint = interface.endpoint(INTERRUPT, true).ok_or(Error::Protocol("a keyboard without a report endpoint"))?;
        dev.configure(xhci, &[endpoint])?;
        let n = interface.number;
        dev.control_out(xhci, SetupPacket::class_interface(hid::SET_PROTOCOL, hid::BOOT_PROTOCOL, n, 0, false), &[])?;
        // (Optional: a keyboard that has no idle rate stalls.)
        match dev.control_out(xhci, SetupPacket::class_interface(hid::SET_IDLE, 0, n, 0, false), &[]) {
            Ok(()) | Err(Error::Stall) => {}
            Err(e) => return Err(e),
        }
        let report = xhci.dma.alloc(1).ok_or(Error::NoRoom)?;
        let dci = crate::xhci::context::dci(&endpoint);
        let k = Keyboard { interface: n, dci, report, state: hid::Keyboard::default(), repeat: hid::Repeat::default(), leds: None, typematic: hid::typematic(0) };
        dev.submit(dci, k.report.phys(), hid::REPORT_BYTES, 0)?;
        dev.kick(xhci, dci);
        Ok(k)
    }

    pub fn dci(&self) -> u8 {
        self.dci
    }

    /// A report came: its keys typed, the next one queued.
    pub fn completed(&mut self, xhci: &mut Xhci, dev: &mut Device, c: Completed) {
        if c.result.is_ok() && c.len >= hid::REPORT_BYTES {
            dma::acquire();
            let mut r = [0u8; hid::REPORT_BYTES];
            r.copy_from_slice(&self.report.bytes()[..hid::REPORT_BYTES]);
            let held = self.state.report(&r, type_codes);
            self.repeat.held(held, oxrt::uptime_ms(), self.typematic.0);
        }
        let _ = dev.submit(self.dci, self.report.phys(), hid::REPORT_BYTES, 0);
        dev.kick(xhci, self.dci);
    }

    /// The autorepeat that is due, and the console's LEDs and typematic
    /// settings followed.
    pub fn tick(&mut self, xhci: &mut Xhci, dev: &mut Device) {
        if let Some(make) = self.repeat.due(oxrt::uptime_ms(), self.typematic.1) {
            type_codes(make);
        }
        let Ok((leds, typematic)) = oxrt::kbd_state() else { return };
        self.typematic = hid::typematic(typematic);
        let leds = hid::leds_from_ps2(leds);
        if self.leds != Some(leds) {
            // (A keyboard without LEDs may stall the report: no matter.)
            let _ = dev.control_out(xhci, SetupPacket::class_interface(hid::SET_REPORT, hid::OUTPUT_REPORT, self.interface, 1, false), &[leds]);
            self.leds = Some(leds);
        }
    }

    /// When the next autorepeat is due (uptime, ms).
    pub fn deadline(&self) -> Option<u64> {
        self.repeat.deadline()
    }

    /// The keyboard went: whatever it held is let go on the console.
    pub fn release(mut self, xhci: &mut Xhci) {
        self.state.release_all(type_codes);
        xhci.dma.free(self.report);
    }
}

fn type_codes(codes: &[u8]) {
    let _ = oxrt::kbd_input(codes);
}
