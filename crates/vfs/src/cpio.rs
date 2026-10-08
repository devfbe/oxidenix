//! The initramfs: a cpio archive in "newc" format.

use core::ops::Range;

const HEADER_LEN: usize = 110;

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
        let header = data.get(pos..pos + HEADER_LEN).ok_or("archive truncated")?;
        if &header[0..6] != b"070701" {
            return Err("bad cpio magic");
        }
        let field = |i: usize| hex(&header[6 + i * 8..6 + (i + 1) * 8]);
        let mode = field(1)? as u32;
        let filesize = field(6)?;
        let namesize = field(11)?;
        let name_start = pos + HEADER_LEN;
        let name = data.get(name_start..name_start + namesize.saturating_sub(1)).ok_or("name truncated")?;
        let name = core::str::from_utf8(name).map_err(|_| "name is not UTF-8")?;
        let data_start = align4(name_start + namesize);
        let data_end = data_start.checked_add(filesize).ok_or("data truncated")?;
        if data_end > data.len() {
            return Err("data truncated");
        }
        self.pos = align4(data_end);
        if name == "TRAILER!!!" {
            return Ok(None);
        }
        let name = name.trim_start_matches("./").trim_start_matches('/');
        Ok(Some(Entry { name, mode, data: data_start..data_end }))
    }
}
