//! CDC-NCM 1.0's transfer blocks: frames travel in NTBs, each a header
//! (NTH16), one or more datagram pointer tables (NDP16) and the frames
//! they point to; the 16-bit format, which every function supports, is
//! the one usbd uses. Also the class requests that set the format up.

/// Class requests.
pub const GET_NTB_PARAMETERS: u8 = 0x80;
pub const SET_NTB_FORMAT: u8 = 0x84;
pub const SET_NTB_INPUT_SIZE: u8 = 0x86;
pub const SET_CRC_MODE: u8 = 0x8a;

/// The 16-bit NTB format (SET_NTB_FORMAT's value).
pub const FORMAT_NTB16: u16 = 0;

pub const NTH16_SIGNATURE: u32 = u32::from_le_bytes(*b"NCMH");
/// An NDP16 without CRCs.
pub const NDP16_SIGNATURE: u32 = u32::from_le_bytes(*b"NCM0");
pub const NTH16_BYTES: usize = 12;
/// An NDP16's header (signature, length, next): its pointers follow.
pub const NDP16_HEADER: usize = 8;
/// Most NDPs one NTB may chain, and pointers usbd follows in all (a
/// device that sends more is cut off).
const MAX_NDPS: usize = 8;
pub const MAX_DATAGRAMS: usize = 64;

/// GET_NTB_PARAMETERS's answer (CDC-NCM 1.0, 6.2.1): what the function
/// takes and gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parameters {
    /// Bit 0: NTB16, bit 1: NTB32.
    pub formats: u16,
    pub in_max_size: u32,
    pub in_divisor: u16,
    pub in_remainder: u16,
    pub in_alignment: u16,
    pub out_max_size: u32,
    pub out_divisor: u16,
    pub out_remainder: u16,
    pub out_alignment: u16,
    /// Most datagrams in one NTB to the function (0: no limit).
    pub out_max_datagrams: u16,
}

pub const PARAMETERS_BYTES: usize = 28;
/// The smallest NTB size a function may announce.
pub const MIN_BLOCK: u32 = 2048;

impl Parameters {
    /// The answer, checked for what usbd relies on (NTB16, the sizes and
    /// alignments it can honour); None otherwise.
    pub fn parse(b: &[u8]) -> Option<Parameters> {
        if b.len() < PARAMETERS_BYTES {
            return None;
        }
        let w = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let d = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"));
        let p = Parameters {
            formats: w(2),
            in_max_size: d(4),
            in_divisor: w(8),
            in_remainder: w(10),
            in_alignment: w(12),
            out_max_size: d(16),
            out_divisor: w(20),
            out_remainder: w(22),
            out_alignment: w(24),
            out_max_datagrams: w(26),
        };
        let power_of_two = |v: u16| v != 0 && v.is_power_of_two();
        // (NCM 1.0, 6.2.1: blocks of at least 2048 bytes each way.)
        let usable = p.formats & 1 != 0
            && p.in_max_size >= MIN_BLOCK
            && p.out_max_size >= MIN_BLOCK
            && power_of_two(p.out_divisor)
            && p.out_remainder < p.out_divisor
            && (p.out_alignment == 0 || power_of_two(p.out_alignment) && p.out_alignment <= 64);
        usable.then_some(p)
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The datagrams (Ethernet frames) of a received NTB16, as ranges of `b`,
/// at most `MAX_DATAGRAMS`; None if its header is malformed. Pointers
/// that reach outside the block are skipped; a malformed NDP ends the
/// walk (what came before it stands).
pub fn datagrams(b: &[u8], out: &mut [(usize, usize); MAX_DATAGRAMS]) -> Option<usize> {
    if u32_at(b, 0)? != NTH16_SIGNATURE || u16_at(b, 4)? as usize != NTH16_BYTES {
        return None;
    }
    let block = (u16_at(b, 8)? as usize).min(b.len());
    let mut ndp = u16_at(b, 10)? as usize;
    let mut n = 0;
    for _ in 0..MAX_NDPS {
        if ndp < NTH16_BYTES || ndp % 4 != 0 || ndp + NDP16_HEADER > block {
            break;
        }
        let len = u16_at(b, ndp + 4)? as usize;
        if u32_at(b, ndp)? != NDP16_SIGNATURE || len < NDP16_HEADER + 8 || len % 4 != 0 || ndp + len > block {
            break;
        }
        for p in (ndp + NDP16_HEADER..ndp + len).step_by(4) {
            let (index, length) = (u16_at(b, p)? as usize, u16_at(b, p + 2)? as usize);
            if index == 0 || length == 0 {
                break;
            }
            if index + length <= block && n < MAX_DATAGRAMS {
                out[n] = (index, index + length);
                n += 1;
            }
        }
        ndp = u16_at(b, ndp + 6)? as usize;
        if ndp == 0 {
            break;
        }
    }
    Some(n)
}

/// Builds one NTB16 for the function in `buf` (at most `out_max_size`
/// bytes of it): the header, then one NDP16 with room for `capacity`
/// pointers, then the datagrams, each placed as the function's divisor and
/// remainder ask.
pub struct Builder<'a> {
    buf: &'a mut [u8],
    params: Parameters,
    /// Where the NDP is.
    ndp: usize,
    capacity: usize,
    count: usize,
    /// The end of what is written.
    end: usize,
    sequence: u16,
}

impl<'a> Builder<'a> {
    /// At most `capacity` datagrams (also within the function's
    /// `out_max_datagrams`).
    pub fn new(buf: &'a mut [u8], params: Parameters, capacity: usize, sequence: u16) -> Builder<'a> {
        let limit = if params.out_max_datagrams == 0 { usize::MAX } else { params.out_max_datagrams as usize };
        let max = (params.out_max_size as usize).min(buf.len()).min(u16::MAX as usize);
        let buf = &mut buf[..max];
        // The NDP right after the header, as the function aligns NDPs (at
        // least to 4), with as many pointers as fit (a terminator too).
        let ndp = NTH16_BYTES.next_multiple_of((params.out_alignment as usize).max(4));
        let fit = max.saturating_sub(ndp + NDP16_HEADER) / 4;
        let capacity = capacity.min(limit).min(MAX_DATAGRAMS).min(fit.saturating_sub(1));
        let end = ndp + NDP16_HEADER + 4 * (capacity + 1);
        Builder { buf, params, ndp, capacity, count: 0, end, sequence }
    }

    /// Where a datagram after `end` starts: at an offset that is the
    /// remainder modulo the divisor.
    fn place(&self) -> usize {
        let (d, r) = (self.params.out_divisor as usize, self.params.out_remainder as usize);
        // The first offset from `end` on that is `r` modulo `d` (a power
        // of two).
        self.end + (r.wrapping_sub(self.end) & (d - 1))
    }

    /// Whether another datagram of `len` bytes fits.
    pub fn has_room(&self, len: usize) -> bool {
        self.count < self.capacity && self.place() + len <= self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Adds a datagram written by `fill` (false, nothing added, if it does
    /// not fit).
    pub fn push_with(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        if len == 0 || !self.has_room(len) {
            return false;
        }
        let at = self.place();
        self.buf[self.end..at].fill(0);
        fill(&mut self.buf[at..at + len]);
        let p = self.ndp + NDP16_HEADER + 4 * self.count;
        self.buf[p..p + 2].copy_from_slice(&(at as u16).to_le_bytes());
        self.buf[p + 2..p + 4].copy_from_slice(&(len as u16).to_le_bytes());
        self.count += 1;
        self.end = at + len;
        true
    }

    /// Writes the header and the NDP; the NTB's length.
    pub fn finish(self) -> usize {
        let ndp = self.ndp;
        let ndp_len = NDP16_HEADER + 4 * (self.capacity + 1);
        let b = &mut *self.buf;
        b[NTH16_BYTES..ndp].fill(0);
        b[0..4].copy_from_slice(&NTH16_SIGNATURE.to_le_bytes());
        b[4..6].copy_from_slice(&(NTH16_BYTES as u16).to_le_bytes());
        b[6..8].copy_from_slice(&self.sequence.to_le_bytes());
        b[8..10].copy_from_slice(&(self.end as u16).to_le_bytes());
        b[10..12].copy_from_slice(&(ndp as u16).to_le_bytes());
        b[ndp..ndp + 4].copy_from_slice(&NDP16_SIGNATURE.to_le_bytes());
        b[ndp + 4..ndp + 6].copy_from_slice(&(ndp_len as u16).to_le_bytes());
        b[ndp + 6..ndp + 8].fill(0);
        // The unused pointers, the terminator among them, are zero.
        b[ndp + NDP16_HEADER + 4 * self.count..ndp + ndp_len].fill(0);
        self.end
    }
}
