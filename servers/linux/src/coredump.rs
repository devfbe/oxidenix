//! Core dumps (core(5), ADR 0022): a signal whose default action dumps core writes an ELF
//! core file of the process before it ends (Linux's do_coredump), and its wait status gets
//! the core flag (WCOREDUMP) if the file was written whole.
//!
//! The thread that takes the signal dumps (`dump`). It ends the process's other threads as
//! the signal would (a group exit), and each of them, on its way out, leaves its registers
//! (the program's, its FPU state, its FS base) in the dump's record and waits there until
//! the file is written (`park`, Linux's coredump_task_exit): no thread runs the program,
//! and none lets go of what the dump reads (its robust futex list's words, its tid word)
//! before the dump is over. The dumper then reads the process's memory from the kernel
//! (`SYS_VM_AREAS` for the mappings, `SYS_VM_RESIDENT` for which pages exist, `SYS_VM_ACCESS`
//! with `VM_ACCESS_FORCE` for their contents, ADR 0019), lays out the file with `elfcore`
//! and writes it through the server's own open of it (`paths::core_file`), with no
//! descriptor. A SIGKILL ends the dump (no core flag); so do the kernel's kills.
//!
//! The settings are the tree's own (each instance's, ADR 0002; Linux's are the machine's):
//! /proc/sys/kernel/core_pattern (`elfcore::pattern`; a pipe to a program is not run: no
//! dump, a line in the kernel's log) and core_uses_pid. RLIMIT_CORE bounds the bytes
//! written (a dump that would exceed it stops there, without the flag; below a page none is
//! tried), PR_SET_DUMPABLE 0 suppresses dumps, and /proc/<pid>/coredump_filter picks the
//! mappings (`elfcore::filter`).

use crate::heap::{self, TryGrow};
use crate::local;
use crate::process::{self, Pid, PROCS};
use crate::procfs::KernelFile;
use crate::signal::{self, SigInfo};
use crate::sync::Mutex;
use crate::syscall;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use elfcore::filter::{self, Kind};
use elfcore::pattern::{self, Facts, Target, PATTERN_MAX};
use elfcore::{MappedFile, Notes, Prpsinfo, Prstatus, Segment, GREGS, PAGE};
use restricted::*;

const EINTR: i64 = 4;
const EINVAL: i64 = 22;
const ENOMEM: i64 = heap::ENOMEM;
const EFBIG: i64 = 27;

/// The host name `%h` gives (uname's nodename).
const HOSTNAME: &[u8] = crate::system::NODENAME.as_bytes();

/// Linux's task flags in `NT_PRPSINFO`: the dumping thread's, and an exiting one's.
const PF_EXITING: u64 = 0x4;
const PF_DUMPCORE: u64 = 0x200;

// ------------------------------------------------------------------ settings

/// /proc/sys/kernel/core_pattern and core_uses_pid.
struct Settings {
    pattern: [u8; PATTERN_MAX],
    len: usize,
    uses_pid: i32,
}

static SETTINGS: Mutex<Settings> = Mutex::new(Settings { pattern: default_pattern(), len: 4, uses_pid: 0 });

/// Linux's default core_pattern, "core".
const fn default_pattern() -> [u8; PATTERN_MAX] {
    let mut p = [0u8; PATTERN_MAX];
    p[0] = b'c';
    p[1] = b'o';
    p[2] = b'r';
    p[3] = b'e';
    p
}

/// A setting's file contents.
pub fn setting(file: KernelFile) -> Result<Vec<u8>, i64> {
    let s = SETTINGS.lock();
    let text = match file {
        KernelFile::CorePattern => {
            let mut out = heap::try_vec(s.len + 1)?;
            out.extend_from_slice(&s.pattern[..s.len]);
            out.push(b'\n');
            return Ok(out);
        }
        KernelFile::CoreUsesPid => crate::try_format!("{}\n", s.uses_pid)?,
    };
    Ok(text.into_bytes())
}

/// A write of `data` to a setting (Linux's proc_dostring and proc_dointvec): the pattern up
/// to its first newline (at most 127 bytes), or a number. The bytes taken.
pub fn set_setting(file: KernelFile, data: &[u8]) -> Result<usize, i64> {
    match file {
        KernelFile::CorePattern => {
            let value = pattern::written(data);
            let mut s = SETTINGS.lock();
            s.pattern = [0; PATTERN_MAX];
            s.pattern[..value.len()].copy_from_slice(value);
            s.len = value.len();
        }
        KernelFile::CoreUsesPid => {
            let value = procproto::render::parse_int(data).and_then(|v| i32::try_from(v).ok()).ok_or(EINVAL)?;
            SETTINGS.lock().uses_pid = value;
        }
    }
    Ok(data.len())
}

// ------------------------------------------------------------------ the dump's record

/// A thread's state as the file shows it.
struct Regs {
    tid: Pid,
    words: [u64; GREGS],
    fpu: [u8; elfcore::FPREGS_SIZE],
}

/// The calling thread's state from its program's registers `s`, the call it is in
/// (`orig_rax`), its FPU registers (live in the CPU while the server runs) and FS base.
fn capture(s: &State, orig_rax: u64) -> Regs {
    let fs_base = syscall(SYS_THREAD_FS, [0, 0, 0, 0, 0, 0]) as u64;
    Regs { tid: local::tid(), words: crate::ptrace::user_regs_words(s, orig_rax, fs_base), fpu: signal::fxsave().0 }
}

/// A core dump under way (`process::Proc::core`).
pub struct Dump {
    /// The thread that dumps.
    dumper: Pid,
    /// 1 once the dump is over: the parked threads go on to their end.
    done: AtomicU32,
    /// A SIGKILL ended it.
    interrupted: AtomicBool,
    /// The other threads, as they parked: room for every thread the process had when the
    /// dump began (none is made after: a clone of an exiting process fails).
    parked: Mutex<Vec<Regs>>,
}

/// The calling thread ends while its process dumps core (it was killed for the dump, or
/// its delivery found the process ending): it leaves its registers (`s`, `orig_rax`) in
/// the dump's record and waits until the dump is over. The process's exit status then, or
/// None if no dump is under way (the dumper itself, or none at all).
pub fn park(s: &State, orig_rax: u64) -> Option<i32> {
    let (tid, pid) = process::me();
    let (dump, words) = {
        let t = PROCS.lock();
        let p = t.procs.get(&pid)?;
        let d = p.core.clone()?;
        if d.dumper == tid || d.done.load(Ordering::Acquire) != 0 {
            return None;
        }
        (d, p.words.clone())
    };
    let regs = capture(s, orig_rax);
    {
        let mut parked = dump.parked.lock();
        // (Within the room made for every thread: no allocation on the way out.)
        if parked.len() < parked.capacity() && !parked.iter().any(|r| r.tid == tid) {
            parked.push(regs);
        }
    }
    process::wake_word(&words.threads);
    // A wait the thread's death does not end (it was killed: Linux's coredump_task_exit
    // sleeps uninterruptibly too); only the dump's end, or a broken instance, does.
    while dump.done.load(Ordering::Acquire) == 0 {
        if syscall(SYS_SERVER_FUTEX_WAIT, [&dump.done as *const AtomicU32 as u64, 0, 0, FUTEX_LOCK, 0, 0]) == -EINTR {
            break;
        }
    }
    PROCS.lock().procs.get(&pid).and_then(|p| p.exiting)
}

/// A SIGKILL for process `pid` while it ends (lock held): a dump under way stops, and the
/// process ends by SIGKILL (Linux's dump_interrupted).
pub fn interrupt(t: &mut process::Table, pid: Pid) {
    let Some(p) = t.procs.get_mut(&pid) else { return };
    let Some(d) = p.core.clone().filter(|d| d.done.load(Ordering::Acquire) == 0) else { return };
    d.interrupted.store(true, Ordering::Release);
    p.exiting = Some(signal::SIGKILL as i32);
    t.kill_thread(d.dumper);
}

// ------------------------------------------------------------------ the dump

/// The calling thread dumps core for the fatal signal `info` (`s`, `orig_rax`: its
/// registers as the file shows them): the wait status the process ends with, the signal
/// with the core flag if the file was written.
pub fn dump(s: &State, orig_rax: u64, info: &SigInfo) -> i32 {
    let sig = info.signo();
    let (tid, pid) = process::me();
    let Some((dump, target, limit)) = begin(tid, pid, sig) else { return sig as i32 };
    let me = capture(s, orig_rax);
    let written = write(&dump, me, &target, limit, info).is_ok();
    // The parked threads go on to their end, with the core flag in the status if it was
    // written (and nothing ended the dump: a SIGKILL's status is SIGKILL's).
    let mut t = PROCS.lock();
    let mut status = sig as i32;
    if let Some(p) = t.procs.get_mut(&pid) {
        if written && !dump.interrupted.load(Ordering::Acquire) && p.exiting == Some(sig as i32) {
            status |= signal::WCOREFLAG;
            p.exiting = Some(status);
        }
        p.core = None;
    }
    drop(t);
    dump.done.store(1, Ordering::Release);
    syscall(SYS_SERVER_FUTEX_WAKE, [&dump.done as *const AtomicU32 as u64, u32::MAX as u64, 0, 0, 0, 0]);
    status
}

/// Whether the process dumps at all, and its start: (the record, where the dump goes, the
/// soft RLIMIT_CORE). The other threads are killed (a group exit with the signal), to park.
fn begin(tid: Pid, pid: Pid, sig: u32) -> Option<(Arc<Dump>, alloc::vec::Vec<u8>, u64)> {
    let (dumpable, limit, threads) = {
        let t = PROCS.lock();
        let p = t.procs.get(&pid)?;
        if p.exiting.is_some() {
            return None;
        }
        (p.dumpable, p.limits.soft(crate::ids::RLIMIT_CORE), p.threads.len())
    };
    if !dumpable {
        return None;
    }
    let target = match name(tid, pid, sig, limit) {
        Ok(Target::File(path)) => path,
        Ok(Target::Pipe(argv)) => {
            // Running a program as the dump's reader is not offered (docs/todo.md).
            let program = argv.first().map_or(&b""[..], |a| a.as_slice());
            if let Ok(msg) = crate::try_format!("core dump of pid {} to a pipe ({}) is not supported: none written", pid, core::str::from_utf8(program).unwrap_or("?")) {
                syscall(SYS_SERVER_LOG, [msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0, 0]);
            }
            return None;
        }
        Err(_) => return None,
    };
    if limit < elfcore::MIN_COREDUMP {
        return None;
    }
    // Room for every thread's registers, had before the lock; a thread made meanwhile asks
    // for more.
    let mut room = threads;
    loop {
        let parked = heap::try_vec::<Regs>(room).ok()?;
        let dump = heap::try_arc(Dump { dumper: tid, done: AtomicU32::new(0), interrupted: AtomicBool::new(false), parked: Mutex::new(parked) }).ok()?;
        let mut t = PROCS.lock();
        let p = t.procs.get_mut(&pid)?;
        if p.exiting.is_some() {
            return None;
        }
        if p.threads.len() > room {
            room = p.threads.len();
            continue;
        }
        p.core = Some(dump.clone());
        t.group_exit(pid, sig as i32, Some(tid));
        return Some((dump, target, limit));
    }
}

/// The file's name for the dump of `pid` by thread `tid` for `sig` (core_pattern,
/// core_uses_pid).
fn name(tid: Pid, pid: Pid, sig: u32, limit: u64) -> Result<Target, i64> {
    let (comm, exe) = {
        let t = PROCS.lock();
        let comm = t.threads.get(&tid).map_or([0; 16], |th| th.comm);
        let exe = t.procs.get(&pid).and_then(|p| p.exe.clone());
        (comm, exe)
    };
    let comm_len = comm.iter().position(|&b| b == 0).unwrap_or(16);
    let mut info = ThreadInfo::default();
    syscall(SYS_THREAD_INFO, [0, &mut info as *mut ThreadInfo as u64, 0, 0, 0, 0]);
    let facts = Facts {
        pid,
        tid,
        global_pid: pid,
        global_tid: tid,
        uid: 0,
        gid: 0,
        signal: sig,
        dumpable: 1,
        cpu: info.cpu as u32,
        limit,
        time: crate::time::realtime().sec,
        hostname: HOSTNAME,
        comm: &comm[..comm_len],
        exe: exe.as_ref().map(|o| o.path.as_bytes()),
    };
    let (pattern, len, uses_pid) = {
        let s = SETTINGS.lock();
        (s.pattern, s.len, s.uses_pid != 0)
    };
    heap::fallible(|| pattern::expand(&pattern[..len], uses_pid, &facts)).map_err(|_| ENOMEM)
}

/// A mapping as the dump sees it.
struct Area {
    area: VmArea,
    kind: Kind,
    /// [vvar]: dumped as Linux's (its pages are no memory of the process's: holes).
    holes: bool,
    filesz: u64,
}

/// Waits for the other threads, gathers the notes and the mappings, and writes the file.
fn write(dump: &Arc<Dump>, me: Regs, target: &[u8], limit: u64, info: &SigInfo) -> Result<(), i64> {
    let (tid, pid) = process::me();
    let (handle, words) = {
        let t = PROCS.lock();
        let p = t.procs.get(&pid).ok_or(EINVAL)?;
        (p.handle, p.words.clone())
    };
    // Every other thread parked or gone.
    loop {
        let seen = words.threads.load(Ordering::Acquire);
        let waiting = {
            let t = PROCS.lock();
            let parked = dump.parked.lock();
            t.procs.get(&pid).is_some_and(|p| p.threads.iter().any(|&x| x != tid && !parked.iter().any(|r| r.tid == x)))
        };
        if !waiting {
            break;
        }
        if aborted(dump, handle) || process::wait_word(&words.threads, seen, 0, false) == -EINTR {
            return Err(EINTR);
        }
    }
    let notes = notes(dump, me, pid, handle, info)?;
    let areas = areas(handle, pid)?;
    let segments = heap::fallible(|| {
        let mut v = Vec::new();
        v.try_reserve_exact(areas.len())?;
        v.extend(areas.iter().map(|a| Segment {
            start: a.area.start,
            end: a.area.end,
            flags: elfcore::segment_flags(a.area.flags & VMA_READ != 0, a.area.flags & VMA_WRITE != 0, a.area.flags & VMA_EXEC != 0),
            filesz: a.filesz,
        }));
        Ok::<_, alloc::collections::TryReserveError>(v)
    })
    .map_err(|_| ENOMEM)?;
    let layout = heap::fallible(|| elfcore::layout(&segments, notes.bytes.len())).map_err(|_| ENOMEM)?;
    drop(segments);
    let path = core::str::from_utf8(target).map_err(|_| EINVAL)?;
    let out = crate::paths::core_file(path)?;
    let mut w = Writer { out: &out, written: 0, limit, end: 0, handle, dump };
    w.emit(0, &layout.head)?;
    w.emit(layout.notes_offset, &notes.bytes)?;
    drop(notes);
    let mut buf = heap::try_vec::<u8>(VM_ACCESS_MAX as usize)?;
    buf.resize(VM_ACCESS_MAX as usize, 0);
    for (a, &at) in areas.iter().zip(&layout.data_offsets) {
        if aborted(dump, handle) {
            return Err(EINTR);
        }
        w.area(a, at, &mut buf)?;
    }
    if let Some((at, sh)) = layout.extnum {
        w.emit(at, &sh)?;
    }
    // The file is as long as its layout, also when it ends in a hole (Linux writes the last
    // byte then).
    if w.end < layout.end {
        w.emit(layout.end - 1, &[0])?;
    }
    Ok(())
}

/// Whether the dump is to stop: a SIGKILL, or the kernel killed the process (out of
/// memory, the monitor).
fn aborted(dump: &Dump, handle: u64) -> bool {
    if dump.interrupted.load(Ordering::Acquire) {
        return true;
    }
    let mut info = ProcInfo::default();
    syscall(SYS_PROC_INFO, [handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
    info.killed != 0
}

/// The open core file (`paths::core_file`), the server's own (no descriptor).
pub enum Output {
    Tmp(Arc<crate::tmpfile::TmpOpen>),
    Data(Arc<crate::datafile::DataOpen>),
}

impl Output {
    fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, i64> {
        match self {
            Output::Tmp(f) => f.write_at(off, data),
            Output::Data(f) => f.write_at(off, data),
        }
    }

    pub fn truncate(&self, size: u64) -> Result<(), i64> {
        match self {
            Output::Tmp(f) => f.truncate(size),
            Output::Data(f) => f.truncate(size),
        }
    }
}

/// The file's writes: what was written counts against RLIMIT_CORE (holes do not, as
/// Linux's `cprm->written`), and the file's end so far; the memory read from the
/// process `handle` names, for the dump `dump`.
struct Writer<'a> {
    out: &'a Output,
    written: u64,
    limit: u64,
    end: u64,
    handle: u64,
    dump: &'a Dump,
}

impl Writer<'_> {
    /// Writes `data` at `at` whole; EFBIG past the limit (nothing of it written).
    fn emit(&mut self, at: u64, data: &[u8]) -> Result<(), i64> {
        if data.is_empty() {
            return Ok(());
        }
        if self.written + data.len() as u64 > self.limit {
            return Err(EFBIG);
        }
        let mut done = 0;
        while done < data.len() {
            let n = self.out.write_at(at + done as u64, &data[done..])?;
            if n == 0 {
                return Err(EFBIG);
            }
            done += n;
        }
        self.written += data.len() as u64;
        self.end = self.end.max(at + data.len() as u64);
        Ok(())
    }

    /// The dumped part of `a` at file offset `at`: its pages that exist, read with
    /// `VM_ACCESS_FORCE`; a page no one touched in anonymous memory, or that cannot be
    /// read, is a hole (Linux's `FOLL_DUMP`).
    fn area(&mut self, a: &Area, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        if a.filesz == 0 || a.holes {
            return Ok(());
        }
        let (start, end) = (a.area.start, a.area.start + a.filesz);
        // Anonymous memory: only the pages it has (its own).
        if a.kind == Kind::AnonPrivate {
            let mut from = start;
            while from < end {
                let mut runs = [VmRun::default(); 32];
                let n = syscall(SYS_VM_RESIDENT, [self.handle, from, end, runs.as_mut_ptr() as u64, runs.len() as u64, 0]);
                if n <= 0 {
                    return if n < 0 { Err(-n) } else { Ok(()) };
                }
                for r in &runs[..n as usize] {
                    if r.flags & VM_RUN_PRIVATE != 0 {
                        self.copy(r.start, r.start + r.pages * PAGE, start, at, buf)?;
                    }
                    from = r.start + r.pages * PAGE;
                }
            }
            return Ok(());
        }
        self.copy(start, end, start, at, buf)
    }

    /// Copies [from, to) of the process's memory to the file (the area starts at `base`,
    /// at file offset `at`), a buffer at a time; what cannot be read stays a hole (unless
    /// the dump is to stop: a dying dumper's reads come back short).
    fn copy(&mut self, from: u64, to: u64, base: u64, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        let mut va = from;
        while va < to {
            if self.dump.interrupted.load(Ordering::Acquire) {
                return Err(EINTR);
            }
            let len = ((to - va) as usize).min(buf.len());
            let n = match crate::ptrace::vm_access(self.handle, va, &mut buf[..len], false, true) {
                Ok(n) => n,
                Err(_) => 0,
            };
            // (Whole pages: `from`, `to` and every chunk are page-aligned.)
            let n = n / PAGE as usize * PAGE as usize;
            self.emit(at + (va - base), &buf[..n])?;
            va += n as u64;
            if n < len {
                if aborted(self.dump, self.handle) {
                    return Err(EINTR);
                }
                // The page that failed is a hole.
                va += PAGE;
            }
        }
        Ok(())
    }
}

/// The process's mappings with what goes of each into the file (`elfcore::filter`).
fn areas(handle: u64, pid: Pid) -> Result<Vec<Area>, i64> {
    let filter = PROCS.lock().procs.get(&pid).map_or(filter::DEFAULT, |p| p.coredump_filter);
    let mut out: Vec<Area> = Vec::new();
    let mut from = 0;
    loop {
        let mut batch = [VmArea::default(); 32];
        let n = syscall(SYS_VM_AREAS, [handle, from, batch.as_mut_ptr() as u64, batch.len() as u64, 0, 0]);
        if n < 0 {
            return Err(-n);
        }
        if n == 0 {
            return Ok(out);
        }
        for a in &batch[..n as usize] {
            let special = crate::vdso::name_of(a.object);
            let kind = if a.flags & VMA_DEVICE != 0 {
                Kind::Device
            } else if special.is_some() {
                Kind::Vdso
            } else if a.flags & VMA_SHARED != 0 {
                if a.flags & VMA_ANON_SHARED != 0 { Kind::AnonShared } else { Kind::FileShared }
            } else if a.object == 0 {
                Kind::AnonPrivate
            } else {
                Kind::FilePrivate
            };
            let written = matches!(kind, Kind::AnonPrivate | Kind::FilePrivate) && has_own_pages(handle, a.start, a.end)?;
            let size = a.end - a.start;
            let filesz = match filter::dump(kind, filter, written, a.flags & VMA_READ != 0, a.offset == 0) {
                filter::Dump::Nothing => 0,
                filter::Dump::Whole => size,
                filter::Dump::ElfHeader => {
                    let mut magic = [0u8; 4];
                    match crate::ptrace::vm_access(handle, a.start, &mut magic, false, true) {
                        Ok(4) if &magic == b"\x7fELF" => PAGE.min(size),
                        _ => 0,
                    }
                }
            };
            out.try_push(Area { area: *a, kind, holes: special == Some("[vvar]"), filesz })?;
        }
        from = batch[n as usize - 1].end;
    }
}

/// Whether [start, end) has a page of its own (Linux's `anon_vma`: written to).
fn has_own_pages(handle: u64, start: u64, end: u64) -> Result<bool, i64> {
    let mut from = start;
    while from < end {
        let mut runs = [VmRun::default(); 32];
        let n = syscall(SYS_VM_RESIDENT, [handle, from, end, runs.as_mut_ptr() as u64, runs.len() as u64, 0]);
        if n < 0 {
            return Err(-n);
        }
        if n == 0 {
            return Ok(false);
        }
        let runs = &runs[..n as usize];
        if runs.iter().any(|r| r.flags & VM_RUN_PRIVATE != 0) {
            return Ok(true);
        }
        let last = runs[runs.len() - 1];
        from = last.start + last.pages * PAGE;
    }
    Ok(false)
}

/// What the notes say of a thread, from the process table.
struct ThreadFacts {
    tid: Pid,
    key: u64,
    pending: u64,
    blocked: u64,
}

/// The notes (Linux's order: the dumping thread's `NT_PRSTATUS`, then the process's
/// `NT_PRPSINFO`, `NT_SIGINFO`, `NT_AUXV`, `NT_FILE`, the dumping thread's `NT_PRFPREG`,
/// then each other thread's `NT_PRSTATUS` and `NT_PRFPREG`).
fn notes(dump: &Dump, me: Regs, pid: Pid, handle: u64, info: &SigInfo) -> Result<Notes, i64> {
    let sig = info.signo();
    // (Every other thread parked or ended: the record changes no more.)
    let parked = core::mem::take(&mut *dump.parked.lock());
    // What the table says, in one pass under the lock (the threads in the order the
    // process lists them, the dumper's first).
    let mut order: Vec<ThreadFacts> = heap::try_vec(parked.len() + 1)?;
    let mut psargs_src = [0u8; 80];
    let (ppid, pgid, sid, cmdline_len, exe_object, children, leader_state, leader_comm, exe) = {
        let t = PROCS.lock();
        let p = t.procs.get(&pid).ok_or(EINVAL)?;
        let facts = |tid: Pid| {
            t.threads.get(&tid).map(|th| ThreadFacts { tid, key: th.key, pending: th.sig.pending.set, blocked: th.sig.mask })
        };
        if let Some(f) = facts(me.tid) {
            order.push(f);
        }
        for &x in &p.threads {
            if x != me.tid && parked.iter().any(|r| r.tid == x) {
                if let Some(f) = facts(x) {
                    order.push(f);
                }
            }
        }
        // The main thread: running if it dumps, waiting for the dump if it parked, else
        // gone ("RSDTZW").
        let leader_state = if me.tid == pid {
            0
        } else if parked.iter().any(|r| r.tid == pid) {
            2
        } else {
            4
        };
        let leader_comm = t.threads.get(&pid).or_else(|| t.threads.get(&me.tid)).map_or([0; 16], |th| th.comm);
        let n = p.cmdline.len().min(80);
        psargs_src[..n].copy_from_slice(&p.cmdline[..n]);
        (p.ppid, p.pgid, p.sid, n, p.exe_object, p.children_usage, leader_state, leader_comm, p.exe.clone())
    };
    let mut proc_info = ProcInfo::default();
    syscall(SYS_PROC_INFO, [handle, &mut proc_info as *mut ProcInfo as u64, 0, 0, 0, 0]);
    let status = |f: &ThreadFacts, regs: &[u64; GREGS]| {
        // The main thread's times are the whole process's (Linux's thread_group_cputime).
        let (user_ns, system_ns) = if f.tid == pid {
            (proc_info.user_ns, proc_info.system_ns)
        } else {
            let mut ti = ThreadInfo::default();
            syscall(SYS_THREAD_INFO, [f.key, &mut ti as *mut ThreadInfo as u64, 0, 0, 0, 0]);
            (ti.user_ns, ti.system_ns)
        };
        Prstatus {
            signo: sig,
            pending: f.pending,
            blocked: f.blocked,
            pid: f.tid,
            ppid,
            pgrp: pgid,
            sid,
            user_ns,
            system_ns,
            child_user_ns: children.user_ns,
            child_system_ns: children.system_ns,
            regs: *regs,
        }
        .to_bytes()
    };
    // The main thread's nice value, if it is there.
    let nice = order.iter().find(|f| f.tid == pid && f.key != 0).map_or(0, |f| {
        let mut ti = ThreadInfo::default();
        syscall(SYS_THREAD_INFO, [f.key, &mut ti as *mut ThreadInfo as u64, 0, 0, 0, 0]);
        ti.nice.clamp(-20, 19) as i8
    });
    let psinfo = Prpsinfo {
        state: leader_state,
        nice,
        flags: if me.tid == pid { PF_DUMPCORE } else { PF_EXITING },
        uid: 0,
        gid: 0,
        pid,
        ppid,
        pgrp: pgid,
        sid,
        fname: leader_comm,
        psargs: Prpsinfo::args(&psargs_src[..cmdline_len]),
    }
    .to_bytes();
    let auxv = process::mm_of(pid).map(|mm| mm.lock().auxv).unwrap_or_default();
    let auxv_bytes = unsafe { core::slice::from_raw_parts(auxv.as_slice().as_ptr() as *const u8, auxv.len * 8) };
    let siginfo = unsafe { core::slice::from_raw_parts(info.0.as_ptr() as *const u8, elfcore::SIGINFO_SIZE) };
    let files = file_note(handle, exe_object, exe.as_deref().map(|o| o.path.as_bytes()))?;
    let mut notes = Notes::default();
    heap::fallible(|| -> Result<(), elfcore::Error> {
        let first = order.first();
        if let Some(f) = first {
            notes.core(elfcore::NT_PRSTATUS, &status(f, &me.words))?;
        }
        notes.core(elfcore::NT_PRPSINFO, &psinfo)?;
        notes.core(elfcore::NT_SIGINFO, siginfo)?;
        notes.core(elfcore::NT_AUXV, auxv_bytes)?;
        if let Some(files) = &files {
            notes.core(elfcore::NT_FILE, files)?;
        }
        if first.is_some() {
            notes.core(elfcore::NT_PRFPREG, &me.fpu)?;
        }
        for f in order.iter().skip(1) {
            let Some(r) = parked.iter().find(|r| r.tid == f.tid) else { continue };
            notes.core(elfcore::NT_PRSTATUS, &status(f, &r.words))?;
            notes.core(elfcore::NT_PRFPREG, &r.fpu)?;
        }
        Ok(())
    })
    .map_err(|_| ENOMEM)?;
    Ok(notes)
}

/// `NT_FILE`'s description: the mappings of files the server can name (the program's;
/// another mapped file has no name here yet, as in /proc/<pid>/maps), None if none.
fn file_note(handle: u64, exe_object: u64, exe: Option<&[u8]>) -> Result<Option<Vec<u8>>, i64> {
    let Some(path) = exe.filter(|_| exe_object != 0) else { return Ok(None) };
    let mut files: Vec<MappedFile> = Vec::new();
    let mut from = 0;
    loop {
        let mut batch = [VmArea::default(); 32];
        let n = syscall(SYS_VM_AREAS, [handle, from, batch.as_mut_ptr() as u64, batch.len() as u64, 0, 0]);
        if n < 0 {
            return Err(-n);
        }
        if n == 0 {
            break;
        }
        for a in &batch[..n as usize] {
            if a.object == exe_object {
                files.try_push(MappedFile { start: a.start, end: a.end, offset: a.offset, path })?;
            }
        }
        from = batch[n as usize - 1].end;
    }
    if files.is_empty() {
        return Ok(None);
    }
    heap::fallible(|| elfcore::file_note(&files)).map(Some).map_err(|_| ENOMEM)
}
