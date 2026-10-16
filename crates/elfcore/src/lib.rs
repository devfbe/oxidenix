//! The ELF core file (core(5), ADR 0022), as Linux's binfmt_elf writes it for an x86-64
//! process that dumps core: the file header, the program headers (one `PT_NOTE`, a `PT_LOAD`
//! per mapping, with Linux's extended numbering past 65534 of them), the notes (`NT_PRSTATUS`
//! per thread, `NT_PRPSINFO`, `NT_SIGINFO`, `NT_AUXV`, `NT_FILE`, `NT_PRFPREG`), which part of
//! each mapping goes in (`filter`, Linux's `vma_dump_size` under `coredump_filter`), and the
//! file's name from `core_pattern` (`pattern`). It only computes: the Linux server gathers the
//! process's state, reads its memory and writes the file (`servers/linux/src/coredump.rs`).
//!
//! The file is laid out as Linux's: the header, the program headers, the notes, then (from
//! the next page boundary) each mapping's dumped part in the order of the program headers;
//! with extended numbering a section header follows the data. Allocations are fallible
//! (`Error`): the server's heap may be short, and a dump that cannot be made is no dump.

#![no_std]

extern crate alloc;

pub mod filter;
pub mod pattern;

use alloc::vec::Vec;

/// A dump's work could not get memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

impl From<alloc::collections::TryReserveError> for Error {
    fn from(_: alloc::collections::TryReserveError) -> Error {
        Error
    }
}

/// The page size of the dump's layout (Linux's ELF_EXEC_PAGESIZE on x86-64).
pub const PAGE: u64 = 4096;
/// The least RLIMIT_CORE a dump to a file is attempted with (Linux's `min_coredump` of the
/// ELF format: one page).
pub const MIN_COREDUMP: u64 = PAGE;

pub const ELF_HEADER_SIZE: usize = 64;
pub const PHDR_SIZE: usize = 56;
pub const SHDR_SIZE: usize = 64;
const ET_CORE: u16 = 4;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
/// e_phnum's marker for extended numbering (the count is the section header's `sh_info`).
pub const PN_XNUM: u16 = 0xffff;

/// Segment flags.
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

/// Note types (`n_type`), all with the name "CORE".
pub const NT_PRSTATUS: u32 = 1;
pub const NT_PRFPREG: u32 = 2;
pub const NT_PRPSINFO: u32 = 3;
pub const NT_AUXV: u32 = 6;
pub const NT_SIGINFO: u32 = 0x5349_4749;
pub const NT_FILE: u32 = 0x4649_4c45;
pub const NOTE_NAME: &[u8] = b"CORE";

/// Words of x86-64's `user_regs_struct` (`elf_gregset_t`), in its order: r15 r14 r13 r12
/// rbp rbx r11 r10 r9 r8 rax rcx rdx rsi rdi orig_rax rip cs eflags rsp ss fs_base gs_base
/// ds es fs gs.
pub const GREGS: usize = 27;
/// x86-64's `struct elf_prstatus`, `struct elf_prpsinfo` and the `fxsave` image
/// (`user_i387_struct`), in bytes.
pub const PRSTATUS_SIZE: usize = 336;
pub const PRPSINFO_SIZE: usize = 136;
pub const FPREGS_SIZE: usize = 512;
pub const SIGINFO_SIZE: usize = 128;

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// A time as `struct timeval` (seconds, microseconds) from nanoseconds.
fn timeval(b: &mut [u8], at: usize, ns: u64) {
    put64(b, at, ns / 1_000_000_000);
    put64(b, at + 8, ns % 1_000_000_000 / 1000);
}

/// A thread's `NT_PRSTATUS` (Linux's `struct elf_prstatus`, filled as `fill_prstatus`
/// does): the signal the process dumps for (every thread's, as Linux's), the thread's
/// pending and blocked signals, the ids, its CPU times (the whole process's for the main
/// thread) and its children's, and its registers.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prstatus {
    pub signo: u32,
    pub pending: u64,
    pub blocked: u64,
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    pub sid: u32,
    pub user_ns: u64,
    pub system_ns: u64,
    pub child_user_ns: u64,
    pub child_system_ns: u64,
    pub regs: [u64; GREGS],
}

impl Prstatus {
    pub fn to_bytes(&self) -> [u8; PRSTATUS_SIZE] {
        let mut b = [0u8; PRSTATUS_SIZE];
        // pr_info (si_signo, si_code, si_errno) and pr_cursig.
        put32(&mut b, 0, self.signo);
        put16(&mut b, 12, self.signo as u16);
        put64(&mut b, 16, self.pending);
        put64(&mut b, 24, self.blocked);
        put32(&mut b, 32, self.pid);
        put32(&mut b, 36, self.ppid);
        put32(&mut b, 40, self.pgrp);
        put32(&mut b, 44, self.sid);
        timeval(&mut b, 48, self.user_ns);
        timeval(&mut b, 64, self.system_ns);
        timeval(&mut b, 80, self.child_user_ns);
        timeval(&mut b, 96, self.child_system_ns);
        for (i, r) in self.regs.iter().enumerate() {
            put64(&mut b, 112 + i * 8, *r);
        }
        // pr_fpvalid: an NT_PRFPREG note follows.
        put32(&mut b, 328, 1);
        b
    }
}

/// `NT_PRPSINFO` (Linux's `struct elf_prpsinfo`, as `fill_psinfo` fills it for the
/// process's main thread).
#[derive(Clone, Copy, Debug)]
pub struct Prpsinfo {
    /// The state's index in "RSDTZW" (0: running).
    pub state: u8,
    pub nice: i8,
    /// Linux's task flags (`PF_*`).
    pub flags: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    pub sid: u32,
    /// The name (comm), NUL-padded.
    pub fname: [u8; 16],
    /// The command line's first 80 bytes, the NULs between arguments made spaces.
    pub psargs: [u8; 80],
}

impl Prpsinfo {
    /// `psargs` from a command line (NUL-terminated arguments), as Linux's `fill_psinfo`:
    /// its first 79 bytes with every NUL made a space (the last argument's terminator too),
    /// the rest zero.
    pub fn args(cmdline: &[u8]) -> [u8; 80] {
        let mut out = [0u8; 80];
        let n = cmdline.len().min(79);
        for (o, &b) in out.iter_mut().zip(&cmdline[..n]) {
            *o = if b == 0 { b' ' } else { b };
        }
        out
    }

    pub fn to_bytes(&self) -> [u8; PRPSINFO_SIZE] {
        let mut b = [0u8; PRPSINFO_SIZE];
        let state = self.state.min(6);
        b[0] = state;
        b[1] = if state > 5 { b'.' } else { b"RSDTZW"[state as usize] };
        b[2] = (b[1] == b'Z') as u8;
        b[3] = self.nice as u8;
        put64(&mut b, 8, self.flags);
        put32(&mut b, 16, self.uid);
        put32(&mut b, 20, self.gid);
        put32(&mut b, 24, self.pid);
        put32(&mut b, 28, self.ppid);
        put32(&mut b, 32, self.pgrp);
        put32(&mut b, 36, self.sid);
        b[40..56].copy_from_slice(&self.fname);
        b[56..136].copy_from_slice(&self.psargs);
        b
    }
}

/// A mapped file for `NT_FILE`: its range, the file offset of its start, the file's path.
pub struct MappedFile<'a> {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub path: &'a [u8],
}

/// `NT_FILE`'s description (Linux's `fill_files_note`): the count, the page size, a (start,
/// end, offset in pages) triple per file mapping, then their paths, NUL-terminated.
pub fn file_note(files: &[MappedFile]) -> Result<Vec<u8>, Error> {
    let names: usize = files.iter().map(|f| f.path.len() + 1).sum();
    let mut out = Vec::new();
    out.try_reserve_exact(16 + files.len() * 24 + names)?;
    out.extend_from_slice(&(files.len() as u64).to_le_bytes());
    out.extend_from_slice(&PAGE.to_le_bytes());
    for f in files {
        out.extend_from_slice(&f.start.to_le_bytes());
        out.extend_from_slice(&f.end.to_le_bytes());
        out.extend_from_slice(&(f.offset / PAGE).to_le_bytes());
    }
    for f in files {
        out.extend_from_slice(f.path);
        out.push(0);
    }
    Ok(out)
}

/// The notes of a core file, appended in the order they go in.
#[derive(Default)]
pub struct Notes {
    pub bytes: Vec<u8>,
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

impl Notes {
    /// The size note (`name`, `kind`, a `desc_len`-byte description) takes.
    pub fn size(name: &[u8], desc_len: usize) -> usize {
        12 + align4(name.len() + 1) + align4(desc_len)
    }

    /// Appends a note: its header (name size with the NUL, description size, type), the
    /// name and the description, each padded to 4 bytes.
    pub fn push(&mut self, name: &[u8], kind: u32, desc: &[u8]) -> Result<(), Error> {
        self.bytes.try_reserve(Self::size(name, desc.len()))?;
        self.bytes.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
        self.bytes.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        self.bytes.extend_from_slice(&kind.to_le_bytes());
        self.bytes.extend_from_slice(name);
        self.bytes.resize(self.bytes.len() + align4(name.len() + 1) - name.len(), 0);
        self.bytes.extend_from_slice(desc);
        self.bytes.resize(self.bytes.len() + align4(desc.len()) - desc.len(), 0);
        Ok(())
    }

    /// A "CORE" note.
    pub fn core(&mut self, kind: u32, desc: &[u8]) -> Result<(), Error> {
        self.push(NOTE_NAME, kind, desc)
    }
}

/// A mapping as it goes into the file: its range and access (`PF_*`) and how many of its
/// bytes are dumped (from its start; `filter`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub flags: u32,
    pub filesz: u64,
}

/// Where everything goes: the bytes before the notes (the file header and the program
/// headers), where the notes start, where each segment's data starts, and the section
/// header of extended numbering with its offset (after the data), if needed.
pub struct Layout {
    pub head: Vec<u8>,
    pub notes_offset: u64,
    pub data_offsets: Vec<u64>,
    pub extnum: Option<(u64, [u8; SHDR_SIZE])>,
    /// The end of the file (after the data and the extended numbering's header).
    pub end: u64,
}

/// The layout of a core file with `segments` and `notes_len` bytes of notes, as Linux's
/// `elf_core_dump` makes it.
pub fn layout(segments: &[Segment], notes_len: usize) -> Result<Layout, Error> {
    // The note's header and one per mapping.
    let segs = segments.len() + 1;
    let phnum = if segs >= PN_XNUM as usize { PN_XNUM } else { segs as u16 };
    let mut head = Vec::new();
    head.try_reserve_exact(ELF_HEADER_SIZE + segs * PHDR_SIZE)?;
    head.resize(ELF_HEADER_SIZE, 0);
    head[..4].copy_from_slice(b"\x7fELF");
    // ELFCLASS64, ELFDATA2LSB, EV_CURRENT, ELFOSABI_NONE.
    head[4] = 2;
    head[5] = 1;
    head[6] = 1;
    put16(&mut head, 16, ET_CORE);
    put16(&mut head, 18, EM_X86_64);
    put32(&mut head, 20, 1);
    put64(&mut head, 32, ELF_HEADER_SIZE as u64);
    put16(&mut head, 52, ELF_HEADER_SIZE as u16);
    put16(&mut head, 54, PHDR_SIZE as u16);
    put16(&mut head, 56, phnum);
    let notes_offset = (ELF_HEADER_SIZE + segs * PHDR_SIZE) as u64;
    let data = (notes_offset + notes_len as u64).div_ceil(PAGE) * PAGE;
    let phdr = |kind: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64, align: u64| {
        let mut p = [0u8; PHDR_SIZE];
        put32(&mut p, 0, kind);
        put32(&mut p, 4, flags);
        put64(&mut p, 8, offset);
        put64(&mut p, 16, vaddr);
        put64(&mut p, 32, filesz);
        put64(&mut p, 40, memsz);
        put64(&mut p, 48, align);
        p
    };
    head.extend_from_slice(&phdr(PT_NOTE, 0, notes_offset, 0, notes_len as u64, 0, 4));
    let mut data_offsets = Vec::new();
    data_offsets.try_reserve_exact(segments.len())?;
    let mut at = data;
    for s in segments {
        head.extend_from_slice(&phdr(PT_LOAD, s.flags, at, s.start, s.filesz, s.end - s.start, PAGE));
        data_offsets.push(at);
        at += s.filesz;
    }
    let extnum = (phnum == PN_XNUM).then(|| {
        // e_shoff, e_shentsize, e_shnum 1, e_shstrndx SHN_UNDEF; the header: SHT_NULL, size
        // e_shnum, link e_shstrndx, info the program headers' count.
        put64(&mut head, 40, at);
        put16(&mut head, 58, SHDR_SIZE as u16);
        put16(&mut head, 60, 1);
        let mut sh = [0u8; SHDR_SIZE];
        put64(&mut sh, 32, 1);
        put32(&mut sh, 44, segs as u32);
        (at, sh)
    });
    let end = at + if extnum.is_some() { SHDR_SIZE as u64 } else { 0 };
    Ok(Layout { head, notes_offset, data_offsets, extnum, end })
}

/// The `PF_*` flags of a mapping that allows reading, writing and executing as given.
pub fn segment_flags(read: bool, write: bool, exec: bool) -> u32 {
    (if read { PF_R } else { 0 }) | (if write { PF_W } else { 0 }) | (if exec { PF_X } else { 0 })
}
