//! Finds a file in a cpio archive in "newc" format (the boot image's
//! initramfs), following the archive's symlinks.

use alloc::string::String;
use alloc::vec::Vec;

const HEADER_LEN: usize = 110;
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
/// Most symlinks followed in one lookup.
const MAX_LINKS: usize = 8;

fn hex(field: &[u8]) -> Option<usize> {
    usize::from_str_radix(core::str::from_utf8(field).ok()?, 16).ok()
}

fn align4(x: usize) -> usize {
    (x + 3) & !3
}

/// The archive's member `name` (without a leading "/"): its mode and its
/// contents.
fn member(data: &'static [u8], name: &str) -> Option<(u32, &'static [u8])> {
    let mut pos = 0;
    loop {
        let header = data.get(pos..pos + HEADER_LEN)?;
        if &header[0..6] != b"070701" {
            return None;
        }
        let field = |i: usize| hex(&header[6 + i * 8..6 + (i + 1) * 8]);
        let (mode, filesize, namesize) = (field(1)? as u32, field(6)?, field(11)?);
        let name_start = pos + HEADER_LEN;
        let this = data.get(name_start..name_start + namesize.saturating_sub(1))?;
        let data_start = align4(name_start + namesize);
        let body = data.get(data_start..data_start + filesize)?;
        pos = align4(data_start + filesize);
        if this == b"TRAILER!!!" {
            return None;
        }
        if this.strip_prefix(b"./").unwrap_or(this) == name.as_bytes() {
            return Some((mode, body));
        }
    }
}

/// Splits `path` into names, resolving "." and "..".
fn names(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c.into()),
        }
    }
    out
}

/// The regular file at the absolute `path` in `data`, its symlinks (in the
/// last name and the directories before it) followed.
pub fn find(data: &'static [u8], path: &str) -> Option<&'static [u8]> {
    let mut pending = names(path);
    let mut links = 0;
    let mut done: Vec<String> = Vec::new();
    while !pending.is_empty() {
        let name = pending.remove(0);
        let mut here = done.clone();
        here.push(name);
        match member(data, &here.join("/")) {
            Some((mode, target)) if mode & S_IFMT == S_IFLNK => {
                links += 1;
                if links > MAX_LINKS {
                    return None;
                }
                let target = core::str::from_utf8(target).ok()?;
                let base = if target.starts_with('/') { String::new() } else { done.join("/") };
                let mut next = names(&alloc::format!("{base}/{target}"));
                next.extend(pending);
                pending = next;
                done.clear();
            }
            Some((mode, body)) if pending.is_empty() => return (mode & S_IFMT == S_IFREG).then_some(body),
            // A directory (or one the archive only implies).
            _ => done = here,
        }
    }
    None
}
