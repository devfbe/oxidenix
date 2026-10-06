//! Memory syscalls: brk, mmap, munmap.

use super::errno::*;
use super::loader::page_up;
use super::with_current;
use x86_64::structures::paging::PageTableFlags;

const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

pub fn brk(addr: u64) -> SysResult {
    with_current(|p| {
        if addr < p.brk_start {
            return Ok(p.brk_end as i64);
        }
        let (old_top, new_top) = (page_up(p.brk_end), page_up(addr));
        if new_top > old_top {
            let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
            if p.space()?.map_zeroed(old_top, new_top - old_top, flags).is_err() {
                return Ok(p.brk_end as i64);
            }
        }
        p.brk_end = addr;
        Ok(addr as i64)
    })
}

fn page_flags(prot: u64) -> PageTableFlags {
    let mut flags = PageTableFlags::empty();
    if prot & PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if prot & PROT_EXEC == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

pub fn mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> SysResult {
    if len == 0 || addr % 4096 != 0 || offset % 4096 != 0 {
        return Err(EINVAL);
    }
    let len = page_up(len);
    // File contents are copied (MAP_PRIVATE semantics, no write-back).
    let file = if flags & MAP_ANONYMOUS == 0 {
        Some(with_current(|p| p.file(fd))?)
    } else {
        None
    };
    let start = with_current(|p| -> Result<u64, i64> {
        let start = if flags & MAP_FIXED != 0 {
            p.space()?.unmap(addr, len);
            addr
        } else {
            p.mmap_next = p.mmap_next.checked_sub(len).filter(|&a| a > p.brk_end).ok_or(ENOMEM)?;
            p.mmap_next
        };
        p.space()?.map_zeroed(start, len, page_flags(prot)).map_err(|_| ENOMEM)?;
        Ok(start)
    })?;
    if let Some(file) = file {
        let inode = file.inode().ok_or(EINVAL)?;
        let mut page = [0u8; 4096];
        for done in (0..len).step_by(4096) {
            let n = inode.read_at(offset + done, &mut page)?;
            if n == 0 {
                break;
            }
            with_current(|p| p.space()?.write(start + done, &page[..n]).map_err(|_| EFAULT))?;
        }
    }
    Ok(start as i64)
}

pub fn munmap(addr: u64, len: u64) -> SysResult {
    if addr % 4096 != 0 || len == 0 {
        return Err(EINVAL);
    }
    with_current(|p| {
        p.space()?.unmap(addr, page_up(len));
        Ok(0)
    })
}
