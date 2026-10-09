//! The socket protocol of the data plane (phase R7b, ADR 0008): what the
//! Linux server (the client) and netd (the service) share over a channel
//! (`ring::channel`) for the instance's internet sockets. Kept apart from
//! both so that its encodings and memory ordering are tested on the host.
//!
//! **The channel** has `SLOTS` slots per ring and a shared area of
//! `SHARED_PAGES` pages (`SharedArea`): a header page with two bitmaps and
//! the net thread's doorbell, then a control block (`Ctl`, 128 bytes) per
//! socket, at most `MAX_SOCKETS`. The client names a socket by the index
//! of its control block. The shared area stays mapped in netd whatever the
//! client does, so netd may use atomics there; granted memory it touches
//! only with its fault-surviving copy.
//!
//! **Data** lives in the client's granted memory: a socket's *area* is a
//! receive ring followed by a send ring of `size` bytes each (`Area`).
//! Positions run freely modulo 2^32; the receive ring's are `rx_tail`
//! (netd's, the bytes it put in) and `rx_head` (the client's, the bytes it
//! took), the send ring's `tx_tail` (the client's) and `tx_head` (netd's).
//! A producer copies its bytes, then publishes its position; the consumer
//! reads the position, copies, then publishes its own (SeqCst throughout:
//! the wake protocols below are Dekker's). Each side keeps its own position
//! and checks the other's: `fill` beyond the ring's size is a protocol
//! violation (netd aborts the socket, the client fails it).
//!
//! - A stream's receive ring holds bytes; a datagram socket's holds
//!   *records* (`Record`: a 16-byte header with the length and the source,
//!   then the data, the next record 16-byte aligned; records wrap around
//!   the end as bytes do). netd puts in only whole records.
//! - A stream's send ring holds bytes; a datagram is sent by `SEND` from the
//!   start of the send ring (which is a plain buffer for a datagram socket).
//!
//! **Waking.** Each control block has an event counter `seq` that both
//! sides advance after a change, and a count of the client's threads that
//! wait on it (`waiters`): a waiter reads `seq` before it looks at the
//! state, announces itself, and sleeps on `seq` with the value it read (a
//! futex: no sleep if `seq` moved); a changer publishes, advances `seq` and
//! wakes if anyone waits (`Ctl::changed`, `Ctl::sleep`). Changes netd makes
//! are also marked in the *client bitmap* for the client's net thread
//! (which reports readiness to the kernel), woken through `client_seq` if
//! it announced a sleep (`ArenaHeader::mark_client`, `wake_client`,
//! `sleep_client`). Changes the client makes for netd (bytes appended,
//! room made in a receive ring netd waits for, `rx_wait`) are marked in
//! the *service bitmap*, and the client rings the submission ring's
//! doorbell (`RingMemory::ring_doorbell`); netd looks at the bitmap after
//! announcing its sleep in the submission ring.
//!
//! **Requests** (`Request`) are the rare operations; netd answers each at
//! once (nothing waits in netd), with a `ring::Completion`:
//!
//! | op | request | completion |
//! |----|---------|------------|
//! | `SOCKET` | `object` socket, `arg[0]` kind, the area (datagram sockets: required; TCP: none) | 0 |
//! | `BIND` | socket, `offset` endpoint (address 0: any; port 0: an ephemeral one), `arg[0]` `BIND_REUSEADDR` | v0 = the port |
//! | `LISTEN` | socket, `arg[0]` backlog, `arg[1]` `BIND_REUSEADDR` (as at listen) | 0 |
//! | `CONNECT` | socket, endpoint; TCP: the area (unless it has one) | 0 once started (TCP: the outcome comes in the control block); a datagram socket's peer is set (endpoint 0: none) |
//! | `ACCEPT` | listener, `arg[0]` the new socket, its area | v0 = peer address, v1 = peer port; `EAGAIN` |
//! | `SEND` | socket, endpoint (0: the peer), `len` bytes at the start of the send ring | bytes; `EAGAIN` if smoltcp's buffer is full |
//! | `SHUTDOWN` | socket, `arg[0]` `SHUT_RD` / `SHUT_WR` bits | 0 (`SHUT_WR`: FIN after what the send ring holds) |
//! | `CLOSE` | socket, `arg[0]` `CLOSE_ABORT` | 0 once netd uses neither its control block nor its area |
//! | `NAME` | socket, `arg[0]` 1 for the peer | v0 = address, v1 = port |
//! | `SETOPT` | socket, `arg[0]` option (`opt`; another is `ENOPROTOOPT`), `arg[1]` value (in its range: `opt::valid`, else `EINVAL`) | 0 |
//! | `LINKS` | a buffer | bytes of `Link` records |
//! | `FORGET` | `grant` | 0 once netd let go of the grant |
//!
//! Every field an operation does not use must be 0 (`EINVAL`), an unknown
//! operation is `ENOSYS`. An endpoint is an IPv4 address and a port in host
//! order, packed as `address | port << 32`.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
pub use ring::{Completion, Desc, Wait};

/// The service's IPC name.
pub const SERVICE: &str = "net";
/// Slots per ring: requests in flight per instance.
pub const SLOTS: u32 = 64;
/// Sockets per channel (control blocks).
pub const MAX_SOCKETS: usize = 1024;
pub const PAGE: usize = 4096;
/// Bytes of a control block.
pub const CTL_BYTES: usize = 128;
/// Pages of the shared area: the header page, then the control blocks.
pub const SHARED_PAGES: u32 = (1 + MAX_SOCKETS * CTL_BYTES / PAGE) as u32;
/// Words of a bitmap of sockets.
pub const BITMAP_WORDS: usize = MAX_SOCKETS / 64;
/// A ring's size: a power of two in this range.
pub const MIN_RING: u32 = 4096;
pub const MAX_RING: u32 = 1 << 20;
/// The most bytes `LINKS` writes.
pub const MAX_LINKS_BUF: u32 = 64 * 1024;

pub mod op {
    pub const SOCKET: u16 = 1;
    pub const BIND: u16 = 2;
    pub const LISTEN: u16 = 3;
    pub const CONNECT: u16 = 4;
    pub const ACCEPT: u16 = 5;
    pub const SEND: u16 = 6;
    pub const SHUTDOWN: u16 = 7;
    pub const CLOSE: u16 = 8;
    pub const NAME: u16 = 9;
    pub const SETOPT: u16 = 10;
    pub const LINKS: u16 = 11;
    pub const FORGET: u16 = 12;
}

/// The errors the protocol itself gives (the others are the stack's).
pub mod errno {
    pub const EBADF: i64 = 9;
    pub const EAGAIN: i64 = 11;
    pub const ENOMEM: i64 = 12;
    pub const EFAULT: i64 = 14;
    pub const EBUSY: i64 = 16;
    pub const EINVAL: i64 = 22;
    pub const ENOSPC: i64 = 28;
    pub const EPIPE: i64 = 32;
    pub const ENOSYS: i64 = 38;
    pub const EDESTADDRREQ: i64 = 89;
    pub const EMSGSIZE: i64 = 90;
    pub const ENOPROTOOPT: i64 = 92;
    pub const EOPNOTSUPP: i64 = 95;
    pub const EADDRINUSE: i64 = 98;
    pub const EADDRNOTAVAIL: i64 = 99;
    pub const ENETUNREACH: i64 = 101;
    pub const ECONNRESET: i64 = 104;
    pub const ENOBUFS: i64 = 105;
    pub const EISCONN: i64 = 106;
    pub const ENOTCONN: i64 = 107;
    pub const ETIMEDOUT: i64 = 110;
    pub const ECONNREFUSED: i64 = 111;
    pub const EALREADY: i64 = 114;
    pub const EIO: i64 = 5;
}
use errno::*;

/// What a socket is (`SOCKET`'s `arg[0]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Tcp = 1,
    Udp = 2,
    /// Raw ICMP (SOCK_RAW, IPPROTO_ICMP): sends ICMP messages (netd adds
    /// the IPv4 header), receives whole IPv4 packets carrying ICMP.
    RawIcmp = 3,
}

impl Kind {
    pub fn from_u64(v: u64) -> Option<Kind> {
        match v {
            1 => Some(Kind::Tcp),
            2 => Some(Kind::Udp),
            3 => Some(Kind::RawIcmp),
            _ => None,
        }
    }

    /// Whether its receive ring holds records.
    pub fn datagrams(self) -> bool {
        self != Kind::Tcp
    }
}

/// `BIND`'s flags.
pub const BIND_REUSEADDR: u64 = 1;
/// `SHUTDOWN`'s bits.
pub const SHUT_RD: u64 = 1;
pub const SHUT_WR: u64 = 2;
/// `CLOSE`'s flags: reset the connection instead of finishing it (unread
/// data, `SO_LINGER` with a zero time).
pub const CLOSE_ABORT: u64 = 1;

/// `SETOPT`'s options.
pub mod opt {
    /// 1: Nagle's algorithm off (TCP_NODELAY), 0: on.
    pub const NODELAY: u32 = 1;
    /// The idle time before keep-alive probes in milliseconds, 0: off
    /// (SO_KEEPALIVE with TCP_KEEPIDLE: whole seconds, 1..=32767, as
    /// Linux takes them).
    pub const KEEPALIVE: u32 = 2;
    /// The hop limit of what it sends, 1..=255 (IP_TTL).
    pub const TTL: u32 = 3;

    /// Whether `value` is valid for `opt` (an unknown option is not).
    pub fn valid(opt: u32, value: u64) -> bool {
        match opt {
            NODELAY => value <= 1,
            KEEPALIVE => value == 0 || (1000..=32_767_000).contains(&value),
            TTL => (1..=255).contains(&value),
            _ => false,
        }
    }
}

/// The state bits netd publishes in a control block (`NetdLine::state`).
pub mod state {
    /// Listening (TCP).
    pub const LISTENING: u32 = 1;
    /// A connect is under way (TCP: the handshake has not finished).
    pub const CONNECTING: u32 = 2;
    /// It was connected (stays set; TCP).
    pub const ESTABLISHED: u32 = 4;
    /// It can still send (TCP: neither its FIN went nor the connection).
    pub const SEND_OPEN: u32 = 8;
    /// No byte beyond `rx_tail` will come (TCP: the peer's FIN, or the
    /// connection is gone).
    pub const RECV_EOF: u32 = 16;
    /// The connection is over (closed, reset, or the connect failed).
    pub const CLOSED: u32 = 32;
    /// A datagram of the largest size fits smoltcp's send buffer now.
    pub const WRITABLE: u32 = 64;
}

/// A word on a cache line of its own.
#[repr(C, align(64))]
pub struct Line(pub AtomicU32);

/// A bit per socket.
#[repr(C, align(64))]
pub struct Bitmap(pub [AtomicU64; BITMAP_WORDS]);

impl Bitmap {
    /// Sets socket `i`'s bit (an index beyond `MAX_SOCKETS` is ignored).
    pub fn set(&self, i: usize) {
        if let Some(w) = self.0.get(i / 64) {
            w.fetch_or(1 << (i % 64), SeqCst);
        }
    }

    /// Takes every set bit, calling `f` for each socket; true if any.
    pub fn take(&self, mut f: impl FnMut(usize)) -> bool {
        let mut any = false;
        for (k, w) in self.0.iter().enumerate() {
            if w.load(SeqCst) == 0 {
                continue;
            }
            let mut bits = w.swap(0, SeqCst);
            while bits != 0 {
                any = true;
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                f(k * 64 + b);
            }
        }
        any
    }

    pub fn any(&self) -> bool {
        self.0.iter().any(|w| w.load(SeqCst) != 0)
    }
}

/// The shared area's first page.
#[repr(C, align(4096))]
pub struct ArenaHeader {
    /// Advanced by netd when it wakes the net thread, and by the client
    /// when it has work for it; the net thread sleeps on it.
    pub client_seq: Line,
    /// The net thread announced a sleep.
    pub client_sleeping: Line,
    /// Sockets netd changed, for the net thread.
    pub client_dirty: Bitmap,
    /// Sockets the client changed, for netd.
    pub service_dirty: Bitmap,
}

impl ArenaHeader {
    /// netd: socket `i` changed (the net thread looks after `wake_client`).
    pub fn mark_client(&self, i: usize) {
        self.client_dirty.set(i);
    }

    /// netd, after marking (once per round): wakes the net thread if it
    /// announced a sleep. Either the thread sees the marks after its
    /// announcement, or this sees the announcement.
    pub fn wake_client(&self, w: &impl Wait) {
        if self.client_sleeping.0.load(SeqCst) != 0 {
            self.client_seq.0.fetch_add(1, SeqCst);
            w.wake(&self.client_seq.0);
        }
    }

    /// The client's own work for the net thread (made known elsewhere):
    /// wakes it whether or not it announced a sleep.
    pub fn poke_client(&self, w: &impl Wait) {
        self.client_seq.0.fetch_add(1, SeqCst);
        w.wake(&self.client_seq.0);
    }

    /// The net thread: takes the marked sockets; true if any.
    pub fn take_client(&self, f: impl FnMut(usize)) -> bool {
        self.client_dirty.take(f)
    }

    /// The net thread: the value of `client_seq` to sleep on, read before
    /// it looks for work (marks or its own).
    pub fn client_seen(&self) -> u32 {
        self.client_seq.0.load(SeqCst)
    }

    /// The net thread, having found no work since `seen`: announces its
    /// sleep, looks at the marks once more, and sleeps unless there are
    /// some or `client_seq` moved.
    pub fn sleep_client(&self, seen: u32, w: &impl Wait) {
        self.client_sleeping.0.store(1, SeqCst);
        if !self.client_dirty.any() {
            w.wait(&self.client_seq.0, seen);
        }
        self.client_sleeping.0.store(0, SeqCst);
    }

    /// The client: socket `i` has work for netd (then ring the submission
    /// ring's doorbell).
    pub fn mark_service(&self, i: usize) {
        self.service_dirty.set(i);
    }

    /// netd: takes the sockets the client marked; true if any.
    pub fn take_service(&self, f: impl FnMut(usize)) -> bool {
        self.service_dirty.take(f)
    }

    /// netd, after announcing its sleep in the submission ring: whether
    /// the client marked a socket (then it must not sleep).
    pub fn service_pending(&self) -> bool {
        self.service_dirty.any()
    }
}

/// netd's half of a control block.
#[repr(C, align(64))]
pub struct NetdLine {
    /// The event counter (both sides advance it; waiters sleep on it).
    pub seq: AtomicU32,
    /// `state` bits.
    pub state: AtomicU32,
    /// The receive ring's producer position.
    pub rx_tail: AtomicU32,
    /// The send ring's consumer position.
    pub tx_head: AtomicU32,
    /// The latest error (a positive errno), and how many netd posted: the
    /// client keeps the count it took (SO_ERROR, a failed call).
    pub error: AtomicU32,
    pub err_seq: AtomicU32,
    /// Connections a listener has ready to accept.
    pub backlog: AtomicU32,
    /// 1: netd holds received data the receive ring had no room for (the
    /// client rings after it made room).
    pub rx_wait: AtomicU32,
}

/// The client's half.
#[repr(C, align(64))]
pub struct ClientLine {
    /// The receive ring's consumer position.
    pub rx_head: AtomicU32,
    /// The send ring's producer position.
    pub tx_tail: AtomicU32,
    /// The client's threads waiting on `seq`.
    pub waiters: AtomicU32,
}

/// A socket's control block.
#[repr(C)]
pub struct Ctl {
    pub netd: NetdLine,
    pub client: ClientLine,
}

impl Ctl {
    /// After a change either side made (published before): advances `seq`
    /// and wakes the client's waiters if there are any.
    pub fn changed(&self, w: &impl Wait) {
        self.netd.seq.fetch_add(1, SeqCst);
        if self.client.waiters.load(SeqCst) != 0 {
            w.wake(&self.netd.seq);
        }
    }

    /// The value to sleep on: read before looking at the state.
    pub fn seen(&self) -> u32 {
        self.netd.seq.load(SeqCst)
    }

    /// Sleeps (`sleep(word, seen)`, a futex wait) as a waiter announced to
    /// the changers. Whatever changed after `seen` was read either moved
    /// `seq` (the futex does not sleep) or sees the waiter (and wakes it).
    pub fn sleep<E>(&self, seen: u32, sleep: impl FnOnce(&AtomicU32, u32) -> Result<(), E>) -> Result<(), E> {
        self.client.waiters.fetch_add(1, SeqCst);
        let r = sleep(&self.netd.seq, seen);
        self.client.waiters.fetch_sub(1, SeqCst);
        r
    }

    /// netd's half for a new socket (its counters from 0; `seq` goes on).
    pub fn reset_netd(&self, state: u32) {
        let n = &self.netd;
        for w in [&n.rx_tail, &n.tx_head, &n.error, &n.err_seq, &n.backlog, &n.rx_wait] {
            w.store(0, SeqCst);
        }
        n.state.store(state, SeqCst);
    }

    /// The client's half for a new socket.
    pub fn reset_client(&self) {
        self.client.rx_head.store(0, SeqCst);
        self.client.tx_tail.store(0, SeqCst);
    }
}

/// The shared area: the header page, then the control blocks.
#[repr(C)]
pub struct SharedArea {
    pub header: ArenaHeader,
    pub ctl: [Ctl; MAX_SOCKETS],
}

const _: () = {
    assert!(core::mem::size_of::<Ctl>() == CTL_BYTES);
    assert!(core::mem::size_of::<ArenaHeader>() == PAGE);
    assert!(core::mem::size_of::<SharedArea>() == SHARED_PAGES as usize * PAGE);
    assert!(core::mem::offset_of!(Ctl, client) == 64);
};

impl SharedArea {
    /// The shared area at `ptr`.
    ///
    /// # Safety
    /// `ptr` is the page-aligned start of a mapping of at least
    /// `SHARED_PAGES` pages that lives for `'a` (any contents are a valid
    /// area: a hostile peer cannot make it unsound).
    pub unsafe fn at<'a>(ptr: *const u8) -> &'a SharedArea {
        unsafe { &*(ptr as *const SharedArea) }
    }

    /// Control block `i` (None beyond `MAX_SOCKETS`).
    pub fn ctl(&self, i: usize) -> Option<&Ctl> {
        self.ctl.get(i)
    }
}

/// A socket's buffers in a grant: the receive ring at `offset`, the send
/// ring right after it, `size` bytes each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub grant: u32,
    pub offset: u32,
    pub size: u32,
}

impl Area {
    pub fn rx(&self) -> u64 {
        self.offset as u64
    }

    pub fn tx(&self) -> u64 {
        self.offset as u64 + self.size as u64
    }

    /// The byte after the area, in the grant.
    pub fn end(&self) -> u64 {
        self.offset as u64 + 2 * self.size as u64
    }

    /// From a descriptor's buffer fields: None for none (all 0), EINVAL for
    /// a malformed one (sizes not a power of two in range, an offset not
    /// page-aligned).
    fn from_desc(d: &Desc) -> Result<Option<Area>, i64> {
        if d.grant == 0 && d.buf_off == 0 && d.len == 0 {
            return Ok(None);
        }
        let size = d.len / 2;
        if d.grant == 0 || d.len % 2 != 0 || !size.is_power_of_two() || !(MIN_RING..=MAX_RING).contains(&size) || d.buf_off as usize % PAGE != 0 {
            return Err(EINVAL);
        }
        Ok(Some(Area { grant: d.grant, offset: d.buf_off, size }))
    }

    fn to_desc(area: Option<Area>, d: &mut Desc) {
        if let Some(a) = area {
            d.grant = a.grant;
            d.buf_off = a.offset;
            d.len = 2 * a.size;
        }
    }
}

/// A byte range of a grant (`LINKS`' result buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buf {
    pub grant: u32,
    pub offset: u32,
    pub len: u32,
}

/// An IPv4 endpoint in host order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: u32,
    pub port: u16,
}

impl Endpoint {
    pub fn pack(&self) -> u64 {
        self.addr as u64 | (self.port as u64) << 32
    }

    pub fn unpack(v: u64) -> Result<Endpoint, i64> {
        if v >> 48 != 0 {
            return Err(EINVAL);
        }
        Ok(Endpoint { addr: v as u32, port: (v >> 32) as u16 })
    }
}

/// A request, validated (`decode`) or to be sent (`encode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Socket { sock: u32, kind: Kind, area: Option<Area> },
    Bind { sock: u32, at: Endpoint, reuse: bool },
    /// `reuse`: SO_REUSEADDR as it is at listen (Linux reads it then).
    Listen { sock: u32, backlog: u32, reuse: bool },
    Connect { sock: u32, to: Endpoint, area: Option<Area> },
    Accept { sock: u32, new: u32, area: Area },
    Send { sock: u32, to: Endpoint, len: u32 },
    Shutdown { sock: u32, how: u64 },
    Close { sock: u32, abort: bool },
    Name { sock: u32, peer: bool },
    SetOpt { sock: u32, opt: u32, value: u64 },
    Links { buf: Buf },
    Forget { grant: u32 },
}

fn sock(object: u64) -> Result<u32, i64> {
    if object >= MAX_SOCKETS as u64 {
        return Err(EINVAL);
    }
    Ok(object as u32)
}

/// The fields of a `Desc` an operation reads, for the check that the
/// others are 0.
#[derive(Clone, Copy)]
struct Used {
    object: bool,
    offset: bool,
    buf: bool,
    grant: bool,
    len: bool,
    args: usize,
}

const NONE: Used = Used { object: false, offset: false, buf: false, grant: false, len: false, args: 0 };

impl Request {
    /// Validates a request copied out of the ring (copied once). A
    /// negative errno to complete it with if it is malformed: ENOSYS for an
    /// unknown operation, EINVAL for a field out of range or set where the
    /// operation takes none. (What netd checks itself: the sockets exist,
    /// the grant holds the area, the kind fits the operation.)
    pub fn decode(d: &Desc) -> Result<Request, i64> {
        let with_sock = Used { object: true, ..NONE };
        let (request, used) = match d.op {
            op::SOCKET => {
                let kind = Kind::from_u64(d.arg[0]).ok_or(EINVAL)?;
                let area = Area::from_desc(d)?;
                // Datagram sockets get their rings at once, TCP at connect
                // or accept (a listener needs none).
                if area.is_some() != kind.datagrams() {
                    return Err(EINVAL);
                }
                (Request::Socket { sock: sock(d.object)?, kind, area }, Used { buf: true, args: 1, ..with_sock })
            }
            op::BIND => {
                if d.arg[0] & !BIND_REUSEADDR != 0 {
                    return Err(EINVAL);
                }
                let r = Request::Bind { sock: sock(d.object)?, at: Endpoint::unpack(d.offset)?, reuse: d.arg[0] != 0 };
                (r, Used { offset: true, args: 1, ..with_sock })
            }
            op::LISTEN => {
                let backlog = u32::try_from(d.arg[0]).map_err(|_| EINVAL)?;
                if d.arg[1] & !BIND_REUSEADDR != 0 {
                    return Err(EINVAL);
                }
                (Request::Listen { sock: sock(d.object)?, backlog, reuse: d.arg[1] != 0 }, Used { args: 2, ..with_sock })
            }
            op::CONNECT => {
                let r = Request::Connect { sock: sock(d.object)?, to: Endpoint::unpack(d.offset)?, area: Area::from_desc(d)? };
                (r, Used { offset: true, buf: true, ..with_sock })
            }
            op::ACCEPT => {
                let area = Area::from_desc(d)?.ok_or(EINVAL)?;
                let new = sock(d.arg[0])?;
                let listener = sock(d.object)?;
                if new == listener {
                    return Err(EINVAL);
                }
                (Request::Accept { sock: listener, new, area }, Used { buf: true, args: 1, ..with_sock })
            }
            op::SEND => {
                if d.len > MAX_RING {
                    return Err(EINVAL);
                }
                let r = Request::Send { sock: sock(d.object)?, to: Endpoint::unpack(d.offset)?, len: d.len };
                (r, Used { offset: true, len: true, ..with_sock })
            }
            op::SHUTDOWN => {
                let how = d.arg[0];
                if how == 0 || how & !(SHUT_RD | SHUT_WR) != 0 {
                    return Err(EINVAL);
                }
                (Request::Shutdown { sock: sock(d.object)?, how }, Used { args: 1, ..with_sock })
            }
            op::CLOSE => {
                if d.arg[0] & !CLOSE_ABORT != 0 {
                    return Err(EINVAL);
                }
                (Request::Close { sock: sock(d.object)?, abort: d.arg[0] != 0 }, Used { args: 1, ..with_sock })
            }
            op::NAME => {
                if d.arg[0] > 1 {
                    return Err(EINVAL);
                }
                (Request::Name { sock: sock(d.object)?, peer: d.arg[0] == 1 }, Used { args: 1, ..with_sock })
            }
            op::SETOPT => {
                let opt = u32::try_from(d.arg[0]).map_err(|_| ENOPROTOOPT)?;
                if !matches!(opt, opt::NODELAY | opt::KEEPALIVE | opt::TTL) {
                    return Err(ENOPROTOOPT);
                }
                // A value out of range never reaches the stack (a keep-alive
                // interval of a few milliseconds would make it probe every
                // round).
                if !opt::valid(opt, d.arg[1]) {
                    return Err(EINVAL);
                }
                (Request::SetOpt { sock: sock(d.object)?, opt, value: d.arg[1] }, Used { args: 2, ..with_sock })
            }
            op::LINKS => {
                if d.grant == 0 || d.len == 0 || d.len > MAX_LINKS_BUF {
                    return Err(EINVAL);
                }
                (Request::Links { buf: Buf { grant: d.grant, offset: d.buf_off, len: d.len } }, Used { buf: true, ..NONE })
            }
            op::FORGET => {
                if d.grant == 0 {
                    return Err(EINVAL);
                }
                (Request::Forget { grant: d.grant }, Used { grant: true, ..NONE })
            }
            _ => return Err(ENOSYS),
        };
        let unused = d.flags != 0
            || (!used.object && d.object != 0)
            || (!used.offset && d.offset != 0)
            || (!used.buf && !used.grant && d.grant != 0)
            || (!used.buf && d.buf_off != 0)
            || (!used.buf && !used.len && d.len != 0)
            || d.arg[used.args..].iter().any(|&a| a != 0);
        if unused {
            return Err(EINVAL);
        }
        Ok(request)
    }

    /// The request as a descriptor with tag `tag`.
    pub fn encode(&self, tag: u64) -> Desc {
        let mut d = Desc { op: self.op(), tag, ..Desc::default() };
        match *self {
            Request::Socket { sock, kind, area } => {
                d.object = sock as u64;
                d.arg[0] = kind as u64;
                Area::to_desc(area, &mut d);
            }
            Request::Bind { sock, at, reuse } => {
                d.object = sock as u64;
                d.offset = at.pack();
                d.arg[0] = if reuse { BIND_REUSEADDR } else { 0 };
            }
            Request::Listen { sock, backlog, reuse } => {
                d.object = sock as u64;
                d.arg = [backlog as u64, if reuse { BIND_REUSEADDR } else { 0 }, 0];
            }
            Request::Connect { sock, to, area } => {
                d.object = sock as u64;
                d.offset = to.pack();
                Area::to_desc(area, &mut d);
            }
            Request::Accept { sock, new, area } => {
                d.object = sock as u64;
                d.arg[0] = new as u64;
                Area::to_desc(Some(area), &mut d);
            }
            Request::Send { sock, to, len } => {
                d.object = sock as u64;
                d.offset = to.pack();
                d.len = len;
            }
            Request::Shutdown { sock, how } => {
                d.object = sock as u64;
                d.arg[0] = how;
            }
            Request::Close { sock, abort } => {
                d.object = sock as u64;
                d.arg[0] = if abort { CLOSE_ABORT } else { 0 };
            }
            Request::Name { sock, peer } => {
                d.object = sock as u64;
                d.arg[0] = peer as u64;
            }
            Request::SetOpt { sock, opt, value } => {
                d.object = sock as u64;
                d.arg = [opt as u64, value, 0];
            }
            Request::Links { buf } => {
                d.grant = buf.grant;
                d.buf_off = buf.offset;
                d.len = buf.len;
            }
            Request::Forget { grant } => d.grant = grant,
        }
        d
    }

    pub fn op(&self) -> u16 {
        match self {
            Request::Socket { .. } => op::SOCKET,
            Request::Bind { .. } => op::BIND,
            Request::Listen { .. } => op::LISTEN,
            Request::Connect { .. } => op::CONNECT,
            Request::Accept { .. } => op::ACCEPT,
            Request::Send { .. } => op::SEND,
            Request::Shutdown { .. } => op::SHUTDOWN,
            Request::Close { .. } => op::CLOSE,
            Request::Name { .. } => op::NAME,
            Request::SetOpt { .. } => op::SETOPT,
            Request::Links { .. } => op::LINKS,
            Request::Forget { .. } => op::FORGET,
        }
    }
}

/// A resource netd shares among the instances (bytes of socket buffers,
/// smoltcp sockets, records of ports in TIME-WAIT, channels): at most
/// `limit` in all. Each *active* instance (one with a channel to netd) is
/// guaranteed `reserve` of it: what others take never cuts into what an
/// active instance holds below its reserve. Beyond the reserves the
/// resource goes to whoever asks first, so one instance alone may use
/// nearly all of it, and n active instances can each count on `reserve`
/// whatever the others do (the fair share scales with the number of
/// active instances; `limit` must cover the reserves of as many as there
/// can be channels). With `for_instances(n)` the reserves of the n - k
/// instances that may still come (k active now) are kept too, so one that
/// connects later finds its reserve whatever the others took. `cap` bounds
/// what one instance holds, whatever number of channels it opened.
/// Charged before anything is allocated, given back to the instance that
/// was charged.
#[derive(Debug, Default)]
pub struct Budget {
    limit: usize,
    reserve: usize,
    cap: usize,
    used: usize,
    /// The reserves of active instances not yet used: what nobody else may
    /// take.
    reserved: usize,
    /// The instances there can be at once (with a channel), and those
    /// active now: the reserves of the others are kept for them.
    slots: usize,
    active: usize,
    /// What each instance holds and how often it is active (instances
    /// neither holding anything nor active are not kept).
    owners: alloc::collections::BTreeMap<u64, Owner>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Owner {
    held: usize,
    active: u32,
}

impl Owner {
    /// What of `reserve` this owner has not used yet (none if inactive).
    fn unused(&self, reserve: usize) -> usize {
        if self.active > 0 { reserve.saturating_sub(self.held) } else { 0 }
    }
}

impl Budget {
    pub const fn new(limit: usize, reserve: usize, cap: usize) -> Budget {
        Budget { limit, reserve, cap, used: 0, reserved: 0, slots: 0, active: 0, owners: alloc::collections::BTreeMap::new() }
    }

    /// Keeps the reserves of instances that are not active yet, of `slots`
    /// at most at once.
    pub const fn for_instances(mut self, slots: usize) -> Budget {
        self.slots = slots;
        self
    }

    fn owner(&self, owner: u64) -> Owner {
        self.owners.get(&owner).copied().unwrap_or_default()
    }

    /// Changes `owner`'s entry by `f`, keeping `reserved` right.
    fn update(&mut self, owner: u64, f: impl FnOnce(&mut Owner)) {
        let mut o = self.owner(owner);
        self.reserved -= o.unused(self.reserve);
        let was = o.active > 0;
        f(&mut o);
        self.reserved += o.unused(self.reserve);
        match (was, o.active > 0) {
            (false, true) => self.active += 1,
            (true, false) => self.active -= 1,
            _ => {}
        }
        if o.held == 0 && o.active == 0 {
            self.owners.remove(&owner);
        } else {
            self.owners.insert(owner, o);
        }
    }

    /// `owner` got a channel: its reserve is kept for it from now on.
    pub fn activate(&mut self, owner: u64) {
        self.update(owner, |o| o.active += 1);
    }

    /// `owner` gave up a channel (its reserve goes with its last).
    pub fn deactivate(&mut self, owner: u64) {
        self.update(owner, |o| o.active = o.active.saturating_sub(1));
    }

    /// What `owner` may still take: what is free beyond the others'
    /// unused reserves (those of instances still to come too), within its
    /// cap.
    pub fn room(&self, owner: u64) -> usize {
        let o = self.owner(owner);
        let to_come = self.slots.saturating_sub(self.active) * self.reserve;
        let others = self.reserved - o.unused(self.reserve) + to_come;
        self.limit.saturating_sub(self.used + others).min(self.cap.saturating_sub(o.held))
    }

    pub fn held(&self, owner: u64) -> usize {
        self.owner(owner).held
    }


    pub fn used(&self) -> usize {
        self.used
    }

    /// Takes `n` for `owner`, or ENOBUFS (nothing taken).
    pub fn charge(&mut self, owner: u64, n: usize) -> Result<(), i64> {
        if n > self.room(owner) {
            return Err(ENOBUFS);
        }
        self.used += n;
        self.update(owner, |o| o.held += n);
        Ok(())
    }

    /// Gives `n` back from `owner` (never more than it holds: a caller
    /// that gives back more has lost count, which debug builds catch).
    pub fn uncharge(&mut self, owner: u64, n: usize) {
        debug_assert!(n <= self.held(owner), "instance {owner} gives back {n} of {}", self.held(owner));
        let n = n.min(self.held(owner));
        self.used -= n;
        self.update(owner, |o| o.held -= n);
    }
}

/// A socket that holds a port, as netd's port rules see it. `owner` is
/// the instance it belongs to (`ring::channel::Offer::instance`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortHolder {
    pub owner: u64,
    pub port: u16,
    /// Its address (None: any).
    pub addr: Option<u32>,
    pub reuse: bool,
    pub listening: bool,
    /// A connection (connected or accepted): matched by both endpoints,
    /// it never receives what is meant for a new socket.
    pub connected: bool,
    /// Its owner closed it; it only finishes (TIME-WAIT).
    pub closing: bool,
}

/// A claim of a port: a bind, or a listen of a bound socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortClaim {
    pub owner: u64,
    pub port: u16,
    pub addr: Option<u32>,
    pub reuse: bool,
}

fn overlaps(a: Option<u32>, b: Option<u32>) -> bool {
    a.is_none() || b.is_none() || a == b
}

/// Whether a TCP claim conflicts with a holder (EADDRINUSE). Linux's rule
/// (inet_csk_bind_conflict): a socket on an overlapping address conflicts
/// unless both allow reuse (SO_REUSEADDR) and it does not listen; netd
/// checks it at bind and again at listen. Across instances it is stricter:
/// a port another instance holds in any way (bound, listening, a
/// connection, closing or in TIME-WAIT) is never shared, so one instance
/// can neither take over a port another one serves nor learn, by a
/// connect's 4-tuple check, whom another instance talked to from it.
pub fn tcp_port_conflict(claim: &PortClaim, other: &PortHolder) -> bool {
    if claim.port != other.port || !overlaps(claim.addr, other.addr) {
        return false;
    }
    claim.owner != other.owner || !(claim.reuse && other.reuse && !other.listening)
}

/// Whether a UDP claim conflicts with a holder: a socket on an overlapping
/// address conflicts unless both allow reuse and both are the same
/// instance's (a datagram goes to one of them: never to another
/// instance's).
pub fn udp_port_conflict(claim: &PortClaim, other: &PortHolder) -> bool {
    claim.port == other.port && overlaps(claim.addr, other.addr) && !(claim.reuse && other.reuse && claim.owner == other.owner)
}

/// The pieces of the ring range [`pos`, `pos + len`) in a ring of `size`
/// bytes (a power of two): (offset in the ring, length) before the wrap
/// and after it (the second empty unless it wraps). `len` is at most
/// `size`.
pub fn pieces(pos: u32, len: u32, size: u32) -> [(u32, u32); 2] {
    let at = pos & (size - 1);
    let len = len.min(size);
    let first = len.min(size - at);
    [(at, first), (0, len - first)]
}

/// What an ICMP message belongs to, so that netd shows it only to the
/// instance that owns that (`icmp_key`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IcmpKey {
    /// An echo request (`request`) or reply with identifier `id`, or an
    /// error about an echo request: the identifier lies at byte `at` of
    /// the packet, in the ICMP message at byte `message` (whose checksum
    /// covers it: `set_echo_id`), in an error inside the quoted request at
    /// byte `inner` (its checksum covers it too).
    Echo { id: u16, request: bool, at: usize, message: usize, inner: Option<usize> },
    /// An error about a TCP or UDP packet from this (local) port.
    Tcp(u16),
    Udp(u16),
}

/// Replaces the echo identifier at byte `at` of `packet` with `id`, and
/// updates the checksums that cover it (RFC 1624's incremental update):
/// that of the quoted echo request at byte `inner` in an error (whose
/// checksum covers the whole original request: the identifier is the only
/// change), then that of the ICMP message at byte `message` (which covers
/// the identifier and the quoted checksum). Out-of-range offsets change
/// nothing.
pub fn set_echo_id(packet: &mut [u8], at: usize, message: usize, inner: Option<usize>, id: u16) {
    let fits = |o: usize, n: usize| o.checked_add(n).is_some_and(|end| end <= packet.len());
    if !fits(at, 2) || !fits(message, 4) || inner.is_some_and(|i| !fits(i, 4)) {
        return;
    }
    fn word(p: &[u8], o: usize) -> u16 {
        u16::from_be_bytes([p[o], p[o + 1]])
    }
    // HC' = ~(~HC + ~m + m') for each changed word m -> m'.
    fn adjust(sum: u16, changes: &[(u16, u16)]) -> u16 {
        let mut s = (!sum) as u32;
        for &(old, new) in changes {
            s += (!old) as u32 + new as u32;
        }
        while s >> 16 != 0 {
            s = (s & 0xffff) + (s >> 16);
        }
        !(s as u16)
    }
    let old = word(packet, at);
    packet[at..at + 2].copy_from_slice(&id.to_be_bytes());
    let mut changes = [(old, id), (0, 0)];
    let mut n = 1;
    if let Some(i) = inner {
        let before = word(packet, i + 2);
        let after = adjust(before, &changes[..1]);
        packet[i + 2..i + 4].copy_from_slice(&after.to_be_bytes());
        changes[1] = (before, after);
        n = 2;
    }
    let sum = adjust(word(packet, message + 2), &changes[..n]);
    packet[message + 2..message + 4].copy_from_slice(&sum.to_be_bytes());
}

/// The echo identifiers netd puts on the wire: every instance's echo
/// requests get identifiers of their own (`outgoing`), so that a reply
/// (or an error about a request) goes to the instance that sent the
/// request, with the identifier it chose (`incoming`), and no instance can
/// receive another's by choosing the same identifier. An instance keeps
/// at most `ECHO_PER_OWNER` (its least recently used goes first); one
/// unused for `ECHO_IDLE_MS` goes.
#[derive(Debug, Default)]
pub struct EchoIds {
    by_wire: alloc::collections::BTreeMap<u16, EchoId>,
    by_owner: alloc::collections::BTreeMap<(u64, u16), u16>,
    next: u16,
}

#[derive(Debug, Clone, Copy)]
struct EchoId {
    owner: u64,
    local: u16,
    used: u64,
}

pub const ECHO_PER_OWNER: usize = 64;
pub const ECHO_IDLE_MS: u64 = 60_000;

impl EchoIds {
    /// The identifier on the wire for `owner`'s echo request with
    /// identifier `local`, at `now` (milliseconds): the one it had, else
    /// `local` itself if nobody uses it, else a free one.
    pub fn outgoing(&mut self, owner: u64, local: u16, now: u64) -> u16 {
        if let Some(&wire) = self.by_owner.get(&(owner, local)) {
            if let Some(e) = self.by_wire.get_mut(&wire) {
                e.used = now;
            }
            return wire;
        }
        self.expire(now);
        let mine: alloc::vec::Vec<(u64, u16)> = self.by_wire.iter().filter(|(_, e)| e.owner == owner).map(|(&w, e)| (e.used, w)).collect();
        if mine.len() >= ECHO_PER_OWNER {
            if let Some(&(_, oldest)) = mine.iter().min() {
                self.remove(oldest);
            }
        }
        let wire = if self.by_wire.contains_key(&local) {
            // 65536 identifiers, at most ECHO_PER_OWNER for each of the
            // instances with a channel: one is free.
            let mut w = self.next;
            while self.by_wire.contains_key(&w) {
                w = w.wrapping_add(1);
            }
            self.next = w.wrapping_add(1);
            w
        } else {
            local
        };
        self.by_wire.insert(wire, EchoId { owner, local, used: now });
        self.by_owner.insert((owner, local), wire);
        wire
    }

    /// Whose identifier `wire` is: the instance and its own identifier.
    pub fn incoming(&self, wire: u16) -> Option<(u64, u16)> {
        self.by_wire.get(&wire).map(|e| (e.owner, e.local))
    }

    /// Identifiers unused since `ECHO_IDLE_MS` go.
    pub fn expire(&mut self, now: u64) {
        let old: alloc::vec::Vec<u16> = self.by_wire.iter().filter(|(_, e)| now.saturating_sub(e.used) >= ECHO_IDLE_MS).map(|(&w, _)| w).collect();
        for w in old {
            self.remove(w);
        }
    }

    /// An instance that went: its identifiers go.
    pub fn forget(&mut self, owner: u64) {
        let gone: alloc::vec::Vec<u16> = self.by_wire.iter().filter(|(_, e)| e.owner == owner).map(|(&w, _)| w).collect();
        for w in gone {
            self.remove(w);
        }
    }

    pub fn len(&self) -> usize {
        self.by_wire.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_wire.is_empty()
    }

    fn remove(&mut self, wire: u16) {
        if let Some(e) = self.by_wire.remove(&wire) {
            self.by_owner.remove(&(e.owner, e.local));
        }
    }
}

/// The key of `packet`, an IPv4 packet as a raw socket receives it
/// (header included); None if it belongs to nobody in particular or is
/// malformed. Errors (destination unreachable, source quench, redirect,
/// time exceeded, parameter problem) quote the IP header and the first 8
/// bytes of the packet that caused them. Every length is checked: the
/// packet comes from the network.
pub fn icmp_key(packet: &[u8]) -> Option<IcmpKey> {
    /// The payload of an IPv4 header at the start of `p` (`whole`: the
    /// packet's total length must fit, as for a packet received; a quoted
    /// one is cut short).
    fn ipv4_payload(p: &[u8], whole: bool) -> Option<(u8, usize, &[u8])> {
        let first = *p.first()?;
        let ihl = (first as usize & 0xf) * 4;
        if first >> 4 != 4 || ihl < 20 || p.len() < ihl {
            return None;
        }
        let end = if whole {
            let total = u16::from_be_bytes([p[2], p[3]]) as usize;
            if total < ihl || total > p.len() {
                return None;
            }
            total
        } else {
            p.len()
        };
        Some((p[9], ihl, &p[ihl..end]))
    }
    // An echo request or reply: its identifier (at byte 4).
    fn echo_id(m: &[u8], request: Option<bool>) -> Option<u16> {
        let fits = m.len() >= 8 && request.is_none_or(|r| m[0] == if r { 8 } else { 0 });
        (fits && (m[0] == 0 || m[0] == 8)).then(|| u16::from_be_bytes([m[4], m[5]]))
    }
    let (proto, message, icmp) = ipv4_payload(packet, true)?;
    if proto != 1 || icmp.len() < 8 {
        return None;
    }
    match icmp[0] {
        0 | 8 => echo_id(icmp, None).map(|id| IcmpKey::Echo { id, request: icmp[0] == 8, at: message + 4, message, inner: None }),
        3 | 4 | 5 | 11 | 12 => {
            let (proto, ihl, l4) = ipv4_payload(&icmp[8..], false)?;
            let port = || Some(u16::from_be_bytes([*l4.first()?, *l4.get(1)?]));
            match proto {
                // (Only an error about a request of ours is about our
                // identifier.)
                1 => echo_id(l4, Some(true)).map(|id| IcmpKey::Echo { id, request: false, at: message + 8 + ihl + 4, message, inner: Some(message + 8 + ihl) }),
                6 => port().map(IcmpKey::Tcp),
                17 => port().map(IcmpKey::Udp),
                _ => None,
            }
        }
        _ => None,
    }
}

/// How many bytes a ring holds between a consumer and a producer
/// position, None if the producer is more than `size` ahead (or behind:
/// a protocol violation).
pub fn fill(head: u32, tail: u32, size: u32) -> Option<u32> {
    let n = tail.wrapping_sub(head);
    (n <= size).then_some(n)
}

/// A datagram's record in a receive ring: this header, then the data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// Bytes of data.
    pub len: u32,
    /// The source.
    pub from: Endpoint,
}

/// Bytes of a record's header.
pub const RECORD_HEADER: u32 = 16;

impl Record {
    pub fn encode(&self) -> [u8; RECORD_HEADER as usize] {
        let mut b = [0u8; RECORD_HEADER as usize];
        b[0..4].copy_from_slice(&self.len.to_le_bytes());
        b[4..8].copy_from_slice(&self.from.addr.to_le_bytes());
        b[8..10].copy_from_slice(&self.from.port.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; RECORD_HEADER as usize]) -> Record {
        let len = u32::from_le_bytes(b[0..4].try_into().expect("4 bytes"));
        let addr = u32::from_le_bytes(b[4..8].try_into().expect("4 bytes"));
        let port = u16::from_le_bytes([b[8], b[9]]);
        Record { len, from: Endpoint { addr, port } }
    }

    /// Bytes the record takes in the ring (the next starts 16-aligned).
    pub fn span(len: u32) -> u64 {
        RECORD_HEADER as u64 + (len as u64).div_ceil(16) * 16
    }
}

/// A link of the loopback kind: traffic to the host's own addresses.
pub const LINK_LOOPBACK: u16 = 1;
/// An Ethernet link (the network card).
pub const LINK_ETHERNET: u16 = 2;
/// Link state: configured up (it sends and receives).
pub const LINK_UP: u16 = 1;
/// Link state: the medium is there (a carrier).
pub const LINK_RUNNING: u16 = 2;

/// A network interface as netd describes it (`LINKS`). netd speaks IPv4
/// with one address per interface; the names are the Linux server's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Link {
    /// Its number, from 1, stable while netd runs.
    pub index: u32,
    /// `LINK_LOOPBACK` or `LINK_ETHERNET`.
    pub kind: u16,
    /// `LINK_UP`, `LINK_RUNNING`.
    pub state: u16,
    /// The largest IP packet it carries.
    pub mtu: u32,
    /// Its hardware address (zero for the loopback).
    pub mac: [u8; 6],
    /// The IPv4 address's prefix length (with `address` 0: none).
    pub prefix: u8,
    /// Its IPv4 address in host order, 0 for none (no DHCP lease yet).
    pub address: u32,
}

impl Link {
    /// The size of one encoded record.
    pub const SIZE: usize = 24;

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut r = [0u8; Self::SIZE];
        r[0..4].copy_from_slice(&self.index.to_le_bytes());
        r[4..6].copy_from_slice(&self.kind.to_le_bytes());
        r[6..8].copy_from_slice(&self.state.to_le_bytes());
        r[8..12].copy_from_slice(&self.mtu.to_le_bytes());
        r[12..18].copy_from_slice(&self.mac);
        r[18] = self.prefix;
        r[20..24].copy_from_slice(&self.address.to_le_bytes());
        r
    }

    /// The records of a `LINKS` result; a partial record at the end is
    /// ignored.
    pub fn decode_all(payload: &[u8]) -> impl Iterator<Item = Link> + '_ {
        payload.chunks_exact(Self::SIZE).map(|r| Link {
            index: u32::from_le_bytes(r[0..4].try_into().expect("4 bytes")),
            kind: u16::from_le_bytes([r[4], r[5]]),
            state: u16::from_le_bytes([r[6], r[7]]),
            mtu: u32::from_le_bytes(r[8..12].try_into().expect("4 bytes")),
            mac: r[12..18].try_into().expect("6 bytes"),
            prefix: r[18],
            address: u32::from_le_bytes(r[20..24].try_into().expect("4 bytes")),
        })
    }
}
