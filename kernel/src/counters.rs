//! Event counters of the hot paths, for the benchmarks (`docs/benchmarks/`):
//! how many system calls, IPC round trips, bytes through IPC, address
//! space switches, bytes copied to or from user memory and kernel heap
//! allocations an operation costs. procfs shows them in `/proc/counters`.
//!
//! The per-CPU counters are written only by their own CPU (no shared cache
//! line on the hot path); a reader adds them up. The IPC counters live
//! under the IPC lock, the allocation counter next to the heap's lock, so
//! counting adds no contention of its own.

use core::sync::atomic::{AtomicU64, Ordering};

/// Counters every CPU keeps for itself (in its `smp::Cpu` block).
pub struct PerCpu {
    pub syscalls: AtomicU64,
    pub address_space_switches: AtomicU64,
    pub user_copy_bytes: AtomicU64,
}

impl PerCpu {
    pub const fn new() -> Self {
        PerCpu { syscalls: AtomicU64::new(0), address_space_switches: AtomicU64::new(0), user_copy_bytes: AtomicU64::new(0) }
    }
}

/// Adds `n` to one of the calling CPU's counters.
#[inline]
pub fn add(counter: impl FnOnce(&PerCpu) -> &AtomicU64, n: u64) {
    counter(&crate::smp::cpu().counters).fetch_add(n, Ordering::Relaxed);
}

pub static HEAP_ALLOCS: AtomicU64 = AtomicU64::new(0);

/// All counters, added up over the CPUs.
pub fn snapshot() -> procproto::Counters {
    let mut c = procproto::Counters::default();
    for i in 0..crate::smp::MAX_CPUS {
        let Some(cpu) = crate::smp::by_index(i) else { continue };
        let p = &cpu.counters;
        c.syscalls += p.syscalls.load(Ordering::Relaxed);
        c.address_space_switches += p.address_space_switches.load(Ordering::Relaxed);
        c.user_copy_bytes += p.user_copy_bytes.load(Ordering::Relaxed);
    }
    (c.ipc_calls, c.ipc_bytes) = crate::process::ipc::counters();
    c.heap_allocs = HEAP_ALLOCS.load(Ordering::Relaxed);
    c
}
