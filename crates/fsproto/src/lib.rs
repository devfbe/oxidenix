//! Message format between the kernel's VFS and filesystem servers.
//!
//! A request is a 40-byte header (operation and four arguments) followed by
//! a payload (names, file data); a response is a 56-byte header (status and
//! six values) followed by a payload. All integers are little-endian. The
//! status is the result (>= 0) or a negative errno, as in Linux syscalls.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

pub const REQUEST_HEADER: usize = 40;
pub const RESPONSE_HEADER: usize = 56;
/// Largest payload in either direction; reads and writes are split into
/// chunks of this size.
pub const MAX_DATA: usize = 32 * 1024;
pub const MAX_MESSAGE: usize = RESPONSE_HEADER + MAX_DATA;

/// Operations. Arguments are listed as (a0, a1, a2, a3) + payload, results
/// as (v0..v5) + payload.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// (ino) -> (mode, size, links, atime, mtime, ctime)
    Stat = 1,
    /// (ino, offset, len) -> status = bytes, payload = data
    Read = 2,
    /// (ino, offset) + data -> status = bytes written
    Write = 3,
    /// (ino, len)
    Truncate = 4,
    /// (dir, cursor) -> v0 = next cursor (0 when done), payload = entries:
    /// u32 inode, u8 type, u8 name length, name
    List = 5,
    /// (dir) + name -> v0 = inode
    Lookup = 6,
    /// (dir, kind, perm) + name, NUL, symlink target -> v0 = inode
    Create = 7,
    /// (dir, want_dir) + name -> payload = u32 inodes whose last link went away
    Unlink = 8,
    /// (old dir, new dir, old name length) + old name + new name -> as Unlink
    Rename = 9,
    /// (ino): free an unlinked inode once the kernel no longer uses it
    Release = 10,
    /// (ino) -> payload = target
    Readlink = 11,
    /// (ino, permission bits)
    SetPerm = 12,
    /// () -> (block size, blocks, free blocks, inodes, free inodes)
    Usage = 13,
}

impl Op {
    pub fn from_u32(v: u32) -> Option<Op> {
        use Op::*;
        [Stat, Read, Write, Truncate, List, Lookup, Create, Unlink, Rename, Release, Readlink, SetPerm, Usage]
            .into_iter()
            .find(|op| *op as u32 == v)
    }
}

pub const KIND_FILE: u64 = 0;
pub const KIND_DIR: u64 = 1;
pub const KIND_SYMLINK: u64 = 2;

pub const TYPE_FILE: u8 = 1;
pub const TYPE_DIR: u8 = 2;
pub const TYPE_SYMLINK: u8 = 7;

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub fn encode_request(op: Op, args: [u64; 4], payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(REQUEST_HEADER + payload.len());
    m.extend_from_slice(&(op as u32).to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    for a in args {
        m.extend_from_slice(&a.to_le_bytes());
    }
    m.extend_from_slice(payload);
    m
}

pub struct Request<'a> {
    pub op: Option<Op>,
    pub args: [u64; 4],
    pub payload: &'a [u8],
}

pub fn decode_request(m: &[u8]) -> Option<Request<'_>> {
    if m.len() < REQUEST_HEADER {
        return None;
    }
    Some(Request {
        op: Op::from_u32(u32_at(m, 0)),
        args: [u64_at(m, 8), u64_at(m, 16), u64_at(m, 24), u64_at(m, 32)],
        payload: &m[REQUEST_HEADER..],
    })
}

/// Writes a response into `out` and returns its length.
pub fn encode_response(out: &mut [u8], status: i64, values: [u64; 6], payload: &[u8]) -> usize {
    let len = RESPONSE_HEADER + payload.len().min(out.len() - RESPONSE_HEADER);
    out[0..8].copy_from_slice(&status.to_le_bytes());
    for (i, v) in values.iter().enumerate() {
        out[8 + i * 8..16 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    out[RESPONSE_HEADER..len].copy_from_slice(&payload[..len - RESPONSE_HEADER]);
    len
}

pub struct Response<'a> {
    pub status: i64,
    pub values: [u64; 6],
    pub payload: &'a [u8],
}

pub fn decode_response(m: &[u8]) -> Option<Response<'_>> {
    if m.len() < RESPONSE_HEADER {
        return None;
    }
    let mut values = [0; 6];
    for (i, v) in values.iter_mut().enumerate() {
        *v = u64_at(m, 8 + i * 8);
    }
    Some(Response { status: u64_at(m, 0) as i64, values, payload: &m[RESPONSE_HEADER..] })
}

/// Appends one directory entry to a List payload.
pub fn push_entry(out: &mut Vec<u8>, ino: u32, kind: u8, name: &[u8]) {
    out.extend_from_slice(&ino.to_le_bytes());
    out.push(kind);
    out.push(name.len() as u8);
    out.extend_from_slice(name);
}

/// Iterates over the entries of a List payload.
pub fn entries(mut p: &[u8]) -> impl Iterator<Item = (u32, u8, &[u8])> {
    core::iter::from_fn(move || {
        if p.len() < 6 {
            return None;
        }
        let (ino, kind, len) = (u32_at(p, 0), p[4], p[5] as usize);
        let name = p.get(6..6 + len)?;
        p = &p[6 + len..];
        Some((ino, kind, name))
    })
}
