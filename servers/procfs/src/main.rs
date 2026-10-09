//! procfs: Linux's /proc, served from user space. The kernel only offers
//! its native process and system information (proc_query, `procproto`);
//! this server renders it in the formats Linux programs parse. It is the
//! Linux personality of the system: a non-Linux userland would not need it.
//!
//! The same server provides the parts of /sys that programs read (the CPU
//! list), mounted as a second tree with its own root.
//!
//! Inode numbers encode what a file is: below 1024 the global files of
//! /proc and /sys, else `(pid + 16) * 64 + kind`. Contents are generated on
//! every read, so they are always current; like Linux, files report size 0.

#![no_std]
#![no_main]

extern crate alloc;

mod render;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use fsproto::*;
use procproto::*;

oxrt::entry!(main);

const EINVAL: i64 = 22;
const ENOENT: i64 = 2;
const ENOSYS: i64 = 38;
const EROFS: i64 = 30;

const S_IFDIR: u64 = 0o040000;
const S_IFREG: u64 = 0o100000;
const S_IFLNK: u64 = 0o120000;

/// Global inodes.
const ROOT: u32 = 1;
const STAT: u32 = 2;
const MEMINFO: u32 = 3;
const LOADAVG: u32 = 4;
const UPTIME: u32 = 5;
const CPUINFO: u32 = 6;
const MOUNTS: u32 = 7;
const SELF: u32 = 8;
const VERSION: u32 = 9;
const SYS: u32 = 10;
const SYS_KERNEL: u32 = 11;
const PID_MAX: u32 = 12;
const FILESYSTEMS: u32 = 13;
const OSTYPE: u32 = 14;
const OSRELEASE: u32 = 15;
const COUNTERS: u32 = 16;

/// /sys: devices/system/cpu with online, possible, present, kernel_max and
/// one directory per CPU (with its `online` file).
const SYS_DEVICES: u32 = 101;
const SYS_SYSTEM: u32 = 102;
const SYS_CPU: u32 = 103;
const CPU_ONLINE: u32 = 104;
const CPU_POSSIBLE: u32 = 105;
const CPU_PRESENT: u32 = 106;
const CPU_KERNEL_MAX: u32 = 107;
const CPU_DIR: u32 = 200;
const CPU_DIR_ONLINE: u32 = 300;

/// Kinds of per-process inodes.
const P_DIR: u32 = 0;
const P_STAT: u32 = 1;
const P_STATM: u32 = 2;
const P_STATUS: u32 = 3;
const P_CMDLINE: u32 = 4;
const P_COMM: u32 = 5;
const P_EXE: u32 = 6;
const P_TASK: u32 = 7;
/// task/<pid> (the only thread) and its files (P_STAT..P_COMM + offset).
const P_THREAD: u32 = 8;
const THREAD_FILES: u32 = 8;
/// counters (oxidenix's own): the process's share of /proc/counters.
const P_COUNTERS: u32 = P_THREAD + THREAD_FILES + 1;

const PER_PID: u32 = 64;
const PID_BASE: u32 = 16;

/// What an inode is.
#[derive(Clone, Copy, PartialEq)]
enum Node {
    Global(u32),
    Process(u64, u32),
}

fn decode(ino: u32) -> Node {
    if ino < PID_BASE * PER_PID {
        Node::Global(ino)
    } else {
        Node::Process((ino / PER_PID - PID_BASE) as u64, ino % PER_PID)
    }
}

fn pid_ino(pid: u64, kind: u32) -> u32 {
    (pid as u32 + PID_BASE) * PER_PID + kind
}

fn cpus() -> u32 {
    (system().cpus as u32).max(1)
}

/// "0-3" (or "0" for one CPU).
fn cpu_range() -> String {
    match cpus() {
        1 => String::from("0\n"),
        n => alloc::format!("0-{}\n", n - 1),
    }
}

/// Kinds inside task/<pid> map onto the process's own files.
fn thread_file(kind: u32) -> Option<u32> {
    (kind > P_THREAD && kind <= P_THREAD + THREAD_FILES).then(|| kind - P_THREAD)
}

const GLOBAL_FILES: &[(&str, u32)] = &[
    ("stat", STAT),
    ("meminfo", MEMINFO),
    ("loadavg", LOADAVG),
    ("uptime", UPTIME),
    ("cpuinfo", CPUINFO),
    ("mounts", MOUNTS),
    ("self", SELF),
    ("version", VERSION),
    ("sys", SYS),
    ("filesystems", FILESYSTEMS),
    ("counters", COUNTERS),
];

const PROCESS_FILES: &[(&str, u32)] = &[
    ("stat", P_STAT),
    ("statm", P_STATM),
    ("status", P_STATUS),
    ("cmdline", P_CMDLINE),
    ("comm", P_COMM),
    ("exe", P_EXE),
    ("task", P_TASK),
    ("counters", P_COUNTERS),
];

pub fn system() -> System {
    let mut buf = [0u8; core::mem::size_of::<System>()];
    oxrt::proc_query(QUERY_SYSTEM, 0, &mut buf).ok().and_then(|_| from_bytes(&buf)).unwrap_or_default()
}

pub fn process(pid: u64) -> Option<Process> {
    let mut buf = [0u8; core::mem::size_of::<Process>()];
    oxrt::proc_query(QUERY_PROCESS, pid, &mut buf).ok()?;
    from_bytes(&buf)
}

pub fn query_text(op: u64, pid: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 8192];
    let n = oxrt::proc_query(op, pid, &mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Processes /proc shows: everything but the kernel task (pid 0).
pub fn pids() -> Vec<u64> {
    let mut buf = vec![0u8; 8 * 1024];
    let n = oxrt::proc_query(QUERY_PIDS, 0, &mut buf).unwrap_or(0);
    buf[..n].chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).filter(|&p| p != 0).collect()
}

fn exists(node: Node) -> bool {
    match node {
        Node::Global(ino) => {
            (ROOT..=COUNTERS).contains(&ino)
                || (SYSFS_ROOT..=CPU_KERNEL_MAX).contains(&ino)
                || (CPU_DIR..CPU_DIR + cpus()).contains(&ino)
                || (CPU_DIR_ONLINE..CPU_DIR_ONLINE + cpus()).contains(&ino)
        }
        Node::Process(pid, kind) => {
            pid != 0 && (kind <= P_THREAD || kind == P_COUNTERS || thread_file(kind).is_some()) && process(pid).is_some()
        }
    }
}

/// (mode, links) of an inode.
fn mode(node: Node) -> (u64, u64) {
    match node {
        Node::Global(ROOT | SYS | SYS_KERNEL | SYSFS_ROOT | SYS_DEVICES | SYS_SYSTEM | SYS_CPU) => (S_IFDIR | 0o555, 2),
        Node::Global(ino) if (CPU_DIR..CPU_DIR + 100).contains(&ino) => (S_IFDIR | 0o555, 2),
        Node::Global(SELF) => (S_IFLNK | 0o777, 1),
        Node::Global(_) => (S_IFREG | 0o444, 1),
        Node::Process(_, P_DIR | P_TASK | P_THREAD) => (S_IFDIR | 0o555, 2),
        Node::Process(_, P_EXE) => (S_IFLNK | 0o777, 1),
        Node::Process(_, _) => (S_IFREG | 0o444, 1),
    }
}

fn entries(node: Node) -> Result<Vec<(String, u32, u8)>, i64> {
    let dir = |name: &str, ino: u32| (String::from(name), ino, TYPE_DIR);
    let mut out = Vec::new();
    match node {
        Node::Global(ROOT) => {
            out.push(dir(".", ROOT));
            out.push(dir("..", ROOT));
            for &(name, ino) in GLOBAL_FILES {
                let kind = match mode(Node::Global(ino)).0 & 0o170000 {
                    S_IFDIR => TYPE_DIR,
                    S_IFLNK => TYPE_SYMLINK,
                    _ => TYPE_FILE,
                };
                out.push((String::from(name), ino, kind));
            }
            for pid in pids() {
                out.push((alloc::format!("{pid}"), pid_ino(pid, P_DIR), TYPE_DIR));
            }
        }
        Node::Global(SYS) => {
            out.push(dir(".", SYS));
            out.push(dir("..", ROOT));
            out.push(dir("kernel", SYS_KERNEL));
        }
        Node::Global(SYS_KERNEL) => {
            out.push(dir(".", SYS_KERNEL));
            out.push(dir("..", SYS));
            out.push((String::from("pid_max"), PID_MAX, TYPE_FILE));
            out.push((String::from("ostype"), OSTYPE, TYPE_FILE));
            out.push((String::from("osrelease"), OSRELEASE, TYPE_FILE));
        }
        Node::Global(SYSFS_ROOT) => {
            out.push(dir(".", SYSFS_ROOT));
            out.push(dir("..", SYSFS_ROOT));
            out.push(dir("devices", SYS_DEVICES));
        }
        Node::Global(SYS_DEVICES) => {
            out.push(dir(".", SYS_DEVICES));
            out.push(dir("..", SYSFS_ROOT));
            out.push(dir("system", SYS_SYSTEM));
        }
        Node::Global(SYS_SYSTEM) => {
            out.push(dir(".", SYS_SYSTEM));
            out.push(dir("..", SYS_DEVICES));
            out.push(dir("cpu", SYS_CPU));
        }
        Node::Global(SYS_CPU) => {
            out.push(dir(".", SYS_CPU));
            out.push(dir("..", SYS_SYSTEM));
            for (name, ino) in [("online", CPU_ONLINE), ("possible", CPU_POSSIBLE), ("present", CPU_PRESENT), ("kernel_max", CPU_KERNEL_MAX)] {
                out.push((String::from(name), ino, TYPE_FILE));
            }
            for i in 0..cpus() {
                out.push((alloc::format!("cpu{i}"), CPU_DIR + i, TYPE_DIR));
            }
        }
        Node::Global(ino) if (CPU_DIR..CPU_DIR + cpus()).contains(&ino) => {
            out.push(dir(".", ino));
            out.push(dir("..", SYS_CPU));
            out.push((String::from("online"), CPU_DIR_ONLINE + (ino - CPU_DIR), TYPE_FILE));
        }
        Node::Process(pid, P_DIR) => {
            out.push(dir(".", pid_ino(pid, P_DIR)));
            out.push(dir("..", ROOT));
            for &(name, kind) in PROCESS_FILES {
                let t = match kind {
                    P_TASK => TYPE_DIR,
                    P_EXE => TYPE_SYMLINK,
                    _ => TYPE_FILE,
                };
                out.push((String::from(name), pid_ino(pid, kind), t));
            }
        }
        Node::Process(pid, P_TASK) => {
            out.push(dir(".", pid_ino(pid, P_TASK)));
            out.push(dir("..", pid_ino(pid, P_DIR)));
            out.push((alloc::format!("{pid}"), pid_ino(pid, P_THREAD), TYPE_DIR));
        }
        Node::Process(pid, P_THREAD) => {
            out.push(dir(".", pid_ino(pid, P_THREAD)));
            out.push(dir("..", pid_ino(pid, P_TASK)));
            for &(name, kind) in &PROCESS_FILES[..5] {
                out.push((String::from(name), pid_ino(pid, P_THREAD + kind), TYPE_FILE));
            }
        }
        _ => return Err(20), // ENOTDIR
    }
    Ok(out)
}

fn lookup(node: Node, name: &str) -> Result<u32, i64> {
    if let Node::Global(ROOT) = node {
        // Any thread's id has a directory, as on Linux (the kernel finds
        // its process), though the listing shows processes only.
        if let Ok(pid) = name.parse::<u64>() {
            return if pid != 0 && process(pid).is_some() { Ok(pid_ino(pid, P_DIR)) } else { Err(ENOENT) };
        }
    }
    entries(node)?.into_iter().find(|(n, _, _)| n == name).map(|(_, ino, _)| ino).ok_or(ENOENT)
}

fn readlink(node: Node, caller: u32) -> Result<Vec<u8>, i64> {
    match node {
        Node::Global(SELF) => Ok(alloc::format!("{caller}").into_bytes()),
        Node::Process(pid, P_EXE) => query_text(QUERY_EXE, pid).filter(|e| !e.is_empty()).ok_or(ENOENT),
        _ => Err(EINVAL),
    }
}

fn contents(node: Node) -> Result<Vec<u8>, i64> {
    let text = match node {
        Node::Global(STAT) => render::stat(&system()),
        Node::Global(MEMINFO) => render::meminfo(&system()),
        Node::Global(COUNTERS) => render::counters(&system().counters),
        Node::Global(LOADAVG) => render::loadavg(&system()),
        Node::Global(UPTIME) => render::uptime(&system()),
        Node::Global(CPUINFO) => render::cpuinfo(&system()),
        Node::Global(MOUNTS) => return query_text(QUERY_MOUNTS, 0).ok_or(ENOENT),
        Node::Global(VERSION) => render::version(),
        Node::Global(FILESYSTEMS) => String::from("nodev\ttmpfs\nnodev\tproc\n\text2\n"),
        Node::Global(PID_MAX) => String::from("4194304\n"),
        Node::Global(OSTYPE) => String::from("oxidenix\n"),
        Node::Global(OSRELEASE) => String::from(concat!(env!("CARGO_PKG_VERSION"), "\n")),
        Node::Global(CPU_ONLINE | CPU_POSSIBLE | CPU_PRESENT) => cpu_range(),
        Node::Global(CPU_KERNEL_MAX) => alloc::format!("{}\n", MAX_CPUS - 1),
        Node::Global(ino) if (CPU_DIR_ONLINE..CPU_DIR_ONLINE + cpus()).contains(&ino) => String::from("1\n"),
        Node::Process(pid, kind) => {
            let kind = thread_file(kind).unwrap_or(kind);
            let p = process(pid).ok_or(ENOENT)?;
            match kind {
                P_STAT => render::pid_stat(&p, &system()),
                P_STATM => render::pid_statm(&p),
                P_STATUS => render::pid_status(&p, &system()),
                P_CMDLINE => return Ok(query_text(QUERY_CMDLINE, pid).unwrap_or_default()),
                P_COMM => alloc::format!("{}\n", render::name(&p)),
                P_COUNTERS => render::pid_counters(&p),
                _ => return Err(EINVAL),
            }
        }
        _ => return Err(EINVAL),
    };
    Ok(text.into_bytes())
}

fn handle(req: &Request, out: &mut [u8]) -> usize {
    let [a0, a1, a2, _] = req.args;
    let node = decode(a0 as u32);
    let result: Result<(i64, [u64; 6], Vec<u8>), i64> = (|| {
        let op = req.op.ok_or(ENOSYS)?;
        if !exists(node) && !matches!(op, Op::Usage | Op::Release) {
            return Err(ENOENT);
        }
        match op {
            Op::Stat => {
                let (mode, links) = mode(node);
                let now = oxrt::now();
                Ok((0, [mode, 0, links, now, now, now], Vec::new()))
            }
            Op::Read => {
                let data = contents(node)?;
                let start = (a1 as usize).min(data.len());
                let end = start.saturating_add(a2 as usize).min(data.len()).min(start + MAX_DATA);
                Ok(((end - start) as i64, [0; 6], data[start..end].to_vec()))
            }
            Op::List => {
                let all = entries(node)?;
                let mut payload = Vec::new();
                let mut next = a1 as usize;
                for (n, i, t) in all.iter().skip(next) {
                    if payload.len() + 6 + n.len() > MAX_DATA {
                        break;
                    }
                    push_entry(&mut payload, *i, *t, n.as_bytes());
                    next += 1;
                }
                let cursor = if next >= all.len() { 0 } else { next as u64 };
                Ok((0, [cursor, 0, 0, 0, 0, 0], payload))
            }
            Op::Lookup => {
                let name = core::str::from_utf8(req.payload).map_err(|_| EINVAL)?;
                Ok((0, [lookup(node, name)? as u64, 0, 0, 0, 0, 0], Vec::new()))
            }
            Op::Readlink => Ok((0, [0; 6], readlink(node, req.caller)?)),
            Op::Usage => Ok((0, [4096, 0, 0, 0, 0, 0], Vec::new())),
            Op::Release => Ok((0, [0; 6], Vec::new())),
            Op::Write | Op::Truncate | Op::Create | Op::Unlink | Op::Rename | Op::SetPerm => Err(EROFS),
        }
    })();
    match result {
        Ok((status, values, payload)) => encode_response(out, status, values, &payload),
        Err(e) => encode_response(out, -e, [0; 6], &[]),
    }
}

fn main(_args: Vec<&'static str>) -> i32 {
    if let Err(e) = oxrt::ipc_register("procfs", ROOT as u64) {
        oxrt::println!("procfs: cannot register: {}", e);
        return 1;
    }
    let mut request = vec![0u8; MAX_MESSAGE];
    let mut response = vec![0u8; MAX_MESSAGE];
    loop {
        let Ok(oxrt::Event::Request(id, len)) = oxrt::ipc_receive(&mut request, None) else { continue };
        let n = match decode_request(&request[..len]) {
            Some(req) => handle(&req, &mut response),
            None => encode_response(&mut response, -EINVAL, [0; 6], &[]),
        };
        let _ = oxrt::ipc_reply(id, &response[..n]);
    }
}
