use super::address_space::{AddressSpace, Backing, Fault, Prot, PAGE};
use super::elf::{Elf, PF_W, PF_X, PT_LOAD, PT_PHDR};
use super::errno::*;
use alloc::string::String;
use alloc::vec::Vec;

const STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
/// The stack area at start (it holds the arguments); it grows on demand up
/// to address_space::STACK_LIMIT.
const STACK_SIZE: u64 = 256 * 1024;

pub struct Image {
    pub space: AddressSpace,
    pub entry: u64,
    pub sp: u64,
    /// First free address after the highest segment (start of brk).
    pub brk: u64,
}

pub fn page_up(x: u64) -> u64 {
    x.saturating_add(4095) & !4095
}

pub fn load(image: &[u8], args: &[String], envs: &[String]) -> Result<Image, i64> {
    let elf = Elf::parse(image).map_err(|_| ENOEXEC)?;
    // iretq to a non-user entry point would fault in ring 0.
    if elf.entry >= super::address_space::USER_END {
        return Err(ENOEXEC);
    }
    let mut space = AddressSpace::new().ok_or(ENOMEM)?;

    let mut phdr = None;
    let mut brk = 0;
    for ph in elf.program_headers() {
        match ph.kind {
            PT_LOAD => {
                let prot = Prot { read: true, write: ph.flags & PF_W != 0, exec: ph.flags & PF_X != 0 };
                map_segment(&mut space, ph.vaddr, ph.memsz, prot)?;
                let bytes = elf.segment_bytes(&ph).map_err(|_| ENOEXEC)?;
                space.write(ph.vaddr, bytes).map_err(fault_errno)?;
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
    Ok(Image { space, entry: elf.entry, sp, brk })
}

fn fault_errno(f: Fault) -> i64 {
    match f {
        Fault::Oom => ENOMEM,
        _ => ENOEXEC,
    }
}

/// An area for a loadable segment. Segments may share a boundary page; that
/// page gets the union of their rights.
fn map_segment(space: &mut AddressSpace, vaddr: u64, memsz: u64, prot: Prot) -> Result<(), i64> {
    let start = vaddr & !(PAGE - 1);
    let end = page_up(vaddr.checked_add(memsz).ok_or(ENOEXEC)?);
    let mut at = start;
    while at < end {
        match space.vma(at).map(|v| (v.prot, v.end)) {
            Some((old, _)) => {
                let union = Prot { read: old.read || prot.read, write: old.write || prot.write, exec: old.exec || prot.exec };
                space.protect(at, PAGE, union).map_err(fault_errno)?;
                at += PAGE;
            }
            None => {
                let next = (at..end).step_by(PAGE as usize).find(|&a| space.vma(a).is_some()).unwrap_or(end);
                space.map(at, next - at, prot, Backing::Anon, false).map_err(fault_errno)?;
                at = next;
            }
        }
    }
    Ok(())
}

/// Linux initial stack: argc, argv[], NULL, envp[], NULL, auxv pairs, AT_NULL.
fn build_stack(space: &mut AddressSpace, args: &[String], envs: &[String], auxv: &[(u64, u64)]) -> Result<u64, Fault> {
    space.map_stack(STACK_TOP, STACK_SIZE)?;
    let mut sp = STACK_TOP;
    let mut push_str = |space: &mut AddressSpace, s: &str| -> Result<u64, Fault> {
        sp -= s.len() as u64 + 1;
        space.write(sp, s.as_bytes())?;
        Ok(sp)
    };
    let argv = args.iter().map(|a| push_str(space, a)).collect::<Result<Vec<_>, _>>()?;
    let envp = envs.iter().map(|e| push_str(space, e)).collect::<Result<Vec<_>, _>>()?;
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
