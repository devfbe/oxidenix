//! AF_UNIX sockets (phase R7a): stream, datagram and sequenced-packet
//! sockets of the server, each a file of its own, as pipes are. This is the sockets' semantics (Linux's
//! net/unix/af_unix.c); `sockcalls` is their system calls (addresses,
//! message headers, ancillary data, options).
//!
//! A message is the unit of every queue: a stream's bytes are queued in
//! the pieces they were written in (a read takes across them, but never
//! glues pieces of different credentials when they are passed, and stops
//! after a piece that carried descriptors), a datagram or a packet is one
//! message. A message carries its descriptors (`scm::Passed`), its
//! sender's credentials and name, and is charged to its sender until it is
//! read: a sender waits while it has its send buffer's worth unread
//! (SO_SNDBUF), and is writable for poll once that is down to a quarter, as
//! on Linux. A datagram receiver also holds back senders that are not its
//! peer once more than 10 datagrams wait (net.unix.max_dgram_qlen).
//!
//! Connections: connect() makes the server's end at once (an embryo with
//! the listener's name) and queues it on the listener, where accept()
//! takes it; both ends are connected from then on. Closing an end shuts the
//! other down both ways (it reads what is left, then end of file; a writer
//! gets EPIPE and SIGPIPE), with ECONNRESET first if the closed end had
//! unread data. Names: a path is a socket inode of the server's tmpfs or of
//! /data, found by its inode; the abstract namespace is the instance's.
//! Both are bound in `BINDINGS`.
//!
//! Waiting and readiness as for pipes: every change of a socket bumps its
//! sequence word and wakes its waiters, and its readiness (Linux's
//! unix_poll and unix_dgram_poll) is reported under its lock.
//!
//! Locking: a socket's lock is never held while another's is taken, with
//! two exceptions that cannot form a cycle: the leaf lock of a socket's
//! waiter list, and a new connection (not yet reachable by anyone else),
//! set up under its listener's lock. The collector's lock (`scm`) comes
//! before any socket's (MSG_PEEK installs descriptors under a socket's
//! lock). Messages (whose drop uncharges their sender and may close
//! descriptors in flight) are dropped only once no socket lock is held.
//! No server lock is held while program memory is copied, except a
//! socket's receive lock, which no service thread takes: a copy may fault
//! on a page the pager must bring (an mmap of /data), and the pager takes
//! sockets' locks (closing them). A receive plans its message under the
//! state lock, copies it with only the receive lock held (receives are
//! serialized, so the message stays at the front unless its socket is
//! emptied, which it then finds by the message's id), and takes what the
//! copy took under the state lock again. A send copies into the server's
//! memory before it takes a lock.
//!
//! A call on a socket keeps it open while it lasts (it holds a reference
//! to the description until it returns, Linux's fdget): another thread's
//! close does not end a blocked receive or accept, as on Linux.

use crate::files;
use crate::namespace::Node;
use crate::scm::Passed;
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use restricted::*;

pub const STREAM: u32 = 1;
pub const DGRAM: u32 = 2;
pub const SEQPACKET: u32 = 5;

pub const EINTR: i64 = 4;
pub const EAGAIN: i64 = 11;
pub const EFAULT: i64 = 14;
pub const EINVAL: i64 = 22;
pub const EPIPE: i64 = 32;
pub const EADDRINUSE: i64 = 98;
pub const EMSGSIZE: i64 = 90;
pub const EPROTOTYPE: i64 = 91;
pub const EOPNOTSUPP: i64 = 95;
pub const ECONNRESET: i64 = 104;
pub const ENOBUFS: i64 = 105;
pub const EISCONN: i64 = 106;
pub const ENOTCONN: i64 = 107;
pub const ETIMEDOUT: i64 = 110;
pub const ECONNREFUSED: i64 = 111;
pub const EALREADY: i64 = 114;
pub const EPERM: i64 = 1;
pub const ENOTSOCK: i64 = 88;

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;
pub const POLLRDHUP: i16 = 0x2000;

/// Shutdown bits: no more receiving, no more sending.
pub const RCV: u8 = 1;
pub const SEND: u8 = 2;

/// net.core.wmem_default and wmem_max (rmem the same).
pub const DEFAULT_BUF: usize = 212992;
pub const MAX_BUF: usize = 212992;
pub const MIN_SNDBUF: usize = 4608;
pub const MIN_RCVBUF: usize = 2304;
/// What a message costs its sender beyond its bytes (Linux charges the
/// buffer's true size).
const OVERHEAD: usize = 512;
/// net.unix.max_dgram_qlen.
const MAX_DGRAM_QLEN: usize = 10;
/// net.core.somaxconn.
pub const SOMAXCONN: usize = 4096;
/// The largest piece one stream write queues.
const MAX_PIECE: usize = 32 * 1024;

/// Credentials as SCM_CREDENTIALS and SO_PEERCRED carry them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Cred {
    /// The calling thread's.
    pub fn current() -> Cred {
        // Everyone is root (uid and gid 0).
        Cred { pid: crate::local::pid(), uid: 0, gid: 0 }
    }

    /// None known (an unconnected socket's peer): Linux's pid 0 and the
    /// ids -1.
    pub const NONE: Cred = Cred { pid: 0, uid: u32::MAX, gid: u32::MAX };

    pub fn bytes(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&self.pid.to_le_bytes());
        b[4..8].copy_from_slice(&self.uid.to_le_bytes());
        b[8..12].copy_from_slice(&self.gid.to_le_bytes());
        b
    }
}

/// A socket's name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Name {
    /// A path, as bind(2) was given it.
    Path(Vec<u8>),
    /// A name in the abstract namespace (without its leading NUL).
    Abstract(Vec<u8>),
}

pub const AF_UNIX: u16 = 1;

impl Name {
    /// The sockaddr_un of a name (just the family for none), as long as it
    /// is: a path with its NUL, an abstract name without one.
    pub fn sockaddr(name: Option<&Name>) -> Vec<u8> {
        let mut out = alloc::vec::Vec::from(AF_UNIX.to_le_bytes());
        match name {
            None => {}
            Some(Name::Path(p)) => {
                out.extend_from_slice(p);
                out.push(0);
            }
            Some(Name::Abstract(a)) => {
                out.push(0);
                out.extend_from_slice(a);
            }
        }
        out
    }
}

/// Where a name is bound: a socket inode (by filesystem and inode number)
/// or an abstract name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Tmp(u64),
    Data(u64),
    Abstract(Vec<u8>),
}

/// A socket's binding: its key, and the inode it keeps (so that a /data
/// inode number is not reused while the socket lives).
struct Binding {
    key: Key,
    _node: Option<Node>,
}

/// The bound names of the instance.
static BINDINGS: Mutex<BTreeMap<Key, Weak<Sock>>> = Mutex::new(BTreeMap::new());

/// The socket bound at `key`, if it still lives.
pub fn find(key: &Key) -> Option<Arc<Sock>> {
    BINDINGS.lock().get(key).and_then(|w| w.upgrade())
}

/// A receive's next message: what it copies with no lock held.
struct Plan {
    id: u64,
    data: Arc<Vec<u8>>,
    off: usize,
    /// It carries descriptors.
    fds: bool,
    /// Its credentials, when the receiver asked for them.
    cred: Option<Cred>,
    from: Option<Arc<Name>>,
}

enum Next {
    Piece(Plan),
    /// Nothing yet: sleep while the sequence word holds this.
    Wait(u32),
}

/// Ids of messages (unique in the instance): a receive finds its message
/// again by it after the copy.
static NEXT_MSG: AtomicU64 = AtomicU64::new(1);

/// A queued message (see the module comment).
pub struct Msg {
    id: u64,
    /// Shared with a receive copying it out.
    data: Arc<Vec<u8>>,
    /// Bytes of a stream piece read already.
    off: usize,
    fds: Vec<Passed>,
    cred: Cred,
    from: Option<Arc<Name>>,
    sender: Arc<Sock>,
    charge: usize,
}

impl Msg {
    fn left(&self) -> usize {
        self.data.len() - self.off
    }
}

impl Drop for Msg {
    fn drop(&mut self) {
        self.sender.uncharge(self.charge);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Unconnected,
    Listening,
    Connected,
}

struct Inner {
    state: State,
    /// A connect() is under way (stream, seqpacket).
    connecting: bool,
    /// A bind() is under way.
    binding: bool,
    /// The connected end (stream, seqpacket) or the default destination
    /// (datagram).
    peer: Option<Arc<Sock>>,
    name: Option<Arc<Name>>,
    bound: Option<Binding>,
    queue: VecDeque<Msg>,
    /// Unread bytes in `queue`.
    bytes: usize,
    /// A listener's connections not yet accepted, and its backlog.
    pending: VecDeque<Arc<Sock>>,
    backlog: usize,
    /// RCV, SEND.
    shut: u8,
    /// The pending error (SO_ERROR), 0 or an errno.
    err: i64,
    passcred: bool,
    rcvbuf: usize,
    /// SO_RCVTIMEO and SO_SNDTIMEO, in nanoseconds (0: none).
    rcvtimeo: u64,
    sndtimeo: u64,
    /// The peer's credentials when the connection was made (SO_PEERCRED).
    peercred: Option<Cred>,
    /// The listener's own, from listen(), for the clients.
    listen_cred: Cred,
    reported: i16,
    /// Its last reference is gone.
    dead: bool,
    /// Options kept for getsockopt that change nothing here (SO_LINGER,
    /// SO_RCVLOWAT, SO_KEEPALIVE, ...).
    opts: BTreeMap<u64, i32>,
}

pub struct Sock {
    pub ty: u32,
    me: Weak<Sock>,
    /// The id of its open file description (0 until it has one: an
    /// unaccepted connection).
    id: AtomicU64,
    seq: AtomicU32,
    /// What its messages queued elsewhere are charged (see `Msg`).
    wmem: AtomicUsize,
    /// SO_SNDBUF (read without the lock by `uncharge`).
    sndbuf: AtomicUsize,
    /// Messages in its queue (for datagram senders' poll).
    qlen: AtomicUsize,
    /// The address of its peer's `Sock` (0: none), for a peer's lockless
    /// look.
    peer_addr: AtomicUsize,
    /// Datagram senders whose writability waits for room in its queue (a
    /// leaf lock).
    waiters: Mutex<Vec<Weak<Sock>>>,
    /// Receives, one at a time: held across the copy to the program's
    /// memory (which may wait for the pager), where the state lock is not.
    /// Never taken by a service thread.
    rlock: crate::sync::SleepLock,
    inner: Mutex<Inner>,
}

/// Received: bytes copied, the message's length (datagrams), whether it
/// was truncated, and what came with it.
pub struct Received {
    pub copied: usize,
    pub len: usize,
    pub trunc: bool,
    pub fds: Fds,
    pub cred: Option<Cred>,
    pub from: Option<Arc<Name>>,
}

/// Descriptors a receive got.
pub enum Fds {
    /// Taken from their message, to be installed.
    Taken(Vec<Passed>),
    /// Installed already, copies of those of a message left in the queue
    /// (MSG_PEEK): as many as there was room for, of how many.
    Installed(Vec<i32>, usize),
}

/// How a receive goes.
#[derive(Clone, Copy)]
pub struct RecvOpts {
    /// MSG_PEEK: the data stays, the descriptors are installed as copies.
    pub peek: bool,
    /// MSG_WAITALL (streams): wait until the buffer is full.
    pub waitall: bool,
    pub nonblock: bool,
    /// MSG_CMSG_CLOEXEC: received descriptors are close-on-exec.
    pub cloexec: bool,
    /// How many descriptors the caller's control buffer has room for
    /// (MSG_PEEK installs no more).
    pub fd_room: usize,
}

/// Installs copies of a peeked message's descriptors, as many as there is
/// room for (`gc` holds the collector off).
fn peek_fds(fds: &[Passed], gc: &crate::sync::ReadGuard, o: &RecvOpts) -> Fds {
    let mut out = Vec::new();
    for p in fds.iter().take(o.fd_room) {
        match p.install_copy(gc, o.cloexec) {
            Ok(fd) => out.push(fd),
            // EMFILE: the rest are not delivered (MSG_CTRUNC).
            Err(_) => break,
        }
    }
    Fds::Installed(out, fds.len())
}

/// Where received bytes go.
pub enum Sink<'a> {
    /// The program's buffers (base, length), from `idx` and `off` on.
    Program { vecs: &'a [(u64, u64)], idx: usize, off: u64 },
    /// The server's memory, from `at` on.
    Server { buf: &'a mut [u8], at: usize },
}

impl Sink<'_> {
    pub fn program(vecs: &[(u64, u64)]) -> Sink<'_> {
        Sink::Program { vecs, idx: 0, off: 0 }
    }

    pub fn room(&self) -> usize {
        match self {
            Sink::Program { vecs, idx, off } => {
                vecs.iter().skip(*idx).fold(0u64, |t, v| t.saturating_add(v.1)).saturating_sub(*off).min(isize::MAX as u64) as usize
            }
            Sink::Server { buf, at } => buf.len() - *at,
        }
    }

    /// Copies as much of `data` as fits; a fault ends the copy where it
    /// is (EFAULT if nothing was copied).
    pub fn put(&mut self, data: &[u8]) -> Result<usize, i64> {
        match self {
            Sink::Server { buf, at } => {
                let n = data.len().min(buf.len() - *at);
                buf[*at..*at + n].copy_from_slice(&data[..n]);
                *at += n;
                Ok(n)
            }
            Sink::Program { vecs, idx, off } => {
                let mut done = 0;
                while done < data.len() && *idx < vecs.len() {
                    let (base, len) = vecs[*idx];
                    if *off >= len {
                        *idx += 1;
                        *off = 0;
                        continue;
                    }
                    let at = base.wrapping_add(*off);
                    // A piece never crosses a page, so a hole ends the copy
                    // exactly where it begins.
                    let n = ((len - *off) as usize).min(data.len() - done).min(4096 - (at % 4096) as usize);
                    if usercopy::to_program(at, &data[done..done + n]).is_err() {
                        return if done > 0 { Ok(done) } else { Err(EFAULT) };
                    }
                    done += n;
                    *off += n as u64;
                }
                Ok(done)
            }
        }
    }
}

/// Where sent bytes come from.
pub enum Source<'a> {
    Program { vecs: &'a [(u64, u64)], idx: usize, off: u64 },
    Server { buf: &'a [u8], at: usize },
}

impl Source<'_> {
    pub fn program(vecs: &[(u64, u64)]) -> Source<'_> {
        Source::Program { vecs, idx: 0, off: 0 }
    }

    pub fn left(&self) -> usize {
        match self {
            Source::Program { vecs, idx, off } => {
                vecs.iter().skip(*idx).fold(0u64, |t, v| t.saturating_add(v.1)).saturating_sub(*off).min(isize::MAX as u64) as usize
            }
            Source::Server { buf, at } => buf.len() - *at,
        }
    }

    /// Copies up to `out.len()` bytes into `out` (the server's memory, an
    /// internet socket's send ring), fewer at a fault (EFAULT if none could
    /// be read).
    pub fn read_into(&mut self, out: &mut [u8]) -> Result<usize, i64> {
        match self {
            Source::Server { buf, at } => {
                let n = out.len().min(buf.len() - *at);
                out[..n].copy_from_slice(&buf[*at..*at + n]);
                *at += n;
                Ok(n)
            }
            Source::Program { vecs, idx, off } => {
                let mut done = 0;
                while done < out.len() && *idx < vecs.len() {
                    let (base, len) = vecs[*idx];
                    if *off >= len {
                        *idx += 1;
                        *off = 0;
                        continue;
                    }
                    let at = base.wrapping_add(*off);
                    // A piece never crosses a page: a hole ends the copy
                    // exactly where it begins.
                    let k = ((len - *off) as usize).min(out.len() - done).min(4096 - (at % 4096) as usize);
                    if usercopy::from_program(at, &mut out[done..done + k]).is_err() {
                        if done == 0 {
                            return Err(EFAULT);
                        }
                        // The rest is not there: what was read is all.
                        *idx = vecs.len();
                        break;
                    }
                    done += k;
                    *off += k as u64;
                }
                Ok(done)
            }
        }
    }

    /// Up to `n` bytes, fewer at a fault (EFAULT if none could be read).
    pub fn take(&mut self, n: usize) -> Result<Vec<u8>, i64> {
        let mut out = Vec::new();
        out.try_reserve_exact(n).map_err(|_| ENOBUFS)?;
        match self {
            Source::Server { buf, at } => {
                let n = n.min(buf.len() - *at);
                out.extend_from_slice(&buf[*at..*at + n]);
                *at += n;
            }
            Source::Program { vecs, idx, off } => {
                let mut chunk = [0u8; 4096];
                while out.len() < n && *idx < vecs.len() {
                    let (base, len) = vecs[*idx];
                    if *off >= len {
                        *idx += 1;
                        *off = 0;
                        continue;
                    }
                    let at = base.wrapping_add(*off);
                    let k = ((len - *off) as usize).min(n - out.len()).min(4096 - (at % 4096) as usize);
                    if usercopy::from_program(at, &mut chunk[..k]).is_err() {
                        if out.is_empty() {
                            return Err(EFAULT);
                        }
                        // The rest is not there: what was read is all.
                        *idx = vecs.len();
                        break;
                    }
                    out.extend_from_slice(&chunk[..k]);
                    *off += k as u64;
                }
            }
        }
        Ok(out)
    }
}



/// Monotonic nanoseconds.
fn now() -> u64 {
    const CLOCK_MONOTONIC: u64 = 1;
    syscall(SYS_CLOCK_READ, [CLOCK_MONOTONIC, 0, 0, 0, 0, 0]).max(0) as u64
}

/// The deadline of a wait with timeout `ns` (0: none) starting now.
fn deadline(ns: u64) -> u64 {
    if ns == 0 { 0 } else { now().saturating_add(ns).max(1) }
}

impl Sock {
    pub fn new(ty: u32) -> Arc<Sock> {
        Arc::new_cyclic(|me| Sock {
            ty,
            me: me.clone(),
            id: AtomicU64::new(0),
            seq: AtomicU32::new(0),
            wmem: AtomicUsize::new(0),
            sndbuf: AtomicUsize::new(DEFAULT_BUF),
            qlen: AtomicUsize::new(0),
            peer_addr: AtomicUsize::new(0),
            waiters: Mutex::new(Vec::new()),
            rlock: crate::sync::SleepLock::new(()),
            inner: Mutex::new(Inner {
                state: State::Unconnected,
                connecting: false,
                binding: false,
                peer: None,
                name: None,
                bound: None,
                queue: VecDeque::new(),
                bytes: 0,
                pending: VecDeque::new(),
                backlog: 0,
                shut: 0,
                err: 0,
                passcred: false,
                rcvbuf: DEFAULT_BUF,
                rcvtimeo: 0,
                sndtimeo: 0,
                peercred: None,
                listen_cred: Cred::NONE,
                reported: 0,
                dead: false,
                opts: BTreeMap::new(),
            }),
        })
    }

    /// Two connected sockets (socketpair), each with the caller's
    /// credentials for the other.
    pub fn pair(ty: u32) -> (Arc<Sock>, Arc<Sock>) {
        let (a, b) = (Sock::new(ty), Sock::new(ty));
        let cred = Cred::current();
        for (me, other) in [(&a, &b), (&b, &a)] {
            let mut i = me.inner.lock();
            i.state = State::Connected;
            i.peercred = Some(cred);
            me.set_peer(&mut i, Some(other.clone()));
        }
        (a, b)
    }

    pub fn id(&self) -> u64 {
        self.id.load(Ordering::Acquire)
    }

    /// Its open file description's id, once it has one; readiness goes
    /// there from now on (`report_now` after the description exists).
    pub fn set_id(&self, id: u64) {
        self.id.store(id, Ordering::Release);
    }


    fn set_peer(&self, i: &mut Inner, peer: Option<Arc<Sock>>) {
        self.peer_addr.store(peer.as_ref().map_or(0, |p| Arc::as_ptr(p) as usize), Ordering::Release);
        i.peer = peer;
    }

    /// Whether this datagram receiver's queue holds back `sender` (not its
    /// peer) now (Linux's unix_recvq_full for a sender it is not connected
    /// to).
    fn holds_back(&self, sender: &Sock) -> bool {
        self.peer_addr.load(Ordering::Acquire) != sender as *const Sock as usize && self.qlen.load(Ordering::Acquire) > MAX_DGRAM_QLEN
    }

    fn add_waiter(&self, w: Weak<Sock>) {
        let mut ws = self.waiters.lock();
        if !ws.iter().any(|x| x.ptr_eq(&w)) {
            ws.push(w);
        }
    }

    /// Room in the queue, or the socket gone: the senders held back may
    /// go on.
    fn wake_waiters(&self) {
        let ws = core::mem::take(&mut *self.waiters.lock());
        for w in ws {
            if let Some(s) = w.upgrade() {
                s.notify(true);
            }
        }
    }

    fn writable(&self) -> bool {
        self.wmem.load(Ordering::Acquire) * 4 <= self.sndbuf.load(Ordering::Acquire)
    }

    /// Poll readiness (lock held): Linux's unix_poll and unix_dgram_poll.
    fn readiness(&self, i: &Inner) -> i16 {
        let mut m = 0;
        if i.err != 0 {
            m |= POLLERR;
        }
        if i.shut == RCV | SEND {
            m |= POLLHUP;
        }
        if i.shut & RCV != 0 {
            m |= POLLRDHUP | POLLIN;
        }
        if !i.queue.is_empty() || !i.pending.is_empty() {
            m |= POLLIN;
        }
        if self.ty == DGRAM {
            let mut writable = self.writable();
            if writable {
                if let Some(p) = &i.peer {
                    if p.holds_back(self) {
                        writable = false;
                        p.add_waiter(self.me.clone());
                    }
                }
            }
            if writable {
                m |= POLLOUT;
            }
            return m;
        }
        if i.state == State::Unconnected && !i.connecting {
            m |= POLLHUP;
        }
        if self.ty == SEQPACKET && i.connecting {
            return m;
        }
        if i.state != State::Listening && self.writable() {
            m |= POLLOUT;
        }
        m
    }

    /// After a change (lock held): wakes the waiters and reports the
    /// readiness if it changed or `event` (new data, room: an edge for
    /// EPOLLET).
    fn changed(&self, i: &mut Inner, event: bool) {
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        let id = self.id();
        if id == 0 {
            return;
        }
        let now = self.readiness(i);
        if now != i.reported || event {
            i.reported = now;
            files::ready(id, now);
        }
    }

    fn notify(&self, event: bool) {
        let mut i = self.inner.lock();
        self.changed(&mut i, event);
    }

    /// Reports the readiness as it is (a new description).
    pub fn report_now(&self) {
        let mut i = self.inner.lock();
        let now = self.readiness(&i);
        i.reported = now;
        files::ready(self.id(), now);
    }

    pub fn readiness_now(&self) -> i16 {
        self.readiness(&self.inner.lock())
    }

    /// Sleeps while the sequence word is `seen`, until `deadline` (0: none):
    /// EINTR for a signal, EAGAIN when the time is up.
    fn wait(&self, seen: u32, deadline: u64) -> Result<(), i64> {
        let word = &self.seq as *const AtomicU32 as u64;
        match syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, deadline, FUTEX_INTERRUPTIBLE, 0, 0]) {
            r if r == -EINTR => Err(EINTR),
            r if r == -ETIMEDOUT => Err(EAGAIN),
            _ => Ok(()),
        }
    }

    /// Charges a message of its own: once a quarter of the send buffer is
    /// taken, it is no longer writable for poll.
    fn charge(&self, n: usize, sndbuf: usize) {
        let old = self.wmem.fetch_add(n, Ordering::AcqRel);
        if old * 4 <= sndbuf && (old + n) * 4 > sndbuf {
            self.notify(false);
        }
    }

    /// A message of its own was read (or dropped): writers waiting for
    /// room go on once it is below the send buffer, and while it is
    /// writable each freed message is room for poll (an edge for EPOLLET),
    /// as Linux's unix_write_space.
    fn uncharge(&self, n: usize) {
        let old = self.wmem.fetch_sub(n, Ordering::AcqRel);
        let (new, sndbuf) = (old - n, self.sndbuf.load(Ordering::Acquire));
        if new * 4 <= sndbuf || (old >= sndbuf && new < sndbuf) {
            self.notify(true);
        }
    }

    // Binding.

    /// Starts a bind (EINVAL if it is bound or binding already).
    pub fn begin_bind(&self) -> Result<(), i64> {
        let mut i = self.inner.lock();
        if i.name.is_some() || i.binding {
            return Err(EINVAL);
        }
        i.binding = true;
        Ok(())
    }

    /// Ends a bind: `name` at `key` (EADDRINUSE if the key is bound), or,
    /// with `None`, nothing (it failed).
    pub fn end_bind(&self, bound: Option<(Name, Key, Option<Node>)>) -> Result<(), i64> {
        let Some((name, key, node)) = bound else {
            self.inner.lock().binding = false;
            return Ok(());
        };
        {
            let mut b = BINDINGS.lock();
            if b.get(&key).is_some_and(|w| w.strong_count() > 0) {
                drop(b);
                self.inner.lock().binding = false;
                return Err(EADDRINUSE);
            }
            b.insert(key.clone(), self.me.clone());
        }
        let mut i = self.inner.lock();
        i.binding = false;
        i.name = Some(Arc::new(name));
        i.bound = Some(Binding { key, _node: node });
        Ok(())
    }

    /// Binds a free name of the abstract namespace (five hex digits, as
    /// Linux's autobind) unless it is bound.
    pub fn autobind(&self) -> Result<(), i64> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        if self.inner.lock().name.is_some() {
            return Ok(());
        }
        if self.begin_bind().is_err() {
            // Bound meanwhile (or binding: that bind decides).
            return Ok(());
        }
        for _ in 0..0x10_0000 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed) & 0xf_ffff;
            let mut text = Vec::with_capacity(5);
            for shift in (0..5).rev() {
                text.push(b"0123456789abcdef"[(n >> (shift * 4)) as usize & 0xf]);
            }
            match self.end_bind(Some((Name::Abstract(text.clone()), Key::Abstract(text), None))) {
                Err(EADDRINUSE) => {
                    self.begin_bind()?;
                }
                r => return r,
            }
        }
        self.end_bind(None)?;
        Err(EADDRINUSE)
    }

    pub fn name(&self) -> Option<Arc<Name>> {
        self.inner.lock().name.clone()
    }

    /// The peer's name (ENOTCONN without a peer).
    pub fn peer_name(&self) -> Result<Option<Arc<Name>>, i64> {
        let peer = self.inner.lock().peer.clone().ok_or(ENOTCONN)?;
        Ok(peer.name())
    }

    // Connections.

    /// listen(backlog).
    pub fn listen(&self, backlog: i32) -> Result<(), i64> {
        if self.ty == DGRAM {
            return Err(EOPNOTSUPP);
        }
        let mut i = self.inner.lock();
        if i.name.is_none() {
            return Err(EINVAL);
        }
        if i.state == State::Connected || i.connecting {
            return Err(EINVAL);
        }
        i.backlog = (backlog.max(0) as usize).min(SOMAXCONN);
        if i.state != State::Listening {
            i.state = State::Listening;
            i.listen_cred = Cred::current();
        }
        self.changed(&mut i, false);
        Ok(())
    }

    /// connect() of a stream or seqpacket socket to the listener `target`.
    pub fn connect(self: &Arc<Self>, target: &Arc<Sock>, nonblock: bool) -> Result<(), i64> {
        let timeout = {
            let mut i = self.inner.lock();
            match i.state {
                State::Listening => return Err(EINVAL),
                State::Connected => return Err(EISCONN),
                State::Unconnected if i.connecting => return Err(EALREADY),
                State::Unconnected => {}
            }
            i.connecting = true;
            self.changed(&mut i, false);
            i.sndtimeo
        };
        let result = self.connect_to(target, nonblock, deadline(timeout));
        if result.is_err() {
            let mut i = self.inner.lock();
            i.connecting = false;
            self.changed(&mut i, false);
        }
        result
    }

    fn connect_to(self: &Arc<Self>, target: &Arc<Sock>, nonblock: bool, deadline: u64) -> Result<(), i64> {
        if target.ty != self.ty {
            return Err(EPROTOTYPE);
        }
        // The server's end, with the client's credentials.
        let embryo = Sock::new(self.ty);
        let listen_cred = loop {
            let mut t = target.inner.lock();
            if t.dead || t.state != State::Listening {
                return Err(ECONNREFUSED);
            }
            if t.pending.len() > t.backlog {
                let seen = target.seq.load(Ordering::Acquire);
                drop(t);
                if nonblock {
                    return Err(EAGAIN);
                }
                target.wait(seen, deadline)?;
                continue;
            }
            {
                let mut e = embryo.inner.lock();
                e.state = State::Connected;
                e.peercred = Some(Cred::current());
                e.name = t.name.clone();
                e.passcred = t.passcred;
                embryo.set_peer(&mut e, Some(self.clone()));
            }
            t.pending.push_back(embryo.clone());
            target.changed(&mut t, true);
            break t.listen_cred;
        };
        let mut i = self.inner.lock();
        i.connecting = false;
        if i.dead {
            // Closed meanwhile: the server's end sees the connection reset.
            drop(i);
            embryo.peer_gone(true);
            return Err(ECONNRESET);
        }
        i.state = State::Connected;
        i.peercred = Some(listen_cred);
        self.set_peer(&mut i, Some(embryo));
        self.changed(&mut i, true);
        Ok(())
    }

    /// accept(): the next connection (waits for one).
    pub fn accept(&self, nonblock: bool) -> Result<Arc<Sock>, i64> {
        if self.ty == DGRAM {
            return Err(EOPNOTSUPP);
        }
        let deadline = deadline(self.inner.lock().rcvtimeo);
        loop {
            let seen;
            {
                let mut i = self.inner.lock();
                if i.state != State::Listening {
                    return Err(EINVAL);
                }
                if let Some(e) = i.pending.pop_front() {
                    self.changed(&mut i, false);
                    return Ok(e);
                }
                seen = self.seq.load(Ordering::Acquire);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.wait(seen, deadline)?;
        }
    }

    /// Puts back a connection accept() could not give a descriptor.
    pub fn unaccept(&self, e: Arc<Sock>) {
        let mut i = self.inner.lock();
        if i.state == State::Listening && !i.dead {
            i.pending.push_front(e);
            self.changed(&mut i, true);
            return;
        }
        drop(i);
        e.release();
    }

    /// connect() of a datagram socket: `target` becomes the default
    /// destination (None: none).
    pub fn set_dgram_peer(&self, target: Option<Arc<Sock>>) -> Result<(), i64> {
        if let Some(t) = &target {
            if t.ty != self.ty {
                return Err(EPROTOTYPE);
            }
            if t.inner.lock().dead {
                return Err(ECONNREFUSED);
            }
        }
        let mut i = self.inner.lock();
        i.state = if target.is_some() { State::Connected } else { State::Unconnected };
        i.peercred = None;
        let old = i.peer.take();
        self.set_peer(&mut i, target);
        self.changed(&mut i, false);
        drop(i);
        drop(old);
        Ok(())
    }

    /// The peer closed (stream, seqpacket): no more data either way, with
    /// ECONNRESET first if it left data unread (or never accepted the
    /// connection).
    fn peer_gone(&self, reset: bool) {
        let mut i = self.inner.lock();
        i.shut = RCV | SEND;
        if reset {
            i.err = ECONNRESET;
        }
        self.changed(&mut i, true);
    }

    /// shutdown(how): 0 no more receiving, 1 no more sending, 2 both.
    pub fn shutdown(&self, how: u64) -> Result<(), i64> {
        let mode = match how {
            0 => RCV,
            1 => SEND,
            2 => RCV | SEND,
            _ => return Err(EINVAL),
        };
        let peer = {
            // Linux takes it on any socket, connected or not.
            let mut i = self.inner.lock();
            i.shut |= mode;
            self.changed(&mut i, true);
            i.peer.clone()
        };
        if let Some(p) = peer.filter(|_| self.ty != DGRAM) {
            let mirrored = (if mode & RCV != 0 { SEND } else { 0 }) | if mode & SEND != 0 { RCV } else { 0 };
            let mut pi = p.inner.lock();
            pi.shut |= mirrored;
            p.changed(&mut pi, true);
        }
        Ok(())
    }

    /// Its description's last reference is gone (or an unaccepted
    /// connection's listener went): the socket closes.
    pub fn release(&self) {
        let (peer, msgs, pending, bound) = {
            let mut i = self.inner.lock();
            if i.dead {
                return;
            }
            i.dead = true;
            i.shut = RCV | SEND;
            let msgs: Vec<Msg> = i.queue.drain(..).collect();
            i.bytes = 0;
            self.qlen.store(0, Ordering::Release);
            let pending: Vec<Arc<Sock>> = i.pending.drain(..).collect();
            let peer = i.peer.take();
            self.peer_addr.store(0, Ordering::Release);
            let bound = i.bound.take();
            i.state = State::Unconnected;
            self.changed(&mut i, true);
            (peer, msgs, pending, bound)
        };
        // A connection reset: data left unread, or a connection never
        // accepted (no description).
        let reset = !msgs.is_empty() || self.id() == 0;
        if let Some(b) = bound {
            let mut map = BINDINGS.lock();
            if map.get(&b.key).is_some_and(|w| w.ptr_eq(&self.me)) {
                map.remove(&b.key);
            }
        }
        if let Some(p) = &peer {
            if self.ty != DGRAM {
                p.peer_gone(reset);
            }
        }
        for e in pending {
            e.release();
        }
        self.wake_waiters();
        drop(msgs);
        drop(peer);
    }

    /// Takes every message out of its queues (its own and, a listener's,
    /// those of its unaccepted connections): the collector found it
    /// unreachable. The caller drops them once it holds no lock.
    pub fn purge(&self) -> Vec<Msg> {
        let (mut msgs, pending) = {
            let mut i = self.inner.lock();
            let msgs: Vec<Msg> = i.queue.drain(..).collect();
            i.bytes = 0;
            self.qlen.store(0, Ordering::Release);
            self.changed(&mut i, false);
            (msgs, i.pending.iter().cloned().collect::<Vec<_>>())
        };
        for e in pending {
            msgs.extend(e.purge());
        }
        msgs
    }

    /// The ids of the server's sockets the descriptors in its queues (and
    /// those of its unaccepted connections) name.
    pub fn passed_sockets(&self) -> Vec<u64> {
        let (mut out, pending) = {
            let i = self.inner.lock();
            let out: Vec<u64> = i.queue.iter().flat_map(|m| m.fds.iter()).filter_map(|p| p.sock().map(|s| s.id())).collect();
            (out, i.pending.iter().cloned().collect::<Vec<_>>())
        };
        for e in pending {
            out.extend(e.passed_sockets());
        }
        out
    }

    // Sending and receiving.

    /// Sends from `src` to the peer, or to `to` (datagrams): the bytes, the
    /// descriptors and the credentials. A stream sends everything (waiting
    /// for room unless `nonblock`: then what fits), a datagram or packet
    /// one message. EPIPE for a stream whose peer stopped receiving: the
    /// caller raises SIGPIPE.
    pub fn send(self: &Arc<Self>, src: &mut Source, to: Option<Arc<Sock>>, fds: Vec<Passed>, cred: Cred, nonblock: bool) -> Result<usize, i64> {
        if self.ty == STREAM { self.send_stream(src, fds, cred, nonblock) } else { self.send_dgram(src, to, fds, cred, nonblock) }
    }

    /// Waits until its unread messages elsewhere are charged less than its
    /// send buffer.
    fn wait_room(&self, nonblock: bool, deadline: u64) -> Result<(), i64> {
        loop {
            let seen;
            {
                let i = self.inner.lock();
                if self.wmem.load(Ordering::Acquire) < self.sndbuf.load(Ordering::Acquire) {
                    return Ok(());
                }
                if i.shut & SEND != 0 {
                    return Err(EPIPE);
                }
                seen = self.seq.load(Ordering::Acquire);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.wait(seen, deadline)?;
        }
    }

    fn send_stream(self: &Arc<Self>, src: &mut Source, mut fds: Vec<Passed>, cred: Cred, nonblock: bool) -> Result<usize, i64> {
        let (peer, sndbuf, timeout, name) = {
            let i = self.inner.lock();
            if i.shut & SEND != 0 {
                return Err(EPIPE);
            }
            let peer = match (&i.peer, i.state) {
                (Some(p), State::Connected) => p.clone(),
                _ => return Err(ENOTCONN),
            };
            (peer, self.sndbuf.load(Ordering::Acquire), i.sndtimeo, i.name.clone())
        };
        let deadline = deadline(timeout);
        let piece_max = (sndbuf / 2).saturating_sub(64).clamp(1, MAX_PIECE);
        let mut sent = 0;
        while src.left() > 0 {
            if let Err(e) = self.wait_room(nonblock, deadline) {
                return if sent > 0 { Ok(sent) } else { Err(e) };
            }
            let data = match src.take(src.left().min(piece_max)) {
                Ok(d) => d,
                Err(e) => return if sent > 0 { Ok(sent) } else { Err(e) },
            };
            let n = data.len();
            if n == 0 {
                break;
            }
            let charge = n + OVERHEAD;
            self.charge(charge, sndbuf);
            let msg = Msg { id: NEXT_MSG.fetch_add(1, Ordering::Relaxed), data: Arc::new(data), off: 0, fds: core::mem::take(&mut fds), cred, from: name.clone(), sender: self.clone(), charge };
            let refused = {
                let mut p = peer.inner.lock();
                if p.dead || p.shut & RCV != 0 {
                    Some(msg)
                } else {
                    p.bytes += n;
                    p.queue.push_back(msg);
                    peer.qlen.store(p.queue.len(), Ordering::Release);
                    peer.changed(&mut p, true);
                    None
                }
            };
            if let Some(msg) = refused {
                drop(msg);
                return if sent > 0 { Ok(sent) } else { Err(EPIPE) };
            }
            sent += n;
        }
        Ok(sent)
    }

    fn send_dgram(self: &Arc<Self>, src: &mut Source, to: Option<Arc<Sock>>, fds: Vec<Passed>, cred: Cred, nonblock: bool) -> Result<usize, i64> {
        let len = src.left();
        let (sndbuf, timeout, passcred) = {
            let i = self.inner.lock();
            (self.sndbuf.load(Ordering::Acquire), i.sndtimeo, i.passcred)
        };
        if len > sndbuf.saturating_sub(32) {
            return Err(EMSGSIZE);
        }
        // A receiver that asks for credentials learns the sender's name.
        if passcred {
            self.autobind()?;
        }
        let (target, default) = {
            let i = self.inner.lock();
            if i.shut & SEND != 0 {
                return Err(EPIPE);
            }
            match to {
                Some(t) => (t, false),
                None => match &i.peer {
                    Some(p) => (p.clone(), true),
                    None => return Err(ENOTCONN),
                },
            }
        };
        if target.ty != self.ty {
            return Err(EPROTOTYPE);
        }
        let deadline = deadline(timeout);
        self.wait_room(nonblock, deadline)?;
        let data = if len == 0 { Vec::new() } else { src.take(len)? };
        let n = data.len();
        let charge = n + OVERHEAD;
        self.charge(charge, sndbuf);
        let name = self.name();
        let mut msg = Some(Msg { id: NEXT_MSG.fetch_add(1, Ordering::Relaxed), data: Arc::new(data), off: 0, fds, cred, from: name, sender: self.clone(), charge });
        loop {
            let seen;
            {
                let mut t = target.inner.lock();
                let err = if t.dead {
                    Some(if self.ty == SEQPACKET { EPIPE } else { ECONNREFUSED })
                } else if self.ty == DGRAM && t.peer.as_ref().is_some_and(|p| !Arc::ptr_eq(p, self)) {
                    Some(EPERM)
                } else if t.shut & RCV != 0 {
                    Some(EPIPE)
                } else {
                    None
                };
                if let Some(e) = err {
                    drop(t);
                    drop(msg);
                    if e == ECONNREFUSED && default {
                        // The default destination is gone: forget it.
                        let mut i = self.inner.lock();
                        if i.peer.as_ref().is_some_and(|p| Arc::ptr_eq(p, &target)) {
                            i.state = State::Unconnected;
                            self.set_peer(&mut i, None);
                        }
                    }
                    return Err(e);
                }
                if self.ty == DGRAM && target.holds_back(self) {
                    seen = target.seq.load(Ordering::Acquire);
                    target.add_waiter(self.me.clone());
                } else {
                    t.bytes += n;
                    t.queue.push_back(msg.take().expect("sent once"));
                    target.qlen.store(t.queue.len(), Ordering::Release);
                    target.changed(&mut t, true);
                    return Ok(n);
                }
            }
            // Held back: the readiness reported may still say writable
            // (others filled the queue since); it says what holds now, and
            // the target wakes it when there is room again.
            self.notify(false);
            if nonblock {
                drop(msg);
                return Err(EAGAIN);
            }
            if let Err(e) = target.wait(seen, deadline) {
                drop(msg);
                return Err(e);
            }
        }
    }

    /// Receives into `dst` (see `Received`, `RecvOpts`).
    pub fn recv(&self, dst: &mut Sink, o: RecvOpts) -> Result<Received, i64> {
        if self.ty == STREAM { self.recv_stream(dst, o) } else { self.recv_dgram(dst, o) }
    }

    fn recv_stream(&self, dst: &mut Sink, o: RecvOpts) -> Result<Received, i64> {
        let want = dst.room();
        let target = if o.waitall { want } else { want.min(1) };
        let mut out = Received { copied: 0, len: 0, trunc: false, fds: Fds::Taken(Vec::new()), cred: None, from: None };
        let mut gone: Vec<Msg> = Vec::new();
        let deadline = deadline(self.inner.lock().rcvtimeo);
        let mut result = Ok(());
        // MSG_PEEK goes through the queue: the last piece peeked.
        let mut peeked: Option<u64> = None;
        let mut reader = Some(self.rlock.lock()?);
        loop {
            // The next piece, or why there is none (under the lock).
            let next = {
                let mut i = self.inner.lock();
                if i.state != State::Connected {
                    result = Err(EINVAL);
                    break;
                }
                let piece = match peeked {
                    Some(id) => i.queue.iter().skip_while(|m| m.id != id).nth(1),
                    None => i.queue.front(),
                };
                match piece {
                    Some(m) if want > 0 && dst.room() > 0 => {
                        // Never glue pieces of different writers.
                        if i.passcred && out.cred.is_some_and(|c| c != m.cred) {
                            break;
                        }
                        Next::Piece(Plan { id: m.id, data: m.data.clone(), off: m.off, fds: !m.fds.is_empty(), cred: i.passcred.then_some(m.cred), from: m.from.clone() })
                    }
                    _ => {
                        if out.copied >= target || want == 0 || dst.room() == 0 || (o.peek && out.copied > 0) {
                            break;
                        }
                        if i.err != 0 {
                            result = Err(core::mem::take(&mut i.err));
                            self.changed(&mut i, false);
                            break;
                        }
                        if i.shut & RCV != 0 {
                            break;
                        }
                        if o.nonblock {
                            result = Err(EAGAIN);
                            break;
                        }
                        Next::Wait(self.seq.load(Ordering::Acquire))
                    }
                }
            };
            let plan = match next {
                Next::Piece(plan) => plan,
                Next::Wait(seen) => {
                    // Not holding the other readers off while it sleeps.
                    reader = None;
                    if let Err(e) = self.wait(seen, deadline) {
                        if out.copied == 0 {
                            result = Err(e);
                        }
                        break;
                    }
                    reader = Some(self.rlock.lock()?);
                    continue;
                }
            };
            // The copy, with no lock held: a fault may wait for the pager.
            let n = match dst.put(&plan.data[plan.off..]) {
                Ok(n) => n,
                Err(e) => {
                    if out.copied == 0 {
                        result = Err(e);
                    }
                    break;
                }
            };
            if out.cred.is_none() {
                out.cred = plan.cred;
            }
            if out.from.is_none() {
                out.from = plan.from.clone();
            }
            // What the copy took leaves the queue (MSG_PEEK: the
            // descriptors are copied), unless the piece went meanwhile (its
            // socket emptied by the collector): its bytes were read.
            {
                let gc = (o.peek && plan.fds).then(crate::scm::hold);
                let mut i = self.inner.lock();
                if o.peek {
                    peeked = Some(plan.id);
                    if let (Some(gc), Some(m)) = (&gc, i.queue.iter().find(|m| m.id == plan.id)) {
                        out.fds = peek_fds(&m.fds, gc, &o);
                    }
                } else if i.queue.front().is_some_and(|m| m.id == plan.id) {
                    let m = i.queue.front_mut().expect("checked");
                    m.off += n;
                    if plan.fds {
                        out.fds = Fds::Taken(core::mem::take(&mut m.fds));
                    }
                    let done = m.left() == 0;
                    i.bytes -= n;
                    if done {
                        gone.extend(i.queue.pop_front());
                    }
                    self.qlen.store(i.queue.len(), Ordering::Release);
                    self.changed(&mut i, false);
                }
            }
            out.copied += n;
            if plan.fds || n < plan.data.len() - plan.off {
                break;
            }
        }
        drop(reader);
        drop(gone);
        out.len = out.copied;
        if out.copied > 0 {
            // Bytes count over an error that came after them.
            return Ok(out);
        }
        result.map(|_| out)
    }

    fn recv_dgram(&self, dst: &mut Sink, o: RecvOpts) -> Result<Received, i64> {
        let deadline = deadline(self.inner.lock().rcvtimeo);
        let mut reader = Some(self.rlock.lock()?);
        loop {
            let next = {
                let mut i = self.inner.lock();
                if self.ty == SEQPACKET && i.state != State::Connected {
                    return Err(ENOTCONN);
                }
                if let Some(m) = i.queue.front() {
                    Next::Piece(Plan { id: m.id, data: m.data.clone(), off: 0, fds: !m.fds.is_empty(), cred: i.passcred.then_some(m.cred), from: m.from.clone() })
                } else {
                    if i.err != 0 {
                        let e = core::mem::take(&mut i.err);
                        self.changed(&mut i, false);
                        return Err(e);
                    }
                    if i.shut & RCV != 0 {
                        return Ok(Received { copied: 0, len: 0, trunc: false, fds: Fds::Taken(Vec::new()), cred: None, from: None });
                    }
                    if o.nonblock {
                        return Err(EAGAIN);
                    }
                    Next::Wait(self.seq.load(Ordering::Acquire))
                }
            };
            let plan = match next {
                Next::Piece(plan) => plan,
                Next::Wait(seen) => {
                    // Not holding the other readers off while it sleeps.
                    drop(reader.take());
                    self.wait(seen, deadline)?;
                    reader = Some(self.rlock.lock()?);
                    continue;
                }
            };
            let len = plan.data.len();
            // The copy, with no lock held (an error leaves the datagram).
            let copied = if len == 0 { 0 } else { dst.put(&plan.data)? };
            let mut out = Received { copied, len, trunc: copied < len, fds: Fds::Taken(Vec::new()), cred: plan.cred, from: plan.from.clone() };
            let gone = {
                let gc = (o.peek && plan.fds).then(crate::scm::hold);
                let mut i = self.inner.lock();
                if o.peek {
                    if let (Some(gc), Some(m)) = (&gc, i.queue.iter().find(|m| m.id == plan.id)) {
                        out.fds = peek_fds(&m.fds, gc, &o);
                    }
                    None
                } else if i.queue.front().is_some_and(|m| m.id == plan.id) {
                    let mut m = i.queue.pop_front().expect("checked");
                    out.fds = Fds::Taken(core::mem::take(&mut m.fds));
                    i.bytes -= len;
                    self.qlen.store(i.queue.len(), Ordering::Release);
                    self.changed(&mut i, false);
                    Some(m)
                } else {
                    // It went meanwhile (the socket emptied): read anyway.
                    None
                }
            };
            drop(reader);
            if !o.peek {
                drop(gone);
                self.wake_waiters();
            }
            return Ok(out);
        }
    }

    // Options and queries.

    /// FIONREAD: unread bytes (a datagram socket: the next datagram's).
    pub fn inq(&self) -> Result<i64, i64> {
        let i = self.inner.lock();
        if i.state == State::Listening {
            return Err(EINVAL);
        }
        Ok(if self.ty == DGRAM { i.queue.front().map_or(0, |m| m.data.len()) } else { i.bytes } as i64)
    }

    /// SIOCOUTQ: what its unread messages elsewhere are charged.
    pub fn outq(&self) -> i64 {
        self.wmem.load(Ordering::Acquire) as i64
    }

    pub fn listening(&self) -> bool {
        self.inner.lock().state == State::Listening
    }

    pub fn connected(&self) -> bool {
        self.inner.lock().state == State::Connected
    }

    pub fn is_bound(&self) -> bool {
        self.inner.lock().name.is_some()
    }

    pub fn take_error(&self) -> i64 {
        let mut i = self.inner.lock();
        let e = core::mem::take(&mut i.err);
        if e != 0 {
            self.changed(&mut i, false);
        }
        e
    }

    pub fn passcred(&self) -> bool {
        self.inner.lock().passcred
    }

    pub fn set_passcred(&self, on: bool) {
        self.inner.lock().passcred = on;
    }

    pub fn peercred(&self) -> Cred {
        self.inner.lock().peercred.unwrap_or(Cred::NONE)
    }

    pub fn bufs(&self) -> (usize, usize) {
        let i = self.inner.lock();
        (self.sndbuf.load(Ordering::Acquire), i.rcvbuf)
    }

    /// SO_SNDBUF and SO_RCVBUF: twice the value asked for, within bounds,
    /// as Linux keeps them.
    pub fn set_sndbuf(&self, v: usize) {
        let mut i = self.inner.lock();
        self.sndbuf.store((v.min(MAX_BUF) * 2).max(MIN_SNDBUF), Ordering::Release);
        self.changed(&mut i, true);
    }

    pub fn set_rcvbuf(&self, v: usize) {
        let mut i = self.inner.lock();
        i.rcvbuf = (v.min(MAX_BUF) * 2).max(MIN_RCVBUF);
    }

    /// SO_RCVTIMEO and SO_SNDTIMEO in nanoseconds.
    pub fn timeouts(&self) -> (u64, u64) {
        let i = self.inner.lock();
        (i.rcvtimeo, i.sndtimeo)
    }

    pub fn opt(&self, name: u64) -> Option<i32> {
        self.inner.lock().opts.get(&name).copied()
    }

    pub fn set_opt(&self, name: u64, value: i32) {
        self.inner.lock().opts.insert(name, value);
    }

    pub fn set_timeout(&self, send: bool, ns: u64) {
        let mut i = self.inner.lock();
        if send {
            i.sndtimeo = ns;
        } else {
            i.rcvtimeo = ns;
        }
    }
}
