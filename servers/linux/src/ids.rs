//! Credentials and process attributes (phase R8): the user and group ids, supplementary
//! groups, capabilities, prctl, the resource usage calls (getrusage, times), the calls
//! that name a thread or process for the kernel (sched_getaffinity, sched_setaffinity), and
//! the resource limits but RLIMIT_NOFILE, which is the descriptor table's (`fdtable`):
//! prlimit64, getrlimit and setrlimit (R9).
//!
//! oxidenix has one user, root (uid and gid 0, every capability): the id getters answer 0,
//! setting an id is accepted (there is no other user to become), a process has no
//! supplementary groups (as one that init starts on Linux), and a list set with setgroups is
//! checked but not kept.

use crate::local;
use crate::process::{self, Pid, PROCS};
use crate::syscall;
use crate::usercopy;
use restricted::*;

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EINVAL: i64 = 22;

const SYS_GETRUSAGE: u64 = 98;
const SYS_TIMES: u64 = 100;
const SYS_GETUID: u64 = 102;
const SYS_GETGID: u64 = 104;
const SYS_SETUID: u64 = 105;
const SYS_SETGID: u64 = 106;
const SYS_GETEUID: u64 = 107;
const SYS_GETEGID: u64 = 108;
const SYS_SETREUID: u64 = 113;
const SYS_SETREGID: u64 = 114;
const SYS_GETGROUPS: u64 = 115;
const SYS_SETGROUPS: u64 = 116;
const SYS_SETRESUID: u64 = 117;
const SYS_GETRESUID: u64 = 118;
const SYS_SETRESGID: u64 = 119;
const SYS_GETRESGID: u64 = 120;
const SYS_SETFSUID: u64 = 122;
const SYS_SETFSGID: u64 = 123;
const SYS_CAPGET: u64 = 125;
const SYS_CAPSET: u64 = 126;
const SYS_PRCTL: u64 = 157;
const SYS_SCHED_SETAFFINITY: u64 = 203;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_PRLIMIT64: u64 = 302;
const SYS_GETRLIMIT: u64 = 97;
const SYS_SETRLIMIT: u64 = 160;
/// Linux's NGROUPS_MAX.
const NGROUPS_MAX: u64 = 65536;

/// The result of a credentials or attribute call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3) = (s.rdi, s.rsi, s.rdx, s.r10);
    let result = match s.rax {
        SYS_GETUID | SYS_GETGID | SYS_GETEUID | SYS_GETEGID => Ok(0),
        SYS_SETUID | SYS_SETGID | SYS_SETREUID | SYS_SETREGID | SYS_SETRESUID | SYS_SETRESGID => Ok(0),
        // setfsuid and setfsgid answer the previous id.
        SYS_SETFSUID | SYS_SETFSGID => Ok(0),
        SYS_GETRESUID | SYS_GETRESGID => getres(a0, a1, a2),
        // getgroups(size, list): the number of groups, none here (any size holds them; a
        // negative one is EINVAL).
        SYS_GETGROUPS if (a0 as i32) < 0 => Err(EINVAL),
        SYS_GETGROUPS => Ok(0),
        SYS_SETGROUPS => setgroups(a0, a1),
        SYS_CAPGET => capget(a0, a1),
        SYS_CAPSET => capset(a0, a1),
        SYS_PRCTL => prctl(a0, a1),
        SYS_GETRUSAGE => getrusage(a0 as i32, a1),
        SYS_TIMES => times(a0),
        SYS_SCHED_GETAFFINITY => getaffinity(a0 as i32, a1, a2),
        SYS_SCHED_SETAFFINITY => setaffinity(a0 as i32, a1, a2),
        SYS_PRLIMIT64 => process_exists(a0 as i32).and_then(|_| prlimit(a0 as Pid, a1, a2, a3)),
        SYS_GETRLIMIT => prlimit(0, a0, 0, a1),
        SYS_SETRLIMIT => prlimit(0, a0, a1, 0),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// The resource limits (Linux's RLIMIT_ numbers, but RLIMIT_NOFILE).
const RLIM_NLIMITS: u64 = 16;
const RLIM_INFINITY: u64 = u64::MAX;
const RLIMIT_STACK: u64 = 3;
const RLIMIT_CORE: u64 = 4;
const RLIMIT_MEMLOCK: u64 = 8;
const RLIMIT_SIGPENDING: u64 = 11;
const RLIMIT_MSGQUEUE: u64 = 12;
const RLIMIT_NICE: u64 = 13;
const RLIMIT_RTPRIO: u64 = 14;

/// The (soft, hard) limit of `resource` a tree's first process starts with: Linux's
/// defaults for a process init starts, where the server holds to them (an execve's stack
/// grows to 8 MiB, no core files are written, the instance queues at most 4096 real-time
/// signals); nothing else is limited.
fn limit(resource: u64) -> (u64, u64) {
    match resource {
        RLIMIT_STACK => (8 << 20, RLIM_INFINITY),
        RLIMIT_CORE => (0, RLIM_INFINITY),
        RLIMIT_MEMLOCK => (8 << 20, 8 << 20),
        RLIMIT_SIGPENDING => (4096, 4096),
        RLIMIT_MSGQUEUE => (819_200, 819_200),
        RLIMIT_NICE | RLIMIT_RTPRIO => (0, 0),
        _ => (RLIM_INFINITY, RLIM_INFINITY),
    }
}

/// A process's resource limits but RLIMIT_NOFILE (the descriptor table's), as (soft, hard)
/// by Linux's RLIMIT_ number: kept in its record (`process::Proc::limits`), inherited by
/// fork and clone, kept by execve, as Linux keeps them per process (its signal struct).
#[derive(Clone, Copy)]
pub struct Limits(pub [(u64, u64); RLIM_NLIMITS as usize]);

impl Default for Limits {
    fn default() -> Limits {
        Limits(core::array::from_fn(|r| limit(r as u64)))
    }
}

/// prlimit64(pid, resource, new, old) for process `pid` (0: the caller's; a thread's id
/// names its process; checked to exist by the caller), and getrlimit and setrlimit: the
/// limits before at `old`, the new ones taken (EINVAL for a soft limit above the hard one;
/// the one user is root, who may raise them). The program's memory is read and written
/// with no lock held.
fn prlimit(pid: Pid, resource: u64, new: u64, old: u64) -> Result<i64, i64> {
    if resource >= RLIM_NLIMITS {
        return Err(EINVAL);
    }
    let new = if new != 0 {
        let [soft, hard]: [u64; 2] = usercopy::read(new)?;
        if soft > hard {
            return Err(EINVAL);
        }
        Some((soft, hard))
    } else {
        None
    };
    let before = {
        let mut t = PROCS.lock();
        let id = if pid == 0 { local::pid() } else { pid };
        let target = t.threads.get(&id).map_or(id, |th| th.pid);
        let p = t.procs.get_mut(&target).ok_or(ESRCH)?;
        let slot = &mut p.limits.0[resource as usize];
        let before = *slot;
        if let Some(n) = new {
            *slot = n;
        }
        before
    };
    if old != 0 {
        usercopy::write(old, &[before.0, before.1])?;
    }
    Ok(0)
}

/// getresuid and getresgid: real, effective and saved are all 0.
fn getres(r: u64, e: u64, s: u64) -> Result<i64, i64> {
    for ptr in [r, e, s] {
        usercopy::write(ptr, &0u32)?;
    }
    Ok(0)
}

/// setgroups(size, list): checks the list (EINVAL for too many, EFAULT for a bad one).
fn setgroups(size: u64, list: u64) -> Result<i64, i64> {
    if size > NGROUPS_MAX {
        return Err(EINVAL);
    }
    for i in 0..size {
        usercopy::read::<u32>(list + i * 4)?;
    }
    Ok(0)
}

/// ESRCH unless `pid` (0: the caller) names a process or thread of the instance.
fn process_exists(pid: i32) -> Result<(), i64> {
    if pid < 0 {
        return Err(ESRCH);
    }
    if pid == 0 || process::exists(pid as Pid) {
        Ok(())
    } else {
        Err(ESRCH)
    }
}

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

/// Checks the header's version; an unknown one is answered with the preferred version and
/// EINVAL, as Linux does. Returns the data words.
fn check_header(hdr: u64) -> Result<usize, i64> {
    let [version, pid]: [u32; 2] = usercopy::read(hdr)?;
    let words = match version {
        CAPABILITY_VERSION_1 => 1,
        CAPABILITY_VERSION_2 | CAPABILITY_VERSION_3 => 2,
        _ => {
            usercopy::write(hdr, &CAPABILITY_VERSION_3)?;
            return Err(EINVAL);
        }
    };
    if pid as i32 > 0 {
        process_exists(pid as i32)?;
    }
    Ok(words)
}

/// capget(hdr, data): effective and permitted are everything, inheritable is empty.
fn capget(hdr: u64, data: u64) -> Result<i64, i64> {
    let words = check_header(hdr)?;
    if data == 0 {
        return Ok(0);
    }
    let set = full_set();
    for (i, &word) in set.iter().enumerate().take(words) {
        // struct { effective, permitted, inheritable }
        usercopy::write(data + i as u64 * 12, &[word, word, 0u32])?;
    }
    Ok(0)
}

/// capset(hdr, data): any subset of the full set is accepted; nothing looks at
/// capabilities yet.
fn capset(hdr: u64, data: u64) -> Result<i64, i64> {
    let words = check_header(hdr)?;
    let set = full_set();
    for (i, &word) in set.iter().enumerate().take(words) {
        let [effective, permitted, inheritable]: [u32; 3] = usercopy::read(data + i as u64 * 12)?;
        if (effective | permitted | inheritable) & !word != 0 || effective & !permitted != 0 {
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
const PR_SET_CHILD_SUBREAPER: u64 = 36;
const PR_GET_CHILD_SUBREAPER: u64 = 37;
const PR_SET_NO_NEW_PRIVS: u64 = 38;
const PR_GET_NO_NEW_PRIVS: u64 = 39;
const PR_GET_TID_ADDRESS: u64 = 40;

fn prctl(option: u64, arg2: u64) -> Result<i64, i64> {
    let (tid, pid) = process::me();
    match option {
        PR_SET_PDEATHSIG => {
            if arg2 > 64 {
                return Err(EINVAL);
            }
            with_proc(pid, |p| p.pdeath = arg2 as u32)?;
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            let sig = with_proc(pid, |p| p.pdeath)?;
            usercopy::write(arg2, &(sig as i32))?;
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(with_proc(pid, |p| p.dumpable)? as i64),
        PR_SET_DUMPABLE => {
            if arg2 > 1 {
                return Err(EINVAL);
            }
            with_proc(pid, |p| p.dumpable = arg2 == 1)?;
            Ok(0)
        }
        PR_SET_NAME => {
            // At most 15 bytes and the NUL, read a byte at a time (the name may end just
            // before an unmapped page).
            let mut comm = [0u8; 16];
            for i in 0..15 {
                let b: u8 = usercopy::read(arg2 + i as u64)?;
                if b == 0 {
                    break;
                }
                comm[i] = b;
            }
            let key = {
                let mut t = PROCS.lock();
                let th = t.threads.get_mut(&tid).ok_or(ESRCH)?;
                th.comm = comm;
                th.key
            };
            process::name_kernel_thread(key, &comm);
            Ok(0)
        }
        PR_GET_NAME => {
            let comm = PROCS.lock().threads.get(&tid).map_or([0; 16], |th| th.comm);
            usercopy::write(arg2, &comm)?;
            Ok(0)
        }
        PR_CAPBSET_READ => {
            if arg2 > CAP_LAST as u64 {
                return Err(EINVAL);
            }
            Ok(1)
        }
        PR_SET_CHILD_SUBREAPER => {
            with_proc(pid, |p| p.subreaper = arg2 != 0)?;
            Ok(0)
        }
        PR_GET_CHILD_SUBREAPER => {
            let on = with_proc(pid, |p| p.subreaper)?;
            usercopy::write(arg2, &(on as i32))?;
            Ok(0)
        }
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 {
                return Err(EINVAL);
            }
            with_proc(pid, |p| p.no_new_privs = true)?;
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => Ok(with_proc(pid, |p| p.no_new_privs)? as i64),
        // The CLONE_CHILD_CLEARTID word is the kernel's: not offered (as Linux without
        // CONFIG_CHECKPOINT_RESTORE).
        PR_GET_TID_ADDRESS => Err(EINVAL),
        _ => Err(EINVAL),
    }
}

fn with_proc<R>(pid: Pid, f: impl FnOnce(&mut process::Proc) -> R) -> Result<R, i64> {
    let mut t = PROCS.lock();
    t.procs.get_mut(&pid).map(f).ok_or(ESRCH)
}

/// The kernel's account of the calling process (0) or thread.
fn proc_info() -> ProcInfo {
    let mut info = ProcInfo::default();
    syscall(SYS_PROC_INFO, [0, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
    info
}

/// getrusage(who, usage): the process (with what its ended threads used), the thread, or
/// the reaped children (with theirs).
fn getrusage(who: i32, usage: u64) -> Result<i64, i64> {
    const RUSAGE_SELF: i32 = 0;
    const RUSAGE_CHILDREN: i32 = -1;
    const RUSAGE_THREAD: i32 = 1;
    let u = match who {
        RUSAGE_SELF => {
            let info = proc_info();
            process::Usage { user_ns: info.user_ns, system_ns: info.system_ns, peak_pages: info.peak_pages }
        }
        RUSAGE_THREAD => {
            let mut info = ThreadInfo::default();
            syscall(SYS_THREAD_INFO, [0, &mut info as *mut ThreadInfo as u64, 0, 0, 0, 0]);
            process::Usage { user_ns: info.user_ns, system_ns: info.system_ns, peak_pages: 0 }
        }
        RUSAGE_CHILDREN => PROCS.lock().procs.get(&local::pid()).map(|p| p.children_usage).unwrap_or_default(),
        _ => return Err(EINVAL),
    };
    process::write_rusage(usage, u)?;
    Ok(0)
}

/// times(buf): CPU times in clock ticks (USER_HZ); returns the ticks since boot.
fn times(buf: u64) -> Result<i64, i64> {
    const TICK: u64 = 10_000_000;
    if buf != 0 {
        let info = proc_info();
        let children = PROCS.lock().procs.get(&local::pid()).map(|p| p.children_usage).unwrap_or_default();
        usercopy::write(buf, &[info.user_ns / TICK, info.system_ns / TICK, children.user_ns / TICK, children.system_ns / TICK])?;
    }
    Ok((syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64 / TICK) as i64)
}

/// The key of the thread a scheduling call names (0: the caller; ESRCH for none).
fn thread_key(tid: i32) -> Result<u64, i64> {
    if tid < 0 {
        return Err(ESRCH);
    }
    process::key_of(tid as Pid).ok_or(ESRCH)
}

/// sched_getaffinity(tid, size, mask): the CPUs the thread may run on, as a 64-bit mask;
/// returns the bytes written, as Linux does.
fn getaffinity(tid: i32, size: u64, mask: u64) -> Result<i64, i64> {
    if size < 8 {
        return Err(EINVAL);
    }
    let key = thread_key(tid)?;
    let cpus = syscall(SYS_THREAD_AFFINITY, [key, 0, 0, 0, 0, 0]);
    if cpus < 0 {
        return Err(-cpus);
    }
    usercopy::write(mask, &(cpus as u64))?;
    Ok(8)
}

/// sched_setaffinity(tid, size, mask): restricts the thread to the CPUs in `mask` that run.
fn setaffinity(tid: i32, size: u64, mask: u64) -> Result<i64, i64> {
    if size == 0 {
        return Err(EINVAL);
    }
    let mut bytes = [0u8; 8];
    let n = size.min(8) as usize;
    usercopy::from_program(mask, &mut bytes[..n])?;
    let key = thread_key(tid)?;
    let r = syscall(SYS_THREAD_AFFINITY, [key, 1, u64::from_le_bytes(bytes), 0, 0, 0]);
    if r < 0 { Err(-r) } else { Ok(0) }
}
