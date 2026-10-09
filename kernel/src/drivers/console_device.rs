//! The console as a device of the Linux server (ADR 0004, ADR 0007): the bytes typed on
//! the keyboard and the console's answers to queries go into a ring, the bytes written go
//! to the screen and the serial mirror as they are; nothing is interpreted (the line
//! discipline is the server's, docs/design/linux-server.md "The terminal").
//!
//! One holder at a time: a Linux server instance (by id) or, with none, the kernel's
//! monitor. Input wakes the holder: the keyboard interrupt sets `PENDING` and wakes the
//! holder's service thread's channel (`HOLDER_CHAN`, the instance's pager channel), whose
//! event wait checks the flag (`take_event`). No lock of an instance is taken in interrupt
//! context. Who becomes the holder and the events about it are `process::linux`'s
//! (`console_grant`).

use super::console;
use crate::process::sched::{prepare_to_wait, wakeup};
use crate::sync::IrqSpinLock;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use heapless::Deque;

/// The input ring: what does not fit is dropped (a keyboard cannot wait).
const RING: usize = 4096;

static INPUT: IrqSpinLock<Deque<u8, RING>> = IrqSpinLock::new(Deque::new());
/// The instance holding the device (0: the kernel's monitor).
static HOLDER: AtomicU64 = AtomicU64::new(0);
/// Where the holder's service thread waits (0: the monitor's channel).
static HOLDER_CHAN: AtomicUsize = AtomicUsize::new(0);
/// Input came that the holder has not been told of.
static PENDING: AtomicBool = AtomicBool::new(false);

/// The monitor waits here for input.
const MONITOR_CHAN: usize = 0x6_0000_0001;

/// Feeds bytes into the device (the keyboard's, from its interrupt; the console's
/// answers). What does not fit is dropped.
pub fn input(bytes: &[u8]) {
    {
        let mut ring = INPUT.lock();
        for &b in bytes {
            if ring.push_back(b).is_err() {
                break;
            }
        }
    }
    PENDING.store(true, Ordering::Release);
    let chan = HOLDER_CHAN.load(Ordering::Acquire);
    wakeup(if chan != 0 { chan } else { MONITOR_CHAN });
}

/// The holder: an instance's id, 0 for the monitor.
pub fn holder() -> u64 {
    HOLDER.load(Ordering::Acquire)
}

/// Serializes changes of the holder (the interrupt only reads it).
static CHANGE: spin::Mutex<()> = spin::Mutex::new(());

/// Makes `id` (0: the monitor) the holder, whose service thread waits on `chan`;
/// returns the previous holder. Input already there is the new holder's.
pub fn set_holder(id: u64, chan: usize) -> u64 {
    let _change = CHANGE.lock();
    HOLDER_CHAN.store(chan, Ordering::Release);
    let old = HOLDER.swap(id, Ordering::AcqRel);
    if !INPUT.lock().is_empty() {
        PENDING.store(true, Ordering::Release);
        wakeup(if chan != 0 { chan } else { MONITOR_CHAN });
    }
    old
}

/// Instance `id` is gone: if it still held the device, the monitor does.
pub fn release(id: u64) {
    let _change = CHANGE.lock();
    if HOLDER.load(Ordering::Acquire) == id {
        HOLDER_CHAN.store(0, Ordering::Release);
        HOLDER.store(0, Ordering::Release);
        wakeup(MONITOR_CHAN);
    }
}

/// For the event wait of instance `id`'s service thread: whether input came for it
/// that it has not been told of (told now).
pub fn take_event(id: u64) -> bool {
    id != 0 && HOLDER.load(Ordering::Acquire) == id && PENDING.swap(false, Ordering::AcqRel)
}

/// Takes up to `out.len()` bytes of input.
pub fn read(out: &mut [u8]) -> usize {
    let mut ring = INPUT.lock();
    let mut n = 0;
    while n < out.len() {
        let Some(b) = ring.pop_front() else { break };
        out[n] = b;
        n += 1;
    }
    n
}

/// Held across a whole write of the holder's (`writer`): one write's bytes are not
/// interleaved with another's. A sleeping lock of the kernel's: its holder may be
/// preempted between the console's budgeted pieces like any thread (a lock of the
/// server's would give it the weight of a lock holder for the whole write).
static WRITER: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

/// The right to write: held across one write of the holder (`write` its pieces).
pub fn writer() -> crate::sync::MutexGuard<'static, ()> {
    WRITER.lock()
}

/// Writes `bytes` to the console as they are; the console's answers to queries in
/// them (a cursor position report) become input.
pub fn write(bytes: &[u8]) {
    console::write_bytes(bytes);
    let mut reply = [0u8; 32];
    let n = console::take_reply(&mut reply);
    if n > 0 {
        input(&reply[..n]);
    }
}

/// The monitor's blocking read of one byte, while it holds the device.
pub fn monitor_read() -> u8 {
    loop {
        let wait = prepare_to_wait(MONITOR_CHAN);
        let mut b = [0u8; 1];
        if HOLDER.load(Ordering::Acquire) == 0 && read(&mut b) == 1 {
            return b[0];
        }
        wait.sleep();
    }
}
