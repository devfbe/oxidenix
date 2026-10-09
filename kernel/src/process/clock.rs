//! The clocks a thread reads (`restricted::SYS_CLOCK_READ`): the wall
//! clock, the monotonic clock, and the CPU time of its own process and of
//! itself, by the kernel's clock ids (`restricted::CLOCK_*`). Linux's clock
//! ids (their aliases, another process's or thread's CPU clock, which the
//! server asks `proc_info` and `thread_info` for) are the Linux server's.

use super::errno::*;
use super::sched::current;
use crate::time;
use restricted::{CLOCK_MONO, CLOCK_PROCESS_CPU, CLOCK_THREAD_CPU, CLOCK_WALL};

/// The clock `id` now, in nanoseconds (the wall clock since the epoch, the
/// others since boot or since the process or thread began).
pub fn read(id: u64) -> Result<u64, i64> {
    Ok(match id {
        CLOCK_WALL => time::realtime(),
        CLOCK_MONO => time::now(),
        CLOCK_PROCESS_CPU => {
            let (user, system) = current().group.info.lock().cputime();
            user + system
        }
        CLOCK_THREAD_CPU => current().runtime(),
        _ => return Err(EINVAL),
    })
}
