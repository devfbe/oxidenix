//! Which Ethernet function a device offers, and how it is wired: its
//! control interface (and notification endpoint), its data interface's
//! alternate setting with the bulk endpoints, where its MAC address is.
//! The kinds usbd drives are CDC-NCM, CDC-ECM and RNDIS: what Android
//! offers for USB tethering (RNDIS, and NCM on newer devices), what
//! Linux's and many embedded gadgets offer (ECM, NCM) and what QEMU's
//! usb-net is (ECM, and RNDIS in its second configuration).

use crate::cdc;
use crate::descriptor::{Configuration, Endpoint, Interface, BULK, INTERRUPT};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Microsoft's Remote NDIS over a CDC-like pair of interfaces.
    Rndis,
    /// CDC Ethernet Control Model: one frame per transfer.
    Ecm,
    /// CDC Network Control Model: frames in transfer blocks (NTBs).
    Ncm,
}

impl Kind {
    /// Its name in messages and in the link's description (as Linux's
    /// drivers are named).
    pub fn driver(self) -> &'static str {
        match self {
            Kind::Rndis => "rndis_host",
            Kind::Ecm => "cdc_ether",
            Kind::Ncm => "cdc_ncm",
        }
    }
}

/// An Ethernet function of one configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetFunction {
    pub kind: Kind,
    /// The control interface (requests, notifications).
    pub control: u8,
    /// The data interface and the alternate setting that carries frames
    /// (0 for RNDIS; ECM and NCM have none in setting 0, 1 in setting 1).
    pub data: u8,
    pub data_alternate: u8,
    pub bulk_in: Endpoint,
    pub bulk_out: Endpoint,
    /// The control interface's interrupt endpoint (notifications), if it
    /// has one.
    pub notify: Option<Endpoint>,
    /// ECM and NCM: the string descriptor with the MAC address (RNDIS asks
    /// for it with a query instead: 0).
    pub mac_string: u8,
    /// ECM and NCM: the largest frame the function takes (0: unknown).
    pub max_segment: u16,
}

/// Whether an interface is an RNDIS function's control interface: the
/// classes RNDIS devices announce (as Linux's `is_rndis`,
/// `is_wireless_rndis` and `is_activesync`-free rndis_host matches): CDC
/// ACM with Microsoft's vendor protocol, "wireless controller / RF /
/// RNDIS" (Android's), "miscellaneous / RNDIS over Ethernet".
pub fn is_rndis(i: &Interface) -> bool {
    matches!((i.class, i.subclass, i.protocol), (cdc::CLASS_COMM, cdc::SUBCLASS_ACM, 0xff) | (0xe0, 0x01, 0x03) | (0xef, 0x04, 0x01))
}

/// The alternate setting of interface `number` that has a bulk IN and a
/// bulk OUT endpoint (the lowest if several do).
fn bulk_pair(config: &Configuration, number: u8) -> Option<(&Interface, Endpoint, Endpoint)> {
    config
        .interfaces
        .iter()
        .filter(|i| i.number == number)
        .find_map(|i| Some((i, i.endpoint(BULK, true)?, i.endpoint(BULK, false)?)))
}

/// The function whose control interface is `control` (setting 0), if it
/// is one usbd drives and is wired as its kind requires.
fn function_at(config: &Configuration, control: &Interface) -> Option<NetFunction> {
    let kind = if is_rndis(control) {
        Kind::Rndis
    } else if control.class == cdc::CLASS_COMM && control.subclass == cdc::SUBCLASS_ECM {
        Kind::Ecm
    } else if control.class == cdc::CLASS_COMM && control.subclass == cdc::SUBCLASS_NCM {
        Kind::Ncm
    } else {
        return None;
    };
    // The data interface: the union's, else (RNDIS devices that have no
    // union) the next interface.
    let data = match cdc::union(control) {
        Some(u) if u.control == control.number => u.subordinate,
        Some(_) => return None,
        None if kind == Kind::Rndis => control.number.checked_add(1)?,
        None => return None,
    };
    let (data_iface, bulk_in, bulk_out) = bulk_pair(config, data)?;
    if data_iface.class != cdc::CLASS_DATA && !(kind == Kind::Rndis && data_iface.class == 0xff) {
        return None;
    }
    let notify = control.endpoint(INTERRUPT, true);
    let (mac_string, max_segment) = match kind {
        Kind::Rndis => (0, 0),
        // (Without its MAC address a CDC function cannot be used.)
        Kind::Ecm | Kind::Ncm => {
            let eth = cdc::ethernet(control)?;
            (eth.mac_string, eth.max_segment)
        }
    };
    if mac_string == 0 && kind != Kind::Rndis {
        return None;
    }
    Some(NetFunction { kind, control: control.number, data, data_alternate: data_iface.alternate, bulk_in, bulk_out, notify, mac_string, max_segment })
}

/// The best Ethernet function of a configuration: NCM before ECM before
/// RNDIS, then the first.
pub fn find(config: &Configuration) -> Option<NetFunction> {
    config.interfaces.iter().filter(|i| i.alternate == 0).filter_map(|i| function_at(config, i)).fold(None, |best: Option<NetFunction>, f| match best {
        Some(b) if b.kind >= f.kind => Some(b),
        _ => Some(f),
    })
}

/// The configuration to choose among a device's, and its function: the
/// best function of all (as `find` ranks them), then the first
/// configuration with it. A standard function wins over RNDIS where a
/// device offers both (QEMU's usb-net: ECM first, RNDIS second; Linux
/// picks the first non-RNDIS configuration too).
pub fn choose(configs: &[Configuration]) -> Option<(usize, NetFunction)> {
    configs.iter().enumerate().filter_map(|(n, c)| Some((n, find(c)?))).fold(None, |best: Option<(usize, NetFunction)>, (n, f)| match best {
        Some((bn, b)) if b.kind >= f.kind => Some((bn, b)),
        _ => Some((n, f)),
    })
}

/// Whether the configuration holds `kind`'s function (for a choice
/// forced by the kernel's test mode).
pub fn find_kind(config: &Configuration, kind: Kind) -> Option<NetFunction> {
    config.interfaces.iter().filter(|i| i.alternate == 0).filter_map(|i| function_at(config, i)).find(|f| f.kind == kind)
}
