//! The kernel's log: a ring of records (Linux's printk buffer) that `printk`, the Linux
//! servers' messages and the native servers' output go to, read by the Linux server for
//! `syslog(2)` and `/dev/kmsg` (`dmesg`).
//!
//! **Records** carry a sequence number (counted from 0 at boot, never reused), the time
//! since boot in nanoseconds, a priority (Linux's `facility << 3 | level`) and at most
//! `LINE_MAX` bytes of text without the newline. They lie one after the other in a ring
//! of `LOG_BYTES` (wrapping byte-wise at its end); a new one evicts the oldest until it
//! fits, so the log holds a window `first..next` of sequence numbers.
//!
//! **Lines**: `printk` hands its text in pieces; a piece without a newline waits in the
//! pending line until one comes (Linux's continuation lines), a line too long is cut into
//! records of `LINE_MAX`. A pending line is its CPU's: text from another CPU ends it first.
//!
//! **Waking readers** (`arm`, `take_event`): an instance whose reader found nothing new
//! arms a watch with the sequence it waits beyond; the record that passes it marks the
//! watch and wakes the instance's service thread, whose event wait then reports
//! `EVENT_KLOG` once. Records are appended with interrupts off and no other lock held
//! but the wait queue's that `wakeup` takes, so `printk` works in interrupt context too;
//! it must not be called with a wait queue or a task's wake lock held. After a panic
//! (`panic`) nothing is logged or woken any more: the other CPUs were stopped, perhaps
//! holding these locks.
//!
//! **The console's level** (Linux's `console_loglevel`, set by `syslog`'s actions 6-8):
//! records of a level below it are printed on the console as well; the kernel's own
//! messages are `KERN_INFO` (6), below the default level 7.

use crate::sync::IrqSpinLock;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// The ring's size (Linux's default, `CONFIG_LOG_BUF_SHIFT` 17).
pub const LOG_BYTES: usize = restricted::KLOG_BYTES as usize;
/// The most text one record carries.
pub const LINE_MAX: usize = restricted::KLOG_LINE_MAX as usize;
/// A record's header in the ring: sequence, time, text length, priority.
const HEADER: usize = 8 + 8 + 2 + 2;

/// `KERN_INFO`: the level of the kernel's own messages (facility 0, the kernel's).
pub const LEVEL_INFO: u16 = 6;
/// `KERN_ERR` and `KERN_WARNING`, for messages that say as much on Linux (the OOM
/// killer's).
pub const LEVEL_ERR: u16 = 3;
pub const LEVEL_WARNING: u16 = 4;
/// Linux's default console level: levels 0-6 are printed.
const CONSOLE_DEFAULT: u32 = 7;
/// The lowest level `CONSOLE_OFF` and `CONSOLE_LEVEL` leave (Linux's `minimum_console_loglevel`).
const CONSOLE_MIN: u32 = 1;

struct Ring {
    bytes: [u8; LOG_BYTES],
    /// Offset of the oldest record, and of the next one's place.
    head: usize,
    tail: usize,
    used: usize,
    first: u64,
    next: u64,
    /// The pending line: its CPU (if any), priority and text.
    pending_cpu: Option<u32>,
    pending_prio: u16,
    pending: [u8; LINE_MAX],
    pending_len: usize,
}

static RING: IrqSpinLock<Ring> = IrqSpinLock::new(Ring {
    bytes: [0; LOG_BYTES],
    head: 0,
    tail: 0,
    used: 0,
    first: 0,
    next: 0,
    pending_cpu: None,
    pending_prio: LEVEL_INFO,
    pending: [0; LINE_MAX],
    pending_len: 0,
});

static CONSOLE_LEVEL: AtomicU32 = AtomicU32::new(CONSOLE_DEFAULT);
/// The level `CONSOLE_ON` restores (0: none saved).
static SAVED_LEVEL: AtomicU32 = AtomicU32::new(0);
static PANICKING: AtomicBool = AtomicBool::new(false);

/// What an instance's watch waits for.
struct Watch {
    instance: u64,
    chan: usize,
    /// Woken once a record of this sequence or later is there.
    seq: u64,
    fired: bool,
}

static WATCHES: IrqSpinLock<Vec<Watch>> = IrqSpinLock::new(Vec::new());
/// Unfired watches: appends look at `WATCHES` only while there are any.
static ARMED: AtomicUsize = AtomicUsize::new(0);

impl Ring {
    fn put(&mut self, at: usize, data: &[u8]) {
        let first = data.len().min(LOG_BYTES - at);
        self.bytes[at..at + first].copy_from_slice(&data[..first]);
        self.bytes[..data.len() - first].copy_from_slice(&data[first..]);
    }

    fn get(&self, at: usize, out: &mut [u8]) {
        let first = out.len().min(LOG_BYTES - at);
        out[..first].copy_from_slice(&self.bytes[at..at + first]);
        let rest = out.len() - first;
        out[first..].copy_from_slice(&self.bytes[..rest]);
    }

    /// The header at `at`: (sequence, time, text length, priority).
    fn header(&self, at: usize) -> (u64, u64, usize, u16) {
        let mut h = [0u8; HEADER];
        self.get(at, &mut h);
        let seq = u64::from_le_bytes(h[0..8].try_into().unwrap());
        let ts = u64::from_le_bytes(h[8..16].try_into().unwrap());
        (seq, ts, u16::from_le_bytes([h[16], h[17]]) as usize, u16::from_le_bytes([h[18], h[19]]))
    }

    /// Appends a record (`text` at most `LINE_MAX`), evicting the oldest until it fits.
    fn append(&mut self, prio: u16, text: &[u8]) {
        let size = HEADER + text.len();
        while LOG_BYTES - self.used < size {
            let (_, _, len, _) = self.header(self.head);
            self.head = (self.head + HEADER + len) % LOG_BYTES;
            self.used -= HEADER + len;
            self.first += 1;
        }
        let mut h = [0u8; HEADER];
        h[0..8].copy_from_slice(&self.next.to_le_bytes());
        h[8..16].copy_from_slice(&crate::time::now().to_le_bytes());
        h[16..18].copy_from_slice(&(text.len() as u16).to_le_bytes());
        h[18..20].copy_from_slice(&prio.to_le_bytes());
        let tail = self.tail;
        self.put(tail, &h);
        self.put((tail + HEADER) % LOG_BYTES, text);
        self.tail = (tail + size) % LOG_BYTES;
        self.used += size;
        self.next += 1;
    }

    fn flush_pending(&mut self) {
        if self.pending_cpu.take().is_some() {
            let (prio, len) = (self.pending_prio, self.pending_len);
            let line = self.pending;
            self.append(prio, &line[..len]);
            self.pending_len = 0;
        }
    }

    /// Takes `text` into the pending line of `cpu`: each newline ends a record.
    fn feed(&mut self, cpu: u32, prio: u16, mut text: &[u8]) {
        if self.pending_cpu.is_some_and(|c| c != cpu) || (self.pending_cpu.is_some() && self.pending_prio != prio) {
            self.flush_pending();
        }
        while !text.is_empty() {
            let end = text.iter().position(|&b| b == b'\n').unwrap_or(text.len());
            for chunk in text[..end].chunks(LINE_MAX) {
                if self.pending_len + chunk.len() > LINE_MAX {
                    self.flush_pending();
                }
                self.pending[self.pending_len..self.pending_len + chunk.len()].copy_from_slice(chunk);
                self.pending_len += chunk.len();
                self.pending_cpu = Some(cpu);
                self.pending_prio = prio;
            }
            if end < text.len() {
                // (An empty line is a record too.)
                self.pending_cpu = Some(cpu);
                self.pending_prio = prio;
                self.flush_pending();
                text = &text[end + 1..];
            } else {
                text = &text[end..];
            }
        }
    }
}

/// Logs `text` of priority `prio` (pieces of lines may come in several calls).
pub fn log(prio: u16, text: &[u8]) {
    if PANICKING.load(Ordering::Relaxed) {
        return;
    }
    let cpu = crate::smp::cpu().index as u32;
    let next = {
        let mut ring = RING.lock();
        ring.feed(cpu, prio, text);
        ring.next
    };
    announce(next);
}

/// Logs one whole record (a write to `/dev/kmsg`): a pending line ends first.
pub fn log_record(prio: u16, text: &[u8]) {
    if PANICKING.load(Ordering::Relaxed) {
        return;
    }
    let text = &text[..text.len().min(LINE_MAX)];
    let next = {
        let mut ring = RING.lock();
        ring.flush_pending();
        ring.append(prio, text);
        ring.next
    };
    announce(next);
}

/// Wakes the watches that records up to `next` pass.
fn announce(next: u64) {
    if ARMED.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut watches = WATCHES.lock();
    for w in watches.iter_mut().filter(|w| !w.fired && w.seq < next) {
        w.fired = true;
        ARMED.fetch_sub(1, Ordering::AcqRel);
        crate::process::sched::wakeup(w.chan);
    }
}

/// Whether the console shows records of priority `prio` (its level, the low 3 bits).
pub fn on_console(prio: u16) -> bool {
    ((prio & 7) as u32) < CONSOLE_LEVEL.load(Ordering::Relaxed)
}

/// From now on nothing is logged (the panic's message goes to the console only).
pub fn panic() {
    PANICKING.store(true, Ordering::Relaxed);
    CONSOLE_LEVEL.store(15, Ordering::Relaxed);
}

/// The records from sequence `from` on (or the first there is) that fit in `cap` bytes of
/// `restricted::KlogRecord` headers and their texts, each padded to 8 bytes; and the window.
pub fn read(from: u64, cap: usize, out: &mut Vec<u8>) -> (u64, u64) {
    use restricted::KlogRecord;
    let head = core::mem::size_of::<KlogRecord>();
    let ring = RING.lock();
    let mut seq = ring.first;
    let mut at = ring.head;
    while seq < ring.next {
        let (s, ts, len, prio) = ring.header(at);
        debug_assert_eq!(s, seq);
        if seq >= from {
            let size = (head + len).next_multiple_of(8);
            if out.len() + size > cap {
                break;
            }
            let record = KlogRecord { seq, ts_ns: ts, len: len as u32, prio: prio as u32 };
            let start = out.len();
            out.resize(start + size, 0);
            out[start..start + head].copy_from_slice(record.as_bytes());
            ring.get((at + HEADER) % LOG_BYTES, &mut out[start + head..start + head + len]);
        }
        at = (at + HEADER + len) % LOG_BYTES;
        seq += 1;
    }
    (ring.first, ring.next)
}

/// Arms instance `instance`'s watch for a record of sequence `seq` or later, woken on
/// `chan`; false if one is there already (nothing armed).
pub fn arm(instance: u64, chan: usize, seq: u64) -> Result<bool, i64> {
    let mut watches = WATCHES.lock();
    // (Under the watches' lock: an append that passes `seq` after this finds the watch.)
    if RING.lock().next > seq {
        return Ok(false);
    }
    match watches.iter_mut().find(|w| w.instance == instance) {
        Some(w) => {
            if w.fired {
                ARMED.fetch_add(1, Ordering::AcqRel);
            }
            w.seq = if w.fired { seq } else { w.seq.min(seq) };
            w.fired = false;
        }
        None => {
            watches.try_reserve(1).map_err(|_| crate::process::errno::ENOMEM)?;
            watches.push(Watch { instance, chan, seq, fired: false });
            ARMED.fetch_add(1, Ordering::AcqRel);
        }
    }
    Ok(true)
}

/// For instance `instance`'s event wait: whether its watch fired (taken now).
pub fn take_event(instance: u64) -> bool {
    if instance == 0 {
        return false;
    }
    let mut watches = WATCHES.lock();
    match watches.iter().position(|w| w.instance == instance && w.fired) {
        Some(i) => {
            watches.swap_remove(i);
            true
        }
        None => false,
    }
}

/// An instance ended: its watch goes.
pub fn forget(instance: u64) {
    let mut watches = WATCHES.lock();
    if let Some(i) = watches.iter().position(|w| w.instance == instance) {
        if !watches[i].fired {
            ARMED.fetch_sub(1, Ordering::AcqRel);
        }
        watches.swap_remove(i);
    }
}

/// Text someone else chose (a server's log line, a thread's name) in a kernel message:
/// control characters (C0, DEL and C1) as `\xNN`, as Linux's `%pE`, so it cannot move
/// the console's cursor, change its palette or make it scroll.
pub struct Escaped<'a>(pub &'a str);

impl core::fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        for c in self.0.chars() {
            if c.is_control() {
                write!(f, "\\x{:02x}", c as u32)?;
            } else {
                core::fmt::Write::write_char(f, c)?;
            }
        }
        Ok(())
    }
}

/// Linux's ratelimit of writes to /dev/kmsg (`printk.devkmsg=ratelimit`): `BURST` records
/// per `INTERVAL`, the rest dropped and counted, their number logged when the next interval
/// starts. Per instance, for trees without the host grant (a tree cannot flood the log
/// the others and the host read).
pub struct RateLimit(IrqSpinLock<(u64, u32, u32)>);

impl RateLimit {
    const INTERVAL: u64 = 5_000_000_000;
    const BURST: u32 = 10;

    pub const fn new() -> RateLimit {
        RateLimit(IrqSpinLock::new((0, 0, 0)))
    }

    /// Whether a record may go now.
    pub fn allow(&self) -> bool {
        let now = crate::time::now();
        let missed = {
            let mut s = self.0.lock();
            let (begin, printed, missed) = &mut *s;
            let mut told = 0;
            if *begin == 0 || now - *begin >= Self::INTERVAL {
                told = core::mem::take(missed);
                *begin = now;
                *printed = 0;
            }
            let ok = *printed < Self::BURST;
            if ok {
                *printed += 1;
            } else {
                *missed += 1;
            }
            if !ok {
                return false;
            }
            told
        };
        if missed != 0 {
            let mut text = heapless::String::<64>::new();
            let _ = core::fmt::Write::write_fmt(&mut text, format_args!("kmsg: {missed} output lines suppressed due to ratelimiting"));
            log_record(4, text.as_bytes());
        }
        true
    }
}

/// `syslog`'s console actions: 6 (`CONSOLE_OFF`), 7 (`CONSOLE_ON`), 8 (`CONSOLE_LEVEL`,
/// `level` 1..=8). EINVAL for another action or level.
pub fn console(action: u64, level: u64) -> Result<(), i64> {
    match action {
        6 => {
            let old = CONSOLE_LEVEL.swap(CONSOLE_MIN, Ordering::Relaxed);
            if SAVED_LEVEL.load(Ordering::Relaxed) == 0 {
                SAVED_LEVEL.store(old, Ordering::Relaxed);
            }
        }
        7 => {
            let saved = SAVED_LEVEL.swap(0, Ordering::Relaxed);
            if saved != 0 {
                CONSOLE_LEVEL.store(saved, Ordering::Relaxed);
            }
        }
        8 if (1..=8).contains(&level) => {
            CONSOLE_LEVEL.store((level as u32).max(CONSOLE_MIN), Ordering::Relaxed);
            SAVED_LEVEL.store(0, Ordering::Relaxed);
        }
        _ => return Err(crate::process::errno::EINVAL),
    }
    Ok(())
}
