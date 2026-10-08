//! The initramfs: a cpio archive in "newc" format.

use core::ops::Range;

/// The length of a member's header; the name follows it.
pub const HEADER_LEN: usize = 110;

/// What a member's header says.
pub struct Header {
    pub mode: u32,
    pub filesize: usize,
    /// With the name's NUL.
    pub namesize: usize,
}

impl Header {
    /// Where the member's data starts, for a member at `pos`.
    pub fn data_start(&self, pos: usize) -> usize {
        align4(pos + HEADER_LEN + self.namesize)
    }

    /// Where the next member starts, for a member at `pos`; None if that
    /// overflows.
    pub fn next(&self, pos: usize) -> Option<usize> {
        self.data_start(pos).checked_add(self.filesize).map(align4)
    }
}

/// Parses a member's header (the first `HEADER_LEN` bytes of `bytes`): for
/// reading an archive piece by piece.
pub fn header(bytes: &[u8]) -> Result<Header, &'static str> {
    let header = bytes.get(..HEADER_LEN).ok_or("archive truncated")?;
    if &header[0..6] != b"070701" {
        return Err("bad cpio magic");
    }
    let field = |i: usize| hex(&header[6 + i * 8..6 + (i + 1) * 8]);
    Ok(Header { mode: field(1)? as u32, filesize: field(6)?, namesize: field(11)? })
}

/// The archive's end marker, as a member's name.
pub const TRAILER: &str = "TRAILER!!!";

/// A member's path as the tree takes it: without a leading "./" or "/".
pub fn member_path(raw: &str) -> &str {
    raw.trim_start_matches("./").trim_start_matches('/')
}

/// One member of the archive: its path (without leading "./" or "/"), its
/// mode and where its data lies in the archive.
pub struct Entry<'a> {
    pub name: &'a str,
    pub mode: u32,
    pub data: Range<usize>,
}

fn hex(field: &[u8]) -> Result<usize, &'static str> {
    let s = core::str::from_utf8(field).map_err(|_| "header is not ASCII")?;
    usize::from_str_radix(s, 16).map_err(|_| "header field is not hex")
}

fn align4(x: usize) -> usize {
    (x + 3) & !3
}

/// The members of `archive` up to the trailer, or the first error.
pub fn entries(archive: &[u8]) -> Entries<'_> {
    Entries { archive, pos: 0, done: false }
}

pub struct Entries<'a> {
    archive: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>, &'static str>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = self.parse();
        if !matches!(result, Ok(Some(_))) {
            self.done = true;
        }
        result.transpose()
    }
}

impl<'a> Entries<'a> {
    fn parse(&mut self) -> Result<Option<Entry<'a>>, &'static str> {
        let data = self.archive;
        let pos = self.pos;
        let h = header(data.get(pos..).ok_or("archive truncated")?)?;
        let name_start = pos + HEADER_LEN;
        let name = data.get(name_start..name_start + h.namesize.saturating_sub(1)).ok_or("name truncated")?;
        let name = core::str::from_utf8(name).map_err(|_| "name is not UTF-8")?;
        let data_start = h.data_start(pos);
        let data_end = data_start.checked_add(h.filesize).ok_or("data truncated")?;
        if data_end > data.len() {
            return Err("data truncated");
        }
        self.pos = align4(data_end);
        if name == TRAILER {
            return Ok(None);
        }
        Ok(Some(Entry { name: member_path(name), mode: h.mode, data: data_start..data_end }))
    }
}
