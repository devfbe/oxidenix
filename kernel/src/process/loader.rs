//! Loading a static ELF program into a new address space: its segments are
//! mapped from the file's page cache (demand-paged, private), so every
//! process running a program shares its unchanged pages.

use super::address_space::{AddressSpace, Backing, Fault, Hold, Prot, PAGE, USER_END};
use super::elf::{Elf, ProgramHeader, PF_W, PF_X, PT_LOAD, PT_PHDR};
use super::errno::*;
use crate::fs::cache::PageCache;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const STACK_TOP: u64 = super::address_space::USER_END - 0x1000;
/// The stack area at start (it holds the arguments); it grows on demand up
/// to address_space::STACK_LIMIT.
const STACK_SIZE: u64 = 256 * 1024;
/// Most bytes of ELF and program headers read (the table is near the start).
const MAX_HEADERS: u64 = 64 * 1024;

pub struct Image {
    pub space: AddressSpace,
    pub entry: u64,
    pub sp: u64,
}

pub fn page_up(x: u64) -> u64 {
    x.saturating_add(4095) & !4095
}

fn page_down(x: u64) -> u64 {
    x & !(PAGE - 1)
}

/// The program in `cache`. `file` is kept alive by the mappings; `exe`
/// keeps it from being written while the program runs.
pub fn load(cache: &Arc<PageCache>, file: Option<Hold>, exe: Option<Hold>, args: &[String], envs: &[String]) -> Result<Image, i64> {
    let size = cache.size();
    let mut head = vec![0u8; size.min(64) as usize];
    cache.read(0, &mut head)?;
    let header = Elf::parse_header(&head).map_err(|_| ENOEXEC)?;
    let table_end = header.table_end().filter(|&e| e <= size.min(MAX_HEADERS)).ok_or(ENOEXEC)?;
    let mut headers = Vec::new();
    headers.try_reserve_exact(table_end as usize).map_err(|_| ENOMEM)?;
    headers.resize(table_end as usize, 0);
    cache.read(0, &mut headers)?;
    let elf = Elf::parse(&headers).map_err(|_| ENOEXEC)?;
    // iretq to a non-user entry point would fault in ring 0.
    if elf.entry >= USER_END {
        return Err(ENOEXEC);
    }
    let mut space = AddressSpace::new().ok_or(ENOMEM)?;
    space.exe = exe;

    let mut phdr = None;
    let mut brk = 0;
    for ph in elf.program_headers() {
        match ph.kind {
            PT_LOAD => {
                map_segment(&mut space, cache, &file, &ph, size)?;
                if ph.offset == 0 {
                    phdr.get_or_insert(ph.vaddr + elf.phoff);
                }
                brk = brk.max(page_up(ph.vaddr + ph.memsz));
            }
            PT_PHDR => phdr = Some(ph.vaddr),
            _ => {}
        }
    }

    let auxv = [
        (3, phdr.unwrap_or(0)),    // AT_PHDR
        (4, elf.phentsize as u64), // AT_PHENT
        (5, elf.phnum as u64),     // AT_PHNUM
        (6, 4096),                 // AT_PAGESZ
        (9, elf.entry),            // AT_ENTRY
        (11, 0),                   // AT_UID
        (12, 0),                   // AT_EUID
        (13, 0),                   // AT_GID
        (14, 0),                   // AT_EGID
    ];
    let sp = build_stack(&mut space, args, envs, &auxv).map_err(|_| ENOMEM)?;
    space.brk_start = brk;
    space.brk_end = brk;
    Ok(Image { space, entry: elf.entry, sp })
}

fn fault_errno(f: Fault) -> i64 {
    match f {
        Fault::Oom => ENOMEM,
        _ => ENOEXEC,
    }
}

/// Maps a loadable segment: its file part from the page cache (private,
/// copy-on-write), the rest of its last file page cleared, and zeroed
/// anonymous memory up to its memory size (bss).
fn map_segment(space: &mut AddressSpace, cache: &Arc<PageCache>, file: &Option<Hold>, ph: &ProgramHeader, size: u64) -> Result<(), i64> {
    let file_end = ph.offset.checked_add(ph.filesz).filter(|&e| e <= size).ok_or(ENOEXEC)?;
    let mem_end = ph.vaddr.checked_add(ph.memsz).filter(|&e| e <= USER_END).ok_or(ENOEXEC)?;
    // A page maps one file page: offsets and addresses must agree in it.
    if ph.filesz > ph.memsz || ph.vaddr % PAGE != ph.offset % PAGE || file_end < ph.offset {
        return Err(ENOEXEC);
    }
    let prot = Prot { read: true, write: ph.flags & PF_W != 0, exec: ph.flags & PF_X != 0 };
    let data_end = ph.vaddr + ph.filesz;
    if ph.filesz > 0 {
        let (start, end) = (page_down(ph.vaddr), page_up(data_end));
        let delta = page_down(ph.offset) as i128 - start as i128;
        let mut at = start;
        while at < end {
            if space.vma(at).is_some() {
                // Shared with the previous segment: both rights; its own
                // bytes unless the page already maps the same file page.
                let same = space.vma(at).is_some_and(|v| matches!(&v.backing,
                    Backing::File { cache: c, offset, .. } if Arc::ptr_eq(c, cache) && *offset as i128 - v.start as i128 == delta));
                union_prot(space, at, prot)?;
                if !same {
                    copy_bytes(space, cache, ph, at.max(ph.vaddr), (at + PAGE).min(data_end))?;
                }
                at += PAGE;
                continue;
            }
            let next = (at..end).step_by(PAGE as usize).find(|&a| space.vma(a).is_some()).unwrap_or(end);
            let offset = (at as i128 + delta) as u64;
            let backing = Backing::File { cache: cache.clone(), offset, shared: false, may_write: false, _hold: file.clone() };
            space.map(at, next - at, prot, backing, false).map_err(fault_errno)?;
            at = next;
        }
        // Bytes after the file part in its last page belong to the bss.
        if data_end % PAGE != 0 && mem_end > data_end {
            let zeros = [0u8; PAGE as usize];
            let stop = page_up(data_end).min(mem_end);
            space.write(data_end, &zeros[..(stop - data_end) as usize]).map_err(fault_errno)?;
        }
    }
    let bss = if ph.filesz > 0 { page_up(data_end) } else { page_down(ph.vaddr) };
    if mem_end > bss {
        map_anon(space, bss, page_up(mem_end), prot)?;
    }
    Ok(())
}

/// Writes the segment's file bytes for [from, to) (a page shared with
/// another segment that maps different file contents).
fn copy_bytes(space: &mut AddressSpace, cache: &PageCache, ph: &ProgramHeader, from: u64, to: u64) -> Result<(), i64> {
    if from >= to {
        return Ok(());
    }
    let mut buf = [0u8; PAGE as usize];
    let n = (to - from) as usize;
    cache.read(ph.offset + (from - ph.vaddr), &mut buf[..n])?;
    space.write(from, &buf[..n]).map_err(fault_errno)
}

fn union_prot(space: &mut AddressSpace, at: u64, prot: Prot) -> Result<(), i64> {
    let old = space.vma(at).map(|v| v.prot).unwrap_or_default();
    let union = Prot { read: old.read || prot.read, write: old.write || prot.write, exec: old.exec || prot.exec };
    space.protect(at, PAGE, union).map_err(fault_errno)
}

/// Zeroed memory for [start, end); pages already mapped by a segment get
/// the union of both rights.
fn map_anon(space: &mut AddressSpace, start: u64, end: u64, prot: Prot) -> Result<(), i64> {
    let mut at = start;
    while at < end {
        if space.vma(at).is_some() {
            union_prot(space, at, prot)?;
            at += PAGE;
            continue;
        }
        let next = (at..end).step_by(PAGE as usize).find(|&a| space.vma(a).is_some()).unwrap_or(end);
        space.map(at, next - at, prot, Backing::Anon, false).map_err(fault_errno)?;
        at = next;
    }
    Ok(())
}

/// Linux initial stack: argc, argv[], NULL, envp[], NULL, auxv pairs, AT_NULL.
/// The strings lie at the top as Linux puts them: the arguments in
/// order, each right after the one before, then the environment (programs
/// rely on it: libuv's process title overwrites the arguments' memory up
/// to the last one's end).
fn build_stack(space: &mut AddressSpace, args: &[String], envs: &[String], auxv: &[(u64, u64)]) -> Result<u64, Fault> {
    space.map_stack(STACK_TOP, STACK_SIZE)?;
    let total: u64 = args.iter().chain(envs).map(|s| s.len() as u64 + 1).sum();
    // The word below the top stays 0, as Linux's end marker.
    let strings = STACK_TOP.checked_sub(8 + total).filter(|&s| s >= STACK_TOP - STACK_SIZE).ok_or(Fault::Oom)?;
    let mut at = strings;
    let mut place = |space: &mut AddressSpace, s: &str| -> Result<u64, Fault> {
        let addr = at;
        space.write(addr, s.as_bytes())?;
        space.write(addr + s.len() as u64, &[0])?;
        at += s.len() as u64 + 1;
        Ok(addr)
    };
    let argv = args.iter().map(|a| place(space, a)).collect::<Result<Vec<_>, _>>()?;
    let envp = envs.iter().map(|e| place(space, e)).collect::<Result<Vec<_>, _>>()?;
    let mut sp = strings & !0xf;
    sp -= 16;
    let random = sp;
    let seed = unsafe { core::arch::x86_64::_rdtsc() };
    space.write(random, &[seed.to_le_bytes(), seed.rotate_left(29).to_le_bytes()].concat())?;

    let mut words: Vec<u64> = Vec::new();
    words.push(argv.len() as u64);
    words.extend(&argv);
    words.push(0);
    words.extend(&envp);
    words.push(0);
    for &(key, val) in auxv {
        words.extend([key, val]);
    }
    words.extend([25, random, 0, 0]); // AT_RANDOM, AT_NULL

    sp = (sp - words.len() as u64 * 8) & !0xf;
    if sp < STACK_TOP - STACK_SIZE {
        return Err(Fault::Oom);
    }
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    space.write(sp, &bytes)?;
    Ok(sp)
}
