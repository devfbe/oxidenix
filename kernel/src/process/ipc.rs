//! Synchronous message passing between the kernel and user-space servers.
//!
//! A server registers under a name, then loops over `receive` and `reply`.
//! The kernel acts as the client on behalf of user programs: `call` queues
//! a request and sleeps until the reply arrives; `post` queues one without
//! waiting. Messages are copied through the kernel.
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
    /// index, so clients that hold per-server state (socket handles)
    /// check the generation to never reach the new server with it.
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

/// The channel a server's event `token` wakes (see `notify`), for one
/// registration of the server: a restarted server cannot wake the
/// waiters of its predecessor's objects. Different tokens may share a
/// channel; that only costs a spurious wakeup.
pub fn event_chan(server: Instance, token: u64) -> usize {
    (1 << 44) | ((server.generation as usize & 0xfff) << 32) | (token as usize & 0xffff_ffff)
}

/// The channel woken when this registration's server dies.
pub fn gone_chan(server: Instance) -> usize {
    0x20_0000_0000 + server.generation as usize
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
    }
    Ok(registered)
}

/// ipc_receive(buffer, length, &id, timeout_ms): waits for the next request
/// to one of the caller's services and returns its length. A negative
/// timeout waits forever; when it runs out, the result is ETIMEDOUT.
///
/// Interrupts of the caller's device lines come first: they are reported
/// with request id 0 and the mask of fired lines as the result.
pub fn receive(buf: u64, len: u64, id_out: u64, timeout_ms: i64) -> SysResult {
    let deadline = (timeout_ms >= 0).then(|| crate::time::now().saturating_add((timeout_ms as u64).saturating_mul(1_000_000)));
    without_interrupts(|| receive_loop(buf, len, id_out, deadline))
}

fn receive_loop(buf: u64, len: u64, id_out: u64, deadline: Option<u64>) -> SysResult {
    let me = super::current_pid();
    loop {
        let wait = prepare_to_wait(super::irq::server_chan(me));
        let fired = super::irq::take_pending(me);
        if fired != 0 {
            drop(wait);
            uaccess::write(id_out, 0u64)?;
            return Ok(fired as i64);
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

/// ipc_notify(token): a server announces that its object `token` changed
/// (netd: a socket's readiness), waking whoever waits on it. Only for the
/// services the caller serves.
pub fn notify(token: u64) -> SysResult {
    let me = super::current_pid();
    let served: Vec<Instance> = lock(|ipc| {
        (0..ipc.services.len())
            .filter(|&i| ipc.services[i].alive && ipc.services[i].server == me)
            .map(|i| Instance { service: i, generation: ipc.services[i].generation })
            .collect()
    });
    if served.is_empty() {
        return Err(EPERM);
    }
    for server in served {
        wakeup(event_chan(server, token));
    }
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

/// Like `call`, but a signal ends the wait with EINTR, for requests that may
/// wait a long time (a socket waiting for data). A request still queued is
/// withdrawn; one the server has taken is abandoned (its reply will be
/// dropped) and the message `cancel(id)` is posted so the server forgets it.
pub fn call_interruptible(to: Instance, message: Vec<u8>, cancel: impl FnOnce(u64) -> Vec<u8>) -> Result<Vec<u8>, i64> {
    let service = to.service;
    without_interrupts(|| {
        let id = enqueue(service, message, true, Some(to.generation))?;
        let mut cancel = Some(cancel);
        wait_reply_with(id, &mut || {
            let taken = lock(|ipc| match ipc.requests.get(&id).map(|r| r.state) {
                Some(State::Queued) => {
                    ipc.requests.remove(&id);
                    if let Some(s) = ipc.services.get_mut(service) {
                        s.queue.retain(|&q| q != id);
                    }
                    Some(false)
                }
                Some(State::Taken) => {
                    ipc.requests.get_mut(&id).expect("present").waits = false;
                    Some(true)
                }
                _ => None,
            });
            match taken {
                Some(true) => {
                    if let Some(cancel) = cancel.take() {
                        post_to(to, cancel(id));
                    }
                    true
                }
                Some(false) => true,
                // Already answered: take the reply instead.
                None => false,
            }
        })
    })
}

/// Sends the control request `message` (a channel offer) to this
/// registration of a server that accepts them and waits for the reply. A
/// signal may end the wait: a request still queued is withdrawn; one the
/// server took is given up only if `give_up()` agrees (then EINTR, and the
/// reply is dropped).
pub fn call_control(to: Instance, message: Vec<u8>, give_up: impl Fn() -> bool) -> Result<Vec<u8>, i64> {
    let service = to.service;
    without_interrupts(|| {
        let id = enqueue_as(service, message, true, Some(to.generation), true)?;
        wait_reply_with(id, &mut || {
            // In one hold of the lock, so the server cannot take or answer
            // the request in between. (`give_up` takes only a channel's
            // lock, which is never held around this one.)
            lock(|ipc| match ipc.requests.get(&id).map(|r| r.state) {
                Some(State::Queued) => {
                    ipc.requests.remove(&id);
                    if let Some(s) = ipc.services.get_mut(service) {
                        s.queue.retain(|&q| q != id);
                    }
                    true
                }
                Some(State::Taken) if give_up() => {
                    ipc.requests.get_mut(&id).expect("present").waits = false;
                    true
                }
                _ => false,
            })
        })
    })
}

/// The process serving this registration, while it is alive.
pub fn server_of(to: Instance) -> Option<Pid> {
    lock(|ipc| ipc.services.get(to.service).filter(|s| s.alive && s.generation == to.generation).map(|s| s.server))
}

fn wait_reply(id: u64) -> Result<Vec<u8>, i64> {
    wait_reply_with(id, &mut || false)
}

/// Waits for the reply to `id`. When a signal is pending, `abandon` may
/// give up the request (returns true; the result is then EINTR).
fn wait_reply_with(id: u64, abandon: &mut dyn FnMut() -> bool) -> Result<Vec<u8>, i64> {
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
        if super::signal::interrupted() && abandon() {
            return Err(EINTR);
        }
        wait.sleep();
    }
}

/// Sends `message` without waiting for (or receiving) a reply. Never
/// sleeps, so it can be used while dropping objects.
pub fn post(service: usize, message: Vec<u8>) {
    let _ = enqueue(service, message, false, None);
}

/// Like `post`, but only to this registration of the server; dropped if
/// the server was restarted meanwhile.
pub fn post_to(to: Instance, message: Vec<u8>) {
    let _ = enqueue(to.service, message, false, Some(to.generation));
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
    let (failed, gone): (Vec<u64>, Vec<Instance>) = lock(|ipc| {
        let dead: Vec<usize> = (0..ipc.services.len()).filter(|&i| ipc.services[i].server == pid && ipc.services[i].alive).collect();
        let gone = dead.iter().map(|&i| Instance { service: i, generation: ipc.services[i].generation }).collect();
        for &i in &dead {
            ipc.services[i].alive = false;
            ipc.services[i].queue.clear();
        }
        let failed = ipc
            .requests
            .iter()
            .filter(|(_, r)| dead.contains(&r.service) && matches!(r.state, State::Queued | State::Taken))
            .map(|(&id, _)| id)
            .collect();
        (failed, gone)
    });
    for id in failed {
        fail(id);
    }
    // Polls on its objects (sockets) see the server gone.
    for server in gone {
        wakeup(gone_chan(server));
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
