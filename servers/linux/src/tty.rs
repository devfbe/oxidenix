//! Terminals (phase R6d, docs/design/linux-server.md "The terminal", ADR 0007): a line
//! discipline (`ldisc`), the job control state of a terminal (the session it controls,
//! its foreground process group, its window size), its hangups, and its driver: the
//! console (`console`) or a pseudo-terminal's slave (`pty`). Each open of a terminal is an
//! open file description of the server's (`TtyOpen`, a placeholder in the kernel's
//! descriptor table, as pipes are).
//!
//! Locks: `inner` holds the state and is never held across a copy to or from program
//! memory, nor across a wait or the console's write. A read or a write takes its turn
//! (`turn`, Linux's atomic_read_lock and atomic_write_lock) for the whole call: first
//! come first served, an interruptible wait on the turn's own counter (`turn_words`,
//! advanced only when the turn passes on), not a lock of `sync`, since it is held across
//! waits for input or room. Within a read the bytes are peeked under `inner`, copied to the program
//! with only `rlock` held, and consumed after, unless an input flush came in between
//! (`Inner::epoch`); a change of settings takes `rlock` for the change (order: `rlock`,
//! then `inner`). `rlock` is held across the copy, where a fault may wait for the pager:
//! safe, since only programs' reads and settings changes take it, never the pager. Output
//! is processed under `inner`; a pty's goes to its master's buffer there, the console's to
//! the device after (no lock of `sync` is held across it, which would give a flooding
//! writer a lock holder's priority for the whole write). Echoes take no turn and never
//! wait: the service thread processes the keyboard's input (`console::device_echo`).
//! Every change bumps `seq` and wakes its waiters (interruptible server futex waits, with
//! a deadline for VTIME), and reports the readiness of the terminal's open file
//! descriptions to the kernel under `inner`, so reports never arrive out of order.
//!
//! Process groups and sessions are the kernel's until R8: the server asks for them
//! (`ids`), sends the terminal's signals through the kernel (`signal`) and learns of a
//! session leader's end (`session_ended`). A process's controlling terminal is the one
//! whose session is the process's (ADR 0007).

use crate::files::{self, File, EFAULT, EINVAL, ENOTTY, O_ACCMODE, O_CLOEXEC, O_NONBLOCK};
use crate::namespace::Origin;
use crate::sync::Mutex;
use crate::syscall;
use crate::unix::{Sink, Source};
use crate::usercopy;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ldisc::*;
use restricted::*;

pub const EPERM: i64 = 1;
pub const ESRCH: i64 = 3;
pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const ENXIO: i64 = 6;
pub const EAGAIN: i64 = 11;

pub const O_NOCTTY: u32 = 0o400;

const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const POLLRDNORM: i16 = 0x40;
const POLLWRNORM: i16 = 0x100;
/// How a hung-up terminal's open file descriptions poll (Linux's `hung_up_tty_poll`).
const HUNG_UP: i16 = POLLIN | POLLOUT | POLLERR | POLLHUP | POLLRDNORM | POLLWRNORM;

const SIGHUP: u64 = 1;
const SIGCONT: u64 = 18;
const SIGTTIN: u64 = 21;
const SIGTTOU: u64 = 22;
const SIGWINCH: u64 = 28;

/// The slave's output a pty's master has not read yet, at most (its writers wait).
pub const MASTER_CAP: usize = 64 * 1024;
/// Echoes held while output is stopped, at most (more are dropped).
const HELD_ECHO: usize = 4096;
/// How far echoes may fill a pty master's buffer beyond `MASTER_CAP` (more are dropped).
pub const ECHO_ROOM: usize = 4096;
/// Bytes of a write processed at once (output processing may make 8 times as many).
const WRITE_CHUNK: usize = 1024;

// ioctl requests (asm-generic/ioctls.h); compared as the kernel's `unsigned int cmd`.
const TCGETS: u32 = 0x5401;
const TCSETS: u32 = 0x5402;
const TCSETSW: u32 = 0x5403;
const TCSETSF: u32 = 0x5404;
const TCSBRK: u32 = 0x5409;
const TCXONC: u32 = 0x540a;
const TCFLSH: u32 = 0x540b;
const TIOCEXCL: u32 = 0x540c;
const TIOCNXCL: u32 = 0x540d;
const TIOCSCTTY: u32 = 0x540e;
const TIOCGPGRP: u32 = 0x540f;
const TIOCSPGRP: u32 = 0x5410;
const TIOCOUTQ: u32 = 0x5411;
const TIOCSTI: u32 = 0x5412;
const TIOCGWINSZ: u32 = 0x5413;
const TIOCSWINSZ: u32 = 0x5414;
const FIONREAD: u32 = 0x541b;
const TIOCNOTTY: u32 = 0x5422;
const TIOCSETD: u32 = 0x5423;
const TIOCGETD: u32 = 0x5424;
const TCSBRKP: u32 = 0x5425;
const TIOCSBRK: u32 = 0x5427;
const TIOCCBRK: u32 = 0x5428;
const TIOCGSID: u32 = 0x5429;
const TCGETS2: u32 = 0x802c_542a;
const TCSETS2: u32 = 0x402c_542b;
const TCSETSW2: u32 = 0x402c_542c;
const TCSETSF2: u32 = 0x402c_542d;
const TIOCVHANGUP: u32 = 0x5437;
const TIOCGEXCL: u32 = 0x8004_5440;

/// What drives a terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Driver {
    Console,
    /// A pseudo-terminal's slave, by its index (`/dev/pts/n`).
    Pty(u32),
}

/// A pty's master side, in its terminal's state.
pub struct PtyState {
    /// The slave's output, for the master to read.
    pub out: VecDeque<u8>,
    /// Flushes of `out`: a master reader's copy is consumed only if none came between.
    pub out_epoch: u64,
    /// The slave cannot be opened (EIO) until unlocked (TIOCSPTLCK, `unlockpt`).
    pub locked: bool,
    /// The master's open file description lives.
    pub master_open: bool,
    /// Open file descriptions of the slave.
    pub slaves: usize,
    /// The slave's last description went (Linux's TTY_OTHER_CLOSED of the master): the
    /// master reads EIO once `out` is empty.
    pub slave_closed: bool,
    /// The master's placeholder, and the readiness last reported for it.
    pub master_id: u64,
    pub master_reported: i16,
}

pub struct Inner {
    pub ld: Ldisc,
    /// The session the terminal controls, and its foreground process group.
    pub session: Option<u64>,
    pub pgrp: Option<u64>,
    /// `struct winsize`: rows, columns, x and y pixels.
    pub winsize: [u16; 4],
    /// Hangups so far: open file descriptions of an earlier generation are hung up.
    gen: u32,
    /// Input flushes so far (see the module comment).
    epoch: u64,
    exclusive: bool,
    /// The turns' queues (`Tty::turn`, by `TURN_*`).
    turns: [TurnQueue; 4],
    /// Echoes made while output was stopped.
    held_echo: Vec<u8>,
    /// The open file descriptions' placeholders: their generation and the readiness last
    /// reported.
    opens: BTreeMap<u64, (u32, i16)>,
    pub pty: Option<PtyState>,
}

pub struct Tty {
    pub driver: Driver,
    pub inner: Mutex<Inner>,
    /// Bumped on every change; waiters sleep on it.
    seq: AtomicU32,
    /// Each turn's own change counter (`TURN_*`): its waiters sleep on it, woken only
    /// when the turn passes on, not by every change of the terminal.
    turn_words: [AtomicU32; 4],
    /// Reads (and settings changes), one at a time.
    rlock: Mutex<()>,
    /// The settings the driver starts with, which a hangup restores.
    init: Termios,
}

/// The turns a call takes for its whole length (Linux's atomic_read_lock and
/// atomic_write_lock, of the slave and of a pty's master).
pub const TURN_READ: usize = 0;
pub const TURN_WRITE: usize = 1;
pub const TURN_MASTER_READ: usize = 2;
pub const TURN_MASTER_WRITE: usize = 3;

/// The callers waiting for one turn, first come first served: each takes a ticket, the
/// turn goes to `serving` (free when `next` is `serving`). A caller that writes again at
/// once queues behind the others, so a writer flooding the terminal starves nobody (a
/// flag that the next caller takes would let it barge in before the woken waiter runs).
/// A waiter a signal interrupts gives its ticket up (`abandoned`, skipped when it comes).
#[derive(Default)]
struct TurnQueue {
    next: u32,
    serving: u32,
    abandoned: Vec<u32>,
}

impl TurnQueue {
    fn free(&self) -> bool {
        self.next == self.serving
    }

    /// The turn goes to the next ticket not given up.
    fn advance(&mut self) {
        self.serving = self.serving.wrapping_add(1);
        while let Some(i) = self.abandoned.iter().position(|&t| t == self.serving) {
            self.abandoned.swap_remove(i);
            self.serving = self.serving.wrapping_add(1);
        }
    }
}

/// A call's turn (`Tty::turn`), given back when dropped.
pub struct Turn<'a> {
    tty: &'a Tty,
    which: usize,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut inner = self.tty.inner.lock();
        inner.turns[self.which].advance();
        self.tty.turn_advanced(self.which);
    }
}

/// An open file description of a terminal.
pub struct TtyOpen {
    pub tty: Arc<Tty>,
    /// The terminal's hangup generation when it was opened.
    gen: u32,
    pub id: u64,
    /// The device node it was opened by (fstat, fchmod and the like).
    pub origin: Origin,
}

/// A process's ids (`process`).
#[derive(Clone, Copy, Debug)]
pub struct Ids {
    pub pid: u64,
    pub pgid: u64,
    pub sid: u64,
    pub orphaned: bool,
}

/// `ids`: `id` names a process group (one of its live members is asked about).
pub const IDS_PGRP: u64 = 1;
/// `ids`: also whether the group is orphaned.
pub const IDS_ORPHANED: u64 = 2;

/// The ids of process `id` (0: the caller), or with `IDS_PGRP` of a process of group `id`;
/// with `IDS_ORPHANED` also whether its group is orphaned. ESRCH for none.
pub fn ids(id: u64, flags: u64) -> Result<Ids, i64> {
    const ESRCH: i64 = 3;
    let (pid, pgid, sid) = if flags & IDS_PGRP != 0 {
        let sid = crate::process::pgrp_session(id as u32).ok_or(ESRCH)?;
        (0, id as u32, sid)
    } else {
        crate::process::ids_of(id as u32).ok_or(ESRCH)?
    };
    let orphaned = flags & IDS_ORPHANED != 0 && crate::process::pgrp_orphaned(pgid);
    Ok(Ids { pid: pid as u64, pgid: pgid as u64, sid: sid as u64, orphaned })
}

/// Whom `signal` sends to: a process group, or a session's leader (while it still leads
/// it).
#[derive(Clone, Copy)]
enum Scope {
    Pgrp,
    Leader,
}
use Scope::{Leader as SIGNAL_LEADER, Pgrp as SIGNAL_PGRP};

/// Sends `sig` from the terminal (SI_KERNEL) to process group `id` or the leader of
/// session `id`.
fn signal(scope: Scope, id: u64, sig: u64) {
    match scope {
        Scope::Pgrp => {
            crate::signal::send_pgrp(id as u32, sig as u32);
        }
        Scope::Leader => {
            if crate::process::ids_of(id as u32).is_some_and(|(pid, _, sid)| pid as u64 == id && sid as u64 == id) {
                crate::signal::send_process(id as u32, sig as u32);
            }
        }
    }
}

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64
}

/// Every terminal of the instance.
pub fn all() -> Vec<Arc<Tty>> {
    let mut ttys: Vec<Arc<Tty>> = crate::pty::all();
    if let Some(c) = crate::console::current() {
        ttys.push(c);
    }
    ttys
}

/// The controlling terminal of session `sid`.
pub fn ctty_of(sid: u64) -> Option<Arc<Tty>> {
    all().into_iter().find(|t| t.inner.lock().session == Some(sid))
}

/// The controlling terminal of session `sid` as /proc/<pid>/stat shows it:
/// its device number (`tty_nr`, 0 for none) and its foreground process
/// group (`tpgid`, -1 for none).
pub fn proc_fields(sid: u64) -> (u64, i64) {
    let Some(tty) = ctty_of(sid) else { return (0, -1) };
    let rdev = match tty.driver {
        Driver::Console => vfs::stat::dev_make(5, 1),
        Driver::Pty(n) => vfs::stat::dev_make(crate::pty::SLAVE_MAJOR, n),
    };
    let fg = tty.inner.lock().pgrp.map_or(-1, |g| g as i64);
    (rdev, fg)
}

/// Opens the terminal device with number `rdev` (the character device node `origin`), if
/// it is one: (5,0) /dev/tty, (5,1) /dev/console, (5,2) /dev/ptmx, (136,n) /dev/pts/n.
/// None for another device.
pub fn open_device(rdev: u64, flags: u32, origin: Origin) -> Option<Result<i64, i64>> {
    Some(match vfs::stat::dev_split(rdev) {
        (5, 0) => ids(0, 0).and_then(|me| {
            let tty = ctty_of(me.sid).ok_or(ENXIO)?;
            open(&tty, flags, origin, false)
        }),
        (5, 1) => crate::console::open(flags, origin),
        (5, 2) => crate::pty::open_master(flags, origin),
        (136, n) => crate::pty::open_slave(n, flags, origin),
        _ => return None,
    })
}

/// A new open file description of `tty` for the caller, opened by the node `origin`;
/// `ctty`: the open may make it the caller's controlling terminal (a session leader
/// without one opening a terminal that controls no session, unless O_NOCTTY: Linux's
/// `tty_open_proc_set_tty`).
pub fn open(tty: &Arc<Tty>, flags: u32, origin: Origin, ctty: bool) -> Result<i64, i64> {
    let id = files::new_id();
    let (open, ready, was_closed) = {
        let mut inner = tty.inner.lock();
        let gen = inner.gen;
        let ready = tty.readiness(&inner, gen);
        inner.opens.insert(id, (gen, ready));
        let mut was_closed = false;
        if let Some(p) = inner.pty.as_mut() {
            p.slaves += 1;
            was_closed = core::mem::replace(&mut p.slave_closed, false);
        }
        tty.changed(&mut inner, false, false);
        (Arc::new(TtyOpen { tty: tty.clone(), gen, id, origin }), ready, was_closed)
    };
    let fd = match files::install(id, File::Tty(open.clone()), flags & (O_ACCMODE | O_NONBLOCK | O_CLOEXEC), ready) {
        Ok(fd) => fd,
        Err(e) => {
            // Never opened: the slave is as it was (a slave never opened is not
            // closed for the master).
            let mut inner = tty.inner.lock();
            inner.opens.remove(&id);
            if let Some(p) = inner.pty.as_mut() {
                p.slaves = p.slaves.saturating_sub(1);
                p.slave_closed = was_closed;
            }
            tty.changed(&mut inner, false, false);
            return Err(e);
        }
    };
    if ctty && flags & O_NOCTTY == 0 {
        if let Ok(me) = ids(0, 0) {
            if me.pid == me.sid && ctty_of(me.sid).is_none() {
                let mut inner = tty.inner.lock();
                if inner.session.is_none() {
                    inner.session = Some(me.sid);
                    inner.pgrp = Some(me.pgid);
                }
            }
        }
    }
    Ok(fd)
}

/// A session leader's process ended (`EVENT_SESSION_END`): its controlling terminal is
/// dissociated (Linux's `disassociate_ctty(1)`): the console is hung up, a pty's
/// foreground group gets SIGHUP.
pub fn session_ended(sid: u64) {
    for tty in all() {
        if tty.inner.lock().session != Some(sid) {
            continue;
        }
        match tty.driver {
            Driver::Console => tty.hangup(true),
            Driver::Pty(_) => {
                let fg = {
                    let mut inner = tty.inner.lock();
                    if inner.session != Some(sid) {
                        continue;
                    }
                    inner.session = None;
                    inner.pgrp.take()
                };
                if let Some(fg) = fg {
                    signal(SIGNAL_PGRP, fg, SIGHUP);
                }
            }
        }
    }
}

impl Tty {
    pub fn new(driver: Driver, init: Termios, winsize: [u16; 4], pty: Option<PtyState>) -> Arc<Tty> {
        Arc::new(Tty {
            driver,
            inner: Mutex::new(Inner {
                ld: Ldisc::new(init),
                session: None,
                pgrp: None,
                winsize,
                gen: 0,
                epoch: 0,
                exclusive: false,
                turns: Default::default(),
                held_echo: Vec::new(),
                opens: BTreeMap::new(),
                pty,
            }),
            seq: AtomicU32::new(0),
            turn_words: Default::default(),
            rlock: Mutex::new(()),
            init,
        })
    }

    /// The readiness of an open file description of generation `gen`.
    fn readiness(&self, inner: &Inner, gen: u32) -> i16 {
        if gen != inner.gen {
            return HUNG_UP;
        }
        let mut ready = 0;
        if inner.ld.readable(true) {
            ready |= POLLIN | POLLRDNORM;
        }
        if Self::writable(inner) {
            ready |= POLLOUT | POLLWRNORM;
        }
        if inner.pty.as_ref().is_some_and(|p| !p.master_open) {
            ready |= POLLHUP;
        }
        ready
    }

    /// Whether output may go now: not stopped, and a pty's master has room.
    fn writable(inner: &Inner) -> bool {
        !inner.ld.stopped() && inner.pty.as_ref().is_none_or(|p| p.out.len() < MASTER_CAP)
    }

    /// The readiness of a pty's master.
    pub fn master_readiness(inner: &Inner) -> i16 {
        let Some(p) = &inner.pty else { return 0 };
        let mut ready = 0;
        if !p.out.is_empty() {
            ready |= POLLIN | POLLRDNORM;
        }
        if inner.ld.room() {
            ready |= POLLOUT | POLLWRNORM;
        }
        if p.slave_closed {
            ready |= POLLHUP;
        }
        ready
    }

    /// After a change (lock held): wakes the waiters and reports the readiness that
    /// changed; `input` (new input for the readers) and `output` (new output for a pty's
    /// master) are events even if readiness stays (an edge for EPOLLET).
    pub fn changed(&self, inner: &mut Inner, input: bool, output: bool) {
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        let current = self.readiness(inner, inner.gen);
        let gen = inner.gen;
        for (&id, (open_gen, reported)) in inner.opens.iter_mut() {
            let now = if *open_gen == gen { current } else { HUNG_UP };
            if now != *reported || (input && *open_gen == gen) {
                *reported = now;
                files::ready(id, now);
            }
        }
        let master = Self::master_readiness(inner);
        if let Some(p) = inner.pty.as_mut() {
            if p.master_open && (master != p.master_reported || output) {
                p.master_reported = master;
                files::ready(p.master_id, master);
            }
        }
    }

    /// Sleeps while `seq` is `seen`, until `deadline` (0: none); EINTR for a signal.
    pub fn wait(&self, seen: u32, deadline: u64) -> Result<(), i64> {
        let word = &self.seq as *const AtomicU32 as u64;
        match syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, deadline, FUTEX_INTERRUPTIBLE, 0, 0]) {
            r if r == -EINTR => Err(EINTR),
            _ => Ok(()),
        }
    }

    /// The change counter, read before a wait (under `inner`).
    pub fn seen(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }

    fn hung_up(&self, open: &TtyOpen) -> bool {
        self.inner.lock().gen != open.gen
    }

    /// Linux's `tty_check_change` (`sig` SIGTTOU) and `job_control` (SIGTTIN): a process
    /// of a background group of the session the terminal controls gets `sig` for its
    /// group and the call restarts after it (EINTR); EIO if its group is orphaned, and
    /// for SIGTTIN if it ignores or blocks it (with SIGTTOU it may go on).
    fn job_check(&self, sig: u64) -> Result<(), i64> {
        let (session, fg) = {
            let inner = self.inner.lock();
            (inner.session, inner.pgrp)
        };
        let (Some(session), Some(fg)) = (session, fg) else { return Ok(()) };
        let me = ids(0, 0)?;
        if me.sid != session || me.pgid == fg {
            return Ok(());
        }
        let (ignored, blocked) = crate::signal::ignored_or_blocked(sig as u32);
        if ignored || blocked {
            return if sig == SIGTTIN { Err(EIO) } else { Ok(()) };
        }
        if ids(0, IDS_ORPHANED)?.orphaned {
            return Err(EIO);
        }
        signal(SIGNAL_PGRP, me.pgid, sig);
        Err(EINTR)
    }

    /// Whether this terminal is the caller's controlling terminal.
    fn is_ctty(&self) -> bool {
        let session = self.inner.lock().session;
        session.is_some() && ids(0, 0).is_ok_and(|me| Some(me.sid) == session)
    }

    /// Receives a byte of input (lock held): echoes into `echo`, signals for the
    /// foreground group into `signals`. A signal's flush empties a pty's output too.
    fn receive(&self, inner: &mut Inner, b: u8, echo: &mut Vec<u8>, signals: &mut Vec<(u64, u64)>) {
        let was_stopped = inner.ld.stopped();
        let r = inner.ld.receive(b, echo);
        if r.flushed {
            inner.epoch += 1;
            inner.held_echo.clear();
            if let Some(p) = inner.pty.as_mut() {
                p.out.clear();
                p.out_epoch += 1;
            }
        }
        if let (Some(sig), Some(fg)) = (r.signal, inner.pgrp) {
            signals.push((fg, sig as u64));
        }
        if was_stopped && !inner.ld.stopped() && !inner.held_echo.is_empty() {
            let held = core::mem::take(&mut inner.held_echo);
            echo.splice(0..0, held);
        }
    }

    /// Places echoes (lock held): held while output is stopped, into a pty's output, or
    /// returned for the console (written by the caller after the lock).
    fn place_echo(&self, inner: &mut Inner, echo: Vec<u8>) -> Option<Vec<u8>> {
        if echo.is_empty() {
            return None;
        }
        if inner.ld.stopped() {
            let room = HELD_ECHO.saturating_sub(inner.held_echo.len());
            inner.held_echo.extend_from_slice(&echo[..echo.len().min(room)]);
            return None;
        }
        match inner.pty.as_mut() {
            Some(p) => {
                // Echoes may go `ECHO_ROOM` beyond the room writers wait for, no
                // further: the rest is dropped (Linux's echo buffer is 4 KiB), so a
                // master that writes but never reads cannot grow the buffer.
                let room = (MASTER_CAP + ECHO_ROOM).saturating_sub(p.out.len());
                p.out.extend(&echo[..echo.len().min(room)]);
                None
            }
            None => Some(echo),
        }
    }

    /// Writes to the console device, no lock held (the flow control characters of TCIOFF
    /// and TCION; a dying thread's or a lost device's bytes go).
    fn console_write(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = crate::console::device_write(bytes);
        }
    }

    /// The turn `which` (`TURN_*`) for a whole call; EAGAIN with `nonblock` if another
    /// call has it, EINTR for a signal while waiting. An interruptible wait on the
    /// turn's counter (`turn_words`), not a lock of `sync`: it is held across waits for
    /// input or room, where a
    /// program's signals must reach the waiter (and no lock holder's priority is due).
    pub fn turn(&self, which: usize, nonblock: bool) -> Result<Turn<'_>, i64> {
        let ticket = {
            let mut inner = self.inner.lock();
            let q = &mut inner.turns[which];
            if nonblock && !q.free() {
                return Err(EAGAIN);
            }
            q.next = q.next.wrapping_add(1);
            q.next.wrapping_sub(1)
        };
        let word = &self.turn_words[which];
        loop {
            let seen = {
                let inner = self.inner.lock();
                if inner.turns[which].serving == ticket {
                    return Ok(Turn { tty: self, which });
                }
                word.load(Ordering::Acquire)
            };
            let addr = word as *const AtomicU32 as u64;
            if syscall(SYS_SERVER_FUTEX_WAIT, [addr, seen as u64, 0, FUTEX_INTERRUPTIBLE, 0, 0]) == -EINTR {
                // Given up: skipped when it comes (or passed on, if it came just now).
                let mut inner = self.inner.lock();
                let q = &mut inner.turns[which];
                if q.serving == ticket {
                    q.advance();
                    self.turn_advanced(which);
                } else {
                    q.abandoned.push(ticket);
                }
                return Err(EINTR);
            }
        }
    }

    /// Turn `which` passed on (lock held): its waiters look whose it is.
    fn turn_advanced(&self, which: usize) {
        let word = &self.turn_words[which];
        word.fetch_add(1, Ordering::Release);
        syscall(SYS_SERVER_FUTEX_WAKE, [word as *const AtomicU32 as u64, i32::MAX as u64, 0, 0, 0, 0]);
    }

    /// Echoes to the console device: never waits (the service thread processes the
    /// keyboard's input; see `console::device_echo`).
    fn console_echo(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            crate::console::device_echo(bytes);
        }
    }

    fn send(signals: Vec<(u64, u64)>) {
        for (pgrp, sig) in signals {
            signal(SIGNAL_PGRP, pgrp, sig);
        }
    }

    /// Input from the device (the keyboard, TIOCSTI): every byte is taken (what does not
    /// fit is dropped); the signals go first (as Linux's `isig` in the receive path),
    /// then the echoes, which never wait.
    pub fn input(&self, bytes: &[u8]) {
        let mut echo = Vec::new();
        let mut signals = Vec::new();
        let out = {
            let mut inner = self.inner.lock();
            for &b in bytes {
                self.receive(&mut inner, b, &mut echo, &mut signals);
            }
            let out = self.place_echo(&mut inner, echo);
            self.changed(&mut inner, true, true);
            out
        };
        Self::send(signals);
        if let Some(out) = out {
            self.console_echo(&out);
        }
    }

    /// Input from a pty's master: waits while the line discipline is full
    /// (noncanonical mode), so nothing is dropped.
    pub fn master_write(&self, mut src: Source, nonblock: bool) -> Result<i64, i64> {
        let _turn = self.turn(TURN_MASTER_WRITE, nonblock)?;
        let mut written = 0usize;
        while src.left() > 0 {
            // A signal ends the write between chunks (Linux's n_tty_write checks
            // signal_pending each pass): what went, or EINTR (restarted) if nothing.
            if files::signal_pending() {
                return if written > 0 { Ok(written as i64) } else { Err(EINTR) };
            }
            let chunk = match src.take(WRITE_CHUNK) {
                Ok(c) => c,
                Err(e) => return if written > 0 { Ok(written as i64) } else { Err(e) },
            };
            let mut i = 0;
            while i < chunk.len() {
                let mut echo = Vec::new();
                let mut signals = Vec::new();
                let seen = {
                    let mut inner = self.inner.lock();
                    let start = i;
                    while i < chunk.len() && inner.ld.room() {
                        self.receive(&mut inner, chunk[i], &mut echo, &mut signals);
                        i += 1;
                    }
                    let _ = self.place_echo(&mut inner, echo);
                    if i > start {
                        self.changed(&mut inner, true, true);
                    }
                    self.seen()
                };
                Self::send(signals);
                if i < chunk.len() {
                    let done = written + i;
                    if nonblock {
                        return if done > 0 { Ok(done as i64) } else { Err(EAGAIN) };
                    }
                    if let Err(e) = self.wait(seen, 0) {
                        return if done > 0 { Ok(done as i64) } else { Err(e) };
                    }
                }
            }
            written += chunk.len();
        }
        Ok(written as i64)
    }

    /// read(2) (Linux's `n_tty_read`): canonical mode a line at most; noncanonical as
    /// VMIN and VTIME say.
    pub fn read(&self, open: &TtyOpen, mut sink: Sink, nonblock: bool) -> Result<i64, i64> {
        if self.hung_up(open) {
            return Ok(0);
        }
        self.job_check(SIGTTIN)?;
        if sink.room() == 0 {
            return Ok(0);
        }
        // One read at a time, for the whole read (Linux's atomic_read_lock): a line or
        // a VMIN batch is never split between readers.
        let _turn = self.turn(TURN_READ, nonblock)?;
        // VMIN and VTIME as at the start (Linux reads them once, too): at least
        // `minimum` bytes, with VTIME between bytes once one came (`time`), or with
        // VMIN 0 as the whole read's timeout (0: none). Whether the read is canonical
        // is asked at every pass (a mode switch while it waits applies at once, as
        // Linux's `ldata->icanon`); begun canonical, `minimum` is 0: switched to raw
        // mode it ends with what came.
        let (minimum, time, mut deadline) = match self.inner.lock().ld.mode() {
            ReadMode::Canonical => (0, 0, None),
            ReadMode::Raw { min, time } if min > 0 => (min as usize, time as u64 * 100_000_000, None),
            ReadMode::Raw { time, .. } => (1, 0, Some(now() + time as u64 * 100_000_000)),
        };
        let mut done = 0usize;
        let mut chunk = [0u8; 2048];
        loop {
            // Held from peeking to consuming, not while waiting: a change of the
            // settings cannot move the bytes in between (`set_termios` takes it).
            let reader = self.rlock.lock();
            let (take, epoch, canonical) = {
                let inner = self.inner.lock();
                if inner.gen != open.gen {
                    return Ok(done as i64);
                }
                let canonical = inner.ld.mode() == ReadMode::Canonical;
                match inner.ld.take(sink.room().min(chunk.len())) {
                    Some(take) => {
                        inner.ld.peek(&mut chunk[..take.copy]);
                        (take, inner.epoch, canonical)
                    }
                    None => {
                        let seen = self.seen();
                        drop(inner);
                        drop(reader);
                        if deadline.is_some_and(|d| now() >= d) {
                            return Ok(done as i64);
                        }
                        if nonblock {
                            return if done > 0 { Ok(done as i64) } else { Err(EAGAIN) };
                        }
                        if let Err(e) = self.wait(seen, deadline.unwrap_or(0)) {
                            return if done > 0 { Ok(done as i64) } else { Err(e) };
                        }
                        continue;
                    }
                }
            };
            // To the program with no lock of the server's but `rlock`: a fault here may
            // wait for the pager, which never takes `rlock` (only programs' reads and
            // settings changes do), so no cycle can form.
            let put = sink.put(&chunk[..take.copy]);
            let k = match put {
                Ok(k) => k,
                Err(e) => return if done > 0 { Ok(done as i64) } else { Err(e) },
            };
            {
                let mut inner = self.inner.lock();
                if inner.epoch == epoch {
                    inner.ld.consume(if k == take.copy { take.consume } else { k });
                    self.changed(&mut inner, false, false);
                }
            }
            drop(reader);
            done += k;
            if k < take.copy || sink.room() == 0 || (canonical && take.line_end) {
                return Ok(done as i64);
            }
            if !canonical && done >= minimum {
                return Ok(done as i64);
            }
            if time > 0 {
                // VTIME between bytes, once one came.
                deadline = Some(now() + time);
            }
        }
    }

    /// write(2): output processing, then to the driver; waits while output is stopped
    /// or a pty's master has no room. With TOSTOP a background writer gets SIGTTOU.
    pub fn write(&self, open: &TtyOpen, mut src: Source, nonblock: bool) -> Result<i64, i64> {
        if self.hung_up(open) {
            return Err(EIO);
        }
        if self.inner.lock().ld.termios().l(TOSTOP) {
            self.job_check(SIGTTOU)?;
        }
        // One write at a time, for the whole write (Linux's atomic_write_lock): its
        // output processing and the device's write keep their order. Not a lock the
        // service thread takes (its echoes never wait, `console_echo`).
        let _turn = self.turn(TURN_WRITE, nonblock)?;
        let mut written = 0usize;
        while src.left() > 0 {
            // A signal ends the write between chunks (Linux's n_tty_write checks
            // signal_pending each pass, before processing): what went, or EINTR
            // (restarted) if nothing. Processed output always goes out.
            if files::signal_pending() {
                return if written > 0 { Ok(written as i64) } else { Err(EINTR) };
            }
            let chunk = match src.take(WRITE_CHUNK) {
                Ok(c) => c,
                Err(e) => return if written > 0 { Ok(written as i64) } else { Err(e) },
            };
            loop {
                // Output processed here goes out: the console's write waits for its
                // turn but no signal ends that wait (only death, after which nothing
                // restarts, or the loss of the device, which hangs the terminal up),
                // so the column bookkeeping moves once per byte that goes out, as
                // Linux's (the interruptible wait is this write's turn, taken before).
                let seen = {
                    let mut inner = self.inner.lock();
                    if inner.gen != open.gen {
                        return if written > 0 { Ok(written as i64) } else { Err(EIO) };
                    }
                    if Self::writable(&inner) {
                        let mut out = Vec::with_capacity(chunk.len() + chunk.len() / 4);
                        inner.ld.output(&chunk, &mut out);
                        match inner.pty.as_mut() {
                            Some(p) => {
                                p.out.extend(out);
                                self.changed(&mut inner, false, true);
                            }
                            None => {
                                drop(inner);
                                if let Err((went, e)) = crate::console::device_write(&out) {
                                    // Part of this chunk's output went out: the chunk
                                    // counts (the rest is lost with the device).
                                    let written = if went > 0 { written + chunk.len() } else { written };
                                    return if written > 0 { Ok(written as i64) } else { Err(e) };
                                }
                            }
                        }
                        break;
                    }
                    self.seen()
                };
                if nonblock {
                    return if written > 0 { Ok(written as i64) } else { Err(EAGAIN) };
                }
                if let Err(e) = self.wait(seen, 0) {
                    return if written > 0 { Ok(written as i64) } else { Err(e) };
                }
            }
            written += chunk.len();
        }
        Ok(written as i64)
    }

    /// The last descriptor of an open file description went.
    pub fn closed(&self, open: &TtyOpen) {
        let mut inner = self.inner.lock();
        inner.opens.remove(&open.id);
        if let Some(p) = inner.pty.as_mut() {
            p.slaves = p.slaves.saturating_sub(1);
            if p.slaves == 0 {
                p.slave_closed = true;
            }
        }
        self.changed(&mut inner, false, false);
    }

    /// Hangs the terminal up (Linux's `__tty_hangup`): its open file descriptions so far
    /// are hung up, its input goes, its settings are the driver's again, it controls no
    /// session; the session's leader gets SIGHUP and SIGCONT, and with `exit_session`
    /// (the leader ended) the foreground group SIGHUP.
    pub fn hangup(&self, exit_session: bool) {
        let (leader, fg) = {
            let mut inner = self.inner.lock();
            inner.gen = inner.gen.wrapping_add(1);
            inner.epoch += 1;
            inner.held_echo.clear();
            inner.ld = Ldisc::new(self.init);
            // What was on its way to a pty's master goes too (Linux flushes the
            // driver's buffer).
            if let Some(p) = inner.pty.as_mut() {
                p.out.clear();
                p.out_epoch += 1;
            }
            let leader = inner.session.take();
            let fg = inner.pgrp.take();
            self.changed(&mut inner, true, true);
            (leader, fg)
        };
        // The leader, if it still leads that session (the kernel checks it on the
        // process it signals).
        if let Some(sid) = leader {
            signal(SIGNAL_LEADER, sid, SIGHUP);
            signal(SIGNAL_LEADER, sid, SIGCONT);
        }
        if exit_session {
            if let Some(fg) = fg {
                signal(SIGNAL_PGRP, fg, SIGHUP);
            }
        }
    }

    /// Writes `c` to the device as a control character of flow control (TCIOFF, TCION).
    fn send_char(&self, c: u8) {
        if c == DISABLED {
            return;
        }
        let out = {
            let mut inner = self.inner.lock();
            let out = match inner.pty.as_mut() {
                Some(p) => {
                    p.out.push_back(c);
                    None
                }
                None => Some([c]),
            };
            self.changed(&mut inner, false, true);
            out
        };
        if let Some(out) = out {
            self.console_write(&out);
        }
    }

    /// Discards input (`input`) and the output the driver holds (`output`: a pty's
    /// unread output).
    fn flush(&self, input: bool, output: bool) {
        let mut inner = self.inner.lock();
        if input {
            inner.ld.flush_input();
            inner.epoch += 1;
        }
        if output {
            inner.held_echo.clear();
            if let Some(p) = inner.pty.as_mut() {
                p.out.clear();
                p.out_epoch += 1;
            }
        }
        self.changed(&mut inner, false, false);
    }

    /// New settings (TCSETS*, TCSETS2*) from `size` bytes at `arg`.
    fn set_termios(&self, arg: u64, size: usize, flush: bool) -> Result<i64, i64> {
        self.job_check(SIGTTOU)?;
        let mut bytes = [0u8; TERMIOS2_SIZE];
        usercopy::from_program(arg, &mut bytes[..size]).map_err(|_| EFAULT)?;
        // No reader is between peeking and consuming meanwhile (`rlock`, held for the
        // change only: the held echo goes out after it).
        let out = {
            let _reader = self.rlock.lock();
            let mut inner = self.inner.lock();
            let new = inner.ld.termios().with_bytes(&bytes[..size]);
            if flush {
                inner.ld.flush_input();
                inner.epoch += 1;
            }
            let was_stopped = inner.ld.stopped();
            inner.ld.set_termios(new);
            let mut out = None;
            if was_stopped && !inner.ld.stopped() {
                let held = core::mem::take(&mut inner.held_echo);
                out = self.place_echo(&mut inner, held);
            }
            self.changed(&mut inner, false, true);
            out
        };
        if let Some(out) = out {
            self.console_echo(&out);
        }
        Ok(0)
    }

    /// Starts or stops output (tcflow's TCOOFF and TCOON).
    fn flow(&self, stop: bool) {
        let out = {
            let mut inner = self.inner.lock();
            let was_stopped = inner.ld.stopped();
            if stop {
                inner.ld.stop(true);
            } else {
                inner.ld.start(true);
            }
            let mut out = None;
            if was_stopped && !inner.ld.stopped() {
                let held = core::mem::take(&mut inner.held_echo);
                out = self.place_echo(&mut inner, held);
            }
            self.changed(&mut inner, false, true);
            out
        };
        if let Some(out) = out {
            self.console_echo(&out);
        }
    }

    /// ioctl(2) on the terminal: through one of its open file descriptions (`open`), or
    /// through its pty's master (None: the job control checks and the hangup do not
    /// apply to it, as Linux's `real_tty`).
    pub fn ioctl(&self, open: Option<&TtyOpen>, request: u64, arg: u64) -> Result<i64, i64> {
        let request = request as u32;
        let master = open.is_none();
        if let Some(open) = open {
            if self.hung_up(open) {
                return Err(if request == TIOCSPGRP { ENOTTY } else { EIO });
            }
        }
        match request {
            TCGETS | TCGETS2 => {
                let size = if request == TCGETS { TERMIOS_SIZE } else { TERMIOS2_SIZE };
                let bytes = self.inner.lock().ld.termios().to_bytes();
                usercopy::to_program(arg, &bytes[..size]).map_err(|_| EFAULT)?;
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF => self.set_termios(arg, TERMIOS_SIZE, request == TCSETSF),
            TCSETS2 | TCSETSW2 | TCSETSF2 => self.set_termios(arg, TERMIOS2_SIZE, request == TCSETSF2),
            // No breaks to send, nothing to drain: the console's output is written when
            // the write returns, a pty's is the master's to read.
            TCSBRK | TCSBRKP | TIOCSBRK | TIOCCBRK => {
                self.job_check(SIGTTOU)?;
                Ok(0)
            }
            TCXONC => {
                self.job_check(SIGTTOU)?;
                let cc = self.inner.lock().ld.termios().cc;
                match arg {
                    0 => self.flow(true),
                    1 => self.flow(false),
                    2 => self.send_char(cc[VSTOP]),
                    3 => self.send_char(cc[VSTART]),
                    _ => return Err(EINVAL),
                }
                Ok(0)
            }
            TCFLSH => {
                self.job_check(SIGTTOU)?;
                match arg {
                    0 => self.flush(true, false),
                    1 => self.flush(false, true),
                    2 => self.flush(true, true),
                    _ => return Err(EINVAL),
                }
                Ok(0)
            }
            TIOCEXCL | TIOCNXCL => {
                self.inner.lock().exclusive = request == TIOCEXCL;
                Ok(0)
            }
            TIOCGEXCL => {
                let excl = self.inner.lock().exclusive as i32;
                usercopy::write(arg, &excl)?;
                Ok(0)
            }
            TIOCGWINSZ => {
                let ws = self.inner.lock().winsize;
                usercopy::write(arg, &ws)?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let ws: [u16; 4] = usercopy::read(arg)?;
                let fg = {
                    let mut inner = self.inner.lock();
                    if inner.winsize == ws {
                        None
                    } else {
                        inner.winsize = ws;
                        inner.pgrp
                    }
                };
                if let Some(fg) = fg {
                    signal(SIGNAL_PGRP, fg, SIGWINCH);
                }
                Ok(0)
            }
            TIOCGPGRP => {
                if !master && !self.is_ctty() {
                    return Err(ENOTTY);
                }
                let pgrp = self.inner.lock().pgrp.unwrap_or(0) as i32;
                usercopy::write(arg, &pgrp)?;
                Ok(0)
            }
            TIOCSPGRP => self.set_pgrp(arg),
            TIOCGSID => {
                if !master && !self.is_ctty() {
                    return Err(ENOTTY);
                }
                let sid = self.inner.lock().session.ok_or(ENOTTY)? as i32;
                usercopy::write(arg, &sid)?;
                Ok(0)
            }
            TIOCSCTTY => self.set_ctty(arg),
            TIOCNOTTY => {
                if master {
                    return Err(ENOTTY);
                }
                self.notty()
            }
            TIOCSTI => {
                // Everyone is root (CAP_SYS_ADMIN): any terminal.
                let b: u8 = usercopy::read(arg)?;
                self.input(&[b]);
                Ok(0)
            }
            FIONREAD => {
                let n = self.inner.lock().ld.available() as i32;
                usercopy::write(arg, &n)?;
                Ok(0)
            }
            TIOCOUTQ => {
                usercopy::write(arg, &0i32)?;
                Ok(0)
            }
            TIOCGETD => {
                usercopy::write(arg, &0i32)?;
                Ok(0)
            }
            TIOCSETD => {
                self.job_check(SIGTTOU)?;
                let ld: i32 = usercopy::read(arg)?;
                // Only N_TTY.
                if ld == 0 { Ok(0) } else { Err(EINVAL) }
            }
            TIOCVHANGUP => {
                self.hangup(false);
                Ok(0)
            }
            _ => Err(ENOTTY),
        }
    }

    /// TIOCSPGRP (Linux's `tiocspgrp`): the foreground group, one of the caller's session.
    fn set_pgrp(&self, arg: u64) -> Result<i64, i64> {
        match self.job_check(SIGTTOU) {
            Err(EIO) => return Err(ENOTTY),
            Err(e) => return Err(e),
            Ok(()) => {}
        }
        let me = ids(0, 0)?;
        if self.inner.lock().session != Some(me.sid) {
            return Err(ENOTTY);
        }
        let pgrp: i32 = usercopy::read(arg)?;
        if pgrp < 0 {
            return Err(EINVAL);
        }
        let target = ids(pgrp as u64, IDS_PGRP).map_err(|_| ESRCH)?;
        if target.sid != me.sid {
            return Err(EPERM);
        }
        let mut inner = self.inner.lock();
        if inner.session != Some(me.sid) {
            return Err(ENOTTY);
        }
        inner.pgrp = Some(pgrp as u64);
        Ok(0)
    }

    /// TIOCSCTTY (Linux's `tiocsctty`): the caller, a session leader without a
    /// controlling terminal, gets this one; one controlling another session is taken
    /// only with `arg` 1 (everyone is root).
    fn set_ctty(&self, arg: u64) -> Result<i64, i64> {
        let me = ids(0, 0)?;
        let leader = me.pid == me.sid;
        if leader && self.inner.lock().session == Some(me.sid) {
            return Ok(0);
        }
        if !leader || ctty_of(me.sid).is_some() {
            return Err(EPERM);
        }
        let mut inner = self.inner.lock();
        if inner.session.is_some() && arg != 1 {
            return Err(EPERM);
        }
        inner.session = Some(me.sid);
        inner.pgrp = Some(me.pgid);
        Ok(0)
    }

    /// TIOCNOTTY: the caller's session leader gives up its controlling terminal (Linux's
    /// `disassociate_ctty(0)`: the foreground group gets SIGHUP and SIGCONT). Another
    /// process of the session keeps it until the leader does (per-process controlling
    /// terminals come with the server's process records, R8; ADR 0007).
    fn notty(&self) -> Result<i64, i64> {
        let me = ids(0, 0)?;
        if self.inner.lock().session != Some(me.sid) {
            return Err(ENOTTY);
        }
        if me.pid != me.sid {
            return Ok(0);
        }
        let fg = {
            let mut inner = self.inner.lock();
            inner.session = None;
            inner.pgrp.take()
        };
        if let Some(fg) = fg {
            signal(SIGNAL_PGRP, fg, SIGHUP);
            signal(SIGNAL_PGRP, fg, SIGCONT);
        }
        Ok(0)
    }
}

/// The calls on an open file description of a terminal.
pub fn call(nr: u64, open: &Arc<TtyOpen>, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    use crate::files::{SYS_FSTAT, SYS_IOCTL, SYS_READ, SYS_READV, SYS_WRITE, SYS_WRITEV};
    let readable = flags & O_ACCMODE != files::O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    let nonblock = flags & O_NONBLOCK != 0;
    let tty = &open.tty;
    match nr {
        SYS_READ | SYS_READV if !readable => Err(files::EBADF),
        SYS_WRITE | SYS_WRITEV if !writable => Err(files::EBADF),
        SYS_READ => tty.read(open, Sink::program(&[(a1, a2)]), nonblock),
        SYS_WRITE => tty.write(open, Source::program(&[(a1, a2)]), nonblock),
        SYS_READV => {
            let vecs = files::iovecs(a1, a2)?;
            tty.read(open, Sink::program(&vecs), nonblock)
        }
        SYS_WRITEV => {
            let vecs = files::iovecs(a1, a2)?;
            tty.write(open, Source::program(&vecs), nonblock)
        }
        SYS_FSTAT => usercopy::to_program(a1, &open.origin.stat()?).map(|_| 0),
        SYS_IOCTL => tty.ioctl(Some(open), a1, a2),
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(files::ESPIPE),
        files::SYS_GETDENTS64 => Err(files::ENOTDIR),
        _ => Err(EINVAL),
    }
}
