//! RNDIS's control side (`usb::rndis`): commands as encapsulated
//! commands, their completions fetched as encapsulated responses (polled
//! at once, as Linux's rndis_host does: the notification is optional),
//! the device's own messages (media indications, keep-alives) answered
//! whenever they come.

use crate::device::{pause, Device};
use crate::xhci::{Error, Xhci};
use alloc::vec::Vec;
use oxrt::println;
use usb::function::NetFunction;
use usb::rndis::{self, Response};
use usb::SetupPacket;

/// Polls of the response a command waits for (at most this many, a few
/// milliseconds apart).
const POLLS: u32 = 50;

pub use rndis::packets;

pub struct Rndis {
    control: u8,
    request: u32,
    max_frame: u32,
}

impl Rndis {
    /// Initializes the function: (state, its MAC address, whether the
    /// medium is connected).
    pub fn init(xhci: &mut Xhci, dev: &mut Device, f: &NetFunction) -> Result<(Rndis, [u8; 6], bool), Error> {
        let mut r = Rndis { control: f.control, request: 0, max_frame: 1514 };
        match r.command(xhci, dev, |id| rndis::initialize(id, rndis::MAX_TRANSFER))? {
            Response::Initialized { status: rndis::STATUS_SUCCESS, info, .. } => {
                if info.max_transfer != 0 && (info.max_transfer as usize) < rndis::PACKET_HEADER + 1514 {
                    return Err(Error::Protocol("an RNDIS function that takes no full frames"));
                }
            }
            _ => return Err(Error::Protocol("RNDIS initialization failed")),
        }
        let mac = match r.command(xhci, dev, |id| rndis::query(id, rndis::OID_802_3_PERMANENT_ADDRESS))? {
            Response::Queried { status: rndis::STATUS_SUCCESS, info, .. } if info.len() >= 6 => [info[0], info[1], info[2], info[3], info[4], info[5]],
            _ => return Err(Error::Protocol("no MAC address from the RNDIS function")),
        };
        if mac[0] & 1 != 0 || mac == [0; 6] {
            return Err(Error::Protocol("a multicast or zero MAC address"));
        }
        if let Ok(Response::Queried { status: rndis::STATUS_SUCCESS, info, .. }) = r.command(xhci, dev, |id| rndis::query(id, rndis::OID_GEN_MAXIMUM_FRAME_SIZE)) {
            if info.len() >= 4 {
                // (The frame size here is the payload's, without the header.)
                r.max_frame = u32::from_le_bytes(info[..4].try_into().expect("4 bytes")) + 14;
            }
        }
        let filter = rndis::PACKET_TYPE_DIRECTED | rndis::PACKET_TYPE_BROADCAST | rndis::PACKET_TYPE_ALL_MULTICAST;
        match r.command(xhci, dev, |id| rndis::set(id, rndis::OID_GEN_CURRENT_PACKET_FILTER, &filter.to_le_bytes()))? {
            Response::Set { status: rndis::STATUS_SUCCESS, .. } => {}
            _ => return Err(Error::Protocol("the RNDIS function refuses the packet filter")),
        }
        let up = match r.command(xhci, dev, |id| rndis::query(id, rndis::OID_GEN_MEDIA_CONNECT_STATUS)) {
            Ok(Response::Queried { status: rndis::STATUS_SUCCESS, info, .. }) if info.len() >= 4 => u32::from_le_bytes(info[..4].try_into().expect("4 bytes")) == rndis::MEDIA_CONNECTED,
            // (Not answered: assumed connected, as Linux does.)
            _ => true,
        };
        Ok((r, mac, up))
    }

    /// The IP MTU the function's frame size allows.
    pub fn mtu(&self) -> u32 {
        self.max_frame.saturating_sub(14).clamp(576, 1500)
    }

    /// Sends the command `make(request id)` and waits for its completion;
    /// the device's own messages meanwhile are handled.
    fn command(&mut self, xhci: &mut Xhci, dev: &mut Device, make: impl FnOnce(u32) -> Vec<u8>) -> Result<Response, Error> {
        self.request = self.request.wrapping_add(1).max(1);
        let id = self.request;
        let message = make(id);
        let kind = u32::from_le_bytes(message[..4].try_into().expect("4 bytes"));
        dev.control_out(xhci, SetupPacket::class_interface(rndis::SEND_ENCAPSULATED_COMMAND, 0, self.control, 0, false), &message)?;
        for _ in 0..POLLS {
            let Some(r) = self.fetch(xhci, dev)? else {
                pause(2);
                continue;
            };
            let request = match &r {
                Response::Initialized { request, .. } | Response::Queried { request, .. } | Response::Set { request, .. } => Some(*request),
                _ => None,
            };
            if request == Some(id) && completes(&r, kind) {
                return Ok(r);
            }
            self.own_message(xhci, dev, &r);
        }
        Err(Error::Timeout)
    }

    /// One encapsulated response (None: none waits).
    fn fetch(&mut self, xhci: &mut Xhci, dev: &mut Device) -> Result<Option<Response>, Error> {
        let bytes = dev.control_in(xhci, SetupPacket::class_interface(rndis::GET_ENCAPSULATED_RESPONSE, 0, self.control, rndis::RESPONSE_BYTES as u16, true))?;
        Ok(if bytes.len() < 8 { None } else { Response::parse(bytes) })
    }

    /// The device's messages: a keep-alive is answered; a media
    /// indication is the carrier's new state.
    fn own_message(&mut self, xhci: &mut Xhci, dev: &mut Device, r: &Response) -> Option<bool> {
        match *r {
            Response::KeepAlive { request } => {
                let reply = rndis::keepalive_complete(request);
                let _ = dev.control_out(xhci, SetupPacket::class_interface(rndis::SEND_ENCAPSULATED_COMMAND, 0, self.control, 0, false), &reply);
                None
            }
            Response::Indication { status: rndis::STATUS_MEDIA_CONNECT } => Some(true),
            Response::Indication { status: rndis::STATUS_MEDIA_DISCONNECT } => Some(false),
            Response::Indication { status } => {
                println!("usbd: RNDIS indication {:#x}", status);
                None
            }
            _ => None,
        }
    }

    /// A notification said responses wait: they are fetched and handled;
    /// the carrier's state if one said it.
    pub fn responses(&mut self, xhci: &mut Xhci, dev: &mut Device) -> Option<bool> {
        let mut carrier = None;
        // (Bounded: a device that always has more is cut off.)
        for _ in 0..8 {
            match self.fetch(xhci, dev) {
                Ok(Some(r)) => {
                    if let Some(up) = self.own_message(xhci, dev, &r) {
                        carrier = Some(up);
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        carrier
    }
}

/// Whether `r` is the completion of a command of type `kind`.
fn completes(r: &Response, kind: u32) -> bool {
    matches!(
        (r, kind),
        (Response::Initialized { .. }, rndis::INITIALIZE_MSG) | (Response::Queried { .. }, rndis::QUERY_MSG) | (Response::Set { .. }, rndis::SET_MSG)
    )
}
