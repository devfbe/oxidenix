//! The system calls of internet sockets (phase R7b): socket(2) for the
//! AF_INET family, and every call on a descriptor of one of the server's
//! internet sockets: addresses (`sockaddr_in`), message headers, flags,
//! options, read and write. The sockets themselves are `inet`.

use crate::files::{self, File, O_CLOEXEC, O_NONBLOCK, O_RDWR};
use crate::inet::*;
use crate::sockcalls::{self, MsgHdr};
use crate::unix::{Sink, Source};
use crate::usercopy;
use alloc::sync::Arc;
use alloc::vec::Vec;
use netring::{opt, Endpoint, Kind};

pub const AF_INET: u16 = 2;
const AF_UNSPEC: u16 = 0;

const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = O_NONBLOCK as u64;
const SOCK_CLOEXEC: u64 = O_CLOEXEC as u64;

const IPPROTO_IP: u64 = 0;
const IPPROTO_ICMP: u64 = 1;
const IPPROTO_TCP: u64 = 6;
const IPPROTO_UDP: u64 = 17;

const EACCES: i64 = 13;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const ENOPROTOOPT: i64 = 92;
const EPROTONOSUPPORT: i64 = 93;
const ESOCKTNOSUPPORT: i64 = 94;
const EAFNOSUPPORT: i64 = 97;

const MSG_OOB: u64 = 0x1;
const MSG_PEEK: u64 = 0x2;
const MSG_TRUNC: u64 = 0x20;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_WAITALL: u64 = 0x100;
const MSG_ERRQUEUE: u64 = 0x2000;
const MSG_NOSIGNAL: u64 = 0x4000;

const SOL_SOCKET: i32 = 1;
const SOL_IP: i32 = 0;
const SOL_TCP: i32 = 6;
const SOL_UDP: i32 = 17;

const SO_REUSEADDR: u64 = 2;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_KEEPALIVE: u64 = 9;
const SO_LINGER: u64 = 13;
const SO_RCVLOWAT: u64 = 18;
const SO_SNDLOWAT: u64 = 19;
const SO_RCVTIMEO_OLD: u64 = 20;
const SO_SNDTIMEO_OLD: u64 = 21;
const SO_ACCEPTCONN: u64 = 30;
const SO_SNDBUFFORCE: u64 = 32;
const SO_RCVBUFFORCE: u64 = 33;
const SO_PROTOCOL: u64 = 38;
const SO_DOMAIN: u64 = 39;
const SO_RCVTIMEO_NEW: u64 = 66;
const SO_SNDTIMEO_NEW: u64 = 67;
/// Socket options kept as an int and otherwise without effect here
/// (dontroute, broadcast, oobinline, no_check, priority, reuseport,
/// timestamp, busy_poll).
const PLAIN_SOCKET: [u64; 8] = [5, 6, 10, 11, 12, 15, 29, 46];

const TCP_NODELAY: u64 = 1;
const TCP_MAXSEG: u64 = 2;
const TCP_KEEPIDLE: u64 = 4;
const TCP_KEEPINTVL: u64 = 5;
const TCP_KEEPCNT: u64 = 6;
/// TCP options kept as an int (cork, syncnt, linger2, defer_accept,
/// window_clamp, quickack, user_timeout, fastopen, notsent_lowat,
/// fastopen_connect).
const PLAIN_TCP: [u64; 10] = [3, 7, 8, 9, 10, 12, 18, 23, 25, 30];

const IP_TOS: u64 = 1;
const IP_TTL: u64 = 2;
const IP_MTU: u64 = 14;
/// IP options kept as an int (hdrincl, recvopts, retopts, pktinfo,
/// mtu_discover, recverr, recvttl, recvtos, freebind, multicast_ttl,
/// multicast_loop, bind_address_no_port).
const PLAIN_IP: [u64; 12] = [3, 6, 7, 8, 10, 11, 12, 13, 15, 33, 34, 24];
/// UDP options kept as an int (cork, segment, gro).
const PLAIN_UDP: [u64; 3] = [1, 103, 104];

/// The bounds Linux applies to SO_SNDBUF and SO_RCVBUF (wmem_max,
/// rmem_max, and the minimums), which it doubles.
const BUF_MAX: i32 = 212992;
const MIN_SNDBUF: i32 = 4608;
const MIN_RCVBUF: i32 = 2304;

const FIONREAD: u64 = 0x541b;
const SIOCOUTQ: u64 = 0x5411;
const SIOCATMARK: u64 = 0x8905;
const SIOCOUTQNSD: u64 = 0x894b;

/// socket(AF_INET, ty, protocol).
pub fn socket(ty: u64, protocol: u64) -> Result<i64, i64> {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let kind = match (ty & SOCK_TYPE_MASK, protocol) {
        (SOCK_STREAM, IPPROTO_IP | IPPROTO_TCP) => Kind::Tcp,
        (SOCK_DGRAM, IPPROTO_IP | IPPROTO_UDP) => Kind::Udp,
        // Ping sockets (net.ipv4.ping_group_range admits nobody).
        (SOCK_DGRAM, IPPROTO_ICMP) => return Err(EACCES),
        // Raw ICMP for ping; everyone is root here.
        (SOCK_RAW, IPPROTO_ICMP) => Kind::RawIcmp,
        (SOCK_STREAM | SOCK_DGRAM | SOCK_RAW, _) => return Err(EPROTONOSUPPORT),
        _ => return Err(ESOCKTNOSUPPORT),
    };
    install(&InetSock::create(kind)?, ty)
}

/// A new placeholder for `sock`: its descriptor (open flags `flags`). If
/// none can be made, the socket goes.
fn install(sock: &Arc<InetSock>, flags: u64) -> Result<i64, i64> {
    let id = files::new_id();
    sock.set_id(id);
    let open = O_RDWR | (flags as u32 & (O_NONBLOCK | O_CLOEXEC));
    match files::install(id, File::Inet(sock.clone()), open, sock.readiness_now()) {
        Ok(fd) => {
            // What changed before the placeholder existed.
            sock.report_now();
            Ok(fd)
        }
        Err(e) => {
            sock.release(true);
            Err(e)
        }
    }
}

/// A sockaddr_in of the program's: None for AF_UNSPEC (with `unspec`),
/// EINVAL if shorter than one, EAFNOSUPPORT for another family.
fn read_addr(addr: u64, len: u64, unspec: bool) -> Result<Option<Endpoint>, i64> {
    if addr == 0 {
        return Err(EFAULT);
    }
    if (len as i64) < 2 {
        return Err(EINVAL);
    }
    let family: u16 = usercopy::read(addr)?;
    if family == AF_UNSPEC && unspec {
        return Ok(None);
    }
    if len < 16 {
        return Err(EINVAL);
    }
    let raw: [u8; 8] = usercopy::read(addr)?;
    let ep = Endpoint { port: u16::from_be_bytes([raw[2], raw[3]]), addr: u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]) };
    // bind(2) takes AF_UNSPEC with INADDR_ANY as AF_INET (Linux's
    // compatibility rule).
    if family != AF_INET && !(family == AF_UNSPEC && ep.addr == 0) {
        return Err(EAFNOSUPPORT);
    }
    Ok(Some(ep))
}

/// A sockaddr_in of an endpoint.
fn sockaddr(ep: Endpoint) -> [u8; 16] {
    let mut raw = [0u8; 16];
    raw[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    raw[2..4].copy_from_slice(&ep.port.to_be_bytes());
    raw[4..8].copy_from_slice(&ep.addr.to_be_bytes());
    raw
}

/// Writes `ep`'s sockaddr_in to `addr` (at most the int at `len` says)
/// and its full length to `len`.
fn put_addr(ep: Endpoint, addr: u64, len: u64) -> Result<(), i64> {
    let raw = sockaddr(ep);
    let cap: i32 = usercopy::read(len)?;
    if cap < 0 {
        return Err(EINVAL);
    }
    let n = raw.len().min(cap as usize);
    usercopy::to_program(addr, &raw[..n])?;
    usercopy::write(len, &(raw.len() as i32))
}

const SYS_CONNECT: u64 = 42;
const SYS_ACCEPT: u64 = 43;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_SENDMSG: u64 = 46;
const SYS_RECVMSG: u64 = 47;
const SYS_SHUTDOWN: u64 = 48;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_GETSOCKNAME: u64 = 51;
const SYS_GETPEERNAME: u64 = 52;
const SYS_SETSOCKOPT: u64 = 54;
const SYS_ACCEPT4: u64 = 288;
const SYS_RECVMMSG: u64 = 299;
const SYS_SENDMMSG: u64 = 307;

/// A socket call `nr` (args `a`) on `sock`, whose descriptor has open
/// flags `flags`.
pub fn call(nr: u64, sock: &Arc<InetSock>, flags: u32, a: [u64; 6]) -> Result<i64, i64> {
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        SYS_CONNECT => sock.connect(read_addr(a[1], a[2], true)?, nonblock).map(|_| 0),
        SYS_ACCEPT => accept4(sock, nonblock, a[1], a[2], 0),
        SYS_ACCEPT4 => accept4(sock, nonblock, a[1], a[2], a[3]),
        SYS_SENDTO => sendto(sock, flags, a[1], a[2], a[3], a[4], a[5]),
        SYS_RECVFROM => recvfrom(sock, flags, a[1], a[2], a[3], a[4], a[5]),
        SYS_SENDMSG => sendmsg(sock, flags, a[1], a[2]).map(|n| n as i64),
        SYS_RECVMSG => recvmsg(sock, flags, a[1], a[2]).map(|n| n as i64),
        SYS_SENDMMSG => sockcalls::sendmmsg_with(a[1], a[2], a[3], |at, f| sendmsg(sock, flags, at, f)),
        SYS_RECVMMSG => sockcalls::recvmmsg_with(a[1], a[2], a[3], a[4], |at, f| recvmsg(sock, flags, at, f)),
        SYS_SHUTDOWN => sock.shutdown(a[1]).map(|_| 0),
        SYS_BIND => {
            let at = read_addr(a[1], a[2], false)?.ok_or(EAFNOSUPPORT)?;
            sock.bind(at).map(|_| 0)
        }
        SYS_LISTEN => sock.listen(a[1] as i32).map(|_| 0),
        SYS_GETSOCKNAME => sock.name(false).and_then(|ep| put_addr(ep, a[1], a[2])).map(|_| 0),
        SYS_GETPEERNAME => sock.name(true).and_then(|ep| put_addr(ep, a[1], a[2])).map(|_| 0),
        SYS_SETSOCKOPT => setsockopt(sock, a[1], a[2], a[3], a[4]),
        _ => getsockopt(sock, a[1], a[2], a[3], a[4]),
    }
}

fn accept4(sock: &Arc<InetSock>, nonblock: bool, addr: u64, len: u64, aflags: u64) -> Result<i64, i64> {
    if aflags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let (conn, peer) = sock.accept(nonblock)?;
    // The address first: a descriptor is only made for a connection that
    // is handed out (else it is closed, as on Linux).
    if addr != 0 {
        if let Err(e) = put_addr(peer, addr, len) {
            conn.release(true);
            return Err(e);
        }
    }
    install(&conn, aflags)
}

/// send(2)'s flags: whether it may not wait; EOPNOTSUPP for out-of-band
/// data.
fn send_flags(fflags: u32, flags: u64) -> Result<bool, i64> {
    if flags & MSG_OOB != 0 {
        return Err(EOPNOTSUPP);
    }
    Ok(fflags & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0)
}

/// The common part of the sends: EPIPE raises SIGPIPE unless MSG_NOSIGNAL.
fn send_common(sock: &Arc<InetSock>, fflags: u32, src: &mut Source, to: Option<Endpoint>, flags: u64) -> Result<usize, i64> {
    let nonblock = send_flags(fflags, flags)?;
    let r = sock.send(src, to, nonblock);
    if r == Err(EPIPE) && flags & MSG_NOSIGNAL == 0 {
        sigpipe();
    }
    r
}

fn sendto(sock: &Arc<InetSock>, fflags: u32, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> Result<i64, i64> {
    let to = if addr != 0 { read_addr(addr, alen, false)? } else { None };
    let vecs = [(buf, len)];
    send_common(sock, fflags, &mut Source::program(&vecs), to, flags).map(|n| n as i64)
}

/// Control data on an internet socket: SOL_SOCKET's (descriptors,
/// credentials) are AF_UNIX's alone (EINVAL), the IP levels' are ignored.
fn check_control(control: u64, len: u64) -> Result<(), i64> {
    if len == 0 {
        return Ok(());
    }
    if len > 20480 {
        return Err(ENOBUFS);
    }
    let mut buf = alloc::vec![0u8; len as usize];
    usercopy::from_program(control, &mut buf)?;
    let mut off = 0usize;
    while off + 16 <= buf.len() {
        let clen = u64::from_le_bytes(buf[off..off + 8].try_into().expect("8 bytes")) as usize;
        let level = i32::from_le_bytes(buf[off + 8..off + 12].try_into().expect("4 bytes"));
        if clen < 16 || clen > buf.len() - off || level == SOL_SOCKET {
            return Err(EINVAL);
        }
        off += (clen + 7) & !7;
    }
    Ok(())
}

fn sendmsg(sock: &Arc<InetSock>, fflags: u32, msg: u64, flags: u64) -> Result<usize, i64> {
    let h: MsgHdr = usercopy::read(msg)?;
    let to = if h.name != 0 && h.namelen != 0 { read_addr(h.name, h.namelen as u64, false)? } else { None };
    let vecs = files::iovecs(h.iov, h.iovlen)?;
    check_control(h.control, h.controllen)?;
    send_common(sock, fflags, &mut Source::program(&vecs), to, flags)
}

/// recv(2)'s work into `dst`: what it got.
fn recv_common(sock: &Arc<InetSock>, fflags: u32, dst: &mut Sink, flags: u64) -> Result<Received, i64> {
    if flags & MSG_ERRQUEUE != 0 {
        // No error queue: nothing in it.
        return Err(EAGAIN);
    }
    if flags & MSG_OOB != 0 {
        return Err(EINVAL);
    }
    let o = RecvOpts { peek: flags & MSG_PEEK != 0, waitall: flags & MSG_WAITALL != 0, nonblock: fflags & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0 };
    sock.recv(dst, o)
}

/// What a receive returns: the bytes, or with MSG_TRUNC a datagram's
/// whole length.
fn returned(r: &Received, flags: u64) -> usize {
    if flags & MSG_TRUNC != 0 { r.len } else { r.copied }
}

fn recvfrom(sock: &Arc<InetSock>, fflags: u32, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> Result<i64, i64> {
    let vecs = [(buf, len)];
    let r = recv_common(sock, fflags, &mut Sink::program(&vecs), flags)?;
    if addr != 0 && alen != 0 {
        match r.from {
            Some(ep) => put_addr(ep, addr, alen)?,
            // A stream says nothing of its sender.
            None => usercopy::write(alen, &0i32)?,
        }
    }
    Ok(returned(&r, flags) as i64)
}

fn recvmsg(sock: &Arc<InetSock>, fflags: u32, msg: u64, flags: u64) -> Result<usize, i64> {
    let h: MsgHdr = usercopy::read(msg)?;
    let vecs = files::iovecs(h.iov, h.iovlen)?;
    let r = recv_common(sock, fflags, &mut Sink::program(&vecs), flags)?;
    let namelen = match (r.from, h.name) {
        (Some(ep), name) if name != 0 => {
            let raw = sockaddr(ep);
            let k = raw.len().min(h.namelen as usize);
            usercopy::to_program(name, &raw[..k])?;
            raw.len() as u32
        }
        _ => 0,
    };
    let mut out_flags = 0u32;
    if r.len > r.copied {
        out_flags |= MSG_TRUNC as u32;
    }
    usercopy::write(msg + 8, &namelen)?;
    usercopy::write(msg + 40, &0u64)?;
    usercopy::write(msg + 48, &out_flags)?;
    Ok(returned(&r, flags))
}

// Options.

/// Linux's doubling of SO_SNDBUF and SO_RCVBUF, within the bounds.
fn buffer(v: i32, min: i32) -> i32 {
    (v.clamp(0, BUF_MAX) * 2).max(min)
}

fn setsockopt(sock: &Arc<InetSock>, level: u64, name: u64, val: u64, len: u64) -> Result<i64, i64> {
    let level = level as i32;
    let int = || sockcalls::opt_int(val, len);
    let tcp = sock.kind == Kind::Tcp;
    match (level, name) {
        (SOL_SOCKET, SO_REUSEADDR) => sock.st.lock().opts.reuseaddr = int()? != 0,
        (SOL_SOCKET, SO_KEEPALIVE) => {
            let on = int()? != 0;
            if tcp {
                // Probes after TCP_KEEPIDLE of silence (smoltcp keeps one
                // interval: also between probes).
                let idle = sock.st.lock().opts.keepidle.clamp(1, 32767) as u64 * 1000;
                sock.setopt(opt::KEEPALIVE, if on { idle } else { 0 })?;
            }
            sock.st.lock().opts.keepalive = on;
        }
        (SOL_SOCKET, SO_SNDBUF | SO_SNDBUFFORCE) => sock.st.lock().opts.sndbuf = buffer(int()?, MIN_SNDBUF),
        (SOL_SOCKET, SO_RCVBUF | SO_RCVBUFFORCE) => sock.st.lock().opts.rcvbuf = buffer(int()?, MIN_RCVBUF),
        (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => sock.st.lock().opts.rcvtimeo = sockcalls::opt_timeout(val, len)?,
        (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => sock.st.lock().opts.sndtimeo = sockcalls::opt_timeout(val, len)?,
        (SOL_SOCKET, SO_RCVLOWAT) => {
            let v = int()?;
            sock.st.lock().opts.rcvlowat = if v < 0 { i32::MAX } else { v.max(1) };
        }
        (SOL_SOCKET, SO_LINGER) => {
            if len < 8 {
                return Err(EINVAL);
            }
            let l: [i32; 2] = usercopy::read(val)?;
            sock.st.lock().opts.linger = (l[0] != 0).then_some(l[1].max(0));
        }
        (SOL_SOCKET, n) if PLAIN_SOCKET.contains(&n) => {
            let v = int()?;
            sock.st.lock().opts.plain.insert((level, n), v);
        }
        (SOL_TCP, _) if !tcp => return Err(ENOPROTOOPT),
        (SOL_TCP, TCP_NODELAY) => {
            let on = int()? != 0;
            sock.setopt(opt::NODELAY, on as u64)?;
            sock.st.lock().opts.nodelay = on;
        }
        (SOL_TCP, TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT) => {
            let v = int()?;
            if v < 1 || v > 32767 {
                return Err(EINVAL);
            }
            let keepalive = {
                let mut l = sock.st.lock();
                match name {
                    TCP_KEEPIDLE => l.opts.keepidle = v,
                    TCP_KEEPINTVL => l.opts.keepintvl = v,
                    _ => l.opts.keepcnt = v,
                }
                (name == TCP_KEEPIDLE && l.opts.keepalive).then_some(l.opts.keepidle as u64 * 1000)
            };
            if let Some(ms) = keepalive {
                sock.setopt(opt::KEEPALIVE, ms)?;
            }
        }
        (SOL_TCP, TCP_MAXSEG) => {
            int()?;
        }
        (SOL_TCP, n) if PLAIN_TCP.contains(&n) => {
            let v = int()?;
            sock.st.lock().opts.plain.insert((level, n), v);
        }
        (SOL_IP, IP_TTL) => {
            let v = int()?;
            // -1: the default.
            let ttl = if v == -1 { 64 } else { v };
            if !(1..=255).contains(&ttl) {
                return Err(EINVAL);
            }
            sock.setopt(opt::TTL, ttl as u64)?;
            sock.st.lock().opts.ttl = ttl;
        }
        (SOL_IP, IP_TOS) => {
            let v = int()?;
            sock.st.lock().opts.plain.insert((level, name), v & 0xff);
        }
        (SOL_IP, n) if PLAIN_IP.contains(&n) => {
            let v = int()?;
            sock.st.lock().opts.plain.insert((level, n), v);
        }
        (SOL_UDP, n) if sock.kind == Kind::Udp && PLAIN_UDP.contains(&n) => {
            let v = int()?;
            sock.st.lock().opts.plain.insert((level, n), v);
        }
        (SOL_SOCKET, _) => return Err(ENOPROTOOPT),
        _ => return Err(ENOPROTOOPT),
    }
    Ok(0)
}

fn getsockopt(sock: &Arc<InetSock>, level: u64, name: u64, val: u64, lenp: u64) -> Result<i64, i64> {
    let level = level as i32;
    let cap: i32 = usercopy::read(lenp)?;
    if cap < 0 {
        return Err(EINVAL);
    }
    let int = |v: i64| (v as i32).to_le_bytes().to_vec();
    let timeval = |ns: u64| {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&((ns / 1_000_000_000) as i64).to_le_bytes());
        b.extend_from_slice(&((ns % 1_000_000_000 / 1000) as i64).to_le_bytes());
        b
    };
    let tcp = sock.kind == Kind::Tcp;
    let o = sock.st.lock().opts.clone();
    let bytes = match (level, name) {
        (SOL_SOCKET, SO_TYPE) => int(match sock.kind {
            Kind::Tcp => SOCK_STREAM,
            Kind::Udp => SOCK_DGRAM,
            Kind::RawIcmp => SOCK_RAW,
        } as i64),
        (SOL_SOCKET, SO_DOMAIN) => int(AF_INET as i64),
        (SOL_SOCKET, SO_PROTOCOL) => int(match sock.kind {
            Kind::Tcp => IPPROTO_TCP,
            Kind::Udp => IPPROTO_UDP,
            Kind::RawIcmp => IPPROTO_ICMP,
        } as i64),
        (SOL_SOCKET, SO_ERROR) => int(sock.take_error()),
        (SOL_SOCKET, SO_ACCEPTCONN) => int(sock.listening() as i64),
        (SOL_SOCKET, SO_REUSEADDR) => int(o.reuseaddr as i64),
        (SOL_SOCKET, SO_KEEPALIVE) => int(o.keepalive as i64),
        (SOL_SOCKET, SO_SNDBUF) => int(o.sndbuf as i64),
        (SOL_SOCKET, SO_RCVBUF) => int(o.rcvbuf as i64),
        (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => timeval(o.rcvtimeo),
        (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => timeval(o.sndtimeo),
        (SOL_SOCKET, SO_RCVLOWAT) => int(o.rcvlowat as i64),
        (SOL_SOCKET, SO_SNDLOWAT) => int(1),
        (SOL_SOCKET, SO_LINGER) => {
            let mut b = int(o.linger.is_some() as i64);
            b.extend_from_slice(&o.linger.unwrap_or(0).to_le_bytes());
            b
        }
        (SOL_SOCKET, n) if PLAIN_SOCKET.contains(&n) => int(o.plain.get(&(level, n)).copied().unwrap_or(0) as i64),
        (SOL_TCP, _) if !tcp => return Err(ENOPROTOOPT),
        (SOL_TCP, TCP_NODELAY) => int(o.nodelay as i64),
        (SOL_TCP, TCP_KEEPIDLE) => int(o.keepidle as i64),
        (SOL_TCP, TCP_KEEPINTVL) => int(o.keepintvl as i64),
        (SOL_TCP, TCP_KEEPCNT) => int(o.keepcnt as i64),
        // The MSS of a connection on the 1500-byte links; 536 before one.
        (SOL_TCP, TCP_MAXSEG) => int(if sock.name(true).is_ok() { 1460 } else { 536 }),
        (SOL_TCP, n) if PLAIN_TCP.contains(&n) => int(o.plain.get(&(level, n)).copied().unwrap_or(0) as i64),
        (SOL_IP, IP_TTL) => int(o.ttl as i64),
        (SOL_IP, IP_TOS) => int(o.plain.get(&(level, name)).copied().unwrap_or(0) as i64),
        (SOL_IP, IP_MTU) => {
            if sock.name(true).is_err() {
                return Err(ENOTCONN);
            }
            int(1500)
        }
        (SOL_IP, n) if PLAIN_IP.contains(&n) => int(o.plain.get(&(level, n)).copied().unwrap_or(0) as i64),
        (SOL_UDP, n) if sock.kind == Kind::Udp && PLAIN_UDP.contains(&n) => int(o.plain.get(&(level, n)).copied().unwrap_or(0) as i64),
        _ => return Err(ENOPROTOOPT),
    };
    let n = bytes.len().min(cap as usize);
    usercopy::to_program(val, &bytes[..n])?;
    usercopy::write(lenp, &(n as i32))?;
    Ok(0)
}

// The calls every file has.

/// read, write, readv, writev, fstat, ioctl and the rest on a socket's
/// descriptor (`flags`: its open flags).
pub fn on_file(nr: u64, sock: &Arc<InetSock>, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    match nr {
        files::SYS_READ => recv_common(sock, flags, &mut Sink::program(&[(a1, a2)]), 0).map(|r| r.copied as i64),
        files::SYS_READV => {
            let vecs = files::iovecs(a1, a2)?;
            recv_common(sock, flags, &mut Sink::program(&vecs), 0).map(|r| r.copied as i64)
        }
        files::SYS_WRITE => send_common(sock, flags, &mut Source::program(&[(a1, a2)]), None, 0).map(|n| n as i64),
        files::SYS_WRITEV => {
            let vecs = files::iovecs(a1, a2)?;
            send_common(sock, flags, &mut Source::program(&vecs), None, 0).map(|n| n as i64)
        }
        files::SYS_FSTAT => {
            if a1 == 0 {
                return Err(EFAULT);
            }
            usercopy::to_program(a1, &stat(sock)).map(|_| 0)
        }
        files::SYS_IOCTL => match a1 {
            FIONREAD => usercopy::write(a2, &(sock.inq()? as i32)).map(|_| 0),
            SIOCOUTQ | SIOCOUTQNSD => usercopy::write(a2, &(sock.outq() as i32)).map(|_| 0),
            SIOCATMARK => usercopy::write(a2, &0i32).map(|_| 0),
            _ => Err(ENOTTY),
        },
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(ESPIPE),
        files::SYS_GETDENTS64 => Err(files::ENOTDIR),
        _ => Err(EINVAL),
    }
}

/// Reads into the server's memory (sendfile from a socket).
pub fn read_server(sock: &Arc<InetSock>, flags: u32, buf: &mut [u8]) -> Result<i64, i64> {
    recv_common(sock, flags, &mut Sink::Server { buf, at: 0 }, 0).map(|r| r.copied as i64)
}

/// Writes the server's memory (sendfile to a socket).
pub fn write_server(sock: &Arc<InetSock>, flags: u32, buf: &[u8]) -> Result<i64, i64> {
    send_common(sock, flags, &mut Source::Server { buf, at: 0 }, None, 0).map(|n| n as i64)
}

/// Its `struct stat`: a socket inode of sockfs.
pub fn stat(sock: &InetSock) -> [u8; 144] {
    const SOCKFS_DEV: u64 = 0x8;
    let mut st = [0u8; 144];
    st[0..8].copy_from_slice(&SOCKFS_DEV.to_le_bytes());
    st[8..16].copy_from_slice(&sock.id().to_le_bytes());
    st[16..24].copy_from_slice(&1u64.to_le_bytes());
    st[24..28].copy_from_slice(&(vfs::S_IFSOCK | 0o777).to_le_bytes());
    st[56..64].copy_from_slice(&4096u64.to_le_bytes());
    st
}

const ENOBUFS: i64 = 105;
