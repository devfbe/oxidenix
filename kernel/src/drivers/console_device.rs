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
//!
//! Writes: a write of the holder's goes out whole in a writer's turn (`writer`, a fair
//! sleeping lock of the kernel's), its pieces interleaved with echoes. Echoes (the line
//! discipline's, from the server's service thread) never wait: they go into a bounded
//! queue (`ECHO`, overflow dropped, as Linux's echo buffer) that whoever has the turn
//! drains between its pieces, or the echoing thread itself when nobody has it. So input
//! processing, and with it the instance's page faults, never stalls behind a program
//! flooding the console.

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
/// Advanced at every change of the holder (see `drain_echo`).
static HOLDER_GEN: AtomicU64 = AtomicU64::new(0);

/// Makes `id` (0: the monitor) the holder, whose service thread waits on `chan`;
/// returns the previous holder. Input already there is the new holder's.
pub fn set_holder(id: u64, chan: usize) -> u64 {
    let _change = CHANGE.lock();
    HOLDER_CHAN.store(chan, Ordering::Release);
    let old = HOLDER.swap(id, Ordering::AcqRel);
    // Echoes of the previous holder's input are not the new one's: those queued go,
    // and a batch a writer took out already is dropped when it sees the generation.
    forget_echoes();
    // Writers of the old holder waiting for a turn give up (EIO).
    wakeup(TURN_CHAN);
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
        forget_echoes();
        wakeup(TURN_CHAN);
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

/// The writers' turns (`writer`): held across a whole write of the holder's, so one
/// write's bytes are not interleaved with another's. A sleeping lock of the kernel's
/// (its holder may be preempted between the console's budgeted pieces like any thread;
/// a lock of the server's would give it the weight of a lock holder for the whole
/// write), first come first served (a writer that writes again at once queues behind
/// the others: a flood starves nobody), and a waiter a signal interrupts gives up its
/// turn (`abandoned`, skipped when it comes).
struct Turns {
    next: u64,
    serving: u64,
    abandoned: alloc::collections::BTreeSet<u64>,
}

static TURNS: spin::Mutex<Turns> = spin::Mutex::new(Turns { next: 0, serving: 0, abandoned: alloc::collections::BTreeSet::new() });
/// Where writers wait for their turn.
const TURN_CHAN: usize = 0x6_0000_0003;

/// A writer's turn; the next one's when dropped.
pub struct Writer(());

impl Drop for Writer {
    fn drop(&mut self) {
        {
            let mut t = TURNS.lock();
            t.serving += 1;
            loop {
                let s = t.serving;
                if !t.abandoned.remove(&s) {
                    break;
                }
                t.serving += 1;
            }
        }
        wakeup(TURN_CHAN);
    }
}

/// Waits for instance `holder`'s turn to write (one write's pieces). A signal does not
/// end the wait (the write's output is processed already; turns are held for one
/// bounded write, within one call); a dying thread gives up (EINTR), and so does a
/// waiter whose instance no longer holds the device (EIO: a change of the holder wakes
/// the waiters). A ticket given up is skipped when it comes.
fn writer(holder: u64) -> Result<Writer, i64> {
    use crate::process::errno::{EINTR, EIO};
    let ticket = {
        let mut t = TURNS.lock();
        t.next += 1;
        t.next - 1
    };
    loop {
        let wait = prepare_to_wait(TURN_CHAN);
        {
            let mut t = TURNS.lock();
            let gone = if HOLDER.load(Ordering::Acquire) != holder {
                Some(EIO)
            } else if crate::process::signal::dying() {
                Some(EINTR)
            } else {
                None
            };
            if t.serving == ticket {
                if gone.is_none() {
                    return Ok(Writer(()));
                }
                // Came just now: passed on.
                drop(t);
                drop(Writer(()));
                return Err(gone.unwrap_or(EIO));
            }
            if let Some(e) = gone {
                t.abandoned.insert(ticket);
                return Err(e);
            }
        }
        wait.sleep();
    }
}

/// Writes all of `bytes` (the kernel's copy of a write of instance `holder`'s) in
/// one turn, piece by piece, with the echoes that came between the pieces; EIO when
/// the instance loses the device (before or during: the rest goes).
pub fn write_all(holder: u64, bytes: &[u8]) -> Result<(), i64> {
    let result = {
        let _turn = writer(holder)?;
        let mut result = Ok(());
        for piece in bytes.chunks(512) {
            if HOLDER.load(Ordering::Acquire) != holder {
                result = Err(crate::process::errno::EIO);
                break;
            }
            write(piece);
        }
        result
    };
    // Echoes queued after the last piece go out (unless another writer has the turn).
    flush_echo();
    result
}

/// The turn if nobody writes or waits now (never sleeps).
fn try_writer() -> Option<Writer> {
    let mut t = TURNS.lock();
    if t.next != t.serving {
        return None;
    }
    t.next += 1;
    Some(Writer(()))
}

/// Echoes waiting for the console (see the module comment).
const ECHO_QUEUE: usize = 4096;
static ECHO: IrqSpinLock<Deque<u8, ECHO_QUEUE>> = IrqSpinLock::new(Deque::new());

/// Writes a piece of the holder's write (the writer's turn held) to the console as it is, then
/// the echoes that came meanwhile. The console's answers to queries in the bytes (a
/// cursor position report) become input; answers to the kernel's own text do not.
fn write(bytes: &[u8]) {
    out(bytes);
    drain_echo();
}

fn out(bytes: &[u8]) {
    console::write_with_replies(bytes, &mut |reply| input(reply));
}

/// The holder changed: its echoes go, those queued and (by the generation, advanced
/// under the queue's lock with them) a batch a writer has taken out already.
fn forget_echoes() {
    let mut q = ECHO.lock();
    HOLDER_GEN.fetch_add(1, Ordering::AcqRel);
    q.clear();
}

/// Writes the queued echoes (the writer's turn held). A batch is written only if the
/// holder it was taken for still holds the device.
fn drain_echo() {
    let mut buf = [0u8; 256];
    loop {
        let (n, gen) = {
            let mut q = ECHO.lock();
            let mut n = 0;
            while n < buf.len() {
                let Some(b) = q.pop_front() else { break };
                buf[n] = b;
                n += 1;
            }
            (n, HOLDER_GEN.load(Ordering::Acquire))
        };
        if n == 0 {
            return;
        }
        if HOLDER_GEN.load(Ordering::Acquire) == gen {
            out(&buf[..n]);
        }
    }
}

/// After a write released the writer's turn: echoes queued while it was held but after its last
/// piece go out (unless another writer holds it now, who takes them).
pub fn flush_echo() {
    while !ECHO.lock().is_empty() {
        let Some(_writer) = try_writer() else { return };
        drain_echo();
    }
}

/// Queues an echo for the console without ever waiting; what does not fit is dropped.
/// Returns how much was taken.
pub fn echo(holder: u64, bytes: &[u8]) -> Result<usize, i64> {
    let mut n = 0;
    {
        let mut q = ECHO.lock();
        // Checked under the queue's lock: a change of the holder (which empties the
        // queue after it changed the holder) never finds the old holder's echo after.
        if HOLDER.load(Ordering::Acquire) != holder {
            return Err(crate::process::errno::EIO);
        }
        for &b in bytes {
            if q.push_back(b).is_err() {
                break;
            }
            n += 1;
        }
    }
    flush_echo();
    Ok(n)
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
