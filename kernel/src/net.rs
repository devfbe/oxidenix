//! Client side of the network server (netd): every socket operation of a
//! user program becomes a `netproto` request. Calls that may wait for the
//! network can be interrupted by signals.

use crate::process::errno::*;
use crate::process::{ipc, Server};
use alloc::sync::Arc;
use alloc::vec::Vec;
use netproto::{Op, MAX_DATA, NONBLOCK};

pub use netproto::{KIND_TCP, KIND_UDP, PEEK};

/// An IPv4 endpoint in host byte order.
#[derive(Clone, Copy, Default)]
pub struct Endpoint {
    pub addr: u32,
    pub port: u16,
}

struct Reply {
    status: i64,
    values: [u64; 6],
    payload: Vec<u8>,
}

/// One socket in netd. Dropping it (with the last descriptor) closes it.
pub struct Socket {
    service: usize,
    handle: u64,
    pub kind: u64,
}

/// netd as started at boot, to restart it after a crash.
static NETD: spin::Once<Arc<Server>> = spin::Once::new();

pub fn set_server(server: Arc<Server>) {
    NETD.call_once(|| server);
}

/// netd's service; a dead netd is restarted (sockets it served are gone).
fn service() -> Result<usize, i64> {
    let netd = NETD.get().ok_or(ENETDOWN)?;
    netd.revive().map(|(service, _)| service).map_err(|_| ENETDOWN)
}

fn call(service: usize, op: Op, args: [u64; 4], payload: &[u8]) -> Result<Reply, i64> {
    let message = netproto::encode_request(op, args, payload);
    let raw = ipc::call_interruptible(service, message, |id| netproto::encode_request(Op::Cancel, [id, 0, 0, 0], &[]))?;
    let r = netproto::decode_response(&raw).ok_or(EIO)?;
    if r.status < 0 {
        return Err(-r.status);
    }
    Ok(Reply { status: r.status, values: r.values, payload: r.payload.to_vec() })
}

fn flags(nonblocking: bool) -> u64 {
    if nonblocking { NONBLOCK } else { 0 }
}

impl Socket {
    pub fn new(kind: u64) -> Result<Socket, i64> {
        let service = service()?;
        let handle = call(service, Op::Socket, [kind, 0, 0, 0], &[])?.values[0];
        Ok(Socket { service, handle, kind })
    }

    fn call(&self, op: Op, args: [u64; 4], payload: &[u8]) -> Result<Reply, i64> {
        call(self.service, op, args, payload)
    }

    pub fn bind(&self, at: Endpoint) -> Result<(), i64> {
        self.call(Op::Bind, [self.handle, at.addr as u64, at.port as u64, 0], &[]).map(|_| ())
    }

    pub fn listen(&self, backlog: u64) -> Result<(), i64> {
        self.call(Op::Listen, [self.handle, backlog, 0, 0], &[]).map(|_| ())
    }

    pub fn accept(&self, nonblocking: bool) -> Result<(Socket, Endpoint), i64> {
        let v = self.call(Op::Accept, [self.handle, flags(nonblocking), 0, 0], &[])?.values;
        let socket = Socket { service: self.service, handle: v[0], kind: self.kind };
        Ok((socket, Endpoint { addr: v[1] as u32, port: v[2] as u16 }))
    }

    pub fn connect(&self, to: Endpoint, nonblocking: bool) -> Result<(), i64> {
        self.call(Op::Connect, [self.handle, to.addr as u64, to.port as u64, flags(nonblocking)], &[]).map(|_| ())
    }

    /// Sends all of `data` (blocking) or as much as fits (non-blocking).
    /// `to` is for unconnected UDP sockets.
    pub fn send(&self, data: &[u8], to: Option<Endpoint>, nonblocking: bool) -> Result<usize, i64> {
        let to = to.unwrap_or_default();
        if self.kind == KIND_UDP {
            // A datagram is never split.
            if data.len() > MAX_DATA {
                return Err(EMSGSIZE);
            }
            let r = self.call(Op::Send, [self.handle, flags(nonblocking), to.addr as u64, to.port as u64], data)?;
            return Ok(r.status as usize);
        }
        let mut done = 0;
        while done < data.len() {
            let chunk = &data[done..(done + MAX_DATA).min(data.len())];
            let args = [self.handle, flags(nonblocking), to.addr as u64, to.port as u64];
            match self.call(Op::Send, args, chunk) {
                Ok(r) => done += r.status as usize,
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            }
            if nonblocking {
                break;
            }
        }
        Ok(done)
    }

    /// Receives up to `buf.len()` bytes; 0 means the peer closed.
    pub fn recv(&self, buf: &mut [u8], nonblocking: bool, peek: bool) -> Result<(usize, Endpoint), i64> {
        let want = buf.len().min(MAX_DATA) as u64;
        let f = flags(nonblocking) | if peek { PEEK } else { 0 };
        let r = self.call(Op::Recv, [self.handle, want, f, 0], &[])?;
        let n = r.payload.len().min(buf.len());
        buf[..n].copy_from_slice(&r.payload[..n]);
        Ok((n, Endpoint { addr: r.values[0] as u32, port: r.values[1] as u16 }))
    }

    pub fn shutdown(&self, how: u64) -> Result<(), i64> {
        self.call(Op::Shutdown, [self.handle, how, 0, 0], &[]).map(|_| ())
    }

    /// Ready poll events among `events`; a dead server reports an error.
    pub fn poll(&self, events: i16) -> i16 {
        match self.call(Op::Poll, [self.handle, events as u16 as u64, 0, 0], &[]) {
            Ok(r) => r.values[0] as i16,
            Err(_) => netproto::POLLERR as i16,
        }
    }

    /// The local (`peer` false) or remote address.
    pub fn name(&self, peer: bool) -> Result<Endpoint, i64> {
        let v = self.call(Op::Name, [self.handle, peer as u64, 0, 0], &[])?.values;
        Ok(Endpoint { addr: v[0] as u32, port: v[1] as u16 })
    }

    /// SO_ERROR: the pending error (a positive errno, or 0), which is cleared.
    pub fn take_error(&self) -> i64 {
        self.call(Op::TakeError, [self.handle, 0, 0, 0], &[]).map_or(EIO, |r| r.status)
    }
}

impl Drop for Socket {
    /// Must not sleep (it runs when a descriptor is dropped), hence `post`.
    fn drop(&mut self) {
        ipc::post(self.service, netproto::encode_request(Op::Close, [self.handle, 0, 0, 0], &[]));
    }
}
