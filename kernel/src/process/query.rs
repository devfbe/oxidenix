//! proc_query (syscall 1005): the kernel's native process and system
//! information for the procfs server (records in `procproto`): the
//! system-wide record only, for it and for the Linux server
//! (`restricted::SYS_SYSTEM_INFO`), whose /proc/<pid> comes from its own
//! process table (R8). Only privileged servers and the Linux server may
//! ask; they decide what programs see.

use super::errno::*;
use super::sched::{self, TABLE};
use super::task::State;
use super::{uaccess, TIMER_HZ};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use procproto::*;

fn system() -> System {
    let mut s = System { hz: TIMER_HZ, uptime: sched::ticks(), page_size: 4096, ..Default::default() };
    s.boot_time = crate::time::boot_time();
    s.tsc_hz = crate::time::tsc_hz();
    for i in 0..MAX_CPUS {
        let Some(cpu) = crate::smp::by_index(i) else { continue };
        let st = sched::cpu_stats(cpu);
        s.cpu[i] = CpuTimes { user: st.user, system: st.system, idle: st.idle };
        s.context_switches += st.switches;
        s.cpus += 1;
    }
    let mem = crate::memory::stats();
    s.mem_total = mem.total_frames * 4096;
    s.mem_free = (mem.total_frames - mem.used_frames) * 4096;
    // In use (free slots and free heap blocks are free memory).
    s.kernel_heap = mem.heap_used as u64;
    s.load = sched::loadavg();
    s.load_shift = sched::LOAD_SHIFT as u64;
    {
        let table = TABLE.lock();
        s.processes = table.groups.len() as u64;
        s.threads = table.tasks.len() as u64;
        s.running = table.tasks.values().filter(|t| matches!(t.state(), State::Running | State::Runnable)).count() as u64;
        s.max_pid = table.next_pid;
    }
    s.forks = sched::forks();
    let (committed, limit) = crate::memory::commit_stats();
    s.committed = committed * 4096;
    s.commit_limit = limit * 4096;
    let shmem = crate::fs::cache::tmpfs_usage().0;
    s.shmem = shmem * 4096;
    s.cached = (crate::memory::cached_pages() + shmem) * 4096;
    s.dirty = crate::fs::cache::dirty_pages() * 4096;
    s.counters = crate::counters::snapshot();
    s
}

/// sysinfo(2): uptime, load and memory in the layout of `struct sysinfo`.
pub fn sysinfo(buf: u64) -> SysResult {
    const SI_LOAD_SHIFT: u32 = 16;
    let s = system();
    let mut out = [0u8; 112];
    let put = |out: &mut [u8; 112], at: usize, v: u64| out[at..at + 8].copy_from_slice(&v.to_le_bytes());
    put(&mut out, 0, s.uptime / TIMER_HZ);
    for i in 0..3 {
        put(&mut out, 8 + i * 8, s.load[i] << (SI_LOAD_SHIFT - sched::LOAD_SHIFT));
    }
    put(&mut out, 32, s.mem_total);
    put(&mut out, 40, s.mem_free);
    out[80..82].copy_from_slice(&(s.processes.min(u16::MAX as u64) as u16).to_le_bytes());
    // mem_unit: the sizes above are in bytes.
    out[104..108].copy_from_slice(&1u32.to_le_bytes());
    uaccess::write(buf, out)?;
    Ok(0)
}

/// The answer to `op` for a buffer of `len` bytes: the system-wide record
/// (`QUERY_SYSTEM`; the processes' records are their Linux server's since
/// R8); ERANGE if it does not fit.
fn answer(op: u64, len: u64) -> Result<Vec<u8>, i64> {
    if op != QUERY_SYSTEM {
        return Err(EINVAL);
    }
    let bytes = as_bytes(&system()).to_vec();
    if bytes.len() as u64 > len {
        return Err(ERANGE);
    }
    Ok(bytes)
}

/// proc_query(op, arg, buf, len): writes the answer to `buf` and returns
/// its length; ERANGE if it does not fit. For the kernel's servers
/// (procfs), and only the system-wide record (`QUERY_SYSTEM`; EPERM for
/// the others): procfs serves every instance, so it must not be able to
/// hand one instance another's processes (each instance's /proc/<pid> is
/// its own server's, `server_query`).
pub fn proc_query(op: u64, arg: u64, buf: u64, len: u64) -> SysResult {
    if !sched::current().group.privileged.load(Ordering::Relaxed) || op != QUERY_SYSTEM {
        return Err(EPERM);
    }
    let _ = arg;
    let bytes = answer(op, len)?;
    uaccess::copy_to(buf, &bytes)?;
    Ok(bytes.len() as i64)
}

/// `restricted::SYS_SYSTEM_INFO`: the system's record for a Linux server,
/// into its own memory.
pub fn server_query(op: u64, buf: u64, len: u64) -> SysResult {
    let bytes = answer(op, len)?;
    uaccess::copy_to_server(buf, &bytes)?;
    Ok(bytes.len() as i64)
}
