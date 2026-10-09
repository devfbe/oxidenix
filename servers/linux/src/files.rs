//! The server's files (phase R6): objects the server implements, each
//! named in the kernel's descriptor table by a placeholder (see
//! `restricted::SYS_KFD_INSTALL`). The table here maps the placeholder's id
//! to the object; an object goes when the kernel reports its placeholder's
//! last descriptor closed (`EVENT_CLOSED`, to the service thread).
//!
//! `handle` takes the system calls on descriptors and creates files (pipe,
//! pipe2, eventfd, netlink sockets; terminals are opened by their device
//! numbers, `tty::open_device`): for a descriptor of one of the
//! server's files it answers itself, for one of the kernel's it returns
//! None and the call passes through (except the requests the server
//! answers for any descriptor: the netdevice ioctls on sockets).

use crate::datafile::{self, DataOpen};
use crate::eventfd::EventFd;
use crate::inotify::{self, Inotify};
use crate::netlink::{self, NetlinkSocket};
use crate::tmpfile::{self, TmpOpen};
use crate::pipe::{self, Dst, PipeEnd, Src};
use crate::sync::Mutex;
use crate::syscall;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
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
const O_DIRECT: u32 = 0o40000;

/// What a placeholder names.
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
    /// An open file of /proc or /sys.
    Proc(Arc<crate::procfile::ProcOpen>),
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

static FILES: Mutex<BTreeMap<u64, File>> = Mutex::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Whether a signal (or a stop) waits for the calling thread (Linux's signal_pending):
/// a long call that does not wait returns what it did so far, or EINTR (restarted as
/// Linux's ERESTARTSYS) if nothing yet.
pub fn signal_pending() -> bool {
    crate::signal::pending()
}

pub fn new_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Registers `file` under `id` and gives it a descriptor (the lowest free
/// one). On failure the file is forgotten again.
pub fn install(id: u64, file: File, flags: u32, ready: i16) -> Result<i64, i64> {
    // Regular files, directories and null and zero are always ready.
    let kind = if matches!(file, File::Tmp(_) | File::Data(_) | File::Dev(_) | File::Proc(_)) { KFD_ALWAYS_READY } else { 0 };
    FILES.lock().insert(id, file);
    let fd = syscall(SYS_KFD_INSTALL, [id, flags as u64, ready as u16 as u64, kind, 0, 0]);
    if fd < 0 {
        FILES.lock().remove(&id);
        return Err(-fd);
    }
    Ok(fd)
}

/// Reports file `id`'s readiness for poll, select and epoll.
pub fn ready(id: u64, ready: i16) {
    syscall(SYS_KFD_READY, [id, ready as u16 as u64, 0, 0, 0, 0]);
}

/// The kernel closed the placeholder's last descriptor: the object goes.
/// `service`: the instance's pager learnt it (a process's exit, a
/// descriptor in flight let go), which may not wait for netd; else the
/// thread whose close(2) it was, before the call returns.
pub fn closed(id: u64, service: bool) {
    let gone = FILES.lock().remove(&id);
    match gone {
        Some(File::Pipe(end)) => end.close(),
        Some(File::Socket(sock)) => {
            sock.release();
            // It may have been the last way into sockets in flight.
            if crate::scm::sockets_in_flight() {
                crate::scm::request();
            }
        }
        // Closed in netd before close(2) returns (its port is free then,
        // as on Linux); by the net thread for the pager.
        Some(File::Inet(sock)) => sock.release(!service),
        Some(File::Tty(open)) => open.tty.closed(&open),
        Some(File::PtyMaster(m)) => crate::pty::master_closed(&m),
        _ => {}
    }
    // An eventfd or a tmpfs or /data file simply goes (the latter two
    // returning their write access; an unlinked /data file's inode goes at
    // the next `datafs::reap`).
}

/// The server's file `id`, if it lives.
pub fn get(id: u64) -> Option<File> {
    FILES.lock().get(&id).cloned()
}

/// Whether descriptor `fd` names one of the server's files.
pub fn is_server_file(fd: u64) -> bool {
    lookup(fd).is_some()
}

/// The open tmpfs file behind descriptor `fd`, if it is one.
pub fn tmp_of(fd: u64) -> Option<Arc<TmpOpen>> {
    match lookup(fd)? {
        (File::Tmp(f), _) => Some(f),
        _ => None,
    }
}

/// The open /data file behind descriptor `fd`, if it is one.
pub fn data_of(fd: u64) -> Option<Arc<DataOpen>> {
    match lookup(fd)? {
        (File::Data(f), _) => Some(f),
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
    let (file, _) = lookup(fd)?;
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

/// The status of descriptor `fd`, one of the server's files or of the
/// kernel's (EBADF for neither).
pub fn stat_of(fd: u64) -> Result<vfs::stat::Stat, i64> {
    match lookup(fd) {
        Some((file, _)) => stat_file(&file),
        None => Ok(vfs::stat::Stat::from_bytes(&kernel_stat(fd)?)),
    }
}

/// The status of one of the server's files.
pub fn stat_file(file: &File) -> Result<vfs::stat::Stat, i64> {
    let bytes = match file {
        File::Pipe(end) => end.stat(),
        File::EventFd(e) => e.stat(),
        File::Tmp(f) => return Ok(f.inode.status()),
        File::Data(f) => crate::datafs::stat(&f.inode)?,
        File::Netlink(n) => n.stat(),
        File::Inotify(i) => i.stat(),
        File::Socket(s) => crate::sockcalls::stat(s),
        File::Inet(s) => crate::inetcalls::stat(s),
        File::Tty(t) => t.origin.stat()?,
        File::PtyMaster(m) => m.origin.stat()?,
        File::Dev(d) => d.origin.stat()?,
        File::Path(p) => p.origin.stat()?,
        File::Proc(p) => crate::procfs::stat(&p.node)?,
    };
    Ok(vfs::stat::Stat::from_bytes(&bytes))
}

/// The open /proc or /sys file behind descriptor `fd`, if it is one.
pub fn proc_of(fd: u64) -> Option<Arc<crate::procfile::ProcOpen>> {
    match lookup(fd)? {
        (File::Proc(f), _) => Some(f),
        _ => None,
    }
}

/// The `struct stat` of descriptor `fd`, one of the kernel's files (EBADF
/// for none), with the times the server keeps for it.
fn kernel_stat(fd: u64) -> Result<[u8; 144], i64> {
    let mut st = [0u8; 144];
    match syscall(SYS_KFD_STAT, [fd, st.as_mut_ptr() as u64, 0, 0, 0, 0]) {
        r if r < 0 => Err(-r),
        _ => {
            crate::namespace::pseudo_times(&mut st);
            Ok(st)
        }
    }
}

/// For mmap of descriptor `fd`: None for a file of the kernel's (mapped
/// through `kfile_object`); else the object to map (a handle to close once
/// mapped) and whether the mapping stays read-only, or why not.
pub fn map_object(fd: u64, shared: bool, prot_write: bool) -> Option<Result<Mapping, i64>> {
    const ENODEV: i64 = 19;
    let object = |r: Result<(u64, bool), i64>| r.map(|(h, ro)| Mapping::Object(h, ro));
    Some(match lookup(fd)? {
        (File::Tmp(f), flags) => object(f.map_object(flags, shared, prot_write)),
        (File::Data(f), flags) => object(f.map_object(flags, shared, prot_write)),
        (File::Dev(d), flags) => crate::devices::map(&d, flags, shared, prot_write),
        (File::Path(_), _) => Err(EBADF),
        _ => Err(ENODEV),
    })
}

/// The server's file behind descriptor `fd` and its open flags, or None
/// for a file of the kernel's (or a bad descriptor: the kernel answers).
pub fn lookup(fd: u64) -> Option<(File, u32)> {
    lookup_checked(fd).ok().flatten()
}

/// `lookup` for calls the kernel cannot answer: EBADF for a bad
/// descriptor, None for a file of the kernel's.
pub fn lookup_checked(fd: u64) -> Result<Option<(File, u32)>, i64> {
    let mut flags = 0u32;
    let id = syscall(SYS_KFD_LOOKUP, [fd, &mut flags as *mut u32 as u64, 0, 0, 0, 0]);
    if id < 0 {
        return Err(-id);
    }
    if id == 0 {
        return Ok(None);
    }
    Ok(FILES.lock().get(&(id as u64)).cloned().map(|f| (f, flags)))
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
        SYS_SENDFILE => {
            let (out, input) = (lookup(a0), lookup(a1));
            if out.is_none() && input.is_none() {
                return None;
            }
            sendfile(a0, out, a1, input, a2, s.r10)
        }
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
        | netlink::SYS_ACCEPT4 => match lookup(a0)? {
            (File::Netlink(n), flags) => netlink::call(s.rax, &n, flags, [a0, a1, a2, a3, s.r8, s.r9]),
            // AF_UNIX sockets and the answer for files that are none:
            // `sockcalls`.
            _ => return None,
        },
        // The kernel's files have nothing to write back: /data's do, in
        // every instance's page cache.
        SYS_SYNC => sync_everywhere().map(|_| 0),
        SYS_SYNCFS => match lookup(a0) {
            Some((File::Data(_), _)) => sync_everywhere().map(|_| 0),
            Some(_) => Ok(0),
            None => return None,
        },
        // The kernel's descriptor table holds the descriptor's flags and
        // close-on-exec bit: the requests on those (FIONBIO, FIONCLEX,
        // FIOCLEX) go there for the server's files, too. A pass-through
        // until the descriptor table moves into the server (R6e).
        SYS_IOCTL if matches!(a1, 0x5421 | 0x5450 | 0x5451) => return None,
        // The interface requests every socket takes, the kernel's too.
        SYS_IOCTL if crate::netdev::is_request(a1) => crate::netdev::ioctl(a0, a1, a2),
        SYS_READ | SYS_WRITE | SYS_READV | SYS_WRITEV | SYS_FSTAT | SYS_LSEEK | SYS_IOCTL | SYS_PREAD64 | SYS_PWRITE64
        | SYS_PREADV | SYS_PWRITEV | SYS_FSYNC | SYS_FDATASYNC | SYS_FTRUNCATE | SYS_GETDENTS64 | SYS_FSTATFS => match lookup(a0) {
            Some((file, flags)) => on_file(s.rax, file, flags, a1, a2, a3),
            // fstat of one of the kernel's files: its answer, with the
            // times the server keeps for it (`namespace::set_pseudo_times`).
            None if s.rax == SYS_FSTAT => kernel_stat(a0).and_then(|st| crate::usercopy::to_program(a1, &st)).map(|_| 0),
            None => return None,
        },
        SYS_PREADV2 | SYS_PWRITEV2 => {
            let (file, flags) = lookup(a0)?;
            rw2(s.rax == SYS_PWRITEV2, file, flags, a1, a2, a3 as i64, s.r9)
        }
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// preadv2/pwritev2: the vectored call at the file position (offset -1) or
/// at the offset, with O_APPEND as the flags have it for this call
/// (`vfs::rw::plan`). RWF_DSYNC/RWF_SYNC make a /data write durable before
/// it returns (the other files are memory: nothing to wait for).
fn rw2(write: bool, file: File, flags: u32, iov: u64, count: u64, offset: i64, rwf: u64) -> Result<i64, i64> {
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

fn on_file(nr: u64, file: File, flags: u32, a1: u64, a2: u64, a3: u64) -> Result<i64, i64> {
    let end = match file {
        File::Pipe(end) => end,
        File::EventFd(e) => return on_eventfd(nr, &e, flags, a1, a2),
        File::Tmp(f) => return tmpfile::call(nr, &f, flags, a1, a2, a3),
        File::Data(f) => return datafile::call(nr, &f, flags, a1, a2, a3),
        File::Netlink(n) => return netlink::call(nr, &n, flags, [0, a1, a2, a3, 0, 0]),
        File::Inotify(i) => return inotify::call(nr, &i, flags, a1, a2),
        File::Socket(s) => return crate::sockcalls::on_file(nr, &s, flags, a1, a2),
        File::Inet(s) => return crate::inetcalls::on_file(nr, &s, flags, a1, a2),
        File::Tty(t) => return crate::tty::call(nr, &t, flags, a1, a2),
        File::PtyMaster(m) => return crate::pty::call(nr, &m, flags, a1, a2),
        File::Dev(d) => return crate::devices::call(nr, &d, flags, a1, a2),
        File::Path(p) => return crate::pathfile::call(nr, &p, a1),
        File::Proc(p) => return crate::procfile::call(nr, &p, flags, a1, a2, a3),
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
    let rfd = install(rid, File::Pipe(read_end.clone()), common, read_end.readiness())?;
    let wfd = match install(wid, File::Pipe(write_end.clone()), common | O_WRONLY, write_end.readiness()) {
        Ok(fd) => fd,
        Err(e) => {
            syscall(SYS_KFD_CLOSE, [rfd as u64, 0, 0, 0, 0, 0]);
            return Err(e);
        }
    };
    if let Err(e) = crate::usercopy::write(fds, &[rfd as i32, wfd as i32]) {
        syscall(SYS_KFD_CLOSE, [rfd as u64, 0, 0, 0, 0, 0]);
        syscall(SYS_KFD_CLOSE, [wfd as u64, 0, 0, 0, 0, 0]);
        return Err(e);
    }
    Ok(0)
}

/// sendfile(out, in, offset, count) with a server file at either end (the
/// kernel does it between its own files): chunks through the server's
/// memory, returning after a short read, as Linux and the kernel do.
fn sendfile(out_fd: u64, out: Option<(File, u32)>, in_fd: u64, input: Option<(File, u32)>, offset: u64, count: u64) -> Result<i64, i64> {
    // An O_PATH descriptor is no file to move data through.
    let path = |f: &Option<(File, u32)>| matches!(f, Some((File::Path(_), _)));
    if path(&out) || path(&input) {
        return Err(EBADF);
    }
    if offset != 0 {
        return Err(EINVAL);
    }
    // An eventfd moves 8-byte values, not data; a directory none; a
    // netlink socket datagrams.
    let unfit = |f: &Option<(File, u32)>| matches!(f, Some((File::EventFd(_) | File::Netlink(_) | File::Inotify(_), _)));
    if unfit(&out) || unfit(&input) {
        return Err(EINVAL);
    }
    if let Some((File::Tmp(f), _)) = &out {
        if f.inode.is_dir() {
            return Err(EINVAL);
        }
    }
    if let Some((File::Data(f), _)) = &out {
        if f.inode.kind == vfs::S_IFDIR {
            return Err(EINVAL);
        }
    }
    if let Some((_, flags)) = &out {
        if flags & O_ACCMODE == 0 {
            return Err(EBADF);
        }
    }
    if let Some((_, flags)) = &input {
        if flags & O_ACCMODE == O_WRONLY {
            return Err(EBADF);
        }
    }
    let mut buf = alloc::vec![0u8; 4096];
    let mut total = 0u64;
    while total < count {
        let want = (count - total).min(buf.len() as u64) as usize;
        let n = match &input {
            Some((File::Pipe(end), flags)) => end.read(Dst::Server(&mut buf[..want]), flags & O_NONBLOCK != 0),
            Some((File::EventFd(_) | File::Netlink(_) | File::Inotify(_), _)) => Err(EINVAL),
            Some((File::Tmp(f), _)) => f.read_server(&mut buf[..want]).map(|n| n as i64),
            Some((File::Data(f), _)) => f.read_server(&mut buf[..want]).map(|n| n as i64),
            Some((File::Proc(f), _)) => f.read_server(&mut buf[..want]).map(|n| n as i64),
            Some((File::Socket(s), flags)) => crate::sockcalls::read_server(s, *flags, &mut buf[..want]),
            Some((File::Inet(s), flags)) => crate::inetcalls::read_server(s, *flags, &mut buf[..want]),
            Some((File::Tty(t), flags)) => {
                t.tty.read(t, crate::unix::Sink::Server { buf: &mut buf[..want], at: 0 }, flags & O_NONBLOCK != 0)
            }
            Some((File::PtyMaster(m), flags)) => m.read(crate::unix::Sink::Server { buf: &mut buf[..want], at: 0 }, flags & O_NONBLOCK != 0),
            Some((File::Path(_), _)) => Err(EBADF),
            Some((File::Dev(d), _)) => crate::devices::read(d, crate::unix::Sink::Server { buf: &mut buf[..want], at: 0 }),
            None => match syscall(SYS_KFD_READ, [in_fd, buf.as_mut_ptr() as u64, want as u64, 0, 0, 0]) {
                r if r < 0 => Err(-r),
                r => Ok(r),
            },
        };
        let n = match n {
            Ok(n) => n as usize,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let wrote = match &out {
            Some((File::Pipe(end), flags)) => end.write(Src::Server(&buf[..n]), flags & O_NONBLOCK != 0),
            Some((File::EventFd(_) | File::Netlink(_) | File::Inotify(_), _)) => Err(EINVAL),
            Some((File::Tmp(f), flags)) => f.write_server(&buf[..n], flags & O_APPEND != 0).map(|n| n as i64),
            Some((File::Data(f), flags)) => f.write_server(&buf[..n], flags & O_APPEND != 0).map(|n| n as i64),
            Some((File::Socket(s), flags)) => crate::sockcalls::write_server(s, *flags, &buf[..n]),
            Some((File::Inet(s), flags)) => crate::inetcalls::write_server(s, *flags, &buf[..n]),
            Some((File::Tty(t), flags)) => t.tty.write(t, crate::unix::Source::Server { buf: &buf[..n], at: 0 }, flags & O_NONBLOCK != 0),
            Some((File::PtyMaster(m), flags)) => m.write(crate::unix::Source::Server { buf: &buf[..n], at: 0 }, flags & O_NONBLOCK != 0),
            Some((File::Path(_) | File::Proc(_), _)) => Err(EBADF),
            Some((File::Dev(_), _)) => Ok(n as i64),
            None => match syscall(SYS_KFD_WRITE, [out_fd, buf.as_ptr() as u64, n as u64, 0, 0, 0]) {
                r if r < 0 => Err(-r),
                r => Ok(r),
            },
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

    fn position(&self) -> crate::sync::MutexGuard<'_, u64> {
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
/// not regular, EXDEV for one of the kernel's (another filesystem).
fn regular(fd: u64) -> Result<(Regular, u32), i64> {
    const EISDIR: i64 = 21;
    const EXDEV: i64 = 18;
    let Some((file, flags)) = lookup(fd) else {
        let st = stat_of(fd)?;
        return Err(match st.mode & vfs::S_IFMT {
            vfs::S_IFREG => EXDEV,
            vfs::S_IFDIR => EISDIR,
            _ => EINVAL,
        });
    };
    let kind = match &file {
        File::Path(_) => return Err(EBADF),
        File::Tmp(f) => f.inode.file_type(),
        File::Data(f) => f.inode.kind,
        _ => return Err(EINVAL),
    };
    match (kind, file) {
        (vfs::S_IFDIR, _) => Err(EISDIR),
        (vfs::S_IFREG, File::Tmp(f)) => Ok((Regular::Tmp(f), flags)),
        (vfs::S_IFREG, File::Data(f)) => Ok((Regular::Data(f), flags)),
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
        None => *input.position(),
    };
    let start_out = match given_out {
        Some(o) => o,
        None => *output.position(),
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
        None => *input.position() = start_in + done,
    }
    match given_out {
        Some(_) => crate::usercopy::write(off_out, &(start_out + done))?,
        None => *output.position() = start_out + done,
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
    install(e.id(), File::EventFd(e.clone()), open, e.ready())
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
