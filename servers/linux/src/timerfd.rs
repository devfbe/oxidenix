//! timerfd (timerfd_create, timerfd_settime, timerfd_gettime): a timer whose expiries a
//! read takes as a count, a file of the server's (an open file description of its table's,
//! `files`), as eventfd is.
//!
//! The count is computed when it is asked for (from when the timer expires first and its
//! interval), so an expiry costs nothing until someone looks: a read that finds none
//! sleeps until the next expiry (a futex wait with that deadline on the timer's sequence
//! word, which a new setting advances). Pollers learn of an expiry from the instance's
//! timer thread (`timer::thread` calls `due`), which keeps every armed timerfd's next
//! expiry and reports each one (an edge for EPOLLET, as Linux's wakes per expiry).
//!
//! A timer on the wall clock set with `TFD_TIMER_ABSTIME` waits for a time of that clock
//! (setting the clock moves it); with `TFD_TIMER_CANCEL_ON_SET` too, a setting of the
//! clock by the instance (clock_settime, settimeofday) cancels it: reads fail with
//! ECANCELED until it is set again. A relative timer is a span of the monotonic clock.

use crate::files::{self, EFAULT, EINVAL};
use crate::heap::{self, TryGrow};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use restricted::*;

const EAGAIN: i64 = 11;
const EINTR: i64 = 4;
const ECANCELED: i64 = 125;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const POLLIN: i16 = 0x1;

const TFD_TIMER_ABSTIME: u64 = 1;
const TFD_TIMER_CANCEL_ON_SET: u64 = 2;
/// `_IOW('T', 0, u64)`: sets the count of expiries (checkpoint/restore).
const TFD_IOC_SET_TICKS: u64 = 0x4008_5400;

pub const SYS_TIMERFD_CREATE: u64 = 283;
pub const SYS_TIMERFD_SETTIME: u64 = 286;
pub const SYS_TIMERFD_GETTIME: u64 = 287;

/// How often the instance set the wall clock (`clock_set`): a cancelable timer set
/// before the last change is canceled.
static CLOCK_SETS: AtomicU64 = AtomicU64::new(0);

/// The armed and disarmed timerfds of the instance, for the timer thread (each from its
/// creation until its file closes).
static TIMERS: Mutex<Vec<Arc<Timer>>> = Mutex::new(Vec::new());

fn mono() -> u64 {
    syscall(SYS_CLOCK_READ, [CLOCK_MONO, 0, 0, 0, 0, 0]).max(0) as u64
}

fn wall() -> u64 {
    syscall(SYS_CLOCK_READ, [CLOCK_WALL, 0, 0, 0, 0, 0]).max(0) as u64
}

/// A timer's setting.
#[derive(Clone, Copy, Default)]
struct Setting {
    /// Its first expiry not read yet (0: disarmed), on the wall clock's scale for an
    /// absolute timer of the wall clock (`wall`), else the monotonic clock's.
    at: u64,
    wall: bool,
    every: u64,
    /// Expiries counted besides those from `at` on (`TFD_IOC_SET_TICKS`).
    ticks: u64,
    /// Canceled when the wall clock is set (`TFD_TIMER_CANCEL_ON_SET`): `CLOCK_SETS` when
    /// it was set.
    cancel: Option<u64>,
    /// The latest expiry the timer thread reported to pollers.
    reported: u64,
    /// The flags of the timerfd_settime that set it (fdinfo's `settime flags`).
    flags: u32,
}

impl Setting {
    /// Its clock's time now.
    fn now(&self) -> u64 {
        if self.wall { wall() } else { mono() }
    }

    fn canceled(&self) -> bool {
        self.cancel.is_some_and(|set| set != CLOCK_SETS.load(Ordering::Acquire))
    }

    /// How many expiries there are to read at `now`.
    fn expired(&self, now: u64) -> u64 {
        let from_at = match (self.at, self.every) {
            (0, _) => 0,
            (at, _) if now < at => 0,
            (_, 0) => 1,
            (at, every) => 1 + (now - at) / every,
        };
        self.ticks.saturating_add(from_at)
    }

    /// The latest expiry at or before `now` (0: none).
    fn last(&self, now: u64) -> u64 {
        match (self.at, self.every) {
            (0, _) => 0,
            (at, _) if now < at => 0,
            (at, 0) => at,
            (at, every) => at + (now - at) / every * every,
        }
    }

    /// The first expiry after `now` (0: none). Saturating: a program's start and interval
    /// may be as large as a timespec allows, and one beyond the clock's range never comes.
    fn next(&self, now: u64) -> u64 {
        match (self.at, self.every) {
            (0, _) => 0,
            (at, _) if now < at => at,
            (_, 0) => 0,
            (at, every) => at.saturating_add(((now - at) / every + 1).saturating_mul(every)),
        }
    }

    /// The expiries up to `now` are read: the timer goes on from the next one.
    fn consume(&mut self, now: u64) {
        self.ticks = 0;
        self.at = self.next(now);
        if self.at == 0 {
            self.every = 0;
        }
    }

    /// (time left, interval) as timerfd_gettime reports them.
    fn left(&self, now: u64) -> (u64, u64) {
        match self.next(now) {
            0 => (0, self.every),
            next => (next - now, self.every),
        }
    }
}

/// The timer behind a timerfd's open file description (`TimerFd`), shared with the timer
/// thread's list.
struct Timer {
    id: u64,
    /// A clock of the wall's (CLOCK_REALTIME, CLOCK_REALTIME_ALARM).
    realtime: bool,
    /// Its clock as the program named it (fdinfo's `clockid`).
    clockid: i32,
    setting: Mutex<Setting>,
    /// Advances when the setting changes (or the clock is set): readers look again.
    seq: AtomicU32,
}

impl Timer {
    fn readiness(s: &Setting) -> i16 {
        if s.canceled() || s.expired(s.now()) > 0 { POLLIN } else { 0 }
    }

    /// After a change (setting locked): readers and pollers look again.
    fn changed(&self, s: &Setting) {
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        files::ready(self.id, Self::readiness(s));
    }
}

/// A timerfd's file.
pub struct TimerFd {
    timer: Arc<Timer>,
}

impl TimerFd {
    pub fn ready(&self) -> i16 {
        Timer::readiness(&self.timer.setting.lock())
    }

    /// For /proc/<pid>/fdinfo: its clock id, the expiries not read yet, the flags of its
    /// setting, the time left and the interval (nanoseconds).
    pub fn fdinfo(&self) -> (i32, u64, u32, u64, u64) {
        let s = self.timer.setting.lock();
        let now = s.now();
        let (left, every) = s.left(now);
        (self.timer.clockid, s.expired(now), s.flags, left, every)
    }

    /// Takes the count of expiries into 8 bytes at `buf`; waits while there is none.
    pub fn read(&self, buf: u64, len: u64, nonblock: bool) -> Result<i64, i64> {
        if len < 8 {
            return Err(EINVAL);
        }
        let timer = &self.timer;
        loop {
            let (seen, deadline) = {
                let mut s = timer.setting.lock();
                if s.canceled() {
                    return Err(ECANCELED);
                }
                let now = s.now();
                let n = s.expired(now);
                if n > 0 {
                    s.consume(now);
                    drop(s);
                    // Copied out after the lock (a fault may wait for a page): taken even
                    // if the copy fails, as Linux's.
                    usercopy::write(buf, &n)?;
                    return Ok(8);
                }
                // The monotonic time of its next expiry (0: none, until a new setting).
                let deadline = match s.next(now) {
                    0 => 0,
                    next if s.wall => mono().saturating_add(next - now),
                    next => next,
                };
                (timer.seq.load(Ordering::Acquire), deadline)
            };
            if nonblock {
                return Err(EAGAIN);
            }
            let word = &timer.seq as *const AtomicU32 as u64;
            if syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, deadline, FUTEX_INTERRUPTIBLE, 0, 0]) == -EINTR {
                return Err(EINTR);
            }
        }
    }

    /// TFD_IOC_SET_TICKS: the count becomes the u64 at `arg` (nonzero).
    fn set_ticks(&self, arg: u64) -> Result<i64, i64> {
        let ticks: u64 = usercopy::read(arg)?;
        if ticks == 0 {
            return Err(EINVAL);
        }
        let mut s = self.timer.setting.lock();
        let now = s.now();
        s.consume(now);
        s.ticks = ticks;
        self.timer.changed(&s);
        Ok(0)
    }

    pub fn stat(&self) -> [u8; 144] {
        // An anonymous inode (no file type), as on Linux.
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.timer.id.to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&0o600u32.to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st
    }
}

impl Drop for TimerFd {
    /// Its file closed: the timer thread forgets it.
    fn drop(&mut self) {
        let mut list = TIMERS.lock();
        if let Some(at) = list.iter().position(|t| Arc::ptr_eq(t, &self.timer)) {
            list.swap_remove(at);
        }
    }
}

/// The calls on a timerfd's descriptor.
pub fn call(nr: u64, f: &TimerFd, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    let nonblock = flags & files::O_NONBLOCK != 0;
    match nr {
        files::SYS_READ => f.read(a1, a2, nonblock),
        // One count per buffer, as reads one after the other.
        files::SYS_READV => {
            let mut done = 0;
            for (base, len) in files::iovecs(a1, a2)? {
                match f.read(base, len, nonblock) {
                    Ok(n) => done += n,
                    Err(e) if done == 0 => return Err(e),
                    Err(_) => break,
                }
            }
            Ok(done)
        }
        files::SYS_FSTAT => usercopy::to_program(a1, &f.stat()).map(|_| 0).map_err(|_| EFAULT),
        files::SYS_IOCTL if a1 == TFD_IOC_SET_TICKS => f.set_ticks(a2),
        files::SYS_IOCTL => Err(ENOTTY),
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(ESPIPE),
        _ => Err(EINVAL),
    }
}

/// The result of a timerfd call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let result = match s.rax {
        SYS_TIMERFD_CREATE => create(s.rdi, s.rsi),
        SYS_TIMERFD_SETTIME => settime(s.rdi, s.rsi, s.rdx, s.r10),
        SYS_TIMERFD_GETTIME => gettime(s.rdi, s.rsi),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// timerfd_create(clock, flags): a disarmed timer of the wall clock (CLOCK_REALTIME and
/// its alarm clock) or the monotonic one (CLOCK_MONOTONIC, CLOCK_BOOTTIME and its alarm
/// clock: nothing suspends).
fn create(clock: u64, flags: u64) -> Result<i64, i64> {
    let realtime = match clock as i32 {
        0 | 8 => true,
        1 | 7 | 9 => false,
        _ => return Err(EINVAL),
    };
    if flags & !((files::O_NONBLOCK | files::O_CLOEXEC) as u64) != 0 {
        return Err(EINVAL);
    }
    let timer = heap::try_arc(Timer { id: files::new_id(), realtime, clockid: clock as i32, setting: Mutex::new(Setting::default()), seq: AtomicU32::new(0) })?;
    let file = heap::try_arc(TimerFd { timer: timer.clone() })?;
    {
        // Known to the timer thread before it can be set; forgotten again if no
        // descriptor can be had (the file goes then).
        let mut list = TIMERS.lock();
        list.try_grow(1)?;
        list.push(timer.clone());
    }
    files::install(timer.id, files::File::TimerFd(file), files::O_RDWR | (flags as u32 & (files::O_NONBLOCK | files::O_CLOEXEC)))
}

/// The timerfd behind descriptor `fd` (EINVAL for another file).
fn timer_of(fd: u64) -> Result<Arc<TimerFd>, i64> {
    match &files::lookup(fd)?.file {
        files::File::TimerFd(t) => Ok(t.clone()),
        _ => Err(EINVAL),
    }
}

/// timerfd_settime(fd, flags, new, old): arms the timer for the time in `new` (relative,
/// or with TFD_TIMER_ABSTIME a time of its clock; 0: disarms) and its interval; the old
/// setting at `old`. The expiries not read yet go.
fn settime(fd: u64, flags: u64, new: u64, old: u64) -> Result<i64, i64> {
    let f = timer_of(fd)?;
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
        return Err(EINVAL);
    }
    let (interval, value) = crate::timer::read_itimerspec(new)?;
    let absolute = flags & TFD_TIMER_ABSTIME != 0;
    let timer = &f.timer;
    let was = {
        let mut s = timer.setting.lock();
        let was = s.left(s.now());
        let wall = value != 0 && absolute && timer.realtime;
        let at = match value {
            0 => 0,
            _ if absolute => value,
            _ => mono().saturating_add(value),
        };
        let cancel = (wall && flags & TFD_TIMER_CANCEL_ON_SET != 0).then(|| CLOCK_SETS.load(Ordering::Acquire));
        *s = Setting { at, wall, every: if at == 0 { 0 } else { interval }, ticks: 0, cancel, reported: 0, flags: flags as u32 };
        timer.changed(&s);
        was
    };
    crate::timer::changed();
    crate::timer::write_itimerspec(old, was)?;
    Ok(0)
}

fn gettime(fd: u64, cur: u64) -> Result<i64, i64> {
    let f = timer_of(fd)?;
    let setting = {
        let s = f.timer.setting.lock();
        s.left(s.now())
    };
    crate::timer::write_itimerspec(cur, setting)?;
    Ok(0)
}

/// The instance set the wall clock: cancelable timers are canceled, and absolute timers of
/// the wall clock wait for its new time (`due` wakes their readers).
pub fn clock_set() {
    CLOCK_SETS.fetch_add(1, Ordering::AcqRel);
    for timer in TIMERS.lock().iter() {
        let s = timer.setting.lock();
        if s.wall {
            timer.changed(&s);
        }
    }
}

/// For the timer thread: reports every expiry since the last look to the timers' pollers
/// (an event for each, also for EPOLLET), and returns the monotonic time of the next one
/// (0: none).
pub fn due() -> u64 {
    let (mono_now, wall_now) = (mono(), wall());
    let mut next = 0;
    for timer in TIMERS.lock().iter() {
        let mut s = timer.setting.lock();
        if s.at == 0 {
            continue;
        }
        let now = if s.wall { wall_now } else { mono_now };
        let last = s.last(now);
        if last != 0 && last != s.reported {
            s.reported = last;
            files::ready(timer.id, POLLIN);
        }
        let deadline = match s.next(now) {
            0 => 0,
            n if s.wall => mono_now.saturating_add(n - now),
            n => n,
        };
        if deadline != 0 && (next == 0 || deadline < next) {
            next = deadline;
        }
    }
    next
}
