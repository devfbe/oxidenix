//! Memory groups: one per process tree (ADR 0024), Linux's cgroup v2 memory controller with
//! one group per container (the tree's instance holds it; its processes and threads name
//! it, `ThreadGroup::memcg`).
//!
//! **Charging.** Every frame allocated for user memory or for what a user can demand (the
//! programs' pages and page tables, the file cache pages a tree brings in first, its Linux
//! server's region and heap, its threads' kernel stacks) is charged when it is allocated to
//! the group of the task that allocates it (`Target::current`; a tree's start charges the
//! new tree, and a native server the kernel starts again at a tree's request is the root's:
//! `Charging`): the frame allocator notes the group in a byte pair per frame
//! (`frame::PhysFrameAllocator::owner`, Linux's `page->memcg_data`) and uncharges it when
//! the frame's last reference goes, wherever that is, so a page shared across trees (the
//! file cache of /data, ADR 0015) stays charged to the tree that brought it in, as on Linux.
//! Usage counts change only under the frames' lock, so a charge checks the limit exactly.
//! The root (`ROOT`) is everything else: the kernel's own memory and its native servers',
//! never limited, never counted here. Kernel heap objects are not charged (Linux charges
//! slab objects through its object groups; the kernel heap here never gives frames back,
//! and what a tree can make of it is bounded by its task table share and its server's
//! handles).
//!
//! **Limits.** `max` (memory.max) refuses a charge that would exceed it: the allocation
//! reclaims the group's own clean cache pages (`memory::reclaim_group`, never another
//! tree's) and, where it may wait (a page fault: `memory::reclaim_retry`), invokes the OOM
//! killer for the group once reclaim stops making progress (`oom::out_of_memory_memcg`),
//! which ends a process of the tree only. An allocation that may not wait fails (ENOMEM,
//! as a refused commit), but one that must not fail (the server's heap pages its reserve
//! covers, an OOM victim's or dying thread's faults: Linux's force charge) goes over.
//! `high` (memory.high) throttles: a thread that charged above it reclaims the group's
//! cache pages on its way back to its program and sleeps for a penalty that grows with
//! the excess (`over_high_return`, Linux's `mem_cgroup_handle_over_high`). `low` and
//! `min` (memory.low, memory.min) protect a group's cache pages from the reclaim of all
//! memory while its usage is within them: `min` always, `low` unless nothing unprotected
//! is left to take (`reclaimable`).
//!
//! **Lifetime.** A group lives as long as its tree's instance and processes hold it
//! (`MemCg`, in an `Arc`); when the last goes, its slot is dying, and the background
//! reclaimer (or the next group made, if no slot is free) moves the frames still charged to
//! it (cache pages other trees use, grants a native server holds) to the root
//! (`collect`, Linux's reparenting) before the slot is used again.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering::*};

/// A group's number: its slot. The frame allocator keeps one per frame.
pub type Id = u16;
/// No group: the kernel's memory, never limited.
pub const ROOT: Id = 0;
/// `Task::charge_to` for "the root, whatever the task's group" (`Charging::kernel`), where
/// `ROOT` there means "no override".
const KERNEL: Id = Id::MAX;
/// Groups at once, the root's slot included (a bit each in `TAKEN` and `DYING`). Trees are
/// fewer: each holds a reserve of the task table (`sched::TREE_RESERVE`).
pub const MAX_GROUPS: usize = 64;
/// No boundary (Linux's "max"), in pages.
pub const UNLIMITED: u64 = u64::MAX;

/// memory.events, in Linux's order.
#[derive(Clone, Copy)]
pub enum Event {
    /// Reclaim took pages of the group while it was below its `low`.
    Low = 0,
    /// A charge took it above its `high`.
    High,
    /// A charge found it at its `max`.
    Max,
    /// The OOM killer was invoked for it.
    Oom,
    /// A process of the group was killed by an OOM killer (the group's or the machine's).
    OomKill,
    /// memory.oom.group's kills (not offered: always 0).
    OomGroupKill,
}
const EVENTS: usize = 6;

/// A group's boundaries in pages (`UNLIMITED`: none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max: u64,
    pub high: u64,
    pub low: u64,
    pub min: u64,
}

impl Limits {
    /// A tree started without boundaries (Linux's defaults: max and high "max", low and
    /// min 0).
    pub const NONE: Limits = Limits { max: UNLIMITED, high: UNLIMITED, low: 0, min: 0 };

    /// From bytes (`restricted::MEMCG_UNLIMITED` stays unlimited), rounded down to pages
    /// as Linux's `page_counter_memparse`.
    pub fn from_bytes(max: u64, high: u64, low: u64, min: u64) -> Limits {
        Limits { max: pages_of(max), high: pages_of(high), low: pages_of(low), min: pages_of(min) }
    }
}

/// Pages of a boundary in bytes (`u64::MAX`: unlimited).
pub fn pages_of(bytes: u64) -> u64 {
    if bytes == u64::MAX { UNLIMITED } else { bytes / super::PAGE }
}

/// Bytes of a boundary in pages (`UNLIMITED`: `u64::MAX`).
pub fn bytes_of(pages: u64) -> u64 {
    if pages == UNLIMITED { u64::MAX } else { pages.saturating_mul(super::PAGE) }
}

struct Slot {
    /// Pages charged: changed only under the frames' lock (`charged`, `uncharged`).
    usage: AtomicU64,
    peak: AtomicU64,
    max: AtomicU64,
    high: AtomicU64,
    low: AtomicU64,
    min: AtomicU64,
    events: [AtomicU64; EVENTS],
    /// The id of the tree's Linux server instance (its name in the kernel's log, and whom
    /// to ask to shrink).
    instance: AtomicU64,
    /// Until when (`time::now`) a reclaim of the group that found nothing makes the next
    /// ones that may not wait skip (`memory::reclaim_group`).
    fruitless_until: AtomicU64,
    /// Pages the tree's server is to be asked to give back (`request_shrink`; 0: none).
    shrink: AtomicU64,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            usage: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            max: AtomicU64::new(UNLIMITED),
            high: AtomicU64::new(UNLIMITED),
            low: AtomicU64::new(0),
            min: AtomicU64::new(0),
            events: [const { AtomicU64::new(0) }; EVENTS],
            instance: AtomicU64::new(0),
            fruitless_until: AtomicU64::new(0),
            shrink: AtomicU64::new(0),
        }
    }
}

static SLOTS: [Slot; MAX_GROUPS] = [const { Slot::new() }; MAX_GROUPS];
/// The slots in use, live or dying (a bit each; the root's always).
static TAKEN: AtomicU64 = AtomicU64::new(1);
/// The slots whose group is gone, with frames maybe still charged to them (`collect`).
static DYING: AtomicU64 = AtomicU64::new(0);
/// Held while dying slots are collected: one collector at a time, so none frees a slot
/// another has handed out again since.
static COLLECTING: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

fn slot(id: Id) -> &'static Slot {
    &SLOTS[id as usize]
}

/// A tree's memory group (see the module comment). Its slot is given back, the frames
/// still charged to it moved to the root, once the last reference is gone.
pub struct MemCg {
    id: Id,
}

impl MemCg {
    /// A new group with `limits`, or EAGAIN if every slot is taken.
    pub fn new(limits: Limits) -> Result<Arc<MemCg>, i64> {
        let id = match take_slot() {
            Some(id) => id,
            None => {
                collect();
                take_slot().ok_or(crate::process::errno::EAGAIN)?
            }
        };
        let s = slot(id);
        // (A slot comes back with nothing charged: `collect`.)
        debug_assert_eq!(s.usage.load(Relaxed), 0, "a memory group's slot reused with frames charged");
        s.peak.store(0, Relaxed);
        for e in &s.events {
            e.store(0, Relaxed);
        }
        s.instance.store(0, Relaxed);
        s.fruitless_until.store(0, Relaxed);
        s.shrink.store(0, Relaxed);
        s.max.store(limits.max, Relaxed);
        s.high.store(limits.high, Relaxed);
        s.low.store(limits.low, Relaxed);
        s.min.store(limits.min, Relaxed);
        Arc::try_new(MemCg { id }).map_err(|_| {
            TAKEN.fetch_and(!(1 << id), AcqRel);
            crate::process::errno::ENOMEM
        })
    }

    pub fn id(&self) -> Id {
        self.id
    }

    /// The tree's instance, once it is made.
    pub fn set_instance(&self, instance: u64) {
        slot(self.id).instance.store(instance, Relaxed);
    }

    /// What `restricted::SYS_MEMCG_STAT` reports.
    pub fn stat(&self) -> restricted::MemCgStat {
        let s = slot(self.id);
        let page = |a: &AtomicU64| a.load(Relaxed).saturating_mul(super::PAGE);
        let event = |e: Event| s.events[e as usize].load(Relaxed);
        restricted::MemCgStat {
            current: page(&s.usage),
            peak: page(&s.peak),
            max: bytes_of(s.max.load(Relaxed)),
            high: bytes_of(s.high.load(Relaxed)),
            low: bytes_of(s.low.load(Relaxed)),
            min: bytes_of(s.min.load(Relaxed)),
            ev_low: event(Event::Low),
            ev_high: event(Event::High),
            ev_max: event(Event::Max),
            ev_oom: event(Event::Oom),
            ev_oom_kill: event(Event::OomKill),
            ev_oom_group_kill: event(Event::OomGroupKill),
        }
    }

    /// Sets the boundary `which` (`restricted::MEMCG_*`) to `pages`, as a write of Linux's
    /// memory.max (reclaim, then the OOM killer, until the usage fits) or memory.high
    /// (reclaim only), memory.low and memory.min (no action). From a thread that holds no
    /// lock; it stops early once the caller has a signal to take (the boundary is set).
    pub fn set(&self, which: u64, pages: u64) -> Result<(), i64> {
        let s = slot(self.id);
        match which {
            restricted::MEMCG_MAX => {
                s.max.store(pages, Relaxed);
                super::shrink_group_to(self.id, pages, true);
            }
            restricted::MEMCG_HIGH => {
                s.high.store(pages, Relaxed);
                super::shrink_group_to(self.id, pages, false);
            }
            restricted::MEMCG_LOW => s.low.store(pages, Relaxed),
            restricted::MEMCG_MIN => s.min.store(pages, Relaxed),
            _ => return Err(crate::process::errno::EINVAL),
        }
        Ok(())
    }
}

impl Drop for MemCg {
    /// The slot is dying: collected (its frames moved to the root) by the background
    /// reclaimer, never here (the last reference may go anywhere, a task's end in the
    /// scheduler included).
    fn drop(&mut self) {
        // Its protection ends with its tree: what is left of it is anyone's to reclaim.
        let s = slot(self.id);
        s.low.store(0, Relaxed);
        s.min.store(0, Relaxed);
        DYING.fetch_or(1 << self.id, AcqRel);
        super::kick_reclaimer();
    }
}

fn take_slot() -> Option<Id> {
    let mut taken = TAKEN.load(Acquire);
    loop {
        let free = !taken;
        if free == 0 {
            return None;
        }
        let id = free.trailing_zeros() as Id;
        match TAKEN.compare_exchange(taken, taken | 1 << id, AcqRel, Acquire) {
            Ok(_) => return Some(id),
            Err(now) => taken = now,
        }
    }
}

/// Whether slots are dying (the background reclaimer collects them).
pub fn dying() -> bool {
    DYING.load(Relaxed) != 0
}

/// Moves the frames still charged to dying groups to the root (Linux's reparenting) and
/// frees their slots. With no lock held (it takes the frames' lock a range of frames at a
/// time, and sleeps for its turn).
pub fn collect() {
    let _one = COLLECTING.lock();
    loop {
        let dying = DYING.load(Acquire);
        if dying == 0 {
            return;
        }
        let id = dying.trailing_zeros() as Id;
        let frames = super::with_frames(|f| f.frame_slots());
        // A range at a time: the frames' lock is held briefly.
        const CHUNK: usize = 16384;
        let mut at = 0;
        while at < frames {
            super::with_frames(|f| f.reparent(id, at, (at + CHUNK).min(frames)));
            at += CHUNK;
            crate::process::sched::cond_resched();
        }
        // Nothing can be charged to it any more (nobody holds the group), so nothing is.
        debug_assert_eq!(slot(id).usage.load(Relaxed), 0, "a dying memory group still charged");
        DYING.fetch_and(!(1 << id), AcqRel);
        TAKEN.fetch_and(!(1 << id), AcqRel);
    }
}

/// Whom an allocation charges: a group, and whether over its limit if need be (Linux's
/// force charge: an OOM victim's or a dying thread's, and what must not fail).
#[derive(Clone, Copy, Debug)]
pub struct Target {
    id: Id,
    force: bool,
}

impl Target {
    pub const ROOT: Target = Target { id: ROOT, force: false };

    /// The running task's: its tree's group (or the one it charges for, `Charging`),
    /// forced if it is dying (killed, or the OOM killer's victim: it must get to its end).
    pub fn current() -> Target {
        // (At boot, before the first task, everything is the kernel's.)
        let Some(me) = crate::process::sched::try_current() else { return Target::ROOT };
        let id = match me.charge_to.load(Relaxed) {
            ROOT => me.group.memcg_id(),
            KERNEL => ROOT,
            other => other,
        };
        let force = me.killed.load(Relaxed) || me.group.oom_victim.load(Relaxed);
        Target { id, force }
    }

    /// Group `id`'s, unforced.
    pub fn of(id: Id) -> Target {
        Target { id, force: false }
    }

    /// The same group, over its limit if need be.
    pub fn forced(self) -> Target {
        Target { force: true, ..self }
    }

    pub fn id(self) -> Id {
        self.id
    }
}

/// Whether `n` more pages may be charged to `t`'s group (exact under the frames' lock).
pub fn may_charge(t: Target, n: u64) -> bool {
    if t.id == ROOT || t.force {
        return true;
    }
    let s = slot(t.id);
    s.usage.load(Relaxed).saturating_add(n) <= s.max.load(Relaxed)
}

/// Whether a charge of `n` pages to `t`'s group would be refused for its limit.
pub fn over(t: Target, n: u64) -> bool {
    !may_charge(t, n)
}

/// `n` pages charged to `id`. Under the frames' lock (`frame`'s, as it notes the owner).
pub(super) fn charged(id: Id, n: u64) {
    if id == ROOT {
        return;
    }
    let s = slot(id);
    let usage = s.usage.fetch_add(n, Relaxed) + n;
    s.peak.fetch_max(usage, Relaxed);
    if usage > s.high.load(Relaxed) {
        // Throttled on the way back to its program (`over_high_return`).
        if let Some(me) = crate::process::sched::try_current() {
            me.memcg_high_pages.fetch_add(n as u32, Relaxed);
        }
    }
}

/// `n` pages of `id` uncharged. Under the frames' lock.
pub(super) fn uncharged(id: Id, n: u64) {
    if id != ROOT {
        let before = slot(id).usage.fetch_sub(n, Relaxed);
        debug_assert!(before >= n, "a memory group uncharged below zero");
    }
}

/// Counts `e` for group `id`.
pub fn event(id: Id, e: Event) {
    if id != ROOT {
        slot(id).events[e as usize].fetch_add(1, Relaxed);
    }
}

/// How often `e` happened to group `id`.
pub fn events(id: Id, e: Event) -> u64 {
    slot(id).events[e as usize].load(Relaxed)
}

/// (usage, max) of group `id` in pages.
pub fn usage(id: Id) -> (u64, u64) {
    let s = slot(id);
    (s.usage.load(Relaxed), s.max.load(Relaxed))
}

/// The id of group `id`'s tree's instance (0: none yet).
pub fn instance_of(id: Id) -> u64 {
    slot(id).instance.load(Relaxed)
}

/// Whether the reclaim of all memory may take a page charged to `id` now: never one of a
/// group within its memory.min, one within its memory.low only with `breach_low` (nothing
/// unprotected was left to take).
pub fn reclaimable(id: Id, breach_low: bool) -> bool {
    if id == ROOT {
        return true;
    }
    let s = slot(id);
    let usage = s.usage.load(Relaxed);
    if usage <= s.min.load(Relaxed) {
        return false;
    }
    breach_low || usage > s.low.load(Relaxed)
}

/// Whether some group's memory.low protects pages from the reclaim of all memory now (so a
/// reclaim that found nothing else may look at those).
pub fn low_protects() -> bool {
    let taken = TAKEN.load(Relaxed) & !1;
    (0..MAX_GROUPS).filter(|&i| taken & 1 << i != 0).any(|i| {
        let s = &SLOTS[i];
        let usage = s.usage.load(Relaxed);
        usage <= s.low.load(Relaxed) && usage > s.min.load(Relaxed)
    })
}

/// Reclaim took pages of the groups in `mask` (a bit per id) that were within their
/// memory.low: their `low` event.
pub fn low_breached(mask: u64) {
    for i in 0..MAX_GROUPS {
        if mask & 1 << i != 0 {
            event(i as Id, Event::Low);
        }
    }
}

/// Whether a reclaim of group `id` found nothing until lately (`note_fruitless`): one that
/// may not wait skips it then.
pub fn fruitless(id: Id, now: u64) -> bool {
    now < slot(id).fruitless_until.load(Relaxed)
}

/// A reclaim of group `id` found nothing: those that may not wait skip until `until`.
pub fn note_fruitless(id: Id, until: u64) {
    slot(id).fruitless_until.store(until, Relaxed);
}

/// Group `id`'s tree's server is to give back `pages` pages it can do without (its
/// shrinkers, `EVENT_SHRINK`): asked by the background reclaimer (`take_shrink_requests`),
/// which may drop the last reference to an instance where the requester may not.
pub fn request_shrink(id: Id, pages: u64) {
    if id != ROOT {
        slot(id).shrink.fetch_max(pages, Relaxed);
    }
}

/// Hands each live group's pending shrink request to `post` (the background reclaimer:
/// `linux::post_shrink_group`).
pub fn take_shrink_requests(post: fn(Id, u64)) {
    let live = TAKEN.load(Acquire) & !DYING.load(Acquire) & !1;
    for i in (0..MAX_GROUPS).filter(|&i| live & 1 << i != 0) {
        let pages = SLOTS[i].shrink.swap(0, Relaxed);
        if pages > 0 {
            post(i as Id, pages);
        }
    }
}

/// The group what the running task makes for someone else is charged to, while it lives:
/// a new tree's (`to`: the starter is another tree, or the kernel's monitor), or the root
/// (`kernel`: a native server the kernel starts again at a tree's request).
pub struct Charging {
    previous: Id,
    _group: Option<Arc<MemCg>>,
}

impl Charging {
    pub fn to(group: &Arc<MemCg>) -> Charging {
        let previous = crate::process::sched::current().charge_to.swap(group.id, Relaxed);
        Charging { previous, _group: Some(group.clone()) }
    }

    pub fn kernel() -> Charging {
        let previous = crate::process::sched::current().charge_to.swap(KERNEL, Relaxed);
        Charging { previous, _group: None }
    }
}

impl Drop for Charging {
    fn drop(&mut self) {
        crate::process::sched::current().charge_to.store(self.previous, Relaxed);
    }
}

/// The longest a thread is throttled above memory.high at a time, and the least worth a
/// sleep (Linux's MEMCG_MAX_HIGH_DELAY_JIFFIES, 2 s, and HZ / 100).
const HIGH_DELAY_MAX: u64 = 2_000_000_000;
const HIGH_DELAY_MIN: u64 = 10_000_000;
/// Pages a penalty is measured against (Linux's MEMCG_CHARGE_BATCH).
const CHARGE_BATCH: u64 = 32;

/// On a thread's way back to its program (its page fault resolved, `restricted_enter`), with
/// no lock held and interrupts on: if it charged pages above its group's memory.high since,
/// the group's cache pages are reclaimed down to `high` and, if the usage stays above, the
/// thread sleeps for a penalty that grows with the square of the excess and with the pages it
/// charged (Linux's `mem_cgroup_handle_over_high` and `calculate_high_delay`), at most 2 s, so
/// a tree above its `high` slows down instead of reaching its `max`.
pub fn over_high_return() {
    let me = crate::process::sched::current();
    if me.memcg_high_pages.load(Relaxed) == 0 {
        return;
    }
    let pages = me.memcg_high_pages.swap(0, Relaxed) as u64;
    let t = Target::current();
    if t.id == ROOT || t.force || !x86_64::instructions::interrupts::are_enabled() {
        return;
    }
    let s = slot(t.id);
    let high = s.high.load(Relaxed);
    let usage = s.usage.load(Relaxed);
    if usage <= high {
        return;
    }
    event(t.id, Event::High);
    super::reclaim_group(t.id, usage - high, false);
    let usage = s.usage.load(Relaxed);
    if usage <= high {
        return;
    }
    // The excess as a fraction of `high` in 1/2^20 (Linux's MEMCG_DELAY_PRECISION_SHIFT),
    // squared, scaled by 2^-14 (MEMCG_DELAY_SCALING_SHIFT) of a second, by the pages charged
    // in batches.
    let overage = (((usage - high) as u128) << 20) / high.max(1) as u128;
    let delay = (overage * overage * 1_000_000_000 >> 34) * pages as u128 / CHARGE_BATCH as u128;
    let delay = delay.min(HIGH_DELAY_MAX as u128) as u64;
    if delay < HIGH_DELAY_MIN {
        return;
    }
    let deadline = crate::time::now() + delay;
    while crate::time::now() < deadline && !crate::process::kill::interrupted() {
        crate::process::sched::prepare_to_sleep().sleep_until(deadline);
    }
}

/// A group's name in the kernel's log: its tree, by its instance (Linux's cgroup path).
pub struct Path(pub Id);

impl core::fmt::Display for Path {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self.0 {
            ROOT => f.write_str("/"),
            id => write!(f, "/tree{}", instance_of(id)),
        }
    }
}
