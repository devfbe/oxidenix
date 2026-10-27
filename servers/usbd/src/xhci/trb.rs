//! Transfer request blocks (xHCI 1.2, 6.4): the 16-byte entries of every
//! ring, built for the command and transfer rings and decoded from the
//! event ring.

/// A TRB: parameter (two dwords), status, control (cycle bit 0, type in
/// bits 15:10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trb(pub [u32; 4]);

pub const BYTES: usize = 16;

// TRB types.
pub const NORMAL: u32 = 1;
pub const SETUP: u32 = 2;
pub const DATA: u32 = 3;
pub const STATUS: u32 = 4;
pub const LINK: u32 = 6;
pub const ENABLE_SLOT: u32 = 9;
pub const DISABLE_SLOT: u32 = 10;
pub const ADDRESS_DEVICE: u32 = 11;
pub const CONFIGURE_ENDPOINT: u32 = 12;
pub const EVALUATE_CONTEXT: u32 = 13;
pub const RESET_ENDPOINT: u32 = 14;
pub const STOP_ENDPOINT: u32 = 15;
pub const SET_TR_DEQUEUE: u32 = 16;
/// A no-op on a transfer ring (padding before the Link TRB).
pub const NOOP: u32 = 8;
pub const TRANSFER_EVENT: u32 = 32;
pub const COMMAND_COMPLETION: u32 = 33;
pub const PORT_STATUS_CHANGE: u32 = 34;
pub const HOST_CONTROLLER: u32 = 37;

// Control bits.
pub const CYCLE: u32 = 1 << 0;
/// Link TRB: toggle the cycle state.
pub const TOGGLE: u32 = 1 << 1;
/// Interrupt on short packet.
pub const ISP: u32 = 1 << 2;
pub const IOC: u32 = 1 << 5;
/// Immediate data (a setup stage's 8 bytes in the parameter).
pub const IDT: u32 = 1 << 6;
/// Address Device: block the SET_ADDRESS request.
pub const BSR: u32 = 1 << 9;
/// Data and status stages: IN.
pub const DIR_IN: u32 = 1 << 16;
/// Setup stage's transfer type: no data, OUT data, IN data.
pub const TRT_NONE: u32 = 0;
pub const TRT_OUT: u32 = 2 << 16;
pub const TRT_IN: u32 = 3 << 16;

/// Completion codes (xHCI 1.2, 6.4.5).
pub const SUCCESS: u8 = 1;
pub const DATA_BUFFER_ERROR: u8 = 2;
pub const BABBLE: u8 = 3;
pub const TRANSACTION_ERROR: u8 = 4;
pub const TRB_ERROR: u8 = 5;
pub const STALL: u8 = 6;
pub const SHORT_PACKET: u8 = 13;
pub const STOPPED: u8 = 26;
pub const STOPPED_LENGTH_INVALID: u8 = 27;
pub const STOPPED_SHORT: u8 = 28;

impl Trb {
    fn new(param: u64, status: u32, kind: u32, flags: u32) -> Trb {
        Trb([param as u32, (param >> 32) as u32, status, kind << 10 | flags])
    }

    pub fn kind(&self) -> u32 {
        self.0[3] >> 10 & 0x3f
    }

    pub fn cycle(&self) -> bool {
        self.0[3] & CYCLE != 0
    }

    pub fn param(&self) -> u64 {
        self.0[0] as u64 | (self.0[1] as u64) << 32
    }

    /// The same TRB with its cycle bit `cycle`.
    pub fn with_cycle(mut self, cycle: bool) -> Trb {
        self.0[3] = (self.0[3] & !CYCLE) | cycle as u32;
        self
    }

    pub fn link(ring: u64) -> Trb {
        Trb::new(ring, 0, LINK, TOGGLE)
    }

    /// A Normal TRB of `len` bytes at `buffer` (`td_size`: packets left in
    /// the TD after this TRB, for multi-TRB TDs; 0 for the last).
    pub fn normal(buffer: u64, len: u32, flags: u32) -> Trb {
        Trb::new(buffer, len & 0x1_ffff, NORMAL, flags)
    }

    pub fn setup(setup: u64, transfer: u32) -> Trb {
        Trb::new(setup, 8, SETUP, IDT | transfer)
    }

    pub fn data(buffer: u64, len: u32, input: bool) -> Trb {
        Trb::new(buffer, len & 0x1_ffff, DATA, if input { DIR_IN | ISP } else { 0 })
    }

    /// The status stage: IN unless the data stage was.
    pub fn status(data_in: bool) -> Trb {
        Trb::new(0, 0, STATUS, IOC | if data_in { 0 } else { DIR_IN })
    }

    pub fn enable_slot() -> Trb {
        Trb::new(0, 0, ENABLE_SLOT, 0)
    }

    pub fn disable_slot(slot: u8) -> Trb {
        Trb::new(0, 0, DISABLE_SLOT, (slot as u32) << 24)
    }

    pub fn address_device(input: u64, slot: u8, block: bool) -> Trb {
        Trb::new(input, 0, ADDRESS_DEVICE, (slot as u32) << 24 | if block { BSR } else { 0 })
    }

    pub fn configure_endpoint(input: u64, slot: u8) -> Trb {
        Trb::new(input, 0, CONFIGURE_ENDPOINT, (slot as u32) << 24)
    }

    pub fn evaluate_context(input: u64, slot: u8) -> Trb {
        Trb::new(input, 0, EVALUATE_CONTEXT, (slot as u32) << 24)
    }

    pub fn reset_endpoint(slot: u8, dci: u8) -> Trb {
        Trb::new(0, 0, RESET_ENDPOINT, (slot as u32) << 24 | (dci as u32) << 16)
    }

    pub fn stop_endpoint(slot: u8, dci: u8) -> Trb {
        Trb::new(0, 0, STOP_ENDPOINT, (slot as u32) << 24 | (dci as u32) << 16)
    }

    /// Set TR Dequeue Pointer: the endpoint's ring continues at `dequeue`
    /// with cycle state `cycle`.
    pub fn set_tr_dequeue(slot: u8, dci: u8, dequeue: u64, cycle: bool) -> Trb {
        Trb::new(dequeue | cycle as u64, 0, SET_TR_DEQUEUE, (slot as u32) << 24 | (dci as u32) << 16)
    }
}

/// An event, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A transfer TRB (`trb`, its device address) completed with `code`;
    /// `residual` bytes of it were not transferred.
    Transfer { slot: u8, dci: u8, trb: u64, code: u8, residual: u32 },
    /// The command TRB `trb` completed with `code` (Enable Slot: the slot).
    Command { trb: u64, code: u8, slot: u8 },
    /// Root hub port `port` (from 1) changed.
    Port { port: u8 },
    /// The controller's own event (an event ring full, ...).
    Controller { code: u8 },
    Other { kind: u32 },
}

impl Event {
    pub fn decode(t: &Trb) -> Event {
        let code = (t.0[2] >> 24) as u8;
        let slot = (t.0[3] >> 24) as u8;
        match t.kind() {
            TRANSFER_EVENT => Event::Transfer { slot, dci: (t.0[3] >> 16 & 0x1f) as u8, trb: t.param(), code, residual: t.0[2] & 0xff_ffff },
            COMMAND_COMPLETION => Event::Command { trb: t.param(), code, slot },
            PORT_STATUS_CHANGE => Event::Port { port: (t.0[0] >> 24) as u8 },
            HOST_CONTROLLER => Event::Controller { code },
            kind => Event::Other { kind },
        }
    }
}

/// A completion code's name, for messages.
pub fn code_name(code: u8) -> &'static str {
    match code {
        SUCCESS => "success",
        DATA_BUFFER_ERROR => "data buffer error",
        BABBLE => "babble",
        TRANSACTION_ERROR => "USB transaction error",
        TRB_ERROR => "TRB error",
        STALL => "stall",
        7 => "resource error",
        8 => "bandwidth error",
        9 => "no slots available",
        11 => "slot not enabled",
        12 => "endpoint not enabled",
        SHORT_PACKET => "short packet",
        17 => "parameter error",
        19 => "context state error",
        21 => "event ring full",
        STOPPED | STOPPED_LENGTH_INVALID | STOPPED_SHORT => "stopped",
        _ => "error",
    }
}
