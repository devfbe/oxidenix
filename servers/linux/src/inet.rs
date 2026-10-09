//! Internet sockets (phase R7b, ADR 0007): TCP, UDP and raw ICMP sockets
//! of the server, each a file with a placeholder in the kernel's
//! descriptor table, as AF_UNIX ones are; netd runs the protocols. This is
//! the sockets' semantics (Linux's, `man 7 tcp`, `udp`, `ip`, `socket`);
//! `inetcalls` is their system calls (addresses, message headers,
//! options), `netclient` the channel to netd.
//!
//! **State** lives in the socket's control block in the channel's shared
//! area (netd's half: the state bits, the rings' positions netd publishes,
//! errors, the accept backlog) and here (`Local`: what the program did to
//! it, shutdowns, the errors it took, options). Readiness (Linux's
//! `tcp_poll`, `udp_poll`) is computed from both and reported under the
//! state lock whenever a call changed it, and by the net thread whenever
//! netd did (`netd_changed`), with an edge for new data or room.
//!
//! **Waiting** is the server's alone (netd answers every request at once):
//! a call reads the block's `seq`, looks at the state, and sleeps on `seq`
//! (`Ctl::sleep`), which netd advances and wakes after every change it
//! publishes, as do the server's own changes (`poke`). Sleeps are
//! interruptible (EINTR, restarted under SA_RESTART as on Linux for calls
//! without a timeout) and end at SO_RCVTIMEO and SO_SNDTIMEO (EAGAIN).
//!
//! **Data** moves between the program and the socket's rings in the
//! server's pool (`netclient::Rings`): one copy, with only the socket's
//! receive or send lock held (receives and sends are serialized, as Linux's
//! socket lock does, and released while a call sleeps), never the state
//! lock: a copy may fault on a page the pager must bring, and the net
//! thread and the pager take state locks.

use crate::files;
use crate::netclient::{self, Net, Rings};
use crate::sync::Mutex;
use crate::syscall;
use crate::unix::{Sink, Source};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
use netring::{fill, pieces, state, Ctl, Endpoint, Kind, Record, Request, RECORD_HEADER};
use restricted::*;

pub const EIO: i64 = 5;
pub const EAGAIN: i64 = 11;
pub const EFAULT: i64 = 14;
pub const EINVAL: i64 = 22;
pub const EPIPE: i64 = 32;
pub const EMSGSIZE: i64 = 90;
pub const EOPNOTSUPP: i64 = 95;
pub const ECONNRESET: i64 = 104;
pub const EISCONN: i64 = 106;
pub const ENOTCONN: i64 = 107;
pub const EALREADY: i64 = 114;
pub const EINPROGRESS: i64 = 115;

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;
pub const POLLRDHUP: i16 = 0x2000;

/// The largest UDP payload.
pub const MAX_UDP: usize = 65507;

/// What a receive asks for.
pub struct RecvOpts {
    pub peek: bool,
    pub waitall: bool,
    pub nonblock: bool,
}

/// What a receive got: bytes copied, the datagram's length (larger with
/// MSG_TRUNC), its source.
pub struct Received {
    pub copied: usize,
    pub len: usize,
    pub from: Option<Endpoint>,
}

/// The program's side of a socket.
pub struct Local {
    /// A connect was started and its outcome not yet reported to a
    /// connect (Linux's SS_CONNECTING).
    connecting: bool,
    /// netd has the socket's rings (TCP: given with the first connect).
    rings_sent: bool,
    listening: bool,
    pub shut_rd: bool,
    pub shut_wr: bool,
    /// The count of netd's errors taken (SO_ERROR, a failed call).
    err_taken: u32,
    /// The channel died: its error was taken.
    dead_taken: bool,
    /// The readiness reported last, and the positions and backlog seen
    /// then (more of them is an edge).
    reported: i16,
    seen_rx: u32,
    seen_tx: u32,
    seen_backlog: u32,
    pub opts: Opts,
}

/// Options: what has an effect, and the rest as Linux reads them back.
#[derive(Clone)]
pub struct Opts {
    pub rcvtimeo: u64,
    pub sndtimeo: u64,
    pub reuseaddr: bool,
    pub nodelay: bool,
    pub keepalive: bool,
    /// TCP_KEEPIDLE, TCP_KEEPINTVL, TCP_KEEPCNT (seconds, count).
    pub keepidle: i32,
    pub keepintvl: i32,
    pub keepcnt: i32,
    pub ttl: i32,
    /// SO_LINGER: None off, Some(seconds).
    pub linger: Option<i32>,
    pub rcvbuf: i32,
    pub sndbuf: i32,
    pub rcvlowat: i32,
    /// Plain options kept as ints, by (level, name).
    pub plain: BTreeMap<(i32, u64), i32>,
}

pub struct InetSock {
    pub kind: Kind,
    pub net: Arc<Net>,
    pub index: u32,
    /// Its placeholder's id.
    id: AtomicU64,
    rings: Mutex<Option<Rings>>,
    rlock: Mutex<()>,
    wlock: Mutex<()>,
    pub st: Mutex<Local>,
}

/// Monotonic nanoseconds.
fn now() -> u64 {
    crate::ringclient::now()
}

/// The deadline of a wait with timeout `ns` (0: none) starting now.
fn deadline(ns: u64) -> u64 {
    if ns == 0 { 0 } else { now().saturating_add(ns).max(1) }
}

/// What netd published, read state first (it publishes the state last:
/// whatever ended the data is seen with all of it).
#[derive(Clone, Copy)]
struct Snap {
    state: u32,
    rx_tail: u32,
    tx_head: u32,
    backlog: u32,
    err_seq: u32,
}

impl InetSock {
    fn new(kind: Kind, net: Arc<Net>, index: u32, rings: Option<Rings>, rings_sent: bool) -> Arc<InetSock> {
        // Linux's defaults: tcp_rmem/tcp_wmem's middle values for TCP,
        // rmem_default/wmem_default for the others.
        let (rcvbuf, sndbuf) = if kind == Kind::Tcp { (131072, 16384) } else { (212992, 212992) };
        Arc::new(InetSock {
            kind,
            net,
            index,
            id: AtomicU64::new(0),
            rings: Mutex::new(rings),
            rlock: Mutex::new(()),
            wlock: Mutex::new(()),
            st: Mutex::new(Local {
                connecting: false,
                rings_sent,
                listening: false,
                shut_rd: false,
                shut_wr: false,
                err_taken: 0,
                dead_taken: false,
                reported: 0,
                seen_rx: 0,
                seen_tx: 0,
                seen_backlog: 0,
                opts: Opts {
                    rcvtimeo: 0,
                    sndtimeo: 0,
                    reuseaddr: false,
                    nodelay: false,
                    keepalive: false,
                    keepidle: 7200,
                    keepintvl: 75,
                    keepcnt: 9,
                    ttl: 64,
                    linger: None,
                    rcvbuf,
                    sndbuf,
                    rcvlowat: 1,
                    plain: BTreeMap::new(),
                },
            }),
        })
    }

    /// A new socket of `kind` in netd.
    pub fn create(kind: Kind) -> Result<Arc<InetSock>, i64> {
        let net = netclient::net()?;
        let index = net.take_index()?;
        let rings = if kind.datagrams() {
            match net.take_rings() {
                Ok(r) => Some(r),
                Err(e) => {
                    net.put_index(index);
                    return Err(e);
                }
            }
        } else {
            None
        };
        if let Err(e) = net.status(Request::Socket { sock: index, kind, area: rings.map(|r| r.area) }) {
            if let Some(r) = rings {
                net.put_rings(r);
            }
            net.put_index(index);
            return Err(e);
        }
        // A datagram socket's rings went with SOCKET, a TCP socket's go
        // with its first connect.
        let s = InetSock::new(kind, net.clone(), index, rings, kind.datagrams());
        net.register(index, &s);
        Ok(s)
    }

    pub fn id(&self) -> u64 {
        self.id.load(SeqCst)
    }

    /// Its placeholder's id, once it has one (`report_now` then).
    pub fn set_id(&self, id: u64) {
        self.id.store(id, SeqCst);
    }

    fn ctl(&self) -> &Ctl {
        self.net.ctl(self.index)
    }

    fn rings(&self) -> Option<Rings> {
        *self.rings.lock()
    }

    fn snap(&self) -> Snap {
        let n = &self.ctl().netd;
        let state = n.state.load(SeqCst);
        Snap { state, rx_tail: n.rx_tail.load(SeqCst), tx_head: n.tx_head.load(SeqCst), backlog: n.backlog.load(SeqCst), err_seq: n.err_seq.load(SeqCst) }
    }

    /// The pending error, if any (taken with `take`).
    fn pending(&self, l: &mut Local, snap: &Snap, take: bool) -> Option<i64> {
        if self.net.is_dead() {
            if l.dead_taken {
                return None;
            }
            if take {
                l.dead_taken = true;
            }
            return Some(if self.kind == Kind::Tcp { ECONNRESET } else { EIO });
        }
        if snap.err_seq == l.err_taken {
            return None;
        }
        let e = self.ctl().netd.error.load(SeqCst) as i64;
        if take {
            l.err_taken = snap.err_seq;
        }
        Some(if e > 0 && e < 4096 { e } else { EIO })
    }

    /// The bytes waiting in the receive ring (EIO for positions netd
    /// should never publish).
    fn available(&self, snap: &Snap, r: &Rings) -> Result<u32, i64> {
        fill(self.ctl().client.rx_head.load(SeqCst), snap.rx_tail, r.size()).ok_or(EIO)
    }

    /// Poll readiness (state lock held): Linux's tcp_poll and udp_poll.
    fn readiness(&self, l: &mut Local) -> i16 {
        let snap = self.snap();
        let dead = self.net.is_dead();
        let err = self.pending(l, &snap, false).is_some();
        let mut m = if err { POLLERR } else { 0 };
        let rings = self.rings();
        let avail = rings.map_or(0, |r| self.available(&snap, &r).unwrap_or(0));
        if self.kind != Kind::Tcp {
            if avail > 0 {
                m |= POLLIN;
            }
            if snap.state & state::WRITABLE != 0 || dead {
                m |= POLLOUT;
            }
            if l.shut_rd && l.shut_wr {
                m |= POLLHUP;
            }
            if l.shut_rd {
                m |= POLLIN | POLLRDHUP;
            }
            return m;
        }
        if dead {
            return m | POLLIN | POLLOUT | POLLHUP | POLLRDHUP;
        }
        if snap.state & state::LISTENING != 0 {
            if snap.backlog > 0 {
                m |= POLLIN;
            }
            return m;
        }
        let established = snap.state & state::ESTABLISHED != 0;
        let connecting = snap.state & state::CONNECTING != 0;
        let closed = snap.state & state::CLOSED != 0 || (!established && !connecting);
        let rcv_shut = l.shut_rd || (established && snap.state & state::RECV_EOF != 0);
        let snd_shut = l.shut_wr || (established && snap.state & state::SEND_OPEN == 0);
        if (rcv_shut && snd_shut) || closed {
            m |= POLLHUP;
        }
        if rcv_shut {
            m |= POLLIN | POLLRDHUP;
        }
        if !connecting {
            if avail > 0 && avail >= l.opts.rcvlowat.max(1) as u32 {
                m |= POLLIN;
            }
            if snd_shut {
                m |= POLLOUT;
            } else {
                // Linux's __sk_stream_is_writeable: free space at least
                // half of what is queued.
                let (queued, size) = match rings {
                    Some(r) => (fill(snap.tx_head, self.ctl().client.tx_tail.load(SeqCst), r.size()).unwrap_or(0), r.size()),
                    None => (0, 1),
                };
                let free = size - queued;
                if free > 0 && free >= queued / 2 {
                    m |= POLLOUT;
                }
            }
        }
        m
    }

    /// After a change (state lock held): reports the readiness if it
    /// changed or `event` (new data, room, a connection: an edge for
    /// EPOLLET).
    fn report(&self, l: &mut Local, event: bool) {
        let id = self.id();
        if id == 0 {
            return;
        }
        let now = self.readiness(l);
        if now != l.reported || event {
            l.reported = now;
            files::ready(id, now);
        }
    }

    /// Reports the readiness as it is (a new placeholder).
    pub fn report_now(&self) {
        let mut l = self.st.lock();
        let now = self.readiness(&mut l);
        l.reported = now;
        files::ready(self.id(), now);
    }

    pub fn readiness_now(&self) -> i16 {
        self.readiness(&mut self.st.lock())
    }

    /// The net thread: netd changed the socket (or the channel died).
    pub fn netd_changed(&self) {
        let snap = self.snap();
        let mut l = self.st.lock();
        let event = snap.rx_tail != l.seen_rx || snap.tx_head != l.seen_tx || snap.backlog > l.seen_backlog;
        l.seen_rx = snap.rx_tail;
        l.seen_tx = snap.tx_head;
        l.seen_backlog = snap.backlog;
        self.report(&mut l, event);
        drop(l);
        if self.net.is_dead() {
            // Waiters sleep on the dead channel's memory: they wake, as
            // every futex wait there fails now.
            self.poke();
        }
    }

    /// The server's own change: waiters look again.
    fn poke(&self) {
        self.ctl().changed(&netclient::Futex);
    }

    fn changed(&self) {
        let mut l = self.st.lock();
        self.report(&mut l, false);
    }

    /// Sleeps while `seq` is `seen`, until `deadline` (0: none).
    fn wait(&self, seen: u32, deadline: u64) -> Result<(), i64> {
        match self.ctl().sleep(seen, |word, value| netclient::sleep(word, value, deadline)) {
            // A dead channel: the caller looks at the state again.
            Err(e) if e == EPIPE => Ok(()),
            r => r,
        }
    }

    // Addresses and connections.

    pub fn bind(&self, at: Endpoint) -> Result<(), i64> {
        let reuse = self.st.lock().opts.reuseaddr;
        self.net.status(Request::Bind { sock: self.index, at, reuse }).map(|_| ())
    }

    pub fn listen(&self, backlog: i32) -> Result<(), i64> {
        if self.kind != Kind::Tcp {
            return Err(EOPNOTSUPP);
        }
        let snap = self.snap();
        if snap.state & (state::ESTABLISHED | state::CONNECTING) != 0 {
            return Err(EINVAL);
        }
        self.net.status(Request::Listen { sock: self.index, backlog: backlog.clamp(0, i32::MAX) as u32 })?;
        let mut l = self.st.lock();
        l.listening = true;
        self.report(&mut l, false);
        Ok(())
    }

    /// connect(2): `to` None is AF_UNSPEC (a datagram socket's peer goes).
    pub fn connect(&self, to: Option<Endpoint>, nonblock: bool) -> Result<(), i64> {
        if self.net.is_dead() {
            return Err(netclient::ENETDOWN);
        }
        if self.kind != Kind::Tcp {
            let to = to.unwrap_or_default();
            self.net.status(Request::Connect { sock: self.index, to, area: None })?;
            self.changed();
            return Ok(());
        }
        let Some(to) = to else { return Err(EINVAL) };
        let timeout = {
            let mut l = self.st.lock();
            if l.listening {
                return Err(EISCONN);
            }
            let snap = self.snap();
            if l.connecting {
                // A connect under way (an earlier one that was nonblocking
                // or interrupted): its outcome.
                if snap.state & state::CONNECTING != 0 {
                    if nonblock {
                        return Err(EALREADY);
                    }
                } else {
                    l.connecting = false;
                    if snap.state & state::ESTABLISHED != 0 && snap.state & state::CLOSED == 0 {
                        return Ok(());
                    }
                    return Err(self.pending(&mut l, &snap, true).unwrap_or(ECONNREFUSED));
                }
            } else if snap.state & state::ESTABLISHED != 0 {
                return Err(EISCONN);
            }
            l.opts.sndtimeo
        };
        if !self.st.lock().connecting {
            // The rings go with the first connect.
            let send_rings = !self.st.lock().rings_sent;
            let rings = match (send_rings, self.rings()) {
                (true, None) => {
                    let r = self.net.take_rings()?;
                    *self.rings.lock() = Some(r);
                    Some(r)
                }
                (true, r) => r,
                (false, _) => None,
            };
            self.net.status(Request::Connect { sock: self.index, to, area: rings.map(|r| r.area) })?;
            let mut l = self.st.lock();
            l.rings_sent = true;
            l.connecting = true;
            self.report(&mut l, false);
            if nonblock {
                return Err(EINPROGRESS);
            }
        }
        let deadline = deadline(timeout);
        loop {
            let seen = self.ctl().seen();
            let snap = self.snap();
            if snap.state & state::CONNECTING == 0 || self.net.is_dead() {
                let mut l = self.st.lock();
                l.connecting = false;
                if snap.state & state::ESTABLISHED != 0 && snap.state & state::CLOSED == 0 && !self.net.is_dead() {
                    return Ok(());
                }
                return Err(self.pending(&mut l, &snap, true).unwrap_or(ECONNREFUSED));
            }
            match self.wait(seen, deadline) {
                Ok(()) => {}
                // The connect goes on (EALREADY for the next until it is
                // done); a timeout is SO_SNDTIMEO's.
                Err(EAGAIN) => return Err(EINPROGRESS),
                Err(e) => return Err(e),
            }
        }
    }

    /// accept(2): a connection, and its peer.
    pub fn accept(&self, nonblock: bool) -> Result<(Arc<InetSock>, Endpoint), i64> {
        if self.kind != Kind::Tcp {
            return Err(EOPNOTSUPP);
        }
        let deadline = deadline(self.st.lock().opts.rcvtimeo);
        loop {
            if self.net.is_dead() {
                return Err(self.pending(&mut self.st.lock(), &self.snap(), true).unwrap_or(EIO));
            }
            let seen = self.ctl().seen();
            let snap = self.snap();
            if snap.state & state::LISTENING == 0 {
                return Err(EINVAL);
            }
            if snap.backlog > 0 {
                let index = self.net.take_index()?;
                let rings = match self.net.take_rings() {
                    Ok(r) => r,
                    Err(e) => {
                        self.net.put_index(index);
                        return Err(e);
                    }
                };
                match self.net.status(Request::Accept { sock: self.index, new: index, area: rings.area }) {
                    Ok(c) => {
                        let peer = Endpoint { addr: c.values[0] as u32, port: c.values[1] as u16 };
                        let s = InetSock::new(Kind::Tcp, self.net.clone(), index, Some(rings), true);
                        // Its options as the listener's (Linux copies them).
                        s.st.lock().opts = self.st.lock().opts.clone();
                        self.net.register(index, &s);
                        self.changed();
                        return Ok((s, peer));
                    }
                    Err(e) => {
                        self.net.put_rings(rings);
                        self.net.put_index(index);
                        // Another thread took it: wait again.
                        if e != EAGAIN {
                            return Err(e);
                        }
                    }
                }
                continue;
            }
            if nonblock {
                return Err(EAGAIN);
            }
            self.wait(seen, deadline)?;
        }
    }

    /// The local (`peer` false) or the peer's address.
    pub fn name(&self, peer: bool) -> Result<Endpoint, i64> {
        if self.net.is_dead() {
            return if peer { Err(ENOTCONN) } else { Ok(Endpoint::default()) };
        }
        let c = self.net.status(Request::Name { sock: self.index, peer })?;
        Ok(Endpoint { addr: c.values[0] as u32, port: c.values[1] as u16 })
    }

    pub fn shutdown(&self, how: u64) -> Result<(), i64> {
        // SHUT_RD 0, SHUT_WR 1, SHUT_RDWR 2.
        let bits = match how {
            0 => netring::SHUT_RD,
            1 => netring::SHUT_WR,
            2 => netring::SHUT_RD | netring::SHUT_WR,
            _ => return Err(EINVAL),
        };
        let snap = self.snap();
        let connected = snap.state & (state::ESTABLISHED | state::CONNECTING) != 0;
        if self.kind == Kind::Tcp && !connected && !self.st.lock().listening {
            return Err(ENOTCONN);
        }
        {
            let mut l = self.st.lock();
            l.shut_rd |= bits & netring::SHUT_RD != 0;
            l.shut_wr |= bits & netring::SHUT_WR != 0;
        }
        if self.kind == Kind::Tcp && connected && !self.net.is_dead() {
            // The FIN after what the send ring holds (writers are done:
            // the send lock).
            let _w = self.wlock.lock();
            self.net.status(Request::Shutdown { sock: self.index, how: bits })?;
        }
        self.poke();
        self.changed();
        if self.kind != Kind::Tcp && self.name(true).is_err() {
            // As Linux: the flags are set, the socket was not connected.
            return Err(ENOTCONN);
        }
        Ok(())
    }

    // Data.

    /// send(2) and friends: from `src`, to `to` (datagrams; None: the
    /// peer). EPIPE once the connection cannot send (the caller raises
    /// SIGPIPE).
    pub fn send(&self, src: &mut Source, to: Option<Endpoint>, nonblock: bool) -> Result<usize, i64> {
        if self.kind == Kind::Tcp {
            if to.is_some() {
                let snap = self.snap();
                return Err(if snap.state & state::ESTABLISHED != 0 { EISCONN } else { ENOTCONN });
            }
            return self.send_stream(src, nonblock);
        }
        self.send_datagram(src, to, nonblock)
    }

    fn send_stream(&self, src: &mut Source, nonblock: bool) -> Result<usize, i64> {
        let timeout = self.st.lock().opts.sndtimeo;
        let deadline = deadline(timeout);
        let mut sent = 0usize;
        let mut writer = Some(self.wlock.lock());
        loop {
            let seen = self.ctl().seen();
            let snap = self.snap();
            {
                let mut l = self.st.lock();
                if let Some(e) = self.pending(&mut l, &snap, false) {
                    if sent > 0 {
                        break;
                    }
                    self.pending(&mut l, &snap, true);
                    self.report(&mut l, false);
                    return Err(e);
                }
                if l.shut_wr || (snap.state & state::ESTABLISHED != 0 && snap.state & state::SEND_OPEN == 0) || snap.state & state::CLOSED != 0 || self.net.is_dead() {
                    if sent > 0 {
                        break;
                    }
                    return Err(EPIPE);
                }
                if snap.state & (state::ESTABLISHED | state::CONNECTING) == 0 {
                    return Err(ENOTCONN);
                }
            }
            let Some(r) = self.rings() else { return Err(ENOTCONN) };
            let ctl = self.ctl();
            let tail = ctl.client.tx_tail.load(SeqCst);
            let queued = fill(snap.tx_head, tail, r.size()).ok_or(EIO)?;
            let room = r.size() - queued;
            if src.left() == 0 {
                break;
            }
            if room > 0 && snap.state & state::CONNECTING == 0 {
                let n = room.min(src.left().min(u32::MAX as usize) as u32);
                let [(a, k), (b, m)] = pieces(tail, n, r.size());
                let first = unsafe { core::slice::from_raw_parts_mut(r.tx().add(a as usize), k as usize) };
                let mut got = match src.read_into(first) {
                    Ok(got) => got,
                    Err(e) if sent == 0 => return Err(e),
                    Err(_) => break,
                };
                if got == k as usize && m > 0 {
                    let second = unsafe { core::slice::from_raw_parts_mut(r.tx().add(b as usize), m as usize) };
                    got += src.read_into(second).unwrap_or(0);
                }
                if got == 0 {
                    break;
                }
                ctl.client.tx_tail.store(tail.wrapping_add(got as u32), SeqCst);
                self.net.kick(self.index);
                sent += got;
                if src.left() == 0 || nonblock {
                    break;
                }
                continue;
            }
            if nonblock {
                if sent > 0 {
                    break;
                }
                return Err(EAGAIN);
            }
            // Not holding the other writers off while it sleeps.
            writer = None;
            if let Err(e) = self.wait(seen, deadline) {
                if sent > 0 {
                    break;
                }
                return Err(e);
            }
            writer = Some(self.wlock.lock());
        }
        drop(writer);
        self.changed();
        Ok(sent)
    }

    fn send_datagram(&self, src: &mut Source, to: Option<Endpoint>, nonblock: bool) -> Result<usize, i64> {
        let len = src.left();
        if len > MAX_UDP || (self.kind == Kind::RawIcmp && len > 65535 - 20) {
            return Err(EMSGSIZE);
        }
        let Some(r) = self.rings() else { return Err(EIO) };
        if len > r.size() as usize {
            return Err(EMSGSIZE);
        }
        let deadline = deadline(self.st.lock().opts.sndtimeo);
        let _writer = self.wlock.lock();
        // The datagram at the start of the send ring (a plain buffer for a
        // datagram socket), copied once.
        let buf = unsafe { core::slice::from_raw_parts_mut(r.tx(), len) };
        let got = src.read_into(buf)?;
        if got < len {
            return Err(EFAULT);
        }
        loop {
            if self.net.is_dead() {
                return Err(self.pending(&mut self.st.lock(), &self.snap(), true).unwrap_or(EIO));
            }
            let seen = self.ctl().seen();
            match self.net.status(Request::Send { sock: self.index, to: to.unwrap_or_default(), len: len as u32 }) {
                Ok(c) => return Ok(c.status as usize),
                Err(EAGAIN) if !nonblock => {}
                Err(e) => return Err(e),
            }
            let snap = self.snap();
            if snap.state & state::WRITABLE != 0 {
                // Room came meanwhile: try again at once.
                continue;
            }
            self.wait(seen, deadline)?;
        }
    }

    /// recv(2) and friends, into `dst`.
    pub fn recv(&self, dst: &mut Sink, o: RecvOpts) -> Result<Received, i64> {
        if self.kind == Kind::Tcp { self.recv_stream(dst, o) } else { self.recv_datagram(dst, o) }
    }

    fn recv_stream(&self, dst: &mut Sink, o: RecvOpts) -> Result<Received, i64> {
        let (timeout, lowat) = {
            let l = self.st.lock();
            (l.opts.rcvtimeo, l.opts.rcvlowat.max(1) as usize)
        };
        let deadline = deadline(timeout);
        let want = dst.room();
        // Linux's sock_rcvlowat: MSG_WAITALL wants all, else SO_RCVLOWAT.
        let target = if o.waitall && !o.peek { want } else { want.min(lowat) };
        let mut copied = 0usize;
        let mut reader = Some(self.rlock.lock());
        loop {
            let seen = self.ctl().seen();
            let snap = self.snap();
            let rings = self.rings();
            let avail = match &rings {
                Some(r) => self.available(&snap, r)?,
                None => 0,
            };
            if avail > 0 && dst.room() > 0 {
                let r = rings.expect("data needs rings");
                let ctl = self.ctl();
                let head = ctl.client.rx_head.load(SeqCst);
                let n = (avail as usize).min(dst.room()) as u32;
                let [(a, k), (b, m)] = pieces(head, n, r.size());
                let first = unsafe { core::slice::from_raw_parts(r.rx().add(a as usize), k as usize) };
                let mut got = match dst.put(first) {
                    Ok(got) => got,
                    Err(e) if copied == 0 => return Err(e),
                    Err(_) => break,
                };
                if got == k as usize && m > 0 {
                    let second = unsafe { core::slice::from_raw_parts(r.rx().add(b as usize), m as usize) };
                    got += dst.put(second).unwrap_or(0);
                }
                copied += got;
                if !o.peek {
                    ctl.client.rx_head.store(head.wrapping_add(got as u32), SeqCst);
                    // netd waits for room only if it said so (Dekker:
                    // our head, then its flag).
                    if ctl.netd.rx_wait.load(SeqCst) != 0 {
                        self.net.kick(self.index);
                    }
                }
                if o.peek || copied >= target || dst.room() == 0 || got < n as usize {
                    break;
                }
                continue;
            }
            if copied >= target || want == 0 {
                break;
            }
            {
                let mut l = self.st.lock();
                if let Some(e) = self.pending(&mut l, &snap, false) {
                    if copied > 0 {
                        break;
                    }
                    self.pending(&mut l, &snap, true);
                    self.report(&mut l, false);
                    return Err(e);
                }
                // (A dead channel's connection is reset: end of file after
                // its error.)
                if l.shut_rd || snap.state & (state::RECV_EOF | state::CLOSED) != 0 || self.net.is_dead() {
                    break;
                }
                if l.listening || snap.state & (state::ESTABLISHED | state::CONNECTING) == 0 {
                    if copied > 0 {
                        break;
                    }
                    return Err(ENOTCONN);
                }
            }
            if o.nonblock {
                if copied > 0 {
                    break;
                }
                return Err(EAGAIN);
            }
            // Not holding the other readers off while it sleeps.
            reader = None;
            if let Err(e) = self.wait(seen, deadline) {
                if copied > 0 {
                    break;
                }
                return Err(e);
            }
            reader = Some(self.rlock.lock());
        }
        drop(reader);
        if !o.peek {
            self.changed();
        }
        Ok(Received { copied, len: copied, from: None })
    }

    fn recv_datagram(&self, dst: &mut Sink, o: RecvOpts) -> Result<Received, i64> {
        let deadline = deadline(self.st.lock().opts.rcvtimeo);
        let Some(r) = self.rings() else { return Err(EIO) };
        let mut reader = Some(self.rlock.lock());
        loop {
            let seen = self.ctl().seen();
            let snap = self.snap();
            let avail = self.available(&snap, &r)?;
            if avail >= RECORD_HEADER {
                let ctl = self.ctl();
                let head = ctl.client.rx_head.load(SeqCst);
                let mut header = [0u8; RECORD_HEADER as usize];
                read_ring(&r, head, &mut header);
                let rec = Record::decode(&header);
                let span = Record::span(rec.len);
                if span > avail as u64 {
                    return Err(EIO);
                }
                let n = (rec.len as usize).min(dst.room()) as u32;
                let start = head.wrapping_add(RECORD_HEADER);
                let [(a, k), (b, m)] = pieces(start, n, r.size());
                let first = unsafe { core::slice::from_raw_parts(r.rx().add(a as usize), k as usize) };
                let mut got = dst.put(first)?;
                if got == k as usize && m > 0 {
                    let second = unsafe { core::slice::from_raw_parts(r.rx().add(b as usize), m as usize) };
                    got += dst.put(second).unwrap_or(0);
                }
                if !o.peek {
                    ctl.client.rx_head.store(head.wrapping_add(span as u32), SeqCst);
                    if ctl.netd.rx_wait.load(SeqCst) != 0 {
                        self.net.kick(self.index);
                    }
                }
                drop(reader);
                if !o.peek {
                    self.changed();
                }
                return Ok(Received { copied: got, len: rec.len as usize, from: Some(rec.from) });
            }
            {
                let mut l = self.st.lock();
                if let Some(e) = self.pending(&mut l, &snap, true) {
                    self.report(&mut l, false);
                    return Err(e);
                }
                if l.shut_rd {
                    return Ok(Received { copied: 0, len: 0, from: None });
                }
                if self.net.is_dead() {
                    return Err(netclient::ENETDOWN);
                }
            }
            if o.nonblock {
                return Err(EAGAIN);
            }
            // Not holding the other readers off while it sleeps.
            drop(reader.take());
            self.wait(seen, deadline)?;
            reader = Some(self.rlock.lock());
        }
    }

    /// FIONREAD: a stream's bytes waiting; a datagram socket's next
    /// datagram's length.
    pub fn inq(&self) -> Result<i64, i64> {
        let snap = self.snap();
        let Some(r) = self.rings() else { return Ok(0) };
        let avail = self.available(&snap, &r)?;
        if self.kind == Kind::Tcp {
            return Ok(avail as i64);
        }
        if avail < RECORD_HEADER {
            return Ok(0);
        }
        let mut header = [0u8; RECORD_HEADER as usize];
        read_ring(&r, self.ctl().client.rx_head.load(SeqCst), &mut header);
        Ok(Record::decode(&header).len as i64)
    }

    /// SIOCOUTQ: bytes written and not yet taken by netd.
    pub fn outq(&self) -> i64 {
        match (self.kind, self.rings()) {
            (Kind::Tcp, Some(r)) => fill(self.snap().tx_head, self.ctl().client.tx_tail.load(SeqCst), r.size()).unwrap_or(0) as i64,
            _ => 0,
        }
    }

    /// SO_ERROR: the pending error (taken), or 0.
    pub fn take_error(&self) -> i64 {
        let snap = self.snap();
        let mut l = self.st.lock();
        let e = self.pending(&mut l, &snap, true).unwrap_or(0);
        self.report(&mut l, false);
        e
    }

    pub fn listening(&self) -> bool {
        self.st.lock().listening
    }

    /// Passes an option netd applies (TCP_NODELAY, keep-alive, IP_TTL).
    pub fn setopt(&self, option: u32, value: u64) -> Result<(), i64> {
        if self.net.is_dead() {
            return Ok(());
        }
        self.net.status(Request::SetOpt { sock: self.index, opt: option, value }).map(|_| ())
    }

    /// The placeholder's last descriptor went: the socket closes in netd
    /// (a reset if data it received is unread, or SO_LINGER with a zero
    /// time), `now` before this returns (its port is free then), else by
    /// the net thread (the caller may not wait for netd: the pager).
    pub fn release(&self, now: bool) {
        let snap = self.snap();
        let rings = self.rings.lock().take();
        let unread = rings.is_some_and(|r| self.kind == Kind::Tcp && self.available(&snap, &r).unwrap_or(0) > 0);
        let abort = unread || self.st.lock().opts.linger == Some(0);
        if now {
            self.net.close_now(self.index, rings, abort);
        } else {
            self.net.queue_close(self.index, rings, abort);
        }
    }
}

/// Copies `out.len()` bytes from the receive ring at position `pos`.
fn read_ring(r: &Rings, pos: u32, out: &mut [u8]) {
    let [(a, k), (b, m)] = pieces(pos, out.len() as u32, r.size());
    unsafe {
        core::ptr::copy_nonoverlapping(r.rx().add(a as usize), out.as_mut_ptr(), k as usize);
        core::ptr::copy_nonoverlapping(r.rx().add(b as usize), out.as_mut_ptr().add(k as usize), m as usize);
    }
}

/// Raises SIGPIPE for the calling thread (a write to a connection that
/// cannot send, without MSG_NOSIGNAL).
pub fn sigpipe() {
    const SIGPIPE: u64 = 13;
    syscall(SYS_SIGNAL_THREAD, [SIGPIPE, 0, 0, 0, 0, 0]);
}

pub const ECONNREFUSED: i64 = 111;
