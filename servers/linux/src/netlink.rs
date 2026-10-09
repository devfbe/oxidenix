//! Netlink sockets (netlink(7)), protocol NETLINK_ROUTE: files of the
//! server with a placeholder in the kernel's descriptor table, as pipes.
//! What a program sends to the kernel's end (port 0) is answered here
//! (`netlink::answer`, rtnetlink(7)) from the interfaces netd describes
//! (`netdev`); the answers wait in the socket's queue for recv.
//! Sockets of the instance can also send each other datagrams by port.
//!
//! As Linux: datagrams keep their boundaries (a short buffer truncates
//! one, MSG_TRUNC tells), an unbound socket is bound to a port of its own
//! when it first sends (autobind), and answers that do not fit the
//! receive buffer are dropped with ENOBUFS at the next receive. Port ids
//! are the ones Linux gives when the process id is taken (-4096 and down):
//! the server learns process ids with R8. No multicast group ever carries
//! a message here (netd's configuration changes are not announced).

use crate::files::{self, File, EBADF, EFAULT, EINVAL, O_ACCMODE, O_NONBLOCK, O_RDWR, O_WRONLY};
use crate::sync::Mutex;
use crate::syscall;
use crate::usercopy;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use restricted::*;

pub const AF_NETLINK: u64 = 16;
const NETLINK_ROUTE: u64 = 0;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;

const MSG_PEEK: u64 = 0x2;
const MSG_TRUNC: u64 = 0x20;
const MSG_DONTWAIT: u64 = 0x40;

const SOL_SOCKET: u64 = 1;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_SNDBUFFORCE: u64 = 32;
const SO_RCVBUFFORCE: u64 = 33;
const SO_PROTOCOL: u64 = 38;
const SO_DOMAIN: u64 = 39;
const SOL_NETLINK: u64 = 270;
const NETLINK_ADD_MEMBERSHIP: u64 = 1;
const NETLINK_DROP_MEMBERSHIP: u64 = 2;
const NETLINK_PKTINFO: u64 = 3;
const NETLINK_BROADCAST_ERROR: u64 = 4;
const NETLINK_NO_ENOBUFS: u64 = 5;
const NETLINK_LISTEN_ALL_NSID: u64 = 8;
const NETLINK_LIST_MEMBERSHIPS: u64 = 9;
const NETLINK_CAP_ACK: u64 = 10;
const NETLINK_EXT_ACK: u64 = 11;
const NETLINK_GET_STRICT_CHK: u64 = 12;

const EAGAIN: i64 = 11;
const EINTR: i64 = 4;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const EMSGSIZE: i64 = 90;
const ENOPROTOOPT: i64 = 92;
const EPROTONOSUPPORT: i64 = 93;
const ESOCKTNOSUPPORT: i64 = 94;
const EOPNOTSUPP: i64 = 95;
const EADDRINUSE: i64 = 98;
const ENOBUFS: i64 = 105;
const ECONNREFUSED: i64 = 111;

const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;

/// Linux's default socket buffer sizes (net.core.rmem_default and
/// wmem_default), their largest (rmem_max and wmem_max: also for the
/// FORCE options here, which Linux lets privileged callers take beyond),
/// and the least SO_RCVBUF/SO_SNDBUF make of a request.
const DEFAULT_BUF: u32 = 212_992;
const MAX_BUF: u32 = 212_992;
const MIN_RCVBUF: u32 = 2304;
const MIN_SNDBUF: u32 = 4608;

/// `struct sockaddr_nl`: family, padding, port id, groups.
const SOCKADDR_NL: usize = 12;

/// A datagram waiting to be received, and the port it came from (0: the
/// kernel's end).
struct Datagram {
    from: u32,
    data: Vec<u8>,
}

struct State {
    /// The bound port id; 0 until bound.
    port: u32,
    /// Multicast groups subscribed to (bit n-1 for group n).
    groups: u32,
    /// connect's default destination.
    dst_port: u32,
    queue: VecDeque<Datagram>,
    queued: usize,
    /// An error for the next receive (ENOBUFS after a dropped answer).
    error: i64,
    rcvbuf: u32,
    sndbuf: u32,
    /// The options that only answer what they were set to.
    options: u32,
    reported: i16,
    /// A dump the kernel's end is producing as the receive buffer has
    /// room (`netlink::Dump`).
    dump: Option<netlink::Dump>,
}

pub struct NetlinkSocket {
    id: u64,
    ty: u64,
    state: Mutex<State>,
    /// Bumped on every change of the queue; receivers sleep on it.
    seq: AtomicU32,
}

/// The instance's bound sockets by port id.
static PORTS: Mutex<BTreeMap<u32, Weak<NetlinkSocket>>> = Mutex::new(BTreeMap::new());
/// The next port id autobind tries (Linux's rover).
static ROVER: Mutex<i32> = Mutex::new(-4096);

/// socket(AF_NETLINK, type, protocol).
pub fn socket(ty: u64, protocol: u64) -> Result<i64, i64> {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let kind = ty & SOCK_TYPE_MASK;
    if kind != SOCK_RAW && kind != SOCK_DGRAM {
        return Err(ESOCKTNOSUPPORT);
    }
    if protocol != NETLINK_ROUTE {
        return Err(EPROTONOSUPPORT);
    }
    let state = State {
        port: 0,
        groups: 0,
        dst_port: 0,
        queue: VecDeque::new(),
        queued: 0,
        error: 0,
        rcvbuf: DEFAULT_BUF,
        sndbuf: DEFAULT_BUF,
        options: 0,
        reported: POLLOUT,
        dump: None,
    };
    let socket = Arc::new(NetlinkSocket { id: files::new_id(), ty: kind, state: Mutex::new(state), seq: AtomicU32::new(0) });
    let flags = O_RDWR | (ty & (SOCK_NONBLOCK | SOCK_CLOEXEC)) as u32;
    files::install(socket.id, File::Netlink(socket.clone()), flags, POLLOUT)
}

/// A `struct sockaddr_nl` of the program's: (port id, groups).
fn read_addr(addr: u64, len: u64) -> Result<(u32, u32), i64> {
    if len < SOCKADDR_NL as u64 {
        return Err(EINVAL);
    }
    let raw: [u8; SOCKADDR_NL] = usercopy::read(addr)?;
    if u16::from_le_bytes([raw[0], raw[1]]) != AF_NETLINK as u16 {
        return Err(EINVAL);
    }
    Ok((u32::from_le_bytes(raw[4..8].try_into().expect("4 bytes")), u32::from_le_bytes(raw[8..12].try_into().expect("4 bytes"))))
}

/// Writes a `struct sockaddr_nl` to the program's (addr, *len), truncated
/// to the room it has, and its full length to *len.
fn write_addr(addr: u64, len_ptr: u64, port: u32, groups: u32) -> Result<(), i64> {
    if addr == 0 || len_ptr == 0 {
        return Ok(());
    }
    let room: u32 = usercopy::read(len_ptr)?;
    if (room as i32) < 0 {
        return Err(EINVAL);
    }
    let mut raw = [0u8; SOCKADDR_NL];
    raw[0..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
    raw[4..8].copy_from_slice(&port.to_le_bytes());
    raw[8..12].copy_from_slice(&groups.to_le_bytes());
    usercopy::to_program(addr, &raw[..(room as usize).min(SOCKADDR_NL)])?;
    usercopy::write(len_ptr, &(SOCKADDR_NL as u32))
}

/// The fields of a `struct msghdr` a call uses.
struct MsgHdr {
    name: u64,
    namelen: u32,
    iov: Vec<(u64, u64)>,
}

fn read_msghdr(msg: u64) -> Result<MsgHdr, i64> {
    let raw: [u64; 7] = usercopy::read(msg)?;
    Ok(MsgHdr { name: raw[0], namelen: raw[1] as u32, iov: files::iovecs(raw[2], raw[3])? })
}

impl NetlinkSocket {
    fn readiness(st: &State) -> i16 {
        let mut r = POLLOUT;
        if !st.queue.is_empty() {
            r |= POLLIN;
        }
        if st.error != 0 {
            r |= POLLERR;
        }
        r
    }

    /// After a change (lock held): wake the receivers, report readiness
    /// (each new datagram is an event, for EPOLLET).
    fn changed(&self, st: &mut State, arrived: bool) {
        self.seq.fetch_add(1, Ordering::Release);
        let word = &self.seq as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAKE, [word, i32::MAX as u64, 0, 0, 0, 0]);
        let now = Self::readiness(st);
        if now != st.reported || arrived {
            st.reported = now;
            files::ready(self.id, now);
        }
    }

    fn wait(&self, seen: u32) -> Result<(), i64> {
        let word = &self.seq as *const AtomicU32 as u64;
        match syscall(SYS_SERVER_FUTEX_WAIT, [word, seen as u64, 0, FUTEX_INTERRUPTIBLE, 0, 0]) {
            r if r == -EINTR => Err(EINTR),
            _ => Ok(()),
        }
    }

    /// Queues datagrams from port `from`; what does not fit the receive
    /// buffer is dropped, and the next receive fails with ENOBUFS (unless
    /// NETLINK_NO_ENOBUFS).
    fn deliver(&self, from: u32, datagrams: Vec<Vec<u8>>) {
        if datagrams.is_empty() {
            return;
        }
        let mut st = self.state.lock();
        for data in datagrams {
            if st.queued + data.len() > st.rcvbuf as usize {
                Self::overrun(&mut st);
                continue;
            }
            st.queued += data.len();
            st.queue.push_back(Datagram { from, data });
        }
        self.changed(&mut st, true);
    }

    /// Answers were dropped for want of room: ENOBUFS at the next receive
    /// (unless NETLINK_NO_ENOBUFS).
    fn overrun(st: &mut State) {
        if st.options & (1 << NETLINK_NO_ENOBUFS) == 0 {
            st.error = ENOBUFS;
        }
    }

    /// Goes on with a running dump while the receive buffer has room (as
    /// Linux's netlink_dump: at the request and after each receive that
    /// leaves the buffer at most half full).
    fn fill_dump(&self, interfaces: &[netlink::Interface]) {
        let mut st = self.state.lock();
        let mut added = false;
        while st.queued < st.rcvbuf as usize {
            let Some(dump) = st.dump.as_mut() else { break };
            match dump.next(interfaces) {
                Some(data) => {
                    st.queued += data.len();
                    st.queue.push_back(Datagram { from: 0, data });
                    added = true;
                }
                None => {}
            }
            if st.dump.as_ref().is_none_or(|d| d.is_done()) {
                st.dump = None;
            }
        }
        if added {
            self.changed(&mut st, true);
        }
    }

    /// Binds to `port`, or to a free port of its own for 0 (autobind).
    /// A bound socket keeps its port (EINVAL for another).
    fn bind_port(self: &Arc<Self>, port: u32) -> Result<u32, i64> {
        let mut st = self.state.lock();
        if st.port != 0 {
            return if port == 0 || port == st.port { Ok(st.port) } else { Err(EINVAL) };
        }
        let mut ports = PORTS.lock();
        let chosen = if port != 0 {
            if ports.get(&port).is_some_and(|w| w.strong_count() > 0) {
                return Err(EADDRINUSE);
            }
            port
        } else {
            let mut rover = ROVER.lock();
            loop {
                let candidate = *rover as u32;
                *rover = if *rover <= i32::MIN + 1 { -4096 } else { *rover - 1 };
                if !ports.get(&candidate).is_some_and(|w| w.strong_count() > 0) {
                    break candidate;
                }
            }
        };
        ports.insert(chosen, Arc::downgrade(self));
        st.port = chosen;
        Ok(chosen)
    }

    /// Whether a datagram of `len` bytes may be sent (EMSGSIZE beyond the
    /// send buffer, as Linux): checked before anything is allocated for it.
    fn fits(&self, len: u64) -> Result<(), i64> {
        let sndbuf = self.state.lock().sndbuf;
        if len.saturating_add(32) > sndbuf as u64 {
            return Err(EMSGSIZE);
        }
        Ok(())
    }

    /// Sends `data` to port `to` (0: the kernel's end, which answers it:
    /// no more than its receive buffer has room for, a dump as it reads).
    fn send(self: &Arc<Self>, data: Vec<u8>, to: u32) -> Result<i64, i64> {
        self.fits(data.len() as u64)?;
        let port = self.bind_port(0)?;
        let len = data.len() as i64;
        if to == 0 {
            let interfaces = crate::netdev::interfaces();
            // The answer is made and queued under the lock: two senders at
            // once cannot both start a dump, nor both take the same room.
            {
                let mut st = self.state.lock();
                let (cap_ack, dumping, room) = (st.options & (1 << NETLINK_CAP_ACK) != 0, st.dump.is_some(), (st.rcvbuf as usize).saturating_sub(st.queued));
                let (replies, overrun) = netlink::answer(&data, port, &interfaces, cap_ack, dumping, room);
                let mut added = false;
                for r in replies {
                    match r {
                        netlink::Reply::Datagram(d) => {
                            st.queued += d.len();
                            st.queue.push_back(Datagram { from: 0, data: d });
                            added = true;
                        }
                        netlink::Reply::Dump(d) => st.dump = Some(d),
                    }
                }
                if overrun {
                    Self::overrun(&mut st);
                }
                if added {
                    self.changed(&mut st, true);
                }
            }
            self.fill_dump(&interfaces);
        } else {
            let peer = PORTS.lock().get(&to).and_then(Weak::upgrade).ok_or(ECONNREFUSED)?;
            peer.deliver(port, alloc::vec![data]);
        }
        Ok(len)
    }

    /// Receives the next datagram into `iov`: (bytes copied, the
    /// datagram's length, its sender). The datagram is taken (or, with
    /// MSG_PEEK, copied) under the lock and copied to the program after it
    /// (a fault there may need the pager); a copy that faults loses it, as
    /// on Linux.
    fn recv(&self, iov: &[(u64, u64)], flags: u64, nonblock: bool) -> Result<(usize, usize, u32), i64> {
        let (d, more) = loop {
            let seen;
            {
                let mut st = self.state.lock();
                if st.error != 0 {
                    let e = core::mem::take(&mut st.error);
                    self.changed(&mut st, false);
                    return Err(e);
                }
                if flags & MSG_PEEK != 0 {
                    if let Some(d) = st.queue.front() {
                        break (Datagram { from: d.from, data: d.data.clone() }, false);
                    }
                } else if let Some(d) = st.queue.pop_front() {
                    st.queued -= d.data.len();
                    self.changed(&mut st, false);
                    let more = st.dump.is_some() && st.queued <= st.rcvbuf as usize / 2;
                    break (d, more);
                }
                seen = self.seq.load(Ordering::Acquire);
            }
            if nonblock || flags & MSG_DONTWAIT != 0 {
                return Err(EAGAIN);
            }
            self.wait(seen)?;
        };
        if more {
            self.fill_dump(&crate::netdev::interfaces());
        }
        let mut done = 0;
        for &(base, len) in iov {
            let n = (len as usize).min(d.data.len() - done);
            usercopy::to_program(base, &d.data[done..done + n])?;
            done += n;
        }
        Ok((done, d.data.len(), d.from))
    }

    fn sockopt_u32(&self, level: u64, name: u64) -> Result<u32, i64> {
        let mut st = self.state.lock();
        Ok(match (level, name) {
            (SOL_SOCKET, SO_TYPE) => self.ty as u32,
            (SOL_SOCKET, SO_ERROR) => {
                let e = core::mem::take(&mut st.error);
                self.changed(&mut st, false);
                e as u32
            }
            (SOL_SOCKET, SO_RCVBUF) => st.rcvbuf,
            (SOL_SOCKET, SO_SNDBUF) => st.sndbuf,
            (SOL_SOCKET, SO_PROTOCOL) => NETLINK_ROUTE as u32,
            (SOL_SOCKET, SO_DOMAIN) => AF_NETLINK as u32,
            (SOL_SOCKET, _) => 0,
            (SOL_NETLINK, NETLINK_PKTINFO | NETLINK_BROADCAST_ERROR | NETLINK_NO_ENOBUFS | NETLINK_LISTEN_ALL_NSID | NETLINK_CAP_ACK | NETLINK_EXT_ACK | NETLINK_GET_STRICT_CHK) => {
                (st.options >> name) & 1
            }
            _ => return Err(ENOPROTOOPT),
        })
    }

    fn setsockopt(&self, level: u64, name: u64, val: u64, len: u64) -> Result<i64, i64> {
        let value: u32 = if len >= 4 { usercopy::read(val)? } else { 0 };
        let mut st = self.state.lock();
        match (level, name) {
            (SOL_SOCKET, SO_RCVBUF | SO_RCVBUFFORCE | SO_SNDBUF | SO_SNDBUFFORCE) => {
                if len < 4 {
                    return Err(EINVAL);
                }
                // Linux doubles the request (for its bookkeeping), within
                // rmem_max/wmem_max, and keeps a minimum.
                let doubled = value.min(MAX_BUF) * 2;
                if matches!(name, SO_RCVBUF | SO_RCVBUFFORCE) {
                    st.rcvbuf = doubled.max(MIN_RCVBUF);
                } else {
                    st.sndbuf = doubled.max(MIN_SNDBUF);
                }
            }
            // The other socket options are accepted and change nothing here.
            (SOL_SOCKET, _) => {}
            (SOL_NETLINK, NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP) => {
                if len < 4 {
                    return Err(EINVAL);
                }
                // rtnetlink's groups go up to 32 here; none carries a message.
                if value == 0 || value > 32 {
                    return Err(EINVAL);
                }
                let bit = 1u32 << (value - 1);
                if name == NETLINK_ADD_MEMBERSHIP {
                    st.groups |= bit;
                } else {
                    st.groups &= !bit;
                }
            }
            (SOL_NETLINK, NETLINK_PKTINFO | NETLINK_BROADCAST_ERROR | NETLINK_NO_ENOBUFS | NETLINK_LISTEN_ALL_NSID | NETLINK_CAP_ACK | NETLINK_EXT_ACK | NETLINK_GET_STRICT_CHK) => {
                if len < 4 {
                    return Err(EINVAL);
                }
                if value != 0 {
                    st.options |= 1 << name;
                } else {
                    st.options &= !(1 << name);
                }
            }
            _ => return Err(ENOPROTOOPT),
        }
        Ok(0)
    }

    /// Its `struct stat`: a socket inode, as on Linux.
    pub fn stat(&self) -> [u8; 144] {
        const S_IFSOCK: u32 = 0o140000;
        let mut st = [0u8; 144];
        st[8..16].copy_from_slice(&self.id.to_le_bytes());
        st[16..24].copy_from_slice(&1u64.to_le_bytes());
        st[24..28].copy_from_slice(&(S_IFSOCK | 0o777).to_le_bytes());
        st[56..64].copy_from_slice(&4096u64.to_le_bytes());
        st
    }
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        let port = self.state.lock().port;
        if port != 0 {
            let mut ports = PORTS.lock();
            if ports.get(&port).is_some_and(|w| w.strong_count() == 0) {
                ports.remove(&port);
            }
        }
    }
}

pub const SYS_CONNECT: u64 = 42;
pub const SYS_ACCEPT: u64 = 43;
pub const SYS_SENDTO: u64 = 44;
pub const SYS_RECVFROM: u64 = 45;
pub const SYS_SENDMSG: u64 = 46;
pub const SYS_RECVMSG: u64 = 47;
pub const SYS_SHUTDOWN: u64 = 48;
pub const SYS_BIND: u64 = 49;
pub const SYS_LISTEN: u64 = 50;
pub const SYS_GETSOCKNAME: u64 = 51;
pub const SYS_GETPEERNAME: u64 = 52;
pub const SYS_SETSOCKOPT: u64 = 54;
pub const SYS_GETSOCKOPT: u64 = 55;
pub const SYS_ACCEPT4: u64 = 288;

/// A system call on netlink socket `sock` (descriptor flags `flags`), with
/// the program's arguments `a`.
pub fn call(nr: u64, sock: &Arc<NetlinkSocket>, flags: u32, a: [u64; 6]) -> Result<i64, i64> {
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        files::SYS_READ | files::SYS_READV | SYS_RECVFROM | SYS_RECVMSG => {
            if flags & O_ACCMODE == O_WRONLY {
                return Err(EBADF);
            }
            let (iov, rflags, name, name_len, msg) = match nr {
                files::SYS_READ => (alloc::vec![(a[1], a[2])], 0, 0, 0, 0),
                files::SYS_READV => (files::iovecs(a[1], a[2])?, 0, 0, 0, 0),
                SYS_RECVFROM => (alloc::vec![(a[1], a[2])], a[3], a[4], a[5], 0),
                _ => {
                    let h = read_msghdr(a[1])?;
                    (h.iov, a[2], h.name, a[1] + 8, a[1])
                }
            };
            let (done, full, from) = sock.recv(&iov, rflags, nonblock)?;
            if msg != 0 {
                // msg_namelen as Linux sets it, no control data, and
                // MSG_TRUNC for a datagram longer than the buffers.
                let h = read_msghdr(msg)?;
                if h.name != 0 {
                    let room = (h.namelen as usize).min(SOCKADDR_NL);
                    let mut raw = [0u8; SOCKADDR_NL];
                    raw[0..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
                    raw[4..8].copy_from_slice(&from.to_le_bytes());
                    usercopy::to_program(h.name, &raw[..room])?;
                }
                usercopy::write(name_len, &(SOCKADDR_NL as u32))?;
                usercopy::write(msg + 40, &0u64)?;
                let out_flags: u32 = if done < full { MSG_TRUNC as u32 } else { 0 };
                usercopy::write(msg + 48, &out_flags)?;
            } else if name != 0 {
                write_addr(name, name_len, from, 0)?;
            }
            Ok(if rflags & MSG_TRUNC != 0 { full } else { done } as i64)
        }
        files::SYS_WRITE | files::SYS_WRITEV | SYS_SENDTO | SYS_SENDMSG => {
            if flags & O_ACCMODE == 0 {
                return Err(EBADF);
            }
            let (iov, name, namelen) = match nr {
                files::SYS_WRITE => (alloc::vec![(a[1], a[2])], 0, 0),
                files::SYS_WRITEV => (files::iovecs(a[1], a[2])?, 0, 0),
                SYS_SENDTO => (alloc::vec![(a[1], a[2])], a[4], a[5]),
                _ => {
                    let h = read_msghdr(a[1])?;
                    (h.iov, h.name, h.namelen as u64)
                }
            };
            let to = if name != 0 && namelen != 0 {
                read_addr(name, namelen)?.0
            } else {
                sock.state.lock().dst_port
            };
            let total = iov.iter().try_fold(0u64, |sum, &(_, len)| sum.checked_add(len)).ok_or(EINVAL)?;
            // EMSGSIZE beyond the send buffer, before anything is allocated.
            sock.fits(total)?;
            let mut data = alloc::vec![0u8; total as usize];
            let mut at = 0;
            for (base, len) in iov {
                usercopy::from_program(base, &mut data[at..at + len as usize])?;
                at += len as usize;
            }
            sock.send(data, to)
        }
        SYS_BIND => {
            let (port, groups) = read_addr(a[1], a[2])?;
            sock.bind_port(port)?;
            sock.state.lock().groups = groups;
            Ok(0)
        }
        SYS_CONNECT => {
            let (port, _) = read_addr(a[1], a[2])?;
            sock.bind_port(0)?;
            sock.state.lock().dst_port = port;
            Ok(0)
        }
        SYS_GETSOCKNAME => {
            let (port, groups) = {
                let st = sock.state.lock();
                (st.port, st.groups)
            };
            write_addr(a[1], a[2], port, groups).map(|_| 0)
        }
        SYS_GETPEERNAME => {
            let port = sock.state.lock().dst_port;
            write_addr(a[1], a[2], port, 0).map(|_| 0)
        }
        SYS_GETSOCKOPT => {
            let (level, name, val, len_ptr) = (a[1], a[2], a[3], a[4]);
            let room: u32 = usercopy::read(len_ptr)?;
            if (room as i32) < 0 {
                return Err(EINVAL);
            }
            if (level, name) == (SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS) {
                // The groups as a bit array; its full length to *len.
                let groups = sock.state.lock().groups;
                let n = (room as usize).min(4);
                usercopy::to_program(val, &groups.to_le_bytes()[..n])?;
                return usercopy::write(len_ptr, &4u32).map(|_| 0);
            }
            let value = sock.sockopt_u32(level, name)?;
            if room < 4 {
                return Err(EINVAL);
            }
            usercopy::write(val, &value)?;
            usercopy::write(len_ptr, &4u32).map(|_| 0)
        }
        SYS_SETSOCKOPT => sock.setsockopt(a[1], a[2], a[3], a[4]),
        SYS_LISTEN | SYS_ACCEPT | SYS_ACCEPT4 | SYS_SHUTDOWN => Err(EOPNOTSUPP),
        files::SYS_FSTAT => usercopy::to_program(a[1], &sock.stat()).map(|_| 0).map_err(|_| EFAULT),
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(ESPIPE),
        files::SYS_IOCTL => Err(ENOTTY),
        files::SYS_GETDENTS64 => Err(crate::files::ENOTDIR),
        _ => Err(EINVAL),
    }
}
