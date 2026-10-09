//! The descriptor table (phase R6e, docs/design/linux-server.md "The
//! descriptor table"): per process, the server's. A descriptor is a slot
//! with a reference to an open file description (`files::FileRef`) and the
//! close-on-exec bit; the table also keeps RLIMIT_NOFILE. The calls on
//! descriptors as such are here: close, close_range, dup, dup2, dup3,
//! fcntl's F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_SETFD, F_GETFL and
//! F_SETFL, the ioctls FIONBIO, FIOCLEX and FIONCLEX, and prlimit's
//! RLIMIT_NOFILE. F_SETFL changes O_APPEND and O_NONBLOCK; of the rest of
//! Linux's SETFL_MASK it ignores O_DIRECT (kept as opened), O_NOATIME (no
//! access times are kept apart) and O_ASYNC (no SIGIO), as the kernel did.
//!
//! **Which table.** Until the process model is the server's (R8), the
//! kernel's clone decides which processes share a table (CLONE_FILES), and
//! each such table of the kernel's (which holds no descriptor of a Linux
//! program) carries a word of the server's, its record: a pointer to its
//! `FilesContext`, as working-directory contexts carry theirs (`records`).
//! The kernel's table holds one strong reference, given with
//! `Arc::into_raw` (tagged `FILES_TAG`) and taken back when it ends: when
//! the last process using it exited, as `EVENT_RELEASE`, which the service
//! thread hands to the worker (`release_later`: closing sockets takes their
//! locks, which the pager must never wait for); when an execve let go of
//! it, from that call (`executed`, on the calling thread). The kernel never
//! replaces a table's record, so a thread's own record is alive while it
//! runs in that table; each thread remembers it in its words
//! (`thread::files`) after the first lookup, until it executes a program
//! or unshares its table.
//!
//! **fork, exec, exit** (the interface the process model calls; R8 takes
//! these over from the hooks below): `FilesContext::fork` is the table a
//! new process gets (a copy: every description shared, close-on-exec kept),
//! `FilesContext::for_exec` the one a process that executes a program gets
//! (the copy without the close-on-exec descriptors, made after the point of
//! no return, when the execve returns to the server; the old table goes
//! there and then, before the new program runs, so its close-on-exec
//! descriptors are closed as on Linux), and the end of a table (its last
//! `Arc`) closes its descriptors. Until R8 the pass-through hooks hand the
//! copies to the kernel as records (`before_pass_through`, `executed`).
//!
//! Locking: one lock per table, never held while program memory is copied
//! or a description is let go of (closing a file may take long, and may
//! take locks of its own): references taken out under the lock are
//! dropped after it.

use crate::files::{self, FileRef, O_APPEND, O_CLOEXEC, O_NONBLOCK};
use crate::sync::Mutex;
use crate::syscall;
use crate::thread;
use crate::usercopy;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const EBADF: i64 = 9;
const ENOMEM: i64 = 12;
const EEXIST: i64 = 17;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const EPERM: i64 = 1;

/// Tags a table's record among the words `EVENT_RELEASE` brings back (bit 0
/// is a tmpfs hold's, bit 1 a /data hold's; a context is 8-aligned).
pub const FILES_TAG: u64 = 4;

/// The largest RLIMIT_NOFILE (Linux's fs.nr_open).
const NR_OPEN: u64 = 1 << 20;
/// RLIMIT_NOFILE a tree starts with (soft and hard).
const DEFAULT_NOFILE: u64 = 4096;

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
    /// RLIMIT_NOFILE: (soft, hard). Kept with the table (a process's limit
    /// until the process model is the server's: tables are per process but
    /// for CLONE_FILES without CLONE_THREAD).
    limit: (u64, u64),
}

/// A descriptor table and the processes' record of it.
pub struct FilesContext {
    table: Mutex<Table>,
}

impl Table {
    /// The lowest free descriptor at or above `min`, below the soft limit.
    fn free_slot(&self, min: usize) -> Result<usize, i64> {
        let from = min.max(self.free_from);
        let found = (from..self.slots.len()).find(|&i| self.slots[i].is_none()).unwrap_or(self.slots.len().max(from));
        if found as u64 >= self.limit.0 {
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

    fn empty() -> Arc<FilesContext> {
        FilesContext::new(Table { slots: Vec::new(), free_from: 0, limit: (DEFAULT_NOFILE, DEFAULT_NOFILE) })
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
                return Ok(FilesContext::new(Table { slots, free_from, limit: t.limit }));
            }
        }
    }
}

/// Hands a table to the kernel as a record: one strong reference it holds.
fn give(context: Arc<FilesContext>) -> u64 {
    Arc::into_raw(context) as u64 | FILES_TAG
}

/// A table's record came back: the kernel's reference goes here (the
/// table's descriptors close with its last reference).
fn released_now(word: u64) {
    drop(unsafe { Arc::from_raw((word & !FILES_TAG) as *const FilesContext) });
}

/// The calling thread's descriptor table; a new, empty one for a table the
/// kernel made without the server (the tree's first process).
pub fn current() -> Arc<FilesContext> {
    let cached = thread::files();
    if cached != 0 {
        // The kernel's reference keeps it alive while this thread runs in
        // the table (see the module comment); this one is ours.
        let ptr = cached as *const FilesContext;
        unsafe {
            Arc::increment_strong_count(ptr);
            return Arc::from_raw(ptr);
        }
    }
    let word = syscall(SYS_FILES_RECORD, [FS_GET, 0, 0, 0, 0, 0]) as u64;
    if word != 0 {
        thread::set_files(word & !FILES_TAG);
        return current();
    }
    let context = FilesContext::empty();
    let word = give(context.clone());
    match syscall(SYS_FILES_RECORD, [FS_SET, word, 0, 0, 0, 0]) {
        0 => {
            thread::set_files(word & !FILES_TAG);
            context
        }
        e => {
            // Not handed over after all.
            released_now(word);
            // Another thread of the table was first: take that one.
            if e == -EEXIST { current() } else { context }
        }
    }
}

const SYS_CLONE: u64 = 56;
const SYS_FORK: u64 = 57;
const SYS_VFORK: u64 = 58;
const SYS_EXECVE: u64 = 59;
const SYS_EXECVEAT: u64 = 322;
const CLONE_FILES: u64 = 0x400;

/// Before a system call passes through: a clone that makes a new process
/// without CLONE_FILES gets a copy of the caller's table
/// (`FilesContext::fork`; without memory for it the call fails with ENOMEM
/// before it is made). An execve keeps a reference to the caller's table
/// (returned) for `executed`.
pub fn before_pass_through(s: &State) -> Result<Option<Arc<FilesContext>>, i64> {
    let copy = match s.rax {
        SYS_FORK | SYS_VFORK => current().fork()?,
        SYS_CLONE if s.rdi & CLONE_FILES == 0 => current().fork()?,
        SYS_EXECVE | SYS_EXECVEAT => return Ok(Some(current())),
        _ => return Ok(None),
    };
    syscall(SYS_FILES_RECORD, [FS_CHILD, give(copy), 0, 0, 0, 0]);
    Ok(None)
}

/// An execve passed through and succeeded (`legacy_syscall`'s answer
/// `LEGACY_EXECUTED`): the process has a new table of the kernel's without
/// a record. The new program's table is `old`'s descriptors without the
/// close-on-exec ones, made now, from the point of no return on (Linux's
/// do_close_on_exec); `released` is the old table's record if the execve
/// let go of its last holder. The old table goes here, on the calling
/// thread, before the new program runs: its close-on-exec descriptors are
/// closed when the program starts, as on Linux.
pub fn executed(old: Arc<FilesContext>, released: u64) {
    match old.for_exec() {
        Ok(new) => {
            let word = give(new);
            if syscall(SYS_FILES_RECORD, [FS_SET, word, 0, 0, 0, 0]) == 0 {
                thread::set_files(word & !FILES_TAG);
            } else {
                thread::set_files(0);
                drop(unsafe { Arc::from_raw((word & !FILES_TAG) as *const FilesContext) });
            }
        }
        Err(_) => {
            // Past the point of no return without memory for the table:
            // the process dies (Linux fails the execve before it).
            const SIGKILL: u64 = 9;
            thread::set_files(0);
            syscall(SYS_SIGNAL_THREAD, [SIGKILL, 0, 0, 0, 0, 0]);
        }
    }
    if released != 0 {
        released_now(released);
    }
    drop(old);
}

/// Tables whose processes ended, for the worker (`release_later`).
static ENDED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// `EVENT_RELEASE` of a table's record (its last process exited): the
/// service thread hands it to the worker, which closes its descriptors
/// (closing a socket takes its locks, which the pager must never wait
/// for).
pub fn release_later(word: u64) {
    ENDED.lock().push(word);
    HANDED.fetch_add(1, Ordering::AcqRel);
    crate::scm::request();
}

/// Tables handed to the worker, and let go of by it (a futex word).
static HANDED: AtomicU32 = AtomicU32::new(0);
static RELEASED: AtomicU32 = AtomicU32::new(0);

/// The worker: lets go of the tables handed to it.
pub fn release_ended() {
    let ended = core::mem::take(&mut *ENDED.lock());
    let n = ended.len() as u32;
    for word in ended {
        released_now(word);
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
const SYS_PRLIMIT64: u64 = 302;
const SYS_GETRLIMIT: u64 = 97;
const SYS_SETRLIMIT: u64 = 160;
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

const RLIMIT_NOFILE: u64 = 7;

/// The result of a call on descriptors as such in `s`, or None.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3) = (s.rdi, s.rsi, s.rdx, s.r10);
    let result = match s.rax {
        SYS_CLOSE => close(a0),
        SYS_DUP => dup(a0),
        SYS_DUP2 => dup3(a0, a1, 0, true),
        SYS_DUP3 => dup3(a0, a1, a2, false),
        SYS_FCNTL => fcntl(a0, a1, a2),
        SYS_CLOSE_RANGE => close_range(a0 as u32 as u64, a1 as u32 as u64, a2),
        SYS_IOCTL if matches!(a1 as u32 as u64, FIONBIO | FIOCLEX | FIONCLEX) => ioctl(a0, a1 as u32 as u64, a2),
        // RLIMIT_NOFILE of the caller (its table's); the other limits and
        // other processes' are the kernel's until R8.
        SYS_PRLIMIT64 if a1 == RLIMIT_NOFILE && is_self(a0) => prlimit_nofile(a2, a3),
        SYS_GETRLIMIT if a0 == RLIMIT_NOFILE => prlimit_nofile(0, a1),
        SYS_SETRLIMIT if a0 == RLIMIT_NOFILE => prlimit_nofile(a1, 0),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// Whether prlimit's `pid` is the caller's process.
fn is_self(pid: u64) -> bool {
    if pid == 0 {
        return true;
    }
    let mut ids = [0u64; 4];
    syscall(SYS_THREAD_IDS, [ids.as_mut_ptr() as u64, 0, 0, 0, 0, 0]) == 0 && ids[0] == pid
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
    if flags & CLOSE_RANGE_UNSHARE != 0 {
        let own = context.fork()?;
        let word = give(own.clone());
        let r = syscall(SYS_FILES_RECORD, [FILES_UNSHARE, word, 0, 0, 0, 0]);
        if r < 0 {
            released_now(word);
            return Err(-r);
        }
        thread::set_files(word & !FILES_TAG);
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
        let mut t = context.table.lock();
        if new >= t.limit.0 {
            return Err(EBADF);
        }
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
            let limit = context.table.lock().limit.0;
            if arg >= limit {
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

/// prlimit64 for RLIMIT_NOFILE of the caller: the old limits to `old`
/// unless 0, the new ones from `new` unless 0 (soft at most hard, hard at
/// most `NR_OPEN`; everyone is root, so the hard limit may grow).
fn prlimit_nofile(new: u64, old: u64) -> Result<i64, i64> {
    let wanted: Option<[u64; 2]> = if new != 0 { Some(usercopy::read(new)?) } else { None };
    if let Some([soft, hard]) = wanted {
        if soft > hard {
            return Err(EINVAL);
        }
        // (RLIM_INFINITY is beyond it, as on Linux.)
        if hard > NR_OPEN {
            return Err(EPERM);
        }
    }
    let context = current();
    let before = {
        let mut t = context.table.lock();
        let before = t.limit;
        if let Some([soft, hard]) = wanted {
            t.limit = (soft, hard);
        }
        before
    };
    if old != 0 {
        usercopy::write(old, &[before.0, before.1])?;
    }
    Ok(0)
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

/// The soft RLIMIT_NOFILE of the caller (poll's bound on `nfds`).
pub fn nofile() -> u64 {
    current().table.lock().limit.0
}
