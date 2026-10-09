//! Supplementary groups: getgroups and setgroups. oxidenix has one user,
//! root (uid and gid 0, every capability; the kernel answers the uid and
//! gid calls until the process model is the server's, R8): a process has
//! no supplementary groups, as one that init starts on Linux, and a list
//! set with setgroups is checked but not kept.

use crate::usercopy;

const EINVAL: i64 = 22;
const SYS_GETGROUPS: u64 = 115;
const SYS_SETGROUPS: u64 = 116;
/// Linux's NGROUPS_MAX.
const NGROUPS_MAX: u64 = 65536;

/// The result of a group call in `s`, or None for other calls.
pub fn handle(s: &restricted::State) -> Option<i64> {
    let result = match s.rax {
        // getgroups(size, list): the number of groups, none here (any
        // size holds them; a negative one is EINVAL).
        SYS_GETGROUPS if (s.rdi as i32) < 0 => Err(EINVAL),
        SYS_GETGROUPS => Ok(0),
        SYS_SETGROUPS => setgroups(s.rdi, s.rsi),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// setgroups(size, list): checks the list (EINVAL for too many, EFAULT for
/// a bad one).
fn setgroups(size: u64, list: u64) -> Result<i64, i64> {
    if size > NGROUPS_MAX {
        return Err(EINVAL);
    }
    for i in 0..size {
        usercopy::read::<u32>(list + i * 4)?;
    }
    Ok(0)
}
