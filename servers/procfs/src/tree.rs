//! The two trees procfs serves: the system-wide part of /proc (its root is
//! `PROC_ROOT`) and /sys (`SYSFS_ROOT`). Each process's own part of /proc
//! (`/proc/<pid>`, `self`, `mounts`) is the Linux server's, which merges
//! it with this root.
//!
//! Inode numbers say what a file is (below 1024, fixed); contents are made
//! on every read from the kernel's records (`proc_query`), so they are
//! always current; as on Linux, files report size 0.

use crate::render;
use alloc::string::String;
use alloc::vec::Vec;
use fsring::{TYPE_DIR, TYPE_FILE};
use procproto::render as fmt;
use procproto::*;

pub const ENOENT: i64 = 2;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;

pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;

/// /proc.
const ROOT: u32 = PROC_ROOT;
const STAT: u32 = 2;
const MEMINFO: u32 = 3;
const LOADAVG: u32 = 4;
const UPTIME: u32 = 5;
const CPUINFO: u32 = 6;
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

/// The files of /proc's root, besides `sys`.
const ROOT_FILES: &[(&str, u32)] = &[
    ("stat", STAT),
    ("meminfo", MEMINFO),
    ("loadavg", LOADAVG),
    ("uptime", UPTIME),
    ("cpuinfo", CPUINFO),
    ("version", VERSION),
    ("filesystems", FILESYSTEMS),
    ("counters", COUNTERS),
];

pub fn system() -> System {
    let mut buf = [0u8; core::mem::size_of::<System>()];
    oxrt::proc_query(QUERY_SYSTEM, 0, &mut buf).ok().and_then(|_| from_bytes(&buf)).unwrap_or_default()
}

fn cpus() -> u32 {
    (system().cpus as u32).clamp(1, MAX_CPUS as u32)
}

fn is_cpu_dir(ino: u32) -> bool {
    (CPU_DIR..CPU_DIR + cpus()).contains(&ino)
}

fn is_cpu_online(ino: u32) -> bool {
    (CPU_DIR_ONLINE..CPU_DIR_ONLINE + cpus()).contains(&ino)
}

pub fn exists(ino: u32) -> bool {
    (ROOT..=COUNTERS).contains(&ino) && !matches!(ino, 7 | 8)
        || (SYSFS_ROOT..=CPU_KERNEL_MAX).contains(&ino)
        || is_cpu_dir(ino)
        || is_cpu_online(ino)
}

pub fn is_dir(ino: u32) -> bool {
    matches!(ino, ROOT | SYS | SYS_KERNEL | SYSFS_ROOT | SYS_DEVICES | SYS_SYSTEM | SYS_CPU) || is_cpu_dir(ino)
}

/// (mode, links) of an inode that exists.
pub fn mode(ino: u32) -> (u32, u32) {
    if is_dir(ino) {
        (S_IFDIR | 0o555, 2)
    } else {
        (S_IFREG | 0o444, 1)
    }
}

/// A directory's entries as (name, inode, type), "." and ".." first.
pub fn entries(dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
    if !exists(dir) {
        return Err(ENOENT);
    }
    let parent = match dir {
        ROOT | SYSFS_ROOT => dir,
        SYS => ROOT,
        SYS_KERNEL => SYS,
        SYS_DEVICES => SYSFS_ROOT,
        SYS_SYSTEM => SYS_DEVICES,
        SYS_CPU => SYS_SYSTEM,
        d if is_cpu_dir(d) => SYS_CPU,
        _ => return Err(ENOTDIR),
    };
    let entry = |name: &str, ino: u32| (String::from(name), ino, if is_dir(ino) { TYPE_DIR } else { TYPE_FILE });
    let mut out = alloc::vec![entry(".", dir), entry("..", parent)];
    match dir {
        ROOT => {
            out.extend(ROOT_FILES.iter().map(|&(n, i)| entry(n, i)));
            out.push(entry("sys", SYS));
        }
        SYS => out.push(entry("kernel", SYS_KERNEL)),
        SYS_KERNEL => out.extend([entry("pid_max", PID_MAX), entry("ostype", OSTYPE), entry("osrelease", OSRELEASE)]),
        SYSFS_ROOT => out.push(entry("devices", SYS_DEVICES)),
        SYS_DEVICES => out.push(entry("system", SYS_SYSTEM)),
        SYS_SYSTEM => out.push(entry("cpu", SYS_CPU)),
        SYS_CPU => {
            out.extend([entry("online", CPU_ONLINE), entry("possible", CPU_POSSIBLE), entry("present", CPU_PRESENT), entry("kernel_max", CPU_KERNEL_MAX)]);
            for i in 0..cpus() {
                out.push(entry(&alloc::format!("cpu{i}"), CPU_DIR + i));
            }
        }
        cpu => out.push(entry("online", CPU_DIR_ONLINE + (cpu - CPU_DIR))),
    }
    Ok(out)
}

pub fn lookup(dir: u32, name: &str) -> Result<u32, i64> {
    entries(dir)?.into_iter().find(|(n, _, _)| n == name).map(|(_, ino, _)| ino).ok_or(ENOENT)
}

/// A file's contents, made now.
pub fn contents(ino: u32) -> Result<Vec<u8>, i64> {
    if !exists(ino) {
        return Err(ENOENT);
    }
    if is_dir(ino) {
        return Err(EISDIR);
    }
    let text = match ino {
        STAT => fmt::stat(&system()),
        MEMINFO => fmt::meminfo(&system()),
        COUNTERS => fmt::counters(&system().counters),
        LOADAVG => fmt::loadavg(&system()),
        UPTIME => fmt::uptime(&system()),
        CPUINFO => render::cpuinfo(&system()),
        VERSION => render::version(),
        FILESYSTEMS => String::from("nodev\tsysfs\nnodev\ttmpfs\nnodev\tdevtmpfs\nnodev\tproc\nnodev\tdevpts\n\text2\n"),
        PID_MAX => String::from("4194304\n"),
        OSTYPE => String::from("oxidenix\n"),
        OSRELEASE => String::from(concat!(env!("CARGO_PKG_VERSION"), "\n")),
        CPU_ONLINE | CPU_POSSIBLE | CPU_PRESENT => alloc::format!("{}\n", fmt::cpu_list(cpus() as u64)),
        CPU_KERNEL_MAX => alloc::format!("{}\n", MAX_CPUS - 1),
        i if is_cpu_online(i) => String::from("1\n"),
        _ => return Err(EINVAL),
    };
    Ok(text.into_bytes())
}
