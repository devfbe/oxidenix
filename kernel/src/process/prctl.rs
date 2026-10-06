//! prctl, capget and capset. oxidenix has no users or privileges yet:
//! every process runs as root with the full capability set, which these
//! calls report truthfully.

use super::errno::*;
use super::sched::current;
use super::uaccess;

/// Highest capability Linux defines (CAP_CHECKPOINT_RESTORE).
const CAP_LAST: u32 = 40;
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const CAPABILITY_VERSION_2: u32 = 0x2007_1026;
const CAPABILITY_VERSION_1: u32 = 0x1998_0330;

/// The full set as the two 32-bit words of a v2/v3 `cap_user_data`.
fn full_set() -> [u32; 2] {
    let all = (1u64 << (CAP_LAST + 1)) - 1;
    [all as u32, (all >> 32) as u32]
}

/// Checks the header's version; an unknown one is answered with the
/// preferred version and EINVAL, as Linux does. Returns the data words.
fn check_header(hdr: u64) -> Result<usize, i64> {
    let [version, pid]: [u32; 2] = uaccess::read(hdr)?;
    let words = match version {
        CAPABILITY_VERSION_1 => 1,
        CAPABILITY_VERSION_2 | CAPABILITY_VERSION_3 => 2,
        _ => {
            uaccess::write(hdr, CAPABILITY_VERSION_3)?;
            return Err(EINVAL);
        }
    };
    if pid as i32 > 0 && super::task(pid as u64).is_none() {
        return Err(ESRCH);
    }
    Ok(words)
}

/// capget(hdr, data): effective and permitted are everything, inheritable
/// is empty.
pub fn capget(hdr: u64, data: u64) -> SysResult {
    let words = check_header(hdr)?;
    if data == 0 {
        return Ok(0);
    }
    let set = full_set();
    for i in 0..words {
        // struct { effective, permitted, inheritable }
        uaccess::write(data + i as u64 * 12, [set[i], set[i], 0u32])?;
    }
    Ok(0)
}

/// capset(hdr, data): any subset of the full set is accepted; there is
/// nothing to drop yet, since no check in the kernel looks at capabilities.
pub fn capset(hdr: u64, data: u64) -> SysResult {
    let words = check_header(hdr)?;
    let set = full_set();
    for i in 0..words {
        let [effective, permitted, inheritable]: [u32; 3] = uaccess::read(data + i as u64 * 12)?;
        if (effective | permitted | inheritable) & !set[i] != 0 || effective & !permitted != 0 {
            return Err(EPERM);
        }
    }
    Ok(0)
}

const PR_SET_PDEATHSIG: u64 = 1;
const PR_GET_PDEATHSIG: u64 = 2;
const PR_GET_DUMPABLE: u64 = 3;
const PR_SET_DUMPABLE: u64 = 4;
const PR_SET_NAME: u64 = 15;
const PR_GET_NAME: u64 = 16;
const PR_CAPBSET_READ: u64 = 23;
const PR_SET_NO_NEW_PRIVS: u64 = 38;
const PR_GET_NO_NEW_PRIVS: u64 = 39;

/// Linux keeps process names (comm) to 15 bytes plus NUL.
const COMM_LEN: usize = 16;

pub fn prctl(option: u64, arg2: u64) -> SysResult {
    let me = current();
    match option {
        PR_SET_PDEATHSIG => {
            if arg2 > 64 {
                return Err(EINVAL);
            }
            me.info.lock().pdeath_sig = arg2 as u32;
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            let sig = me.info.lock().pdeath_sig;
            uaccess::write(arg2, sig as i32)?;
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(me.info.lock().dumpable as i64),
        PR_SET_DUMPABLE => {
            if arg2 > 1 {
                return Err(EINVAL);
            }
            me.info.lock().dumpable = arg2 == 1;
            Ok(0)
        }
        PR_SET_NAME => {
            let mut raw = [0u8; COMM_LEN];
            for (i, b) in raw.iter_mut().enumerate().take(COMM_LEN - 1) {
                *b = uaccess::read(arg2 + i as u64)?;
                if *b == 0 {
                    break;
                }
            }
            let len = raw.iter().position(|&b| b == 0).unwrap_or(COMM_LEN - 1);
            me.info.lock().name = alloc::string::String::from_utf8_lossy(&raw[..len]).into_owned();
            Ok(0)
        }
        PR_GET_NAME => {
            let mut raw = [0u8; COMM_LEN];
            let name = me.info.lock().name.clone();
            let n = name.len().min(COMM_LEN - 1);
            raw[..n].copy_from_slice(&name.as_bytes()[..n]);
            uaccess::write(arg2, raw)?;
            Ok(0)
        }
        PR_CAPBSET_READ => {
            if arg2 > CAP_LAST as u64 {
                return Err(EINVAL);
            }
            Ok(1)
        }
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 {
                return Err(EINVAL);
            }
            me.info.lock().no_new_privs = true;
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => Ok(me.info.lock().no_new_privs as i64),
        _ => Err(EINVAL),
    }
}
