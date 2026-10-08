use vfs::cpio::{entries, header, member_path, HEADER_LEN, TRAILER};

/// One newc member: header, name with NUL, padding, data, padding.
fn member(out: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
    let fields = [1, mode, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
    out.extend_from_slice(b"070701");
    for f in fields {
        out.extend_from_slice(format!("{f:08X}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn archive() -> Vec<u8> {
    let mut a = Vec::new();
    member(&mut a, "./bin", 0o040755, b"");
    member(&mut a, "./bin/hello", 0o100755, b"hello world");
    member(&mut a, "/bin/sh", 0o120777, b"hello");
    member(&mut a, TRAILER, 0, b"");
    a
}

#[test]
fn entries_up_to_the_trailer() {
    let a = archive();
    let got: Vec<_> = entries(&a).map(|e| e.unwrap()).map(|e| (e.name.to_string(), e.mode, a[e.data].to_vec())).collect();
    assert_eq!(
        got,
        vec![
            ("bin".to_string(), 0o040755, vec![]),
            ("bin/hello".to_string(), 0o100755, b"hello world".to_vec()),
            ("bin/sh".to_string(), 0o120777, b"hello".to_vec()),
        ]
    );
}

#[test]
fn piece_by_piece_matches_the_iterator() {
    let a = archive();
    let mut pos = 0;
    let mut names = Vec::new();
    loop {
        let h = header(&a[pos..]).unwrap();
        let name = std::str::from_utf8(&a[pos + HEADER_LEN..pos + HEADER_LEN + h.namesize - 1]).unwrap();
        if name == TRAILER {
            break;
        }
        let start = h.data_start(pos);
        names.push((member_path(name).to_string(), a[start..start + h.filesize].to_vec()));
        pos = h.next(pos).unwrap();
    }
    assert_eq!(names[1], ("bin/hello".to_string(), b"hello world".to_vec()));
    assert_eq!(names.len(), 3);
}

#[test]
fn corrupt_archives_are_errors() {
    let mut a = archive();
    assert!(header(&a[..50]).is_err());
    a[0] = b'x';
    assert!(header(&a).is_err());
    assert!(entries(&a).next().unwrap().is_err());
    let mut b = archive();
    b.truncate(HEADER_LEN + 2); // inside the first name
    assert!(entries(&b).next().unwrap().is_err());
}
