//! The out-of-memory killer (Linux's mm/oom_kill.c): when memory cannot be had after
//! reclaim stopped making progress, it ends the process whose end gives back the most (its
//! badness) with SIGKILL and has its private memory reaped, so that what could not be
//! allocated can be tried again (ADR 0013).
//!
//! **When.** An allocation that may wait (a page fault, a copy of the kernel or the Linux
//! server into program memory, a fill of a cached page) and found no frame after reclaim
//! retried without progress (`memory::reclaim_retry`) calls `out_of_memory`; once a victim
//! is killed it reclaims and retries anew, and only if no process may be killed it fails
//! (its process is killed, as before there was a killer). Allocations that cannot wait
//! (page tables made with the frames locked, the kernel heap's growth, the Linux servers'
//! own heap pages) have the background reclaimer act for them: when user memory stays
//! exhausted as long as such a retry lasts, it calls `out_of_memory` itself
//! (`memory::reclaimer`). In the strict overcommit mode, the default, committed memory always
//! has its frames, so the killer acts only when what nobody commits (page tables, the
//! kernel's own needs) runs out; in the heuristic and always modes it is what ends an
//! overcommitment that came due.
//!
//! **Whom.** The processes of Linux programs, of every process tree (one memory: a tree
//! confined to killing within itself would make the trees that need memory pay for the one
//! that took it). Never the kernel's threads, its native servers (diskfs, netd, procfs), a
//! Linux server instance's service process, the first process of a tree that holds the host
//! grant (Linux's global init) or a process whose oom_score_adj is -1000. Of the others the
//! one with the highest badness (Linux's `oom_badness`): its resident pages and page tables
//! (there is no swap), plus oom_score_adj thousandths of all memory. Every process sharing
//! the victim's address space (`PROC_SHARE_VM`, a vfork child) dies with it; if one of them
//! may not, the space is not reaped.
//!
//! **One victim at a time.** While a victim's memory has not come back (its address space
//! is still there and was not reaped), no other is chosen (Linux's `tsk_is_oom_victim`):
//! the callers reclaim and wait again. A victim's own faults may take frames of the
//! kernel's reserve (`memory::emergency_frame`), so that it gets to its end (Linux's
//! TIF_MEMDIE); so may the Linux server's allocations that must not fail.
//!
//! **The reaper** (Linux's oom_reaper): a kernel thread frees a victim's private memory
//! (`AddressSpace::reap`) as soon as it gets hold of its address space (ten tries, 100 ms
//! apart, while a thread of the victim holds it), so memory comes back even while a thread
//! of the victim is held up somewhere; then the victim counts no more (MMF_OOM_SKIP).
//!
//! **Messages** as Linux's, in the kernel's log: who invoked the killer, the table of the
//! processes it weighed, `Out of memory: Killed process N (name) total-vm:...kB,
//! anon-rss:...kB, file-rss:...kB, shmem-rss:...kB, UID:0 pgtables:...kB oom_score_adj:N`,
//! and the reaper's `oom_reaper: reaped process N (name), now anon-rss:...`. The pids are
//! the kernel's: the log is the machine's, a tree's own pids are its server's.

use crate::klog::{LEVEL_ERR, LEVEL_INFO, LEVEL_WARNING};
use crate::process::address_space::MemStats;
use crate::process::sched;
use crate::process::task::ThreadGroup;
use crate::sync::IrqSpinLock;
use alloc::sync::Arc;
use core::ops::Bound;
use core::sync::atomic::{AtomicUsize, Ordering};

/// The range of oom_score_adj: -1000 exempts a process, 1000 makes it the first choice.
pub const ADJ_MIN: i64 = -1000;
pub const ADJ_MAX: i64 = 1000;

/// A victim whose memory has not come back yet.
struct Victim {
    group: Arc<ThreadGroup>,
    stats: Arc<MemStats>,
    /// Nobody who may not be killed shares its address space: the reaper may free it.
    reapable: bool,
    /// The reaper took it on.
    taken: bool,
}

/// Victims waited for, at most `MAX_VICTIMS` (one beyond is killed all the same, but not
/// waited for or reaped: its end frees its memory). Their count, for `space_gone`.
const MAX_VICTIMS: usize = 8;
static VICTIMS: IrqSpinLock<heapless::Vec<Victim, MAX_VICTIMS>> = IrqSpinLock::new(heapless::Vec::new());
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Held while a victim is chosen and killed (Linux's oom_lock): one at a time.
static CHOOSING: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

/// How often and how far apart the reaper tries to get a victim's address space.
const REAP_TRIES: u32 = 10;
const REAP_RETRY: u64 = 100_000_000;

/// A name for the log, without the heap (at most 15 bytes, as a comm).
struct Name {
    bytes: [u8; 15],
    len: usize,
}

impl Name {
    fn of(s: &str) -> Name {
        let mut len = s.len().min(15);
        while !s.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0; 15];
        bytes[..len].copy_from_slice(&s.as_bytes()[..len]);
        Name { bytes, len }
    }

    /// A process's: its first live thread's name, as Linux names a victim by its
    /// task's comm (prctl's PR_SET_NAME shows), else the process's.
    fn group(group: &ThreadGroup) -> Name {
        let info = group.info.lock();
        match info.threads.first() {
            Some(t) => Name::of(&t.comm.lock()),
            None => Name::of(&info.name),
        }
    }
}

impl core::fmt::Display for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("?"))
    }
}

/// The process after `at` in the kernel's table (`at` moves to it; None: from the start),
/// so the table is not locked while the caller looks at it.
fn next_group(at: &mut Option<u64>) -> Option<Arc<ThreadGroup>> {
    let table = sched::TABLE.lock();
    let from = at.map_or(Bound::Unbounded, Bound::Excluded);
    let (&pid, group) = table.groups.range((from, Bound::Unbounded)).next()?;
    *at = Some(pid);
    Some(group.clone())
}

/// Whether `group` is the first process of a tree that holds the host grant (Linux's
/// global init, which is never killed).
fn host_init(group: &ThreadGroup) -> bool {
    let instance = group.instance.load(Ordering::Acquire);
    instance != 0 && !group.server_reaps.load(Ordering::Relaxed) && crate::process::linux::host_granted(instance)
}

/// What the OOM killer weighs `group` at (Linux's `oom_badness`) against `total` pages of
/// memory: its resident pages and page tables, plus oom_score_adj thousandths of the
/// total; with its memory's counts. None if it may not be chosen (see the module
/// comment), has ended, or its memory was reaped already.
pub fn badness(group: &ThreadGroup, total: u64) -> Option<(Arc<MemStats>, i64)> {
    if group.instance.load(Ordering::Acquire) == 0 {
        return None;
    }
    let adj = group.oom_score_adj.load(Ordering::Relaxed);
    if adj == ADJ_MIN {
        return None;
    }
    let stats = {
        let info = group.info.lock();
        if info.threads.is_empty() {
            return None;
        }
        info.mem.clone()?
    };
    if stats.oom_skip.load(Ordering::Acquire) || host_init(group) {
        return None;
    }
    let points = stats.pages.load(Ordering::Relaxed) + stats.table_pages.load(Ordering::Relaxed);
    Some((stats, points as i64 + adj * (total / 1000) as i64))
}

/// `badness` as /proc/<pid>/oom_score needs it: the points, or None.
pub fn points(group: &ThreadGroup) -> Option<i64> {
    badness(group, super::total_pages()).map(|(_, points)| points)
}

/// Whether a victim's memory is still to come back (its address space there, neither
/// reaped nor given up on); forgets those whose memory came back.
fn victim_pending() -> bool {
    let mut gone: heapless::Vec<Victim, MAX_VICTIMS> = heapless::Vec::new();
    let pending = {
        let mut victims = VICTIMS.lock();
        let mut i = 0;
        while i < victims.len() {
            let v = &victims[i];
            if v.stats.mm_alive() && !v.stats.oom_skip.load(Ordering::Acquire) {
                i += 1;
            } else {
                let _ = gone.push(victims.swap_remove(i));
            }
        }
        PENDING.store(victims.len(), Ordering::Relaxed);
        !victims.is_empty()
    };
    // (The references go with the lock released: one may be a process's last.)
    drop(gone);
    pending
}

/// Memory ran out (see the module comment) for an allocation of `n` frames, with no lock
/// held: kills the process of the highest badness unless a victim's memory is still on its
/// way or the frames are there now. Whether the caller should try its allocation again:
/// false if no process may be killed, or the caller's own was (it dies).
pub fn out_of_memory(n: u64) -> bool {
    let me = sched::current();
    if crate::process::kill::dying() {
        return false;
    }
    let Some(_choosing) = CHOOSING.try_lock() else {
        // Another thread chooses now: its victim's memory is for this one too.
        return true;
    };
    // Memory came back meanwhile, or a victim's is on its way.
    if super::with_frames(|f| f.user_may_take(n)) || victim_pending() {
        return true;
    }
    let total = super::total_pages();
    let mut best: Option<(Arc<ThreadGroup>, Arc<MemStats>, i64)> = None;
    let mut at = None;
    while let Some(group) = next_group(&mut at) {
        if let Some((stats, points)) = badness(&group, total) {
            if best.as_ref().is_none_or(|b| points > b.2) {
                best = Some((group, stats, points));
            }
        }
    }
    let comm = Name::of(&me.comm.lock());
    crate::printkln_at!(LEVEL_WARNING, "{} invoked oom-killer: order=0, oom_score_adj={}", comm, me.group.oom_score_adj.load(Ordering::Relaxed));
    dump_tasks();
    let Some((victim, stats, _)) = best else {
        crate::printkln_at!(LEVEL_ERR, "Out of memory and no killable processes...");
        return false;
    };
    kill(&victim, &stats);
    !crate::process::kill::dying()
}

/// The table of the processes the killer weighs (Linux's `dump_tasks`), in pages.
fn dump_tasks() {
    crate::printkln_at!(LEVEL_INFO, "Tasks state (memory values in pages):");
    crate::printkln_at!(
        LEVEL_INFO,
        "[  pid  ]   uid  tgid total_vm      rss rss_anon rss_file rss_shmem pgtables_bytes swapents oom_score_adj name"
    );
    let mut at = None;
    while let Some(group) = next_group(&mut at) {
        if group.instance.load(Ordering::Acquire) == 0 {
            continue;
        }
        let Some(stats) = group.info.lock().mem.clone() else { continue };
        let (pages, anon, shmem) = counts(&stats);
        crate::printkln_at!(
            LEVEL_INFO,
            "[{:>7}] {:>5} {:>5} {:>8} {:>8} {:>8} {:>8} {:>9} {:>8} {:>8}         {:>5} {}",
            group.tgid,
            0,
            group.tgid,
            stats.virt_pages.load(Ordering::Relaxed),
            pages,
            anon,
            pages - anon - shmem,
            shmem,
            stats.table_pages.load(Ordering::Relaxed) * super::PAGE,
            0,
            group.oom_score_adj.load(Ordering::Relaxed),
            Name::group(&group)
        );
    }
}

/// (resident, anonymous, shmem) pages, consistent with each other (each counter is
/// updated on its own).
fn counts(stats: &MemStats) -> (u64, u64, u64) {
    let pages = stats.pages.load(Ordering::Relaxed);
    let anon = stats.anon_pages.load(Ordering::Relaxed).min(pages);
    let shmem = stats.shmem_pages.load(Ordering::Relaxed).min(pages - anon);
    (pages, anon, shmem)
}

/// Kills `victim` (and every process sharing its address space) and has its memory reaped.
fn kill(victim: &Arc<ThreadGroup>, stats: &Arc<MemStats>) {
    let name = Name::group(victim);
    let kb = super::PAGE / 1024;
    let (pages, anon, shmem) = counts(stats);
    crate::printkln_at!(
        LEVEL_INFO,
        "oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg=/,task={},pid={},uid=0",
        name,
        victim.tgid
    );
    crate::printkln_at!(
        LEVEL_ERR,
        "Out of memory: Killed process {} ({}) total-vm:{}kB, anon-rss:{}kB, file-rss:{}kB, shmem-rss:{}kB, UID:0 pgtables:{}kB oom_score_adj:{}",
        victim.tgid,
        name,
        stats.virt_pages.load(Ordering::Relaxed) * kb,
        anon * kb,
        (pages - anon - shmem) * kb,
        shmem * kb,
        stats.table_pages.load(Ordering::Relaxed) * kb,
        victim.oom_score_adj.load(Ordering::Relaxed)
    );
    victim.oom_victim.store(true, Ordering::Release);
    crate::process::kill::kill_process(victim, crate::process::kill::SIGKILL as i32);
    // Every other process of its address space dies with it (its memory would not come
    // back otherwise); one that may not be killed keeps it, unreaped.
    let mut reapable = true;
    let mut at = None;
    while let Some(group) = next_group(&mut at) {
        if Arc::ptr_eq(&group, victim) || !group.info.lock().mem.as_ref().is_some_and(|m| Arc::ptr_eq(m, stats)) {
            continue;
        }
        if group.instance.load(Ordering::Acquire) == 0 || group.oom_score_adj.load(Ordering::Relaxed) == ADJ_MIN || host_init(&group) {
            reapable = false;
            crate::printkln_at!(LEVEL_INFO, "oom killer {} ({}) has mm pinned by {} ({})", victim.tgid, name, group.tgid, Name::group(&group));
            continue;
        }
        group.oom_victim.store(true, Ordering::Release);
        crate::process::kill::kill_process(&group, crate::process::kill::SIGKILL as i32);
    }
    let queued = {
        let mut victims = VICTIMS.lock();
        let pushed = victims.push(Victim { group: victim.clone(), stats: stats.clone(), reapable, taken: false });
        PENDING.store(victims.len(), Ordering::Relaxed);
        pushed.is_ok()
    };
    if queued {
        sched::wakeup(reaper_chan());
    }
}

/// An address space went: if a victim's, the allocations waiting for memory look again.
/// (Cheap unless a victim is pending.)
pub fn space_gone() {
    if PENDING.load(Ordering::Relaxed) > 0 {
        super::wake_throttled();
    }
}

/// Sets `group`'s oom_score_adj to `adj` (Linux's `__set_oom_adj`), and that of every other
/// process sharing its address space (but a host tree's first process): EINVAL outside
/// -1000..=1000, EACCES below the least it may set (oom_score_adj_min) unless `privileged`
/// (the host grant, Linux's CAP_SYS_RESOURCE), which also makes `adj` the new least unless
/// `legacy` (a write of the old oom_adj). The old value.
pub fn set_score_adj(group: &Arc<ThreadGroup>, adj: i64, privileged: bool, legacy: bool) -> Result<i64, i64> {
    if !(ADJ_MIN..=ADJ_MAX).contains(&adj) {
        return Err(crate::process::errno::EINVAL);
    }
    if adj < group.oom_score_adj_min.load(Ordering::Relaxed) && !privileged {
        return Err(crate::process::errno::EACCES);
    }
    let set = |g: &ThreadGroup| {
        let old = g.oom_score_adj.swap(adj, Ordering::Relaxed);
        if privileged && !legacy {
            g.oom_score_adj_min.store(adj, Ordering::Relaxed);
        }
        old
    };
    let old = set(group);
    let Some(stats) = group.info.lock().mem.clone() else { return Ok(old) };
    let mut at = None;
    while let Some(other) = next_group(&mut at) {
        if !Arc::ptr_eq(&other, group) && other.info.lock().mem.as_ref().is_some_and(|m| Arc::ptr_eq(m, &stats)) && !host_init(&other) {
            set(&other);
        }
    }
    Ok(old)
}

fn reaper_chan() -> usize {
    &VICTIMS as *const _ as usize
}

/// Starts the reaper's kernel thread.
pub fn start_reaper() -> Result<(), i64> {
    sched::spawn_kernel_thread("oom_reaper", reaper)
}

fn reaper() -> ! {
    loop {
        let wait = sched::prepare_to_wait(reaper_chan());
        let next = VICTIMS.lock().iter_mut().find(|v| !v.taken).map(|v| {
            v.taken = true;
            (v.group.clone(), v.stats.clone(), v.reapable)
        });
        let Some((group, stats, reapable)) = next else {
            wait.sleep();
            continue;
        };
        drop(wait);
        reap(&group, &stats, reapable);
        // Reaped, gone, or given up on: the killer may choose another.
        stats.oom_skip.store(true, Ordering::Release);
        drop((group, stats));
        victim_pending();
        super::wake_throttled();
    }
}

/// Frees the victim's private memory as soon as its address space is free (see the module
/// comment); waits for its end instead if it is not `reapable`.
fn reap(group: &ThreadGroup, stats: &MemStats, reapable: bool) {
    for _ in 0..REAP_TRIES {
        // (Gone: its memory came back.)
        let Some(mm) = stats.mm() else { return };
        let reaped = reapable && mm.try_lock().map(|mut space| space.reap()).is_some();
        // (The address space may be the victim's last reference: let go of it first.)
        drop(mm);
        if reaped {
            let kb = super::PAGE / 1024;
            let (pages, anon, shmem) = counts(stats);
            crate::printkln!(
                "oom_reaper: reaped process {} ({}), now anon-rss:{}kB, file-rss:{}kB, shmem-rss:{}kB",
                group.tgid,
                Name::group(group),
                anon * kb,
                (pages - anon - shmem) * kb,
                shmem * kb
            );
            return;
        }
        sched::prepare_to_sleep().sleep_until(crate::time::now() + REAP_RETRY);
    }
    if reapable {
        crate::printkln!("oom_reaper: unable to reap pid:{} ({})", group.tgid, Name::group(group));
    }
}
