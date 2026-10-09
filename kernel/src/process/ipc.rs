//! Synchronous message passing between the kernel and user-space servers.
//!
//! A server registers under a name, then loops over `receive` and `reply`.
//! The kernel acts as the client on behalf of user programs: `call` queues
//! a request and sleeps until the reply arrives. Messages are copied
//! through the kernel. (Only procfs still serves such requests, for the
//! kernel's `/proc`; the data plane to diskfs and netd is the channels'.)
//!
//! Besides the requests of its own protocol, a server registered with
//! `IPC_CHANNELS` gets control requests from the kernel itself (offers of
//! data-plane channels, see `channel`): their ids have `CONTROL` set, so no
//! protocol message can pass for one.

use super::errno::*;
use super::sched::prepare_to_wait;
use super::{uaccess, wakeup, Pid};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use crate::sync::IrqSpinLock;
use x86_64::instructions::interrupts::without_interrupts;

/// Upper bound for one message in either direction.
pub const MAX_MESSAGE: usize = 64 * 1024;
/// Set in the id of a control request (one of the kernel's own, not of the
/// service's protocol).
pub const CONTROL: u64 = 1 << 63;
/// `ipc_register` flag: the service accepts channel offers.
pub const IPC_CHANNELS: u64 = 1;

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
    /// It accepts channel offers (`IPC_CHANNELS`).
    channels: bool,
    /// Unique per registration. A restarted server reuses the service
    /// index, so a request meant for one registration (a channel offer)
    /// checks the generation to never reach the new server.
    generation: u64,
}

struct Ipc {
    services: Vec<Service>,
    requests: BTreeMap<u64, Request>,
    next_id: u64,
    next_generation: u64,
    /// Requests sent, and bytes of requests and replies (see `counters`).
    calls: u64,
    bytes: u64,
}

static IPC: IrqSpinLock<Ipc> = IrqSpinLock::new(Ipc {
    services: Vec::new(),
    requests: BTreeMap::new(),
    next_id: 1,
    next_generation: 1,
    calls: 0,
    bytes: 0,
});

/// (requests sent, bytes of requests and replies) since boot.
pub fn counters() -> (u64, u64) {
    lock(|ipc| (ipc.calls, ipc.bytes))
}

/// A service as one particular registration of its server.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Instance {
    pub service: usize,
    pub generation: u64,
}

// Sleep channels; far away from the small fixed ones and from pipe
// addresses. A server sleeps on `irq::server_chan(pid)`.
fn request_chan(id: u64) -> usize {
    0x2_0000_0000 + (id & !CONTROL) as usize
}

fn lock<R>(f: impl FnOnce(&mut Ipc) -> R) -> R {
    f(&mut IPC.lock())
}

/// ipc_register(name, length, arg, flags): makes the calling (privileged)
/// process the server behind `name`; `IPC_CHANNELS` in `flags`: it accepts
/// channel offers. A dead server's name can be taken over.
///
/// A server registers once its device is reset and ready: what its
/// predecessor's device might still have reached (quarantined grants, see
/// `channel::DmaDomain`) is released here.
pub fn register(name: u64, len: u64, arg: u64, flags: u64) -> SysResult {
    if !super::sched::current().group.privileged.load(core::sync::atomic::Ordering::Relaxed) {
        return Err(EPERM);
    }
    if len > 64 || flags & !IPC_CHANNELS != 0 {
        return Err(EINVAL);
    }
    let name = String::from_utf8(uaccess::read_vec(name, len)?).map_err(|_| EINVAL)?;
    let me = super::current_pid();
    let registered = lock(|ipc| {
        let generation = ipc.next_generation;
        ipc.next_generation += 1;
        let channels = flags & IPC_CHANNELS != 0;
        let service = Service { name: name.clone(), server: me, arg, alive: true, queue: VecDeque::new(), channels, generation };
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
    })?;
    if let Some(server) = super::with_current(|p| p.server.clone()) {
        server.domain.device_reset();
        server.registered(me);
    }
    Ok(registered)
}

/// ipc_receive(buffer, length, &id, timeout_ms): waits for the next request
/// to one of the caller's services and returns its length. A negative
/// timeout waits forever; when it runs out, the result is ETIMEDOUT.
///
/// Interrupts of the caller's device lines come first: they are reported
/// with request id 0 and the mask of fired lines as the result. Then a
/// doorbell the caller watches (`channel::watch`): request id 0 and the
/// result `DOORBELL`.
pub fn receive(buf: u64, len: u64, id_out: u64, timeout_ms: i64) -> SysResult {
    let deadline = (timeout_ms >= 0).then(|| crate::time::now().saturating_add((timeout_ms as u64).saturating_mul(1_000_000)));
    without_interrupts(|| receive_loop(buf, len, id_out, deadline))
}

/// `ipc_receive`'s result (with request id 0) for a doorbell that rang.
pub const DOORBELL: i64 = 1 << 16;

fn receive_loop(buf: u64, len: u64, id_out: u64, deadline: Option<u64>) -> SysResult {
    let me = super::current_pid();
    let server = super::with_current(|p| p.server.clone());
    loop {
        let wait = prepare_to_wait(super::irq::server_chan(me));
        let fired = super::irq::take_pending(me);
        if fired != 0 {
            drop(wait);
            uaccess::write(id_out, 0u64)?;
            return Ok(fired as i64);
        }
        if server.as_ref().is_some_and(|s| s.doorbell.swap(false, core::sync::atomic::Ordering::Acquire)) {
            drop(wait);
            uaccess::write(id_out, 0u64)?;
            return Ok(DOORBELL);
        }
        let next = lock(|ipc| {
            // Without a service yet, only interrupts and the timeout end the wait.
            let Some(index) = ipc.services.iter().position(|s| s.server == me && s.alive) else {
                return Ok::<_, i64>(Err(()));
            };
            let Some(id) = ipc.services[index].queue.pop_front() else { return Ok(Err(())) };
            let req = ipc.requests.get_mut(&id).expect("queued request exists");
            req.state = State::Taken;
            Ok(Ok((id, core::mem::take(&mut req.message))))
        })?;
        match next {
            Ok((id, message)) => {
                drop(wait);
                if message.len() as u64 > len {
                    fail(id);
                    continue;
                }
                if let Err(e) = uaccess::copy_to(buf, &message).and_then(|_| uaccess::write(id_out, id)) {
                    fail(id);
                    return Err(e);
                }
                return Ok(message.len() as i64);
            }
            Err(()) => {
                if super::signal::interrupted() {
                    return Err(EINTR);
                }
                match deadline {
                    Some(d) if crate::time::now() >= d => return Err(ETIMEDOUT),
                    Some(d) => wait.sleep_until(d),
                    None => wait.sleep(),
                }
            }
        }
    }
}

/// ipc_reply(id, buffer, length): answers a request taken with receive.
pub fn reply(id: u64, buf: u64, len: u64) -> SysResult {
    if len as usize > MAX_MESSAGE {
        return Err(EINVAL);
    }
    let data = uaccess::read_vec(buf, len)?;
    let me = super::current_pid();
    lock(|ipc| {
        let req = ipc.requests.get(&id).ok_or(EINVAL)?;
        if req.state != State::Taken || ipc.services[req.service].server != me {
            return Err(EINVAL);
        }
        ipc.bytes += data.len() as u64;
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

fn enqueue(service: usize, message: Vec<u8>, waits: bool, generation: Option<u64>) -> Result<u64, i64> {
    enqueue_as(service, message, waits, generation, false)
}

fn enqueue_as(service: usize, message: Vec<u8>, waits: bool, generation: Option<u64>, control: bool) -> Result<u64, i64> {
    let (id, server) = lock(|ipc| {
        let current = ipc.services.get(service).filter(|s| s.alive).ok_or(EIO)?;
        if generation.is_some_and(|g| g != current.generation) {
            return Err(EIO);
        }
        if control && !current.channels {
            return Err(EOPNOTSUPP);
        }
        let id = ipc.next_id | if control { CONTROL } else { 0 };
        ipc.next_id += 1;
        ipc.calls += 1;
        ipc.bytes += message.len() as u64;
        ipc.requests.insert(id, Request { service, waits, message, reply: Vec::new(), state: State::Queued });
        ipc.services[service].queue.push_back(id);
        Ok((id, ipc.services[service].server))
    })?;
    wakeup(super::irq::server_chan(server));
    Ok(id)
}

/// Sends `message` to `service` and sleeps until the reply. Not
/// interruptible by signals: the server may already be working on it.
pub fn call(service: usize, message: Vec<u8>) -> Result<Vec<u8>, i64> {
    // Enqueueing, checking and sleeping happen with interrupts off, so the
    // reply cannot slip in between the check and the sleep.
    without_interrupts(|| {
        let id = enqueue(service, message, true, None)?;
        wait_reply(id)
    })
}

/// Sends the control request `message` (a channel offer) to this
/// registration of a server that accepts them; returns its id. The caller
/// waits on `reply_chan(id)`, takes the answer with `take_reply` and
/// gives the request up with `abandon` (it decides itself when it is done:
/// a channel's connect is complete once the service attached, answered or
/// not).
pub fn send_control(to: Instance, message: Vec<u8>) -> Result<u64, i64> {
    enqueue_as(to.service, message, true, Some(to.generation), true)
}

/// Where the sender of request `id` waits: its answer, its failure and
/// whatever else the sender waits for (`wake`) wake it there.
pub fn reply_chan(id: u64) -> usize {
    request_chan(id)
}

/// Wakes whoever waits on `reply_chan(id)`.
pub fn wake(id: u64) {
    wakeup(request_chan(id));
}

/// The answer to request `id` if it came (EIO if the server failed it or
/// died); the request is then gone.
pub fn take_reply(id: u64) -> Option<Result<Vec<u8>, i64>> {
    lock(|ipc| match ipc.requests.get(&id).map(|r| r.state) {
        Some(State::Done) => Some(Ok(ipc.requests.remove(&id).expect("present").reply)),
        Some(State::Failed) | None => {
            ipc.requests.remove(&id);
            Some(Err(EIO))
        }
        _ => None,
    })
}

/// Gives request `id` up: withdrawn if still queued, its answer dropped if
/// the server took it.
pub fn abandon(id: u64) {
    lock(|ipc| match ipc.requests.get(&id).map(|r| (r.state, r.service)) {
        Some((State::Queued, service)) => {
            ipc.requests.remove(&id);
            if let Some(s) = ipc.services.get_mut(service) {
                s.queue.retain(|&q| q != id);
            }
        }
        Some((State::Taken, _)) => ipc.requests.get_mut(&id).expect("present").waits = false,
        Some(_) => {
            ipc.requests.remove(&id);
        }
        None => {}
    });
}

/// The process serving this registration, while it is alive.
pub fn server_of(to: Instance) -> Option<Pid> {
    lock(|ipc| ipc.services.get(to.service).filter(|s| s.alive && s.generation == to.generation).map(|s| s.server))
}

/// Waits for the reply to `id` (not interruptible: the server may already
/// be working on it).
fn wait_reply(id: u64) -> Result<Vec<u8>, i64> {
    loop {
        let wait = prepare_to_wait(request_chan(id));
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
        wait.sleep();
    }
}

/// The current registration behind `name`, if its server is alive.
pub fn instance(name: &str) -> Option<Instance> {
    lock(|ipc| {
        let i = ipc.services.iter().position(|s| s.alive && s.name == name)?;
        Some(Instance { service: i, generation: ipc.services[i].generation })
    })
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

pub fn is_alive(service: usize) -> bool {
    lock(|ipc| ipc.services.get(service).is_some_and(|s| s.alive))
}

/// Waits up to `timeout` nanoseconds for `name` to be registered, looking
/// every scheduler tick.
pub fn wait_for(name: &str, timeout: u64) -> Option<(usize, u64)> {
    let deadline = crate::time::now().saturating_add(timeout);
    loop {
        if let Some(found) = lookup(name) {
            return Some(found);
        }
        let now = crate::time::now();
        if now >= deadline {
            return None;
        }
        let _ = super::sleep_until(now.saturating_add(crate::timer::TICK_NS).min(deadline));
    }
}
