//! proc_query (syscall 1005): the kernel's native process and system
//! information for the procfs server (records in `procproto`). Only
//! privileged servers may ask; procfs decides what programs see.

use super::errno::*;
use super::sched::{self, TABLE};
use super::task::State;
use super::{uaccess, Pid, TIMER_HZ};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use procproto::*;

fn system() -> System {
    let mut s = System { hz: TIMER_HZ, uptime: sched::ticks(), page_size: 4096, ..Default::default() };
    s.boot_time = crate::time::boot_time();
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
    s.kernel_heap = (mem.heap_used + mem.heap_free) as u64;
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
    s
}

fn process(pid: Pid) -> Result<Process, i64> {
    let g = TABLE.lock().groups.get(&pid).cloned().ok_or(ESRCH)?;
    let info = g.info.lock();
    // The process's state is its main thread's (or the first live one's):
    // running if any thread runs.
    let state = if info.threads.is_empty() {
        STATE_ZOMBIE
    } else if info.threads.iter().any(|t| matches!(t.state(), State::Running | State::Runnable)) {
        STATE_RUNNING
    } else if info.threads.iter().all(|t| t.state() == State::Stopped) {
        STATE_STOPPED
    } else {
        STATE_SLEEPING
    };
    let (user_ns, system_ns) = info.cputime();
    let tick_ns = crate::time::NSEC_PER_SEC / TIMER_HZ;
    let mut p = Process {
        pid,
        ppid: info.ppid,
        pgid: info.pgid,
        sid: info.sid,
        state: state as u64,
        utime: user_ns / tick_ns,
        stime: system_ns / tick_ns,
        start: g.start_ticks,
        pages: info.mem.as_ref().map_or(0, |m| m.pages.load(Ordering::Relaxed)),
        virt_pages: info.mem.as_ref().map_or(0, |m| m.virt_pages.load(Ordering::Relaxed)),
        nice: info.nice as i64,
        threads: info.threads.len() as u64,
        cpu: info.threads.first().map_or(0, |t| t.last_cpu.load(Ordering::Relaxed)) as u64,
        flags: 0,
        name: [0; 16],
    };
    if g.privileged.load(Ordering::Relaxed) {
        p.flags |= FLAG_SERVER;
    }
    if pid == 0 {
        p.flags |= FLAG_KERNEL;
    }
    let n = info.name.len().min(15);
    p.name[..n].copy_from_slice(&info.name.as_bytes()[..n]);
    Ok(p)
}

fn text_of(pid: Pid, f: impl FnOnce(&super::task::Info) -> Vec<u8>) -> Result<Vec<u8>, i64> {
    let g = TABLE.lock().groups.get(&pid).cloned().ok_or(ESRCH)?;
    let info = g.info.lock();
    Ok(f(&info))
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

/// proc_query(op, arg, buf, len): writes the answer to `buf` and returns
/// its length; ERANGE if it does not fit (except QUERY_PIDS, which fills
/// what fits).
pub fn proc_query(op: u64, arg: u64, buf: u64, len: u64) -> SysResult {
    if !sched::current().group.privileged.load(Ordering::Relaxed) {
        return Err(EPERM);
    }
    let bytes: Vec<u8> = match op {
        QUERY_SYSTEM => as_bytes(&system()).to_vec(),
        QUERY_PIDS => {
            let pids: Vec<u64> = TABLE.lock().groups.keys().copied().collect();
            let fit = (len / 8) as usize;
            pids.iter().take(fit).flat_map(|p| p.to_le_bytes()).collect()
        }
        QUERY_PROCESS => as_bytes(&process(arg)?).to_vec(),
        QUERY_CMDLINE => text_of(arg, |i| i.cmdline.clone())?,
        QUERY_EXE => text_of(arg, |i| i.exe.as_bytes().to_vec())?,
        QUERY_MOUNTS => crate::fs::mounts().into_bytes(),
        _ => return Err(EINVAL),
    };
    if bytes.len() as u64 > len {
        return Err(ERANGE);
    }
    uaccess::copy_to(buf, &bytes)?;
    Ok(bytes.len() as i64)
}
