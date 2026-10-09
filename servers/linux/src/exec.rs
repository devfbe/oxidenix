//! execve and execveat (phase R8, ADR 0010): the ELF loader is the server's.
//!
//! Before the point of no return everything that can fail is done: the program is resolved
//! in the namespace and checked (a regular file with an execute bit, EACCES; not open for
//! writing, ETXTBSY: the hold it is run through keeps it so), `#!` lines are followed (four
//! deep, ELOOP), the ELF headers (and an interpreter's) are read through the file object and
//! checked (ENOEXEC), and the arguments and environment are copied (E2BIG). Then the
//! process's other threads end, the kernel swaps in a fresh address space
//! (`SYS_EXEC_SPACE`), and the server maps the segments, zeroes the tail of the last file
//! page, maps the bss and the stack (growing down), writes the strings, `AT_RANDOM`, the
//! platform name, the vectors and the auxiliary vector, and starts the program. A failure
//! after the point of no return kills the process with SIGSEGV, as on Linux.
//!
//! `ET_EXEC` programs go where they say; `ET_DYN` ones (PIE) at `DYN_BASE`; a `PT_INTERP`
//! interpreter where the kernel finds room for it. There is no vDSO.

use crate::local;
use crate::process::{self, PROCS};
use crate::signal;
use crate::syscall;
use crate::usercopy;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use restricted::*;

const PAGE: u64 = 4096;
const ENOENT: i64 = 2;
const EINTR: i64 = 4;
const E2BIG: i64 = 7;
const EAGAIN: i64 = 11;
const ENAMETOOLONG: i64 = 36;
const ENOEXEC: i64 = 8;
const ENOMEM: i64 = 12;
const EACCES: i64 = 13;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ELOOP: i64 = 40;

const SYS_EXECVE: u64 = 59;
const SYS_EXECVEAT: u64 = 322;
const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_EMPTY_PATH: u64 = 0x1000;

/// Where the stack ends (the word below is the end marker), as the kernel's loader had it.
const STACK_TOP: u64 = SHARED_BASE - PAGE;
/// The stack area at start, at least (it grows down on demand, up to 8 MiB).
const STACK_SIZE: u64 = 256 * 1024;
/// Where a position-independent program goes (Linux's ELF_ET_DYN_BASE: two thirds of the
/// address space).
const DYN_BASE: u64 = (SHARED_BASE / 3 * 2) & !(PAGE - 1);
/// Linux's limits: one string, and all of them with their pointers (a quarter of the 8 MiB
/// stack).
const MAX_ARG_STRLEN: usize = 32 * 4096;
const MAX_ARGS_TOTAL: usize = 2 * 1024 * 1024;
/// How deep `#!` interpreters may nest.
const MAX_SCRIPT_DEPTH: u32 = 4;
/// The most bytes of ELF and program headers read.
const MAX_HEADERS: u64 = 64 * 1024;

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;

/// The result of an exec call in `s`, or None for other calls. On success the registers
/// are the new program's (the "result" is its rax, 0).
pub fn handle(s: &mut State) -> Option<i64> {
    let result = match s.rax {
        SYS_EXECVE => execveat(s, AT_FDCWD as i64 as u64, s.rdi, s.rsi, s.rdx, 0),
        SYS_EXECVEAT => execveat(s, s.rdi, s.rsi, s.rdx, s.r10, s.r8),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// A program file, held (`mo_hold`) so that nobody writes it while it is run.
struct File {
    handle: u64,
}

impl Drop for File {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
    }
}

impl File {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let n = syscall(SYS_MO_READ, [self.handle, offset, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0]);
        if n < 0 { Err(-n) } else { Ok(n as usize) }
    }

    fn size(&self) -> Result<u64, i64> {
        let n = syscall(SYS_MO_FILE_SIZE, [self.handle, 0, 0, 0, 0, 0]);
        if n < 0 { Err(-n) } else { Ok(n as u64) }
    }
}

/// A program header.
#[derive(Clone, Copy)]
struct Segment {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

/// A checked ELF image: its type, entry, program header table and segments.
struct Elf {
    kind: u16,
    entry: u64,
    phoff: u64,
    phentsize: u16,
    segments: Vec<Segment>,
    interp: Option<String>,
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().expect("4 bytes"))
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().expect("8 bytes"))
}

impl Elf {
    /// Reads and checks the headers of `file` (whose first bytes are `head`).
    fn load(file: &File, head: &[u8]) -> Result<Elf, i64> {
        if head.len() < 64 || &head[0..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 || u16_at(head, 18) != 0x3e {
            return Err(ENOEXEC);
        }
        let kind = u16_at(head, 16);
        if kind != ET_EXEC && kind != ET_DYN {
            return Err(ENOEXEC);
        }
        let (entry, phoff, phentsize, phnum) = (u64_at(head, 24), u64_at(head, 32), u16_at(head, 54), u16_at(head, 56));
        if phentsize < 56 || phnum == 0 {
            return Err(ENOEXEC);
        }
        let table_end = phoff.checked_add(phentsize as u64 * phnum as u64).filter(|&e| e <= MAX_HEADERS).ok_or(ENOEXEC)?;
        let mut table = Vec::new();
        table.try_reserve_exact(table_end as usize).map_err(|_| ENOMEM)?;
        table.resize(table_end as usize, 0);
        if file.read(0, &mut table)? < table.len() {
            return Err(ENOEXEC);
        }
        let mut segments = Vec::new();
        let mut interp = None;
        for i in 0..phnum as usize {
            let o = phoff as usize + i * phentsize as usize;
            let seg = Segment {
                kind: u32_at(&table, o),
                flags: u32_at(&table, o + 4),
                offset: u64_at(&table, o + 8),
                vaddr: u64_at(&table, o + 16),
                filesz: u64_at(&table, o + 32),
                memsz: u64_at(&table, o + 40),
            };
            if seg.kind == PT_LOAD {
                // A page maps one file page: offsets and addresses agree in it.
                if seg.filesz > seg.memsz || seg.vaddr % PAGE != seg.offset % PAGE {
                    return Err(ENOEXEC);
                }
                seg.vaddr.checked_add(seg.memsz).filter(|&e| e <= SHARED_BASE).ok_or(ENOEXEC)?;
                seg.offset.checked_add(seg.filesz).ok_or(ENOEXEC)?;
            }
            if seg.kind == PT_INTERP {
                if seg.filesz == 0 || seg.filesz > 4096 || interp.is_some() {
                    return Err(ENOEXEC);
                }
                let mut name = vec![0u8; seg.filesz as usize];
                if file.read(seg.offset, &mut name)? < name.len() {
                    return Err(ENOEXEC);
                }
                // NUL-terminated.
                if name.pop() != Some(0) {
                    return Err(ENOEXEC);
                }
                interp = Some(String::from_utf8(name).map_err(|_| ENOEXEC)?);
            }
            segments.push(seg);
        }
        if !segments.iter().any(|s| s.kind == PT_LOAD) {
            return Err(ENOEXEC);
        }
        Ok(Elf { kind, entry, phoff, phentsize, segments, interp })
    }

    fn loads(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.kind == PT_LOAD)
    }

    /// The span of its loadable segments: (lowest page, end).
    fn span(&self) -> (u64, u64) {
        let low = self.loads().map(|s| s.vaddr & !(PAGE - 1)).min().unwrap_or(0);
        let high = self.loads().map(|s| page_up(s.vaddr + s.memsz)).max().unwrap_or(0);
        (low, high)
    }

    /// Where the program header table lies in memory (AT_PHDR), without the bias.
    fn phdr(&self) -> u64 {
        if let Some(p) = self.segments.iter().find(|s| s.kind == PT_PHDR) {
            return p.vaddr;
        }
        self.loads()
            .find(|s| s.offset <= self.phoff && self.phoff < s.offset + s.filesz)
            .map_or(0, |s| s.vaddr + (self.phoff - s.offset))
    }
}

fn page_up(x: u64) -> u64 {
    x.saturating_add(PAGE - 1) & !(PAGE - 1)
}

/// A program ready to run: its file and headers, its interpreter's, and what it runs with.
struct Prepared {
    file: File,
    elf: Elf,
    interp: Option<(File, Elf)>,
    args: Vec<Vec<u8>>,
    envs: Vec<Vec<u8>>,
    /// The path execve was given (AT_EXECFN, the name), and the program's absolute path
    /// (/proc's exe).
    filename: String,
    exe: String,
}

/// A NUL-terminated string of the program's, at most `max` bytes (E2BIG beyond).
fn read_string(addr: u64, max: usize) -> Result<Vec<u8>, i64> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let at = addr.checked_add(out.len() as u64).ok_or(EFAULT)?;
        let n = chunk.len().min(4096 - (at % 4096) as usize);
        usercopy::from_program(at, &mut chunk[..n])?;
        if let Some(end) = chunk[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&chunk[..end]);
            return Ok(out);
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() >= max {
            return Err(E2BIG);
        }
    }
}

/// A NULL-terminated array of strings of the program's (argv, envp); a null array is
/// empty. `total` counts the bytes of all of them (E2BIG beyond `MAX_ARGS_TOTAL`).
fn read_strings(addr: u64, total: &mut usize) -> Result<Vec<Vec<u8>>, i64> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    loop {
        let at = addr.checked_add(out.len() as u64 * 8).ok_or(EFAULT)?;
        let ptr: u64 = usercopy::read(at)?;
        if ptr == 0 {
            return Ok(out);
        }
        let s = read_string(ptr, MAX_ARG_STRLEN)?;
        *total += s.len() + 1 + 8;
        if *total > MAX_ARGS_TOTAL {
            return Err(E2BIG);
        }
        out.try_reserve(1).map_err(|_| ENOMEM)?;
        out.push(s);
    }
}

/// Opens the program `path` (relative to `base`) for running: its held file object and its
/// absolute path.
fn open(base: &str, path: &str, follow: bool) -> Result<(File, String), i64> {
    let (handle, abs) = crate::paths::exec_open(base, path, follow)?;
    Ok((File { handle }, abs))
}

/// execve(path, argv, envp) and execveat(dirfd, path, argv, envp, flags).
fn execveat(s: &mut State, dirfd: u64, path: u64, argv: u64, envp: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let filename = String::from_utf8(read_string(path, 4096).map_err(|e| if e == E2BIG { ENAMETOOLONG } else { e })?).map_err(|_| ENOEXEC)?;
    let mut total = 0;
    let args = read_strings(argv, &mut total)?;
    let envs = read_strings(envp, &mut total)?;
    let (file, exe) = if filename.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(ENOENT);
        }
        let (handle, abs) = crate::paths::exec_open_fd(dirfd)?;
        (File { handle }, abs)
    } else {
        let base = crate::paths::base_dir_of(dirfd, &filename)?;
        open(&base, &filename, flags & AT_SYMLINK_NOFOLLOW == 0)?
    };
    let shown = if filename.is_empty() { exe.clone() } else { filename };
    let prepared = prepare(file, exe, shown, args, envs, 0)?;
    run(s, prepared);
    Ok(0)
}

/// Reads the program's first bytes and follows a `#!` line (`depth`: how many were
/// followed), until an ELF image with its interpreter is ready.
fn prepare(file: File, exe: String, filename: String, mut args: Vec<Vec<u8>>, envs: Vec<Vec<u8>>, depth: u32) -> Result<Prepared, i64> {
    let size = file.size()?;
    let mut head = vec![0u8; size.min(256) as usize];
    let n = file.read(0, &mut head)?;
    head.truncate(n);
    if head.starts_with(b"#!") {
        if depth >= MAX_SCRIPT_DEPTH {
            return Err(ELOOP);
        }
        // binfmt_script: the interpreter and one optional argument, then the script's path,
        // then the arguments after the first.
        let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
        let line = &head[2..line_end];
        let line: Vec<u8> = line.iter().copied().map(|b| if b == b'\t' { b' ' } else { b }).collect();
        let trimmed = trim(&line);
        if trimmed.is_empty() {
            return Err(ENOEXEC);
        }
        let (interp, arg) = match trimmed.iter().position(|&b| b == b' ') {
            Some(at) => (trimmed[..at].to_vec(), Some(trim(&trimmed[at..]).to_vec())),
            None => (trimmed.to_vec(), None),
        };
        let mut new_args = Vec::new();
        new_args.push(interp.clone());
        if let Some(a) = arg.filter(|a| !a.is_empty()) {
            new_args.push(a);
        }
        new_args.push(filename.as_bytes().to_vec());
        if !args.is_empty() {
            args.remove(0);
        }
        new_args.append(&mut args);
        drop(file);
        let interp = String::from_utf8(interp).map_err(|_| ENOEXEC)?;
        let cwd = crate::records::current().state.lock().cwd.clone();
        let (ifile, iexe) = open(&cwd, &interp, true)?;
        // (AT_EXECFN and the name stay the script's, as Linux's bprm->filename.)
        return prepare(ifile, iexe, filename, new_args, envs, depth + 1);
    }
    let elf = Elf::load(&file, &head)?;
    let interp = match &elf.interp {
        Some(name) => {
            let cwd = crate::records::current().state.lock().cwd.clone();
            let (ifile, _) = open(&cwd, name, true)?;
            let size = ifile.size()?;
            let mut ihead = vec![0u8; size.min(256) as usize];
            let n = ifile.read(0, &mut ihead)?;
            ihead.truncate(n);
            let ielf = Elf::load(&ifile, &ihead)?;
            // An interpreter has none of its own.
            if ielf.interp.is_some() {
                return Err(ELOOP);
            }
            Some((ifile, ielf))
        }
        None => None,
    };
    Ok(Prepared { file, elf, interp, args, envs, filename, exe })
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| b != b' ').unwrap_or(s.len());
    let end = s.iter().rposition(|&b| b != b' ').map_or(start, |e| e + 1);
    &s[start..end]
}

/// The point of no return: the process's other threads end, the address space is replaced,
/// the program is mapped and started. Any failure kills the process (SIGSEGV).
fn run(s: &mut State, p: Prepared) {
    if de_thread().is_err() {
        // Killed meanwhile: the process ends anyway.
        process::die(signal::SIGKILL as i32);
    }
    let comm = process::comm_from(&p.filename);
    let name_len = comm.iter().position(|&b| b == 0).unwrap_or(15).min(15);
    let r = syscall(SYS_EXEC_SPACE, [p.file.handle, comm.as_ptr() as u64, name_len as u64, 0, 0, 0]);
    if r < 0 {
        process::die(signal::SIGSEGV as i32);
    }
    match load(&p) {
        Ok((entry, sp, brk)) => {
            finish(&p, comm, brk);
            *s = State { rip: entry, rsp: sp, rflags: 0x202, ..State::default() };
        }
        Err(_) => process::die(signal::SIGSEGV as i32),
    }
}

/// Ends the calling process's other threads and waits until they are gone; a thread that
/// is not the main one takes the process's id.
fn de_thread() -> Result<(), i64> {
    let (tid, pid) = process::me();
    let words = {
        let mut t = PROCS.lock();
        let p = t.procs.get_mut(&pid).ok_or(EINVAL)?;
        if p.exiting.is_some() || p.execing {
            return Err(EAGAIN);
        }
        p.execing = true;
        let words = p.words.clone();
        let others: Vec<process::Pid> = p.threads.iter().copied().filter(|&x| x != tid).collect();
        for other in others {
            t.kill_thread(other);
        }
        words
    };
    loop {
        let seen = words.threads.load(core::sync::atomic::Ordering::Acquire);
        {
            let mut t = PROCS.lock();
            let p = t.procs.get(&pid).ok_or(EINVAL)?;
            if p.exiting.is_some() {
                return Err(EINTR);
            }
            if p.threads.len() == 1 {
                if tid != pid {
                    // The old main thread is gone: this one takes its id.
                    let mut th = t.threads.remove(&tid).ok_or(EINVAL)?;
                    th.tid = pid;
                    let key = th.key;
                    t.threads.insert(pid, th);
                    t.keys.insert(key, pid);
                    let p = t.procs.get_mut(&pid).expect("checked");
                    p.threads = vec![pid];
                    let l = local::get();
                    l.tid.store(pid, core::sync::atomic::Ordering::Relaxed);
                }
                return Ok(());
            }
        }
        // Killed from outside: the wait ends, the process with it.
        if process::wait_word(&words.threads, seen, 0, false) == -EINTR {
            return Err(EINTR);
        }
    }
}

/// Maps the program (and its interpreter) and builds the stack: (entry, stack pointer,
/// program break).
fn load(p: &Prepared) -> Result<(u64, u64, u64), i64> {
    let bias = if p.elf.kind == ET_DYN { DYN_BASE - p.elf.span().0 } else { 0 };
    map_image(&p.file, &p.elf, bias)?;
    let brk = page_up(bias + p.elf.span().1);
    let (entry, interp_base) = match &p.interp {
        Some((file, elf)) => {
            let (low, high) = elf.span();
            // Room for it where the kernel finds some.
            let at = map(0, 0, high - low, 0, 0)?;
            let ibias = at - low;
            map_image(file, elf, ibias)?;
            (ibias + elf.entry, ibias)
        }
        None => (bias + p.elf.entry, 0),
    };
    let sp = build_stack(p, bias, interp_base, bias + p.elf.entry)?;
    Ok((entry, sp, brk))
}

/// `mo_map`.
fn map(handle: u64, addr: u64, len: u64, offset: u64, prot: u64) -> Result<u64, i64> {
    let flags = if addr != 0 { MO_FIXED } else { 0 };
    let r = syscall(SYS_MO_MAP, [handle, addr, len, offset, prot, flags]);
    if r < 0 { Err(-r) } else { Ok(r as u64) }
}

fn prot_of(flags: u32) -> u64 {
    let mut prot = 0;
    if flags & PF_R != 0 {
        prot |= 1;
    }
    if flags & PF_W != 0 {
        prot |= 2;
    }
    if flags & PF_X != 0 {
        prot |= 4;
    }
    prot
}

/// Maps the loadable segments of `elf` with `bias`, as Linux's elf_map and elf_load: the
/// file pages private, the rest of the last file page zeroed, anonymous pages for the bss.
fn map_image(file: &File, elf: &Elf, bias: u64) -> Result<(), i64> {
    for seg in elf.loads() {
        let prot = prot_of(seg.flags);
        let start = bias + (seg.vaddr & !(PAGE - 1));
        let data_end = bias + seg.vaddr + seg.filesz;
        let mem_end = bias + seg.vaddr + seg.memsz;
        if seg.filesz > 0 {
            let len = page_up(data_end) - start;
            map(file.handle, start, len, seg.offset & !(PAGE - 1), prot)?;
        }
        if seg.memsz > seg.filesz {
            if seg.filesz > 0 && data_end % PAGE != 0 {
                // The bss's part of the last file page.
                let stop = page_up(data_end).min(mem_end);
                let page = data_end & !(PAGE - 1);
                let writable = prot & 2 != 0;
                if !writable {
                    syscall(SYS_MO_PROTECT, [page, PAGE, prot | 2, 0, 0, 0]);
                }
                let zeros = [0u8; PAGE as usize];
                usercopy::to_program(data_end, &zeros[..(stop - data_end) as usize])?;
                if !writable {
                    syscall(SYS_MO_PROTECT, [page, PAGE, prot, 0, 0, 0]);
                }
            }
            let bss = if seg.filesz > 0 { page_up(data_end) } else { start };
            let end = page_up(mem_end);
            if end > bss {
                map(0, bss, end - bss, 0, prot)?;
            }
        }
    }
    Ok(())
}

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_PLATFORM: u64 = 15;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_HWCAP2: u64 = 26;
const AT_EXECFN: u64 = 31;
const AT_MINSIGSTKSZ: u64 = 51;

/// Linux's initial stack, from the top: the end marker, the file name (AT_EXECFN), the
/// environment's strings, the arguments' (back to back, in order: programs rely on it,
/// libuv's process title overwrites them), the platform name, 16 random bytes, then
/// argc, argv, envp and the auxiliary vector. Returns the stack pointer.
fn build_stack(p: &Prepared, bias: u64, interp_base: u64, entry: u64) -> Result<u64, i64> {
    let strings: usize = p.args.iter().chain(&p.envs).map(|s| s.len() + 1).sum::<usize>() + p.filename.len() + 1;
    let vectors = (p.args.len() + p.envs.len() + 3) * 8 + 24 * 16;
    let need = page_up((strings + vectors + 64 * 1024) as u64).max(STACK_SIZE);
    syscall_ok(SYS_MO_MAP, [0, STACK_TOP - need, need, 0, 3, MO_FIXED | MO_GROWSDOWN])?;
    let mut at = STACK_TOP - 8;
    let mut place = |bytes: &[u8]| -> Result<u64, i64> {
        at -= bytes.len() as u64 + 1;
        usercopy::to_program(at, bytes)?;
        usercopy::to_program(at + bytes.len() as u64, &[0])?;
        Ok(at)
    };
    let execfn = place(p.filename.as_bytes())?;
    // The environment's, then the arguments' (placed from the top down, in reverse, so that
    // they lie in order).
    let mut envp = Vec::new();
    for e in p.envs.iter().rev() {
        envp.push(place(e)?);
    }
    envp.reverse();
    let mut argv = Vec::new();
    for a in p.args.iter().rev() {
        argv.push(place(a)?);
    }
    argv.reverse();
    let mut sp = at & !15;
    sp -= 16;
    let platform = sp;
    usercopy::to_program(platform, b"x86_64\0")?;
    sp -= 16;
    let random = sp;
    let seed = unsafe { core::arch::x86_64::_rdtsc() };
    let mixed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (sp << 7);
    usercopy::write(random, &[seed.rotate_left(17) ^ mixed, mixed.rotate_left(29) ^ seed])?;
    let hwcap = core::arch::x86_64::__cpuid(1).edx as u64;
    let auxv = [
        (AT_MINSIGSTKSZ, 2048),
        (AT_HWCAP, hwcap),
        (AT_PAGESZ, PAGE),
        (AT_CLKTCK, 100),
        (AT_PHDR, bias + p.elf.phdr()),
        (AT_PHENT, p.elf.phentsize as u64),
        (AT_PHNUM, p.elf.segments.len() as u64),
        (AT_BASE, interp_base),
        (AT_FLAGS, 0),
        (AT_ENTRY, entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, random),
        (AT_HWCAP2, 0),
        (AT_EXECFN, execfn),
        (AT_PLATFORM, platform),
        (AT_NULL, 0),
    ];
    let mut words: Vec<u64> = Vec::new();
    words.push(argv.len() as u64);
    words.extend(&argv);
    words.push(0);
    words.extend(&envp);
    words.push(0);
    for (k, v) in auxv {
        words.extend([k, v]);
    }
    sp = (sp - words.len() as u64 * 8) & !15;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    usercopy::to_program(sp, &bytes)?;
    Ok(sp)
}

fn syscall_ok(nr: u64, a: [u64; 6]) -> Result<i64, i64> {
    let r = syscall(nr, a);
    if r < 0 { Err(-r) } else { Ok(r) }
}

/// After the program is in place: the process's records (name, command line, break, its
/// signal actions reset, the vfork parent let go).
fn finish(p: &Prepared, comm: [u8; 16], brk: u64) {
    let (tid, pid) = process::me();
    syscall(SYS_VM_FLOOR, [brk, 0, 0, 0, 0, 0]);
    let mut cmdline = Vec::new();
    for a in &p.args {
        if cmdline.len() + a.len() + 1 > 4096 {
            break;
        }
        cmdline.extend_from_slice(a);
        cmdline.push(0);
    }
    let mut t = PROCS.lock();
    if let Some(th) = t.threads.get_mut(&tid) {
        th.comm = comm;
        th.sig.alt = Default::default();
    }
    // Shared actions become the process's own; caught signals go back to their default
    // (ignored ones stay ignored).
    let hand = t.procs[&pid].sig.hand;
    let own = if t.hands[&hand].refs > 1 {
        let h = t.new_hand(Some(hand));
        t.put_hand(hand);
        h
    } else {
        hand
    };
    for a in t.hands.get_mut(&own).expect("listed").actions.iter_mut() {
        if a.handler != signal::SIG_IGN {
            *a = signal::SigAction::default();
        }
    }
    let words = {
        let proc = t.procs.get_mut(&pid).expect("listed");
        proc.sig.hand = own;
        proc.execing = false;
        proc.did_exec = true;
        proc.dumpable = true;
        proc.cmdline = cmdline;
        proc.exe = p.exe.clone();
        *proc.brk.lock() = process::Brk { start: brk, end: brk };
        // A vfork parent goes on: the child no longer uses its memory.
        core::mem::replace(&mut proc.vfork, false).then(|| proc.words.clone())
    };
    drop(t);
    if let Some(w) = words {
        w.vfork.store(1, core::sync::atomic::Ordering::Release);
        process::wake_word(&w.vfork);
    }
}

/// `ROLE_INIT`: runs the program the kernel started the tree with (`SYS_INIT_ARGS`); a
/// program that cannot run ends the process with status 127.
pub fn init(s: &mut State) {
    let mut buf = vec![0u8; 64 * 1024];
    let n = syscall(SYS_INIT_ARGS, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0]);
    buf.truncate(n.max(0) as usize);
    let mut parts = buf.split(|&b| b == 0);
    let path = String::from_utf8_lossy(parts.next().unwrap_or(b"")).into_owned();
    let mut args = Vec::new();
    for a in parts.by_ref() {
        if a.is_empty() {
            break;
        }
        args.push(a.to_vec());
    }
    let mut envs = Vec::new();
    for e in parts {
        if e.is_empty() {
            break;
        }
        envs.push(e.to_vec());
    }
    let result = open("/", &path, true).and_then(|(file, exe)| prepare(file, exe, path.clone(), args, envs, 0));
    match result {
        Ok(prepared) => run(s, prepared),
        Err(e) => {
            let mut msg = String::new();
            let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("cannot run {} (errno {})", path, e));
            syscall(SYS_SERVER_LOG, [msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0, 0]);
            process::exit(127 << 8, true);
        }
    }
}

/// EACCES: a program that is not a regular file or has no execute bit (everyone is root,
/// which needs one of them).
pub fn check_mode(mode: u32) -> Result<(), i64> {
    if mode & vfs::S_IFMT != vfs::S_IFREG || mode & 0o111 == 0 {
        return Err(EACCES);
    }
    Ok(())
}
