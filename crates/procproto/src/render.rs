//! The text formats of Linux's /proc files (proc(5)): the system-wide ones
//! procfs serves (`stat`, `meminfo`, `loadavg`, `uptime`, `counters`) and a
//! process's own the Linux server makes (`<pid>/stat`, `statm`, `status`,
//! `oom_score`, `oom_adj`), from the kernel's records, a descriptor's
//! `fdinfo` (`FdInfo`, from the Linux server's files), and how the numbers
//! written to /proc are read (`parse_int`). Pure functions, tested on the
//! host (`cargo test -p procproto`).

use crate::*;
use alloc::format;
use alloc::string::String;
use core::fmt::Write;

/// Linux reports times in USER_HZ = 100 ticks per second.
pub const USER_HZ: u64 = 100;

/// What the per-process formats need of the machine: the kernel's tick
/// rate, its page size and how many CPUs run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    pub hz: u64,
    pub page_size: u64,
    pub cpus: u64,
}

impl Machine {
    pub fn of(s: &System) -> Machine {
        Machine { hz: s.hz.max(1), page_size: s.page_size.max(1), cpus: s.cpus.max(1) }
    }
}

/// What the Linux server knows of a process beyond the kernel's record:
/// its controlling terminal (`tty_nr`, 0 for none; the foreground group
/// `tpgid`, -1 for none) and its umask (None where it does not know it:
/// another process's, until the process model is the server's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Linux {
    pub tty_nr: u64,
    pub tpgid: i64,
    pub umask: Option<u32>,
}

impl Default for Linux {
    fn default() -> Linux {
        Linux { tty_nr: 0, tpgid: -1, umask: None }
    }
}

/// Everyone is root: every capability (Linux's CAP_LAST_CAP is 40).
const CAPS_ALL: u64 = (1 << 41) - 1;

pub fn to_user_hz(ticks: u64, hz: u64) -> u64 {
    ticks * USER_HZ / hz.max(1)
}

/// A process's name (comm), up to its first NUL.
pub fn name(p: &Process) -> &str {
    let len = p.name.iter().position(|&b| b == 0).unwrap_or(p.name.len());
    core::str::from_utf8(&p.name[..len]).unwrap_or("?")
}

/// The CPUs that run, as a list ("0-3", or "0" for one).
pub fn cpu_list(cpus: u64) -> String {
    match cpus {
        0 | 1 => String::from("0"),
        n => format!("0-{}", n - 1),
    }
}

/// /proc/stat: the CPU times (all CPUs, then each), context switches, the
/// boot time, processes created, and those running.
pub fn stat(s: &System) -> String {
    let mut out = String::new();
    let hz = s.hz;
    let line = |label: &str, c: &CpuTimes| {
        format!("{label} {} 0 {} {} 0 0 0 0 0 0\n", to_user_hz(c.user, hz), to_user_hz(c.system, hz), to_user_hz(c.idle, hz))
    };
    let cpus = &s.cpu[..(s.cpus as usize).min(MAX_CPUS)];
    let total = cpus.iter().fold(CpuTimes::default(), |a, c| CpuTimes { user: a.user + c.user, system: a.system + c.system, idle: a.idle + c.idle });
    out.push_str(&line("cpu ", &total));
    for (i, c) in cpus.iter().enumerate() {
        out.push_str(&line(&format!("cpu{i}"), c));
    }
    let _ = write!(
        out,
        "intr 0\nctxt {}\nbtime {}\nprocesses {}\nprocs_running {}\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n",
        s.context_switches, s.boot_time, s.forks, s.running
    );
    out
}

/// /proc/meminfo, in kB.
pub fn meminfo(s: &System) -> String {
    let kb = |bytes: u64| bytes / 1024;
    let mut out = String::new();
    for (key, value) in [
        ("MemTotal", kb(s.mem_total)),
        ("MemFree", kb(s.mem_free)),
        // Cached file pages beyond tmpfs can be dropped when needed.
        ("MemAvailable", kb(s.mem_free + s.cached.saturating_sub(s.shmem + s.dirty))),
        ("Buffers", 0),
        ("Cached", kb(s.cached)),
        ("SwapCached", 0),
        ("Active", kb(s.mem_total.saturating_sub(s.mem_free))),
        ("Inactive", 0),
        ("SwapTotal", 0),
        ("SwapFree", 0),
        ("Dirty", kb(s.dirty)),
        ("Writeback", kb(s.writeback)),
        ("Shmem", kb(s.shmem)),
        ("Slab", kb(s.kernel_heap)),
        ("SReclaimable", 0),
        ("SUnreclaim", kb(s.kernel_heap)),
        ("KernelStack", 0),
        ("PageTables", 0),
        ("CommitLimit", kb(s.commit_limit)),
        ("Committed_AS", kb(s.committed)),
        ("HugePages_Total", 0),
        ("Hugepagesize", 2048),
    ] {
        let unit = if key.starts_with("HugePages_") { "" } else { " kB" };
        let _ = writeln!(out, "{:<16}{:>8}{}", format!("{key}:"), value, unit);
    }
    out
}

/// A fixed-point value with `shift` fraction bits, to two decimals.
fn fixed(v: u64, shift: u64) -> String {
    let one = 1u64 << shift;
    let hundredths = (v * 100 + one / 2) >> shift;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// /proc/loadavg: the load averages, runnable and all threads, the last
/// pid given.
pub fn loadavg(s: &System) -> String {
    format!(
        "{} {} {} {}/{} {}\n",
        fixed(s.load[0], s.load_shift),
        fixed(s.load[1], s.load_shift),
        fixed(s.load[2], s.load_shift),
        // Runnable and all scheduling entities: threads, as on Linux.
        s.running,
        s.threads,
        s.max_pid.saturating_sub(1)
    )
}

fn seconds(ticks: u64, hz: u64) -> String {
    let hz = hz.max(1);
    format!("{}.{:02}", ticks / hz, ticks % hz * 100 / hz)
}

/// /proc/uptime: seconds since boot, and the CPUs' idle seconds.
pub fn uptime(s: &System) -> String {
    let idle: u64 = s.cpu[..(s.cpus as usize).min(MAX_CPUS)].iter().map(|c| c.idle).sum();
    format!("{} {}\n", seconds(s.uptime, s.hz), seconds(idle, s.hz))
}

/// /proc/counters (oxidenix's own): hot-path event counters since boot, one
/// "name value" per line.
pub fn counters(c: &Counters) -> String {
    let mut out = String::new();
    for (key, value) in [
        ("syscalls", c.syscalls),
        ("ipc_calls", c.ipc_calls),
        ("ipc_bytes", c.ipc_bytes),
        ("address_space_switches", c.address_space_switches),
        ("user_copy_bytes", c.user_copy_bytes),
        ("heap_allocs", c.heap_allocs),
    ] {
        let _ = writeln!(out, "{key} {value}");
    }
    out
}

/// Linux's PF_KTHREAD: tools such as htop hide kernel threads by it.
const PF_KTHREAD: u64 = 0x0020_0000;

/// /proc/<pid>/stat: 52 space-separated fields.
pub fn pid_stat(p: &Process, m: &Machine, l: &Linux) -> String {
    let flags = if p.flags & FLAG_KERNEL != 0 { PF_KTHREAD } else { 0 };
    let priority = 20 + p.nice;
    let vsize = p.virt_pages * m.page_size;
    format!(
        "{pid} ({name}) {state} {ppid} {pgid} {sid} {tty} {tpgid} {flags} 0 0 0 0 {utime} {stime} 0 0 {priority} {nice} {threads} 0 {start} {vsize} {rss} 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 {cpu} 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        tty = l.tty_nr,
        tpgid = l.tpgid,
        pid = p.pid,
        name = name(p),
        state = p.state as u8 as char,
        ppid = p.ppid,
        pgid = p.pgid,
        sid = p.sid,
        utime = to_user_hz(p.utime, m.hz),
        stime = to_user_hz(p.stime, m.hz),
        nice = p.nice,
        threads = p.threads,
        start = to_user_hz(p.start, m.hz),
        rss = p.pages,
        cpu = p.cpu,
    )
}

/// /proc/<pid>/statm: size resident shared text lib data dt (pages), as
/// Linux's task_statm: shared are the resident file and shmem pages, text
/// the program's code (`text_pages`), data its private writable memory and
/// its stacks; lib and dt are always 0.
pub fn pid_statm(p: &Process) -> String {
    format!(
        "{} {} {} {} 0 {} 0\n",
        p.virt_pages,
        p.pages,
        p.pages.saturating_sub(p.anon_pages),
        p.text_pages,
        p.data_pages + p.stack_pages
    )
}

/// The program's code and the other executable memory (libraries, an
/// interpreter, JIT code), in pages: Linux's VmExe (the code's span, at
/// most what is mapped executable) and VmLib (the rest).
fn exe_and_lib(p: &Process) -> (u64, u64) {
    let exe = p.text_pages.min(p.exec_pages);
    (exe, p.exec_pages - exe)
}

/// /proc/<pid>/status: the fields programs read, in Linux's order. (Not
/// shown: the context switches the kernel does not count per thread.)
pub fn pid_status(p: &Process, m: &Machine, l: &Linux) -> String {
    let state = match p.state as u8 {
        STATE_RUNNING => "R (running)",
        STATE_SLEEPING => "S (sleeping)",
        STATE_STOPPED => "T (stopped)",
        STATE_ZOMBIE => "Z (zombie)",
        _ => "? (unknown)",
    };
    let kb = |pages: u64| pages * m.page_size / 1024;
    let (exe, lib) = exe_and_lib(p);
    let mut out = format!("Name:\t{}\n", name(p));
    if let Some(umask) = l.umask {
        let _ = writeln!(out, "Umask:\t{umask:04o}");
    }
    let _ = write!(
        out,
        "State:\t{state}\nTgid:\t{tgid}\nNgid:\t0\nPid:\t{pid}\nPPid:\t{ppid}\nTracerPid:\t0\n\
         Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nFDSize:\t256\nGroups:\t\nNStgid:\t{tgid}\nNSpid:\t{pid}\nNSpgid:\t{pgid}\nNSsid:\t{sid}\n\
         VmPeak:\t{peak:>8} kB\nVmSize:\t{virt:>8} kB\nVmLck:\t       0 kB\nVmPin:\t       0 kB\nVmHWM:\t{hwm:>8} kB\n\
         VmRSS:\t{rss:>8} kB\nRssAnon:\t{anon:>8} kB\nRssFile:\t{file:>8} kB\nRssShmem:\t{shmem:>8} kB\n\
         VmData:\t{data:>8} kB\nVmStk:\t{stack:>8} kB\nVmExe:\t{exe:>8} kB\nVmLib:\t{lib:>8} kB\nVmPTE:\t{pte:>8} kB\nVmSwap:\t       0 kB\n\
         Threads:\t{threads}\nSigPnd:\t{pnd:016x}\nShdPnd:\t{shd:016x}\nSigBlk:\t{blk:016x}\nSigIgn:\t{ign:016x}\nSigCgt:\t{cgt:016x}\n\
         CapInh:\t0000000000000000\nCapPrm:\t{caps:016x}\nCapEff:\t{caps:016x}\nCapBnd:\t{caps:016x}\nCapAmb:\t0000000000000000\n\
         Cpus_allowed_list:\t{cpus}\n",
        pid = p.pid,
        tgid = p.tgid,
        ppid = p.ppid,
        pgid = p.pgid,
        sid = p.sid,
        peak = kb(p.virt_peak.max(p.virt_pages)),
        virt = kb(p.virt_pages),
        hwm = kb(p.peak_pages.max(p.pages)),
        rss = kb(p.pages),
        anon = kb(p.anon_pages.min(p.pages)),
        file = kb(p.pages.saturating_sub(p.anon_pages).saturating_sub(p.shmem_pages)),
        shmem = kb(p.shmem_pages.min(p.pages.saturating_sub(p.anon_pages))),
        data = kb(p.data_pages),
        stack = kb(p.stack_pages),
        exe = kb(exe),
        lib = kb(lib),
        pte = kb(p.table_pages),
        threads = p.threads,
        pnd = p.sig_pending,
        shd = p.sig_shared,
        blk = p.sig_blocked,
        ign = p.sig_ignored,
        cgt = p.sig_caught,
        caps = CAPS_ALL,
        cpus = cpu_list(m.cpus),
    );
    out
}

/// /proc/<pid>/oom_score from the OOM killer's badness `points` (None: it
/// would not choose the process) against `total` pages, as Linux's
/// `proc_oom_score`: 0 for a process it never chooses, else the badness
/// scaled into 0..=2000 (1000 + thousandths of memory, times 2/3: the badness
/// is at most twice all memory, with oom_score_adj 1000).
pub fn oom_score(points: Option<i64>, total: u64) -> u64 {
    match points {
        None => 0,
        Some(points) => {
            let total = total.max(1) as i128;
            ((1000 + points as i128 * 1000 / total) * 2 / 3).clamp(0, 2000) as u64
        }
    }
}

/// The legacy /proc/<pid>/oom_adj (-17..=15) from oom_score_adj
/// (-1000..=1000), as Linux reads it: 15 for 1000, else scaled by 17/1000.
pub fn oom_adj(score_adj: i64) -> i64 {
    if score_adj == 1000 { 15 } else { score_adj * 17 / 1000 }
}

/// The oom_score_adj a write of the legacy oom_adj sets (None: EINVAL, outside
/// -16..=15 and not -17, `OOM_DISABLE`): 1000 for 15, else scaled by 1000/17.
pub fn score_adj_of_oom_adj(oom_adj: i64) -> Option<i64> {
    match oom_adj {
        15 => Some(1000),
        -17 | -16..=14 => Some(oom_adj * 1000 / 17),
        _ => None,
    }
}

/// A number written to /proc (Linux's `kstrtoint` after `strstrip`, as
/// oom_score_adj and the sysctls read it): whitespace around it, an optional
/// sign, then decimal, octal with a leading 0, or hexadecimal with 0x. None
/// for anything else, or beyond i64.
pub fn parse_int(text: &[u8]) -> Option<i64> {
    let text = text.trim_ascii();
    let (negative, digits) = match text.first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) = match digits {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        _ => (10, digits),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i128 = 0;
    for &b in digits {
        let d = (b as char).to_digit(radix)? as i128;
        value = value * radix as i128 + d;
        if value > i64::MAX as i128 + 1 {
            return None;
        }
    }
    let value = if negative { -value } else { value };
    i64::try_from(value).ok()
}

/// What /proc/<pid>/fdinfo/<fd> shows of a descriptor (proc(5); Linux's `seq_show` in
/// fs/proc/fd.c and the files' `show_fdinfo`): the file position, the status flags
/// (`O_CLOEXEC` for a close-on-exec descriptor; octal with a leading 0), the id of the
/// mount the file is on, its inode number, then what its kind of file adds.
#[derive(Clone, Copy, Debug)]
pub struct FdInfo<'a> {
    pub pos: i64,
    pub flags: u32,
    pub mnt_id: u32,
    pub ino: u64,
    pub kind: FdKind<'a>,
}

/// The lines a kind of file adds to its fdinfo.
#[derive(Clone, Copy, Debug)]
pub enum FdKind<'a> {
    Plain,
    /// A pidfd: the id of its process or thread, -1 once that is gone (`Pid:` and the
    /// one-level `NSpid:`).
    PidFd { pid: i64 },
    /// An eventfd: its count, its id, whether it is a semaphore.
    EventFd { count: u64, id: u64, semaphore: bool },
    /// A timerfd: its clock id, the expiries not read yet, the flags of its last
    /// timerfd_settime, the time left and the interval in nanoseconds (shown as
    /// `(seconds, nanoseconds)`).
    TimerFd { clockid: i32, ticks: u64, settime_flags: u32, value: u64, interval: u64 },
    /// An epoll instance: its interests (`tfd:` lines).
    Epoll(&'a [EpollTarget]),
    /// An inotify instance: its watches (`inotify wd:` lines).
    Inotify(&'a [InotifyMark]),
}

/// An interest of an epoll instance: the descriptor number it was added by, its events
/// and flags, its data, and of the file: its position, inode number and device (`sdev`,
/// the kernel's encoding, `kernel_dev`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EpollTarget {
    pub fd: i32,
    pub events: u32,
    pub data: u64,
    pub pos: i64,
    pub ino: u64,
    pub sdev: u32,
}

/// A watch of an inotify instance: its descriptor, the watched inode's number and device
/// (the kernel's encoding), its mask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InotifyMark {
    pub wd: i32,
    pub ino: u64,
    pub sdev: u32,
    pub mask: u32,
}

/// A device number in the kernel's own encoding (`MKDEV`: the major above bit 20), as
/// fdinfo's `sdev` shows a superblock's `s_dev`.
pub fn kernel_dev(major: u32, minor: u32) -> u32 {
    major << 20 | minor & 0xf_ffff
}

impl core::fmt::Display for FdInfo<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "pos:\t{}\nflags:\t0{:o}\nmnt_id:\t{}\nino:\t{}\n", self.pos, self.flags, self.mnt_id, self.ino)?;
        const NS: u64 = 1_000_000_000;
        match self.kind {
            FdKind::Plain => Ok(()),
            FdKind::PidFd { pid } => write!(f, "Pid:\t{pid}\nNSpid:\t{pid}\n"),
            FdKind::EventFd { count, id, semaphore } => {
                write!(f, "eventfd-count: {count:16x}\neventfd-id: {id}\neventfd-semaphore: {}\n", semaphore as u8)
            }
            FdKind::TimerFd { clockid, ticks, settime_flags, value, interval } => write!(
                f,
                "clockid: {clockid}\nticks: {ticks}\nsettime flags: 0{settime_flags:o}\nit_value: ({}, {})\nit_interval: ({}, {})\n",
                value / NS,
                value % NS,
                interval / NS,
                interval % NS
            ),
            FdKind::Epoll(targets) => {
                for t in targets {
                    writeln!(f, "tfd: {:8} events: {:8x} data: {:16x}  pos:{} ino:{:x} sdev:{:x}", t.fd, t.events, t.data, t.pos, t.ino, t.sdev)?;
                }
                Ok(())
            }
            FdKind::Inotify(marks) => {
                for m in marks {
                    // (The trailing space: Linux's line leaves room for a file handle, which
                    // these filesystems do not export.)
                    writeln!(f, "inotify wd:{:x} ino:{:x} sdev:{:x} mask:{:x} ignored_mask:0 ", m.wd, m.ino, m.sdev, m.mask)?;
                }
                Ok(())
            }
        }
    }
}

