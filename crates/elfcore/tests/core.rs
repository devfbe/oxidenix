//! The core file's format, filter and names on the host: headers and notes read back
//! field by field at Linux's offsets.

use elfcore::filter::{self, Dump, Kind};
use elfcore::pattern::{expand, written, Facts, Target};
use elfcore::*;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[test]
fn header_and_program_headers() {
    let segs = [
        Segment { start: 0x400000, end: 0x402000, flags: PF_R | PF_X, filesz: 0x1000 },
        Segment { start: 0x600000, end: 0x603000, flags: PF_R | PF_W, filesz: 0x3000 },
        Segment { start: 0x7000_0000, end: 0x7000_1000, flags: 0, filesz: 0 },
    ];
    let l = layout(&segs, 1000).unwrap();
    let h = &l.head;
    assert_eq!(&h[..4], b"\x7fELF");
    assert_eq!((h[4], h[5], h[6], h[7]), (2, 1, 1, 0));
    assert_eq!(u16_at(h, 16), 4, "ET_CORE");
    assert_eq!(u16_at(h, 18), 62, "EM_X86_64");
    assert_eq!(u32_at(h, 20), 1);
    assert_eq!(u64_at(h, 24), 0, "no entry");
    assert_eq!(u64_at(h, 32), 64, "phoff");
    assert_eq!(u64_at(h, 40), 0, "no section headers");
    assert_eq!(u16_at(h, 52), 64);
    assert_eq!(u16_at(h, 54), 56);
    assert_eq!(u16_at(h, 56), 4, "a note and three loads");
    assert_eq!(h.len(), 64 + 4 * 56);
    assert_eq!(l.notes_offset, 64 + 4 * 56);
    // The note's header.
    let p = &h[64..120];
    assert_eq!(u32_at(p, 0), 4, "PT_NOTE");
    assert_eq!(u64_at(p, 8), l.notes_offset);
    assert_eq!(u64_at(p, 32), 1000);
    assert_eq!(u64_at(p, 48), 4);
    // The loads: data from the page after the notes, back to back.
    let data = (l.notes_offset + 1000).div_ceil(4096) * 4096;
    assert_eq!(l.data_offsets, vec![data, data + 0x1000, data + 0x4000]);
    for (i, s) in segs.iter().enumerate() {
        let p = &h[120 + i * 56..176 + i * 56];
        assert_eq!(u32_at(p, 0), 1, "PT_LOAD");
        assert_eq!(u32_at(p, 4), s.flags);
        assert_eq!(u64_at(p, 8), l.data_offsets[i]);
        assert_eq!(u64_at(p, 16), s.start);
        assert_eq!(u64_at(p, 24), 0, "p_paddr");
        assert_eq!(u64_at(p, 32), s.filesz);
        assert_eq!(u64_at(p, 40), s.end - s.start);
        assert_eq!(u64_at(p, 48), 4096);
    }
    assert!(l.extnum.is_none());
    assert_eq!(l.end, data + 0x4000);
}

#[test]
fn extended_numbering() {
    let n = 70_000;
    let segs: Vec<Segment> = (0..n).map(|i| Segment { start: i * 0x2000, end: i * 0x2000 + 0x1000, flags: PF_R, filesz: 0x1000 }).collect();
    let l = layout(&segs, 100).unwrap();
    assert_eq!(u16_at(&l.head, 56), 0xffff, "PN_XNUM");
    let (at, sh) = l.extnum.unwrap();
    assert_eq!(u64_at(&l.head, 40), at, "e_shoff");
    assert_eq!(u16_at(&l.head, 58), 64);
    assert_eq!(u16_at(&l.head, 60), 1);
    assert_eq!(at, l.data_offsets[n as usize - 1] + 0x1000, "after the data");
    assert_eq!(u32_at(&sh, 4), 0, "SHT_NULL");
    assert_eq!(u64_at(&sh, 32), 1, "sh_size: e_shnum");
    assert_eq!(u32_at(&sh, 44), n as u32 + 1, "sh_info: every program header");
    assert_eq!(l.end, at + 64);
    assert_eq!(l.head.len(), 64 + (n as usize + 1) * 56);
}

#[test]
fn notes_are_padded() {
    let mut notes = Notes::default();
    notes.core(NT_SIGINFO, &[7; 5]).unwrap();
    notes.push(b"LINUX", 0x202, &[1; 8]).unwrap();
    let b = &notes.bytes;
    assert_eq!(u32_at(b, 0), 5, "\"CORE\" and its NUL");
    assert_eq!(u32_at(b, 4), 5);
    assert_eq!(u32_at(b, 8), NT_SIGINFO);
    assert_eq!(&b[12..17], b"CORE\0");
    assert_eq!(&b[20..25], &[7; 5]);
    assert_eq!(&b[25..28], &[0; 3]);
    assert_eq!(Notes::size(b"CORE", 5), 28);
    let second = &b[28..];
    assert_eq!(u32_at(second, 0), 6);
    assert_eq!(&second[12..18], b"LINUX\0");
    assert_eq!(&second[20..28], &[1; 8]);
    assert_eq!(b.len(), 28 + Notes::size(b"LINUX", 8));
}

#[test]
fn prstatus_layout() {
    let mut regs = [0u64; GREGS];
    for (i, r) in regs.iter_mut().enumerate() {
        *r = 0x1000 + i as u64;
    }
    let p = Prstatus {
        signo: 11,
        pending: 0x4000,
        blocked: 0x8,
        pid: 42,
        ppid: 1,
        pgrp: 40,
        sid: 30,
        user_ns: 2_500_000_000,
        system_ns: 1_000,
        child_user_ns: 0,
        child_system_ns: 7_000_000_000,
        regs,
    }
    .to_bytes();
    assert_eq!(u32_at(&p, 0), 11, "si_signo");
    assert_eq!(u16_at(&p, 12), 11, "pr_cursig");
    assert_eq!(u64_at(&p, 16), 0x4000);
    assert_eq!(u64_at(&p, 24), 0x8);
    assert_eq!((u32_at(&p, 32), u32_at(&p, 36), u32_at(&p, 40), u32_at(&p, 44)), (42, 1, 40, 30));
    assert_eq!((u64_at(&p, 48), u64_at(&p, 56)), (2, 500_000), "utime");
    assert_eq!((u64_at(&p, 64), u64_at(&p, 72)), (0, 1), "stime");
    assert_eq!(u64_at(&p, 96), 7, "cstime");
    // rip is word 16 of user_regs_struct.
    assert_eq!(u64_at(&p, 112 + 16 * 8), 0x1010);
    assert_eq!(u64_at(&p, 112 + 26 * 8), 0x1000 + 26);
    assert_eq!(u32_at(&p, 328), 1, "pr_fpvalid");
}

#[test]
fn prpsinfo_layout() {
    let mut fname = [0u8; 16];
    fname[..4].copy_from_slice(b"prog");
    let p = Prpsinfo { state: 0, nice: -5, flags: 0x200, uid: 0, gid: 0, pid: 9, ppid: 2, pgrp: 9, sid: 1, fname, psargs: Prpsinfo::args(b"prog\0-x\0") }.to_bytes();
    assert_eq!((p[0], p[1], p[2]), (0, b'R', 0));
    assert_eq!(p[3] as i8, -5);
    assert_eq!(u64_at(&p, 8), 0x200);
    assert_eq!((u32_at(&p, 24), u32_at(&p, 28), u32_at(&p, 32), u32_at(&p, 36)), (9, 2, 9, 1));
    assert_eq!(&p[40..45], b"prog\0");
    assert_eq!(&p[56..65], b"prog -x \0", "every NUL a space, as Linux's");
    let long = [b'a'; 100];
    let args = Prpsinfo::args(&long);
    assert_eq!(&args[..79], &[b'a'; 79]);
    assert_eq!(args[79], 0);
}

#[test]
fn file_note_layout() {
    let files = [MappedFile { start: 0x400000, end: 0x401000, offset: 0, path: b"/bin/prog" }, MappedFile { start: 0x401000, end: 0x403000, offset: 0x2000, path: b"/bin/prog" }];
    let d = file_note(&files).unwrap();
    assert_eq!(u64_at(&d, 0), 2);
    assert_eq!(u64_at(&d, 8), 4096);
    assert_eq!((u64_at(&d, 16), u64_at(&d, 24), u64_at(&d, 32)), (0x400000, 0x401000, 0));
    assert_eq!((u64_at(&d, 40), u64_at(&d, 48), u64_at(&d, 56)), (0x401000, 0x403000, 2), "offsets in pages");
    assert_eq!(&d[64..], b"/bin/prog\0/bin/prog\0");
}

#[test]
fn the_default_filter() {
    let f = filter::DEFAULT;
    assert_eq!(f, 0x33);
    assert_eq!(filter::dump(Kind::AnonPrivate, f, true, true, false), Dump::Whole);
    assert_eq!(filter::dump(Kind::AnonPrivate, f, false, true, false), Dump::Nothing, "never touched");
    assert_eq!(filter::dump(Kind::FilePrivate, f, true, true, true), Dump::Whole, "written to");
    assert_eq!(filter::dump(Kind::FilePrivate, f, false, true, true), Dump::ElfHeader);
    assert_eq!(filter::dump(Kind::FilePrivate, f, false, true, false), Dump::Nothing);
    assert_eq!(filter::dump(Kind::FilePrivate, f, false, false, true), Dump::Nothing, "unreadable");
    assert_eq!(filter::dump(Kind::AnonShared, f, false, true, true), Dump::Whole);
    assert_eq!(filter::dump(Kind::FileShared, f, true, true, true), Dump::Nothing);
    assert_eq!(filter::dump(Kind::Vdso, 0, false, true, true), Dump::Whole, "always");
    assert_eq!(filter::dump(Kind::Device, filter::MASK, true, true, true), Dump::Nothing, "never");
    assert_eq!(filter::dump(Kind::FileShared, filter::MAPPED_SHARED, false, true, true), Dump::Whole);
    assert_eq!(filter::dump(Kind::FilePrivate, filter::MAPPED_PRIVATE, false, true, false), Dump::Whole);
    assert_eq!(filter::dump(Kind::AnonPrivate, 0, true, true, false), Dump::Nothing);
}

fn facts() -> Facts<'static> {
    Facts {
        pid: 42,
        tid: 43,
        global_pid: 42,
        global_tid: 43,
        uid: 0,
        gid: 0,
        signal: 11,
        dumpable: 1,
        cpu: 3,
        limit: u64::MAX,
        time: 1_700_000_000,
        hostname: b"oxidenix",
        comm: b"crash",
        exe: Some(b"/bin/sub/crash"),
    }
}

fn file(pattern: &[u8], uses_pid: bool) -> Vec<u8> {
    match expand(pattern, uses_pid, &facts()).unwrap() {
        Target::File(name) => name,
        Target::Pipe(_) => panic!("a file was expected"),
    }
}

#[test]
fn file_names() {
    assert_eq!(file(b"core", false), b"core");
    assert_eq!(file(b"core", true), b"core.42");
    assert_eq!(file(b"core.%p", true), b"core.42", "no second pid");
    assert_eq!(file(b"core.%P", true), b"core.42.42", "%P does not count");
    assert_eq!(file(b"%e-%s.core", false), b"crash-11.core");
    assert_eq!(file(b"/tmp/%h/%i/%u.%g.%d.%C.%t", false), b"/tmp/oxidenix/43/0.0.1.3.1700000000");
    assert_eq!(file(b"%c", false), b"18446744073709551615");
    assert_eq!(file(b"%E", false), b"!bin!sub!crash");
    assert_eq!(file(b"%f", false), b"crash");
    assert_eq!(file(b"100%%", false), b"100%");
    assert_eq!(file(b"a%", false), b"a", "a trailing % is dropped");
    assert_eq!(file(b"a%zb%F", false), b"ab", "unknown specifiers are dropped");
    assert_eq!(file(b"", false), b"");
    assert_eq!(file(b"", true), b".42");
}

#[test]
fn escaped_values() {
    let mut f = facts();
    f.comm = b"..";
    f.hostname = b"";
    match expand(b"x/%e/%h/%e", false, &f).unwrap() {
        Target::File(name) => assert_eq!(name, b"x/!./!/!."),
        _ => panic!(),
    }
    f.comm = b"a/b";
    f.exe = None;
    match expand(b"%e %E", false, &f).unwrap() {
        Target::File(name) => assert_eq!(name, b"a!b a!b (path unknown)"),
        _ => panic!(),
    }
}

#[test]
fn pipes() {
    let mut f = facts();
    f.comm = b"two words";
    match expand(b"|/sbin/helper  %p %e  x%s ", true, &f).unwrap() {
        Target::Pipe(argv) => assert_eq!(argv, vec![b"/sbin/helper".to_vec(), b"42".to_vec(), b"two words".to_vec(), b"x11".to_vec()]),
        _ => panic!("a pipe"),
    }
}

#[test]
fn writes_to_core_pattern() {
    assert_eq!(written(b"core.%p\n"), b"core.%p");
    assert_eq!(written(b"a\0b"), b"a");
    assert_eq!(written(&[b'x'; 300]).len(), 127);
}
