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
    p.anon_pages = 6;
    p.shmem_pages = 1;
    p.table_pages = 2;
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
    // Shared: the resident file and shmem pages.
    assert_eq!(pid_statm(&p), "300 10 4 0 0 0 0\n");
    let status = pid_status(&p, &m, &Linux::default());
    assert!(status.starts_with("Name:\tsome name\nState:"), "no Umask line without one known");
    let p2 = Process { sig_blocked: 1 << 1, sig_ignored: 1 << 12, sig_caught: 1 << 16, peak_pages: 20, virt_peak: 400, ..p };
    let full = pid_status(&p2, &m, &Linux { umask: Some(0o027), ..Linux::default() });
    assert!(full.starts_with("Name:\tsome name\nUmask:\t0027\nState:\tS (sleeping)\n"));
    assert!(full.contains("\nSigBlk:\t0000000000000002\n") && full.contains("\nSigIgn:\t0000000000001000\n") && full.contains("\nSigCgt:\t0000000000010000\n"));
    assert!(full.contains("\nVmPeak:\t    1600 kB\n") && full.contains("\nVmHWM:\t      80 kB\n") && full.contains("\nNSpid:\t42\n"));
    assert!(full.contains("\nCapEff:\t000001ffffffffff\n") && full.contains("\nCapInh:\t0000000000000000\n"));
    assert!(status.contains("\nPid:\t42\n") && status.contains("\nTgid:\t42\n") && status.contains("\nPPid:\t1\n"));
    assert!(status.contains("\nPPid:\t1\nTracerPid:\t0\n"));
    // A ptrace stop, and its tracer's process id.
    let traced = pid_status(&Process { state: STATE_TRACED as u64, tracer: 7, ..p }, &m, &Linux::default());
    assert!(traced.contains("\nState:\tt (tracing stop)\n") && traced.contains("\nTracerPid:\t7\n"));
    assert!(status.contains("\nThreads:\t3\n") && status.contains("\nVmRSS:\t      40 kB\n"));
    assert!(status.contains("\nCpus_allowed_list:\t0-1\n"));
    // Resident pages by kind, the areas by kind, the page tables: Linux's order.
    assert!(status.contains(
        "\nVmRSS:\t      40 kB\nRssAnon:\t      24 kB\nRssFile:\t      12 kB\nRssShmem:\t       4 kB\n\
         VmData:\t       0 kB\nVmStk:\t       0 kB\nVmExe:\t       0 kB\nVmLib:\t       0 kB\nVmPTE:\t       8 kB\nVmSwap:\t       0 kB\n"
    ));
    let c = counters(&Counters { syscalls: 5, ..Default::default() });
    assert!(c.starts_with("syscalls 5\nipc_calls 0\n") && c.lines().count() == 6);
}

#[test]
fn memory_by_kind_of_mapping() {
    let m = Machine::of(&system());
    // 20 pages of code mapped executable, 5 more executable pages (a library, JIT code),
    // 100 of data and 64 of stack.
    let p = Process { text_pages: 20, exec_pages: 25, data_pages: 100, stack_pages: 64, ..process() };
    // statm: size resident shared text lib data dt; data counts the stacks too.
    assert_eq!(pid_statm(&p), "300 10 4 20 0 164 0\n");
    let status = pid_status(&p, &m, &Linux::default());
    // In Linux's order: after RssShmem, before VmPTE and VmSwap.
    let want = "\nRssShmem:\t       4 kB\nVmData:\t     400 kB\nVmStk:\t     256 kB\nVmExe:\t      80 kB\nVmLib:\t      20 kB\nVmPTE:\t       8 kB\nVmSwap:\t       0 kB\n";
    assert!(status.contains(want), "{status}");
    // VmExe is at most what is mapped executable (Linux's min with exec_vm); VmLib never
    // below zero.
    let unmapped = Process { text_pages: 20, exec_pages: 8, ..process() };
    let status = pid_status(&unmapped, &m, &Linux::default());
    assert!(status.contains("\nVmExe:\t      32 kB\nVmLib:\t       0 kB\n"), "{status}");
    // statm's text is the code's span, as Linux's (not bounded by what is mapped).
    assert_eq!(pid_statm(&unmapped).split(' ').nth(3), Some("20"));
}

#[test]
fn cpu_lists() {
    assert_eq!(cpu_list(1), "0");
    assert_eq!(cpu_list(4), "0-3");
}

#[test]
fn oom_score_scales_the_badness_as_linux() {
    // Never chosen: 0.
    assert_eq!(oom_score(None, 65536), 0);
    // Nothing resident: the middle of the scale, times 2/3.
    assert_eq!(oom_score(Some(0), 65536), 666);
    // All memory resident: 2000 * 2/3; with oom_score_adj 1000 on top: 2000.
    assert_eq!(oom_score(Some(65536), 65536), 1333);
    assert_eq!(oom_score(Some(2 * 65536), 65536), 2000);
    // A small process with oom_score_adj -999 (its points less 999 * 65):
    // nearly 0, and never below.
    assert_eq!(oom_score(Some(10 - 999 * 65), 65536), 6);
    assert_eq!(oom_score(Some(-3 * 65536), 65536), 0);
    // Larger processes score higher.
    assert!(oom_score(Some(8192), 65536) > oom_score(Some(2048), 65536));
}

#[test]
fn legacy_oom_adj_converts_as_linux() {
    assert_eq!(oom_adj(1000), 15);
    assert_eq!(oom_adj(-1000), -17);
    assert_eq!(oom_adj(0), 0);
    assert_eq!(oom_adj(500), 8);
    assert_eq!(oom_adj(-500), -8);
    assert_eq!(score_adj_of_oom_adj(15), Some(1000));
    assert_eq!(score_adj_of_oom_adj(-17), Some(-1000));
    assert_eq!(score_adj_of_oom_adj(-16), Some(-941));
    assert_eq!(score_adj_of_oom_adj(8), Some(470));
    assert_eq!(score_adj_of_oom_adj(16), None);
    assert_eq!(score_adj_of_oom_adj(-18), None);
}

#[test]
fn numbers_written_to_proc() {
    assert_eq!(parse_int(b"500\n"), Some(500));
    assert_eq!(parse_int(b"  -1000 \n"), Some(-1000));
    assert_eq!(parse_int(b"+7"), Some(7));
    assert_eq!(parse_int(b"0x10"), Some(16));
    assert_eq!(parse_int(b"010"), Some(8));
    assert_eq!(parse_int(b"0"), Some(0));
    assert_eq!(parse_int(b"-9223372036854775808"), Some(i64::MIN));
    for bad in [&b""[..], b"\n", b"-", b"12a", b"1 2", b"08", b"0x", b"9223372036854775808", b"--1"] {
        assert_eq!(parse_int(bad), None, "{:?}", core::str::from_utf8(bad));
    }
}

fn fdinfo(kind: FdKind) -> String {
    FdInfo { pos: 5, flags: 0o2000002, mnt_id: 14, ino: 77, kind }.to_string()
}

#[test]
fn fdinfo_head_as_linux_seq_show() {
    assert_eq!(fdinfo(FdKind::Plain), "pos:\t5\nflags:\t02000002\nmnt_id:\t14\nino:\t77\n");
    // A read-only descriptor without flags: "00", as Linux's "0%o".
    let ro = FdInfo { pos: 0, flags: 0, mnt_id: 1, ino: 2, kind: FdKind::Plain }.to_string();
    assert!(ro.starts_with("pos:\t0\nflags:\t00\n"));
}

#[test]
fn fdinfo_of_a_pidfd() {
    assert!(fdinfo(FdKind::PidFd { pid: 42 }).ends_with("ino:\t77\nPid:\t42\nNSpid:\t42\n"));
    assert!(fdinfo(FdKind::PidFd { pid: -1 }).ends_with("Pid:\t-1\nNSpid:\t-1\n"));
}

#[test]
fn fdinfo_of_an_eventfd_and_a_timerfd() {
    let e = fdinfo(FdKind::EventFd { count: 0x2a, id: 3, semaphore: true });
    assert!(e.ends_with("eventfd-count:               2a\neventfd-id: 3\neventfd-semaphore: 1\n"), "{e}");
    let t = fdinfo(FdKind::TimerFd { clockid: 1, ticks: 2, settime_flags: 1, value: 1_500_000_000, interval: 250_000_000 });
    assert!(t.ends_with("clockid: 1\nticks: 2\nsettime flags: 01\nit_value: (1, 500000000)\nit_interval: (0, 250000000)\n"), "{t}");
}

#[test]
fn fdinfo_of_epoll_and_inotify() {
    let targets = [EpollTarget { fd: 7, events: 0x8000_0019, data: 0xabc, pos: 0, ino: 0x1f, sdev: kernel_dev(0, 0xe) }];
    let e = fdinfo(FdKind::Epoll(&targets));
    assert!(e.ends_with("ino:\t77\ntfd:        7 events: 80000019 data:              abc  pos:0 ino:1f sdev:e\n"), "{e}");
    let marks = [InotifyMark { wd: 1, ino: 0x10, sdev: kernel_dev(0xfe, 0), mask: 0x300 }];
    let i = fdinfo(FdKind::Inotify(&marks));
    assert!(i.ends_with("inotify wd:1 ino:10 sdev:fe00000 mask:300 ignored_mask:0 \n"), "{i}");
    assert_eq!(kernel_dev(8, 1), 0x80_0001);
}
