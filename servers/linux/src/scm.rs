//! Descriptors in flight (SCM_RIGHTS over AF_UNIX sockets, phase R7a): a
//! descriptor a message carries is a handle on its open file description
//! (`SYS_KFILE_OBJECT`), whatever the file is (one of the kernel's, or a
//! placeholder of one of the server's files). The handle keeps the
//! description alive while the message waits in a socket's queue, also
//! after the sender closed its descriptor; the receiver gets a descriptor of
//! its own for the same description (`SYS_KFD_INSTALL_FILE`: shared offset
//! and status flags, as on Linux), and the handle goes. A message dropped
//! unread (its socket closed) closes its handles: a placeholder whose last
//! reference that was is reported closed as if its last descriptor went.
//!
//! Garbage: a socket can be in flight in its own queue, or two in each
//! other's, after every descriptor of theirs is closed; then only the
//! messages keep them, and nobody can ever receive those. The collector
//! (`collect`, Linux's unix_gc) finds them: a socket in flight whose
//! description has no reference but its handles in flight
//! (`SYS_KFILE_INFO`) and no call in progress is a candidate; a candidate
//! that a message outside the candidates' queues refers to (one of a
//! reachable socket's queue, or one being sent or received right now) is
//! reachable, and so is every candidate a reachable candidate's queue (or
//! the queues of a listener's unaccepted connections) refers to. The rest
//! is garbage: their queues are emptied, which closes the handles and
//! with them the sockets. It runs when a descriptor is closed while
//! sockets are in flight (a close passed through to the kernel, or a
//! socket's last reference gone).
//!
//! Locking: `INFLIGHT` is taken before any socket's lock and never while
//! one is held; so a `Passed` is never dropped under a socket's lock.

use crate::files::{self, File};
use crate::sync::Mutex;
use crate::syscall;
use crate::unix::Sock;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use restricted::*;

const ETOOMANYREFS: i64 = 109;

/// Most descriptors in flight in the instance at once (Linux bounds them by
/// the sender's RLIMIT_NOFILE).
const MAX_INFLIGHT: usize = 16 * 1024;

/// A descriptor in flight.
pub struct Passed {
    handle: u64,
    /// The server's socket it is, if it is one (for the collector).
    sock: Option<Arc<Sock>>,
}

/// The sockets in flight: for each (by its file id), the socket and the
/// handles of the descriptors in flight that name it.
struct Inflight {
    sockets: BTreeMap<u64, (Arc<Sock>, Vec<u64>)>,
}

static INFLIGHT: Mutex<Inflight> = Mutex::new(Inflight { sockets: BTreeMap::new() });
/// Descriptors in flight in the instance (all kinds).
static TOTAL: AtomicUsize = AtomicUsize::new(0);

impl Passed {
    /// Descriptor `fd` of the calling process, to send (EBADF).
    pub fn take(fd: i32) -> Result<Passed, i64> {
        if fd < 0 {
            return Err(files::EBADF);
        }
        if TOTAL.fetch_add(1, Ordering::Relaxed) >= MAX_INFLIGHT {
            TOTAL.fetch_sub(1, Ordering::Relaxed);
            return Err(ETOOMANYREFS);
        }
        let handle = syscall(SYS_KFILE_OBJECT, [fd as u64, 0, 0, 0, 0, 0]);
        if handle < 0 {
            TOTAL.fetch_sub(1, Ordering::Relaxed);
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
            INFLIGHT.lock().sockets.entry(id).or_insert_with(|| (s.clone(), Vec::new())).1.push(handle);
        }
        Ok(Passed { handle, sock })
    }

    /// A descriptor of the calling process for it (close-on-exec with
    /// `cloexec`); the passed reference goes.
    pub fn install(self, cloexec: bool) -> Result<i32, i64> {
        self.install_copy(cloexec)
    }

    /// A descriptor of the calling process for it, which stays in flight
    /// (MSG_PEEK).
    pub fn install_copy(&self, cloexec: bool) -> Result<i32, i64> {
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
        if let Some(s) = &self.sock {
            let mut gc = INFLIGHT.lock();
            let id = s.id();
            if let Some((_, handles)) = gc.sockets.get_mut(&id) {
                handles.retain(|&h| h != self.handle);
                if handles.is_empty() {
                    gc.sockets.remove(&id);
                }
            }
        }
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
        TOTAL.fetch_sub(1, Ordering::Relaxed);
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
    let garbage = {
        let gc = INFLIGHT.lock();
        if gc.sockets.is_empty() {
            return;
        }
        // Candidates: every reference is a handle in flight, and no call
        // uses the socket.
        let mut candidates: BTreeMap<u64, (Arc<Sock>, usize)> = BTreeMap::new();
        for (&id, (sock, handles)) in gc.sockets.iter() {
            if sock.busy() {
                continue;
            }
            let Some((refs, _)) = info(handles[0]) else { continue };
            if refs as usize == handles.len() {
                candidates.insert(id, (sock.clone(), handles.len()));
            }
        }
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
        let mut seen: alloc::collections::BTreeSet<u64> = reachable.iter().copied().collect();
        while let Some(id) = reachable.pop() {
            for t in edges.get(&id).into_iter().flatten() {
                if candidates.contains_key(t) && seen.insert(*t) {
                    reachable.push(*t);
                }
            }
        }
        let dead: Vec<Arc<Sock>> = candidates.into_iter().filter(|(id, _)| !seen.contains(id)).map(|(_, (s, _))| s).collect();
        // Their messages leave the queues now; they are dropped (and the
        // handles closed) once the lock is free.
        dead.iter().map(|s| s.purge()).collect::<Vec<_>>()
    };
    drop(garbage);
}
