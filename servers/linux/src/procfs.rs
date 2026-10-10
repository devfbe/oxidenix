//! /proc and /sys in the server (I/O rings step 5, docs/design/linux-server.md
//! "/proc and /sys"). Two sources make one tree:
//!
//! - **procfs** (servers/procfs) serves the system-wide files (`stat`,
//!   `meminfo`, `cpuinfo`, `uptime`, `loadavg`, `counters`, `sys/kernel/*`,
//!   ...) and all of /sys over the instance's channel, in the file protocol
//!   (`fsring`, through `fsclient`): `LOOKUP`, `STAT`, `READDIR`, and
//!   `READ` of a file's contents, made at the moment, into the channel's
//!   scratch buffer. Nothing of it is cached here: procfs's inodes are
//!   numbers that name what a file is, stateless and without holds, so a
//!   procfs that restarted serves them as before (a new channel is made
//!   when the old one died).
//! - **The server** makes each process's own part: `/proc/<pid>` (its
//!   files, `task/<tid>`, `fd/<n>`, `cwd`, `root`, `exe`, `mounts`),
//!   `self`, `thread-self` and `mounts`, merged into procfs's root. The
//!   records come from the server's process table (`process::query`, R8,
//!   with the kernel's accounts of CPU time and memory); the descriptors of
//!   `fd` from the process's descriptor table (`fdtable`, R6e;
//!   `process::files_of`). The text formats are `procproto::render`'s,
//!   shared with procfs.
//!
//! **Magic links.** `/proc/<pid>/fd/<n>` reads as the path the descriptor
//! was opened by (or `pipe:[ino]`, `socket:[ino]`, `anon_inode:[...]`), and
//! following it leads to the open file itself (`follow`), as Linux's
//! `proc_fd_link` does: the file's node (a tmpfs or /data inode, unlinked
//! or not; a terminal's node; a pipe or socket as `ProcNode::Open`), so
//! open(2) of it opens that file again (a pipe gets a new end, `pipe::reopen`;
//! a socket or an anonymous file is ENXIO, as on Linux). Every process's
//! descriptors are shown (everyone is root: Linux's ptrace check lets
//! root see them).

use crate::files::{self, File, FileRef};
use crate::fsclient::Client;
use crate::namespace::{self, Node};
use crate::sync::Mutex;
use crate::syscall;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use fsring::{Completion, Request};
use procproto::render::{self as fmt, Machine};
use procproto::*;
use restricted::*;

/// st_dev of /proc's and /sys's files (anonymous devices 0:21 and 0:20).
pub const PROC_DEV: u64 = 0x15;
pub const SYS_DEV: u64 = 0x14;
/// statfs's f_type of each (Linux's PROC_SUPER_MAGIC, SYSFS_MAGIC).
const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const SYSFS_MAGIC: u64 = 0x6265_6572;
/// Pages of the channel's scratch buffer: a generated file is read in
/// pieces of this size (procfs's files are a few KiB).
const SCRATCH_PAGES: u64 = 16;
/// The largest file or directory taken from procfs (a bound on a hostile
/// or broken service).
const MAX_REMOTE: usize = 1 << 20;

const ENOENT: i64 = 2;
const ESRCH: i64 = 3;
const EIO: i64 = 5;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EINVAL: i64 = 22;
const ERANGE: i64 = 34;

const S_IFDIR: u32 = vfs::S_IFDIR;
const S_IFREG: u32 = vfs::S_IFREG;
const S_IFLNK: u32 = vfs::S_IFLNK;

/// Directory entry types (getdents64's d_type).
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

/// The files of a process's directory (and, the first five, of a
/// thread's, `task/<tid>`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PidFile {
    Dir = 0,
    Stat,
    Statm,
    Status,
    Cmdline,
    Comm,
    Exe,
    Task,
    Mounts,
    Fd,
    Cwd,
    Root,
}

const PID_FILES: &[(&str, PidFile)] = &[
    ("stat", PidFile::Stat),
    ("statm", PidFile::Statm),
    ("status", PidFile::Status),
    ("cmdline", PidFile::Cmdline),
    ("comm", PidFile::Comm),
    ("exe", PidFile::Exe),
    ("task", PidFile::Task),
    ("mounts", PidFile::Mounts),
    ("fd", PidFile::Fd),
    ("cwd", PidFile::Cwd),
    ("root", PidFile::Root),
];
const TASK_FILES: &[(&str, PidFile)] = &[
    ("stat", PidFile::Stat),
    ("statm", PidFile::Statm),
    ("status", PidFile::Status),
    ("cmdline", PidFile::Cmdline),
    ("comm", PidFile::Comm),
];

/// A node of /proc or /sys.
#[derive(Clone)]
pub enum ProcNode {
    /// One of procfs's: in /sys (`sys`) or /proc's system-wide part, with
    /// the mode its `LOOKUP` reported.
    Remote { sys: bool, ino: u32, mode: u32 },
    /// /proc/self, /proc/thread-self, /proc/mounts: symlinks.
    SelfLink,
    ThreadSelf,
    MountsLink,
    /// /proc/<pid> (a process's or a thread's id) and its files.
    Pid { pid: u64, file: PidFile },
    /// /proc/<pid>/task/<tid> and its files.
    Task { pid: u64, tid: u64, file: PidFile },
    /// /proc/<pid>/fd/<fd>: a magic link.
    Fd { pid: u64, fd: u32 },
    /// The open file a magic link led to that is no filesystem's node (a
    /// pipe, a socket, an eventfd, an epoll instance).
    Open(Opened),
}

/// An open file without a node, as a magic link reached it. It keeps
/// nothing of the file alive that its last close should end (an O_PATH
/// descriptor of it may outlive every real one): a pipe is the pipe itself
/// (its buffer, no end: what reopening needs), anything else (a socket, an
/// eventfd, inotify, an epoll instance) its status when it was reached.
#[derive(Clone)]
pub enum Opened {
    Pipe(crate::pipe::Pipe),
    Anonymous([u8; 144]),
}

/// What a magic link leads to (`follow`): the node and its mode, and the
/// path the file was opened by, if it has one.
pub struct Follow {
    pub node: Node,
    pub mode: u32,
    pub path: Option<String>,
}

// ------------------------------------------------------------- procfs

static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);
/// One reconnection at a time (held across the channel's offer: a sleeping lock).
static RECONNECT: crate::sync::SleepLock = crate::sync::SleepLock::new(());

/// The instance's channel to procfs, a new one if the old died (procfs is
/// started again if it did).
fn client() -> Result<Arc<Client>, i64> {
    if let Some(c) = CLIENT.lock().clone().filter(|c| !c.is_dead()) {
        return Ok(c);
    }
    let _one = RECONNECT.lock()?;
    if let Some(c) = CLIENT.lock().clone().filter(|c| !c.is_dead()) {
        return Ok(c);
    }
    let c = Arc::new(Client::connect(procproto::SERVICE, 0, SCRATCH_PAGES).map_err(|_| EIO)?);
    *CLIENT.lock() = Some(c.clone());
    Ok(c)
}

/// Sends `r` and waits for its completion; its error as one.
fn call(c: &Client, r: Request) -> Result<Completion, i64> {
    let done = c.call(r.encode(0)).map_err(|_| EIO)?;
    if done.status < 0 {
        return Err(-done.status);
    }
    Ok(done)
}

/// procfs's handle of inode `ino` (its inodes all have generation 0).
fn proc_node(ino: u32) -> fsring::Node {
    fsring::Node::new(ino, 0)
}

fn remote_lookup(sys: bool, dir: u32, name: &str) -> Result<ProcNode, i64> {
    if name.is_empty() || name.len() > vfs::NAME_MAX {
        return Err(ENOENT);
    }
    let c = client()?;
    let scratch = c.scratch(1);
    scratch.put(0, name.as_bytes());
    let done = call(&c, Request::Lookup { dir: proc_node(dir), name: scratch.buf(0, name.len() as u64) })?;
    let ino = u32::try_from(done.values[0]).map_err(|_| EIO)?;
    let mode = done.values[1] as u32;
    if !matches!(mode & vfs::S_IFMT, S_IFDIR | S_IFREG) {
        return Err(EIO);
    }
    Ok(ProcNode::Remote { sys, ino, mode })
}

fn remote_stat(ino: u32) -> Result<fsring::Stat, i64> {
    let c = client()?;
    Ok(fsring::Stat::from_values(&call(&c, Request::Stat { ino: proc_node(ino) })?.values))
}

/// A file's contents, as procfs makes them now: one `READ` into the
/// scratch buffer for what fits (all of procfs's files), more for the rest.
fn remote_read(ino: u32) -> Result<Vec<u8>, i64> {
    let c = client()?;
    let scratch = c.scratch(c.scratch_pages());
    let mut data = Vec::new();
    loop {
        let len = scratch.len();
        let done = call(&c, Request::Read { ino: proc_node(ino), offset: data.len() as u64, buf: scratch.buf(0, len) })?;
        let n = done.status as u64;
        if n > len {
            return Err(EIO);
        }
        data.extend_from_slice(&scratch.get(0, n));
        let size = done.values[0] as usize;
        if n < len || data.len() >= size {
            return Ok(data);
        }
        if data.len() > MAX_REMOTE {
            return Err(EIO);
        }
    }
}

/// A directory's entries (`.` and `..` included) as (name, inode, d_type).
fn remote_readdir(dir: u32) -> Result<Vec<(String, u64, u8)>, i64> {
    let c = client()?;
    let scratch = c.scratch(1);
    let mut out = Vec::new();
    let mut cursor = 0;
    loop {
        let len = scratch.len();
        let done = call(&c, Request::Readdir { dir: proc_node(dir), cursor, buf: scratch.buf(0, len) })?;
        let n = done.status as u64;
        if n > len {
            return Err(EIO);
        }
        for (ino, kind, name) in fsring::dirents(&scratch.get(0, n)) {
            let dtype = match kind {
                fsring::TYPE_DIR => DT_DIR,
                fsring::TYPE_SYMLINK => DT_LNK,
                _ => DT_REG,
            };
            out.push((String::from_utf8_lossy(name).into_owned(), ino as u64, dtype));
        }
        cursor = done.values[0];
        if cursor == 0 {
            return Ok(out);
        }
        if out.len() > 4096 || n == 0 {
            return Err(EIO);
        }
    }
}

// ------------------------------------------------- the kernel's records

/// A record (`procproto`'s queries): the processes' from the server's own table
/// (`process::query`), the system's from the kernel (`SYS_SYSTEM_INFO`).
fn proc_info(op: u64, arg: u64, cap: usize) -> Result<Vec<u8>, i64> {
    if op != QUERY_SYSTEM {
        return crate::process::query(op, arg, cap);
    }
    let mut buf = alloc::vec![0u8; cap];
    let n = syscall(SYS_SYSTEM_INFO, [op, arg, buf.as_mut_ptr() as u64, cap as u64, 0, 0]);
    if n < 0 {
        return Err(-n);
    }
    buf.truncate(n as usize);
    Ok(buf)
}

/// A list of ids (`QUERY_PIDS`, `QUERY_THREADS`), all of them.
fn ids(op: u64, arg: u64) -> Result<Vec<u64>, i64> {
    // Sized by what comes back: a small buffer first, doubled while full.
    let mut cap = 64;
    loop {
        let bytes = proc_info(op, arg, cap * 8)?;
        if bytes.len() < cap * 8 || cap >= 1 << 16 {
            return Ok(bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes"))).collect());
        }
        cap *= 2;
    }
}

/// A text record (`QUERY_CMDLINE`, `QUERY_EXE`) of process `pid`, in a
/// buffer sized by probing: 256 bytes first, doubled on ERANGE up to 128
/// KiB (Linux's ARG_MAX for a command line). ENOENT for no process.
fn text(op: u64, pid: u64) -> Result<Vec<u8>, i64> {
    let mut cap = 256;
    loop {
        match proc_info(op, pid, cap) {
            Err(ERANGE) if cap < 128 * 1024 => cap *= 2,
            Err(ESRCH) => return Err(ENOENT),
            other => return other,
        }
    }
}

/// What the server knows of process `p` beyond its record: its session's
/// controlling terminal, and its umask.
fn linux(p: &Process) -> fmt::Linux {
    let (tty_nr, tpgid) = crate::tty::proc_fields(p.sid);
    let umask = crate::process::umask_of(p.tgid as u32);
    fmt::Linux { tty_nr, tpgid, umask }
}

/// The process (or thread) `pid`: ENOENT if there is none. The kernel's
/// own task (0) is no process of /proc.
fn process(pid: u64) -> Result<Process, i64> {
    if pid == 0 {
        return Err(ENOENT);
    }
    let bytes = proc_info(QUERY_PROCESS, pid, core::mem::size_of::<Process>()).map_err(|e| if e == ESRCH { ENOENT } else { e })?;
    from_bytes(&bytes).ok_or(EIO)
}

/// The kernel's tick rate, page size and CPUs (fixed after boot).
/// The kernel's record of the system now (`SYS_SYSTEM_INFO`).
pub fn system() -> Option<System> {
    proc_info(QUERY_SYSTEM, 0, core::mem::size_of::<System>()).ok().and_then(|b| from_bytes::<System>(&b))
}

fn machine() -> Machine {
    static HZ: AtomicU64 = AtomicU64::new(0);
    static PAGE_SIZE: AtomicU64 = AtomicU64::new(0);
    static CPUS: AtomicU64 = AtomicU64::new(0);
    if HZ.load(Ordering::Acquire) == 0 {
        if let Some(s) = proc_info(QUERY_SYSTEM, 0, core::mem::size_of::<System>()).ok().and_then(|b| from_bytes::<System>(&b)) {
            let m = Machine::of(&s);
            PAGE_SIZE.store(m.page_size, Ordering::Relaxed);
            CPUS.store(m.cpus, Ordering::Relaxed);
            HZ.store(m.hz, Ordering::Release);
        }
    }
    Machine { hz: HZ.load(Ordering::Acquire).max(1), page_size: PAGE_SIZE.load(Ordering::Relaxed).max(4096), cpus: CPUS.load(Ordering::Relaxed).max(1) }
}

/// The calling thread's process and thread ids.
fn caller() -> (u64, u64) {
    (crate::local::pid() as u64, crate::local::tid() as u64)
}

/// The descriptor table of process (or thread) `pid`: ENOENT for none (a zombie has none).
fn table(pid: u64) -> Result<Arc<crate::fdtable::FilesContext>, i64> {
    let tgid = process(pid)?.tgid;
    if tgid == caller().0 {
        return Ok(crate::fdtable::current());
    }
    crate::process::files_of(tgid as u32).ok_or(ENOENT)
}

/// The description behind descriptor `fd` of process `pid` (ENOENT for none).
fn fd_file(pid: u64, fd: u32) -> Result<FileRef, i64> {
    table(pid)?.get(fd as u64).map_err(|_| ENOENT)
}

// ------------------------------------------------------------ the tree

/// The root of /proc (`sys` false) or /sys.
pub fn root(sys: bool) -> ProcNode {
    ProcNode::Remote { sys, ino: if sys { SYSFS_ROOT } else { PROC_ROOT }, mode: S_IFDIR | 0o555 }
}

/// A name of a process or thread id: digits without a leading zero.
fn id_name(name: &str) -> Option<u64> {
    if name.is_empty() || (name.len() > 1 && name.starts_with('0')) || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

fn pid_mode(file: PidFile) -> u32 {
    match file {
        PidFile::Dir | PidFile::Task => S_IFDIR | 0o555,
        PidFile::Fd => S_IFDIR | 0o500,
        PidFile::Exe | PidFile::Cwd | PidFile::Root => S_IFLNK | 0o777,
        _ => S_IFREG | 0o444,
    }
}

/// A node's mode (of an open file: its status's).
pub fn mode(node: &ProcNode) -> u32 {
    match node {
        ProcNode::Remote { mode, .. } => *mode,
        ProcNode::SelfLink | ProcNode::ThreadSelf | ProcNode::MountsLink => S_IFLNK | 0o777,
        ProcNode::Pid { file, .. } | ProcNode::Task { file, .. } => pid_mode(*file),
        ProcNode::Fd { .. } => S_IFLNK | 0o700,
        ProcNode::Open(_) => stat(node).map_or(0, |st| namespace::mode_of(&st)),
    }
}

pub fn is_dir(node: &ProcNode) -> bool {
    mode(node) & vfs::S_IFMT == S_IFDIR
}

/// Whether the node (still) exists: its process, its descriptor.
fn check(node: &ProcNode) -> Result<(), i64> {
    match node {
        ProcNode::Pid { pid, .. } => process(*pid).map(|_| ()),
        ProcNode::Task { pid, tid, .. } => {
            if process(*tid)?.tgid != process(*pid)?.tgid {
                return Err(ENOENT);
            }
            Ok(())
        }
        ProcNode::Fd { pid, fd } => fd_file(*pid, *fd).map(|_| ()),
        _ => Ok(()),
    }
}

/// Whether the caller may open the node (it exists; a process's `fd` directory while the
/// process has a descriptor table).
pub fn may_open(node: &ProcNode) -> Result<(), i64> {
    match node {
        ProcNode::Pid { pid, file: PidFile::Fd } => table(*pid).map(|_| ()),
        _ => check(node),
    }
}

/// `name` in the directory `dir`.
pub fn lookup(dir: &ProcNode, name: &str) -> Result<ProcNode, i64> {
    match dir {
        ProcNode::Remote { sys: false, ino: PROC_ROOT, .. } => match name {
            "self" => Ok(ProcNode::SelfLink),
            "thread-self" => Ok(ProcNode::ThreadSelf),
            "mounts" => Ok(ProcNode::MountsLink),
            _ => match id_name(name) {
                // Any thread's id has a directory, as on Linux, though the
                // listing shows processes only.
                Some(pid) => process(pid).map(|_| ProcNode::Pid { pid, file: PidFile::Dir }),
                None => remote_lookup(false, PROC_ROOT, name),
            },
        },
        ProcNode::Remote { sys, ino, mode } if mode & vfs::S_IFMT == S_IFDIR => remote_lookup(*sys, *ino, name),
        ProcNode::Pid { pid, file: PidFile::Dir } => {
            process(*pid)?;
            let file = PID_FILES.iter().find(|(n, _)| *n == name).map(|&(_, f)| f).ok_or(ENOENT)?;
            Ok(ProcNode::Pid { pid: *pid, file })
        }
        ProcNode::Pid { pid, file: PidFile::Task } => {
            let tid = id_name(name).ok_or(ENOENT)?;
            if !ids(QUERY_THREADS, *pid).map_err(|_| ENOENT)?.contains(&tid) {
                return Err(ENOENT);
            }
            Ok(ProcNode::Task { pid: *pid, tid, file: PidFile::Dir })
        }
        ProcNode::Task { pid, tid, file: PidFile::Dir } => {
            check(dir)?;
            let file = TASK_FILES.iter().find(|(n, _)| *n == name).map(|&(_, f)| f).ok_or(ENOENT)?;
            Ok(ProcNode::Task { pid: *pid, tid: *tid, file })
        }
        ProcNode::Pid { pid, file: PidFile::Fd } => {
            let fd = id_name(name).and_then(|n| u32::try_from(n).ok()).ok_or(ENOENT)?;
            fd_file(*pid, fd)?;
            Ok(ProcNode::Fd { pid: *pid, fd })
        }
        _ => Err(ENOTDIR),
    }
}

/// A node's inode number: procfs's below 1024, the server's above.
fn ino(node: &ProcNode) -> u64 {
    match node {
        ProcNode::Remote { ino, .. } => *ino as u64,
        ProcNode::SelfLink => 0x1_0001,
        ProcNode::ThreadSelf => 0x1_0002,
        ProcNode::MountsLink => 0x1_0003,
        ProcNode::Pid { pid, file } => (pid + 1) << 20 | *file as u64,
        ProcNode::Task { tid, file, .. } => (tid + 1) << 20 | 1 << 12 | *file as u64,
        ProcNode::Fd { pid, fd } => (pid + 1) << 20 | 2 << 12 | *fd as u64,
        ProcNode::Open(_) => 0,
    }
}

fn dtype(mode: u32) -> u8 {
    match mode & vfs::S_IFMT {
        S_IFDIR => DT_DIR,
        S_IFLNK => DT_LNK,
        _ => DT_REG,
    }
}

/// A directory's entries (`.` and `..` first) as (name, inode, d_type).
pub fn list(dir: &ProcNode) -> Result<Vec<(String, u64, u8)>, i64> {
    let entry = |name: &str, node: &ProcNode| (String::from(name), ino(node), dtype(mode(node)));
    match dir {
        ProcNode::Remote { sys: false, ino: PROC_ROOT, .. } => {
            // Without procfs, the processes are still there.
            let mut out = remote_readdir(PROC_ROOT).unwrap_or_else(|_| alloc::vec![entry(".", dir), entry("..", dir)]);
            for (name, node) in [("self", ProcNode::SelfLink), ("thread-self", ProcNode::ThreadSelf), ("mounts", ProcNode::MountsLink)] {
                out.push(entry(name, &node));
            }
            for pid in ids(QUERY_PIDS, 0)?.into_iter().filter(|&p| p != 0) {
                out.push(entry(&format!("{pid}"), &ProcNode::Pid { pid, file: PidFile::Dir }));
            }
            Ok(out)
        }
        ProcNode::Remote { ino, mode, .. } if mode & vfs::S_IFMT == S_IFDIR => remote_readdir(*ino),
        ProcNode::Pid { pid, file: PidFile::Dir } => {
            process(*pid)?;
            let mut out = alloc::vec![entry(".", dir), entry("..", &root(false))];
            out.extend(PID_FILES.iter().map(|&(n, file)| entry(n, &ProcNode::Pid { pid: *pid, file })));
            Ok(out)
        }
        ProcNode::Pid { pid, file: PidFile::Task } => {
            let mut out = alloc::vec![entry(".", dir), entry("..", &ProcNode::Pid { pid: *pid, file: PidFile::Dir })];
            for tid in ids(QUERY_THREADS, *pid).map_err(|_| ENOENT)? {
                out.push(entry(&format!("{tid}"), &ProcNode::Task { pid: *pid, tid, file: PidFile::Dir }));
            }
            Ok(out)
        }
        ProcNode::Task { pid, tid, file: PidFile::Dir } => {
            check(dir)?;
            let mut out = alloc::vec![entry(".", dir), entry("..", &ProcNode::Pid { pid: *pid, file: PidFile::Task })];
            out.extend(TASK_FILES.iter().map(|&(n, file)| entry(n, &ProcNode::Task { pid: *pid, tid: *tid, file })));
            Ok(out)
        }
        ProcNode::Pid { pid, file: PidFile::Fd } => {
            let fds = table(*pid)?.open_fds();
            let mut out = alloc::vec![entry(".", dir), entry("..", &ProcNode::Pid { pid: *pid, file: PidFile::Dir })];
            for fd in fds {
                out.push(entry(&format!("{fd}"), &ProcNode::Fd { pid: *pid, fd }));
            }
            Ok(out)
        }
        _ => Err(ENOTDIR),
    }
}

/// A node's `struct stat`, with the times set on it (`namespace::set_pseudo_times`).
pub fn stat(node: &ProcNode) -> Result<[u8; 144], i64> {
    let now = crate::time::realtime();
    let mut st = match node {
        ProcNode::Open(Opened::Pipe(p)) => return Ok(p.stat()),
        ProcNode::Open(Opened::Anonymous(st)) => return Ok(*st),
        ProcNode::Remote { sys, ino, .. } => {
            let s = remote_stat(*ino)?;
            let time = |sec: u32| vfs::stat::Time { sec: sec as i64, nsec: 0 };
            vfs::stat::Stat {
                dev: if *sys { SYS_DEV } else { PROC_DEV },
                ino: *ino as u64,
                nlink: s.links as u64,
                mode: s.mode,
                size: s.size,
                blksize: 4096,
                atime: time(s.atime),
                mtime: time(s.mtime),
                ctime: time(s.ctime),
                ..Default::default()
            }
            .to_bytes()
        }
        _ => {
            check(node)?;
            let mode = mode(node);
            let nlink = if mode & vfs::S_IFMT == S_IFDIR { 2 } else { 1 };
            vfs::stat::Stat { dev: PROC_DEV, ino: ino(node), nlink, mode, blksize: 1024, atime: now, mtime: now, ctime: now, ..Default::default() }.to_bytes()
        }
    };
    namespace::pseudo_times(&mut st);
    Ok(st)
}

/// The target of one of the symlinks.
pub fn readlink(node: &ProcNode) -> Result<String, i64> {
    match node {
        ProcNode::SelfLink => Ok(format!("{}", caller().0)),
        ProcNode::ThreadSelf => {
            let (pid, tid) = caller();
            Ok(format!("{pid}/task/{tid}"))
        }
        ProcNode::MountsLink => Ok(String::from("self/mounts")),
        ProcNode::Pid { pid, file: PidFile::Exe } => {
            let exe = text(QUERY_EXE, *pid).map_err(|e| if e == ERANGE { ENOENT } else { e })?;
            if exe.is_empty() {
                return Err(ENOENT);
            }
            String::from_utf8(exe).map_err(|_| ENOENT)
        }
        ProcNode::Pid { pid, file: PidFile::Cwd } => crate::process::cwd_of(process(*pid)?.tgid as u32).ok_or(ENOENT),
        ProcNode::Pid { pid, file: PidFile::Root } => {
            process(*pid)?;
            Ok(String::from("/"))
        }
        ProcNode::Fd { pid, fd } => describe(&fd_file(*pid, *fd)?),
        _ => Err(EINVAL),
    }
}

/// What /proc/<pid>/fd/<fd> reads as: the path the file was opened by
/// (" (deleted)" after it, for one unlinked meanwhile), or its kind and
/// inode for a file without a path.
fn describe(f: &FileRef) -> Result<String, i64> {
    let anon = |kind: &str| Ok(format!("anon_inode:{kind}"));
    let ino = |st: Result<vfs::stat::Stat, i64>| st.map(|s| s.ino).unwrap_or(0);
    match &f.file {
        File::Tmp(t) => Ok(if t.inode.removed() { format!("{} (deleted)", t.path) } else { t.path.clone() }),
        File::Data(d) => Ok(if d.inode.unlinked() { format!("{} (deleted)", d.path) } else { d.path.clone() }),
        File::Proc(p) => Ok(p.path.clone()),
        File::Pipe(end) => Ok(format!("pipe:[{}]", end.ino())),
        File::Socket(_) | File::Inet(_) | File::Netlink(_) => Ok(format!("socket:[{}]", ino(files::stat_file(&f.file)))),
        File::EventFd(_) => anon("[eventfd]"),
        File::Inotify(_) => anon("inotify"),
        File::Epoll(_) => anon("[eventpoll]"),
        File::Tty(_) | File::PtyMaster(_) | File::Dev(_) | File::Path(_) => files::origin_of_file(f).map(|o| o.path).ok_or(ENOENT),
    }
}

/// Where a magic link leads (None: an ordinary symlink, read with
/// `readlink`).
pub fn follow(node: &ProcNode) -> Result<Option<Follow>, i64> {
    let ProcNode::Fd { pid, fd } = node else { return Ok(None) };
    let f = fd_file(*pid, *fd)?;
    let open = |file: File| -> Result<Follow, i64> {
        let st = files::stat_file(&file)?;
        let opened = match &file {
            File::Pipe(end) => Opened::Pipe(end.pipe()),
            _ => Opened::Anonymous(st.to_bytes()),
        };
        Ok(Follow { node: Node::Proc(ProcNode::Open(opened)), mode: st.mode, path: None })
    };
    let found = match &f.file {
        File::Tmp(t) => Follow { mode: t.inode.mode(), node: Node::Tmp(t.inode.clone()), path: Some(t.path.clone()) },
        File::Data(d) => Follow { mode: d.inode.kind | 0o777, node: Node::Data(d.inode.clone()), path: Some(d.path.clone()) },
        File::Proc(p) => Follow { mode: mode(&p.node), node: Node::Proc(p.node.clone()), path: Some(p.path.clone()) },
        file @ (File::Pipe(_) | File::Socket(_) | File::Inet(_) | File::Netlink(_) | File::EventFd(_) | File::Inotify(_) | File::Epoll(_)) => open(file.clone())?,
        File::Tty(_) | File::PtyMaster(_) | File::Dev(_) | File::Path(_) => {
            let o = files::origin_of_file(&f).ok_or(ENOENT)?;
            let node = o.node()?;
            Follow { mode: namespace::mode_of(&node.stat()?), node, path: Some(o.path) }
        }
    };
    Ok(Some(found))
}

/// A file's contents, made now.
pub fn contents(node: &ProcNode) -> Result<Vec<u8>, i64> {
    let (id, file) = match node {
        ProcNode::Remote { ino, mode, .. } if mode & vfs::S_IFMT == S_IFREG => return remote_read(*ino),
        ProcNode::Pid { pid, file } => (*pid, *file),
        ProcNode::Task { tid, file, .. } => {
            check(node)?;
            (*tid, *file)
        }
        _ if is_dir(node) => return Err(EISDIR),
        _ => return Err(EINVAL),
    };
    let p = process(id)?;
    let text = match file {
        PidFile::Stat => fmt::pid_stat(&p, &machine(), &linux(&p)),
        PidFile::Statm => fmt::pid_statm(&p),
        PidFile::Status => fmt::pid_status(&p, &machine(), &linux(&p)),
        PidFile::Cmdline => return text(QUERY_CMDLINE, id),
        PidFile::Comm => format!("{}\n", fmt::name(&p)),
        PidFile::Mounts => namespace::mounts_text(),
        PidFile::Dir | PidFile::Task | PidFile::Fd => return Err(EISDIR),
        PidFile::Exe | PidFile::Cwd | PidFile::Root => return Err(EINVAL),
    };
    Ok(text.into_bytes())
}

/// The `struct statfs` (120 bytes) of /proc or /sys, as Linux's: no blocks,
/// no inodes counted.
pub fn statfs(node: &ProcNode) -> [u8; 120] {
    let sys = matches!(node, ProcNode::Remote { sys: true, .. });
    let words: [u64; 15] = [if sys { SYSFS_MAGIC } else { PROC_SUPER_MAGIC }, 4096, 0, 0, 0, 0, 0, 0, vfs::NAME_MAX as u64, 4096, 0, 0, 0, 0, 0];
    let mut out = [0u8; 120];
    for (i, w) in words.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// Whether two nodes are of one filesystem (both of /proc or both of
/// /sys).
pub fn same_fs(a: &ProcNode, b: &ProcNode) -> bool {
    let sys = |n: &ProcNode| matches!(n, ProcNode::Remote { sys: true, .. });
    sys(a) == sys(b)
}
