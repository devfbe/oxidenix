//! The text formats of /proc against what Linux programs parse: the field
//! counts and orders of /proc/stat, /proc/<pid>/stat, statm and status, the
//! units of meminfo, the fixed-point load averages, times in USER_HZ.

use procproto::render::*;
use procproto::*;

fn system() -> System {
    let mut s = System { hz: 1000, uptime: 12_345, boot_time: 1_700_000_000, cpus: 2, page_size: 4096, ..Default::default() };
    s.cpu[0] = CpuTimes { user: 1000, system: 500, idle: 3000 };
    s.cpu[1] = CpuTimes { user: 2000, system: 0, idle: 1000 };
    s.mem_total = 256 << 20;
    s.mem_free = 128 << 20;
    s.cached = 16 << 20;
    s.shmem = 4 << 20;
    s.dirty = 1 << 20;
    s.load_shift = 11;
    s.load = [2048, 1024 + 512, 0];
    s.running = 3;
    s.threads = 40;
    s.max_pid = 101;
    s.forks = 99;
    s.context_switches = 555;
    s
}

fn process() -> Process {
    let mut p = Process { pid: 42, tgid: 42, ppid: 1, pgid: 42, sid: 1, state: STATE_SLEEPING as u64, ..Default::default() };
    p.utime = 2500;
    p.stime = 500;
    p.start = 100;
    p.pages = 10;
    p.virt_pages = 300;
    p.nice = 5;
    p.threads = 3;
    p.cpu = 1;
    p.name[..9].copy_from_slice(b"some name");
    p
}

#[test]
fn proc_stat_has_a_total_and_one_line_per_cpu() {
    let text = stat(&system());
    let lines: Vec<&str> = text.lines().collect();
    // Times in USER_HZ: 3000 user ticks at 1000 Hz are 300.
    assert_eq!(lines[0], "cpu  300 0 50 400 0 0 0 0 0 0");
    assert_eq!(lines[1], "cpu0 100 0 50 300 0 0 0 0 0 0");
    assert_eq!(lines[2], "cpu1 200 0 0 100 0 0 0 0 0 0");
    assert!(text.contains("\nctxt 555\n") && text.contains("\nbtime 1700000000\n") && text.contains("\nprocesses 99\n"));
    assert!(text.contains("\nprocs_running 3\n"));
}

#[test]
fn meminfo_is_in_kilobytes_and_available_counts_droppable_pages() {
    let text = meminfo(&system());
    let field = |key: &str| -> u64 {
        let line = text.lines().find(|l| l.starts_with(&format!("{key}:"))).expect(key);
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    assert_eq!(field("MemTotal"), 256 * 1024);
    assert_eq!(field("MemFree"), 128 * 1024);
    // Free plus the cached pages that are neither shared memory nor dirty.
    assert_eq!(field("MemAvailable"), 128 * 1024 + 11 * 1024);
    assert_eq!(field("Dirty"), 1024);
    assert!(text.lines().all(|l| l.ends_with(" kB") || l.starts_with("HugePages_")));
}

#[test]
fn loadavg_and_uptime() {
    assert_eq!(loadavg(&system()), "1.00 0.75 0.00 3/40 100\n");
    // 12345 ticks at 1000 Hz; idle 4000 ticks.
    assert_eq!(uptime(&system()), "12.34 4.00\n");
}

#[test]
fn pid_stat_has_52_fields() {
    let m = Machine::of(&system());
    let text = pid_stat(&process(), &m, &Linux::default());
    assert!(text.ends_with('\n'));
    // Without a controlling terminal: tty_nr (7) 0, tpgid (8) -1.
    let none: Vec<String> = text.trim_end().split_once(") ").unwrap().1.split(' ').map(String::from).collect();
    assert_eq!((none[4].as_str(), none[5].as_str()), ("0", "-1"));
    let with_tty = pid_stat(&process(), &m, &Linux { tty_nr: 0x8801, tpgid: 42, umask: None });
    let f: Vec<&str> = with_tty.trim_end().split_once(") ").unwrap().1.split(' ').collect();
    assert_eq!((f[4], f[5]), ("34817", "42"));
    // The name is in parentheses and may hold spaces: count after it.
    let (head, rest) = text.trim_end().split_once(") ").unwrap();
    assert_eq!(head, "42 (some name");
    let fields: Vec<&str> = rest.split(' ').collect();
    assert_eq!(fields.len() + 2, 52);
    assert_eq!(fields[0], "S");
    // utime and stime (fields 14 and 15) in USER_HZ, nice (19), threads (20).
    assert_eq!(fields[11], "250");
    assert_eq!(fields[12], "50");
    assert_eq!(fields[16], "5");
    assert_eq!(fields[17], "3");
    // vsize (23) in bytes, rss (24) in pages.
    assert_eq!(fields[20], (300 * 4096).to_string());
    assert_eq!(fields[21], "10");
}

#[test]
fn statm_status_and_counters() {
    let m = Machine::of(&system());
    let p = process();
    assert_eq!(pid_statm(&p), "300 10 0 0 0 10 0\n");
    let status = pid_status(&p, &m, &Linux::default());
    assert!(status.starts_with("Name:\tsome name\nState:"), "no Umask line without one known");
    let p2 = Process { sig_blocked: 1 << 1, sig_ignored: 1 << 12, sig_caught: 1 << 16, peak_pages: 20, virt_peak: 400, ..p };
    let full = pid_status(&p2, &m, &Linux { umask: Some(0o027), ..Linux::default() });
    assert!(full.starts_with("Name:\tsome name\nUmask:\t0027\nState:\tS (sleeping)\n"));
    assert!(full.contains("\nSigBlk:\t0000000000000002\n") && full.contains("\nSigIgn:\t0000000000001000\n") && full.contains("\nSigCgt:\t0000000000010000\n"));
    assert!(full.contains("\nVmPeak:\t    1600 kB\n") && full.contains("\nVmHWM:\t      80 kB\n") && full.contains("\nNSpid:\t42\n"));
    assert!(full.contains("\nCapEff:\t000001ffffffffff\n") && full.contains("\nCapInh:\t0000000000000000\n"));
    assert!(status.contains("\nPid:\t42\n") && status.contains("\nTgid:\t42\n") && status.contains("\nPPid:\t1\n"));
    assert!(status.contains("\nThreads:\t3\n") && status.contains("\nVmRSS:\t      40 kB\n"));
    assert!(status.contains("\nCpus_allowed_list:\t0-1\n"));
    let c = counters(&Counters { syscalls: 5, ..Default::default() });
    assert!(c.starts_with("syscalls 5\nipc_calls 0\n") && c.lines().count() == 6);
}

#[test]
fn cpu_lists() {
    assert_eq!(cpu_list(1), "0");
    assert_eq!(cpu_list(4), "0-3");
}
