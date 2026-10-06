//! Synchronous message passing between the kernel and user-space servers.
//!
//! A server registers under a name, then loops over `receive` and `reply`.
//! The kernel acts as the client on behalf of user programs: `call` queues
//! a request and sleeps until the reply arrives; `post` queues one without
//! waiting. Messages are copied through the kernel.

use super::errno::*;
use super::{sleep_on, uaccess, wakeup, with_current, Pid};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

/// Upper bound for one message in either direction.
pub const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Queued,
    Taken,
    Done,
    Failed,
}

struct Request {
    service: usize,
    /// Whether a client sleeps until the reply (otherwise it is discarded).
    waits: bool,
    message: Vec<u8>,
    reply: Vec<u8>,
    state: State,
}

struct Service {
    name: String,
    server: Pid,
    arg: u64,
    alive: bool,
    queue: VecDeque<u64>,
}

struct Ipc {
    services: Vec<Service>,
    requests: BTreeMap<u64, Request>,
    next_id: u64,
}

static IPC: Mutex<Ipc> = Mutex::new(Ipc { services: Vec::new(), requests: BTreeMap::new(), next_id: 1 });

// Sleep channels; far away from the small fixed ones and from pipe addresses.
fn service_chan(service: usize) -> usize {
    0x1_0000_0000 + service
}

fn request_chan(id: u64) -> usize {
    0x2_0000_0000 + id as usize
}

fn lock<R>(f: impl FnOnce(&mut Ipc) -> R) -> R {
    without_interrupts(|| f(&mut IPC.lock()))
}

/// ipc_register(name, length, arg): makes the calling (privileged) process
/// the server behind `name`. A dead server's name can be taken over.
pub fn register(name: u64, len: u64, arg: u64) -> SysResult {
    if !with_current(|p| p.privileged) {
        return Err(EPERM);
    }
    if len > 64 {
        return Err(EINVAL);
    }
    let name = String::from_utf8(uaccess::slice(name, len)?.to_vec()).map_err(|_| EINVAL)?;
    let me = super::current_pid();
    lock(|ipc| {
        let service = Service { name: name.clone(), server: me, arg, alive: true, queue: VecDeque::new() };
        match ipc.services.iter().position(|s| s.name == name) {
            Some(i) if ipc.services[i].alive => Err(EEXIST),
            Some(i) => {
                ipc.services[i] = service;
                Ok(i as i64)
            }
            None => {
                ipc.services.push(service);
                Ok(ipc.services.len() as i64 - 1)
            }
        }
    })
}

/// ipc_receive(buffer, length, &id): waits for the next request to one of
/// the caller's services and returns its length.
pub fn receive(buf: u64, len: u64, id_out: u64) -> SysResult {
    without_interrupts(|| receive_loop(buf, len, id_out))
}

fn receive_loop(buf: u64, len: u64, id_out: u64) -> SysResult {
    let me = super::current_pid();
    loop {
        let next = lock(|ipc| {
            let index = ipc.services.iter().position(|s| s.server == me && s.alive).ok_or(EINVAL)?;
            let Some(id) = ipc.services[index].queue.pop_front() else { return Ok::<_, i64>(Err(index)) };
            let req = ipc.requests.get_mut(&id).expect("queued request exists");
            req.state = State::Taken;
            Ok(Ok((id, core::mem::take(&mut req.message))))
        })?;
        match next {
            Ok((id, message)) => {
                if message.len() as u64 > len {
                    fail(id);
                    continue;
                }
                uaccess::slice_mut(buf, message.len() as u64)?.copy_from_slice(&message);
                uaccess::write(id_out, id)?;
                return Ok(message.len() as i64);
            }
            Err(index) => {
                if super::signal::interrupted() {
                    return Err(EINTR);
                }
                sleep_on(service_chan(index));
            }
        }
    }
}

/// ipc_reply(id, buffer, length): answers a request taken with receive.
pub fn reply(id: u64, buf: u64, len: u64) -> SysResult {
    if len as usize > MAX_MESSAGE {
        return Err(EINVAL);
    }
    let data = uaccess::slice(buf, len)?.to_vec();
    let me = super::current_pid();
    lock(|ipc| {
        let req = ipc.requests.get(&id).ok_or(EINVAL)?;
        if req.state != State::Taken || ipc.services[req.service].server != me {
            return Err(EINVAL);
        }
        if req.waits {
            let req = ipc.requests.get_mut(&id).expect("checked above");
            req.reply = data;
            req.state = State::Done;
        } else {
            ipc.requests.remove(&id);
        }
        Ok(())
    })?;
    wakeup(request_chan(id));
    Ok(0)
}

fn fail(id: u64) {
    lock(|ipc| {
        if let Some(req) = ipc.requests.get_mut(&id) {
            if req.waits {
                req.state = State::Failed;
            } else {
                ipc.requests.remove(&id);
            }
        }
    });
    wakeup(request_chan(id));
}

fn enqueue(service: usize, message: Vec<u8>, waits: bool) -> Result<u64, i64> {
    let id = lock(|ipc| {
        if !ipc.services.get(service).is_some_and(|s| s.alive) {
            return Err(EIO);
        }
        let id = ipc.next_id;
        ipc.next_id += 1;
        ipc.requests.insert(id, Request { service, waits, message, reply: Vec::new(), state: State::Queued });
        ipc.services[service].queue.push_back(id);
        Ok(id)
    })?;
    wakeup(service_chan(service));
    Ok(id)
}

/// Sends `message` to `service` and sleeps until the reply. Not
/// interruptible by signals: the server may already be working on it.
pub fn call(service: usize, message: Vec<u8>) -> Result<Vec<u8>, i64> {
    // Enqueueing, checking and sleeping happen with interrupts off, so the
    // reply cannot slip in between the check and the sleep.
    without_interrupts(|| {
        let id = enqueue(service, message, true)?;
        wait_reply(id)
    })
}

fn wait_reply(id: u64) -> Result<Vec<u8>, i64> {
    loop {
        let done = lock(|ipc| match ipc.requests.get(&id).map(|r| r.state) {
            Some(State::Done) => Some(Ok(ipc.requests.remove(&id).expect("present").reply)),
            Some(State::Failed) | None => {
                ipc.requests.remove(&id);
                Some(Err(EIO))
            }
            _ => None,
        });
        if let Some(result) = done {
            return result;
        }
        sleep_on(request_chan(id));
    }
}

/// Sends `message` without waiting for (or receiving) a reply. Never
/// sleeps, so it can be used while dropping objects.
pub fn post(service: usize, message: Vec<u8>) {
    let _ = enqueue(service, message, false);
}

/// Called when a process exits: its services die and every request they
/// had not answered fails with EIO.
pub fn on_exit(pid: Pid) {
    let failed: Vec<u64> = lock(|ipc| {
        let dead: Vec<usize> = (0..ipc.services.len()).filter(|&i| ipc.services[i].server == pid && ipc.services[i].alive).collect();
        for &i in &dead {
            ipc.services[i].alive = false;
            ipc.services[i].queue.clear();
        }
        ipc.requests
            .iter()
            .filter(|(_, r)| dead.contains(&r.service) && matches!(r.state, State::Queued | State::Taken))
            .map(|(&id, _)| id)
            .collect()
    });
    for id in failed {
        fail(id);
    }
}

/// The service registered under `name`, if its server is alive: (index, arg).
pub fn lookup(name: &str) -> Option<(usize, u64)> {
    lock(|ipc| ipc.services.iter().position(|s| s.alive && s.name == name).map(|i| (i, ipc.services[i].arg)))
}

/// Waits up to `ticks` timer ticks for `name` to be registered.
pub fn wait_for(name: &str, ticks: u64) -> Option<(usize, u64)> {
    let deadline = super::ticks() + ticks;
    loop {
        if let Some(found) = lookup(name) {
            return Some(found);
        }
        if super::ticks() >= deadline {
            return None;
        }
        let _ = super::sleep_ticks(1);
    }
}
