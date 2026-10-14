//! The vDSO in the instance's programs (ADR 0014): the shared object `vdso/vdso.rs`
//! (built by `build.rs`, embedded here) in a memory object of the instance, and the
//! kernel's clock page (`SYS_CLOCK_PAGE`) it reads, the same pages in every program of the
//! tree. execve maps both (`map`): the clock page shared and read-only as `[vvar]`, the
//! vDSO private (copy-on-write, as Linux's: a debugger may set breakpoints) and executable
//! one page above it as `[vdso]`, and announces the vDSO's ELF header as
//! `AT_SYSINFO_EHDR`; fork keeps both as every other mapping. /proc/<pid>/maps names them
//! by their objects (`name_of`), wherever a program moved them.

use crate::syscall;
use core::sync::atomic::{AtomicU64, Ordering};
use restricted::*;

const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_EXEC: u64 = 4;

/// The vDSO's image.
static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.so"));

/// The handles of the vDSO's object and of the clock page, and their ids (`SYS_MO_ID`);
/// 0 until `init` made them (and for good if it could not: programs then go without).
static VDSO: AtomicU64 = AtomicU64::new(0);
static VDSO_ID: AtomicU64 = AtomicU64::new(0);
static CLOCK: AtomicU64 = AtomicU64::new(0);
static CLOCK_ID: AtomicU64 = AtomicU64::new(0);

fn pages() -> u64 {
    (IMAGE.len() as u64).div_ceil(PAGE)
}

fn check(r: i64) -> Result<u64, i64> {
    if r < 0 { Err(-r) } else { Ok(r as u64) }
}

/// Makes the instance's vDSO object and gets the clock page, at the instance's start
/// (before its first program runs). Without memory for them, or without a clock page,
/// programs run without a vDSO (their C library makes the system calls instead).
pub fn init() {
    let made = (|| -> Result<(u64, u64, u64, u64), i64> {
        let clock = check(syscall(SYS_CLOCK_PAGE, [0; 6]))?;
        let vdso = check(syscall(SYS_MO_CREATE, [pages(), 0, 0, 0, 0, 0]))?;
        let written = check(syscall(SYS_MO_WRITE, [vdso, 0, IMAGE.as_ptr() as u64, IMAGE.len() as u64, 0, 0]))?;
        if written != IMAGE.len() as u64 {
            return Err(12);
        }
        let ids = (check(syscall(SYS_MO_ID, [vdso, 0, 0, 0, 0, 0]))?, check(syscall(SYS_MO_ID, [clock, 0, 0, 0, 0, 0]))?);
        Ok((vdso, ids.0, clock, ids.1))
    })();
    match made {
        Ok((vdso, vdso_id, clock, clock_id)) => {
            VDSO_ID.store(vdso_id, Ordering::Relaxed);
            CLOCK.store(clock, Ordering::Relaxed);
            CLOCK_ID.store(clock_id, Ordering::Relaxed);
            VDSO.store(vdso, Ordering::Release);
        }
        Err(e) => {
            let msg = crate::try_format!("no vDSO (errno {e}): programs read the clocks by system call");
            if let Ok(msg) = msg {
                syscall(SYS_SERVER_LOG, [msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0, 0]);
            }
        }
    }
}

/// Maps the clock page and the vDSO into the calling thread's new program, the clock
/// page just below the vDSO where the kernel finds room for both: the vDSO's address
/// (AT_SYSINFO_EHDR), or None without a vDSO.
pub fn map() -> Result<Option<u64>, i64> {
    let vdso = VDSO.load(Ordering::Acquire);
    if vdso == 0 {
        return Ok(None);
    }
    let len = (1 + pages()) * PAGE;
    // Room for both (a reservation without access, replaced right away).
    let at = check(syscall(SYS_MO_MAP, [0, 0, len, 0, 0, 0]))?;
    let clock = CLOCK.load(Ordering::Relaxed);
    check(syscall(SYS_MO_MAP, [clock, at, PAGE, 0, PROT_READ, MO_FIXED | MO_SHARED | MO_READONLY]))?;
    check(syscall(SYS_MO_MAP, [vdso, at + PAGE, pages() * PAGE, 0, PROT_READ | PROT_EXEC, MO_FIXED]))?;
    Ok(Some(at + PAGE))
}

/// The name /proc/<pid>/maps gives a mapping of the object `id` (`VmArea::object`), if it
/// is the vDSO's or the clock page.
pub fn name_of(id: u64) -> Option<&'static str> {
    if id == 0 {
        None
    } else if id == VDSO_ID.load(Ordering::Relaxed) {
        Some("[vdso]")
    } else if id == CLOCK_ID.load(Ordering::Relaxed) {
        Some("[vvar]")
    } else {
        None
    }
}
