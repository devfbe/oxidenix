//! Descriptors in flight (SCM_RIGHTS over AF_UNIX sockets, phase R7a): a
//! descriptor a message carries is a handle on its open file description
//! (`SYS_KFILE_OBJECT` with `KFILE_INFLIGHT`), whatever the file is (one of
//! the kernel's, or a placeholder of one of the server's files). The handle
//! keeps the description alive while the message waits in a socket's
//! queue, also after the sender closed its descriptor; the receiver gets a
//! descriptor of its own for the same description (`SYS_KFD_INSTALL_FILE`:
//! shared offset and status flags, as on Linux), and the handle goes. A
//! message dropped unread (its socket closed) closes its handles: a
//! placeholder whose last reference that was is reported closed as if its
//! last descriptor went.
//!
//! Garbage: a socket can be in flight in its own queue, or two in each
//! other's, after every descriptor of theirs is closed; then only the
//! messages keep them, and nobody can ever receive those. The collector
//! (`collect`, Linux's unix_gc) finds them: a socket in flight whose
//! description has no reference but its handles in flight
//! (`SYS_KFILE_INFO`; a call that uses the socket pins it, `kfd_lookup`, so
//! it is referenced while the call lasts) is a candidate; a candidate that a
//! message outside the candidates' queues refers to (one of a reachable
//! socket's queue, or one being sent or received right now) is reachable,
//! and so is every candidate a reachable candidate's queue (or the queues
//! of a listener's unaccepted connections) refers to. The rest is garbage:
//! their queues are emptied, which closes the handles and with them the
//! sockets.
//!
//! The collector runs when the kernel reports a reference to a socket in
//! flight gone (`EVENT_INFLIGHT`: a descriptor closed, also by exit or
//! exec, or a call that pinned it ended), when a socket closes, and before
//! a sender is refused for having too many descriptors in flight.
//!
//! Consistency: what the collector looks at holds still while it runs.
//! `GC` is a reader-writer lock: making a descriptor in flight, installing
//! one (also MSG_PEEK's copies) and letting one go take it shared; the
//! collector takes it exclusively from choosing the candidates until their
//! queues are emptied (Linux's unix_gc_lock and unix_peek_fds). Messages
//! move between queues without it, but a socket a call takes messages from
//! or sends them through is pinned (no candidate), and a message on its way
//! counts as referring from outside.
//!
//! Locking order: `GC`, then `INFLIGHT`, then sockets' locks; so a `Passed`
//! is never dropped under a socket's lock, nor while `GC` is held.

use crate::files::{self, File};
use crate::sync::{Mutex, ReadGuard, RwLock};
use crate::syscall;
use crate::unix::{Cred, Sock};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use restricted::*;

const ETOOMANYREFS: i64 = 109;

/// Most descriptors one process may have in flight (Linux bounds a user's
/// by RLIMIT_NOFILE); the instance's handle table bounds them all.
const MAX_PER_SENDER: usize = 16 * 1024;

/// A descriptor in flight.
pub struct Passed {
    handle: u64,
    /// The server's socket it is, if it is one (for the collector).
    sock: Option<Arc<Sock>>,
    /// The process charged for it.
    sender: u32,
}

/// The sockets in flight: for each (by its file id), the socket and the
/// handles of the descriptors in flight that name it; and how many each
/// process has in flight.
struct Inflight {
    sockets: BTreeMap<u64, (Arc<Sock>, BTreeSet<u64>)>,
    senders: BTreeMap<u32, usize>,
}

static GC: RwLock = RwLock::new();
static INFLIGHT: Mutex<Inflight> = Mutex::new(Inflight { sockets: BTreeMap::new(), senders: BTreeMap::new() });

/// Holds the collector off (see the module comment): for MSG_PEEK's
/// copies, which are installed under a socket's lock.
pub fn hold() -> ReadGuard<'static> {
    GC.read()
}

/// Charges one more descriptor in flight to `sender`, unless it has its
/// limit; false then.
fn charge(sender: u32) -> bool {
    let mut gc = INFLIGHT.lock();
    let n = gc.senders.entry(sender).or_insert(0);
    if *n >= MAX_PER_SENDER {
        return false;
    }
    *n += 1;
    true
}

fn uncharge(gc: &mut Inflight, sender: u32) {
    if let Some(n) = gc.senders.get_mut(&sender) {
        *n -= 1;
        if *n == 0 {
            gc.senders.remove(&sender);
        }
    }
}

impl Passed {
    /// Descriptor `fd` of the calling process, to send (EBADF; ETOOMANYREFS
    /// when the caller has too many in flight even after collecting).
    pub fn take(fd: i32) -> Result<Passed, i64> {
        if fd < 0 {
            return Err(files::EBADF);
        }
        let sender = Cred::current().pid;
        if !charge(sender) {
            // Linux's wait_for_unix_gc: garbage may hold them.
            collect();
            if !charge(sender) {
                return Err(ETOOMANYREFS);
            }
        }
        let _gc = GC.read();
        let handle = syscall(SYS_KFILE_OBJECT, [fd as u64, KFILE_INFLIGHT, 0, 0, 0, 0]);
        if handle < 0 {
            uncharge(&mut INFLIGHT.lock(), sender);
            return Err(-handle);
        }
        let handle = handle as u64;
        // The description the handle holds, not whatever the descriptor
        // names by now.
        let id = info(handle).map_or(0, |(_, id)| id);
        let sock = match files::get(id) {
            Some(File::Socket(s)) => Some(s),
            _ => None,
        };
        if let Some(s) = &sock {
            INFLIGHT.lock().sockets.entry(id).or_insert_with(|| (s.clone(), BTreeSet::new())).1.insert(handle);
        }
        Ok(Passed { handle, sock, sender })
    }

    /// A descriptor of the calling process for it (close-on-exec with
    /// `cloexec`); the passed reference goes.
    pub fn install(self, cloexec: bool) -> Result<i32, i64> {
        let gc = GC.read();
        let r = self.install_copy(&gc, cloexec);
        drop(gc);
        r
    }

    /// A descriptor of the calling process for it, which stays in flight
    /// (MSG_PEEK); the caller holds the collector off (`hold`).
    pub fn install_copy(&self, _gc: &ReadGuard, cloexec: bool) -> Result<i32, i64> {
        let flags = if cloexec { files::O_CLOEXEC as u64 } else { 0 };
        let fd = syscall(SYS_KFD_INSTALL_FILE, [self.handle, flags, 0, 0, 0, 0]);
        if fd < 0 { Err(-fd) } else { Ok(fd as i32) }
    }

    /// The socket it is, if one of the server's.
    pub fn sock(&self) -> Option<&Arc<Sock>> {
        self.sock.as_ref()
    }
}

impl Drop for Passed {
    fn drop(&mut self) {
        let _gc = GC.read();
        let mut gc = INFLIGHT.lock();
        if let Some(s) = &self.sock {
            let id = s.id();
            if let Some((_, handles)) = gc.sockets.get_mut(&id) {
                handles.remove(&self.handle);
                if handles.is_empty() {
                    gc.sockets.remove(&id);
                }
            }
        }
        uncharge(&mut gc, self.sender);
        drop(gc);
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
    }
}

/// (references, the server's file id or 0) of the description behind a
/// `kfile_object` handle.
fn info(handle: u64) -> Option<(u64, u64)> {
    let mut out = [0u64; 2];
    if syscall(SYS_KFILE_INFO, [handle, out.as_mut_ptr() as u64, 0, 0, 0, 0]) < 0 {
        return None;
    }
    Some((out[0], out[1]))
}

/// Whether any socket is in flight (the collector has work only then).
pub fn sockets_in_flight() -> bool {
    !INFLIGHT.lock().sockets.is_empty()
}

/// Finds the sockets only unreachable messages keep and empties their
/// queues (see the module comment).
pub fn collect() {
    let gc_lock = GC.write();
    let garbage = {
        let gc = INFLIGHT.lock();
        if gc.sockets.is_empty() {
            return;
        }
        // Candidates: every reference is a handle in flight (no descriptor,
        // no call that pinned it).
        let mut candidates: BTreeMap<u64, (Arc<Sock>, usize)> = BTreeMap::new();
        for (&id, (sock, handles)) in gc.sockets.iter() {
            let Some(&first) = handles.iter().next() else { continue };
            let Some((refs, _)) = info(first) else { continue };
            if refs as usize == handles.len() {
                candidates.insert(id, (sock.clone(), handles.len()));
            }
        }
        drop(gc);
        if candidates.is_empty() {
            return;
        }
        // What each candidate's queues refer to.
        let edges: BTreeMap<u64, Vec<u64>> = candidates.iter().map(|(&id, (sock, _))| (id, sock.passed_sockets())).collect();
        // References from candidates' queues, taken off each target's
        // count: what remains comes from outside.
        let mut outside: BTreeMap<u64, usize> = candidates.iter().map(|(&id, (_, n))| (id, *n)).collect();
        for targets in edges.values() {
            for t in targets {
                if let Some(n) = outside.get_mut(t) {
                    *n = n.saturating_sub(1);
                }
            }
        }
        let mut reachable: Vec<u64> = outside.iter().filter(|(_, n)| **n > 0).map(|(id, _)| *id).collect();
        let mut seen: BTreeSet<u64> = reachable.iter().copied().collect();
        while let Some(id) = reachable.pop() {
            for t in edges.get(&id).into_iter().flatten() {
                if candidates.contains_key(t) && seen.insert(*t) {
                    reachable.push(*t);
                }
            }
        }
        let dead: Vec<Arc<Sock>> = candidates.into_iter().filter(|(id, _)| !seen.contains(id)).map(|(_, (s, _))| s).collect();
        // Their messages leave the queues now; they are dropped (and the
        // handles closed) once the locks are free.
        dead.iter().flat_map(|s| s.purge()).collect::<Vec<_>>()
    };
    drop(gc_lock);
    drop(garbage);
}
