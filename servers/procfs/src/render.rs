//! The text formats of Linux's /proc files.

use alloc::format;
use alloc::string::String;
use core::fmt::Write;
use procproto::*;

/// Linux reports times in USER_HZ = 100 ticks per second.
const USER_HZ: u64 = 100;

fn to_user_hz(ticks: u64, hz: u64) -> u64 {
    ticks * USER_HZ / hz.max(1)
}

pub fn name(p: &Process) -> &str {
    let len = p.name.iter().position(|&b| b == 0).unwrap_or(p.name.len());
    core::str::from_utf8(&p.name[..len]).unwrap_or("?")
}

pub fn stat(s: &System) -> String {
    let mut out = String::new();
    let hz = s.hz;
    let line = |label: &str, c: &CpuTimes| {
        format!(
            "{label} {} 0 {} {} 0 0 0 0 0 0\n",
            to_user_hz(c.user, hz),
            to_user_hz(c.system, hz),
            to_user_hz(c.idle, hz)
        )
    };
    let cpus = &s.cpu[..(s.cpus as usize).min(MAX_CPUS)];
    let total = cpus.iter().fold(CpuTimes::default(), |a, c| CpuTimes {
        user: a.user + c.user,
        system: a.system + c.system,
        idle: a.idle + c.idle,
    });
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

pub fn meminfo(s: &System) -> String {
    let kb = |bytes: u64| bytes / 1024;
    let mut out = String::new();
    for (key, value) in [
        ("MemTotal", kb(s.mem_total)),
        ("MemFree", kb(s.mem_free)),
        // Cached file pages beyond tmpfs can be dropped when needed.
        ("MemAvailable", kb(s.mem_free + s.cached - s.shmem)),
        ("Buffers", 0),
        ("Cached", kb(s.cached)),
        ("SwapCached", 0),
        ("Active", kb(s.mem_total - s.mem_free)),
        ("Inactive", 0),
        ("SwapTotal", 0),
        ("SwapFree", 0),
        ("Dirty", 0),
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

fn fixed(v: u64, shift: u64) -> String {
    let one = 1u64 << shift;
    let hundredths = (v * 100 + one / 2) >> shift;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

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

pub fn uptime(s: &System) -> String {
    let idle: u64 = s.cpu[..(s.cpus as usize).min(MAX_CPUS)].iter().map(|c| c.idle).sum();
    format!("{} {}\n", seconds(s.uptime, s.hz), seconds(idle, s.hz))
}

pub fn version() -> String {
    String::from(concat!("oxidenix version ", env!("CARGO_PKG_VERSION"), " (built with Rust) #1 SMP\n"))
}

/// /proc/cpuinfo from CPUID (an unprivileged instruction).
pub fn cpuinfo(s: &System) -> String {
    use core::arch::x86_64::__cpuid;
    let words = |order: [u32; 4]| -> alloc::vec::Vec<u8> { order.iter().flat_map(|w| w.to_le_bytes()).collect() };
    let v = __cpuid(0);
    let vendor_bytes = words([v.ebx, v.edx, v.ecx, 0]);
    let vendor = String::from_utf8_lossy(&vendor_bytes[..12]).into_owned();
    let mut brand = alloc::vec::Vec::new();
    if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
        for leaf in 0x8000_0002..=0x8000_0004u32 {
            let r = __cpuid(leaf);
            brand.extend(words([r.eax, r.ebx, r.ecx, r.edx]));
        }
    }
    let brand: String = String::from_utf8_lossy(&brand).trim_matches(|c: char| c == '\0' || c == ' ').into();
    let sig = __cpuid(1).eax;
    let family = (sig >> 8) & 0xf;
    let model = ((sig >> 4) & 0xf) | ((sig >> 12) & 0xf0);
    let n = s.cpus;
    let mut out = String::new();
    for i in 0..n {
        let _ = write!(
            out,
            "processor\t: {i}\nvendor_id\t: {vendor}\ncpu family\t: {family}\nmodel\t\t: {model}\nmodel name\t: {brand}\nphysical id\t: 0\nsiblings\t: {n}\ncore id\t\t: {i}\ncpu cores\t: {n}\napicid\t\t: {i}\nflags\t\t: fpu tsc msr pae cx8 apic sep pge cmov pat clflush mmx fxsr sse sse2 syscall nx lm\n\n"
        );
    }
    out
}

fn state_letter(p: &Process) -> char {
    p.state as u8 as char
}

/// Linux's PF_KTHREAD: tools such as htop hide kernel threads by it.
const PF_KTHREAD: u64 = 0x0020_0000;

/// /proc/<pid>/stat: 52 space-separated fields.
pub fn pid_stat(p: &Process, s: &System) -> String {
    let flags = if p.flags & FLAG_KERNEL != 0 { PF_KTHREAD } else { 0 };
    let priority = 20 + p.nice;
    let vsize = p.virt_pages * s.page_size;
    format!(
        "{pid} ({name}) {state} {ppid} {pgid} {sid} 0 -1 {flags} 0 0 0 0 {utime} {stime} 0 0 {priority} {nice} {threads} 0 {start} {vsize} {rss} 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 {cpu} 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        pid = p.pid,
        name = name(p),
        state = state_letter(p),
        ppid = p.ppid,
        pgid = p.pgid,
        sid = p.sid,
        utime = to_user_hz(p.utime, s.hz),
        stime = to_user_hz(p.stime, s.hz),
        nice = p.nice,
        threads = p.threads,
        start = to_user_hz(p.start, s.hz),
        rss = p.pages,
        cpu = p.cpu,
    )
}

/// /proc/<pid>/statm: size resident shared text lib data dt (pages).
pub fn pid_statm(p: &Process) -> String {
    format!("{} {} 0 0 0 {} 0\n", p.virt_pages, p.pages, p.pages)
}

pub fn pid_status(p: &Process, s: &System) -> String {
    let state = match p.state as u8 {
        STATE_RUNNING => "R (running)",
        STATE_SLEEPING => "S (sleeping)",
        STATE_STOPPED => "T (stopped)",
        STATE_ZOMBIE => "Z (zombie)",
        _ => "? (unknown)",
    };
    let kb = p.pages * 4;
    let virt_kb = p.virt_pages * 4;
    format!(
        "Name:\t{name}\nUmask:\t0022\nState:\t{state}\nTgid:\t{pid}\nNgid:\t0\nPid:\t{pid}\nPPid:\t{ppid}\nTracerPid:\t0\n\
         Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nFDSize:\t256\nGroups:\t\nVmPeak:\t{virt_kb:>8} kB\nVmSize:\t{virt_kb:>8} kB\n\
         VmRSS:\t{kb:>8} kB\nRssAnon:\t{kb:>8} kB\nVmSwap:\t       0 kB\nThreads:\t{threads}\nCpus_allowed_list:\t0-{last}\n",
        name = name(p),
        pid = p.pid,
        ppid = p.ppid,
        threads = p.threads,
        last = s.cpus.saturating_sub(1),
    )
}
