//! Memory system calls (phase R4): Linux's semantics of mmap, munmap,
//! mprotect, mremap, madvise, msync and the mlock family, brk (with the
//! process model, R8) and membarrier (ADR 0021), over the kernel's mapping calls. The kernel keeps the page tables and the areas; this is
//! where the arguments are checked and turned into mappings.

use restricted::*;

const PAGE: u64 = 4096;
const PROT_WRITE: u64 = 2;
/// The end of the program's memory (ADR 0003).
const USER_END: u64 = SHARED_BASE;

const EINVAL: i64 = 22;
const ENOMEM: i64 = 12;

const MAP_SHARED: u64 = 0x01;
const MAP_SHARED_VALIDATE: u64 = 0x03;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_32BIT: u64 = 0x40;
const MAP_NORESERVE: u64 = 0x4000;
const MAP_POPULATE: u64 = 0x8000;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

const MADV_DONTNEED: u64 = 4;
const MADV_FREE: u64 = 8;

const SYS_MMAP: u64 = 9;
const SYS_BRK: u64 = 12;
const SYS_MPROTECT: u64 = 10;
const SYS_MUNMAP: u64 = 11;
const SYS_MREMAP: u64 = 25;
const SYS_MSYNC: u64 = 26;
const SYS_MADVISE: u64 = 28;
const SYS_MLOCK: u64 = 149;
const SYS_MUNLOCKALL: u64 = 152;
const SYS_MLOCK2: u64 = 325;
/// (Linux's number; the kernel's mechanism is `restricted::SYS_MEMBARRIER`.)
const SYS_MEMBARRIER_LINUX: u64 = 324;

fn page_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE - 1).map(|v| v & !(PAGE - 1))
}

fn aligned(x: u64) -> bool {
    x % PAGE == 0
}

/// The result of a memory system call in `s`, or None for other calls.
pub fn handle(s: &State) -> Option<i64> {
    let (a0, a1, a2, a3, a4, a5) = (s.rdi, s.rsi, s.rdx, s.r10, s.r8, s.r9);
    Some(match s.rax {
        SYS_BRK => brk(a0),
        SYS_MMAP => mmap(a0, a1, a2, a3, a4, a5),
        SYS_MUNMAP => munmap(a0, a1),
        SYS_MPROTECT => mprotect(a0, a1, a2),
        SYS_MREMAP => crate::syscall(SYS_VM_REMAP, [a0, a1, a2, a3, a4, 0]),
        SYS_MSYNC => msync(a0, a1, a2),
        SYS_MADVISE => madvise(a0, a1, a2),
        // Nothing is ever swapped out.
        SYS_MLOCK..=SYS_MUNLOCKALL | SYS_MLOCK2 => 0,
        SYS_MEMBARRIER_LINUX => membarrier(a0, a1),
        _ => return None,
    })
}

fn mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, mut offset: u64) -> i64 {
    if len == 0 || !aligned(addr) || !aligned(offset) || prot & !7 != 0 {
        return -EINVAL;
    }
    let Some(len) = page_up(len) else { return -EINVAL };
    if offset.checked_add(len).is_none() || len >= USER_END {
        return -EINVAL;
    }
    let sharing = flags & MAP_SHARED_VALIDATE;
    if sharing == 0 {
        return -EINVAL;
    }
    // Only the lowest 2 GiB would do, and programs live above.
    if flags & MAP_32BIT != 0 {
        return -ENOMEM;
    }
    let shared = sharing == MAP_SHARED || sharing == MAP_SHARED_VALIDATE;
    let mut mo_flags = 0;
    if shared {
        mo_flags |= MO_SHARED;
    }
    if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        mo_flags |= MO_FIXED;
    }
    if flags & MAP_FIXED_NOREPLACE != 0 {
        mo_flags |= MO_NOREPLACE;
    }
    if flags & MAP_NORESERVE != 0 {
        mo_flags |= MO_NORESERVE;
    }
    if flags & MAP_POPULATE != 0 {
        mo_flags |= MO_POPULATE;
    }
    // The object to map: none (anonymous private memory), a new one
    // (anonymous shared memory), or the descriptor's file's object.
    let mapping = if flags & MAP_ANONYMOUS == 0 {
        crate::files::map_object(fd, shared, prot & PROT_WRITE != 0)
    } else {
        Ok(crate::files::Mapping::Anonymous { read_only: false })
    };
    // The handle to map, and whether it is this call's to close.
    let (handle, own) = match mapping {
        Ok(crate::files::Mapping::Object(h, read_only)) => {
            if read_only {
                mo_flags |= MO_READONLY;
            }
            (h as i64, true)
        }
        Err(e) => (-e, false),
        // Anonymous memory (also zero's): the offset means nothing.
        Ok(crate::files::Mapping::Anonymous { read_only }) if shared => {
            offset = 0;
            if read_only {
                mo_flags |= MO_READONLY;
            }
            (crate::syscall(SYS_MO_CREATE, [len / PAGE, 0, 0, 0, 0, 0]), true)
        }
        Ok(crate::files::Mapping::Anonymous { .. }) => {
            offset = 0;
            (0, false)
        }
    };
    if handle < 0 {
        return handle;
    }
    let mapped = crate::syscall(SYS_MO_MAP, [handle as u64, addr, len, offset, prot, mo_flags]);
    if own && handle > 0 {
        // The mapping holds the object now.
        crate::syscall(SYS_HANDLE_CLOSE, [handle as u64, 0, 0, 0, 0, 0]);
    }
    mapped
}

/// brk(addr) (phase R8, the process model's): the heap is anonymous memory from the end of
/// the program (the break exec set) to the break, committed as it grows. It may not run
/// into a mapping above it, nor into the stack's room and its guard gap (`Brk::limit`; then
/// the break stays). The kernel places mappings above the break (`SYS_VM_FLOOR`). Returns
/// the (possibly unchanged) break.
fn brk(addr: u64) -> i64 {
    let Some(mm) = crate::process::mm_of(crate::local::pid()) else { return 0 };
    let mut m = mm.lock();
    let b = &mut m.brk;
    if addr < b.start || addr > USER_END {
        return b.end as i64;
    }
    let (Some(old_top), Some(new_top)) = (page_up(b.end), page_up(addr)) else { return b.end as i64 };
    if new_top > old_top {
        if new_top > b.limit {
            return b.end as i64;
        }
        let r = crate::syscall(SYS_MO_MAP, [0, old_top, new_top - old_top, 0, 3, MO_FIXED | MO_NOREPLACE]);
        if r < 0 {
            return b.end as i64;
        }
    } else if new_top < old_top {
        crate::syscall(SYS_MO_UNMAP, [new_top, old_top - new_top, 0, 0, 0, 0]);
    }
    b.end = addr;
    crate::syscall(SYS_VM_FLOOR, [new_top, 0, 0, 0, 0, 0]);
    addr as i64
}

fn munmap(addr: u64, len: u64) -> i64 {
    if !aligned(addr) || len == 0 || addr.checked_add(len).is_none_or(|e| e > USER_END) {
        return -EINVAL;
    }
    let Some(len) = page_up(len) else { return -EINVAL };
    crate::syscall(SYS_MO_UNMAP, [addr, len, 0, 0, 0, 0])
}

/// ENOMEM if part of the range is unmapped or making private memory
/// writable exceeds the commit limit, EACCES for a shared mapping of a file
/// not opened for writing (the kernel's answers).
fn mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    if !aligned(addr) || prot & !7 != 0 {
        return -EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let Some(len) = page_up(len) else { return -ENOMEM };
    if addr.checked_add(len).is_none_or(|e| e > USER_END) {
        return -ENOMEM;
    }
    crate::syscall(SYS_MO_PROTECT, [addr, len, prot, 0, 0, 0])
}

/// msync: the kernel checks the range and writes back its own files; the
/// /data files mapped shared in it are written back here (MS_SYNC).
fn msync(addr: u64, len: u64, flags: u64) -> i64 {
    // (key, first page, end page) of each /data file in the range.
    let mut found = [[0u64; 3]; 16];
    let n = crate::syscall(SYS_VM_SYNC, [addr, len, flags, found.as_mut_ptr() as u64, found.len() as u64, 0]);
    if n <= 0 {
        return n;
    }
    let mut result = 0;
    for &[key, first, end] in found.iter().take(n as usize) {
        if let Err(e) = crate::datafs::msync(key, first, end) {
            result = -e;
        }
    }
    // More mappings than fit: the rest written back with everything else.
    if n as usize > found.len() {
        if let Err(e) = crate::datafs::sync_all() {
            result = -e;
        }
    }
    result
}

const EPERM: i64 = 1;

/// membarrier(2)'s commands (its `cmd`, and the registration bits `GET_REGISTRATIONS`
/// reports: each REGISTER_* command's own value).
const MEMBARRIER_CMD_QUERY: u32 = 0;
const MEMBARRIER_CMD_GLOBAL: u32 = 1;
const MEMBARRIER_CMD_GLOBAL_EXPEDITED: u32 = 2;
const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: u32 = 4;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: u32 = 8;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: u32 = 16;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE: u32 = 32;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: u32 = 64;
const MEMBARRIER_CMD_GET_REGISTRATIONS: u32 = 512;
/// What QUERY reports: everything but the rseq commands (128 and 256), as a Linux without
/// CONFIG_RSEQ (there is no rseq, ADR 0021).
const MEMBARRIER_SUPPORTED: u32 = MEMBARRIER_CMD_GLOBAL
    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_GET_REGISTRATIONS;

/// membarrier(cmd, flags, cpu_id): the barriers are the kernel's (`SYS_MEMBARRIER`: an
/// interrupt on each CPU that runs the address space, or on every CPU), the registrations
/// the address space's (`process::Mm`: copied by fork, shared by CLONE_VM, none after
/// execve). The private expedited commands need their registration (EPERM); the global
/// ones reach every CPU, which covers every registered process (and is what GLOBAL asks:
/// every running thread passes a barrier, the others do when they are switched in). Only
/// the rseq command takes a flag, and there is no rseq: a flag, the rseq commands and
/// unknown ones are EINVAL; `cpu_id` matters only with the flag.
fn membarrier(cmd: u64, flags: u64) -> i64 {
    // (`cmd` is an int.)
    let cmd = cmd as u32;
    if flags as u32 != 0 {
        return -EINVAL;
    }
    let barrier = |scope| crate::syscall(SYS_MEMBARRIER, [scope, 0, 0, 0, 0, 0]);
    let Some(mm) = crate::process::mm_of(crate::local::pid()) else { return -EINVAL };
    match cmd {
        MEMBARRIER_CMD_QUERY => MEMBARRIER_SUPPORTED as i64,
        MEMBARRIER_CMD_GLOBAL | MEMBARRIER_CMD_GLOBAL_EXPEDITED => barrier(MEMBARRIER_ALL),
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            mm.lock().membarrier |= cmd;
            0
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => {
            // (Each command's registration is the command's value shifted once.)
            if mm.lock().membarrier & (cmd << 1) == 0 {
                return -EPERM;
            }
            barrier(MEMBARRIER_SPACE)
        }
        MEMBARRIER_CMD_GET_REGISTRATIONS => mm.lock().membarrier as i64,
        _ => -EINVAL,
    }
}

/// DONTNEED and FREE drop private pages; other advice is accepted.
fn madvise(addr: u64, len: u64, advice: u64) -> i64 {
    if !aligned(addr) {
        return -EINVAL;
    }
    if matches!(advice, MADV_DONTNEED | MADV_FREE) && len > 0 {
        let Some(len) = page_up(len) else { return -EINVAL };
        return crate::syscall(SYS_VM_DISCARD, [addr, len, 0, 0, 0, 0]);
    }
    0
}
