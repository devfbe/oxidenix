//! The descriptor table (phase R6e, docs/design/linux-server.md "The
//! descriptor table"): per process, the server's. A descriptor is a slot
//! with a reference to an open file description (`files::FileRef`) and the
//! close-on-exec bit. A new descriptor is below the caller's process's soft RLIMIT_NOFILE
//! (`ids::soft`: a table may be shared by processes with limits of their own, as on Linux).
//! The calls on descriptors as such are here: close, close_range, dup, dup2, dup3,
//! fcntl's F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_SETFD, F_GETFL and
//! F_SETFL, the ioctls FIONBIO, FIOCLEX and FIONCLEX. F_SETFL changes O_APPEND and O_NONBLOCK; of the rest of
//! Linux's SETFL_MASK it ignores O_DIRECT (kept as opened), O_NOATIME (no
//! access times are kept apart) and O_ASYNC (no SIGIO), as the kernel did.
//!
//! **Which table.** Each thread of a program has one (`process::Thread::files`), shared by
//! the threads and processes made with CLONE_FILES; the thread finds its own without a lock
//! (`local::Local::files`, valid while it runs: only the thread itself replaces it). The
//! process model (`process`, `exec`) makes and ends tables: `FilesContext::fork` is the table
//! a new process gets (a copy: every description shared, close-on-exec kept),
//! `FilesContext::for_exec` the one a process that executes a program gets (the copy without
//! the close-on-exec descriptors, made after the point of no return; the old table goes there
//! and then, before the new program runs, so its close-on-exec descriptors are closed as on
//! Linux), and the end of a table (its last `Arc`) closes its descriptors: on the exiting
//! thread (`process::exit`), or, for a thread the kernel ended without the server, on the
//! worker (`end_later`), which then ends the process if that was its last thread.
//!
//! Locking: one lock per table, never held while program memory is copied
//! or a description is let go of (closing a file may take long, and may
//! take locks of its own): references taken out under the lock are
//! dropped after it.

use crate::files::{self, FileRef, O_APPEND, O_CLOEXEC, O_NONBLOCK};
use crate::sync::Mutex;
use crate::syscall;
use crate::local;
use crate::usercopy;
use alloc::sync::Arc;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const EBADF: i64 = 9;
const ENOMEM: i64 = 12;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;

/// A descriptor.
#[derive(Clone)]
struct Slot {
    file: FileRef,
    cloexec: bool,
}

struct Table {
    slots: Vec<Option<Slot>>,
    /// No slot below this one is free (Linux's next_fd).
    free_from: usize,
}

/// A descriptor table and the processes' record of it.
pub struct FilesContext {
    table: Mutex<Table>,
}

impl Table {
    /// The lowest free descriptor at or above `min`, below the caller's soft limit.
    fn free_slot(&self, min: usize) -> Result<usize, i64> {
        let from = min.max(self.free_from);
        let found = (from..self.slots.len()).find(|&i| self.slots[i].is_none()).unwrap_or(self.slots.len().max(from));
        if found as u64 >= nofile() {
            return Err(EMFILE);
        }
        Ok(found)
    }

    /// Makes room for descriptor `fd`.
    fn reserve(&mut self, fd: usize) -> Result<(), i64> {
        if fd >= self.slots.len() {
            let more = fd + 1 - self.slots.len();
            self.slots.try_reserve(more).map_err(|_| ENOMEM)?;
            self.slots.resize(fd + 1, None);
        }
        Ok(())
    }

    fn put(&mut self, fd: usize, slot: Slot) -> Option<Slot> {
        let old = self.slots[fd].replace(slot);
        if fd == self.free_from {
            self.free_from = fd + 1;
        }
        old
    }

    fn take(&mut self, fd: usize) -> Option<Slot> {
        let old = self.slots.get_mut(fd)?.take();
        if old.is_some() && fd < self.free_from {
            self.free_from = fd;
        }
        old
    }

    fn get(&self, fd: u64) -> Option<&Slot> {
        self.slots.get(usize::try_from(fd).ok()?)?.as_ref()
    }
}

impl FilesContext {
    fn new(table: Table) -> Arc<FilesContext> {
        Arc::new(FilesContext { table: Mutex::new(table) })
    }

    /// A table without descriptors (the tree's first process starts with one).
    pub fn empty() -> Arc<FilesContext> {
        FilesContext::new(Table { slots: Vec::new(), free_from: 0 })
    }

    /// The description behind descriptor `fd` (EBADF).
    pub fn get(&self, fd: u64) -> Result<FileRef, i64> {
        self.table.lock().get(fd).map(|s| s.file.clone()).ok_or(EBADF)
    }

    /// A descriptor (the lowest free one at or above `min`) for `file`.
    pub fn install(&self, file: FileRef, cloexec: bool, min: u64) -> Result<i64, i64> {
        let mut t = self.table.lock();
        let fd = t.free_slot(min.min(usize::MAX as u64) as usize)?;
        t.reserve(fd)?;
        t.put(fd, Slot { file, cloexec });
        Ok(fd as i64)
    }

    /// A descriptor (the lowest free one) for the description `make` makes
    /// once there is room for it (nothing is made without one).
    pub fn install_new(&self, make: impl FnOnce() -> FileRef, cloexec: bool) -> Result<i64, i64> {
        let mut t = self.table.lock();
        let fd = t.free_slot(0)?;
        t.reserve(fd)?;
        t.put(fd, Slot { file: make(), cloexec });
        Ok(fd as i64)
    }

    /// Takes descriptor `fd` out (for the caller to let go of after the lock).
    pub fn take(&self, fd: u64) -> Option<FileRef> {
        let fd = usize::try_from(fd).ok()?;
        self.table.lock().take(fd).map(|s| s.file)
    }

    /// The table a new process gets (fork, vfork, clone without
    /// CLONE_FILES): the same descriptions, close-on-exec kept.
    pub fn fork(&self) -> Result<Arc<FilesContext>, i64> {
        self.copy(false)
    }

    /// The table a process that executes a program gets: its descriptors
    /// without the close-on-exec ones (and their slots free).
    pub fn for_exec(&self) -> Result<Arc<FilesContext>, i64> {
        self.copy(true)
    }

    fn copy(&self, exec: bool) -> Result<Arc<FilesContext>, i64> {
        // Room is reserved outside the lock, for the table as it was; if it
        // grew meanwhile, again.
        let mut slots: Vec<Option<Slot>> = Vec::new();
        loop {
            let len = self.table.lock().slots.len();
            slots.try_reserve_exact(len).map_err(|_| ENOMEM)?;
            let t = self.table.lock();
            if t.slots.len() <= slots.capacity() {
                slots.extend(t.slots.iter().map(|s| s.as_ref().filter(|s| !(exec && s.cloexec)).cloned()));
                let free_from = if exec { slots.iter().position(|s| s.is_none()).unwrap_or(slots.len()) } else { t.free_from };
                return Ok(FilesContext::new(Table { slots, free_from }));
            }
        }
    }
}

/// The calling thread's descriptor table (an empty one for a thread without one: a service
/// thread, which serves no program).
pub fn current() -> Arc<FilesContext> {
    let ptr = local::get().files.load(Ordering::Acquire);
    if ptr.is_null() {
        return FilesContext::empty();
    }
    // The thread's record keeps it alive while the thread runs (see the module comment);
    // this reference is the caller's.
    unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
}

/// Tables of threads the kernel ended, for the worker (`end_later`), in the order they came,
/// each with its process (whose end waits for it) and whether it was handed over within its
/// thread's reservation. Room for one per program thread is reserved when the thread is made
/// (`reserve_end`), so the service thread never allocates (nor closes anything) to hand one
/// over.
struct Ended {
    items: VecDeque<Handed>,
    /// Program threads that may still hand a table over within their reservation (live
    /// ones, and those whose handed table the worker has not taken yet).
    reserved: usize,
    /// Tables handed over without a reservation (room found when they came) still queued.
    extra: usize,
}

struct Handed {
    files: Arc<FilesContext>,
    pid: u32,
    reserved: bool,
}

impl Ended {
    /// Room for every table that may be queued: `items`' capacity at least
    /// `reserved + extra`.
    fn ensure(&mut self) -> Result<(), i64> {
        let need = (self.reserved + self.extra).saturating_sub(self.items.len());
        self.items.try_reserve(need).map_err(|_| ENOMEM)
    }
}

static ENDED: Mutex<Ended> = Mutex::new(Ended { items: VecDeque::new(), reserved: 0, extra: 0 });

/// Room for a new program thread's table at its end (ENOMEM without it).
pub fn reserve_end() -> Result<(), i64> {
    let mut e = ENDED.lock();
    e.reserved += 1;
    if let Err(err) = e.ensure() {
        e.reserved -= 1;
        return Err(err);
    }
    Ok(())
}

/// A thread's reservation goes unused (a clone that failed, a thread that let go of its
/// table itself, a reserved hand-over the worker took).
pub fn unreserve_end() {
    let mut e = ENDED.lock();
    e.reserved = e.reserved.saturating_sub(1);
}

/// A thread the kernel ended still had its table (`process::thread_ended`, on the service
/// thread): the worker lets go of it, which closes its descriptors with its last reference
/// (closing a socket takes its locks, which the service thread must never wait for), and
/// then tells process `pid` (`process::end_deferred`): its parent learns of the end only
/// after its descriptors closed (Linux's exit_files before exit_notify). `reserved`: the
/// thread's reservation covers it; without one (a first thread whose reservation failed)
/// room is found now, or the table is kept for good rather than closed here. False if it was
/// not handed over (nothing then waits for it). Called with the process table's lock held
/// (lock order: `process::PROCS`, then `ENDED`).
pub fn end_later(files: Arc<FilesContext>, pid: u32, reserved: bool) -> bool {
    let mut e = ENDED.lock();
    if !reserved {
        e.extra += 1;
        if e.ensure().is_err() {
            e.extra -= 1;
            drop(e);
            let msg = "[linux] no room to hand a descriptor table to the worker: kept\n";
            syscall(SYS_SERVER_LOG, [msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0, 0]);
            core::mem::forget(files);
            return false;
        }
    }
    // (Within the room `ensure` keeps: no allocation.)
    e.items.push_back(Handed { files, pid, reserved });
    drop(e);
    HANDED.fetch_add(1, Ordering::AcqRel);
    crate::scm::request();
    true
}

/// Tables handed to the worker, and let go of by it (a futex word).
static HANDED: AtomicU32 = AtomicU32::new(0);
static RELEASED: AtomicU32 = AtomicU32::new(0);

/// The worker: lets go of the tables handed to it, one by one in the order they came (the
/// queue keeps its room), and tells their processes. One table's closing never holds up the
/// ones after it for long: on the worker (a service thread) nothing a description's close
/// does waits for another party (an internet socket closes through the net thread, a /data
/// inode goes at the next `datafs::reap`, a socket's messages in flight go to the
/// collector), so the order costs no process's end more than the closing work before it.
pub fn release_ended() {
    let mut n = 0;
    loop {
        let item = ENDED.lock().items.pop_front();
        let Some(Handed { files, pid, reserved }) = item else { break };
        drop(files);
        {
            let mut e = ENDED.lock();
            if reserved {
                e.reserved = e.reserved.saturating_sub(1);
            } else {
                e.extra = e.extra.saturating_sub(1);
            }
        }
        crate::process::end_deferred(pid);
        n += 1;
    }
    if n > 0 {
        RELEASED.fetch_add(n, Ordering::AcqRel);
        syscall(SYS_SERVER_FUTEX_WAKE, [&RELEASED as *const AtomicU32 as u64, u32::MAX as u64, 0, 0, 0, 0]);
    }
}

/// The instance's last program is gone (`EVENT_CLOSING`): the service
/// thread waits until the worker let go of every table handed to it, so
/// that what their sockets still had to send reaches netd first.
pub fn settle() {
    // At most 5 s (as `netclient::settle`): a worker stuck on a socket's
    // lock must not keep the instance from ending.
    let deadline = crate::ringclient::now() + 5_000_000_000;
    loop {
        let done = RELEASED.load(Ordering::Acquire);
        if done == HANDED.load(Ordering::Acquire) || crate::ringclient::now() >= deadline {
            return;
        }
        syscall(SYS_SERVER_FUTEX_WAIT, [&RELEASED as *const AtomicU32 as u64, done as u64, deadline, 0, 0, 0]);
    }
}

const SYS_CLOSE: u64 = 3;
const SYS_IOCTL: u64 = 16;
const SYS_DUP: u64 = 32;
const SYS_DUP2: u64 = 33;
const SYS_FCNTL: u64 = 72;
const SYS_DUP3: u64 = 292;
const SYS_CLOSE_RANGE: u64 = 436;

const FIONBIO: u64 = 0x5421;
const FIONCLEX: u64 = 0x5450;
const FIOCLEX: u64 = 0x5451;

const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;
const FD_CLOEXEC: u64 = 1;

/// The result of a call on descriptors as such in `s`, or None.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2) = (s.rdi, s.rsi, s.rdx);
    let result = match s.rax {
        SYS_CLOSE => close(a0),
        SYS_DUP => dup(a0),
        SYS_DUP2 => dup3(a0, a1, 0, true),
        SYS_DUP3 => dup3(a0, a1, a2, false),
        SYS_FCNTL => fcntl(a0, a1, a2),
        SYS_CLOSE_RANGE => close_range(a0 as u32 as u64, a1 as u32 as u64, a2),
        SYS_IOCTL if matches!(a1 as u32 as u64, FIONBIO | FIOCLEX | FIONCLEX) => ioctl(a0, a1 as u32 as u64, a2),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// close(fd): the descriptor goes, its description with its last
/// reference. As on Linux, the descriptor is gone even if closing fails.
fn close(fd: u64) -> Result<i64, i64> {
    let gone = current().take(fd).ok_or(EBADF)?;
    drop(gone);
    Ok(0)
}

/// close_range(first, last, flags): every open descriptor in the range
/// closes, or (CLOSE_RANGE_CLOEXEC) gets close-on-exec; with
/// CLOSE_RANGE_UNSHARE the caller gets a table of its own first.
fn close_range(first: u64, last: u64, flags: u64) -> Result<i64, i64> {
    const CLOSE_RANGE_UNSHARE: u64 = 2;
    const CLOSE_RANGE_CLOEXEC: u64 = 4;
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last {
        return Err(EINVAL);
    }
    let mut context = current();
    if flags & CLOSE_RANGE_UNSHARE != 0 && Arc::strong_count(&context) > 2 {
        // (Shared: more than the thread's record and this reference.)
        let own = context.fork()?;
        let old = crate::process::set_files(own.clone());
        drop(old);
        context = own;
    }
    let mut gone = Vec::new();
    {
        let mut t = context.table.lock();
        let end = (last as usize).min(t.slots.len().saturating_sub(1));
        for fd in first as usize..=end {
            if fd >= t.slots.len() {
                break;
            }
            if flags & CLOSE_RANGE_CLOEXEC != 0 {
                if let Some(s) = t.slots[fd].as_mut() {
                    s.cloexec = true;
                }
            } else if let Some(s) = t.take(fd) {
                // Closed after the lock; without room to keep it, now (it
                // has been taken out, which is what counts for the table).
                if gone.try_reserve(1).is_ok() {
                    gone.push(s.file);
                } else {
                    drop(t);
                    drop(s);
                    t = context.table.lock();
                }
            }
        }
    }
    drop(gone);
    Ok(0)
}

fn dup(fd: u64) -> Result<i64, i64> {
    let context = current();
    let file = context.get(fd)?;
    context.install(file, false, 0)
}

/// dup2 (`dup2`: the same descriptor is fine) and dup3 (O_CLOEXEC only).
fn dup3(old: u64, new: u64, flags: u64, dup2: bool) -> Result<i64, i64> {
    // dup3's own checks come first (Linux's ksys_dup3).
    if !dup2 && (flags & !(O_CLOEXEC as u64) != 0 || old == new) {
        return Err(EINVAL);
    }
    let context = current();
    let file = context.get(old)?;
    if old == new {
        return Ok(new as i64);
    }
    let replaced = {
        if new >= nofile() {
            return Err(EBADF);
        }
        let mut t = context.table.lock();
        t.reserve(new as usize)?;
        t.put(new as usize, Slot { file, cloexec: flags & O_CLOEXEC as u64 != 0 })
    };
    // Closed only here, after the table's lock.
    drop(replaced);
    Ok(new as i64)
}

fn fcntl(fd: u64, cmd: u64, arg: u64) -> Result<i64, i64> {
    let context = current();
    let file = context.get(fd)?;
    // An O_PATH descriptor takes only these (Linux's fdget_raw in fcntl).
    if file.is_path() && !matches!(cmd, F_DUPFD | F_DUPFD_CLOEXEC | F_GETFD | F_SETFD | F_GETFL) {
        return Err(EBADF);
    }
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            if arg >= nofile() {
                return Err(EINVAL);
            }
            context.install(file, cmd == F_DUPFD_CLOEXEC, arg)
        }
        F_GETFD => {
            let t = context.table.lock();
            Ok(if t.get(fd).ok_or(EBADF)?.cloexec { FD_CLOEXEC as i64 } else { 0 })
        }
        F_SETFD => {
            let mut t = context.table.lock();
            let slot = t.slots.get_mut(fd as usize).and_then(|s| s.as_mut()).ok_or(EBADF)?;
            slot.cloexec = arg & FD_CLOEXEC != 0;
            Ok(0)
        }
        F_GETFL => Ok(file.flags() as i64),
        F_SETFL => {
            // O_APPEND and O_NONBLOCK change (Linux's SETFL_MASK has
            // O_DIRECT and O_NOATIME too: O_DIRECT is kept as opened here,
            // O_NOATIME means nothing without access times kept apart).
            file.set_flags(O_APPEND | O_NONBLOCK, arg as u32);
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// The requests on the descriptor rather than the file, which every
/// descriptor takes (Linux's do_vfs_ioctl): libuv makes pipes and sockets
/// non-blocking with FIONBIO.
fn ioctl(fd: u64, request: u64, arg: u64) -> Result<i64, i64> {
    let context = current();
    // An O_PATH descriptor takes no ioctl (Linux's fdget).
    let file = files::lookup(fd)?;
    match request {
        FIONBIO => {
            let on: i32 = usercopy::read(arg)?;
            file.set_flags(O_NONBLOCK, if on != 0 { O_NONBLOCK } else { 0 });
            Ok(0)
        }
        _ => {
            let mut t = context.table.lock();
            let slot = t.slots.get_mut(fd as usize).and_then(|s| s.as_mut()).ok_or(EBADF)?;
            slot.cloexec = request == FIOCLEX;
            Ok(0)
        }
    }
}

impl FilesContext {
    /// The open descriptors, ascending (/proc/<pid>/fd).
    pub fn open_fds(&self) -> Vec<u32> {
        let t = self.table.lock();
        t.slots.iter().enumerate().filter(|(_, s)| s.is_some()).map(|(fd, _)| fd as u32).collect()
    }

    /// How many descriptors the table has room for now (select's max_fds).
    pub fn size(&self) -> usize {
        self.table.lock().slots.len()
    }
}

/// The soft RLIMIT_NOFILE of the caller's process (the bound on its descriptors, and
/// poll's on `nfds`).
pub fn nofile() -> u64 {
    crate::ids::soft(crate::ids::RLIMIT_NOFILE)
}
