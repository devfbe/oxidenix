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
pub const B38400: u32 = 0o17;
pub const CS8: u32 = 0o60;
pub const CREAD: u32 = 0o200;
pub const HUPCL: u32 = 0o2000;

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

    /// Settings from `struct termios` (36 bytes: the speeds stay `self`'s) or
    /// `struct termios2` (44 bytes).
    pub fn with_bytes(&self, b: &[u8]) -> Termios {
        let u32_at = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let mut cc = [0u8; NCCS];
        cc.copy_from_slice(&b[17..36]);
        let (ispeed, ospeed) = if b.len() >= TERMIOS2_SIZE { (u32_at(36), u32_at(40)) } else { (self.ispeed, self.ospeed) };
        Termios { iflag: u32_at(0), oflag: u32_at(4), cflag: u32_at(8), lflag: u32_at(12), line: b[16], cc, ispeed, ospeed }
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
