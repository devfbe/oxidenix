//! The JBD2 journal format (ext3's, docs/design/ext3-journal.md): the journal superblock,
//! the log blocks a transaction is written as (descriptors with their tags, the logged
//! blocks, revoke blocks, the commit block with its CRC-32), and recovery's scan of a log
//! (which transactions are complete, which blocks they revoke, what to write where).
//! Pure: the caller reads and writes the journal's blocks (`Journal` in `lib.rs`).
//! Everything on the disk is big-endian.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

pub const MAGIC: u32 = 0xC03B_3998;
pub const DESCRIPTOR: u32 = 1;
pub const COMMIT: u32 = 2;
pub const SUPERBLOCK_V2: u32 = 4;
pub const REVOKE: u32 = 5;

pub const COMPAT_CHECKSUM: u32 = 1;
pub const INCOMPAT_REVOKE: u32 = 1;
pub const INCOMPAT_64BIT: u32 = 2;
pub const INCOMPAT_ASYNC_COMMIT: u32 = 4;
/// The incompatible features we read (others refuse the journal).
pub const INCOMPAT_KNOWN: u32 = INCOMPAT_REVOKE | INCOMPAT_ASYNC_COMMIT;

const FLAG_ESCAPE: u16 = 1;
const FLAG_SAME_UUID: u16 = 2;
const FLAG_DELETED: u16 = 4;
const FLAG_LAST_TAG: u16 = 8;
/// A tag without the 64bit and checksum features: block number, checksum (0), flags.
const TAG_BYTES: usize = 8;
const HEADER: usize = 12;
const CRC32_CHKSUM: u8 = 1;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

fn put_be16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_be_bytes());
}

fn header(b: &mut [u8], kind: u32, sequence: u32) {
    put_be32(b, 0, MAGIC);
    put_be32(b, 4, kind);
    put_be32(b, 8, sequence);
}

/// Linux's `crc32_be`: CRC-32, polynomial 0x04C11DB7 most significant bit first, no
/// inversion of its own (JBD2 seeds it with `!0`).
pub fn crc32_be(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// The journal's superblock (journal block 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superblock {
    pub block_size: u32,
    /// Blocks of the journal (`s_maxlen`).
    pub len: u32,
    /// The first log block (`s_first`).
    pub first: u32,
    /// The sequence the log starts with (`s_sequence`).
    pub sequence: u32,
    /// Where the log starts (`s_start`; 0: empty).
    pub start: u32,
    pub compat: u32,
    pub incompat: u32,
    pub uuid: [u8; 16],
    /// The block as read: what this does not know of it is kept.
    raw: Vec<u8>,
}

impl Superblock {
    /// A new journal of `len` blocks for a filesystem with `uuid`: empty, with the features
    /// we write.
    pub fn new(block_size: u32, len: u32, uuid: [u8; 16]) -> Superblock {
        let mut raw = vec![0u8; block_size as usize];
        // One user (the filesystem itself), as mke2fs makes an internal journal.
        put_be32(&mut raw, 64, 1);
        raw[0x100..0x110].copy_from_slice(&uuid);
        Superblock {
            block_size,
            len,
            first: 1,
            sequence: 1,
            start: 0,
            compat: COMPAT_CHECKSUM,
            incompat: INCOMPAT_REVOKE | INCOMPAT_ASYNC_COMMIT,
            uuid,
            raw,
        }
    }

    pub fn parse(b: &[u8]) -> Result<Superblock, &'static str> {
        if b.len() < 1024 || be32(b, 0) != MAGIC || be32(b, 4) != SUPERBLOCK_V2 {
            return Err("not a JBD2 (version 2) journal");
        }
        let sb = Superblock {
            block_size: be32(b, 12),
            len: be32(b, 16),
            first: be32(b, 20),
            sequence: be32(b, 24),
            start: be32(b, 28),
            compat: be32(b, 36),
            incompat: be32(b, 40),
            uuid: b[48..64].try_into().unwrap(),
            raw: b.to_vec(),
        };
        if sb.incompat & !INCOMPAT_KNOWN != 0 || be32(b, 44) != 0 {
            return Err("unsupported journal features");
        }
        if sb.block_size as usize != b.len() || sb.first == 0 || sb.first >= sb.len || sb.start >= sb.len {
            return Err("corrupt journal superblock");
        }
        Ok(sb)
    }

    /// The block as written.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.raw.clone();
        b.resize(self.block_size as usize, 0);
        header(&mut b, SUPERBLOCK_V2, 0);
        put_be32(&mut b, 12, self.block_size);
        put_be32(&mut b, 16, self.len);
        put_be32(&mut b, 20, self.first);
        put_be32(&mut b, 24, self.sequence);
        put_be32(&mut b, 28, self.start);
        put_be32(&mut b, 36, self.compat);
        put_be32(&mut b, 40, self.incompat);
        b[48..64].copy_from_slice(&self.uuid);
        b
    }

    /// The log block after `at` (the log is a ring from `first` to `len`).
    pub fn next(&self, at: u32) -> u32 {
        if at + 1 >= self.len { self.first } else { at + 1 }
    }

    /// Log blocks in the ring.
    pub fn log_blocks(&self) -> u32 {
        self.len - self.first
    }
}

/// Tags that fit one descriptor block (the first carries the UUID).
fn tags_per_descriptor(block_size: usize) -> usize {
    (block_size - HEADER - 16) / TAG_BYTES
}

/// Revoked block numbers that fit one revoke block.
fn revokes_per_block(block_size: usize) -> usize {
    (block_size - HEADER - 4) / 4
}

/// How many log blocks a transaction of `blocks` logged blocks and `revokes` revoked
/// blocks takes (descriptors, the blocks, revoke blocks, the commit block).
pub fn transaction_len(block_size: usize, blocks: usize, revokes: usize) -> usize {
    let per = tags_per_descriptor(block_size);
    let rper = revokes_per_block(block_size);
    blocks.div_ceil(per) + blocks + revokes.div_ceil(rper) + 1
}

/// Transaction `sequence` as log blocks, in order: `blocks` (home block number, contents),
/// `revokes`, a commit block at `time` (seconds). The commit's CRC-32 covers the
/// descriptors and logged blocks in log order.
pub fn encode_transaction(block_size: usize, uuid: &[u8; 16], sequence: u32, blocks: &[(u32, &[u8])], revokes: &[u32], time: u64) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut crc = !0u32;
    let per = tags_per_descriptor(block_size);
    for chunk in blocks.chunks(per) {
        let mut d = vec![0u8; block_size];
        header(&mut d, DESCRIPTOR, sequence);
        let mut at = HEADER;
        let mut logged = Vec::with_capacity(chunk.len());
        for (i, &(home, data)) in chunk.iter().enumerate() {
            let mut flags = 0;
            let mut copy = data.to_vec();
            if be32(&copy, 0) == MAGIC {
                // (Would read as a header: logged with its first word zeroed.)
                copy[..4].fill(0);
                flags |= FLAG_ESCAPE;
            }
            if i > 0 {
                flags |= FLAG_SAME_UUID;
            }
            if i + 1 == chunk.len() {
                flags |= FLAG_LAST_TAG;
            }
            put_be32(&mut d, at, home);
            put_be16(&mut d, at + 4, 0);
            put_be16(&mut d, at + 6, flags);
            at += TAG_BYTES;
            if i == 0 {
                d[at..at + 16].copy_from_slice(uuid);
                at += 16;
            }
            logged.push(copy);
        }
        crc = crc32_be(crc, &d);
        out.push(d);
        for copy in logged {
            crc = crc32_be(crc, &copy);
            out.push(copy);
        }
    }
    let rper = revokes_per_block(block_size);
    for chunk in revokes.chunks(rper) {
        let mut r = vec![0u8; block_size];
        header(&mut r, REVOKE, sequence);
        put_be32(&mut r, HEADER, (HEADER + 4 + chunk.len() * 4) as u32);
        for (i, &b) in chunk.iter().enumerate() {
            put_be32(&mut r, HEADER + 4 + i * 4, b);
        }
        out.push(r);
    }
    let mut c = vec![0u8; block_size];
    header(&mut c, COMMIT, sequence);
    c[12] = CRC32_CHKSUM;
    c[13] = 4;
    put_be32(&mut c, 16, crc);
    c[48..56].copy_from_slice(&time.to_be_bytes());
    out.push(c);
    out
}

/// What recovery found in a log.
#[derive(Debug, Default)]
pub struct Recovery {
    /// The blocks to write home (the newest copy of each, revokes applied), unescaped.
    pub blocks: BTreeMap<u32, Vec<u8>>,
    /// Complete transactions, and the sequence after the last.
    pub transactions: u32,
    pub next_sequence: u32,
    /// Where the log continues after the last complete transaction.
    pub next_block: u32,
}

/// One transaction as the scan found it.
struct Found {
    sequence: u32,
    /// (home block, log block, escaped) of its tags, in order.
    tags: Vec<(u32, u32, bool)>,
    revokes: Vec<u32>,
}

/// Scans the log of `sb` (reading journal block `n` with `read`), collects the complete
/// transactions' revokes and the blocks to replay (as JBD2's three passes). A torn or
/// foreign block ends the log.
pub fn recover(sb: &Superblock, read: &mut dyn FnMut(u32) -> Result<Vec<u8>, ()>) -> Result<Recovery, &'static str> {
    let mut rec = Recovery { next_sequence: sb.sequence, next_block: if sb.start == 0 { sb.first } else { sb.start }, ..Recovery::default() };
    if sb.start == 0 {
        return Ok(rec);
    }
    let bs = sb.block_size as usize;
    let checksums = sb.compat & COMPAT_CHECKSUM != 0;
    let mut found: Vec<Found> = Vec::new();
    let mut at = sb.start;
    let mut sequence = sb.sequence;
    let mut current = Found { sequence, tags: Vec::new(), revokes: Vec::new() };
    let mut crc = !0u32;
    // (At most the whole ring: a log never wraps onto itself.)
    let mut budget = sb.log_blocks() as u64 + 1;
    loop {
        if budget == 0 {
            break;
        }
        let b = read(at).map_err(|_| "cannot read the journal")?;
        budget -= 1;
        if be32(&b, 0) != MAGIC || be32(&b, 8) != sequence {
            break;
        }
        match be32(&b, 4) {
            DESCRIPTOR => {
                crc = crc32_be(crc, &b);
                let mut o = HEADER;
                let mut log = sb.next(at);
                let mut last = false;
                while !last && o + TAG_BYTES <= bs {
                    let home = be32(&b, o);
                    let flags = be16(&b, o + 6);
                    o += TAG_BYTES;
                    if flags & FLAG_SAME_UUID == 0 {
                        o += 16;
                    }
                    last = flags & FLAG_LAST_TAG != 0;
                    if checksums {
                        let data = read(log).map_err(|_| "cannot read the journal")?;
                        crc = crc32_be(crc, &data);
                    }
                    if flags & FLAG_DELETED == 0 {
                        current.tags.push((home, log, flags & FLAG_ESCAPE != 0));
                    }
                    log = sb.next(log);
                    budget = budget.saturating_sub(1);
                }
                at = log;
            }
            REVOKE => {
                let count = (be32(&b, HEADER) as usize).min(bs);
                let mut o = HEADER + 4;
                while o + 4 <= count {
                    current.revokes.push(be32(&b, o));
                    o += 4;
                }
                at = sb.next(at);
            }
            COMMIT => {
                if checksums && !(b[12] == CRC32_CHKSUM && b[13] == 4 && be32(&b, 16) == crc) {
                    // Torn (async commit): this transaction and what follows are not replayed.
                    break;
                }
                at = sb.next(at);
                found.push(core::mem::replace(&mut current, Found { sequence: sequence.wrapping_add(1), tags: Vec::new(), revokes: Vec::new() }));
                sequence = sequence.wrapping_add(1);
                crc = !0;
                rec.next_block = at;
                rec.next_sequence = sequence;
            }
            _ => break,
        }
    }
    // Revokes: the newest sequence that revokes each block.
    let mut revoked: BTreeMap<u32, u32> = BTreeMap::new();
    for t in &found {
        for &b in &t.revokes {
            let e = revoked.entry(b).or_insert(t.sequence);
            if seq_after(t.sequence, *e) {
                *e = t.sequence;
            }
        }
    }
    // Replay, oldest first: a copy is skipped if a transaction as new or newer revokes it.
    for t in &found {
        for &(home, log, escaped) in &t.tags {
            if revoked.get(&home).is_some_and(|&r| !seq_after(t.sequence, r)) {
                continue;
            }
            let mut data = read(log).map_err(|_| "cannot read the journal")?;
            if escaped {
                data[..4].copy_from_slice(&MAGIC.to_be_bytes());
            }
            rec.blocks.insert(home, data);
        }
    }
    rec.transactions = found.len() as u32;
    Ok(rec)
}

/// Whether sequence `a` comes after `b` (with wrapping, as JBD2's `tid_gt`).
pub fn seq_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
