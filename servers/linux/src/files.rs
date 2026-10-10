//! The server's open files (phase R6, the descriptor table since R6e): an
//! open file description (`Description`) is a file the server implements
//! (`File`: a pipe end, a socket, an open file of tmpfs or /data, a
//! terminal, an epoll instance, ...) with its status flags. Descriptors of the
//! calling process's table (`fdtable`) refer to descriptions (`FileRef`),
//! as do descriptors in flight (`scm`) and calls that use one: a
//! description goes, and its file closes, with its last reference (Linux's
//! fput), on whatever thread lets go of that.
//!
//! `handle` takes the system calls on descriptors and creates files (pipe,
//! pipe2, eventfd, netlink sockets; terminals are opened by their device
//! numbers, `tty::open_device`); the descriptor table's own calls (dup,
//! close, fcntl, ...) are `fdtable`'s, poll and select `poll`'s, epoll
//! `epoll`'s. Readiness changes reach them through `ready`.

use crate::datafile::{self, DataOpen};
use crate::eventfd::EventFd;
use crate::fdtable;
use crate::inotify::{self, Inotify};
use crate::netlink::{self, NetlinkSocket};
use crate::pipe::{self, Dst, PipeEnd, Src};
use crate::tmpfile::{self, TmpOpen};
use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use restricted::*;

pub const EBADF: i64 = 9;
pub const EINVAL: i64 = 22;
pub const ESPIPE: i64 = 29;
pub const ENOTTY: i64 = 25;
pub const EFAULT: i64 = 14;
pub const ENOTDIR: i64 = 20;

pub const O_ACCMODE: u32 = 0o3;
pub const O_WRONLY: u32 = 0o1;
pub const O_RDWR: u32 = 0o2;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_APPEND: u32 = 0o2000;
pub const O_CLOEXEC: u32 = 0o2000000;
pub const O_PATH: u32 = 0o10000000;
const O_DIRECT: u32 = 0o40000;

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;
pub const POLLNVAL: i16 = 0x20;
pub const POLLRDNORM: i16 = 0x40;
pub const POLLWRNORM: i16 = 0x100;
/// What a file that is always ready reports (Linux's DEFAULT_POLLMASK).
pub const ALWAYS_READY: i16 = POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM;

/// What an open file description is.
#[derive(Clone)]
pub enum File {
    Pipe(Arc<PipeEnd>),
    EventFd(Arc<EventFd>),
    /// An open file of the server's tmpfs.
    Tmp(Arc<TmpOpen>),
    /// An open file of /data.
    Data(Arc<DataOpen>),
    /// A netlink socket.
    Netlink(Arc<NetlinkSocket>),
    /// An inotify instance.
    Inotify(Arc<Inotify>),
    /// An AF_UNIX socket.
    Socket(Arc<crate::unix::Sock>),
    /// An internet socket (TCP, UDP, raw ICMP).
    Inet(Arc<crate::inet::InetSock>),
    /// An open terminal (the console, a pty's slave).
    Tty(Arc<crate::tty::TtyOpen>),
    /// A pty's master.
    PtyMaster(Arc<crate::pty::PtyMaster>),
    /// Null or zero on a node of the server's.
    Dev(Arc<crate::devices::DevOpen>),
    /// An O_PATH descriptor (of any node).
    Path(Arc<crate::pathfile::PathOpen>),
    /// An epoll instance.
    Epoll(Arc<crate::epoll::Epoll>),
    /// An open file of /proc or /sys.
    Proc(Arc<crate::procfile::ProcOpen>),
}

impl File {
    /// Always ready for poll, whatever happens (a regular file or a
    /// directory, null and zero, /proc and /sys): epoll
    /// refuses it with EPERM, as Linux does for a file without a poll
    /// method. Decided per kind of file: a file of one of these kinds that
    /// had readiness of its own (a pollable /proc file, as Linux's
    /// /proc/self/mounts) would need it decided per node, and a watch.
    pub fn always_ready(&self) -> bool {
        matches!(self, File::Tmp(_) | File::Data(_) | File::Dev(_) | File::Proc(_))
    }
}

/// An open file description: the file, its status flags (the access mode,
/// O_APPEND, O_NONBLOCK, ...; O_PATH for one that only names a node), and
/// its watchers (`poll::Watch`: pollers and epoll interests).
pub struct Description {
    pub id: u64,
    pub file: File,
    flags: AtomicU32,
    /// None for a file that is always ready (nothing to watch).
    pub watch: Option<Arc<crate::poll::Watch>>,
    /// References in flight (SCM_RIGHTS messages, `scm::Passed`): the
    /// collector of sockets in flight compares them with all references.
    pub inflight: AtomicUsize,
}

impl Description {
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// F_SETFL and FIONBIO: the flags in `changeable` take `value`'s
    /// (atomically: another thread may change the same description's at
    /// once).
    pub fn set_flags(&self, changeable: u32, value: u32) {
        let mut old = self.flags.load(Ordering::Relaxed);
        loop {
            let new = old & !changeable | value & changeable;
            match self.flags.compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(seen) => old = seen,
            }
        }
    }

    pub fn is_path(&self) -> bool {
        self.flags() & O_PATH != 0
    }

    /// Its readiness for poll, select and epoll now (each file computes its
    /// own, as Linux's poll methods do).
    pub fn poll_mask(&self) -> i16 {
        match &self.file {
            File::Pipe(end) => end.readiness(),
            File::EventFd(e) => e.ready(),
            File::Netlink(n) => n.readiness_now(),
            File::Inotify(i) => i.readiness_now(),
            File::Socket(s) => s.readiness_now(),
            File::Inet(s) => s.readiness_now(),
            File::Tty(t) => t.readiness_now(),
            File::PtyMaster(m) => m.readiness_now(),
            File::Epoll(e) => {
                if e.has_events() {
                    POLLIN | POLLRDNORM
                } else {
                    0
                }
            }
            File::Path(_) => POLLNVAL,
            File::Tmp(_) | File::Data(_) | File::Dev(_) | File::Proc(_) => ALWAYS_READY,
        }
    }
}

impl Drop for Description {
    /// The last reference went: the file closes (on the thread that let go
    /// of it: a close(2), the service thread for an exit, the worker for a
    /// message the collector dropped).
    fn drop(&mut self) {
        if let Some(watch) = &self.watch {
            crate::poll::forget(self.id, watch);
        }
        let service = crate::local::is_service();
        match &self.file {
            File::Pipe(end) => end.close(),
            File::Socket(sock) => {
                sock.release();
                // It may have been the last way into sockets in flight.
                if crate::scm::sockets_in_flight() {
                    crate::scm::request();
                }
            }
            // Closed in netd before close(2) returns (its port is free then,
            // as on Linux); by the net thread for a service thread, which
            // may not wait for netd.
            File::Inet(sock) => sock.release(!service),
            File::Tty(open) => open.tty.closed(open),
            File::PtyMaster(m) => crate::pty::master_closed(m),
            File::Epoll(e) => e.closed(),
            // An eventfd or a tmpfs or /data file simply goes (the latter two
            // returning their write access; an unlinked /data file's inode
            // goes at the next `datafs::reap`); a file of the kernel's closes
            // its handle.
            _ => {}
        }
    }
}

/// A reference to an open file description: what a descriptor, a call
/// using it and a descriptor in flight hold. Letting go of one of a
/// description in flight may leave nothing but its references in flight:
/// the collector of sockets in flight looks then (as Linux's unix_gc looks
/// when a file's references are all in flight).
pub struct FileRef(ManuallyDrop<Arc<Description>>);

impl FileRef {
    pub fn new(description: Arc<Description>) -> FileRef {
        FileRef(ManuallyDrop::new(description))
    }

    pub fn arc(&self) -> &Arc<Description> {
        &self.0
    }

    /// The reference itself (a descriptor in flight takes it over).
    pub fn into_arc(self) -> Arc<Description> {
        let mut me = ManuallyDrop::new(self);
        unsafe { ManuallyDrop::take(&mut me.0) }
    }

    /// The same description (for keys: epoll's interests).
    pub fn ptr(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
}

impl Clone for FileRef {
    fn clone(&self) -> FileRef {
        FileRef::new(Arc::clone(&self.0))
    }
}

impl core::ops::Deref for FileRef {
    type Target = Description;
    fn deref(&self) -> &Description {
        &self.0
    }
}

impl Drop for FileRef {
    fn drop(&mut self) {
        // Taken once, here.
        let description = unsafe { ManuallyDrop::take(&mut self.0) };
        let in_flight = description.inflight.load(Ordering::Acquire) > 0;
        drop(description);
        // After the reference is gone, so that the collector sees it gone.
        if in_flight {
            crate::scm::request();
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Most bytes one getdents64 call returns (its buffer is the server's, reserved up front).
pub const GETDENTS_MAX: usize = 64 * 1024;

/// Most bytes all directory snapshots of the tree may take (`getdents64` takes one per open
/// directory description from its start): a directory read through many descriptions at
/// once cannot fill the server's heap (ENOMEM beyond it).
const MAX_SNAPSHOT_BYTES: usize = 32 << 20;
const ENOMEM: i64 = 12;
static SNAPSHOT_BYTES: AtomicUsize = AtomicUsize::new(0);

/// What a directory snapshot takes of `MAX_SNAPSHOT_BYTES`, given back when it goes.
pub struct SnapshotCharge {
    bytes: usize,
}

impl SnapshotCharge {
    pub const fn new() -> SnapshotCharge {
        SnapshotCharge { bytes: 0 }
    }

    /// `n` bytes more (an entry's name and what it takes besides); ENOMEM beyond the bound.
    pub fn add(&mut self, n: usize) -> Result<(), i64> {
        SNAPSHOT_BYTES.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| (c + n <= MAX_SNAPSHOT_BYTES).then_some(c + n)).map_err(|_| ENOMEM)?;
        self.bytes += n;
        Ok(())
    }
}

impl Drop for SnapshotCharge {
    fn drop(&mut self) {
        let n = self.bytes;
        let _ = SNAPSHOT_BYTES.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| Some(c.saturating_sub(n)));
    }
}

/// Whether a signal (or a stop) waits for the calling thread (Linux's signal_pending):
/// a long call that does not wait returns what it did so far, or EINTR (restarted as
/// Linux's ERESTARTSYS) if nothing yet.
pub fn signal_pending() -> bool {
    crate::signal::pending()
}

pub fn new_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A new open file description of `file` (status flags `flags`; O_CLOEXEC
/// is the descriptor's), known for readiness reports as `id`, and a
/// descriptor for it in the calling process's table (the lowest free one).
/// If no descriptor can be had, nothing was made: the file is the caller's
/// to undo (no close happens for it).
pub fn install(id: u64, file: File, flags: u32) -> Result<i64, i64> {
    let watch = (!file.always_ready()).then(|| crate::poll::Watch::new(id));
    install_watched(id, file, flags, watch)
}

/// `install` with the description's watch made by the caller (an epoll
/// instance knows its own before it is installed). Without a descriptor
/// the watch goes again.
pub fn install_watched(id: u64, file: File, flags: u32, watch: Option<Arc<crate::poll::Watch>>) -> Result<i64, i64> {
    let kept = watch.clone();
    let r = fdtable::current().install_new(
        || FileRef::new(Arc::new(Description { id, file, flags: AtomicU32::new(flags & !O_CLOEXEC), watch, inflight: AtomicUsize::new(0) })),
        flags & O_CLOEXEC != 0,
    );
    if r.is_err() {
        if let Some(w) = &kept {
            crate::poll::forget(id, w);
        }
    }
    r
}

/// File `id` changed its readiness (now `ready`), or had an event that
/// counts as an edge for EPOLLET (new data, room): its pollers and epoll
/// interests hear of it. Each file reports under its own lock, so reports
/// keep their order.
pub fn ready(id: u64, ready: i16) {
    crate::poll::report(id, ready);
}

/// The description behind descriptor `fd` (EBADF), O_PATH ones too (for
/// what takes them: Linux's fdget_raw).
pub fn lookup_raw(fd: u64) -> Result<FileRef, i64> {
    fdtable::current().get(fd)
}

/// The description behind descriptor `fd` for an operation on its file:
/// EBADF also for an O_PATH descriptor, which names a node only (Linux's
/// fdget).
pub fn lookup(fd: u64) -> Result<FileRef, i64> {
    let f = lookup_raw(fd)?;
    if f.is_path() {
        return Err(EBADF);
    }
    Ok(f)
}

/// The open tmpfs file behind descriptor `fd`, if it is one.
pub fn tmp_of(fd: u64) -> Option<Arc<TmpOpen>> {
    match &lookup_raw(fd).ok()?.file {
        File::Tmp(f) => Some(f.clone()),
        _ => None,
    }
}

/// The open /data file behind descriptor `fd`, if it is one.
pub fn data_of(fd: u64) -> Option<Arc<DataOpen>> {
    match &lookup_raw(fd).ok()?.file {
        File::Data(f) => Some(f.clone()),
        _ => None,
    }
}

/// What a descriptor of the server's that keeps its origin (a terminal, a pty's master,
/// null or zero, an O_PATH descriptor) was opened by; None for other descriptors.
pub struct OriginOf {
    pub path: alloc::string::String,
    /// The node's file type (`S_IFMT` bits).
    pub kind: u32,
    /// An O_PATH descriptor.
    pub o_path: bool,
    file: File,
}

impl OriginOf {
    /// The node itself (another reference to it).
    pub fn node(&self) -> Result<crate::namespace::Node, i64> {
        self.origin().node.duplicate()
    }

    pub fn origin(&self) -> &crate::namespace::Origin {
        match &self.file {
            File::Tty(t) => &t.origin,
            File::PtyMaster(m) => &m.origin,
            File::Dev(d) => &d.origin,
            File::Path(p) => &p.origin,
            _ => unreachable!("only files with an origin"),
        }
    }
}

pub fn origin_of(fd: u64) -> Option<OriginOf> {
    origin_of_file(&lookup_raw(fd).ok()?)
}

/// `origin_of` for a description (any process's: /proc/<pid>/fd).
pub fn origin_of_file(f: &FileRef) -> Option<OriginOf> {
    let file = f.file.clone();
    let o_path = match &file {
        File::Tty(_) | File::PtyMaster(_) | File::Dev(_) => false,
        File::Path(_) => true,
        _ => return None,
    };
    let mut found = OriginOf { path: alloc::string::String::new(), kind: 0, o_path, file };
    let (path, kind) = (found.origin().path.clone(), found.origin().kind);
    found.path = path;
    found.kind = kind;
    Some(found)
}

/// The status of descriptor `fd` (EBADF for none).
pub fn stat_of(fd: u64) -> Result<vfs::stat::Stat, i64> {
    stat_file(&lookup_raw(fd)?.file)
}

/// The open /proc or /sys file behind descriptor `fd`, if it is one.
pub fn proc_of(fd: u64) -> Option<Arc<crate::procfile::ProcOpen>> {
    match &lookup_raw(fd).ok()?.file {
        File::Proc(f) => Some(f.clone()),
        _ => None,
    }
}

/// The status of an open file.
pub fn stat_file(file: &File) -> Result<vfs::stat::Stat, i64> {
    let bytes = match file {
        File::Pipe(end) => end.stat(),
        File::EventFd(e) => e.stat(),
        File::Tmp(t) => return Ok(t.inode.status()),
        File::Data(d) => crate::datafs::stat(&d.inode)?,
        File::Netlink(n) => n.stat(),
        File::Inotify(i) => i.stat(),
        File::Socket(s) => crate::sockcalls::stat(s),
        File::Inet(s) => crate::inetcalls::stat(s),
        File::Tty(t) => t.origin.stat()?,
        File::PtyMaster(m) => m.origin.stat()?,
        File::Dev(d) => d.origin.stat()?,
        File::Path(p) => p.origin.stat()?,
        File::Epoll(e) => e.stat(),
        File::Proc(p) => crate::procfs::stat(&p.node)?,
    };
    Ok(vfs::stat::Stat::from_bytes(&bytes))
}

/// For mmap of descriptor `fd`: the object to map (a handle to close once
/// mapped, unless `Mapping::Kernel`) and whether the mapping stays
/// read-only, or why not.
pub fn map_object(fd: u64, shared: bool, prot_write: bool) -> Result<Mapping, i64> {
    const ENODEV: i64 = 19;
    let object = |r: Result<(u64, bool), i64>| r.map(|(h, ro)| Mapping::Object(h, ro));
    let f = lookup_raw(fd)?;
    let flags = f.flags();
    match &f.file {
        File::Tmp(t) => object(t.map_object(flags, shared, prot_write)),
        File::Data(d) => object(d.map_object(flags, shared, prot_write)),
        File::Dev(d) => crate::devices::map(d, flags, shared, prot_write),
        File::Path(_) => Err(EBADF),
        _ => Err(ENODEV),
    }
}

/// What mmap of one of the server's files maps.
pub enum Mapping {
    /// A memory object (a handle to close once mapped), and whether the mapping stays
    /// read-only.
    Object(u64, bool),
    /// Anonymous memory, as for MAP_ANONYMOUS (zero's mappings); `read_only`: a shared
    /// mapping that may never become writable.
    Anonymous { read_only: bool },
}

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_FSTAT: u64 = 5;
pub const SYS_LSEEK: u64 = 8;
pub const SYS_IOCTL: u64 = 16;
const SYS_SENDFILE: u64 = 40;
const SYS_SOCKET: u64 = 41;
const SYS_COPY_FILE_RANGE: u64 = 326;
pub const SYS_PREAD64: u64 = 17;
pub const SYS_PWRITE64: u64 = 18;
pub const SYS_READV: u64 = 19;
pub const SYS_WRITEV: u64 = 20;
const SYS_PIPE: u64 = 22;
pub const SYS_FSYNC: u64 = 74;
pub const SYS_FDATASYNC: u64 = 75;
pub const SYS_FTRUNCATE: u64 = 77;
const SYS_PIPE2: u64 = 293;
const SYS_EVENTFD: u64 = 284;
const SYS_EVENTFD2: u64 = 290;
pub const SYS_PREADV: u64 = 295;
pub const SYS_PWRITEV: u64 = 296;
const SYS_SYNC: u64 = 162;
const SYS_SYNCFS: u64 = 306;
const SYS_PREADV2: u64 = 327;
const SYS_PWRITEV2: u64 = 328;
pub const SYS_GETDENTS64: u64 = 217;
pub const SYS_FSTATFS: u64 = 138;

/// The result of a file system call in `s` the server handles, or None.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3) = (s.rdi, s.rsi, s.rdx, s.r10);
    let result = match s.rax {
        SYS_PIPE => pipe2(a0, 0),
        SYS_EVENTFD => eventfd2(a0, 0),
        SYS_EVENTFD2 => eventfd2(a0, a1),
        SYS_SENDFILE => sendfile(a0, a1, a2, s.r10),
        SYS_PIPE2 => pipe2(a0, a1),
        SYS_COPY_FILE_RANGE => copy_file_range(a0, a1, a2, a3, s.r8, s.r9),
        SYS_SOCKET if a0 == netlink::AF_NETLINK => netlink::socket(a1, a2),
        inotify::SYS_INOTIFY_INIT => inotify::init(0),
        inotify::SYS_INOTIFY_INIT1 => inotify::init(a0),
        inotify::SYS_INOTIFY_RM_WATCH => inotify::instance(a0).and_then(|i| i.rm_watch(a1 as i32)),
        netlink::SYS_CONNECT
        | netlink::SYS_ACCEPT
        | netlink::SYS_SENDTO
        | netlink::SYS_RECVFROM
        | netlink::SYS_SENDMSG
        | netlink::SYS_RECVMSG
        | netlink::SYS_SHUTDOWN
        | netlink::SYS_BIND
        | netlink::SYS_LISTEN
        | netlink::SYS_GETSOCKNAME
        | netlink::SYS_GETPEERNAME
        | netlink::SYS_SETSOCKOPT
        | netlink::SYS_GETSOCKOPT
        | netlink::SYS_ACCEPT4 => match lookup(a0) {
            Ok(f) => match &f.file {
                File::Netlink(n) => netlink::call(s.rax, n, f.flags(), [a0, a1, a2, a3, s.r8, s.r9]),
                // AF_UNIX and internet sockets and the answer for files that
                // are none: `sockcalls`.
                _ => return None,
            },
            Err(_) => return None,
        },
        // Only /data's files have something to write back, in every
        // instance's page cache.
        SYS_SYNC => sync_everywhere().map(|_| 0),
        SYS_SYNCFS => lookup(a0).and_then(|f| match &f.file {
            File::Data(_) => sync_everywhere().map(|_| 0),
            _ => Ok(0),
        }),
        // The interface requests every socket takes.
        SYS_IOCTL if crate::netdev::is_request(a1) => crate::netdev::ioctl(a0, a1, a2),
        SYS_READ | SYS_WRITE | SYS_READV | SYS_WRITEV | SYS_FSTAT | SYS_LSEEK | SYS_IOCTL | SYS_PREAD64 | SYS_PWRITE64
        | SYS_PREADV | SYS_PWRITEV | SYS_FSYNC | SYS_FDATASYNC | SYS_FTRUNCATE | SYS_GETDENTS64 | SYS_FSTATFS => {
            // fstat and fstatfs take an O_PATH descriptor (they describe its
            // node); the requests on the descriptor itself are `fdtable`'s.
            let f = if matches!(s.rax, SYS_FSTAT | SYS_FSTATFS) { lookup_raw(a0) } else { lookup(a0) };
            match f {
                Ok(f) => on_file(s.rax, &f.file, f.flags(), a1, a2, a3),
                Err(e) => Err(e),
            }
        }
        SYS_PREADV2 | SYS_PWRITEV2 => lookup(a0).and_then(|f| rw2(s.rax == SYS_PWRITEV2, &f.file, f.flags(), a1, a2, a3 as i64, s.r9)),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// preadv2/pwritev2: the vectored call at the file position (offset -1) or
/// at the offset, with O_APPEND as the flags have it for this call
/// (`vfs::rw::plan`). RWF_DSYNC/RWF_SYNC make a /data write durable before
/// it returns (the other files are memory: nothing to wait for).
fn rw2(write: bool, file: &File, flags: u32, iov: u64, count: u64, offset: i64, rwf: u64) -> Result<i64, i64> {
    let allowed = if write { flags & O_ACCMODE != 0 } else { flags & O_ACCMODE != O_WRONLY };
    if !allowed {
        return Err(EBADF);
    }
    let plan = vfs::rw::plan(write, offset, rwf, flags & O_APPEND != 0)?;
    let flags = if plan.append { flags | O_APPEND } else { flags & !O_APPEND };
    let flags = if plan.sync { flags | datafile::O_DSYNC } else { flags };
    let nr = match (write, plan.at) {
        (false, None) => SYS_READV,
        (true, None) => SYS_WRITEV,
        (false, Some(_)) => SYS_PREADV,
        (true, Some(_)) => SYS_PWRITEV,
    };
    on_file(nr, file, flags, iov, count, plan.at.unwrap_or(0))
}

fn on_file(nr: u64, file: &File, flags: u32, a1: u64, a2: u64, a3: u64) -> Result<i64, i64> {
    let end = match file {
        File::Pipe(end) => end,
        File::EventFd(e) => return on_eventfd(nr, e, flags, a1, a2),
        File::Tmp(f) => return tmpfile::call(nr, f, flags, a1, a2, a3),
        File::Data(f) => return datafile::call(nr, f, flags, a1, a2, a3),
        File::Netlink(n) => return netlink::call(nr, n, flags, [0, a1, a2, a3, 0, 0]),
        File::Inotify(i) => return inotify::call(nr, i, flags, a1, a2),
        File::Socket(s) => return crate::sockcalls::on_file(nr, s, flags, a1, a2),
        File::Inet(s) => return crate::inetcalls::on_file(nr, s, flags, a1, a2),
        File::Tty(t) => return crate::tty::call(nr, t, flags, a1, a2),
        File::PtyMaster(m) => return crate::pty::call(nr, m, flags, a1, a2),
        File::Dev(d) => return crate::devices::call(nr, d, flags, a1, a2),
        File::Path(p) => return crate::pathfile::call(nr, p, a1),
        File::Epoll(e) => return crate::epoll::on_file(nr, e, a1),
        File::Proc(p) => return crate::procfile::call(nr, p, flags, a1, a2, a3),
    };
    let readable = flags & O_ACCMODE != O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        SYS_READ if !readable => Err(EBADF),
        SYS_WRITE if !writable => Err(EBADF),
        SYS_READ => end.read(Dst::Program(&[(a1, a2)]), nonblock),
        SYS_WRITE => end.write(Src::Program(&[(a1, a2)]), nonblock),
        SYS_READV | SYS_WRITEV => {
            let vecs = iovecs(a1, a2)?;
            match nr {
                SYS_READV if !readable => Err(EBADF),
                SYS_WRITEV if !writable => Err(EBADF),
                SYS_READV => end.read(Dst::Program(&vecs), nonblock),
                _ => end.write(Src::Program(&vecs), nonblock),
            }
        }
        SYS_FSTAT => end.fstat(a1),
        SYS_LSEEK | SYS_PREAD64 | SYS_PWRITE64 | SYS_PREADV | SYS_PWRITEV => Err(ESPIPE),
        SYS_IOCTL => Err(ENOTTY),
        SYS_GETDENTS64 => Err(ENOTDIR),
        _ => Err(EINVAL),
    }
}

/// An iovec array of the program: (base, length) pairs; EINVAL for more
/// than 1024 or a length negative as an ssize_t (Linux's
/// copy_iovec_from_user).
pub fn iovecs(iov: u64, count: u64) -> Result<alloc::vec::Vec<(u64, u64)>, i64> {
    if count > 1024 {
        return Err(EINVAL);
    }
    let mut out = alloc::vec::Vec::new();
    for i in 0..count {
        let pair: [u64; 2] = crate::usercopy::read(iov + i * 16)?;
        if (pair[1] as i64) < 0 {
            return Err(EINVAL);
        }
        out.push((pair[0], pair[1]));
    }
    Ok(out)
}

/// pipe2(fds, flags): a pipe; its read and write descriptors at `fds`.
fn pipe2(fds: u64, flags: u64) -> Result<i64, i64> {
    let flags = flags as u32;
    if flags & !(O_CLOEXEC | O_NONBLOCK | O_DIRECT) != 0 {
        return Err(EINVAL);
    }
    let (read_end, write_end) = pipe::new();
    let (rid, wid) = (read_end.id(), write_end.id());
    let common = flags & (O_CLOEXEC | O_NONBLOCK);
    let table = fdtable::current();
    let rfd = install(rid, File::Pipe(read_end.clone()), common)?;
    let wfd = match install(wid, File::Pipe(write_end.clone()), common | O_WRONLY) {
        Ok(fd) => fd,
        Err(e) => {
            drop(table.take(rfd as u64));
            return Err(e);
        }
    };
    if let Err(e) = crate::usercopy::write(fds, &[rfd as i32, wfd as i32]) {
        drop(table.take(rfd as u64));
        drop(table.take(wfd as u64));
        return Err(e);
    }
    Ok(0)
}

/// sendfile(out, in, offset, count), through the server's memory: chunks,
/// returning after a short read, as Linux does.
fn sendfile(out_fd: u64, in_fd: u64, offset: u64, count: u64) -> Result<i64, i64> {
    let (out, input) = (lookup(out_fd)?, lookup(in_fd)?);
    if offset != 0 {
        return Err(EINVAL);
    }
    // An eventfd moves 8-byte values, not data; a directory none; a
    // netlink socket datagrams; an epoll instance nothing.
    let unfit = |f: &File| matches!(f, File::EventFd(_) | File::Netlink(_) | File::Inotify(_) | File::Epoll(_));
    if unfit(&out.file) || unfit(&input.file) {
        return Err(EINVAL);
    }
    if let File::Tmp(f) = &out.file {
        if f.inode.is_dir() {
            return Err(EINVAL);
        }
    }
    if let File::Data(f) = &out.file {
        if f.inode.kind == vfs::S_IFDIR {
            return Err(EINVAL);
        }
    }
    if out.flags() & O_ACCMODE == 0 || input.flags() & O_ACCMODE == O_WRONLY {
        return Err(EBADF);
    }
    let (in_flags, out_flags) = (input.flags(), out.flags());
    let mut buf = alloc::vec![0u8; 4096];
    let mut total = 0u64;
    while total < count {
        let want = (count - total).min(buf.len() as u64) as usize;
        let n = match &input.file {
            File::Pipe(end) => end.read(Dst::Server(&mut buf[..want]), in_flags & O_NONBLOCK != 0),
            File::Tmp(f) => f.read_server(&mut buf[..want]).map(|n| n as i64),
            File::Data(f) => f.read_server(&mut buf[..want]).map(|n| n as i64),
            File::Socket(s) => crate::sockcalls::read_server(s, in_flags, &mut buf[..want]),
            File::Inet(s) => crate::inetcalls::read_server(s, in_flags, &mut buf[..want]),
            File::Tty(t) => t.tty.read(t, crate::unix::Sink::Server { buf: &mut buf[..want], at: 0 }, in_flags & O_NONBLOCK != 0),
            File::PtyMaster(m) => m.read(crate::unix::Sink::Server { buf: &mut buf[..want], at: 0 }, in_flags & O_NONBLOCK != 0),
            File::Dev(d) => crate::devices::read(d, crate::unix::Sink::Server { buf: &mut buf[..want], at: 0 }),
            File::Proc(f) => f.read_server(&mut buf[..want]).map(|n| n as i64),
            File::EventFd(_) | File::Netlink(_) | File::Inotify(_) | File::Epoll(_) | File::Path(_) => Err(EINVAL),
        };
        let n = match n {
            Ok(n) => n as usize,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let wrote = match &out.file {
            File::Pipe(end) => end.write(Src::Server(&buf[..n]), out_flags & O_NONBLOCK != 0),
            File::Tmp(f) => f.write_server(&buf[..n], out_flags & O_APPEND != 0).map(|n| n as i64),
            File::Data(f) => f.write_server(&buf[..n], out_flags & O_APPEND != 0).map(|n| n as i64),
            File::Socket(s) => crate::sockcalls::write_server(s, out_flags, &buf[..n]),
            File::Inet(s) => crate::inetcalls::write_server(s, out_flags, &buf[..n]),
            File::Tty(t) => t.tty.write(t, crate::unix::Source::Server { buf: &buf[..n], at: 0 }, out_flags & O_NONBLOCK != 0),
            File::PtyMaster(m) => m.write(crate::unix::Source::Server { buf: &buf[..n], at: 0 }, out_flags & O_NONBLOCK != 0),
            File::Dev(_) => Ok(n as i64),
            File::Path(_) | File::Proc(_) => Err(EBADF),
            File::EventFd(_) | File::Netlink(_) | File::Inotify(_) | File::Epoll(_) => Err(EINVAL),
        };
        match wrote {
            Ok(_) => total += n as u64,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        }
        if n < want {
            break;
        }
    }
    Ok(total as i64)
}

fn on_eventfd(nr: u64, e: &EventFd, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        SYS_READ => e.read(a1, a2, nonblock),
        SYS_WRITE => e.write(a1, a2, nonblock),
        // One value per buffer, as reads and writes one after the other.
        SYS_READV | SYS_WRITEV => {
            let mut done = 0;
            for (base, len) in iovecs(a1, a2)? {
                let r = if nr == SYS_READV { e.read(base, len, nonblock) } else { e.write(base, len, nonblock) };
                match r {
                    Ok(n) => done += n,
                    Err(err) if done == 0 => return Err(err),
                    Err(_) => break,
                }
            }
            Ok(done)
        }
        SYS_FSTAT => e.fstat(a1),
        SYS_LSEEK | SYS_PREAD64 | SYS_PWRITE64 | SYS_PREADV | SYS_PWRITEV => Err(ESPIPE),
        SYS_IOCTL => Err(ENOTTY),
        _ => Err(EINVAL),
    }
}

/// A regular file of the server's filesystems, for copy_file_range.
enum Regular {
    Tmp(Arc<TmpOpen>),
    Data(Arc<DataOpen>),
}

impl Regular {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        match self {
            Regular::Tmp(f) => f.read_at(off, buf),
            Regular::Data(f) => f.read_at(off, buf),
        }
    }

    fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        match self {
            Regular::Tmp(f) => f.write_at(off, data),
            Regular::Data(f) => f.write_at(off, data),
        }
    }

    fn position(&self) -> Result<crate::sync::SleepMutexGuard<'_, u64>, i64> {
        match self {
            Regular::Tmp(f) => f.position(),
            Regular::Data(f) => f.position(),
        }
    }

    fn size(&self) -> Result<u64, i64> {
        match self {
            Regular::Tmp(f) => Ok(f.inode.size()),
            Regular::Data(f) => crate::datafs::size(&f.inode),
        }
    }

    /// Whether both are the same inode, and whether of one filesystem.
    fn same(&self, other: &Regular) -> (bool, bool) {
        match (self, other) {
            (Regular::Tmp(a), Regular::Tmp(b)) => (Arc::ptr_eq(&a.inode, &b.inode), true),
            (Regular::Data(a), Regular::Data(b)) => (Arc::ptr_eq(&a.inode, &b.inode), true),
            _ => (false, false),
        }
    }
}

/// The file behind `fd` for copy_file_range, with its open flags: EBADF
/// for no descriptor, EISDIR for a directory, EINVAL for a file that is
/// not regular.
fn regular(fd: u64) -> Result<(Regular, u32), i64> {
    const EISDIR: i64 = 21;
    let f = lookup(fd)?;
    let flags = f.flags();
    let kind = match &f.file {
        File::Tmp(t) => t.inode.file_type(),
        File::Data(d) => d.inode.kind,
        _ => return Err(EINVAL),
    };
    match (kind, &f.file) {
        (vfs::S_IFDIR, _) => Err(EISDIR),
        (vfs::S_IFREG, File::Tmp(t)) => Ok((Regular::Tmp(t.clone()), flags)),
        (vfs::S_IFREG, File::Data(d)) => Ok((Regular::Data(d.clone()), flags)),
        _ => Err(EINVAL),
    }
}

/// copy_file_range(fd_in, off_in, fd_out, off_out, len, flags): copies
/// within one of the server's filesystems (EXDEV across them, as Linux
/// between filesystems of different types), through the server's memory;
/// at the given offsets (which move) or at the descriptions' positions.
fn copy_file_range(fd_in: u64, off_in: u64, fd_out: u64, off_out: u64, len: u64, flags: u64) -> Result<i64, i64> {
    const EXDEV: i64 = 18;
    if flags != 0 {
        return Err(EINVAL);
    }
    let (input, in_flags) = regular(fd_in)?;
    let (output, out_flags) = regular(fd_out)?;
    if in_flags & O_ACCMODE == O_WRONLY || out_flags & O_ACCMODE == 0 || out_flags & O_APPEND != 0 {
        return Err(EBADF);
    }
    let (same_inode, same_fs) = input.same(&output);
    if !same_fs {
        return Err(EXDEV);
    }
    let read_off = |p: u64| -> Result<Option<u64>, i64> {
        if p == 0 {
            return Ok(None);
        }
        let v: i64 = crate::usercopy::read(p)?;
        if v < 0 { Err(EINVAL) } else { Ok(Some(v as u64)) }
    };
    let (given_in, given_out) = (read_off(off_in)?, read_off(off_out)?);
    // The positions of the descriptions that take part (one lock for one
    // description used at both ends).
    // The positions are read now and written when the copy is done, with
    // no lock held across it (as Linux: two copies between the same files
    // in opposite directions must not wait for each other).
    let start_in = match given_in {
        Some(o) => o,
        None => *input.position()?,
    };
    let start_out = match given_out {
        Some(o) => o,
        None => *output.position()?,
    };
    // Nothing beyond the input's end is copied: the length is clamped
    // before the overlap is checked (as Linux's generic checks).
    let size = input.size()?;
    let len = len.min(size.saturating_sub(start_in));
    if same_inode && len > 0 && start_in < start_out.saturating_add(len) && start_out < start_in.saturating_add(len) {
        return Err(EINVAL);
    }
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let mut done = 0u64;
    while done < len {
        let want = (len - done).min(buf.len() as u64) as usize;
        let n = match input.read_at(start_in + done, &mut buf[..want]) {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        match output.write_at(start_out + done, &buf[..n]) {
            Ok(w) => done += w as u64,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
        if n < want {
            break;
        }
    }
    match given_in {
        Some(_) => crate::usercopy::write(off_in, &(start_in + done))?,
        None => *input.position()? = start_in + done,
    }
    match given_out {
        Some(_) => crate::usercopy::write(off_out, &(start_out + done))?,
        None => *output.position()? = start_out + done,
    }
    Ok(done as i64)
}

/// eventfd2(initval, flags): a counter starting at `initval` (32 bits).
fn eventfd2(initval: u64, flags: u64) -> Result<i64, i64> {
    const EFD_SEMAPHORE: u64 = 1;
    if flags & !(EFD_SEMAPHORE | (O_NONBLOCK | O_CLOEXEC) as u64) != 0 {
        return Err(EINVAL);
    }
    let e = Arc::new(EventFd::new(initval as u32 as u64, flags & EFD_SEMAPHORE != 0));
    let open = O_RDWR | (flags as u32 & (O_NONBLOCK | O_CLOEXEC));
    install(e.id(), File::EventFd(e.clone()), open)
}

/// sync(2) of /data: this instance's caches and, at the same time, every
/// other instance's (each has its own page cache of the disk); returns
/// once all are written back and flushed. The error is this instance's.
fn sync_everywhere() -> Result<(), i64> {
    let ticket = crate::syscall(SYS_SYNC_OTHERS, [0; 6]);
    let result = crate::datafs::sync_all();
    if ticket > 0 {
        crate::syscall(SYS_SYNC_OTHERS, [ticket as u64, 0, 0, 0, 0, 0]);
    }
    result
}
