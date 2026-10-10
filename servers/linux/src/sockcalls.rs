//! The system calls of AF_UNIX sockets (phase R7a): socket and socketpair
//! for the AF_UNIX family, and every call on a descriptor of one of the
//! server's AF_UNIX sockets: addresses (sockaddr_un, paths in the server's
//! namespace, the abstract namespace), message headers and their ancillary
//! data (SCM_RIGHTS, SCM_CREDENTIALS), options, read and write. The
//! sockets themselves are `unix`. `handle` also routes the socket calls of
//! the other families: internet sockets to `inetcalls` (R7b); no socket
//! call reaches the kernel.

use crate::files::{self, File, O_CLOEXEC, O_NONBLOCK, O_RDWR};
use crate::namespace::{self, Node};
use crate::records;
use crate::scm::Passed;
use crate::syscall;
use crate::unix::*;
use crate::usercopy;
use alloc::sync::Arc;
use alloc::vec::Vec;
use restricted::*;

const SYS_SOCKET: u64 = 41;
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
const SYS_SOCKETPAIR: u64 = 53;
const SYS_SETSOCKOPT: u64 = 54;
const SYS_GETSOCKOPT: u64 = 55;
const SYS_ACCEPT4: u64 = 288;
const SYS_RECVMMSG: u64 = 299;
const SYS_SENDMMSG: u64 = 307;

const ENOENT: i64 = 2;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const EDOM: i64 = 33;
const ENOPROTOOPT: i64 = 92;
const EPROTONOSUPPORT: i64 = 93;
const ESOCKTNOSUPPORT: i64 = 94;
const ESRCH: i64 = 3;


const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_RAW: u32 = 3;
const SOCK_NONBLOCK: u64 = O_NONBLOCK as u64;
const SOCK_CLOEXEC: u64 = O_CLOEXEC as u64;
const AF_UNSPEC: u16 = 0;

const MSG_OOB: u64 = 0x1;
const MSG_PEEK: u64 = 0x2;
const MSG_CTRUNC: u32 = 0x8;
const MSG_TRUNC: u64 = 0x20;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_WAITALL: u64 = 0x100;
const MSG_NOSIGNAL: u64 = 0x4000;
const MSG_WAITFORONE: u64 = 0x10000;
const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
/// Most descriptors one message carries.
const SCM_MAX_FD: usize = 253;
/// Most ancillary data one message carries (net.core.optmem_max).
const OPTMEM_MAX: u64 = 20480;

const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_LINGER: u64 = 13;
const SO_PASSCRED: u64 = 16;
const SO_PEERCRED: u64 = 17;
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
/// Options kept as an int and otherwise without effect here (reuseaddr,
/// dontroute, broadcast, keepalive, oobinline, priority, timestamp).
const PLAIN_OPTS: [u64; 7] = [2, 5, 6, 9, 10, 12, 29];

const FIONREAD: u64 = 0x541b;
const SIOCOUTQ: u64 = 0x5411;

const SOCKADDR_UN_MAX: u64 = 110;

/// The result of a socket call in `s`: every one is the server's (the
/// kernel implements no sockets). socket(2) and socketpair(2) by family
/// (netlink's are `files`', the families nobody implements EAFNOSUPPORT),
/// the calls on a descriptor by the socket behind it (AF_UNIX here,
/// internet sockets `inetcalls`, netlink `files`; ENOTSOCK for another
/// file).
pub fn handle(s: &State) -> Option<i64> {
    const EAFNOSUPPORT: i64 = 97;
    let (a0, a1, a2, a3, a4, a5) = (s.rdi, s.rsi, s.rdx, s.r10, s.r8, s.r9);
    let result = match s.rax {
        SYS_SOCKET if a0 == AF_UNIX as u64 => socket(a1, a2),
        SYS_SOCKET if a0 == crate::inetcalls::AF_INET as u64 => crate::inetcalls::socket(a1, a2),
        SYS_SOCKET => Err(EAFNOSUPPORT),
        SYS_SOCKETPAIR if a0 == AF_UNIX as u64 => socketpair(a1, a2, a3),
        // Internet sockets come in no pairs.
        SYS_SOCKETPAIR if a0 == crate::inetcalls::AF_INET as u64 => Err(EOPNOTSUPP),
        SYS_SOCKETPAIR => Err(EAFNOSUPPORT),
        SYS_CONNECT | SYS_ACCEPT | SYS_SENDTO | SYS_RECVFROM | SYS_SENDMSG | SYS_RECVMSG | SYS_SHUTDOWN | SYS_BIND | SYS_LISTEN
        | SYS_GETSOCKNAME | SYS_GETPEERNAME | SYS_SETSOCKOPT | SYS_GETSOCKOPT | SYS_ACCEPT4 | SYS_RECVMMSG | SYS_SENDMMSG => {
            // (An O_PATH descriptor is no file: EBADF, Linux's fdget. The
            // description stays referenced while the call uses it.)
            let file = match files::lookup(a0) {
                Err(e) => return Some(-e),
                Ok(f) => f,
            };
            let flags = file.flags();
            let sock = match &file.file {
                File::Socket(sock) => sock.clone(),
                File::Inet(sock) => {
                    let r = crate::inetcalls::call(s.rax, sock, flags, [a0, a1, a2, a3, a4, a5]);
                    return Some(r.unwrap_or_else(|e| -e));
                }
                // Another file, of the server's or of the kernel's.
                _ => return Some(-crate::unix::ENOTSOCK),
            };
            match s.rax {
                SYS_CONNECT => connect(&sock, flags, a1, a2),
                SYS_ACCEPT => accept4(&sock, flags, a1, a2, 0),
                SYS_ACCEPT4 => accept4(&sock, flags, a1, a2, a3),
                SYS_SENDTO => sendto(&sock, flags, a1, a2, a3, a4, a5),
                SYS_RECVFROM => recvfrom(&sock, flags, a1, a2, a3, a4, a5),
                SYS_SENDMSG => sendmsg(&sock, flags, a1, a2).map(|n| n as i64),
                SYS_RECVMSG => recvmsg(&sock, flags, a1, a2).map(|n| n as i64),
                SYS_SENDMMSG => sendmmsg(&sock, flags, a1, a2, a3),
                SYS_RECVMMSG => recvmmsg(&sock, flags, a1, a2, a3, a4),
                SYS_SHUTDOWN => sock.shutdown(a1).map(|_| 0),
                SYS_BIND => bind(&sock, a1, a2),
                SYS_LISTEN => sock.listen(a1 as i32).map(|_| 0),
                SYS_GETSOCKNAME => put_addr(sock.name().as_deref(), a1, a2).map(|_| 0),
                SYS_GETPEERNAME => sock.peer_name().and_then(|n| put_addr(n.as_deref(), a1, a2)).map(|_| 0),
                SYS_SETSOCKOPT => setsockopt(&sock, a1, a2, a3, a4),
                _ => getsockopt(&sock, a1, a2, a3, a4),
            }
        }
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| -e))
}

/// A new open file description for `sock` and its descriptor (open flags
/// `flags`).
fn install(sock: &Arc<Sock>, flags: u64) -> Result<i64, i64> {
    let id = files::new_id();
    sock.set_id(id);
    let open = O_RDWR | (flags as u32 & (O_NONBLOCK | O_CLOEXEC));
    let fd = files::install(id, File::Socket(sock.clone()), open)?;
    // What changed before the description existed.
    sock.report_now();
    Ok(fd)
}

/// The type of socket(2)'s and socketpair(2)'s `type` and `protocol`.
fn kind(ty: u64, protocol: u64) -> Result<u32, i64> {
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    if protocol != 0 && protocol != AF_UNIX as u64 {
        return Err(EPROTONOSUPPORT);
    }
    match (ty & SOCK_TYPE_MASK) as u32 {
        STREAM => Ok(STREAM),
        // SOCK_RAW is a datagram socket here, as on Linux.
        DGRAM | SOCK_RAW => Ok(DGRAM),
        SEQPACKET => Ok(SEQPACKET),
        _ => Err(ESOCKTNOSUPPORT),
    }
}

fn socket(ty: u64, protocol: u64) -> Result<i64, i64> {
    let kind = kind(ty, protocol)?;
    install(&Sock::new(kind), ty)
}

fn socketpair(ty: u64, protocol: u64, sv: u64) -> Result<i64, i64> {
    let kind = kind(ty, protocol)?;
    let (a, b) = Sock::pair(kind);
    let fa = match install(&a, ty) {
        Ok(fd) => fd,
        Err(e) => {
            // Each keeps the other (its peer): both go.
            a.release();
            b.release();
            return Err(e);
        }
    };
    let table = crate::fdtable::current();
    let fb = match install(&b, ty) {
        Ok(fd) => fd,
        Err(e) => {
            drop(table.take(fa as u64));
            b.release();
            return Err(e);
        }
    };
    if let Err(e) = usercopy::write(sv, &[fa as i32, fb as i32]) {
        drop(table.take(fa as u64));
        drop(table.take(fb as u64));
        return Err(e);
    }
    Ok(0)
}

// Addresses.

/// A sockaddr_un of the program's (EINVAL beyond its size or below the
/// family's).
fn read_addr(addr: u64, len: u64) -> Result<Vec<u8>, i64> {
    if len > SOCKADDR_UN_MAX || len < 2 {
        return Err(EINVAL);
    }
    let mut bytes = alloc::vec![0u8; len as usize];
    usercopy::from_program(addr, &mut bytes)?;
    Ok(bytes)
}

fn family(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

/// The name a sockaddr_un gives (EINVAL for another family or none).
fn parse(bytes: &[u8]) -> Result<Name, i64> {
    if family(bytes) != AF_UNIX || bytes.len() <= 2 {
        return Err(EINVAL);
    }
    let path = &bytes[2..];
    if path[0] == 0 {
        return Ok(Name::Abstract(path[1..].to_vec()));
    }
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    Ok(Name::Path(path[..end].to_vec()))
}

/// Writes `name`'s sockaddr_un to `addr` (at most the length at `len`
/// says, an int) and its full length to `len`.
fn put_addr(name: Option<&Name>, addr: u64, len: u64) -> Result<(), i64> {
    let bytes = Name::sockaddr(name);
    let cap: i32 = usercopy::read(len)?;
    if cap < 0 {
        return Err(EINVAL);
    }
    let n = bytes.len().min(cap as usize);
    usercopy::to_program(addr, &bytes[..n])?;
    usercopy::write(len, &(bytes.len() as i32))
}

fn cwd() -> alloc::string::String {
    records::current().state.lock().cwd.clone()
}

fn path_str(p: &[u8]) -> Result<&str, i64> {
    core::str::from_utf8(p).map_err(|_| ENOENT)
}

/// The socket a name leads to: ECONNREFUSED if nothing listens there (a
/// file that is no socket, or a socket inode nobody has bound now).
fn lookup(name: &Name) -> Result<Arc<Sock>, i64> {
    let key = match name {
        Name::Abstract(a) => Key::Abstract(a.clone()),
        Name::Path(p) => {
            let r = namespace::resolve(&cwd(), path_str(p)?, true)?;
            if r.mode & vfs::S_IFMT != vfs::S_IFSOCK {
                return Err(ECONNREFUSED);
            }
            match &r.node {
                Node::Tmp(t) => Key::Tmp(t.ino),
                Node::Data(d) => Key::Data(d.ino as u64),
                // A socket /proc/<pid>/fd/N leads to is sockfs's, no name.
                Node::Proc(_) => return Err(ECONNREFUSED),
            }
        }
    };
    find(&key).ok_or(ECONNREFUSED)
}

/// A new socket inode for `path` (EADDRINUSE if the name exists).
fn socket_inode(path: &[u8]) -> Result<(Key, Option<Node>), i64> {
    let (dir, name) = namespace::resolve_parent(&cwd(), path_str(path)?).map_err(|e| if e == namespace::EBUSY { EADDRINUSE } else { e })?;
    let perm = 0o777 & !records::current().state.lock().umask;
    let in_use = |e: i64| if e == EEXIST { EADDRINUSE } else { e };
    match &dir.node {
        Node::Tmp(d) => {
            let inode = d.socket(&name, perm).map_err(in_use)?;
            Ok((Key::Tmp(inode.ino), Some(Node::Tmp(inode))))
        }
        Node::Data(d) => {
            let inode = crate::datafs::create(d, &name, crate::datafs::New::Socket, perm).map_err(in_use)?;
            Ok((Key::Data(inode.ino as u64), Some(Node::Data(inode))))
        }
        // /proc and /sys take no sockets.
        Node::Proc(_) => Err(EACCES),
    }
}

fn bind(sock: &Arc<Sock>, addr: u64, len: u64) -> Result<i64, i64> {
    let bytes = read_addr(addr, len)?;
    if family(&bytes) != AF_UNIX {
        return Err(EINVAL);
    }
    if bytes.len() == 2 {
        // Just the family: a name of the abstract namespace.
        if sock.is_bound() {
            return Err(EINVAL);
        }
        return sock.autobind().map(|_| 0);
    }
    let name = parse(&bytes)?;
    sock.begin_bind()?;
    let made = match &name {
        Name::Abstract(a) => Ok((Key::Abstract(a.clone()), None)),
        Name::Path(p) => socket_inode(p),
    };
    match made {
        Ok((key, node)) => sock.end_bind(Some((name, key, node))).map(|_| 0),
        Err(e) => {
            sock.end_bind(None)?;
            Err(e)
        }
    }
}

fn connect(sock: &Arc<Sock>, flags: u32, addr: u64, len: u64) -> Result<i64, i64> {
    let bytes = read_addr(addr, len)?;
    if sock.ty == DGRAM {
        if family(&bytes) == AF_UNSPEC {
            return sock.set_dgram_peer(None).map(|_| 0);
        }
        let target = lookup(&parse(&bytes)?)?;
        if sock.passcred() {
            sock.autobind()?;
        }
        return sock.set_dgram_peer(Some(target)).map(|_| 0);
    }
    let target = lookup(&parse(&bytes)?)?;
    if sock.passcred() {
        sock.autobind()?;
    }
    sock.connect(&target, flags & O_NONBLOCK != 0).map(|_| 0)
}

fn accept4(sock: &Arc<Sock>, flags: u32, addr: u64, len: u64, aflags: u64) -> Result<i64, i64> {
    if aflags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let conn = sock.accept(flags & O_NONBLOCK != 0)?;
    // The address first: a descriptor is only made for a connection that
    // is handed out.
    if addr != 0 {
        let peer = conn.peer_name().ok().flatten();
        if let Err(e) = put_addr(peer.as_deref(), addr, len) {
            sock.unaccept(conn);
            return Err(e);
        }
    }
    match install(&conn, aflags) {
        Ok(fd) => Ok(fd),
        Err(e) => {
            sock.unaccept(conn);
            Err(e)
        }
    }
}

// Messages.

/// A msghdr of the program's.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct MsgHdr {
    pub name: u64,
    pub namelen: u32,
    _pad: u32,
    pub iov: u64,
    pub iovlen: u64,
    pub control: u64,
    pub controllen: u64,
    pub flags: i32,
    _pad2: u32,
}

const fn cmsg_align(n: usize) -> usize {
    (n + 7) & !7
}

/// Takes SCM_RIGHTS's descriptors and SCM_CREDENTIALS's credentials from a
/// message's ancillary data (others of other levels are ignored, as Linux
/// does).
fn parse_control(control: u64, len: u64) -> Result<(Vec<Passed>, Cred), i64> {
    let mut fds = Vec::new();
    let mut cred = Cred::current();
    if len == 0 {
        return Ok((fds, cred));
    }
    if len > OPTMEM_MAX {
        return Err(ENOBUFS);
    }
    let mut buf = alloc::vec![0u8; len as usize];
    usercopy::from_program(control, &mut buf)?;
    let mut off = 0usize;
    while off + 16 <= buf.len() {
        let clen = u64::from_le_bytes(buf[off..off + 8].try_into().expect("8 bytes")) as usize;
        let level = i32::from_le_bytes(buf[off + 8..off + 12].try_into().expect("4 bytes"));
        let ty = i32::from_le_bytes(buf[off + 12..off + 16].try_into().expect("4 bytes"));
        if clen < 16 || clen > buf.len() - off {
            return Err(EINVAL);
        }
        let data = &buf[off + 16..off + clen];
        if level == SOL_SOCKET {
            match ty {
                SCM_RIGHTS => {
                    let n = data.len() / 4;
                    if fds.len() + n > SCM_MAX_FD {
                        return Err(EINVAL);
                    }
                    for k in 0..n {
                        let fd = i32::from_le_bytes(data[k * 4..k * 4 + 4].try_into().expect("4 bytes"));
                        fds.push(Passed::take(fd)?);
                    }
                }
                SCM_CREDENTIALS => {
                    if data.len() != 12 {
                        return Err(EINVAL);
                    }
                    let word = |k: usize| u32::from_le_bytes(data[k..k + 4].try_into().expect("4 bytes"));
                    let given = Cred { pid: word(0), uid: word(4), gid: word(8) };
                    // Everyone is root: any process's credentials may be
                    // claimed, but the process must exist in this tree
                    // (another tree's are none of its business).
                    if given.pid != cred.pid
                        && (given.pid == 0 || !crate::process::exists(given.pid))
                    {
                        return Err(ESRCH);
                    }
                    cred = given;
                }
                _ => return Err(EINVAL),
            }
        }
        off += cmsg_align(clen);
    }
    Ok((fds, cred))
}

/// sendmsg's work: `name` (a sockaddr_un) or the peer, the program's
/// buffers, the ancillary data at `control`.
fn send_common(sock: &Arc<Sock>, fflags: u32, name: Option<Vec<u8>>, src: &mut Source, control: (u64, u64), flags: u64) -> Result<usize, i64> {
    if flags & MSG_OOB != 0 {
        return Err(EOPNOTSUPP);
    }
    let nonblock = fflags & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0;
    let to = match (name, sock.ty) {
        (None, _) => None,
        (Some(_), STREAM) => return Err(if sock.connected() { EISCONN } else { EOPNOTSUPP }),
        // A connected packet socket's destination is its peer.
        (Some(_), SEQPACKET) => None,
        (Some(bytes), _) => Some(lookup(&parse(&bytes)?)?),
    };
    if sock.ty == SEQPACKET && !sock.connected() {
        return Err(ENOTCONN);
    }
    let (fds, cred) = parse_control(control.0, control.1)?;
    let r = sock.send(src, to, fds, cred, nonblock);
    if r == Err(EPIPE) && sock.ty == STREAM && flags & MSG_NOSIGNAL == 0 {
        crate::signal::raise_thread(crate::signal::SIGPIPE);
    }
    r
}

fn sendto(sock: &Arc<Sock>, fflags: u32, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> Result<i64, i64> {
    let name = if addr != 0 { Some(read_addr(addr, alen)?) } else { None };
    let vecs = [(buf, len)];
    send_common(sock, fflags, name, &mut Source::program(&vecs), (0, 0), flags).map(|n| n as i64)
}

fn sendmsg(sock: &Arc<Sock>, fflags: u32, msg: u64, flags: u64) -> Result<usize, i64> {
    let h: MsgHdr = usercopy::read(msg)?;
    let name = if h.name != 0 && h.namelen != 0 { Some(read_addr(h.name, h.namelen as u64)?) } else { None };
    let vecs = files::iovecs(h.iov, h.iovlen)?;
    send_common(sock, fflags, name, &mut Source::program(&vecs), (h.control, h.controllen), flags)
}

fn sendmmsg(sock: &Arc<Sock>, fflags: u32, vec: u64, vlen: u64, flags: u64) -> Result<i64, i64> {
    sendmmsg_with(vec, vlen, flags, |at, f| sendmsg(sock, fflags, at, f))
}

/// sendmmsg(2) with `send(msghdr, flags)` for each message: how many went
/// (an error only if the first failed); each one's length goes to its
/// `msg_len`.
pub fn sendmmsg_with(vec: u64, vlen: u64, flags: u64, mut send: impl FnMut(u64, u64) -> Result<usize, i64>) -> Result<i64, i64> {
    let vlen = vlen.min(1024);
    let mut sent = 0;
    while sent < vlen {
        let at = vec + sent * 64;
        match send(at, flags) {
            Ok(n) => usercopy::write(at + 56, &(n as u32))?,
            Err(e) if sent == 0 => return Err(e),
            Err(_) => break,
        }
        sent += 1;
    }
    Ok(sent as i64)
}

/// What a receive reports beyond its bytes.
struct RecvInfo {
    ret: usize,
    flags: u32,
    from: Option<Arc<Name>>,
    control_used: u64,
}

/// recvmsg's work into the program's buffers; the ancillary data goes to
/// `control` (capacity `cap`).
fn recv_common(sock: &Arc<Sock>, fflags: u32, dst: &mut Sink, control: u64, cap: u64, flags: u64) -> Result<RecvInfo, i64> {
    if flags & MSG_OOB != 0 {
        return Err(EOPNOTSUPP);
    }
    let nonblock = fflags & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0;
    let cloexec = flags & MSG_CMSG_CLOEXEC != 0;
    let cap = cap.min(isize::MAX as u64) as usize;
    // Room for descriptors after the credentials (which come first).
    let left = cap - if sock.passcred() { cmsg_align(16 + 12).min(cap) } else { 0 };
    let fd_room = if control != 0 && left > 16 { (left - 16) / 4 } else { 0 };
    let o = RecvOpts { peek: flags & MSG_PEEK != 0, waitall: flags & MSG_WAITALL != 0, nonblock, cloexec, fd_room };
    let r = sock.recv(dst, o)?;
    let mut info = RecvInfo { ret: r.copied, flags: 0, from: r.from, control_used: 0 };
    if r.trunc {
        info.flags |= MSG_TRUNC as u32;
        if flags & MSG_TRUNC != 0 {
            info.ret = r.len;
        }
    }
    let mut used = 0usize;
    // What was received is the caller's now: a control buffer it cannot
    // be told through loses what it would have said, not the data.
    if let Some(c) = r.cred {
        put_cmsg(control, cap, &mut used, &mut info.flags, SCM_CREDENTIALS, &c.bytes());
    }
    put_fds(control, cap, &mut used, &mut info.flags, r.fds, cloexec);
    info.control_used = used as u64;
    Ok(info)
}

/// One control message (Linux's put_cmsg: truncated to what is left, with
/// MSG_CTRUNC; nothing if the buffer cannot be written).
fn put_cmsg(control: u64, cap: usize, used: &mut usize, flags: &mut u32, ty: i32, data: &[u8]) {
    let left = cap - *used;
    if control == 0 || left < 16 {
        *flags |= MSG_CTRUNC;
        return;
    }
    let mut len = 16 + data.len();
    if left < len {
        *flags |= MSG_CTRUNC;
        len = left;
    }
    let mut bytes = Vec::with_capacity(16 + data.len());
    bytes.extend_from_slice(&(len as u64).to_le_bytes());
    bytes.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    bytes.extend_from_slice(&ty.to_le_bytes());
    bytes.extend_from_slice(data);
    if usercopy::to_program(control + *used as u64, &bytes[..len]).is_ok() {
        *used += cmsg_align(16 + data.len()).min(left);
    }
}

/// SCM_RIGHTS: installs as many of the descriptors as fit (Linux's
/// scm_detach_fds); the rest are closed, with MSG_CTRUNC.
fn put_fds(control: u64, cap: usize, used: &mut usize, flags: &mut u32, fds: Fds, cloexec: bool) {
    let count = match &fds {
        Fds::Taken(v) => v.len(),
        Fds::Installed(_, total) => *total,
    };
    if count == 0 {
        return;
    }
    let left = cap - *used;
    let fit = if control != 0 && left > 16 { (left - 16) / 4 } else { 0 };
    let mut installed: Vec<i32> = Vec::new();
    match fds {
        Fds::Taken(v) => {
            for p in v.into_iter() {
                if installed.len() >= fit {
                    break;
                }
                match p.install(cloexec) {
                    Ok(fd) => installed.push(fd),
                    Err(_) => break,
                }
            }
        }
        Fds::Installed(v, _) => {
            for fd in v {
                if installed.len() < fit {
                    installed.push(fd);
                } else {
                    drop(crate::fdtable::current().take(fd as u64));
                }
            }
        }
    }
    if installed.len() < count {
        *flags |= MSG_CTRUNC;
    }
    if installed.is_empty() {
        return;
    }
    let len = 16 + installed.len() * 4;
    let mut bytes = Vec::with_capacity(len);
    bytes.extend_from_slice(&(len as u64).to_le_bytes());
    bytes.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    bytes.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
    for fd in &installed {
        bytes.extend_from_slice(&fd.to_le_bytes());
    }
    // The descriptors are the receiver's now, whether or not it can be
    // told (as on Linux).
    let _ = usercopy::to_program(control + *used as u64, &bytes);
    *used += cmsg_align(len).min(left);
}

fn recvfrom(sock: &Arc<Sock>, fflags: u32, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> Result<i64, i64> {
    let vecs = [(buf, len)];
    let info = recv_common(sock, fflags, &mut Sink::program(&vecs), 0, 0, flags)?;
    if addr != 0 {
        put_from(info.from.as_deref(), addr, alen)?;
    }
    Ok(info.ret as i64)
}

/// The sender's address of a received message (none: length 0).
fn put_from(from: Option<&Name>, addr: u64, len: u64) -> Result<(), i64> {
    match from {
        Some(n) => put_addr(Some(n), addr, len),
        None => usercopy::write(len, &0i32),
    }
}

fn recvmsg(sock: &Arc<Sock>, fflags: u32, msg: u64, flags: u64) -> Result<usize, i64> {
    let h: MsgHdr = usercopy::read(msg)?;
    let vecs = files::iovecs(h.iov, h.iovlen)?;
    let info = recv_common(sock, fflags, &mut Sink::program(&vecs), h.control, h.controllen, flags)?;
    let namelen = match (&info.from, h.name) {
        (Some(n), name) if name != 0 => {
            let bytes = Name::sockaddr(Some(n));
            let k = bytes.len().min(h.namelen as usize);
            usercopy::to_program(name, &bytes[..k])?;
            bytes.len() as u32
        }
        _ => 0,
    };
    usercopy::write(msg + 8, &namelen)?;
    usercopy::write(msg + 40, &info.control_used)?;
    usercopy::write(msg + 48, &info.flags)?;
    Ok(info.ret)
}

fn recvmmsg(sock: &Arc<Sock>, fflags: u32, vec: u64, vlen: u64, flags: u64, timeout: u64) -> Result<i64, i64> {
    recvmmsg_with(vec, vlen, flags, timeout, |at, f| recvmsg(sock, fflags, at, f))
}

/// recvmmsg(2) with `recv(msghdr, flags)` for each message: how many came
/// (an error only if the first failed), MSG_WAITFORONE and the timeout as
/// Linux has them.
pub fn recvmmsg_with(vec: u64, vlen: u64, flags: u64, timeout: u64, mut recv: impl FnMut(u64, u64) -> Result<usize, i64>) -> Result<i64, i64> {
    let vlen = vlen.min(1024);
    let deadline = if timeout != 0 {
        let ts: [i64; 2] = usercopy::read(timeout)?;
        if ts[0] < 0 || !(0..1_000_000_000).contains(&ts[1]) {
            return Err(EINVAL);
        }
        let ns = (ts[0] as u64).saturating_mul(1_000_000_000).saturating_add(ts[1] as u64);
        Some(syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64 + ns)
    } else {
        None
    };
    let mut got = 0;
    let mut flags = flags;
    while got < vlen {
        let at = vec + got * 64;
        match recv(at, flags & !MSG_WAITFORONE) {
            Ok(n) => usercopy::write(at + 56, &(n as u32))?,
            Err(e) if got == 0 => return Err(e),
            Err(_) => break,
        }
        got += 1;
        if flags & MSG_WAITFORONE != 0 {
            flags |= MSG_DONTWAIT;
        }
        // As on Linux, the timeout is looked at after each message.
        if deadline.is_some_and(|d| syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]).max(0) as u64 >= d) {
            break;
        }
    }
    Ok(got as i64)
}

// Options.

pub fn opt_int(val: u64, len: u64) -> Result<i32, i64> {
    if len < 4 {
        return Err(EINVAL);
    }
    usercopy::read(val)
}

/// A struct timeval (or __kernel_sock_timeval: the same on x86_64) as
/// nanoseconds; 0 is none.
pub fn opt_timeout(val: u64, len: u64) -> Result<u64, i64> {
    if len < 16 {
        return Err(EINVAL);
    }
    let tv: [i64; 2] = usercopy::read(val)?;
    if !(0..1_000_000).contains(&tv[1]) {
        return Err(EDOM);
    }
    if tv[0] < 0 {
        // Linux takes a negative time as "at once".
        return Ok(1);
    }
    Ok((tv[0] as u64).saturating_mul(1_000_000_000).saturating_add(tv[1] as u64 * 1000))
}

fn setsockopt(sock: &Arc<Sock>, level: u64, name: u64, val: u64, len: u64) -> Result<i64, i64> {
    if level as i32 != SOL_SOCKET {
        return Err(EOPNOTSUPP);
    }
    match name {
        SO_PASSCRED => sock.set_passcred(opt_int(val, len)? != 0),
        SO_SNDBUF | SO_SNDBUFFORCE => sock.set_sndbuf(opt_int(val, len)?.max(0) as usize),
        SO_RCVBUF | SO_RCVBUFFORCE => sock.set_rcvbuf(opt_int(val, len)?.max(0) as usize),
        SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => sock.set_timeout(false, opt_timeout(val, len)?),
        SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => sock.set_timeout(true, opt_timeout(val, len)?),
        SO_RCVLOWAT => {
            // Linux's rule; reads here wait for one byte whatever it is.
            let v = opt_int(val, len)?;
            sock.set_opt(name, if v < 0 { i32::MAX } else { v.max(1) });
        }
        SO_LINGER => {
            if len < 8 {
                return Err(EINVAL);
            }
            let l: [i32; 2] = usercopy::read(val)?;
            sock.set_opt(name, if l[0] != 0 { l[1].max(0) } else { -1 });
        }
        n if PLAIN_OPTS.contains(&n) => sock.set_opt(n, opt_int(val, len)?),
        _ => return Err(ENOPROTOOPT),
    }
    Ok(0)
}

fn getsockopt(sock: &Arc<Sock>, level: u64, name: u64, val: u64, lenp: u64) -> Result<i64, i64> {
    if level as i32 != SOL_SOCKET {
        return Err(EOPNOTSUPP);
    }
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
    let bytes = match name {
        SO_TYPE => int(sock.ty as i64),
        SO_DOMAIN => int(AF_UNIX as i64),
        SO_PROTOCOL => int(0),
        SO_ERROR => int(sock.take_error()),
        SO_ACCEPTCONN => int(sock.listening() as i64),
        SO_PASSCRED => int(sock.passcred() as i64),
        SO_SNDBUF => int(sock.bufs().0 as i64),
        SO_RCVBUF => int(sock.bufs().1 as i64),
        SO_PEERCRED => sock.peercred().bytes().to_vec(),
        SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => timeval(sock.timeouts().0),
        SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => timeval(sock.timeouts().1),
        SO_RCVLOWAT => int(sock.opt(name).unwrap_or(1) as i64),
        SO_SNDLOWAT => int(1),
        SO_LINGER => {
            let l = sock.opt(name).unwrap_or(-1);
            let mut b = int((l >= 0) as i64);
            b.extend_from_slice(&l.max(0).to_le_bytes());
            b
        }
        n if PLAIN_OPTS.contains(&n) => int(sock.opt(n).unwrap_or(0) as i64),
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
pub fn on_file(nr: u64, sock: &Arc<Sock>, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    match nr {
        files::SYS_READ => recv_common(&sock, flags, &mut Sink::program(&[(a1, a2)]), 0, 0, 0).map(|i| i.ret as i64),
        files::SYS_READV => {
            let vecs = files::iovecs(a1, a2)?;
            recv_common(&sock, flags, &mut Sink::program(&vecs), 0, 0, 0).map(|i| i.ret as i64)
        }
        files::SYS_WRITE => send_common(&sock, flags, None, &mut Source::program(&[(a1, a2)]), (0, 0), 0).map(|n| n as i64),
        files::SYS_WRITEV => {
            let vecs = files::iovecs(a1, a2)?;
            send_common(&sock, flags, None, &mut Source::program(&vecs), (0, 0), 0).map(|n| n as i64)
        }
        files::SYS_FSTAT => fstat(&sock, a1),
        files::SYS_IOCTL => match a1 {
            FIONREAD => usercopy::write(a2, &(sock.inq()? as i32)).map(|_| 0),
            SIOCOUTQ => usercopy::write(a2, &(sock.outq() as i32)).map(|_| 0),
            _ => Err(ENOTTY),
        },
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(ESPIPE),
        files::SYS_GETDENTS64 => Err(crate::files::ENOTDIR),
        _ => Err(EINVAL),
    }
}

/// Reads into the server's memory (sendfile from a socket).
pub fn read_server(sock: &Arc<Sock>, flags: u32, buf: &mut [u8]) -> Result<i64, i64> {
    recv_common(&sock, flags, &mut Sink::Server { buf, at: 0 }, 0, 0, 0).map(|i| i.ret as i64)
}

/// Writes the server's memory (sendfile to a socket).
pub fn write_server(sock: &Arc<Sock>, flags: u32, buf: &[u8]) -> Result<i64, i64> {
    send_common(&sock, flags, None, &mut Source::Server { buf, at: 0 }, (0, 0), 0).map(|n| n as i64)
}

/// fstat: a socket inode of sockfs.
fn fstat(sock: &Sock, buf: u64) -> Result<i64, i64> {
    if buf == 0 {
        return Err(crate::files::EFAULT);
    }
    usercopy::to_program(buf, &stat(sock)).map(|_| 0)
}

/// Its `struct stat`: a socket inode of sockfs.
pub fn stat(sock: &Sock) -> [u8; 144] {
    const SOCKFS_DEV: u64 = 0x8;
    let mut st = [0u8; 144];
    st[0..8].copy_from_slice(&SOCKFS_DEV.to_le_bytes());
    st[8..16].copy_from_slice(&sock.id().to_le_bytes());
    st[16..24].copy_from_slice(&1u64.to_le_bytes());
    st[24..28].copy_from_slice(&(vfs::S_IFSOCK | 0o777).to_le_bytes());
    st[56..64].copy_from_slice(&4096u64.to_le_bytes());
    st
}

