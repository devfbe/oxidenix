//! Socket system calls (IPv4 TCP and UDP). The protocol work happens in
//! netd; this layer translates the Linux ABI (sockaddr_in, flags, msghdr).

use super::errno::*;
use super::{uaccess, with_current};
use crate::fs::file::{Kind, OpenFile, O_CLOEXEC, O_NONBLOCK, O_RDWR};
use crate::net::{Endpoint, Socket, KIND_TCP, KIND_UDP};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const AF_INET: u16 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = O_NONBLOCK as u64;
const SOCK_CLOEXEC: u64 = O_CLOEXEC as u64;

const MSG_PEEK: u64 = 0x2;
const MSG_DONTWAIT: u64 = 0x40;

const SOL_SOCKET: u64 = 1;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;

/// Largest datagram or chunk moved by one sendmsg/recvmsg.
const MAX_MSG: usize = 64 * 1024;

fn socket_file(fd: u64) -> Result<Arc<OpenFile>, i64> {
    let f = with_current(|p| p.file(fd))?;
    if f.socket().is_none() {
        return Err(ENOTSOCK);
    }
    Ok(f)
}

fn sock(f: &OpenFile) -> &Socket {
    f.socket().expect("checked by socket_file")
}

fn install(socket: Socket, flags: u64) -> SysResult {
    let file = OpenFile::new(Kind::Socket(socket), O_RDWR | (flags & SOCK_NONBLOCK) as u32, None);
    with_current(|p| p.alloc_fd(file, flags & SOCK_CLOEXEC != 0, 0))
}

/// Reads a sockaddr_in.
fn read_addr(addr: u64, len: u64) -> Result<Endpoint, i64> {
    if addr == 0 || len < 8 {
        return Err(EINVAL);
    }
    let raw: [u8; 8] = uaccess::read(addr)?;
    if u16::from_le_bytes([raw[0], raw[1]]) != AF_INET {
        return Err(EAFNOSUPPORT);
    }
    Ok(Endpoint {
        port: u16::from_be_bytes([raw[2], raw[3]]),
        addr: u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]),
    })
}

/// Writes a sockaddr_in to (addr, *len_ptr), truncated as Linux does.
fn write_addr(addr: u64, len_ptr: u64, ep: Endpoint) -> Result<(), i64> {
    if addr == 0 || len_ptr == 0 {
        return Ok(());
    }
    let mut raw = [0u8; 16];
    raw[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    raw[2..4].copy_from_slice(&ep.port.to_be_bytes());
    raw[4..8].copy_from_slice(&ep.addr.to_be_bytes());
    let len: u32 = uaccess::read(len_ptr)?;
    let n = (len as usize).min(raw.len());
    uaccess::slice_mut(addr, n as u64)?.copy_from_slice(&raw[..n]);
    uaccess::write(len_ptr, raw.len() as u32)
}

pub fn socket(domain: u64, ty: u64, _protocol: u64) -> SysResult {
    if domain != AF_INET as u64 {
        return Err(EAFNOSUPPORT);
    }
    let kind = match ty & SOCK_TYPE_MASK {
        SOCK_STREAM => KIND_TCP,
        SOCK_DGRAM => KIND_UDP,
        _ => return Err(EPROTONOSUPPORT),
    };
    install(Socket::new(kind)?, ty)
}

pub fn bind(fd: u64, addr: u64, len: u64) -> SysResult {
    let f = socket_file(fd)?;
    sock(&f).bind(read_addr(addr, len)?)?;
    Ok(0)
}

pub fn listen(fd: u64, backlog: u64) -> SysResult {
    let f = socket_file(fd)?;
    sock(&f).listen(backlog.clamp(1, 16))?;
    Ok(0)
}

pub fn accept(fd: u64, addr: u64, len_ptr: u64, flags: u64) -> SysResult {
    let f = socket_file(fd)?;
    let (socket, peer) = sock(&f).accept(f.nonblocking())?;
    write_addr(addr, len_ptr, peer)?;
    install(socket, flags)
}

pub fn connect(fd: u64, addr: u64, len: u64) -> SysResult {
    let f = socket_file(fd)?;
    sock(&f).connect(read_addr(addr, len)?, f.nonblocking())?;
    Ok(0)
}

pub fn sendto(fd: u64, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> SysResult {
    let f = socket_file(fd)?;
    let to = if addr != 0 { Some(read_addr(addr, alen)?) } else { None };
    // Sent straight from user memory, chunk by chunk: no kernel copy of
    // an arbitrarily large buffer.
    let data = uaccess::slice(buf, len)?;
    let n = sock(&f).send(data, to, f.nonblocking() || flags & MSG_DONTWAIT != 0)?;
    Ok(n as i64)
}

pub fn recvfrom(fd: u64, buf: u64, len: u64, flags: u64, addr: u64, len_ptr: u64) -> SysResult {
    let f = socket_file(fd)?;
    let mut data = vec![0u8; (len as usize).min(MAX_MSG)];
    let nonblocking = f.nonblocking() || flags & MSG_DONTWAIT != 0;
    let (n, from) = sock(&f).recv(&mut data, nonblocking, flags & MSG_PEEK != 0)?;
    uaccess::slice_mut(buf, n as u64)?.copy_from_slice(&data[..n]);
    write_addr(addr, len_ptr, from)?;
    Ok(n as i64)
}

/// The fields of a struct msghdr that matter here.
struct MsgHdr {
    name: u64,
    namelen: u32,
    iov: Vec<(u64, u64)>,
}

fn read_msghdr(msg: u64) -> Result<MsgHdr, i64> {
    let raw: [u64; 7] = uaccess::read(msg)?;
    let (iov, iovlen) = (raw[2], raw[3]);
    if iovlen > 64 {
        return Err(EMSGSIZE);
    }
    let mut vecs = Vec::new();
    for i in 0..iovlen {
        let [base, len]: [u64; 2] = uaccess::read(iov + i * 16)?;
        vecs.push((base, len));
    }
    Ok(MsgHdr { name: raw[0], namelen: raw[1] as u32, iov: vecs })
}

pub fn sendmsg(fd: u64, msg: u64, flags: u64) -> SysResult {
    let h = read_msghdr(msg)?;
    let mut data = Vec::new();
    for (base, len) in h.iov {
        if len > (MAX_MSG - data.len()) as u64 {
            return Err(EMSGSIZE);
        }
        data.extend_from_slice(uaccess::slice(base, len)?);
    }
    let f = socket_file(fd)?;
    let to = if h.name != 0 { Some(read_addr(h.name, h.namelen as u64)?) } else { None };
    let n = sock(&f).send(&data, to, f.nonblocking() || flags & MSG_DONTWAIT != 0)?;
    Ok(n as i64)
}

pub fn recvmsg(fd: u64, msg: u64, flags: u64) -> SysResult {
    let h = read_msghdr(msg)?;
    let total = h.iov.iter().fold(0u64, |sum, &(_, len)| sum.saturating_add(len)).min(MAX_MSG as u64) as usize;
    let f = socket_file(fd)?;
    let mut data = vec![0u8; total];
    let nonblocking = f.nonblocking() || flags & MSG_DONTWAIT != 0;
    let (n, from) = sock(&f).recv(&mut data, nonblocking, flags & MSG_PEEK != 0)?;
    let mut done = 0;
    for (base, len) in h.iov {
        let chunk = (len as usize).min(n - done);
        uaccess::slice_mut(base, chunk as u64)?.copy_from_slice(&data[done..done + chunk]);
        done += chunk;
    }
    // msg_namelen is updated by write_addr; no control data; no flags.
    write_addr(h.name, msg + 8, from)?;
    uaccess::write(msg + 40, 0u64)?;
    uaccess::write(msg + 48, 0u32)?;
    Ok(n as i64)
}

pub fn shutdown(fd: u64, how: u64) -> SysResult {
    if how > 2 {
        return Err(EINVAL);
    }
    let f = socket_file(fd)?;
    sock(&f).shutdown(how)?;
    Ok(0)
}

pub fn getsockname(fd: u64, addr: u64, len_ptr: u64, peer: bool) -> SysResult {
    let f = socket_file(fd)?;
    write_addr(addr, len_ptr, sock(&f).name(peer)?)?;
    Ok(0)
}

pub fn getsockopt(fd: u64, level: u64, name: u64, val: u64, len_ptr: u64) -> SysResult {
    let f = socket_file(fd)?;
    let value: i32 = match (level, name) {
        (SOL_SOCKET, SO_ERROR) => sock(&f).take_error() as i32,
        (SOL_SOCKET, SO_TYPE) => if sock(&f).kind == KIND_TCP { SOCK_STREAM as i32 } else { SOCK_DGRAM as i32 },
        // Everything else reads as off/zero.
        _ => 0,
    };
    let len: u32 = uaccess::read(len_ptr)?;
    if len < 4 {
        return Err(EINVAL);
    }
    uaccess::write(val, value)?;
    uaccess::write(len_ptr, 4u32)?;
    Ok(0)
}

/// Options are accepted and ignored (SO_REUSEADDR, TCP_NODELAY, timeouts...).
pub fn setsockopt(fd: u64) -> SysResult {
    socket_file(fd)?;
    Ok(0)
}
