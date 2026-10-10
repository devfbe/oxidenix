//! The file protocol's encodings: every request survives encode and decode
//! (inode handles with their generations),
//! malformed descriptors are refused with the right errno (unknown
//! operations, stray fields, lengths, promises and names out of range), and the
//! completion, stat, usage and directory entry encodings round-trip.

use fsring::errno::*;
use fsring::*;

fn buf(grant: u32, offset: u32, len: u32) -> Buf {
    Buf { grant, offset, len }
}

/// A handle (a generation that does not fit 16 bits, so its bits are checked).
fn n(ino: u32) -> Node {
    Node::new(ino, 0x8000_0000 | ino << 4)
}

fn every_request() -> Vec<Request> {
    vec![
        Request::Root,
        Request::Read { ino: n(12), offset: 1 << 40, buf: buf(3, 4096, MAX_TRANSFER) },
        Request::Write { ino: n(12), offset: 7, buf: buf(1, 0, 1) },
        Request::Flush,
        Request::Stat { ino: n(2) },
        Request::Lookup { dir: n(2), name: buf(4, 100, NAME_MAX) },
        Request::Create { dir: n(2), name: buf(4, 0, 5), kind: Kind::File, perm: 0o644 },
        Request::Create { dir: n(2), name: buf(4, 0, 5), kind: Kind::Dir, perm: 0o7777 },
        Request::Create { dir: n(2), name: buf(4, 0, 5), kind: Kind::Socket, perm: 0o755 },
        Request::Create { dir: n(2), name: buf(4, 10, 5), kind: Kind::Symlink(buf(4, 15, TARGET_MAX)), perm: 0 },
        Request::Unlink { dir: n(2), name: buf(4, 0, 3), is_dir: true },
        Request::Rename { from: n(2), name: buf(4, 0, 3), to: n(11), new_name: buf(4, 3, 9) },
        Request::Truncate { ino: n(12), len: 12345 },
        Request::Readdir { dir: n(2), cursor: 17, buf: buf(5, 0, 4096) },
        Request::Release { ino: n(99) },
        Request::Readlink { ino: n(13), buf: buf(5, 8, 64) },
        Request::SetPerm { ino: n(12), perm: 0o600 },
        Request::SetTimes { ino: n(12), atime: Some(1), mtime: None, ctime: None },
        Request::SetTimes { ino: n(12), atime: Some(0), mtime: Some(u32::MAX), ctime: Some(7) },
        Request::Statfs,
        Request::Forget { grant: 4095 },
        Request::Promise { ino: n(12), offset: 1 << 40, len: MAX_TRANSFER as u64 },
    ]
}

#[test]
fn every_request_round_trips() {
    for (tag, r) in every_request().into_iter().enumerate() {
        let d = r.encode(tag as u64 + 100);
        assert_eq!(d.tag, tag as u64 + 100);
        assert_eq!(d.op, r.op());
        assert_eq!(Request::decode(&d), Ok(r), "{r:?}");
    }
}

#[test]
fn only_reads_and_writes_are_concurrent() {
    for r in every_request() {
        assert_eq!(r.is_data(), matches!(r.op(), op::READ | op::WRITE), "{r:?}");
    }
}

#[test]
fn unknown_operations_are_refused() {
    for op in [0, 19, 99, u16::MAX] {
        let d = Desc { op, ..Desc::default() };
        assert_eq!(Request::decode(&d), Err(ENOSYS));
    }
}

#[test]
fn a_field_the_operation_does_not_use_must_be_zero() {
    for r in every_request() {
        let good = r.encode(1);
        let mut variants = vec![];
        let mut d = good;
        d.flags = 1;
        variants.push(d);
        // The fields each operation leaves unused, set one by one.
        let mut d = good;
        if d.object == 0 && !matches!(r, Request::Stat { .. }) {
            d.object = 1;
            variants.push(d);
        }
        let mut d = good;
        if d.arg[2] == 0 {
            d.arg[2] = 1;
            if !matches!(r, Request::Create { .. }) {
                variants.push(d);
            }
        }
        if matches!(r, Request::Flush | Request::Statfs | Request::Stat { .. } | Request::Release { .. } | Request::SetPerm { .. }) {
            for set in [|d: &mut Desc| d.grant = 1, |d: &mut Desc| d.buf_off = 1, |d: &mut Desc| d.len = 1, |d: &mut Desc| d.offset = 1] {
                let mut d = good;
                set(&mut d);
                variants.push(d);
            }
        }
        if let Request::Forget { .. } | Request::Promise { .. } = r {
            let mut d = good;
            d.len = 1;
            variants.push(d);
        }
        if let Request::Promise { .. } = r {
            let mut d = good;
            d.arg[1] = 1;
            variants.push(d);
        }
        for d in variants {
            assert_eq!(Request::decode(&d), Err(EINVAL), "{r:?} with {d:?}");
        }
    }
}

#[test]
fn settimes_takes_known_times_in_32_bits() {
    let set = |which: u64, arg: [u64; 3]| Request::decode(&Desc { op: op::SETTIMES, object: n(12).to_object(), offset: which, arg, ..Desc::default() });
    assert_eq!(set(TIME_MTIME, [0, 5, 0]), Ok(Request::SetTimes { ino: n(12), atime: None, mtime: Some(5), ctime: None }));
    assert_eq!(set(8, [0, 0, 0]), Err(EINVAL));
    assert_eq!(set(TIME_ATIME, [1 << 32, 0, 0]), Err(EINVAL));
    assert_eq!(set(TIME_ATIME, [1, 1, 0]), Err(EINVAL), "a time not chosen must be 0");
    assert_eq!(set(0, [0, 0, 0]), Ok(Request::SetTimes { ino: n(12), atime: None, mtime: None, ctime: None }));
}

#[test]
fn transfers_are_bounded() {
    let read = |offset: u64, len: u32| Request::decode(&Desc { op: op::READ, object: 12, offset, len, grant: 1, ..Desc::default() });
    assert!(read(0, MAX_TRANSFER).is_ok());
    assert_eq!(read(0, MAX_TRANSFER + 1), Err(EINVAL));
    assert_eq!(read(u64::MAX - 10, 11), Err(EINVAL));
    assert!(read(u64::MAX - 10, 10).is_ok());
    // A handle is the inode's number (low half) and generation (high half).
    let d = Desc { op: op::STAT, object: 7 << 32 | 12, ..Desc::default() };
    assert_eq!(Request::decode(&d), Ok(Request::Stat { ino: Node::new(12, 7) }));
    // A promise covers at most one transfer.
    let promise = |offset: u64, len: u64| Request::decode(&Desc { op: op::PROMISE, object: 12, offset, arg: [len, 0, 0], ..Desc::default() });
    assert!(promise(0, MAX_TRANSFER as u64).is_ok());
    assert_eq!(promise(0, MAX_TRANSFER as u64 + 1), Err(EINVAL));
    assert_eq!(promise(u64::MAX, 1), Err(EINVAL));
    // Result buffers are not empty.
    let d = Desc { op: op::READDIR, object: 2, grant: 1, ..Desc::default() };
    assert_eq!(Request::decode(&d), Err(EINVAL));
}

#[test]
fn names_and_targets_are_bounded() {
    let lookup = |len: u32| Request::decode(&Desc { op: op::LOOKUP, object: 2, grant: 1, len, ..Desc::default() });
    assert_eq!(lookup(0), Err(EINVAL));
    assert!(lookup(NAME_MAX).is_ok());
    assert_eq!(lookup(NAME_MAX + 1), Err(ENAMETOOLONG));
    let symlink = |target: u64| Request::decode(&Desc { op: op::CREATE, object: 2, grant: 1, len: 3, arg: [KIND_SYMLINK, 0o777, target], ..Desc::default() });
    assert_eq!(symlink(0), Err(EINVAL));
    assert_eq!(symlink(TARGET_MAX as u64 + 1), Err(ENAMETOOLONG));
    assert!(symlink(TARGET_MAX as u64).is_ok());
    // The target follows the name: it must stay addressable in the grant.
    let d = Desc { op: op::CREATE, object: 2, grant: 1, buf_off: u32::MAX - 1, len: 3, arg: [KIND_SYMLINK, 0, 1], ..Desc::default() };
    assert_eq!(Request::decode(&d), Err(EINVAL));
    // A file, directory or socket has no target; kinds and permissions
    // are known.
    for arg in [[KIND_FILE, 0, 1], [KIND_DIR, 0, 1], [KIND_SOCKET, 0, 1], [0, 0, 0], [5, 0, 0], [KIND_FILE, 0o10000, 0]] {
        let d = Desc { op: op::CREATE, object: 2, grant: 1, len: 3, arg, ..Desc::default() };
        assert_eq!(Request::decode(&d), Err(EINVAL), "{arg:?}");
    }
    let rename = |new: u64| Request::decode(&Desc { op: op::RENAME, object: 2, grant: 1, len: 3, arg: [2, new, 0], ..Desc::default() });
    assert_eq!(rename(0), Err(EINVAL));
    assert_eq!(rename(NAME_MAX as u64 + 1), Err(ENAMETOOLONG));
    assert!(rename(NAME_MAX as u64).is_ok());
    let unlink = |flag: u64| Request::decode(&Desc { op: op::UNLINK, object: 2, grant: 1, len: 3, arg: [flag, 0, 0], ..Desc::default() });
    assert!(unlink(1).is_ok());
    assert_eq!(unlink(2), Err(EINVAL));
}

#[test]
fn names_are_utf8_without_slashes_or_nuls() {
    assert_eq!(check_name(b"hello.txt"), Ok("hello.txt"));
    assert_eq!(check_name(b".."), Ok(".."));
    for bad in [&b""[..], b"a/b", b"/", b"a\0b", b"\xff\xfe"] {
        assert_eq!(check_name(bad), Err(EINVAL), "{bad:?}");
    }
    assert_eq!(check_target(b"../a/b"), Ok("../a/b"));
    assert_eq!(check_target(b"a\0"), Err(EINVAL));
    assert_eq!(check_target(b""), Err(EINVAL));
}

#[test]
fn completions_stats_and_usage_round_trip() {
    let c = Completion { tag: 0xdead_beef, op: op::READ, status: -5, values: [1, u64::MAX, 3, 1 << 63] };
    assert_eq!(Completion::from_desc(&c.to_desc()), c);
    let s = Stat { mode: 0o100644, links: 3, size: 1 << 40, atime: 1, mtime: u32::MAX, ctime: 7, generation: u32::MAX - 1 };
    assert_eq!(Stat::from_values(&s.to_values()), s);
    let u = Usage { block_size: 1024, blocks: 65536, free_blocks: 100, inodes: 16384, free_inodes: u32::MAX, max_file_size: 16 << 30 };
    assert_eq!(Usage::from_values(&u.to_values()), u);
}

#[test]
fn directory_entries_pack_and_parse() {
    let mut out = [0u8; 40];
    let a = put_dirent(&mut out, 2, TYPE_DIR, b".").unwrap();
    let b = put_dirent(&mut out[a..], 12, TYPE_FILE, b"README.txt").unwrap();
    assert_eq!((a, b), (7, 16));
    // No room for another of 6 + 20 bytes.
    assert_eq!(put_dirent(&mut out[a + b..], 13, TYPE_FILE, &[b'x'; 20]), None);
    assert_eq!(put_dirent(&mut [0u8; 300], 1, TYPE_FILE, &[b'x'; 256]), None);
    let parsed: Vec<_> = dirents(&out[..a + b]).collect();
    assert_eq!(parsed, vec![(2, TYPE_DIR, &b"."[..]), (12, TYPE_FILE, &b"README.txt"[..])]);
    // A truncated entry ends the list.
    assert_eq!(dirents(&out[..a + 10]).count(), 1);
}
