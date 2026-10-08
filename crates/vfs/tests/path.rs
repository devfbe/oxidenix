//! Lexical path normalization, as the Linux server resolves paths.

use vfs::cpio;
use vfs::path::{join, normalize};

fn n(cwd: &str, p: &str) -> String {
    join(&normalize(cwd, p))
}

#[test]
fn normalizes_like_the_kernel() {
    assert_eq!(n("/", "/"), "/");
    assert_eq!(n("/a/b", "c"), "/a/b/c");
    assert_eq!(n("/a/b", "../c"), "/a/c");
    assert_eq!(n("/a/b", "/x/./y//z/"), "/x/y/z");
    assert_eq!(n("/", "../../.."), "/");
    assert_eq!(n("/a", "b/../../.."), "/");
    assert_eq!(n("/a", ""), "/a");
}

fn member(name: &str, mode: u32, data: &[u8]) -> Vec<u8> {
    let mut h = format!("070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}", 1, mode, 0, 0, 1, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0).into_bytes();
    h.extend_from_slice(name.as_bytes());
    h.push(0);
    while h.len() % 4 != 0 {
        h.push(0);
    }
    h.extend_from_slice(data);
    while h.len() % 4 != 0 {
        h.push(0);
    }
    h
}

#[test]
fn reads_newc_archives() {
    let mut a = member("./bin", 0o040755, b"");
    a.extend(member("bin/hello", 0o100755, b"hello world"));
    a.extend(member("bin/sh", 0o120777, b"busybox"));
    a.extend(member("TRAILER!!!", 0, b""));
    let e: Vec<_> = cpio::entries(&a).collect::<Result<_, _>>().unwrap();
    assert_eq!(e.len(), 3);
    assert_eq!((e[0].name, e[0].mode), ("bin", 0o040755));
    assert_eq!(&a[e[1].data.clone()], b"hello world");
    assert_eq!((e[2].name, &a[e[2].data.clone()]), ("bin/sh", &b"busybox"[..]));
}

#[test]
fn refuses_broken_archives() {
    let mut a = member("x", 0o100644, b"data");
    a.truncate(a.len() - 6);
    assert!(cpio::entries(&a).any(|r| r.is_err()));
    assert!(cpio::entries(b"070702").next().unwrap().is_err());
}
