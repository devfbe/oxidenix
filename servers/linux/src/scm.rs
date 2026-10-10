//! Descriptors in flight (SCM_RIGHTS over AF_UNIX sockets, phase R7a; in
//! the server's own memory since R6e): a descriptor a message carries is a
//! reference to its open file description (`Passed`), whatever the file is
//! (one of the server's or one of the kernel's it holds by handle), counted
//! in the description's `inflight`. It keeps the description alive while
//! the message waits in a socket's queue, also after the sender closed its
//! descriptor; the receiver gets a descriptor of its own for the same
//! description (shared offset and status flags, as on Linux). A message
//! dropped unread lets go of its references: a description whose last one
//! that was closes as if its last descriptor went.
//!
//! Garbage: a socket can be in flight in its own queue, or two in each
//! other's, after every descriptor of theirs is closed; then only the
//! messages keep them, and nobody can ever receive those. The collector
//! (`collect`, Linux's unix_gc) finds them: a socket in flight whose
//! description has no reference but its references in flight (a
//! descriptor, and a call that uses the socket, holds one too: Linux's
//! fdget) is a candidate; a candidate that a message outside the
//! candidates' queues refers to (one of a reachable socket's queue, or one
//! being sent or received right now) is reachable, and so is every
//! candidate a reachable candidate's queue (or the queues of a listener's
//! unaccepted connections) refers to. The rest is garbage: their queues are
//! emptied, which lets go of the references and with them the sockets.
//!
//! The collector runs on the instance's worker thread (`worker`), asked by
//! `request`: when a reference to a description in flight goes and only
//! references in flight may be left (`files::FileRef`'s drop: a descriptor
//! closed, also by exit or exec, or a call that used one ended; as Linux's
//! unix_gc looks when a file's references are all in flight), and when a
//! socket closes; requests that come while it runs make one more run.
//! Never on the pager: the collector waits for sockets' locks, and the
//! pager must stay free to bring the pages whose faults a holder of such a
//! lock may wait for. It also runs on the sender's own thread before a
//! sender is refused for too many descriptors in flight.
//!
//! Bounds: at most `MAX_INFLIGHT` descriptors are in flight in the
//! instance, and each user may have at most `MAX_PER_USER` (Linux's
//! per-user too_many_unix_fds; everyone is root here, so that is the
//! instance's bound again). A user's charge goes with the descriptor,
//! whoever then holds it, so neither forks nor pid reuse change it.
//!
//! Consistency: what the collector looks at holds still while it runs.
//! `GC` is a reader-writer lock: making a descriptor in flight, installing
//! one (also MSG_PEEK's copies) and letting one go take it shared; the
//! collector takes it exclusively from choosing the candidates until their
//! queues are emptied (Linux's unix_gc_lock and unix_peek_fds). Messages
//! move between queues without it, but a socket a call takes messages from
//! or sends them through is referenced by the call (no candidate), and a
//! message on its way counts as referring from outside.
//!
//! Locking order: `GC`, then `INFLIGHT`, then sockets' locks; so a `Passed`
//! is never dropped under a socket's lock, nor while `GC` is held.

use crate::files::{self, Description, File, FileRef};
use crate::sync::{Mutex, ReadGuard, RwLock};
use crate::syscall;
use crate::unix::{Cred, Sock};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

const ETOOMANYREFS: i64 = 109;

/// Most descriptors in flight in the instance.
const MAX_INFLIGHT: usize = 16 * 1024;
/// Most descriptors one user may have in flight.
const MAX_PER_USER: usize = 16 * 1024;

/// A descriptor in flight.
pub struct Passed {
    file: Arc<Description>,
    /// The server's socket it is, if it is one (for the collector).
    sock: Option<Arc<Sock>>,
    /// The user charged for it.
    user: u32,
}

/// The sockets in flight: for each (by its description's id), the socket,
/// its description and how many references to it are in flight; how many
/// descriptors are in flight, and how many of each user.
struct Inflight {
    sockets: BTreeMap<u64, (Arc<Sock>, Weak<Description>, usize)>,
    total: usize,
    users: BTreeMap<u32, usize>,
}

static GC: RwLock = RwLock::new();
static INFLIGHT: Mutex<Inflight> = Mutex::new(Inflight { sockets: BTreeMap::new(), total: 0, users: BTreeMap::new() });

/// Collections asked for and done (futex words of the worker).
static REQUESTED: AtomicU32 = AtomicU32::new(0);

/// Asks the worker to collect (it coalesces requests).
pub fn request() {
    REQUESTED.fetch_add(1, Ordering::Release);
    syscall(SYS_SERVER_FUTEX_WAKE, [&REQUESTED as *const AtomicU32 as u64, 1, 0, 0, 0, 0]);
}

/// The worker thread: lets go of the descriptor tables whose processes
/// ended (`fdtable::end_later`), connects to a restarted diskfs
/// (`datafs::reconnect_later`), shrinks the heap and /data's inode cache
/// when memory is short (`heap::shrink_if_asked`) and collects whenever
/// asked.
pub fn worker() -> ! {
    let mut done = 0;
    loop {
        let asked = REQUESTED.load(Ordering::Acquire);
        if asked != done {
            done = asked;
            crate::datafs::reconnect_if_asked();
            crate::fdtable::release_ended();
            crate::heap::shrink_if_asked();
            collect();
            continue;
        }
        syscall(SYS_SERVER_FUTEX_WAIT, [&REQUESTED as *const AtomicU32 as u64, asked as u64, 0, 0, 0, 0]);
    }
}

/// Holds the collector off (see the module comment): for MSG_PEEK's
/// copies, which are installed under a socket's lock.
pub fn hold() -> ReadGuard<'static> {
    GC.read()
}

/// Charges one more descriptor in flight to `user` and the instance,
/// unless either is at its bound; false then.
fn charge(user: u32) -> bool {
    let mut gc = INFLIGHT.lock();
    if gc.total >= MAX_INFLIGHT || gc.users.get(&user).is_some_and(|&n| n >= MAX_PER_USER) {
        return false;
    }
    gc.total += 1;
    *gc.users.entry(user).or_insert(0) += 1;
    true
}

fn uncharge(gc: &mut Inflight, user: u32) {
    gc.total -= 1;
    if let Some(n) = gc.users.get_mut(&user) {
        *n -= 1;
        if *n == 0 {
            gc.users.remove(&user);
        }
    }
}

impl Passed {
    /// Descriptor `fd` of the calling process, to send (EBADF; an O_PATH
    /// one too, as Linux's fget_raw; ETOOMANYREFS when too many are in
    /// flight even after collecting).
    pub fn take(fd: i32) -> Result<Passed, i64> {
        if fd < 0 {
            return Err(files::EBADF);
        }
        let file = files::lookup_raw(fd as u64)?;
        let user = Cred::current().uid;
        if !charge(user) {
            // Linux's wait_for_unix_gc: garbage may hold them. (On this
            // thread, which brings no pages.)
            collect();
            if !charge(user) {
                return Err(ETOOMANYREFS);
            }
        }
        let _gc = GC.read();
        let file = file.into_arc();
        file.inflight.fetch_add(1, Ordering::AcqRel);
        let sock = match &file.file {
            File::Socket(s) => Some(s.clone()),
            _ => None,
        };
        if let Some(s) = &sock {
            let mut gc = INFLIGHT.lock();
            let entry = gc.sockets.entry(file.id).or_insert_with(|| (s.clone(), Arc::downgrade(&file), 0));
            entry.2 += 1;
        }
        Ok(Passed { file, sock, user })
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
        crate::fdtable::current().install(FileRef::new(self.file.clone()), cloexec, 0).map(|fd| fd as i32)
    }

    /// The socket it is, if one of the server's.
    pub fn sock(&self) -> Option<&Arc<Sock>> {
        self.sock.as_ref()
    }
}

impl Drop for Passed {
    fn drop(&mut self) {
        {
            let _gc = GC.read();
            let mut gc = INFLIGHT.lock();
            if self.sock.is_some() {
                let id = self.file.id;
                if let Some(entry) = gc.sockets.get_mut(&id) {
                    entry.2 -= 1;
                    if entry.2 == 0 {
                        gc.sockets.remove(&id);
                    }
                }
            }
            self.file.inflight.fetch_sub(1, Ordering::AcqRel);
            uncharge(&mut gc, self.user);
        }
        // The reference goes after the locks (its file may close now).
    }
}

/// Whether any socket is in flight (the collector has work only then).
pub fn sockets_in_flight() -> bool {
    !INFLIGHT.lock().sockets.is_empty()
}

/// One collection at a time, from choosing the candidates until their
/// messages are let go of (and their descriptors uncharged): a sender
/// refused for too many in flight collects itself, and must find a
/// collection under way (the worker's, after an exit) done, not half done
/// with its garbage still charged (Linux's wait_for_unix_gc). A sleeping
/// lock: letting go of the garbage closes sockets, which may wait for netd.
static COLLECTOR: crate::sync::SleepLock = crate::sync::SleepLock::new(());

/// Finds the sockets only unreachable messages keep and empties their
/// queues (see the module comment); waits for a collection under way first.
pub fn collect() {
    // (A thread that dies meanwhile does not collect.)
    let Ok(_one) = COLLECTOR.lock() else { return };
    let gc_lock = GC.write();
    let garbage = {
        let gc = INFLIGHT.lock();
        if gc.sockets.is_empty() {
            return;
        }
        // Candidates: every reference is one in flight (no descriptor, no
        // call that uses it). The reference taken to look is not counted.
        let mut candidates: BTreeMap<u64, (Arc<Sock>, usize)> = BTreeMap::new();
        for (&id, (sock, file, n)) in gc.sockets.iter() {
            let Some(file) = file.upgrade() else { continue };
            if Arc::strong_count(&file) - 1 == *n {
                candidates.insert(id, (sock.clone(), *n));
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
        // references let go of) once the locks are free.
        dead.iter().flat_map(|s| s.purge()).collect::<Vec<_>>()
    };
    drop(gc_lock);
    drop(garbage);
}
