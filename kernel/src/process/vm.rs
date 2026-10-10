//! Mappings in the caller's address space, as its calls ask for them: the
//! mechanism under the Linux server's mmap family (`restricted::SYS_MO_MAP`,
//! `SYS_MO_UNMAP`, `SYS_MO_PROTECT`, `SYS_VM_*`; the Linux semantics are the
//! server's) and a native server's memory. The areas and pages behind them
//! are in `address_space`.

use super::address_space::{self, Backing, Fault, Prot, PAGE};
use super::errno::*;
use super::loader::page_up;
use super::with_current;
use alloc::sync::Arc;

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;

fn errno(f: Fault) -> i64 {
    match f {
        Fault::Oom => ENOMEM,
        Fault::Access => EACCES,
        Fault::Segv | Fault::Bus | Fault::Retry => EINVAL,
    }
}

fn aligned(addr: u64) -> bool {
    addr % PAGE == 0
}

fn mm() -> Result<Arc<address_space::Mm>, i64> {
    with_current(|p| p.mm())
}

/// The length of the range at `addr` in pages' bytes: page-aligned,
/// nonempty, within the program's 64 TiB (EINVAL otherwise).
pub fn range(addr: u64, len: u64) -> Result<u64, i64> {
    let len = len.checked_add(PAGE - 1).ok_or(EINVAL)? & !(PAGE - 1);
    if !aligned(addr) || len == 0 || addr.checked_add(len).is_none_or(|e| e > address_space::USER_END) {
        return Err(EINVAL);
    }
    Ok(len)
}

/// mmap's protection bits.
pub fn prot(bits: u64) -> Result<Prot, i64> {
    if bits & !7 != 0 { Err(EINVAL) } else { Ok(Prot::from_bits(bits)) }
}

/// Anonymous memory of `len` bytes: private (demand-zero) or shared.
pub fn anon_backing(shared: bool, len: u64) -> Result<Backing, i64> {
    if shared { Backing::shared_anon(len / PAGE).map_err(errno) } else { Ok(Backing::Anon) }
}

/// Where a mapping goes and how.
#[derive(Clone, Copy, Default)]
pub struct Placement {
    /// At `addr` exactly (replacing what is there); else `addr` is a hint.
    pub fixed: bool,
    /// Fixed, but EEXIST if anything is mapped there.
    pub no_replace: bool,
    /// Writable private memory is not committed (MAP_NORESERVE).
    pub no_reserve: bool,
    /// The pages are made present now (best effort, as on Linux).
    pub populate: bool,
}

/// Maps `len` bytes with `backing` in the calling process: at `addr` if
/// fixed, else at the hint if that range is free, else in a free range
/// below MMAP_TOP and above the floor (`restricted::SYS_VM_FLOOR`; a native
/// server's: the end of its program). Returns where.
pub fn place_and_map(addr: u64, len: u64, prot: Prot, backing: Backing, how: Placement) -> SysResult {
    if len == 0 || len % PAGE != 0 || len > address_space::USER_END || addr % PAGE != 0 {
        return Err(EINVAL);
    }
    let mm = mm()?;
    let mut space = mm.lock();
    let floor = page_up(space.brk_end);
    let start = if how.fixed {
        let end = addr.checked_add(len).filter(|&e| e <= address_space::USER_END && addr != 0).ok_or(EINVAL)?;
        if how.no_replace && space.overlaps(addr, end) {
            return Err(EEXIST);
        }
        addr
    } else {
        // A hint is taken if the range is free (one lookup of the areas,
        // whatever the length).
        let hint_free = addr != 0
            && addr >= floor
            && addr.checked_add(len).is_some_and(|e| e <= address_space::MMAP_TOP && !space.overlaps(addr, e));
        if hint_free { addr } else { space.find_free(len, floor).ok_or(ENOMEM)? }
    };
    space.map(start, len, prot, backing, how.no_reserve).map_err(errno)?;
    if how.populate && !prot.none() {
        // Best effort, as on Linux.
        let _ = space.populate(start, len, prot.write);
    }
    Ok(start as i64)
}

/// Removes the mappings in [addr, addr + len).
pub fn unmap(addr: u64, len: u64) -> SysResult {
    let len = range(addr, len)?;
    mm()?.lock().unmap(addr, len);
    Ok(0)
}

/// Changes the protection of [addr, addr + len): ENOMEM if part of it is
/// unmapped or making private memory writable exceeds the commit limit,
/// EACCES for a mapping that may not become writable (or executable: a
/// read-only grant).
pub fn protect(addr: u64, len: u64, bits: u64) -> SysResult {
    let len = range(addr, len)?;
    let prot = prot(bits)?;
    mm()?.lock().protect(addr, len, prot).map_err(|e| if e == Fault::Access { EACCES } else { ENOMEM })?;
    Ok(0)
}

/// mremap's contract (`restricted::SYS_VM_REMAP`): grows in place when the
/// space behind is free, else moves (MREMAP_MAYMOVE) or goes to `new_addr`
/// (MREMAP_FIXED). Every range is checked to lie within the program's
/// 64 TiB first (EINVAL, ENOMEM for a length beyond it, as Linux's), so no
/// sum below can wrap.
pub fn remap(old: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> SysResult {
    if !aligned(old) || new_len == 0 || old_len == 0 || flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 {
        return Err(EINVAL);
    }
    if flags & MREMAP_FIXED != 0 && (flags & MREMAP_MAYMOVE == 0 || !aligned(new_addr)) {
        return Err(EINVAL);
    }
    let old_len = range(old, old_len)?;
    let new_len = new_len.checked_add(PAGE - 1).ok_or(ENOMEM)? & !(PAGE - 1);
    if new_len > address_space::USER_END {
        return Err(ENOMEM);
    }
    let fixed = (flags & MREMAP_FIXED != 0).then_some(new_addr);
    if let Some(t) = fixed {
        let t_end = t.checked_add(new_len).filter(|&e| e <= address_space::USER_END).ok_or(EINVAL)?;
        let overlap = t < old + old_len && old < t_end;
        if overlap {
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

/// msync's contract (`restricted::SYS_VM_SYNC`): the flags and the range
/// checked (ENOMEM if part of it is not mapped); the shared mappings of
/// cached objects (the server's page cache of disk files) in the range are
/// the server's to write back with MS_SYNC: how many there are, the first
/// `cap` stored at `out` as (key, first page, end page).
pub fn sync(addr: u64, len: u64, flags: u64, out: u64, cap: u64) -> SysResult {
    const MS_ASYNC: u64 = 1;
    const MS_INVALIDATE: u64 = 2;
    const MS_SYNC: u64 = 4;
    if !aligned(addr) || flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 || flags & (MS_ASYNC | MS_SYNC) == MS_ASYNC | MS_SYNC {
        return Err(EINVAL);
    }
    let end = len.checked_add(PAGE - 1).map(|l| l & !(PAGE - 1)).and_then(|l| addr.checked_add(l)).filter(|&e| e <= address_space::USER_END).ok_or(ENOMEM)?;
    let files = mm()?.lock().file_ranges(addr, end).ok_or(ENOMEM)?;
    if flags & MS_SYNC == 0 {
        return Ok(0);
    }
    let mut cached = 0u64;
    for (cache, pages) in files {
        if let Some(key) = cache.cached_key() {
            if cached < cap {
                let words = [key, pages.start, pages.end];
                let bytes: alloc::vec::Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
                super::uaccess::copy_to_server(out + cached * 24, &bytes)?;
            }
            cached += 1;
        }
    }
    Ok(cached as i64)
}

/// Drops the pages of private mappings in [addr, addr + len)
/// (`restricted::SYS_VM_DISCARD`: zero or the file's again on the next
/// access).
pub fn discard(addr: u64, len: u64) -> SysResult {
    if !aligned(addr) {
        return Err(EINVAL);
    }
    if len > 0 {
        let len = range(addr, len)?;
        mm()?.lock().discard(addr, len).map_err(errno)?;
    }
    Ok(0)
}
