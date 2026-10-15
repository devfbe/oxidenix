//! Terminal settings as Linux keeps them: `struct termios` and `struct termios2` in x86-64's
//! layout (tcgetattr's TCGETS, TCGETS2), their flags and control characters, and the
//! defaults of Linux's `tty_std_termios`.

// c_iflag
pub const IGNBRK: u32 = 0o1;
pub const BRKINT: u32 = 0o2;
pub const IGNPAR: u32 = 0o4;
pub const PARMRK: u32 = 0o10;
pub const INPCK: u32 = 0o20;
pub const ISTRIP: u32 = 0o40;
pub const INLCR: u32 = 0o100;
pub const IGNCR: u32 = 0o200;
pub const ICRNL: u32 = 0o400;
pub const IUCLC: u32 = 0o1000;
pub const IXON: u32 = 0o2000;
pub const IXANY: u32 = 0o4000;
pub const IXOFF: u32 = 0o10000;
pub const IMAXBEL: u32 = 0o20000;
pub const IUTF8: u32 = 0o40000;

// c_oflag
pub const OPOST: u32 = 0o1;
pub const OLCUC: u32 = 0o2;
pub const ONLCR: u32 = 0o4;
pub const OCRNL: u32 = 0o10;
pub const ONOCR: u32 = 0o20;
pub const ONLRET: u32 = 0o40;
pub const TABDLY: u32 = 0o14000;
/// Tabs expanded to spaces (TAB3).
pub const XTABS: u32 = 0o14000;

// c_cflag
pub const CBAUD: u32 = 0o10017;
pub const B0: u32 = 0;
pub const B9600: u32 = 0o15;
pub const B38400: u32 = 0o17;
pub const B115200: u32 = 0o10002;
/// The speeds beyond B38400 (with CBAUD's low bits), and alone an exact speed in
/// `c_ispeed`/`c_ospeed` (BOTHER, termios2).
pub const CBAUDEX: u32 = 0o10000;
pub const BOTHER: u32 = 0o10000;
pub const CSIZE: u32 = 0o60;
pub const CS5: u32 = 0;
pub const CS6: u32 = 0o20;
pub const CS7: u32 = 0o40;
pub const CS8: u32 = 0o60;
pub const CSTOPB: u32 = 0o100;
pub const CREAD: u32 = 0o200;
pub const PARENB: u32 = 0o400;
pub const PARODD: u32 = 0o1000;
pub const HUPCL: u32 = 0o2000;
pub const CLOCAL: u32 = 0o4000;
/// The input speed's CBAUD bits, `IBSHIFT` up (B0 there: as the output speed).
pub const CIBAUD: u32 = 0o2003600000;
pub const IBSHIFT: u32 = 16;
/// Stick parity (mark or space).
pub const CMSPAR: u32 = 0o10000000000;
pub const CRTSCTS: u32 = 0o20000000000;

/// The speeds of the Bfoo values, by index (Linux's `baud_table`): B0 to B38400 are
/// indices 0 to 15, B57600 (CBAUDEX | 1) to B4000000 (CBAUDEX | 15) 16 to 30.
const BAUD_TABLE: [u32; 31] = [
    0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800,
    500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000, 2500000, 3000000, 3500000, 4000000,
];

/// The Bfoo value of `BAUD_TABLE`'s index `i`.
fn baud_bits(i: usize) -> u32 {
    if i < 16 { i as u32 } else { CBAUDEX | (i as u32 - 15) }
}

/// The speed of CBAUD bits `cbaud` (not BOTHER): Linux's table lookup, where a CBAUDEX
/// value beyond the table counts as its low bits alone.
fn table_speed(cbaud: u32) -> u32 {
    let mut i = (cbaud & !CBAUDEX) as usize;
    if cbaud & CBAUDEX != 0 && (1..16).contains(&i) {
        i += 15;
    }
    BAUD_TABLE[i.min(BAUD_TABLE.len() - 1)]
}

// c_lflag
pub const ISIG: u32 = 0o1;
pub const ICANON: u32 = 0o2;
pub const ECHO: u32 = 0o10;
pub const ECHOE: u32 = 0o20;
pub const ECHOK: u32 = 0o40;
pub const ECHONL: u32 = 0o100;
pub const NOFLSH: u32 = 0o200;
pub const TOSTOP: u32 = 0o400;
pub const ECHOCTL: u32 = 0o1000;
pub const ECHOPRT: u32 = 0o2000;
pub const ECHOKE: u32 = 0o4000;
pub const FLUSHO: u32 = 0o10000;
pub const PENDIN: u32 = 0o40000;
pub const IEXTEN: u32 = 0o100000;
pub const EXTPROC: u32 = 0o200000;

// c_cc indices
pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VSWTC: usize = 7;
pub const VSTART: usize = 8;
pub const VSTOP: usize = 9;
pub const VSUSP: usize = 10;
pub const VEOL: usize = 11;
pub const VREPRINT: usize = 12;
pub const VDISCARD: usize = 13;
pub const VWERASE: usize = 14;
pub const VLNEXT: usize = 15;
pub const VEOL2: usize = 16;
pub const NCCS: usize = 19;

/// A control character of this value is disabled (`_POSIX_VDISABLE`).
pub const DISABLED: u8 = 0;

/// The signals the line discipline raises (x86-64 numbers).
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGTSTP: u32 = 20;

/// `struct termios` is 36 bytes, `struct termios2` 44 (the speeds after it).
pub const TERMIOS_SIZE: usize = 36;
pub const TERMIOS2_SIZE: usize = 44;

/// A terminal's settings: `struct termios2` (the speeds only matter to TCGETS2/TCSETS2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
    pub ispeed: u32,
    pub ospeed: u32,
}

impl Default for Termios {
    /// Linux's `tty_std_termios`.
    fn default() -> Self {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03;
        cc[VQUIT] = 0x1c;
        cc[VERASE] = 0x7f;
        cc[VKILL] = 0x15;
        cc[VEOF] = 0x04;
        cc[VTIME] = 0;
        cc[VMIN] = 1;
        cc[VSTART] = 0x11;
        cc[VSTOP] = 0x13;
        cc[VSUSP] = 0x1a;
        cc[VREPRINT] = 0x12;
        cc[VDISCARD] = 0x0f;
        cc[VWERASE] = 0x17;
        cc[VLNEXT] = 0x16;
        Termios {
            iflag: ICRNL | IXON,
            oflag: OPOST | ONLCR,
            cflag: B38400 | CS8 | CREAD | HUPCL,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc,
            ispeed: 38400,
            ospeed: 38400,
        }
    }
}

impl Termios {
    /// `struct termios2`'s bytes (the first `TERMIOS_SIZE` are `struct termios`).
    pub fn to_bytes(&self) -> [u8; TERMIOS2_SIZE] {
        let mut b = [0u8; TERMIOS2_SIZE];
        b[0..4].copy_from_slice(&self.iflag.to_le_bytes());
        b[4..8].copy_from_slice(&self.oflag.to_le_bytes());
        b[8..12].copy_from_slice(&self.cflag.to_le_bytes());
        b[12..16].copy_from_slice(&self.lflag.to_le_bytes());
        b[16] = self.line;
        b[17..36].copy_from_slice(&self.cc);
        b[36..40].copy_from_slice(&self.ispeed.to_le_bytes());
        b[40..44].copy_from_slice(&self.ospeed.to_le_bytes());
        b
    }

    /// Settings from `struct termios` (36 bytes) or `struct termios2` (44 bytes), with
    /// the speeds as Linux's `set_termios` makes them: from CBAUD and CIBAUD, the
    /// structure's own only for BOTHER (termios2; `struct termios` keeps `self`'s).
    pub fn with_bytes(&self, b: &[u8]) -> Termios {
        let u32_at = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let mut cc = [0u8; NCCS];
        cc.copy_from_slice(&b[17..36]);
        let (ispeed, ospeed) = if b.len() >= TERMIOS2_SIZE { (u32_at(36), u32_at(40)) } else { (self.ispeed, self.ospeed) };
        let mut t = Termios { iflag: u32_at(0), oflag: u32_at(4), cflag: u32_at(8), lflag: u32_at(12), line: b[16], cc, ispeed, ospeed };
        t.ispeed = t.input_baud_rate();
        t.ospeed = t.baud_rate();
        t
    }

    /// The output speed (Linux's `tty_termios_baud_rate`).
    pub fn baud_rate(&self) -> u32 {
        let cbaud = self.cflag & CBAUD;
        if cbaud == BOTHER { self.ospeed } else { table_speed(cbaud) }
    }

    /// The input speed (Linux's `tty_termios_input_baud_rate`): CIBAUD's, the output
    /// speed where it is B0.
    pub fn input_baud_rate(&self) -> u32 {
        let cbaud = (self.cflag >> IBSHIFT) & CBAUD;
        match cbaud {
            B0 => self.baud_rate(),
            BOTHER => self.ispeed,
            _ => table_speed(cbaud),
        }
    }

    /// Records the speeds a driver really runs at (Linux's
    /// `tty_termios_encode_baud_rate`): a Bfoo value for a speed within 2% of one
    /// (unless BOTHER asked for exactness), BOTHER otherwise; an input speed of its own
    /// only if it differs or one was asked for.
    pub fn encode_baud_rate(&mut self, ibaud: u32, obaud: u32) {
        let mut iclose = ibaud / 50;
        let mut oclose = obaud / 50;
        if obaud == 0 {
            iclose = 0;
        }
        self.ispeed = ibaud;
        self.ospeed = obaud;
        let ibinput = (self.cflag >> IBSHIFT) & CBAUD != B0;
        if self.cflag & CBAUD == BOTHER {
            oclose = 0;
            if !ibinput {
                iclose = 0;
            }
        }
        if (self.cflag >> IBSHIFT) & CBAUD == BOTHER {
            iclose = 0;
        }
        self.cflag &= !CBAUD & !(CBAUD << IBSHIFT);
        let (mut ifound, mut ofound) = (None, None);
        for (i, &speed) in BAUD_TABLE.iter().enumerate() {
            if obaud.saturating_sub(oclose) <= speed && obaud + oclose >= speed {
                self.cflag |= baud_bits(i);
                ofound = Some(i);
            }
            if ibaud.saturating_sub(iclose) <= speed && ibaud + iclose >= speed {
                ifound = Some(i);
                if !(ofound == Some(i) && !ibinput) {
                    self.cflag |= baud_bits(i) << IBSHIFT;
                }
            }
        }
        if ofound.is_none() {
            self.cflag |= BOTHER;
        }
        if ifound.is_none() && (ibaud != obaud || ibinput) {
            self.cflag |= BOTHER << IBSHIFT;
        }
    }

    pub fn i(&self, flag: u32) -> bool {
        self.iflag & flag != 0
    }

    pub fn o(&self, flag: u32) -> bool {
        self.oflag & flag != 0
    }

    pub fn l(&self, flag: u32) -> bool {
        self.lflag & flag != 0
    }

    /// Whether `c` is the control character `index` (never a disabled one).
    pub fn is(&self, index: usize, c: u8) -> bool {
        self.cc[index] != DISABLED && self.cc[index] == c
    }
}
