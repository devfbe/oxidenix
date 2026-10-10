//! Calls about the machine and the calling thread's CPU state (phase R9, from the kernel's
//! Linux code): uname, sysinfo, reboot, getrandom, getcpu, arch_prctl, and ioperm and iopl,
//! over the kernel's mechanisms (`SYS_SYSTEM_INFO`, `SYS_POWER`, `SYS_RANDOM`,
//! `SYS_THREAD_INFO`, `SYS_THREAD_FS`). A tree without the kernel's host grant is a pid
//! namespace that is not the initial one: reboot ends the tree (ADR 0011).

use crate::syscall;
use crate::usercopy;
use restricted::*;

const EPERM: i64 = 1;
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const EINVAL: i64 = 22;
const EFAULT: i64 = 14;

const SYS_UNAME: u64 = 63;
const SYS_SYSINFO: u64 = 99;
const SYS_ARCH_PRCTL: u64 = 158;
const SYS_IOPL: u64 = 172;
const SYS_IOPERM: u64 = 173;
const SYS_REBOOT: u64 = 169;
const SYS_GETCPU: u64 = 309;
const SYS_GETRANDOM: u64 = 318;

/// The result of such a call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2) = (s.rdi, s.rsi, s.rdx);
    let result = match s.rax {
        SYS_UNAME => uname(a0),
        SYS_SYSINFO => sysinfo(a0),
        SYS_REBOOT => reboot(a0, a1, a2, s.r10),
        SYS_GETRANDOM => getrandom(a0, a1, a2),
        SYS_GETCPU => getcpu(a0, a1),
        SYS_ARCH_PRCTL => arch_prctl(a0, a1),
        // The tree has no I/O ports (the native servers' are the kernel's to give).
        SYS_IOPERM | SYS_IOPL => Err(EPERM),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// uname(2): oxidenix speaks Linux's ABI but says what it is.
fn uname(buf: u64) -> Result<i64, i64> {
    let mut uts = [0u8; 6 * 65];
    let fields = ["oxidenix", "oxidenix", env!("CARGO_PKG_VERSION"), "#1 SMP", "x86_64", "(none)"];
    for (i, field) in fields.iter().enumerate() {
        uts[i * 65..i * 65 + field.len()].copy_from_slice(field.as_bytes());
    }
    usercopy::to_program(buf, &uts)?;
    Ok(0)
}

/// sysinfo(2): uptime, load and memory from the kernel's record of the system, in the
/// layout of `struct sysinfo` (sizes in bytes: `mem_unit` 1), and the instance's own
/// threads as `procs` (its pid namespace's view, as /proc shows it).
fn sysinfo(buf: u64) -> Result<i64, i64> {
    const SI_LOAD_SHIFT: u64 = 16;
    let s = crate::procfs::system().ok_or(EINVAL)?;
    let mut out = [0u8; 112];
    let put = |out: &mut [u8; 112], at: usize, v: u64| out[at..at + 8].copy_from_slice(&v.to_le_bytes());
    put(&mut out, 0, s.uptime / s.hz.max(1));
    for i in 0..3 {
        let load = if s.load_shift <= SI_LOAD_SHIFT { s.load[i] << (SI_LOAD_SHIFT - s.load_shift) } else { s.load[i] >> (s.load_shift - SI_LOAD_SHIFT) };
        put(&mut out, 8 + i * 8, load);
    }
    put(&mut out, 32, s.mem_total);
    put(&mut out, 40, s.mem_free);
    put(&mut out, 48, s.shmem);
    let procs = crate::process::PROCS.lock().threads.len();
    out[80..82].copy_from_slice(&(procs.min(u16::MAX as usize) as u16).to_le_bytes());
    out[104..108].copy_from_slice(&1u32.to_le_bytes());
    usercopy::to_program(buf, &out)?;
    Ok(0)
}

/// reboot(2): power off (QEMU exits) or restart the machine, after every instance of the
/// server wrote its caches back (`SYS_POWER`). The magic numbers are checked as Linux does;
/// LINUX_REBOOT_CMD_HALT powers off too, RESTART2 restarts (its command string is
/// checked, not used); CAD_ON and CAD_OFF are accepted (there is no Ctrl-Alt-Del to
/// configure).
fn reboot(magic: u64, magic2: u64, cmd: u64, arg: u64) -> Result<i64, i64> {
    const MAGIC: u64 = 0xfee1_dead;
    const MAGIC2: [u64; 4] = [0x2812_1969, 0x0512_1996, 0x1604_1998, 0x2011_2000];
    const RESTART: u64 = 0x0123_4567;
    const RESTART2: u64 = 0xa1b2_c3d4;
    const HALT: u64 = 0xcdef_0123;
    const POWER_OFF_CMD: u64 = 0x4321_fedc;
    const CAD_ON: u64 = 0x89ab_cdef;
    const CAD_OFF: u64 = 0;
    if magic as u32 as u64 != MAGIC || !MAGIC2.contains(&(magic2 as u32 as u64)) {
        return Err(EINVAL);
    }
    let how = match cmd as u32 as u64 {
        CAD_ON | CAD_OFF => return Ok(0),
        RESTART => POWER_RESTART,
        RESTART2 => {
            usercopy::read_cstr(arg)?;
            POWER_RESTART
        }
        HALT | POWER_OFF_CMD => POWER_OFF,
        _ => return Err(EINVAL),
    };
    let r = syscall(SYS_POWER, [how, 0, 0, 0, 0, 0]);
    if r != -EPERM {
        return Err(-r);
    }
    // No host grant: the tree is a pid namespace that is not the initial
    // one, and reboot ends it as Linux's reboot_pid_ns does: its init dies
    // (and with it every process of the tree), its parent's wait reporting
    // SIGHUP for a restart, SIGINT for a power off or halt; the caller exits.
    let sig = if how == POWER_RESTART { SIGHUP } else { SIGINT };
    crate::process::PROCS.lock().group_exit(1, sig, None);
    crate::process::die(0)
}

/// getrandom(2): bytes of the kernel's generator, which is seeded before any process runs,
/// so it never blocks and GRND_NONBLOCK and GRND_RANDOM change nothing (as on Linux since
/// 5.6); unknown flags, and GRND_INSECURE with GRND_RANDOM, are EINVAL. Made 256 bytes at a
/// time (`SYS_RANDOM`); a long request ends early with what it made when a signal comes (the
/// first piece always comes, as Linux's), and a fault after some bytes returns how many were
/// copied. At most what one read(2) may return.
fn getrandom(buf: u64, len: u64, flags: u64) -> Result<i64, i64> {
    const GRND_NONBLOCK: u64 = 1;
    const GRND_RANDOM: u64 = 2;
    const GRND_INSECURE: u64 = 4;
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 || flags & (GRND_RANDOM | GRND_INSECURE) == GRND_RANDOM | GRND_INSECURE {
        return Err(EINVAL);
    }
    let len = len.min(crate::devices::MAX_RW_COUNT);
    if buf.checked_add(len).is_none_or(|end| end > SHARED_BASE) {
        return Err(EFAULT);
    }
    let mut piece = [0u8; 256];
    let mut done = 0u64;
    while done < len {
        if done > 0 && crate::signal::pending() {
            break;
        }
        let want = (len - done).min(piece.len() as u64) as usize;
        let n = syscall(SYS_RANDOM, [piece.as_mut_ptr() as u64, want as u64, 0, 0, 0, 0]);
        if n <= 0 {
            break;
        }
        let copied = usercopy::to_program(buf + done, &piece[..n as usize]);
        piece.fill(0);
        if let Err(e) = copied {
            return if done > 0 { Ok(done as i64) } else { Err(e) };
        }
        done += n as u64;
    }
    Ok(done as i64)
}

/// getcpu(&cpu, &node, cache): the CPU the caller runs on (where it ran last, which is
/// where it runs: the kernel's record of it); one NUMA node.
fn getcpu(cpu: u64, node: u64) -> Result<i64, i64> {
    let mut info = ThreadInfo::default();
    let r = syscall(SYS_THREAD_INFO, [0, &mut info as *mut ThreadInfo as u64, 0, 0, 0, 0]);
    if r < 0 {
        return Err(-r);
    }
    if cpu != 0 {
        usercopy::write(cpu, &(info.cpu as u32))?;
    }
    if node != 0 {
        usercopy::write(node, &0u32)?;
    }
    Ok(0)
}

/// arch_prctl(code, addr): the thread's FS base (its TLS pointer), set and read; the GS
/// base and the CPUID and shadow-stack controls are not offered (EINVAL).
fn arch_prctl(code: u64, addr: u64) -> Result<i64, i64> {
    const ARCH_SET_FS: u64 = 0x1002;
    const ARCH_GET_FS: u64 = 0x1003;
    match code {
        ARCH_SET_FS => {
            if addr >= SHARED_BASE {
                return Err(EPERM);
            }
            let r = syscall(SYS_THREAD_FS, [1, addr, 0, 0, 0, 0]);
            if r < 0 { Err(-r) } else { Ok(0) }
        }
        ARCH_GET_FS => {
            let base = syscall(SYS_THREAD_FS, [0, 0, 0, 0, 0, 0]) as u64;
            usercopy::write(addr, &base)?;
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}
