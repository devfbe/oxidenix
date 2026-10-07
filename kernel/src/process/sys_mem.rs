//! Memory syscalls: brk, mmap, munmap, mprotect, mremap, madvise and the
//! ones that need no work here (msync, mlock). The areas and pages behind
//! them are in `address_space`.

use super::address_space::{self, Backing, Fault, Prot, PAGE};
use super::errno::*;
use super::loader::page_up;
use super::with_current;
use crate::fs::Device;
use alloc::sync::Arc;

const MAP_PRIVATE: u64 = 0x02;
const MAP_SHARED_VALIDATE: u64 = 0x03;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_32BIT: u64 = 0x40;
const MAP_NORESERVE: u64 = 0x4000;
const MAP_POPULATE: u64 = 0x8000;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;

const MADV_DONTNEED: u64 = 4;
const MADV_FREE: u64 = 8;

fn errno(f: Fault) -> i64 {
    match f {
        Fault::Oom => ENOMEM,
        Fault::Access => EACCES,
        Fault::Segv | Fault::Bus => EINVAL,
    }
}

fn aligned(addr: u64) -> bool {
    addr % PAGE == 0
}

fn mm() -> Result<Arc<address_space::Mm>, i64> {
    with_current(|p| p.mm())
}

/// brk(2): the heap is one anonymous area from the end of the program; it
/// is committed as it grows. Returns the (possibly unchanged) break.
pub fn brk(addr: u64) -> SysResult {
    let mm = mm()?;
    let mut space = mm.lock();
    let (brk_start, brk_end) = (space.brk_start, space.brk_end);
    if addr < brk_start {
        return Ok(brk_end as i64);
    }
    let (old_top, new_top) = (page_up(brk_end), page_up(addr));
    if new_top > old_top {
        // The heap may not run into a mapping above it.
        if space.vma(old_top).is_some() || (old_top..new_top).step_by(PAGE as usize).any(|a| space.vma(a).is_some()) {
            return Ok(brk_end as i64);
        }
        if space.map(old_top, new_top - old_top, Prot::RW, Backing::Anon, false).is_err() {
            return Ok(brk_end as i64);
        }
    } else if new_top < old_top && new_top >= page_up(brk_start) {
        space.unmap(new_top, old_top - new_top);
    }
    space.brk_end = addr;
    Ok(addr as i64)
}

pub fn mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> SysResult {
    if len == 0 || !aligned(addr) || !aligned(offset) || prot & !7 != 0 {
        return Err(EINVAL);
    }
    let len = page_up(len);
    if offset.checked_add(len).is_none() || len >= address_space::USER_END {
        return Err(EINVAL);
    }
    let sharing = flags & MAP_SHARED_VALIDATE;
    if sharing == 0 {
        return Err(EINVAL);
    }
    if flags & MAP_32BIT != 0 {
        return Err(ENOMEM);
    }
    let prot = Prot::from_bits(prot);
    let shared = sharing != MAP_PRIVATE;
    let anon = |shared: bool| if shared { Backing::shared_anon(len / PAGE).map_err(errno) } else { Ok(Backing::Anon) };
    let backing = if flags & MAP_ANONYMOUS != 0 {
        anon(shared)?
    } else {
        let f = with_current(|p| p.file(fd))?;
        if !f.readable() || (shared && prot.write && !f.writable()) {
            return Err(EACCES);
        }
        let inode = f.inode().ok_or(ENODEV)?.clone();
        match inode.device() {
            // /dev/zero: anonymous memory.
            Some(Device::Zero) => anon(shared)?,
            Some(_) => return Err(ENODEV),
            None => {
                let cache = inode.cache().map_err(|_| ENODEV)?;
                Backing::File { cache, offset, shared, may_write: f.writable(), _file: Some(inode) }
            }
        }
    };
    let noreserve = flags & MAP_NORESERVE != 0;
    let mm = mm()?;
    let start = {
        let mut space = mm.lock();
        let floor = page_up(space.brk_end);
        let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
        let start = if fixed {
            if addr.checked_add(len).is_none_or(|e| e > address_space::USER_END) || addr == 0 {
                return Err(EINVAL);
            }
            if flags & MAP_FIXED_NOREPLACE != 0 && (addr..addr + len).step_by(PAGE as usize).any(|a| space.vma(a).is_some()) {
                return Err(EEXIST);
            }
            addr
        } else {
            // A hint is taken if the range is free.
            let hint_free = addr != 0
                && addr >= floor
                && addr.checked_add(len).is_some_and(|e| e <= address_space::MMAP_TOP)
                && (addr..addr + len).step_by(PAGE as usize).all(|a| space.vma(a).is_none());
            if hint_free { addr } else { space.find_free(len, floor).ok_or(ENOMEM)? }
        };
        space.map(start, len, prot, backing, noreserve).map_err(errno)?;
        if flags & MAP_POPULATE != 0 && !prot.none() {
            // Best effort, as on Linux.
            let _ = space.populate(start, len, prot.write);
        }
        start
    };
    Ok(start as i64)
}

pub fn munmap(addr: u64, len: u64) -> SysResult {
    if !aligned(addr) || len == 0 || addr.checked_add(len).is_none_or(|e| e > address_space::USER_END) {
        return Err(EINVAL);
    }
    mm()?.lock().unmap(addr, page_up(len));
    Ok(0)
}

/// mprotect(2): fails with ENOMEM if part of the range is unmapped or if
/// making private memory writable exceeds the commit limit, and with
/// EACCES for a shared mapping of a file not opened for writing.
pub fn mprotect(addr: u64, len: u64, prot: u64) -> SysResult {
    if !aligned(addr) || prot & !7 != 0 {
        return Err(EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    mm()?.lock().protect(addr, page_up(len), Prot::from_bits(prot)).map_err(|e| if e == Fault::Access { EACCES } else { ENOMEM })?;
    Ok(0)
}

/// mremap(2): grows in place when the space behind is free, else moves
/// (MREMAP_MAYMOVE) or goes to `new_addr` (MREMAP_FIXED).
pub fn mremap(old: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> SysResult {
    if !aligned(old) || new_len == 0 || old_len == 0 || flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 {
        return Err(EINVAL);
    }
    if flags & MREMAP_FIXED != 0 && (flags & MREMAP_MAYMOVE == 0 || !aligned(new_addr)) {
        return Err(EINVAL);
    }
    let (old_len, new_len) = (page_up(old_len), page_up(new_len));
    let fixed = (flags & MREMAP_FIXED != 0).then_some(new_addr);
    if let Some(t) = fixed {
        let overlap = t < old + old_len && old < t + new_len;
        if overlap || t.checked_add(new_len).is_none_or(|e| e > address_space::USER_END) {
            return Err(EINVAL);
        }
    }
    let mm = mm()?;
    let mut space = mm.lock();
    let floor = page_up(space.brk_end);
    match space.remap(old, old_len, new_len, flags & MREMAP_MAYMOVE != 0, fixed, floor) {
        Ok(at) => Ok(at as i64),
        Err(Fault::Oom) => Err(ENOMEM),
        Err(_) => Err(EFAULT),
    }
}

/// madvise(2): DONTNEED/FREE drop private pages; other advice is accepted.
pub fn madvise(addr: u64, len: u64, advice: u64) -> SysResult {
    if !aligned(addr) {
        return Err(EINVAL);
    }
    if matches!(advice, MADV_DONTNEED | MADV_FREE) && len > 0 {
        mm()?.lock().discard(addr, page_up(len)).map_err(errno)?;
    }
    Ok(0)
}
