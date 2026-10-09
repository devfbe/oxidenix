//! Processes (phase R8, docs/design/linux-server.md "Processes and signals", ADR 0010): the
//! instance's pid namespace, the process tree, sessions and process groups, fork, vfork,
//! clone and clone3, exit and exit_group, wait4 and waitid, and the ids calls.
//!
//! The kernel keeps processes and threads as containers (an address space, a descriptor
//! table until R6e, threads it schedules); everything Linux knows about them is here, in one
//! table under one lock (`PROCS`, Linux's tasklist_lock and siglock in one). The lock is held
//! briefly and never across a copy to or from program memory: the service thread takes it
//! for the exits it learns (`EVENT_THREAD_EXIT`), and it must never wait for a page.
//!
//! A thread exits by `thread_exit` after its record is marked; the kernel reports its end to
//! the service thread once its references to the address space and the descriptor table are
//! gone (`thread_ended`). When a process's last thread is gone the process becomes a zombie
//! (`process_end`): its children go to a subreaper or pid 1, its parent hears of it. Pid 1's
//! end takes the whole instance with it, as a pid namespace's init's does on Linux.

use crate::local;
use crate::records::{self, FsContext};
use crate::signal::{self, SigInfo};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

pub type Pid = u32;

pub const EPERM: i64 = 1;
pub const ESRCH: i64 = 3;
pub const EINTR: i64 = 4;
pub const ECHILD: i64 = 10;
pub const EAGAIN: i64 = 11;
pub const EACCES: i64 = 13;
pub const EINVAL: i64 = 22;

/// Linux's default pid_max, and where allocation starts over after it (RESERVED_PIDS).
pub const PID_MAX: Pid = 32768;
const PID_WRAP: Pid = 300;

const SYS_GETPID: u64 = 39;
const SYS_CLONE: u64 = 56;
const SYS_FORK: u64 = 57;
const SYS_VFORK: u64 = 58;
const SYS_EXIT: u64 = 60;
const SYS_WAIT4: u64 = 61;
const SYS_SETPGID: u64 = 109;
const SYS_GETPPID: u64 = 110;
const SYS_GETPGRP: u64 = 111;
const SYS_SETSID: u64 = 112;
const SYS_GETPGID: u64 = 121;
const SYS_GETSID: u64 = 124;
const SYS_GETTID: u64 = 186;
const SYS_SET_TID_ADDRESS: u64 = 218;
const SYS_EXIT_GROUP: u64 = 231;
const SYS_WAITID: u64 = 247;
const SYS_CLONE3: u64 = 435;

const CSIGNAL: u64 = 0xff;
const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_PTRACE: u64 = 0x2000;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x10_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
const CLONE_DETACHED: u64 = 0x40_0000;
const CLONE_UNTRACED: u64 = 0x80_0000;
const CLONE_CHILD_SETTID: u64 = 0x100_0000;
const CLONE_IO: u64 = 0x8000_0000;
const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// What clone accepts: namespaces, pidfds and cgroups are EINVAL; SYSVSEM, PTRACE, UNTRACED,
/// DETACHED and IO change nothing here.
const CLONE_SUPPORTED: u64 = CSIGNAL
    | CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_PTRACE
    | CLONE_VFORK
    | CLONE_PARENT
    | CLONE_THREAD
    | CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_DETACHED
    | CLONE_UNTRACED
    | CLONE_CHILD_SETTID
    | CLONE_IO;

/// Futex words of a process, kept (by an `Arc`) by whoever waits on them, also after the
/// process is reaped.
#[derive(Default)]
pub struct Words {
    /// Advances when a child of the process changed (ended, stopped, continued): wait4 and
    /// waitid sleep on it.
    pub child: AtomicU32,
    /// Advances when one of its threads ended: an exec waits for the others.
    pub threads: AtomicU32,
    /// Advances when a group stop ends: stopped threads sleep on it.
    pub stop: AtomicU32,
    /// 1 once the process (a vfork child) exec'd or ended: its parent waits for it.
    pub vfork: AtomicU32,
}

/// CPU time in nanoseconds and the most pages resident.
#[derive(Clone, Copy, Default)]
pub struct Usage {
    pub user_ns: u64,
    pub system_ns: u64,
    pub peak_pages: u64,
}

/// What `wait` reports of a child that has not ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Report {
    Stopped(u32),
    Continued,
}

/// The program break (brk), shared by the processes sharing an address space.
#[derive(Default)]
pub struct Brk {
    pub start: u64,
    pub end: u64,
}

pub struct Proc {
    pub pid: Pid,
    /// The parent (0: none in the instance: pid 1, an orphan nobody adopted).
    pub ppid: Pid,
    pub pgid: Pid,
    pub sid: Pid,
    /// The kernel's handle on the process.
    pub handle: u64,
    /// Live threads, the main one first while it lives.
    pub threads: Vec<Pid>,
    pub children: Vec<Pid>,
    /// The wait status once every thread is gone (a zombie until reaped).
    pub zombie: Option<i32>,
    /// The status of a group exit under way (exit_group, a fatal signal, an exec's failure).
    pub exiting: Option<i32>,
    /// A thread execs: the others are being ended (another exec or a fork waits).
    pub execing: bool,
    /// It exec'd since its fork (setpgid on it by its parent is EACCES).
    pub did_exec: bool,
    /// The main thread's own exit status (exit, not exit_group), the process's unless a
    /// group exit sets another.
    pub main_status: Option<i32>,
    /// The signal its parent gets when it ends (SIGCHLD; 0: none).
    pub exit_signal: u32,
    /// PR_SET_PDEATHSIG, PR_SET_DUMPABLE, PR_SET_NO_NEW_PRIVS, PR_SET_CHILD_SUBREAPER.
    pub pdeath: u32,
    pub dumpable: bool,
    pub no_new_privs: bool,
    pub subreaper: bool,
    /// Its parent waits until it execs or ends (vfork).
    pub vfork: bool,
    /// The program's arguments (NUL-terminated, at most 4 KiB) and its path, for /proc.
    pub cmdline: Vec<u8>,
    pub exe: String,
    pub sig: signal::ProcSignals,
    /// A stop or continue not yet reported to `wait`.
    pub report: Option<Report>,
    /// What its reaped children used (with their reaped children).
    pub children_usage: Usage,
    pub itimer: crate::timer::ITimer,
    pub brk: Arc<Mutex<Brk>>,
    pub words: Arc<Words>,
}

pub struct Thread {
    pub tid: Pid,
    pub pid: Pid,
    /// The kernel's key (0 until known: a new thread sets it itself at its start too).
    pub key: u64,
    pub fs: Arc<FsContext>,
    /// Its name (comm), NUL-padded.
    pub comm: [u8; 16],
    pub sig: signal::ThreadSignals,
    /// It called exit, or was killed: no signal is meant for it any more.
    pub exited: bool,
}

pub struct Table {
    pub procs: BTreeMap<Pid, Proc>,
    pub threads: BTreeMap<Pid, Thread>,
    /// Threads by key.
    pub keys: BTreeMap<u64, Pid>,
    /// Signal actions by id, shared by `CLONE_SIGHAND`.
    pub hands: BTreeMap<u64, signal::Hand>,
    next_pid: Pid,
    next_hand: u64,
    /// Real-time signals queued in the instance.
    pub rt_queued: usize,
}

pub static PROCS: Mutex<Table> = Mutex::new(Table {
    procs: BTreeMap::new(),
    threads: BTreeMap::new(),
    keys: BTreeMap::new(),
    hands: BTreeMap::new(),
    next_pid: 1,
    next_hand: 1,
    rt_queued: 0,
});

/// A child's start: what its server needs before the program runs (`thread_create`'s
/// cookie points at it; the child takes it).
pub struct Birth {
    pub tid: Pid,
    pub pid: Pid,
    /// CLONE_CHILD_SETTID's word in a new address space: written by the child itself.
    pub settid: u64,
    pub fs: *const FsContext,
}

impl Table {
    /// A free pid: not a live or zombie process's, not a thread's, not a process group's or
    /// session's. None when all are taken.
    fn alloc_pid(&mut self) -> Option<Pid> {
        let used = |t: &Table, p: Pid| {
            t.procs.contains_key(&p) || t.threads.contains_key(&p) || t.procs.values().any(|q| q.pgid == p || q.sid == p)
        };
        let mut candidate = self.next_pid;
        for _ in 0..PID_MAX {
            if candidate >= PID_MAX {
                candidate = PID_WRAP;
            }
            if !used(self, candidate) {
                self.next_pid = candidate + 1;
                return Some(candidate);
            }
            candidate += 1;
        }
        None
    }

    /// A new set of actions (a copy of `from`'s, or the defaults), with one reference.
    pub fn new_hand(&mut self, from: Option<u64>) -> u64 {
        let actions = from.and_then(|h| self.hands.get(&h)).map(|h| h.actions).unwrap_or([signal::SigAction::default(); 64]);
        let id = self.next_hand;
        self.next_hand += 1;
        self.hands.insert(id, signal::Hand { refs: 1, actions });
        id
    }

    pub fn put_hand(&mut self, id: u64) {
        if let Some(h) = self.hands.get_mut(&id) {
            h.refs -= 1;
            if h.refs == 0 {
                self.hands.remove(&id);
            }
        }
    }

    /// The actions of process `pid`.
    pub fn actions(&self, pid: Pid) -> &[signal::SigAction; 64] {
        &self.hands[&self.procs[&pid].sig.hand].actions
    }

    pub fn actions_mut(&mut self, pid: Pid) -> &mut [signal::SigAction; 64] {
        let hand = self.procs[&pid].sig.hand;
        &mut self.hands.get_mut(&hand).expect("a process's actions").actions
    }

    /// POSIX's orphaned process group (Linux's `will_become_orphaned_pgrp`): no live member
    /// (but `ignore`) has a parent in another group of the same session.
    pub fn pgrp_orphaned(&self, pgid: Pid, ignore: Option<Pid>) -> bool {
        for p in self.procs.values() {
            if p.pgid != pgid || p.zombie.is_some() || Some(p.pid) == ignore {
                continue;
            }
            if let Some(parent) = self.procs.get(&p.ppid) {
                if parent.zombie.is_none() && parent.pgid != pgid && parent.sid == p.sid {
                    return false;
                }
            }
        }
        true
    }

    /// Whether a live member of group `pgid` is stopped or stopping (Linux's
    /// `has_stopped_jobs`).
    pub fn pgrp_stopped(&self, pgid: Pid) -> bool {
        self.procs.values().any(|p| p.pgid == pgid && p.zombie.is_none() && p.sig.stop.sig != 0)
    }

    /// Kicks thread `tid` (a signal or a stop for it), unless it is not known to the kernel
    /// yet (it looks at its signals before it first runs) or gone.
    pub fn kick(&self, tid: Pid) {
        if let Some(t) = self.threads.get(&tid) {
            if t.key != 0 && !t.exited {
                syscall(SYS_THREAD_KICK, [t.key, 0, 0, 0, 0, 0]);
            }
        }
    }

    /// Kills thread `tid`: it ends at its next return to its program (every wait of its ends
    /// first).
    pub fn kill_thread(&mut self, tid: Pid) {
        if let Some(t) = self.threads.get_mut(&tid) {
            t.exited = true;
            if t.key != 0 {
                syscall(SYS_THREAD_KILL, [t.key, 0, 0, 0, 0, 0]);
            }
        }
    }

    /// Starts the group exit of process `pid` with `status` (unless one is under way): every
    /// thread but `except` is killed.
    pub fn group_exit(&mut self, pid: Pid, status: i32, except: Option<Pid>) {
        let Some(p) = self.procs.get_mut(&pid) else { return };
        if p.exiting.is_some() || p.zombie.is_some() {
            return;
        }
        p.exiting = Some(status);
        let threads = p.threads.clone();
        for tid in threads {
            if Some(tid) != except {
                self.kill_thread(tid);
            }
        }
    }

    /// The process a parent's children go to when it ends: the nearest live child
    /// subreaper among its ancestors, else pid 1 while it lives, else none.
    fn reaper_for(&self, dying: Pid) -> Pid {
        let mut at = self.procs.get(&dying).map_or(0, |p| p.ppid);
        while let Some(p) = self.procs.get(&at) {
            if p.subreaper && p.zombie.is_none() && p.exiting.is_none() {
                return at;
            }
            at = p.ppid;
        }
        match self.procs.get(&1) {
            Some(init) if init.zombie.is_none() && dying != 1 => 1,
            _ => 0,
        }
    }
}

/// A futex wait on a word of the server's memory; interruptible ones end with EINTR when the
/// thread is kicked or dies, the others only when it dies.
pub fn wait_word(word: &AtomicU32, seen: u32, deadline: u64, interruptible: bool) -> i64 {
    let flags = if interruptible { FUTEX_INTERRUPTIBLE } else { 0 };
    syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, seen as u64, deadline, flags, 0, 0])
}

pub fn wake_word(word: &AtomicU32) {
    word.fetch_add(1, Ordering::Release);
    syscall(SYS_SERVER_FUTEX_WAKE, [word as *const AtomicU32 as u64, u32::MAX as u64, 0, 0, 0, 0]);
}

/// The calling thread's ids: (tid, pid).
pub fn me() -> (Pid, Pid) {
    (local::tid(), local::pid())
}

/// The name (comm) of `path`'s last component, as exec gives it.
pub fn comm_from(path: &str) -> [u8; 16] {
    let base = path.rsplit('/').next().unwrap_or(path).as_bytes();
    let mut comm = [0u8; 16];
    let n = base.len().min(15);
    comm[..n].copy_from_slice(&base[..n]);
    comm
}

fn comm_len(comm: &[u8; 16]) -> usize {
    comm.iter().position(|&b| b == 0).unwrap_or(16)
}

/// Gives the kernel the thread's name (its monitor and messages).
pub fn name_kernel_thread(key: u64, comm: &[u8; 16]) {
    syscall(SYS_THREAD_NAME, [key, comm.as_ptr() as u64, comm_len(comm) as u64, 0, 0, 0]);
}

// ------------------------------------------------------------------ starting threads

/// `ROLE_INIT`: the tree's first thread registers pid 1, a session and process group leader,
/// with a record at the root. Its program is run by `exec::init`.
pub fn register_init(key: u64) {
    let handle = syscall(SYS_PROC_SELF, [0; 6]);
    let fs = records::root();
    let fs_ptr = Arc::as_ptr(&fs);
    {
        let mut t = PROCS.lock();
        let pid = t.alloc_pid().unwrap_or(1);
        let hand = t.new_hand(None);
        let p = Proc {
            pid,
            ppid: 0,
            pgid: pid,
            sid: pid,
            handle: handle.max(0) as u64,
            threads: alloc::vec![pid],
            children: Vec::new(),
            zombie: None,
            exiting: None,
            execing: false,
            did_exec: false,
            main_status: None,
            exit_signal: signal::SIGCHLD,
            pdeath: 0,
            dumpable: true,
            no_new_privs: false,
            subreaper: false,
            vfork: false,
            cmdline: Vec::new(),
            exe: String::new(),
            sig: signal::ProcSignals::new(hand),
            report: None,
            children_usage: Usage::default(),
            itimer: Default::default(),
            brk: Arc::new(Mutex::new(Brk::default())),
            words: Arc::new(Words::default()),
        };
        t.procs.insert(pid, p);
        t.threads.insert(pid, Thread { tid: pid, pid, key, fs, comm: comm_from("init"), sig: signal::ThreadSignals::default(), exited: false });
        t.keys.insert(key, pid);
        local::set(pid, pid, key, fs_ptr);
    }
}

/// A thread made by `clone` starts: its record is there already (its creator made it); it
/// learns its ids from its birth record, writes CLONE_CHILD_SETTID in its own address space
/// and registers its key (a signal posted meanwhile is seen at its first delivery check).
pub fn start_thread(cookie: u64, key: u64) {
    let birth = unsafe { alloc::boxed::Box::from_raw(cookie as *mut Birth) };
    local::set(birth.tid, birth.pid, key, birth.fs);
    if birth.settid != 0 {
        // As Linux's schedule_tail: a fault here is ignored.
        let _ = usercopy::write(birth.settid, &birth.tid);
    }
    let mut t = PROCS.lock();
    if let Some(th) = t.threads.get_mut(&birth.tid) {
        th.key = key;
        t.keys.insert(key, birth.tid);
    }
}

// ------------------------------------------------------------------ system calls

/// The result of a process call in `s`, or None for other calls.
pub fn handle(s: &mut State) -> Option<i64> {
    let (a0, a1, a2, a3, a4) = (s.rdi, s.rsi, s.rdx, s.r10, s.r8);
    let result = match s.rax {
        SYS_GETPID => Ok(local::pid() as i64),
        SYS_GETTID => Ok(local::tid() as i64),
        SYS_GETPPID => Ok(getppid()),
        SYS_FORK => clone(s, Clone { exit_signal: signal::SIGCHLD as u64, legacy: true, ..Clone::default() }),
        SYS_VFORK => clone(s, Clone { flags: CLONE_VM | CLONE_VFORK, exit_signal: signal::SIGCHLD as u64, legacy: true, ..Clone::default() }),
        SYS_CLONE => {
            // x86-64's order: flags, stack, parent_tid, child_tid, tls.
            let flags = a0 & 0xffff_ffff;
            clone(s, Clone { flags: flags & !CSIGNAL, exit_signal: flags & CSIGNAL, stack: a1, ptid: a2, ctid: a3, tls: a4, legacy: true })
        }
        SYS_CLONE3 => clone3(s, a0, a1),
        SYS_EXIT => exit(((a0 & 0xff) << 8) as i32, false),
        SYS_EXIT_GROUP => exit(((a0 & 0xff) << 8) as i32, true),
        SYS_WAIT4 => wait4(a0 as i32, a1, a2, a3),
        SYS_WAITID => waitid(a0, a1 as u32, a2, a3, a4),
        SYS_SETPGID => setpgid(a0 as i32, a1 as i32),
        SYS_GETPGRP => getpgid(0),
        SYS_GETPGID => getpgid(a0 as i32),
        SYS_GETSID => getsid(a0 as i32),
        SYS_SETSID => setsid(),
        SYS_SET_TID_ADDRESS => {
            syscall(SYS_THREAD_CLEARTID, [a0, 0, 0, 0, 0, 0]);
            Ok(local::tid() as i64)
        }
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

fn getppid() -> i64 {
    let t = PROCS.lock();
    t.procs.get(&local::pid()).map_or(0, |p| p.ppid as i64)
}

/// What a clone asks for (clone, fork, vfork, clone3).
#[derive(Default)]
struct Clone {
    flags: u64,
    exit_signal: u64,
    stack: u64,
    ptid: u64,
    ctid: u64,
    tls: u64,
    /// clone(2) rather than clone3 (whose stack is a base and a size, checked stricter).
    legacy: bool,
}

/// clone3(args, size): `struct clone_args` (at least its first 64 bytes, CLONE_ARGS_SIZE_VER0;
/// bytes beyond what is known must be zero, E2BIG).
fn clone3(s: &State, args: u64, size: u64) -> Result<i64, i64> {
    const E2BIG: i64 = 7;
    if size < 64 || size > 4096 {
        return Err(EINVAL);
    }
    let mut raw = [0u64; 11];
    let known = (size as usize).min(88);
    let bytes = unsafe { core::slice::from_raw_parts_mut(raw.as_mut_ptr() as *mut u8, 88) };
    usercopy::from_program(args, &mut bytes[..known])?;
    if size as usize > 88 {
        let mut rest = alloc::vec![0u8; size as usize - 88];
        usercopy::from_program(args + 88, &mut rest)?;
        if rest.iter().any(|&b| b != 0) {
            return Err(E2BIG);
        }
    }
    let [flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup] = raw;
    let _ = (pidfd, cgroup);
    if set_tid != 0 || set_tid_size != 0 || flags & (CLONE_PIDFD | CLONE_INTO_CGROUP) != 0 {
        return Err(EINVAL);
    }
    if exit_signal & !CSIGNAL != 0 || flags & CSIGNAL != 0 {
        return Err(EINVAL);
    }
    if flags & (CLONE_THREAD | CLONE_PARENT) != 0 && exit_signal != 0 {
        return Err(EINVAL);
    }
    if (stack == 0) != (stack_size == 0) {
        return Err(EINVAL);
    }
    let top = if stack == 0 { 0 } else { stack.checked_add(stack_size).ok_or(EINVAL)? };
    clone(s, Clone { flags, exit_signal, stack: top, ptid: parent_tid, ctid: child_tid, tls, legacy: false })
}

/// clone and its relatives: a new process (fork) or thread (CLONE_THREAD).
fn clone(s: &State, c: Clone) -> Result<i64, i64> {
    let flags = c.flags;
    let supported = CLONE_SUPPORTED | if c.legacy { 0 } else { CLONE_CLEAR_SIGHAND };
    if flags & !supported & !CSIGNAL != 0 {
        return Err(EINVAL);
    }
    // Linux's rules: threads share handlers, shared handlers need shared memory.
    if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0 {
        return Err(EINVAL);
    }
    if flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0 {
        return Err(EINVAL);
    }
    if flags & CLONE_CLEAR_SIGHAND != 0 && flags & CLONE_SIGHAND != 0 {
        return Err(EINVAL);
    }
    if c.exit_signal > signal::NSIG as u64 {
        return Err(EINVAL);
    }
    if c.stack >= SHARED_BASE {
        return Err(EINVAL);
    }
    let thread = flags & CLONE_THREAD != 0;
    let (my_tid, my_pid) = me();
    // The new ids and records, before the kernel's thread exists.
    let (tid, settid_in_child) = {
        let mut t = PROCS.lock();
        let p = t.procs.get(&my_pid).ok_or(ESRCH)?;
        // Nothing new comes out of a process that is ending or exec'ing.
        if p.exiting.is_some() || p.execing {
            return Err(EAGAIN);
        }
        // The instance's init cannot make a sibling (Linux: CLONE_PARENT of a namespace's
        // init is EINVAL).
        if flags & CLONE_PARENT != 0 && my_pid == 1 {
            return Err(EINVAL);
        }
        let tid = t.alloc_pid().ok_or(EAGAIN)?;
        (tid, !thread && flags & CLONE_VM == 0 && flags & CLONE_CHILD_SETTID != 0)
    };
    // CLONE_PARENT_SETTID before the child runs (pthread_create relies on it), and
    // CLONE_CHILD_SETTID too where the child's memory is the caller's.
    if flags & CLONE_PARENT_SETTID != 0 {
        usercopy::write(c.ptid, &tid)?;
    }
    if flags & CLONE_CHILD_SETTID != 0 && flags & CLONE_VM != 0 {
        usercopy::write(c.ctid, &tid)?;
    }
    // The kernel's process for a new one.
    let handle = if thread {
        0
    } else {
        let mut pflags = if flags & CLONE_VM != 0 { PROC_SHARE_VM } else { PROC_FORK };
        if flags & CLONE_FILES != 0 {
            pflags |= PROC_SHARE_FILES;
        }
        let h = syscall(SYS_PROC_CREATE, [pflags, 0, 0, 0, 0, 0]);
        if h < 0 {
            return Err(-h);
        }
        h as u64
    };
    let pid = if thread { my_pid } else { tid };
    let fs = {
        let mine = records::current();
        if flags & CLONE_FS != 0 { mine } else { records::copy(&mine) }
    };
    let fs_ptr = Arc::as_ptr(&fs);
    // The records.
    {
        let mut t = PROCS.lock();
        let Some(me_thread) = t.threads.get(&my_tid) else {
            drop(t);
            close_handle(handle);
            return Err(ESRCH);
        };
        let thread_sig = me_thread.sig.for_child(flags & CLONE_VM != 0 && flags & CLONE_VFORK == 0);
        let comm = me_thread.comm;
        if !thread {
            let parent_hand = t.procs[&my_pid].sig.hand;
            let hand = if flags & CLONE_SIGHAND != 0 {
                t.hands.get_mut(&parent_hand).expect("the caller's actions").refs += 1;
                parent_hand
            } else {
                let h = t.new_hand(Some(parent_hand));
                if flags & CLONE_CLEAR_SIGHAND != 0 {
                    // Caught signals go back to their default; ignored ones stay.
                    for a in t.hands.get_mut(&h).expect("just made").actions.iter_mut() {
                        if a.handler != signal::SIG_IGN {
                            *a = signal::SigAction::default();
                        }
                    }
                }
                h
            };
            let parent = &t.procs[&my_pid];
            // With CLONE_PARENT the caller's parent is the parent, and gets the caller's exit
            // signal, as on Linux.
            let (ppid, exit_signal) =
                if flags & CLONE_PARENT != 0 { (parent.ppid, parent.exit_signal) } else { (my_pid, c.exit_signal as u32) };
            let brk = if flags & CLONE_VM != 0 {
                parent.brk.clone()
            } else {
                let b = parent.brk.lock();
                Arc::new(Mutex::new(Brk { start: b.start, end: b.end }))
            };
            let p = Proc {
                pid,
                ppid,
                pgid: parent.pgid,
                sid: parent.sid,
                handle,
                threads: alloc::vec![tid],
                children: Vec::new(),
                zombie: None,
                exiting: None,
                execing: false,
                did_exec: false,
                main_status: None,
                exit_signal,
                // The parent-death signal is not inherited, as on Linux.
                pdeath: 0,
                dumpable: parent.dumpable,
                no_new_privs: parent.no_new_privs,
                subreaper: false,
                vfork: flags & CLONE_VFORK != 0,
                cmdline: parent.cmdline.clone(),
                exe: parent.exe.clone(),
                sig: signal::ProcSignals::new(hand),
                report: None,
                children_usage: Usage::default(),
                itimer: Default::default(),
                brk,
                words: Arc::new(Words::default()),
            };
            t.procs.insert(pid, p);
            if let Some(pp) = t.procs.get_mut(&ppid) {
                pp.children.push(pid);
            }
        } else {
            t.procs.get_mut(&pid).expect("checked above").threads.push(tid);
        }
        t.threads.insert(tid, Thread { tid, pid, key: 0, fs, comm, sig: thread_sig, exited: false });
    }
    // The child's registers: the caller's, returning 0, on the new stack.
    let mut child = *s;
    child.rax = 0;
    if c.stack != 0 {
        child.rsp = c.stack;
    }
    let birth = alloc::boxed::Box::new(Birth { tid, pid, settid: if settid_in_child { c.ctid } else { 0 }, fs: fs_ptr });
    let cookie = alloc::boxed::Box::into_raw(birth) as u64;
    let tflags = if flags & CLONE_SETTLS != 0 { THREAD_SETTLS } else { 0 };
    let ctid = if flags & CLONE_CHILD_CLEARTID != 0 { c.ctid } else { 0 };
    let key = syscall(SYS_THREAD_CREATE, [handle, &child as *const State as u64, tflags, c.tls, ctid, cookie]);
    if key < 0 {
        drop(unsafe { alloc::boxed::Box::from_raw(cookie as *mut Birth) });
        let mut t = PROCS.lock();
        undo_clone(&mut t, tid, pid, thread);
        drop(t);
        close_handle(handle);
        return Err(-key);
    }
    let vfork_words = {
        let mut t = PROCS.lock();
        if let Some(th) = t.threads.get_mut(&tid) {
            th.key = key as u64;
            t.keys.insert(key as u64, tid);
        }
        name_kernel_thread(key as u64, &t.threads.get(&tid).map_or([0; 16], |th| th.comm));
        (flags & CLONE_VFORK != 0).then(|| t.procs.get(&pid).map(|p| p.words.clone())).flatten()
    };
    if let Some(words) = vfork_words {
        // Until the child execs or ends; only a kill ends the wait (the child runs on the
        // caller's stack, which the caller must not touch meanwhile).
        while words.vfork.load(Ordering::Acquire) == 0 {
            // (An uninterruptible wait ends early only for a thread that dies.)
            if wait_word(&words.vfork, 0, 0, false) == -EINTR {
                break;
            }
        }
    }
    Ok(tid as i64)
}

/// Takes back the records of a clone whose thread the kernel could not make.
fn undo_clone(t: &mut Table, tid: Pid, pid: Pid, thread: bool) {
    t.threads.remove(&tid);
    if thread {
        if let Some(p) = t.procs.get_mut(&pid) {
            p.threads.retain(|&x| x != tid);
        }
        return;
    }
    if let Some(p) = t.procs.remove(&pid) {
        t.put_hand(p.sig.hand);
        if let Some(pp) = t.procs.get_mut(&p.ppid) {
            pp.children.retain(|&c| c != pid);
        }
    }
}

fn close_handle(handle: u64) {
    if handle != 0 {
        syscall(SYS_HANDLE_CLOSE, [handle, 0, 0, 0, 0, 0]);
    }
}

/// exit (`group` false) and exit_group: the calling thread ends, with exit_group every
/// thread of its process.
pub fn exit(status: i32, group: bool) -> ! {
    let (tid, pid) = me();
    {
        let mut t = PROCS.lock();
        if group {
            t.group_exit(pid, status, Some(tid));
        }
        if let Some(th) = t.threads.get_mut(&tid) {
            th.exited = true;
        }
        if let Some(p) = t.procs.get_mut(&pid) {
            if tid == pid {
                p.main_status = Some(status);
            }
        }
    }
    let flags = if group { EXIT_GROUP } else { 0 };
    syscall(SYS_THREAD_EXIT, [status as u32 as u64, flags, 0, 0, 0, 0]);
    unreachable!("thread_exit returned")
}

/// The process ends as by a fatal signal or exit_group with `status`, from the calling
/// thread (which ends too).
pub fn die(status: i32) -> ! {
    exit(status, true)
}

/// `EVENT_THREAD_EXIT` (the service thread): the thread `key` is gone. The last thread of a
/// process ends the process.
pub fn thread_ended(key: u64) {
    let mut after = After::default();
    let gone = {
        let mut t = PROCS.lock();
        let Some(tid) = t.keys.remove(&key) else { return };
        let Some(th) = t.threads.remove(&tid) else { return };
        let pid = th.pid;
        let mut last = false;
        if let Some(p) = t.procs.get_mut(&pid) {
            p.threads.retain(|&x| x != tid);
            last = p.threads.is_empty();
            // An exec waits for the others to be gone.
            wake_word(&p.words.threads);
        }
        if last {
            process_end(&mut t, pid, &mut after);
        } else {
            // A group stop does not wait for a thread that is gone; process signals it was
            // to take go to another.
            signal::thread_left(&mut t, pid, &th);
        }
        th
    };
    drop(gone);
    after.run();
}

/// What must happen after the lock is let go of (it takes other locks, or the terminal's).
#[derive(Default)]
pub struct After {
    /// Sessions whose leader ended (their terminal is dissociated).
    sessions: Vec<Pid>,
    /// Kernel handles of processes reaped.
    handles: Vec<u64>,
}

impl After {
    pub fn run(self) {
        for h in self.handles {
            close_handle(h);
        }
        for sid in self.sessions {
            crate::tty::session_ended(sid as u64);
        }
    }
}

/// The last thread of process `pid` is gone: it becomes a zombie (lock held).
fn process_end(t: &mut Table, pid: Pid, after: &mut After) {
    let killed = {
        let p = &t.procs[&pid];
        let mut info = ProcInfo::default();
        syscall(SYS_PROC_INFO, [p.handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
        info.killed as i32
    };
    let (ppid, sid, pgid, vfork) = {
        let p = t.procs.get_mut(&pid).expect("listed");
        // A group exit's status, else the kernel's kill, else the main thread's own.
        let status = p.exiting.or((killed != 0).then_some(killed)).or(p.main_status).unwrap_or(0);
        p.zombie = Some(status);
        p.itimer = Default::default();
        (p.ppid, p.sid, p.pgid, core::mem::replace(&mut p.vfork, false))
    };
    crate::timer::changed();
    if vfork {
        let w = t.procs[&pid].words.clone();
        w.vfork.store(1, Ordering::Release);
        wake_word(&w.vfork);
    }
    if sid == pid {
        after.sessions.push(sid);
    }
    // Its process group may be left orphaned with stopped members (Linux's
    // kill_orphaned_pgrp): its parent tied it to the session.
    if let Some(parent) = t.procs.get(&ppid) {
        if parent.pgid != pgid && parent.sid == sid && t.pgrp_orphaned(pgid, Some(pid)) && t.pgrp_stopped(pgid) {
            signal::hup_and_continue(t, pgid);
        }
    }
    // Its children go to the reaper.
    let children = core::mem::take(&mut t.procs.get_mut(&pid).expect("listed").children);
    let reaper = t.reaper_for(pid);
    for child in children {
        let (cpgid, csid, pdeath, zombie) = {
            let Some(c) = t.procs.get_mut(&child) else { continue };
            c.ppid = reaper;
            // The new parent hears of the end the ordinary way.
            c.exit_signal = signal::SIGCHLD;
            (c.pgid, c.sid, c.pdeath, c.zombie.is_some())
        };
        if let Some(r) = t.procs.get_mut(&reaper) {
            r.children.push(child);
        }
        if pdeath != 0 && !zombie {
            signal::post_process(t, child, SigInfo::kernel(pdeath), false);
        }
        if zombie {
            // A zombie the new parent may reap (or that nobody will: gone now).
            if reaper == 0 || notify_parent_exit(t, child) {
                reap(t, child, after);
            }
        } else if cpgid != pgid && csid == sid && t.pgrp_orphaned(cpgid, None) && t.pgrp_stopped(cpgid) {
            signal::hup_and_continue(t, cpgid);
        }
    }
    // Pid 1's end ends the instance's every process (a pid namespace's init).
    if pid == 1 {
        let all: Vec<Pid> = t.procs.values().filter(|p| p.zombie.is_none()).map(|p| p.pid).collect();
        for other in all {
            t.group_exit(other, signal::SIGKILL as i32, None);
        }
    }
    if ppid == 0 || !t.procs.contains_key(&ppid) || notify_parent_exit(t, pid) {
        reap(t, pid, after);
    }
}

/// Tells the parent of the zombie `pid` that it ended (Linux's do_notify_parent): its exit
/// signal and a wakeup of its waits. Whether the parent does not want the zombie (it
/// ignores SIGCHLD or set SA_NOCLDWAIT): then it is reaped at once.
fn notify_parent_exit(t: &mut Table, pid: Pid) -> bool {
    let (ppid, exit_signal, status, handle) = {
        let p = &t.procs[&pid];
        (p.ppid, p.exit_signal, p.zombie.unwrap_or(0), p.handle)
    };
    let Some(parent) = t.procs.get(&ppid) else { return true };
    if parent.zombie.is_some() {
        return true;
    }
    let chld = t.hands[&parent.sig.hand].actions[signal::SIGCHLD as usize - 1];
    let words = parent.words.clone();
    let mut sig = exit_signal;
    let mut autoreap = false;
    if sig == signal::SIGCHLD && (chld.handler == signal::SIG_IGN || chld.flags & signal::SA_NOCLDWAIT != 0) {
        autoreap = true;
        if chld.handler == signal::SIG_IGN {
            sig = 0;
        }
    }
    if sig != 0 {
        let mut info = ProcInfo::default();
        syscall(SYS_PROC_INFO, [handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
        let (code, value) = if status & 0x7f == 0 { (signal::CLD_EXITED, (status >> 8) & 0xff) } else { (signal::CLD_KILLED, status & 0x7f) };
        let si = SigInfo::chld(sig, code, pid, value, info.user_ns, info.system_ns);
        signal::post_process(t, ppid, si, false);
    }
    wake_word(&words.child);
    autoreap
}

/// Removes the zombie `pid`: its usage goes to its parent's account (when one wants it, by
/// wait), its handle is closed after the lock.
fn reap(t: &mut Table, pid: Pid, after: &mut After) {
    let Some(p) = t.procs.remove(&pid) else { return };
    t.put_hand(p.sig.hand);
    if let Some(pp) = t.procs.get_mut(&p.ppid) {
        pp.children.retain(|&c| c != pid);
    }
    after.handles.push(p.handle);
}

/// What a child used: its own CPU time and memory and its reaped children's.
fn usage_of(p: &Proc) -> Usage {
    let mut info = ProcInfo::default();
    syscall(SYS_PROC_INFO, [p.handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
    Usage {
        user_ns: info.user_ns + p.children_usage.user_ns,
        system_ns: info.system_ns + p.children_usage.system_ns,
        peak_pages: info.peak_pages.max(p.children_usage.peak_pages),
    }
}

// ------------------------------------------------------------------ wait

const WNOHANG: u64 = 1;
const WUNTRACED: u64 = 2;
const WSTOPPED: u64 = 2;
const WEXITED: u64 = 4;
const WCONTINUED: u64 = 8;
const WNOWAIT: u64 = 0x0100_0000;
const WNOTHREAD: u64 = 0x2000_0000;
const WALL: u64 = 0x4000_0000;
const WCLONE: u64 = 0x8000_0000;

/// Which children a wait is for.
#[derive(Clone, Copy)]
enum Which {
    Pid(Pid),
    Pgrp(Pid),
    Any,
}

/// What a wait found.
struct Found {
    pid: Pid,
    /// wait4's status.
    status: i32,
    /// waitid's si_code and si_status.
    code: i32,
    value: i32,
    usage: Usage,
}

/// Waits for a child (`options`: WEXITED, WSTOPPED, WCONTINUED, WNOHANG, WNOWAIT, __WALL,
/// __WCLONE). None with WNOHANG if no child is ready.
fn wait_child(which: Which, options: u64) -> Result<Option<Found>, i64> {
    let pid = local::pid();
    // A kick ended the wait: one more look (a child's end wins over the signal it sent),
    // then EINTR.
    let mut interrupted = false;
    loop {
        let mut after = After::default();
        let (found, words, seen) = {
            let mut t = PROCS.lock();
            let me = t.procs.get(&pid).ok_or(ECHILD)?;
            let words = me.words.clone();
            let seen = words.child.load(Ordering::Acquire);
            let mut any = false;
            let mut found: Option<(Pid, Found, bool)> = None;
            for &c in &me.children {
                let Some(child) = t.procs.get(&c) else { continue };
                let selected = match which {
                    Which::Pid(p) => c == p,
                    Which::Pgrp(g) => child.pgid == g,
                    Which::Any => true,
                };
                // A "clone" child is one whose exit signal is not SIGCHLD: __WCLONE waits for
                // those only, __WALL for every child.
                let clone_child = child.exit_signal != signal::SIGCHLD;
                if !selected || (options & WALL == 0 && clone_child != (options & WCLONE != 0)) {
                    continue;
                }
                any = true;
                if let Some(status) = child.zombie {
                    if options & WEXITED != 0 {
                        let (code, value) =
                            if status & 0x7f == 0 { (signal::CLD_EXITED, (status >> 8) & 0xff) } else { (signal::CLD_KILLED, status & 0x7f) };
                        found = Some((c, Found { pid: c, status, code, value, usage: usage_of(child) }, true));
                        break;
                    }
                    continue;
                }
                match child.report {
                    Some(Report::Stopped(sig)) if options & WSTOPPED != 0 => {
                        let status = ((sig as i32) << 8) | 0x7f;
                        found = Some((c, Found { pid: c, status, code: signal::CLD_STOPPED, value: sig as i32, usage: usage_of(child) }, false));
                        break;
                    }
                    Some(Report::Continued) if options & WCONTINUED != 0 => {
                        let sig = signal::SIGCONT as i32;
                        found = Some((c, Found { pid: c, status: 0xffff, code: signal::CLD_CONTINUED, value: sig, usage: usage_of(child) }, false));
                        break;
                    }
                    _ => {}
                }
            }
            if let Some((c, f, exited)) = found {
                if options & WNOWAIT == 0 {
                    if exited {
                        let u = f.usage;
                        if let Some(me) = t.procs.get_mut(&pid) {
                            me.children_usage.user_ns += u.user_ns;
                            me.children_usage.system_ns += u.system_ns;
                            me.children_usage.peak_pages = me.children_usage.peak_pages.max(u.peak_pages);
                        }
                        reap(&mut t, c, &mut after);
                    } else if let Some(child) = t.procs.get_mut(&c) {
                        child.report = None;
                    }
                }
                (Some(f), words, seen)
            } else if !any {
                return Err(ECHILD);
            } else {
                (None, words, seen)
            }
        };
        after.run();
        if found.is_some() {
            return Ok(found);
        }
        if options & WNOHANG != 0 {
            return Ok(None);
        }
        if interrupted {
            return Err(EINTR);
        }
        if wait_word(&words.child, seen, 0, true) == -EINTR {
            interrupted = true;
        }
    }
}

/// wait4(pid, status, options, rusage).
fn wait4(pid: i32, status: u64, options: u64, rusage: u64) -> Result<i64, i64> {
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE) != 0 {
        return Err(EINVAL);
    }
    let which = match pid {
        p if p > 0 => Which::Pid(p as Pid),
        0 => Which::Pgrp(PROCS.lock().procs.get(&local::pid()).map_or(0, |p| p.pgid)),
        -1 => Which::Any,
        // INT_MIN cannot be negated: no such group.
        i32::MIN => return Err(ESRCH),
        p => Which::Pgrp(p.unsigned_abs()),
    };
    match wait_child(which, options | WEXITED)? {
        Some(f) => {
            if status != 0 {
                usercopy::write(status, &f.status)?;
            }
            if rusage != 0 {
                write_rusage(rusage, f.usage)?;
            }
            Ok(f.pid as i64)
        }
        None => Ok(0),
    }
}

/// waitid(idtype, id, infop, options, rusage).
fn waitid(idtype: u64, id: u32, infop: u64, options: u64, rusage: u64) -> Result<i64, i64> {
    const P_ALL: u64 = 0;
    const P_PID: u64 = 1;
    const P_PGID: u64 = 2;
    if options & !(WNOHANG | WSTOPPED | WEXITED | WCONTINUED | WNOWAIT | WNOTHREAD | WALL | WCLONE) != 0 {
        return Err(EINVAL);
    }
    if options & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
        return Err(EINVAL);
    }
    let which = match idtype {
        P_ALL => Which::Any,
        P_PID if id as i32 > 0 => Which::Pid(id),
        P_PGID if id == 0 => Which::Pgrp(PROCS.lock().procs.get(&local::pid()).map_or(0, |p| p.pgid)),
        P_PGID if (id as i32) > 0 => Which::Pgrp(id),
        // P_PIDFD: there are no pidfds yet.
        _ => return Err(EINVAL),
    };
    let found = wait_child(which, options)?;
    if infop != 0 {
        let mut info = [0u64; 16];
        if let Some(f) = &found {
            let si = SigInfo::chld(signal::SIGCHLD, f.code, f.pid, f.value, f.usage.user_ns, f.usage.system_ns);
            info = si.0;
        }
        // (No child ready with WNOHANG: a zeroed siginfo, si_pid 0.)
        usercopy::write(infop, &info)?;
    }
    if rusage != 0 {
        write_rusage(rusage, found.as_ref().map_or(Usage::default(), |f| f.usage))?;
    }
    Ok(0)
}

/// Writes a `struct rusage`: CPU time (user, system) and the most memory resident
/// (ru_maxrss, in KiB); the other fields are zero.
pub fn write_rusage(addr: u64, u: Usage) -> Result<(), i64> {
    let timeval = |ns: u64| [ns / 1_000_000_000, ns % 1_000_000_000 / 1000];
    let mut out = [0u64; 18];
    out[..2].copy_from_slice(&timeval(u.user_ns));
    out[2..4].copy_from_slice(&timeval(u.system_ns));
    out[4] = u.peak_pages * 4;
    usercopy::write(addr, &out)
}

// ------------------------------------------------------------------ groups and sessions

/// The process `pid` names (0: the caller's), as Linux's find_task_by_vpid: a thread's id
/// names its process too.
fn target(t: &Table, pid: i32) -> Result<Pid, i64> {
    if pid < 0 {
        return Err(ESRCH);
    }
    if pid == 0 {
        return Ok(local::pid());
    }
    let pid = pid as Pid;
    if t.procs.contains_key(&pid) {
        return Ok(pid);
    }
    t.threads.get(&pid).map(|th| th.pid).ok_or(ESRCH)
}

fn getpgid(pid: i32) -> Result<i64, i64> {
    let t = PROCS.lock();
    let p = target(&t, pid)?;
    Ok(t.procs[&p].pgid as i64)
}

fn getsid(pid: i32) -> Result<i64, i64> {
    let t = PROCS.lock();
    let p = target(&t, pid)?;
    Ok(t.procs[&p].sid as i64)
}

/// setpgid(pid, pgid), with Linux's rules: the caller or a child of its (not after the
/// child's exec, EACCES), in the caller's session, not a session leader (EPERM); the group
/// is the process's own or one of the session (EPERM).
fn setpgid(pid: i32, pgid: i32) -> Result<i64, i64> {
    if pgid < 0 {
        return Err(EINVAL);
    }
    let mut t = PROCS.lock();
    let me = local::pid();
    let target = if pid == 0 { me } else { pid as Pid };
    let pgid = if pgid == 0 { target } else { pgid as Pid };
    let my_sid = t.procs[&me].sid;
    let p = t.procs.get(&target).filter(|p| p.zombie.is_none()).ok_or(ESRCH)?;
    if target != me {
        if p.ppid != me {
            return Err(ESRCH);
        }
        if p.sid != my_sid {
            return Err(EPERM);
        }
        if p.did_exec {
            return Err(EACCES);
        }
    }
    if p.sid == target {
        return Err(EPERM);
    }
    if pgid != target && !t.procs.values().any(|q| q.pgid == pgid && q.sid == my_sid && q.zombie.is_none()) {
        return Err(EPERM);
    }
    t.procs.get_mut(&target).expect("found above").pgid = pgid;
    Ok(0)
}

/// setsid(): a new session and process group led by the caller (EPERM if it leads a group).
fn setsid() -> Result<i64, i64> {
    let mut t = PROCS.lock();
    let me = local::pid();
    if t.procs.values().any(|p| p.pgid == me) {
        return Err(EPERM);
    }
    let p = t.procs.get_mut(&me).ok_or(ESRCH)?;
    p.sid = me;
    p.pgid = me;
    Ok(me as i64)
}

/// The ids of process `id` (0: the caller's) for the terminals: (pid, process group,
/// session).
pub fn ids_of(id: Pid) -> Option<(Pid, Pid, Pid)> {
    let t = PROCS.lock();
    let pid = if id == 0 { local::pid() } else { id };
    let p = t.procs.get(&pid)?;
    Some((p.pid, p.pgid, p.sid))
}

/// The session of a member of process group `pgid`, if it has one.
pub fn pgrp_session(pgid: Pid) -> Option<Pid> {
    let t = PROCS.lock();
    t.procs.values().find(|p| p.pgid == pgid).map(|p| p.sid)
}

/// Whether process group `pgid` is orphaned.
pub fn pgrp_orphaned(pgid: Pid) -> bool {
    PROCS.lock().pgrp_orphaned(pgid, None)
}

/// Whether a process `pid` of the instance exists (a zombie counts).
pub fn exists(pid: Pid) -> bool {
    let t = PROCS.lock();
    t.procs.contains_key(&pid) || t.threads.contains_key(&pid)
}

/// The key of thread `tid` (0: the caller), if it lives.
pub fn key_of(tid: Pid) -> Option<u64> {
    if tid == 0 {
        return Some(local::get().key.load(Ordering::Relaxed));
    }
    let t = PROCS.lock();
    t.threads.get(&tid).filter(|th| th.key != 0).map(|th| th.key)
}

/// The keys of every live thread of the processes that `select` picks.
pub fn keys_where(select: impl Fn(&Proc) -> bool) -> Vec<u64> {
    let t = PROCS.lock();
    t.procs
        .values()
        .filter(|p| p.zombie.is_none() && select(p))
        .flat_map(|p| p.threads.iter().filter_map(|tid| t.threads.get(tid)).filter(|th| th.key != 0).map(|th| th.key))
        .collect()
}

/// The working directory of process `pid` (its main thread's, or its first live one's).
pub fn cwd_of(pid: Pid) -> Option<String> {
    let fs = {
        let t = PROCS.lock();
        let p = t.procs.get(&pid)?;
        p.threads.first().and_then(|tid| t.threads.get(tid)).map(|th| th.fs.clone())?
    };
    let cwd = fs.state.lock().cwd.clone();
    Some(cwd)
}

/// The umask of process `pid` (its main thread's record).
pub fn umask_of(pid: Pid) -> Option<u32> {
    let fs = {
        let t = PROCS.lock();
        let p = t.procs.get(&pid)?;
        p.threads.first().and_then(|tid| t.threads.get(tid)).map(|th| th.fs.clone())?
    };
    let umask = fs.state.lock().umask;
    Some(umask)
}

/// /proc's records of the instance's processes (`procproto`'s `QUERY_PIDS`,
/// `QUERY_THREADS`, `QUERY_PROCESS`, `QUERY_CMDLINE`, `QUERY_EXE`), from the process table
/// and the kernel's accounts (`proc_info`, `thread_info`): the bytes of the answer, at most
/// `cap` (lists of ids: what fits; ERANGE for a record that does not fit), ESRCH for no
/// such process (or thread).
pub fn query(op: u64, arg: u64, cap: usize) -> Result<Vec<u8>, i64> {
    use procproto::*;
    const ERANGE: i64 = 34;
    let t = PROCS.lock();
    let ids = |list: &mut dyn Iterator<Item = Pid>| -> Vec<u8> { list.take(cap / 8).flat_map(|p| (p as u64).to_le_bytes()).collect() };
    let text = |bytes: &[u8]| if bytes.len() > cap { Err(ERANGE) } else { Ok(bytes.to_vec()) };
    // The process `arg` names: a process, or a thread's.
    let id = arg as Pid;
    let owner = || -> Result<Pid, i64> {
        if t.procs.contains_key(&id) {
            Ok(id)
        } else {
            t.threads.get(&id).map(|th| th.pid).ok_or(ESRCH)
        }
    };
    match op {
        QUERY_PIDS => Ok(ids(&mut t.procs.keys().copied())),
        QUERY_THREADS => {
            let p = t.procs.get(&owner()?).ok_or(ESRCH)?;
            let mut tids = p.threads.clone();
            tids.sort_unstable();
            Ok(ids(&mut tids.into_iter()))
        }
        QUERY_CMDLINE => text(&t.procs[&owner()?].cmdline),
        QUERY_EXE => text(t.procs[&owner()?].exe.as_bytes()),
        QUERY_PROCESS => {
            let pid = owner()?;
            let p = &t.procs[&pid];
            let mut info = ProcInfo::default();
            syscall(SYS_PROC_INFO, [p.handle, &mut info as *mut ProcInfo as u64, 0, 0, 0, 0]);
            // The thread asked for, or the main (first live) one.
            let tid = if t.threads.contains_key(&id) { id } else { p.threads.first().copied().unwrap_or(pid) };
            let th = t.threads.get(&tid);
            let mut tinfo = ThreadInfo::default();
            if let Some(th) = th.filter(|th| th.key != 0) {
                syscall(SYS_THREAD_INFO, [th.key, &mut tinfo as *mut ThreadInfo as u64, 0, 0, 0, 0]);
            }
            const TICK_NS: u64 = 10_000_000;
            let (user, system) = if t.threads.contains_key(&id) && id != pid { (tinfo.user_ns, tinfo.system_ns) } else { (info.user_ns, info.system_ns) };
            let state = if p.zombie.is_some() {
                STATE_ZOMBIE
            } else if p.sig.stop.stopped {
                STATE_STOPPED
            } else if (if id != pid { tinfo.running } else { info.running }) > 0 {
                STATE_RUNNING
            } else {
                STATE_SLEEPING
            };
            let actions = &t.hands[&p.sig.hand].actions;
            let (mut ignored, mut caught) = (0, 0);
            for (i, a) in actions.iter().enumerate() {
                match a.handler {
                    signal::SIG_IGN => ignored |= 1 << i,
                    signal::SIG_DFL => {}
                    _ => caught |= 1 << i,
                }
            }
            let comm = th.map_or([0; 16], |th| th.comm);
            let record = Process {
                pid: id as u64,
                tgid: pid as u64,
                ppid: p.ppid as u64,
                pgid: p.pgid as u64,
                sid: p.sid as u64,
                state: state as u64,
                utime: user / TICK_NS,
                stime: system / TICK_NS,
                start: info.start_ticks,
                pages: info.pages,
                virt_pages: info.virt_pages,
                nice: tinfo.nice,
                threads: p.threads.len() as u64,
                cpu: tinfo.cpu,
                flags: 0,
                legacy_calls: info.legacy_calls,
                peak_pages: info.peak_pages,
                virt_peak: info.virt_pages,
                sig_pending: th.map_or(0, |th| th.sig.pending.set),
                sig_shared: p.sig.shared.set,
                sig_blocked: th.map_or(0, |th| th.sig.mask),
                sig_ignored: ignored,
                sig_caught: caught,
                name: comm,
            };
            text(as_bytes(&record))
        }
        _ => Err(EINVAL),
    }
}

/// The program break of process `pid` (shared by the processes sharing its address
/// space).
pub fn brk_of(pid: Pid) -> Option<Arc<Mutex<Brk>>> {
    PROCS.lock().procs.get(&pid).map(|p| p.brk.clone())
}

/// The kernel's handle on process `pid`.
pub fn handle_of(pid: Pid) -> Option<u64> {
    PROCS.lock().procs.get(&pid).map(|p| p.handle)
}
