//! Where positional and vectored reads and writes go: preadv2/pwritev2's
//! flags against the descriptor's O_APPEND, as Linux decides them.

use vfs::rw::*;

const EINVAL: i64 = 22;
const EOPNOTSUPP: i64 = 95;

#[test]
fn offset_minus_one_uses_the_file_position() {
    assert_eq!(plan(false, -1, 0, false), Ok(Plan { at: None, append: false, sync: false }));
    assert_eq!(plan(true, -1, 0, false), Ok(Plan { at: None, append: false, sync: false }));
}

#[test]
fn an_offset_is_positional() {
    assert_eq!(plan(false, 4096, 0, false), Ok(Plan { at: Some(4096), append: false, sync: false }));
    assert_eq!(plan(true, 0, 0, false), Ok(Plan { at: Some(0), append: false, sync: false }));
}

#[test]
fn offsets_below_minus_one_are_invalid() {
    assert_eq!(plan(false, -2, 0, false), Err(EINVAL));
    assert_eq!(plan(true, i64::MIN, 0, false), Err(EINVAL));
}

#[test]
fn o_append_appends_even_at_an_offset() {
    // Linux's pwrite on an O_APPEND descriptor writes at the end.
    assert_eq!(plan(true, 100, 0, true), Ok(Plan { at: Some(100), append: true, sync: false }));
    assert_eq!(plan(true, -1, 0, true), Ok(Plan { at: None, append: true, sync: false }));
}

#[test]
fn rwf_noappend_overrides_o_append() {
    assert_eq!(plan(true, 100, RWF_NOAPPEND, true), Ok(Plan { at: Some(100), append: false, sync: false }));
    assert_eq!(plan(true, -1, RWF_NOAPPEND, true), Ok(Plan { at: None, append: false, sync: false }));
}

#[test]
fn rwf_append_appends_without_o_append() {
    assert_eq!(plan(true, 100, RWF_APPEND, false), Ok(Plan { at: Some(100), append: true, sync: false }));
}

#[test]
fn append_and_noappend_together_are_invalid() {
    assert_eq!(plan(true, 0, RWF_APPEND | RWF_NOAPPEND, false), Err(EINVAL));
}

#[test]
fn reads_ignore_the_append_flags() {
    assert_eq!(plan(false, 0, RWF_APPEND, true), Ok(Plan { at: Some(0), append: false, sync: false }));
}

#[test]
fn sync_flags_ask_for_durability_of_writes() {
    assert_eq!(plan(true, 0, RWF_DSYNC, false), Ok(Plan { at: Some(0), append: false, sync: true }));
    assert_eq!(plan(true, 0, RWF_SYNC, false), Ok(Plan { at: Some(0), append: false, sync: true }));
    assert_eq!(plan(false, 0, RWF_SYNC, false), Ok(Plan { at: Some(0), append: false, sync: false }));
    assert_eq!(plan(true, 0, RWF_HIPRI, false), Ok(Plan { at: Some(0), append: false, sync: false }));
}

#[test]
fn unsupported_flags_are_eopnotsupp() {
    assert_eq!(plan(false, 0, RWF_NOWAIT, false), Err(EOPNOTSUPP));
    assert_eq!(plan(true, 0, 0x40, false), Err(EOPNOTSUPP));
    assert_eq!(plan(true, 0, 1 << 40, false), Err(EOPNOTSUPP));
}
