//! The system calls of the kernel's native servers (diskfs, netd, procfs,
//! ringtest; `oxrt::sys`): the kernel's own interface, numbered as the
//! Linux server's (`restricted`) where a call is the same mechanism (memory,
//! futexes, clocks, randomness, the end of the process), and IPC, I/O
//! ports, interrupts, DMA and the service's end of channels. A Linux
//! system call number means nothing here (ENOSYS): the kernel implements
//! none (R9); Linux programs run under their server in restricted mode.

use super::errno::*;
use super::syscall::Frame;
use super::{uaccess, vm};
use restricted::*;

/// `ipc_register(name, len, arg, flags)` .. `proc_query`, as before R9.
const IPC_REGISTER: u64 = 1000;
const IPC_RECEIVE: u64 = 1001;
const IPC_REPLY: u64 = 1002;
const IRQ_ENABLE: u64 = 1003;
const DMA_MAP: u64 = 1004;
const PROC_QUERY: u64 = 1005;
/// `ioperm(from, count, on)`: the I/O ports the kernel assigned the server.
const IOPERM: u64 = 1006;
/// `exec(argv)`: the server's program anew (see `exec`).
const EXEC: u64 = 1007;
/// `log(buf, len) -> len`: text on the kernel's console (at most
/// `LOG_MAX` bytes a call).
const LOG: u64 = 1008;
const LOG_MAX: u64 = 4096;
/// The service's end of a channel (`channel`).
const CHAN_ATTACH: u64 = 1068;
const CHAN_DETACH: u64 = 1069;
const GRANT_MAP: u64 = 1070;
const GRANT_DMA: u64 = 1071;
const GRANT_DMA_UNMAP: u64 = 1072;
const CHAN_WATCH: u64 = 1073;
const SET_COPY_FIXUP: u64 = 1074;
const GRANT_DMA_PAGES: u64 = 1075;
const CHAN_PREDECESSORS: u64 = 1086;
/// Bytes of randomness one `SYS_RANDOM` gives at most.
const RANDOM_MAX: usize = 256;

/// Carries out the native call in `f` (its result in `rax`, or a new
/// program after `EXEC`).
pub fn dispatch(f: &mut Frame) {
    let (a0, a1, a2, a3, a4, a5) = (f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9);
    if f.rax == EXEC {
        // On success `f` starts the new program.
        if let Err(e) = super::exec::exec(f, a0) {
            f.rax = (-e) as u64;
        }
        return;
    }
    let result: SysResult = match f.rax {
        IPC_REGISTER => super::ipc::register(a0, a1, a2, a3),
        IPC_RECEIVE => super::ipc::receive(a0, a1, a2, a3 as i64),
        IPC_REPLY => super::ipc::reply(a0, a1, a2),
        IRQ_ENABLE => super::irq::enable(a0),
        DMA_MAP => super::dma_map(a0),
        PROC_QUERY => super::query::proc_query(a0, a1, a2, a3),
        IOPERM => super::ioperm(a0, a1, a2),
        LOG => log(a0, a1),
        CHAN_ATTACH => super::channel::attach(a0),
        CHAN_DETACH => super::channel::detach(a0),
        GRANT_MAP => super::channel::grant_map(a0, a1, a2, a3),
        GRANT_DMA => super::channel::grant_dma(a0, a1, a2),
        GRANT_DMA_UNMAP => super::channel::grant_dma_unmap(a0, a1),
        CHAN_WATCH => super::channel::watch(a0, a1),
        SET_COPY_FIXUP => super::channel::set_copy_fixup(a0, a1),
        GRANT_DMA_PAGES => super::channel::grant_dma_pages(a0, a1, a2, a3, a4),
        CHAN_PREDECESSORS => super::channel::predecessors(),
        SYS_MO_MAP => map(a0, a1, a2, a3, a4, a5),
        SYS_MO_UNMAP => vm::unmap(a0, a1),
        SYS_MO_PROTECT => vm::protect(a0, a1, a2),
        SYS_FUTEX_WAIT | SYS_FUTEX_WAKE | SYS_FUTEX_REQUEUE => futex_call(f.rax, [a0, a1, a2, a3, a4, a5]),
        SYS_CLOCK_READ => super::clock::read(a0).map(|ns| ns as i64),
        SYS_YIELD => {
            super::yield_now();
            Ok(0)
        }
        SYS_RANDOM => random(a0, a1),
        SYS_THREAD_EXIT => match a1 {
            0 => super::exit_thread(a0 as i32),
            EXIT_GROUP => super::exit_group(a0 as i32),
            _ => Err(EINVAL),
        },
        _ => Err(ENOSYS),
    };
    f.rax = result.unwrap_or_else(|e| -e) as u64;
}

/// `mo_map(0, addr, len, 0, prot, flags)`: anonymous private memory (a
/// native server has no objects by handle).
fn map(handle: u64, addr: u64, len: u64, offset: u64, prot: u64, flags: u64) -> SysResult {
    if handle != 0 || offset != 0 || flags & !(MO_FIXED | MO_NOREPLACE | MO_NORESERVE | MO_POPULATE) != 0 || addr % 4096 != 0 {
        return Err(EINVAL);
    }
    let len = vm::range(0, len)?;
    let placement = vm::Placement {
        fixed: flags & (MO_FIXED | MO_NOREPLACE) != 0,
        no_replace: flags & MO_NOREPLACE != 0,
        no_reserve: flags & MO_NORESERVE != 0,
        populate: flags & MO_POPULATE != 0,
    };
    vm::place_and_map(addr, len, vm::prot(prot)?, vm::anon_backing(false, len)?, placement)
}

/// The futex calls (`restricted::SYS_FUTEX_*`) on the caller's memory: a
/// native server's own, or, through the Linux server, its program's.
pub fn futex_call(nr: u64, a: [u64; 6]) -> SysResult {
    match nr {
        SYS_FUTEX_WAIT => {
            let (addr, val, deadline, bitset, flags) = (a[0], a[1] as u32, a[2], a[3] as u32, a[4]);
            if flags & !FUTEX_PRIVATE != 0 {
                return Err(EINVAL);
            }
            super::futex::wait(addr, val, (deadline != 0).then_some(deadline), bitset, flags & FUTEX_PRIVATE != 0)
        }
        SYS_FUTEX_WAKE => {
            let (addr, n, bitset, flags) = (a[0], a[1], a[2] as u32, a[3]);
            if flags & !FUTEX_PRIVATE != 0 {
                return Err(EINVAL);
            }
            super::futex::wake(addr, n, bitset, flags & FUTEX_PRIVATE != 0)
        }
        _ => {
            let (addr, n_wake, n_move, addr2, val, flags) = (a[0], a[1], a[2], a[3], a[4] as u32, a[5]);
            if flags & !(FUTEX_PRIVATE | FUTEX_CMP) != 0 {
                return Err(EINVAL);
            }
            let cmp = (flags & FUTEX_CMP != 0).then_some(val);
            super::futex::requeue(addr, n_wake, n_move, addr2, cmp, flags & FUTEX_PRIVATE != 0)
        }
    }
}

/// `random(buf, len) -> n`: at most `RANDOM_MAX` bytes of the kernel's
/// generator into the caller's memory.
fn random(buf: u64, len: u64) -> SysResult {
    let mut bytes = [0u8; RANDOM_MAX];
    let n = (len as usize).min(RANDOM_MAX);
    crate::random::fill(&mut bytes[..n]);
    let copied = uaccess::copy_to(buf, &bytes[..n]);
    bytes.fill(0);
    copied.map(|_| n as i64)
}

/// `log(buf, len)`: the caller's text on the console.
fn log(buf: u64, len: u64) -> SysResult {
    let n = len.min(LOG_MAX) as usize;
    let mut text = alloc::vec::Vec::new();
    text.try_reserve_exact(n).map_err(|_| ENOMEM)?;
    text.resize(n, 0);
    uaccess::copy_from(buf, &mut text)?;
    crate::drivers::console::write_text(&text);
    Ok(n as i64)
}
