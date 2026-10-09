//! The socket service: `netproto` requests from the kernel mapped onto
//! smoltcp sockets. A request that cannot complete yet (accept without a
//! connection, recv without data, ...) is kept and answered later, after
//! the stack made progress; the kernel may cancel it when its caller is
//! interrupted by a signal.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use netproto::*;
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::{raw, tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpVersion, Ipv4Address, Ipv4Packet, Ipv4Repr};

const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const EINVAL: i64 = 22;
const EPIPE: i64 = 32;
const ENOSYS: i64 = 38;
const EOPNOTSUPP: i64 = 95;
const EADDRINUSE: i64 = 98;
const ENETUNREACH: i64 = 101;
const ENOBUFS: i64 = 105;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ETIMEDOUT: i64 = 110;
const ECONNREFUSED: i64 = 111;
const EINPROGRESS: i64 = 115;
const EDESTADDRREQ: i64 = 89;
const EMSGSIZE: i64 = 90;
const EINTR: i64 = 4;

/// Upper bound for sockets, so the 4 MiB heap holds all buffers.
const MAX_SOCKETS: usize = 64;
const TCP_BUFFER: usize = 16 * 1024;
const UDP_PACKETS: usize = 16;
const UDP_BUFFER: usize = 16 * 1024;
const MAX_BACKLOG: usize = 8;
/// The Ethernet header a frame adds to an IP packet.
const ETHERNET_HEADER: usize = 14;
/// Waiting requests, and the data waiting sends may hold in total, so that
/// many blocked programs cannot exhaust netd's heap.
const MAX_PENDING: usize = 128;
const MAX_PENDING_BYTES: usize = 512 * 1024;
/// A connection attempt (or unacknowledged data) gives up after this.
const TCP_TIMEOUT: Duration = Duration::from_secs(20);

enum Entry {
    Tcp(Tcp),
    Udp(Udp),
    Raw(Raw),
}

struct Raw {
    socket: SocketHandle,
    peer: Option<Ipv4Address>,
}

const IPV4_HEADER: usize = 20;

struct Tcp {
    socket: SocketHandle,
    /// Address from bind, used by listen and connect.
    local: Option<IpListenEndpoint>,
    /// Listening: the sockets waiting for connections (the first is `socket`).
    backlog: Option<Vec<SocketHandle>>,
    /// Connect started at this time (until it completed or failed).
    connecting: Option<Instant>,
    connected: bool,
    /// Pending error for SO_ERROR (positive errno).
    error: i64,
}

struct Udp {
    socket: SocketHandle,
    peer: Option<IpEndpoint>,
}

/// A request waiting for the network.
struct Pending {
    id: u64,
    op: Op,
    args: [u64; 4],
    payload: Vec<u8>,
}

/// Network configuration for Info.
#[derive(Default, Clone, Copy)]
pub struct Config {
    pub address: u32,
    pub prefix: u8,
    pub gateway: u32,
    pub dns: u32,
    pub mac: [u8; 6],
}

pub struct Service {
    entries: BTreeMap<u64, Entry>,
    next_handle: u64,
    next_port: u16,
    pending: Vec<Pending>,
    /// The poll events last seen for each socket (see `announce`).
    announced: BTreeMap<u64, u64>,
    /// Sockets closed by their owner that still finish their FIN exchange.
    closing: Vec<SocketHandle>,
    pub config: Config,
}

/// The result of one attempt at a request.
enum Outcome {
    Done(i64, [u64; 6], Vec<u8>),
    /// Try again after the next poll (or answer EAGAIN if non-blocking).
    Wait,
}

use Outcome::*;

fn done(status: i64) -> Result<Outcome, i64> {
    Ok(Done(status, [0; 6], Vec::new()))
}

fn ipv4(addr: u64) -> IpAddress {
    IpAddress::Ipv4(Ipv4Address::from_bits(addr as u32))
}

fn bits(addr: IpAddress) -> u64 {
    match addr {
        IpAddress::Ipv4(a) => a.to_bits() as u64,
    }
}

impl Service {
    pub fn new() -> Service {
        Service {
            entries: BTreeMap::new(),
            next_handle: 1,
            next_port: 49152,
            pending: Vec::new(),
            announced: BTreeMap::new(),
            closing: Vec::new(),
            config: Config::default(),
        }
    }

    fn ephemeral_port(&mut self) -> u16 {
        let port = self.next_port;
        self.next_port = if port == u16::MAX { 49152 } else { port + 1 };
        port
    }

    fn new_tcp(sockets: &mut SocketSet<'static>) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0; TCP_BUFFER]);
        let tx = tcp::SocketBuffer::new(vec![0; TCP_BUFFER]);
        let mut socket = tcp::Socket::new(rx, tx);
        socket.set_timeout(Some(TCP_TIMEOUT));
        socket.set_nagle_enabled(false);
        sockets.add(socket)
    }

    fn sockets_in_use(&self, sockets: &SocketSet<'static>) -> usize {
        sockets.iter().count()
    }

    /// Handles one request. Returns the response, or None if it is kept
    /// pending (then it is answered by `progress`).
    pub fn handle(
        &mut self,
        id: u64,
        op: Op,
        args: [u64; 4],
        payload: &[u8],
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        now: Instant,
    ) -> Option<(i64, [u64; 6], Vec<u8>)> {
        match op {
            Op::Cancel => {
                // The kernel no longer waits; the answer only frees the request.
                if let Some(i) = self.pending.iter().position(|p| p.id == args[0]) {
                    let p = self.pending.remove(i);
                    self.respond(p.id, -EINTR, [0; 6], &[]);
                }
                return None;
            }
            Op::Close => {
                self.close(args[0], sockets);
                return None;
            }
            _ => {}
        }
        if op == Op::Connect {
            // Starting the attempt happens once; waiting is a separate step.
            match self.start_connect(args, iface, sockets, now) {
                Ok(true) => {}
                Ok(false) => return Some((0, [0; 6], Vec::new())),
                Err(e) => return Some((-e, [0; 6], Vec::new())),
            }
            if args[3] & NONBLOCK != 0 {
                return Some((-EINPROGRESS, [0; 6], Vec::new()));
            }
        }
        match self.attempt(op, args, payload, iface, sockets, now) {
            Ok(Done(status, values, data)) => Some((status, values, data)),
            Ok(Wait) if Self::nonblocking(op, args) => Some((-EAGAIN, [0; 6], Vec::new())),
            Ok(Wait) => {
                let held: usize = self.pending.iter().map(|p| p.payload.len()).sum();
                if self.pending.len() >= MAX_PENDING || held + payload.len() > MAX_PENDING_BYTES {
                    return Some((-ENOBUFS, [0; 6], Vec::new()));
                }
                self.pending.push(Pending { id, op, args, payload: payload.to_vec() });
                None
            }
            Err(e) => Some((-e, [0; 6], Vec::new())),
        }
    }

    fn nonblocking(op: Op, args: [u64; 4]) -> bool {
        let flags = match op {
            Op::Accept | Op::Send => args[1],
            Op::Recv => args[2],
            _ => 0,
        };
        flags & NONBLOCK != 0
    }

    /// Retries the pending requests after the stack made progress.
    pub fn progress(&mut self, iface: &mut Interface, sockets: &mut SocketSet<'static>, now: Instant) {
        let mut i = 0;
        while i < self.pending.len() {
            let p = &self.pending[i];
            let (op, args, payload) = (p.op, p.args, core::mem::take(&mut self.pending[i].payload));
            match self.attempt(op, args, &payload, iface, sockets, now) {
                Ok(Wait) => {
                    self.pending[i].payload = payload;
                    i += 1;
                }
                result => {
                    let p = self.pending.remove(i);
                    match result {
                        Ok(Done(status, values, data)) => self.respond(p.id, status, values, &data),
                        Err(e) => self.respond(p.id, -e, [0; 6], &[]),
                        Ok(Wait) => unreachable!(),
                    }
                }
            }
        }
        // Sockets whose owner closed them go away once the FIN exchange is over.
        self.closing.retain(|&h| {
            let gone = sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed;
            if gone {
                sockets.remove(h);
            }
            !gone
        });
    }

    pub fn respond(&self, id: u64, status: i64, values: [u64; 6], payload: &[u8]) {
        let mut out = vec![0u8; RESPONSE_HEADER + payload.len()];
        let n = encode_response(&mut out, status, values, payload);
        let _ = oxrt::ipc_reply(id, &out[..n]);
    }

    fn close(&mut self, handle: u64, sockets: &mut SocketSet<'static>) {
        // Nobody waits on a socket whose last descriptor is gone.
        match self.entries.remove(&handle) {
            Some(Entry::Tcp(t)) => {
                if let Some(backlog) = t.backlog {
                    for h in backlog {
                        sockets.remove(h);
                    }
                } else {
                    let socket = sockets.get_mut::<tcp::Socket>(t.socket);
                    if socket.state() == tcp::State::Closed {
                        sockets.remove(t.socket);
                    } else {
                        socket.close();
                        self.closing.push(t.socket);
                    }
                }
            }
            Some(Entry::Udp(u)) => {
                sockets.remove(u.socket);
            }
            Some(Entry::Raw(r)) => {
                sockets.remove(r.socket);
            }
            None => {}
        }
    }

    /// Begins a TCP connect (Ok(true): wait for it) or connects a UDP
    /// socket (Ok(false): done).
    fn start_connect(&mut self, args: [u64; 4], iface: &mut Interface, sockets: &mut SocketSet<'static>, now: Instant) -> Result<bool, i64> {
        let remote = IpEndpoint::new(ipv4(args[1]), args[2] as u16);
        let port = self.ephemeral_port();
        match self.entries.get_mut(&args[0]).ok_or(EBADF)? {
            Entry::Udp(u) => {
                u.peer = Some(remote);
                let socket = sockets.get_mut::<udp::Socket>(u.socket);
                if !socket.is_open() {
                    socket.bind(port).map_err(|_| EADDRINUSE)?;
                }
                Ok(false)
            }
            Entry::Raw(r) => {
                r.peer = Some(Ipv4Address::from_bits(args[1] as u32));
                Ok(false)
            }
            Entry::Tcp(t) => {
                if t.backlog.is_some() {
                    return Err(EINVAL);
                }
                if t.connected || t.connecting.is_some() {
                    return Err(EISCONN);
                }
                let mut local = t.local.unwrap_or_default();
                if local.port == 0 {
                    local.port = port;
                }
                let socket = sockets.get_mut::<tcp::Socket>(t.socket);
                socket.connect(iface.context(), remote, local).map_err(|e| match e {
                    tcp::ConnectError::InvalidState => EISCONN,
                    tcp::ConnectError::Unaddressable => ENETUNREACH,
                })?;
                t.connecting = Some(now);
                Ok(true)
            }
        }
    }

    /// The poll events socket `handle` has now (all of them).
    fn readiness(&self, handle: u64, sockets: &SocketSet<'static>) -> Option<u64> {
        let mut ready = 0;
        match self.entries.get(&handle)? {
            Entry::Tcp(t) => {
                if let Some(set) = &t.backlog {
                    let any = set
                        .iter()
                        .any(|&h| !matches!(sockets.get::<tcp::Socket>(h).state(), tcp::State::Listen | tcp::State::SynReceived));
                    if any {
                        ready |= POLLIN;
                    }
                } else {
                    let s = sockets.get::<tcp::Socket>(t.socket);
                    let state = s.state();
                    let opening = matches!(state, tcp::State::SynSent | tcp::State::SynReceived);
                    if s.can_recv() || (!s.may_recv() && !opening && (t.connected || t.connecting.is_some())) {
                        ready |= POLLIN;
                    }
                    if s.can_send() && !opening {
                        ready |= POLLOUT;
                    }
                    if state == tcp::State::Closed && t.connecting.is_some() {
                        // A failed connect: writable with an error, as on Linux.
                        ready |= POLLOUT | POLLERR | POLLHUP;
                    } else if state == tcp::State::Closed && t.connected {
                        ready |= POLLHUP;
                    }
                }
                if t.error != 0 {
                    ready |= POLLERR;
                }
            }
            Entry::Udp(u) => {
                let s = sockets.get::<udp::Socket>(u.socket);
                if s.can_recv() {
                    ready |= POLLIN;
                }
                if s.can_send() {
                    ready |= POLLOUT;
                }
            }
            Entry::Raw(r) => {
                let s = sockets.get::<raw::Socket>(r.socket);
                if s.can_recv() {
                    ready |= POLLIN;
                }
                if s.can_send() {
                    ready |= POLLOUT;
                }
            }
        }
        Some(ready)
    }

    /// Tells the kernel about every socket that gained poll events since
    /// the last call, so that poll, select and epoll waiting on it wake
    /// up. Called after every step that can change readiness: the stack's
    /// progress and each request. Losing events needs no notice (waiters
    /// check again), and the current state is remembered either way.
    pub fn announce(&mut self, sockets: &SocketSet<'static>) {
        let handles: Vec<u64> = self.entries.keys().copied().collect();
        for handle in handles {
            let now = self.readiness(handle, sockets).unwrap_or(0);
            let before = self.announced.insert(handle, now).unwrap_or(0);
            if now & !before != 0 {
                let _ = oxrt::ipc_notify(handle);
            }
        }
        let entries = &self.entries;
        self.announced.retain(|h, _| entries.contains_key(h));
    }

    fn attempt(
        &mut self,
        op: Op,
        args: [u64; 4],
        payload: &[u8],
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        now: Instant,
    ) -> Result<Outcome, i64> {
        let handle = args[0];
        match op {
            Op::Socket => {
                if self.sockets_in_use(sockets) >= MAX_SOCKETS {
                    return Err(ENOBUFS);
                }
                let entry = match args[0] {
                    KIND_TCP => Entry::Tcp(Tcp {
                        socket: Self::new_tcp(sockets),
                        local: None,
                        backlog: None,
                        connecting: None,
                        connected: false,
                        error: 0,
                    }),
                    KIND_UDP => {
                        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], vec![0; UDP_BUFFER]);
                        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], vec![0; UDP_BUFFER]);
                        Entry::Udp(Udp { socket: sockets.add(udp::Socket::new(rx, tx)), peer: None })
                    }
                    KIND_RAW_ICMP => {
                        let rx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; UDP_PACKETS], vec![0; UDP_BUFFER]);
                        let tx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; UDP_PACKETS], vec![0; UDP_BUFFER]);
                        let socket = raw::Socket::new(Some(IpVersion::Ipv4), Some(IpProtocol::Icmp), rx, tx);
                        Entry::Raw(Raw { socket: sockets.add(socket), peer: None })
                    }
                    _ => return Err(EINVAL),
                };
                let h = self.next_handle;
                self.next_handle += 1;
                self.entries.insert(h, entry);
                Ok(Done(0, [h, 0, 0, 0, 0, 0], Vec::new()))
            }
            Op::Bind => {
                let addr = (args[1] != 0).then(|| ipv4(args[1]));
                let port = if args[2] == 0 && matches!(self.entries.get(&handle), Some(Entry::Udp(_))) {
                    self.ephemeral_port()
                } else {
                    args[2] as u16
                };
                match self.entries.get_mut(&handle).ok_or(EBADF)? {
                    Entry::Tcp(t) => {
                        t.local = Some(IpListenEndpoint { addr, port });
                        done(0)
                    }
                    Entry::Udp(u) => {
                        let socket = sockets.get_mut::<udp::Socket>(u.socket);
                        socket.bind(IpListenEndpoint { addr, port }).map_err(|_| EINVAL)?;
                        done(0)
                    }
                    // A raw socket's address only selects the source; ours is fixed.
                    Entry::Raw(_) => done(0),
                }
            }
            Op::Listen => {
                let backlog = (args[1] as usize).clamp(1, MAX_BACKLOG);
                let port = self.ephemeral_port();
                let free = MAX_SOCKETS.saturating_sub(self.sockets_in_use(sockets));
                let Entry::Tcp(t) = self.entries.get_mut(&handle).ok_or(EBADF)? else { return Err(EOPNOTSUPP) };
                if t.backlog.is_some() {
                    return done(0);
                }
                let mut local = t.local.unwrap_or_default();
                if local.port == 0 {
                    local.port = port;
                    t.local = Some(local);
                }
                sockets.get_mut::<tcp::Socket>(t.socket).listen(local).map_err(|_| EINVAL)?;
                let mut set = vec![t.socket];
                for _ in 1..backlog.min(free + 1) {
                    let h = Self::new_tcp(sockets);
                    let _ = sockets.get_mut::<tcp::Socket>(h).listen(local);
                    set.push(h);
                }
                t.backlog = Some(set);
                done(0)
            }
            Op::Accept => {
                let Entry::Tcp(t) = self.entries.get_mut(&handle).ok_or(EBADF)? else { return Err(EOPNOTSUPP) };
                let local = t.local.unwrap_or_default();
                let Some(set) = t.backlog.as_mut() else { return Err(EINVAL) };
                let ready = set.iter().position(|&h| {
                    let s = sockets.get::<tcp::Socket>(h);
                    !matches!(s.state(), tcp::State::Listen | tcp::State::SynReceived | tcp::State::Closed)
                });
                let Some(i) = ready else { return Ok(Wait) };
                let conn = set[i];
                // A fresh socket takes the place of the accepted one.
                let fresh = Self::new_tcp(sockets);
                let _ = sockets.get_mut::<tcp::Socket>(fresh).listen(local);
                set[i] = fresh;
                let peer = sockets.get::<tcp::Socket>(conn).remote_endpoint().unwrap_or(IpEndpoint::new(ipv4(0), 0));
                let h = self.next_handle;
                self.next_handle += 1;
                self.entries.insert(
                    h,
                    Entry::Tcp(Tcp { socket: conn, local: Some(local), backlog: None, connecting: None, connected: true, error: 0 }),
                );
                Ok(Done(0, [h, bits(peer.addr), peer.port as u64, 0, 0, 0], Vec::new()))
            }
            Op::Connect => {
                let Entry::Tcp(t) = self.entries.get_mut(&handle).ok_or(EBADF)? else { return done(0) };
                let s = sockets.get::<tcp::Socket>(t.socket);
                match s.state() {
                    tcp::State::Established | tcp::State::CloseWait => {
                        t.connecting = None;
                        t.connected = true;
                        done(0)
                    }
                    tcp::State::SynSent | tcp::State::SynReceived => Ok(Wait),
                    _ => {
                        let started = t.connecting.take().unwrap_or(now);
                        let e = if now - started >= TCP_TIMEOUT { ETIMEDOUT } else { ECONNREFUSED };
                        Err(e)
                    }
                }
            }
            Op::Send => match self.entries.get_mut(&handle).ok_or(EBADF)? {
                Entry::Tcp(t) => {
                    let s = sockets.get_mut::<tcp::Socket>(t.socket);
                    if matches!(s.state(), tcp::State::SynSent | tcp::State::SynReceived) {
                        return Ok(Wait);
                    }
                    if !s.may_send() {
                        return Err(if t.connected { EPIPE } else { ENOTCONN });
                    }
                    if !s.can_send() {
                        return Ok(Wait);
                    }
                    let n = s.send_slice(payload).map_err(|_| EPIPE)?;
                    done(n as i64)
                }
                Entry::Udp(u) => {
                    let to = if args[2] != 0 { IpEndpoint::new(ipv4(args[2]), args[3] as u16) } else { u.peer.ok_or(EDESTADDRREQ)? };
                    let port = self.next_port;
                    let s = sockets.get_mut::<udp::Socket>(u.socket);
                    if !s.is_open() {
                        s.bind(port).map_err(|_| EADDRINUSE)?;
                        self.ephemeral_port();
                    }
                    if !s.can_send() {
                        return Ok(Wait);
                    }
                    match s.send_slice(payload, to) {
                        Ok(()) => done(payload.len() as i64),
                        Err(udp::SendError::BufferFull) => Ok(Wait),
                        Err(udp::SendError::Unaddressable) => Err(ENETUNREACH),
                    }
                }
                Entry::Raw(r) => {
                    let dst = if args[2] != 0 { Ipv4Address::from_bits(args[2] as u32) } else { r.peer.ok_or(EDESTADDRREQ)? };
                    if payload.len() + IPV4_HEADER > UDP_BUFFER {
                        return Err(EMSGSIZE);
                    }
                    let src = iface.get_source_address_ipv4(&dst).ok_or(ENETUNREACH)?;
                    let s = sockets.get_mut::<raw::Socket>(r.socket);
                    if !s.can_send() {
                        return Ok(Wait);
                    }
                    // The application writes the ICMP message; the IP header is ours.
                    let repr = Ipv4Repr { src_addr: src, dst_addr: dst, next_header: IpProtocol::Icmp, payload_len: payload.len(), hop_limit: 64 };
                    let mut packet = vec![0u8; IPV4_HEADER + payload.len()];
                    repr.emit(&mut Ipv4Packet::new_unchecked(&mut packet[..]), &ChecksumCapabilities::default());
                    packet[IPV4_HEADER..].copy_from_slice(payload);
                    s.send_slice(&packet).map_err(|_| ENOBUFS)?;
                    done(payload.len() as i64)
                }
            },
            Op::Recv => {
                let max = (args[1] as usize).min(MAX_DATA);
                let peek = args[2] & PEEK != 0;
                match self.entries.get_mut(&handle).ok_or(EBADF)? {
                    Entry::Tcp(t) => {
                        let s = sockets.get_mut::<tcp::Socket>(t.socket);
                        if s.can_recv() {
                            let mut data = vec![0u8; max];
                            let n = if peek { s.peek_slice(&mut data) } else { s.recv_slice(&mut data) }.unwrap_or(0);
                            data.truncate(n);
                            let peer = s.remote_endpoint().unwrap_or(IpEndpoint::new(ipv4(0), 0));
                            return Ok(Done(n as i64, [bits(peer.addr), peer.port as u64, 0, 0, 0, 0], data));
                        }
                        match s.state() {
                            tcp::State::SynSent | tcp::State::SynReceived | tcp::State::Established => Ok(Wait),
                            tcp::State::Listen => Err(ENOTCONN),
                            _ if !t.connected && t.connecting.is_none() => Err(ENOTCONN),
                            // The peer closed (or the connection is gone): end of file.
                            _ => done(0),
                        }
                    }
                    Entry::Udp(u) => {
                        let s = sockets.get_mut::<udp::Socket>(u.socket);
                        let result = if peek {
                            s.peek().map(|(d, m)| (d.to_vec(), m.endpoint))
                        } else {
                            s.recv().map(|(d, m)| (d.to_vec(), m.endpoint))
                        };
                        match result {
                            Ok((mut data, from)) => {
                                data.truncate(max);
                                Ok(Done(data.len() as i64, [bits(from.addr), from.port as u64, 0, 0, 0, 0], data))
                            }
                            Err(_) => Ok(Wait),
                        }
                    }
                    Entry::Raw(r) => {
                        let s = sockets.get_mut::<raw::Socket>(r.socket);
                        let packet = if peek { s.peek().map(|d| d.to_vec()) } else { s.recv().map(|d| d.to_vec()) };
                        match packet {
                            Ok(mut data) => {
                                // Whole IPv4 packet, header included, as on Linux.
                                let from = Ipv4Packet::new_checked(&data[..]).map_or(0, |p| p.src_addr().to_bits() as u64);
                                data.truncate(max);
                                Ok(Done(data.len() as i64, [from, 0, 0, 0, 0, 0], data))
                            }
                            Err(_) => Ok(Wait),
                        }
                    }
                }
            }
            Op::Shutdown => {
                if let Entry::Tcp(t) = self.entries.get(&handle).ok_or(EBADF)? {
                    if args[1] != 0 && t.backlog.is_none() {
                        sockets.get_mut::<tcp::Socket>(t.socket).close();
                    }
                }
                done(0)
            }
            Op::Poll => {
                let ready = self.readiness(handle, sockets).ok_or(EBADF)?;
                Ok(Done(0, [ready & (args[1] | POLLERR | POLLHUP), 0, 0, 0, 0, 0], Vec::new()))
            }
            Op::Name => {
                let peer = args[1] != 0;
                let ep = match self.entries.get(&handle).ok_or(EBADF)? {
                    Entry::Tcp(t) => {
                        let s = sockets.get::<tcp::Socket>(t.socket);
                        if peer {
                            s.remote_endpoint().ok_or(ENOTCONN)?
                        } else {
                            s.local_endpoint().or_else(|| {
                                let l = t.local?;
                                Some(IpEndpoint::new(l.addr.unwrap_or(ipv4(0)), l.port))
                            }).unwrap_or(IpEndpoint::new(ipv4(0), 0))
                        }
                    }
                    Entry::Udp(u) => {
                        if peer {
                            u.peer.ok_or(ENOTCONN)?
                        } else {
                            let l = sockets.get::<udp::Socket>(u.socket).endpoint();
                            let addr = l.addr.unwrap_or(ipv4(self.config.address as u64));
                            IpEndpoint::new(addr, l.port)
                        }
                    }
                    Entry::Raw(r) => match (peer, r.peer) {
                        (true, Some(p)) => IpEndpoint::new(IpAddress::Ipv4(p), 0),
                        (true, None) => return Err(ENOTCONN),
                        (false, _) => IpEndpoint::new(ipv4(0), 0),
                    },
                };
                // An unbound local address reads as the interface address.
                let addr = match bits(ep.addr) {
                    0 if !peer => self.config.address as u64,
                    a => a,
                };
                Ok(Done(0, [addr, ep.port as u64, 0, 0, 0, 0], Vec::new()))
            }
            Op::TakeError => {
                // A failed connect leaves its reason here (for non-blocking connects).
                let Entry::Tcp(t) = self.entries.get_mut(&handle).ok_or(EBADF)? else { return done(0) };
                if let Some(started) = t.connecting {
                    let state = sockets.get::<tcp::Socket>(t.socket).state();
                    if state == tcp::State::Closed {
                        t.connecting = None;
                        t.error = if now - started >= TCP_TIMEOUT { ETIMEDOUT } else { ECONNREFUSED };
                    } else if matches!(state, tcp::State::Established | tcp::State::CloseWait) {
                        t.connecting = None;
                        t.connected = true;
                    }
                }
                let e = core::mem::take(&mut t.error);
                done(e)
            }
            Op::Info => {
                let c = self.config;
                let _ = iface;
                Ok(Done(0, [c.address as u64, c.prefix as u64, c.gateway as u64, c.dns as u64, 0, 0], c.mac.to_vec()))
            }
            Op::Links => {
                // The loopback (frames to the host's own addresses come
                // back in `Nic`) and the card, both with Ethernet framing:
                // the card's MTU applies to both.
                let mtu = (crate::virtio_net::MTU - ETHERNET_HEADER) as u32;
                let c = self.config;
                let up = netproto::LINK_UP | netproto::LINK_RUNNING;
                let (address, prefix) = (crate::LOOPBACK.address().to_bits(), crate::LOOPBACK.prefix_len());
                let lo = Link { index: 1, kind: netproto::LINK_LOOPBACK, state: up, mtu, mac: [0; 6], prefix, address };
                let card = Link { index: 2, kind: netproto::LINK_ETHERNET, state: up, mtu, mac: c.mac, prefix: c.prefix, address: c.address };
                let mut payload = Vec::with_capacity(2 * Link::SIZE);
                payload.extend_from_slice(&lo.encode());
                payload.extend_from_slice(&card.encode());
                Ok(Done(0, [0; 6], payload))
            }
            Op::Close | Op::Cancel => Err(ENOSYS),
        }
    }
}
