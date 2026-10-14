//! The kernel's log as Linux programs see it: `syslog(2)` (busybox's dmesg and klogd) and
//! `/dev/kmsg` (1,11; util-linux's dmesg, systemd), over the kernel's log of records
//! (`SYS_KLOG_*`, `kernel/src/klog.rs`).
//!
//! **syslog(2)**, as Linux's `do_syslog`: `READ_ALL` (3) gives the newest records since the
//! last clear that fit the buffer, `READ_CLEAR` (4) and `CLEAR` (5) move the clear
//! position, `READ` (2) takes records from the syslog reader's position on (blocking until
//! one is there, a record that does not fit is split across reads), `SIZE_UNREAD` (9) and
//! `SIZE_BUFFER` (10) tell sizes, and the console's actions (6, 7, 8) are the kernel's.
//! Each line reads `<prio>[seconds.micros] text`, a record's embedded newlines making more
//! lines with the same prefix. The positions are the instance's (Linux has one per system).
//!
//! **/dev/kmsg**, as Linux's `devkmsg_*`: each open has its own position, from the oldest
//! record; a read gives one record (`prio,seq,micros,-;text`, bytes below 0x20, from 0x7f
//! on and `\` escaped as `\xNN`) or EINVAL if it does not fit, EPIPE once if records were
//! lost under the position (which then moves to the oldest), EAGAIN or a wait when there
//! is none; a write is one record (an optional `<N>` prefix gives level and facility, the
//! user facility by default, level 4); lseek(0) goes to the oldest record (`SEEK_SET`),
//! the clear position (`SEEK_DATA`) or past the newest (`SEEK_END`); poll is readable while
//! there is a record, with POLLERR and POLLPRI too if records were lost.
//!
//! **Waiting**: a reader that finds nothing new arms the instance's watch in the kernel
//! (`SYS_KLOG_ARM`); the record that passes it sends `EVENT_KLOG` to the service thread,
//! which bumps `SEQ`, wakes the readers sleeping on it and reports every open /dev/kmsg as
//! readable to its pollers (`event`). No lock is held while program memory is copied.

use crate::devices::{checked_iovecs, one_buffer};
use crate::files::{self, File, EINVAL, ESPIPE, ENOTTY, O_ACCMODE, O_CLOEXEC, O_NONBLOCK, O_WRONLY, POLLERR, POLLIN, POLLRDNORM};
use crate::namespace::Origin;
use crate::signal::ERESTARTSYS;
use crate::sync::Mutex;
use crate::syscall;
use crate::unix::Sink;
use crate::usercopy;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const EBADF: i64 = 9;
const EINTR: i64 = 4;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EPIPE: i64 = 32;
const POLLPRI: i16 = 0x2;

/// Linux's `default_message_loglevel`: a write to /dev/kmsg without a level.
const DEFAULT_LEVEL: u64 = 4;
/// `LOG_USER`: a write's facility unless its prefix names another one but the kernel's.
const FACILITY_USER: u64 = 1;
/// The longest write to /dev/kmsg (Linux's `PRINTKRB_RECORD_MAX`).
const WRITE_MAX: u64 = 1024;
/// One record as `klog_read` gives it, at most.
const RECORD_MAX: usize = (core::mem::size_of::<KlogRecord>() + KLOG_LINE_MAX as usize).next_multiple_of(8);

/// Bumped on each `EVENT_KLOG`; readers sleep on it.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// The syslog reader's position (`READ`), the bytes of its record already read, and the
/// clear position (`READ_ALL`, `SEEK_DATA`).
struct Syslog {
    seq: u64,
    partial: usize,
    clear: u64,
}

static SYSLOG: Mutex<Syslog> = Mutex::new(Syslog { seq: 0, partial: 0, clear: 0 });

/// The open /dev/kmsg descriptions, for the readiness reports of `event`.
static OPENS: Mutex<Vec<(u64, Weak<KmsgOpen>)>> = Mutex::new(Vec::new());

/// A record of the kernel's log.
struct Record<'a> {
    seq: u64,
    ts_ns: u64,
    prio: u32,
    text: &'a [u8],
}

/// The log's window: (first, next).
fn window() -> (u64, u64) {
    let mut out = [0u64; 2];
    let r = syscall(SYS_KLOG_READ, [u64::MAX, 0, 0, out.as_mut_ptr() as u64, 0, 0]);
    // (Only a fault on `out`, the server's own stack, could fail it.)
    assert!(r == 0, "klog_read of the window failed: {r}");
    (out[0], out[1])
}

/// Calls `each` with the records from `from` on, oldest first, until it returns false or
/// the newest is passed; the window as it was at the last read.
fn records(from: u64, mut each: impl FnMut(&Record) -> bool) -> Result<(u64, u64), i64> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(KLOG_READ_MAX as usize).map_err(|_| ENOMEM)?;
    buf.resize(KLOG_READ_MAX as usize, 0u8);
    let mut seq = from;
    loop {
        let mut out = [0u64; 2];
        let n = syscall(SYS_KLOG_READ, [seq, buf.as_mut_ptr() as u64, buf.len() as u64, out.as_mut_ptr() as u64, 0, 0]);
        if n < 0 {
            return Err(-n);
        }
        let mut at = 0;
        while at < n as usize {
            let head: KlogRecord = unsafe { core::ptr::read_unaligned(buf[at..].as_ptr() as *const KlogRecord) };
            let start = at + core::mem::size_of::<KlogRecord>();
            let r = Record { seq: head.seq, ts_ns: head.ts_ns, prio: head.prio, text: &buf[start..start + head.len as usize] };
            if !each(&r) {
                return Ok((out[0], out[1]));
            }
            seq = head.seq + 1;
            at = (start + head.len as usize).next_multiple_of(8);
        }
        if n == 0 || seq >= out[1] {
            return Ok((out[0], out[1]));
        }
    }
}

/// A record of sequence `seq` or later, if any: (its sequence, its /dev/kmsg line).
fn kmsg_line(seq: u64) -> Result<(Option<Vec<u8>>, u64), i64> {
    let mut buf = [0u8; RECORD_MAX];
    let mut out = [0u64; 2];
    let n = syscall(SYS_KLOG_READ, [seq, buf.as_mut_ptr() as u64, RECORD_MAX as u64, out.as_mut_ptr() as u64, 0, 0]);
    if n < 0 {
        return Err(-n);
    }
    if n == 0 {
        return Ok((None, out[0]));
    }
    let head: KlogRecord = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const KlogRecord) };
    let start = core::mem::size_of::<KlogRecord>();
    let text = &buf[start..start + head.len as usize];
    let mut line = Vec::new();
    line.try_reserve(32 + 4 * text.len()).map_err(|_| ENOMEM)?;
    let _ = write!(Bytes(&mut line), "{},{},{},-;", head.prio, head.seq, head.ts_ns / 1000);
    for &b in text {
        if b < b' ' || b >= 0x7f || b == b'\\' {
            let _ = write!(Bytes(&mut line), "\\x{:02x}", b);
        } else {
            line.push(b);
        }
    }
    line.push(b'\n');
    Ok((Some(line), head.seq))
}

/// `fmt::Write` into a byte vector.
struct Bytes<'a>(&'a mut Vec<u8>);

impl Write for Bytes<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// A record as syslog's lines: `<prio>[seconds.micros] ` before each of its lines.
fn syslog_text(r: &Record, out: &mut Vec<u8>) {
    let (secs, micros) = (r.ts_ns / 1_000_000_000, r.ts_ns % 1_000_000_000 / 1000);
    for line in r.text.split(|&b| b == b'\n') {
        let _ = write!(Bytes(out), "<{}>[{:5}.{:06}] ", r.prio, secs, micros);
        out.extend_from_slice(line);
        out.push(b'\n');
    }
}

/// The length of `syslog_text` of `r`.
fn syslog_len(r: &Record) -> usize {
    let mut v = Vec::new();
    syslog_text(r, &mut v);
    v.len()
}

/// Sleeps until the log may have a record of sequence `seq` or later (at once if it
/// has): EINTR for a signal.
fn wait_for(seq: u64) -> Result<(), i64> {
    let seen = SEQ.load(Ordering::Acquire);
    if syscall(SYS_KLOG_ARM, [seq, 0, 0, 0, 0, 0]) <= 0 {
        return Ok(());
    }
    let word = &SEQ as *const AtomicU32 as u64;
    if syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, 0, FUTEX_INTERRUPTIBLE, 0, 0]) == -EINTR {
        return Err(EINTR);
    }
    Ok(())
}

/// `EVENT_KLOG` (service thread): the log has new records.
pub fn event() {
    SEQ.fetch_add(1, Ordering::Release);
    let word = &SEQ as *const AtomicU32 as u64;
    syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
    let ids: Vec<u64> = OPENS.lock().iter().map(|&(id, _)| id).collect();
    for id in ids {
        files::ready(id, POLLIN | POLLRDNORM);
    }
}

/// syslog(type, buf, len).
pub fn syslog(action: u64, buf: u64, len: u64) -> Result<i64, i64> {
    const READ: u64 = 2;
    const READ_ALL: u64 = 3;
    const READ_CLEAR: u64 = 4;
    let len = len as i32;
    if matches!(action, READ | READ_ALL | READ_CLEAR) {
        if buf == 0 || len < 0 {
            return Err(EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }
        if buf.checked_add(len as u64).is_none_or(|end| end > SHARED_BASE) {
            return Err(EFAULT);
        }
    }
    let len = len.max(0) as usize;
    match action {
        // CLOSE and OPEN: nothing to do.
        0 | 1 => Ok(0),
        READ => syslog_read(buf, len),
        READ_ALL | READ_CLEAR => {
            let from = SYSLOG.lock().clear;
            // Every record since the clear position, then the newest that fit.
            let mut text = Vec::new();
            let mut ends: Vec<usize> = Vec::new();
            let mut failed = false;
            let (_, next) = records(from, |r| {
                if text.try_reserve(syslog_len(r)).is_err() || ends.try_reserve(1).is_err() {
                    failed = true;
                    return false;
                }
                syslog_text(r, &mut text);
                ends.push(text.len());
                true
            })?;
            if failed {
                return Err(ENOMEM);
            }
            // The first record's start from which the rest fits (the last end always does).
            let total = text.len();
            let start = core::iter::once(0).chain(ends.iter().copied()).find(|&s| total - s <= len).unwrap_or(total);
            if action == READ_CLEAR {
                SYSLOG.lock().clear = next;
            }
            usercopy::to_program(buf, &text[start..])?;
            Ok((total - start) as i64)
        }
        // CLEAR
        5 => {
            SYSLOG.lock().clear = window().1;
            Ok(0)
        }
        6..=8 => {
            let r = syscall(SYS_KLOG_CONSOLE, [action, len as u64, 0, 0, 0, 0]);
            if r < 0 { Err(-r) } else { Ok(0) }
        }
        // SIZE_UNREAD: the bytes READ would give.
        9 => {
            let (seq, partial) = {
                let s = SYSLOG.lock();
                (s.seq, s.partial)
            };
            let mut total = 0usize;
            let (first, _) = records(seq, |r| {
                total += syslog_len(r);
                true
            })?;
            Ok(total.saturating_sub(if seq < first { 0 } else { partial }) as i64)
        }
        10 => Ok(KLOG_BYTES as i64),
        _ => Err(EINVAL),
    }
}

/// syslog's READ: records from the reader's position on (waiting for one), a record that
/// does not fit split across reads.
fn syslog_read(buf: u64, len: usize) -> Result<i64, i64> {
    loop {
        let out = {
            let mut s = SYSLOG.lock();
            let (seq, partial) = (s.seq, s.partial);
            let mut out = Vec::new();
            // (The log's text as lines is at most a few times its size.)
            out.try_reserve(len.min(4 * KLOG_BYTES as usize)).map_err(|_| ENOMEM)?;
            let len = len.min(4 * KLOG_BYTES as usize);
            let mut position = (seq, partial);
            let mut failed = false;
            let (first, _) = records(seq, |r| {
                let skip = if r.seq == seq { partial } else { 0 };
                let mut text = Vec::new();
                if text.try_reserve(syslog_len(r)).is_err() {
                    failed = true;
                    return false;
                }
                syslog_text(r, &mut text);
                let rest = &text[skip.min(text.len())..];
                let n = rest.len().min(len - out.len());
                out.extend_from_slice(&rest[..n]);
                position = if n < rest.len() { (r.seq, skip + n) } else { (r.seq + 1, 0) };
                out.len() < len && n == rest.len()
            })?;
            if failed {
                return Err(ENOMEM);
            }
            if seq < first && out.is_empty() {
                position = (first, 0);
            }
            (s.seq, s.partial) = position;
            out
        };
        if !out.is_empty() {
            usercopy::to_program(buf, &out)?;
            return Ok(out.len() as i64);
        }
        let seq = SYSLOG.lock().seq;
        if wait_for(seq).is_err() {
            return Err(ERESTARTSYS);
        }
    }
}

/// An open file description of /dev/kmsg.
pub struct KmsgOpen {
    pub id: u64,
    pub origin: Origin,
    /// The next record it reads.
    seq: Mutex<u64>,
}

impl Drop for KmsgOpen {
    fn drop(&mut self) {
        OPENS.lock().retain(|&(id, _)| id != self.id);
    }
}

/// Opens /dev/kmsg by the node `origin`.
pub fn open(flags: u32, origin: Origin) -> Result<i64, i64> {
    let kept = flags & (O_ACCMODE | O_NONBLOCK | O_CLOEXEC);
    let k = Arc::new(KmsgOpen { id: files::new_id(), origin, seq: Mutex::new(window().0) });
    {
        let mut opens = OPENS.lock();
        opens.try_reserve(1).map_err(|_| ENOMEM)?;
        opens.push((k.id, Arc::downgrade(&k)));
    }
    files::install(k.id, File::Kmsg(k.clone()), kept)
}

impl KmsgOpen {
    /// Its readiness for poll and epoll now (arming the kernel's watch if nothing is new).
    pub fn readiness_now(&self) -> i16 {
        let seq = *self.seq.lock();
        let (first, next) = window();
        let mut ready = 0;
        if seq < next || syscall(SYS_KLOG_ARM, [seq, 0, 0, 0, 0, 0]) == 0 {
            ready = POLLIN | POLLRDNORM;
        }
        if seq < first {
            ready |= POLLERR | POLLPRI;
        }
        ready
    }

    /// read(2): one record.
    fn read(&self, mut sink: Sink, nonblock: bool) -> Result<i64, i64> {
        loop {
            let line = {
                let mut seq = self.seq.lock();
                let (line, at) = kmsg_line(*seq)?;
                if at > *seq {
                    // Lost under the position: told once, then on from the oldest.
                    *seq = if line.is_some() { at } else { window().0 };
                    return Err(EPIPE);
                }
                match line {
                    Some(line) if line.len() > sink.room() => return Err(EINVAL),
                    Some(line) => {
                        *seq = at + 1;
                        Some(line)
                    }
                    None => None,
                }
            };
            if let Some(line) = line {
                return sink.put(&line).map(|n| n as i64);
            }
            let seq = *self.seq.lock();
            if nonblock {
                // (Armed: a poller hears of the next record.)
                syscall(SYS_KLOG_ARM, [seq, 0, 0, 0, 0, 0]);
                return Err(EAGAIN);
            }
            if wait_for(seq).is_err() {
                return Err(ERESTARTSYS);
            }
        }
    }

    /// write(2): one record of the bytes at `vecs`.
    fn write(&self, vecs: &[(u64, u64)]) -> Result<i64, i64> {
        let len: u64 = vecs.iter().map(|v| v.1).sum();
        if len > WRITE_MAX {
            return Err(EINVAL);
        }
        let mut text = [0u8; WRITE_MAX as usize];
        let mut at = 0;
        for &(addr, n) in vecs {
            usercopy::from_program(addr, &mut text[at..at + n as usize])?;
            at += n as usize;
        }
        let mut line = &text[..at];
        let (mut level, mut facility) = (DEFAULT_LEVEL, FACILITY_USER);
        if line.first() == Some(&b'<') {
            let digits = line[1..].iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0 && line.get(1 + digits) == Some(&b'>') {
                let u = line[1..1 + digits].iter().fold(0u64, |v, &d| v.saturating_mul(10).saturating_add((d - b'0') as u64));
                level = u & 7;
                if (u & 0x3f8) >> 3 != 0 {
                    facility = (u & 0x3f8) >> 3;
                }
                line = &line[2 + digits..];
            }
        }
        if line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
        }
        let r = syscall(SYS_KLOG_WRITE, [facility << 3 | level, line.as_ptr() as u64, line.len() as u64, 0, 0, 0]);
        if r < 0 {
            return Err(-r);
        }
        Ok(len as i64)
    }

    /// lseek(fd, 0, whence).
    fn seek(&self, offset: u64, whence: u64) -> Result<i64, i64> {
        const SEEK_SET: u64 = 0;
        const SEEK_END: u64 = 2;
        const SEEK_DATA: u64 = 3;
        if offset != 0 {
            return Err(ESPIPE);
        }
        let to = match whence {
            SEEK_SET => window().0,
            SEEK_DATA => SYSLOG.lock().clear.max(window().0),
            SEEK_END => window().1,
            _ => return Err(EINVAL),
        };
        *self.seq.lock() = to;
        Ok(0)
    }
}

/// The calls on an open /dev/kmsg.
pub fn call(nr: u64, k: &KmsgOpen, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    use crate::files::{SYS_FSTAT, SYS_IOCTL, SYS_LSEEK, SYS_PREAD64, SYS_PREADV, SYS_PWRITE64, SYS_PWRITEV, SYS_READ, SYS_READV, SYS_WRITE, SYS_WRITEV};
    let readable = flags & O_ACCMODE != O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        SYS_FSTAT => usercopy::to_program(a1, &k.origin.stat()?).map(|_| 0),
        SYS_READ | SYS_READV if !readable => Err(EBADF),
        SYS_WRITE | SYS_WRITEV if !writable => Err(EBADF),
        SYS_READ => k.read(Sink::program(&one_buffer(a1, a2)?), nonblock),
        SYS_READV => k.read(Sink::program(&checked_iovecs(a1, a2)?), nonblock),
        SYS_WRITE => k.write(&one_buffer(a1, a2)?),
        SYS_WRITEV => k.write(&checked_iovecs(a1, a2)?),
        SYS_LSEEK => k.seek(a1, a2),
        SYS_PREAD64 | SYS_PREADV | SYS_PWRITE64 | SYS_PWRITEV => Err(ESPIPE),
        SYS_IOCTL => Err(ENOTTY),
        _ => Err(EINVAL),
    }
}
