# ADR 0007: Internet sockets: shared control blocks, byte rings, answers at once

Date: 2026-10-09. Status: accepted.

## Context

R7b moves `AF_INET` sockets from the kernel into the Linux server, which talks to netd over a
channel (ADR 0005, `docs/design/io-rings.md`). The file protocol (`fsring`) is
request/completion: every read and write is a descriptor, the data in a grant. Sockets differ
from files in three ways that matter:

1. **Data arrives unasked.** A TCP connection receives whenever the peer sends; a request per
   read would put a round trip into every `recv`, and netd would hold unanswered requests
   (as its IPC protocol did, with cancellation for signals).
2. **Readiness is continuous state** that the kernel's `poll` and `epoll` need at any time,
   for sockets no thread of the server is looking at.
3. **netd's stack is smoltcp**, whose socket buffers are its own; netd must survive a client
   that revokes a grant under it (instance teardown does exactly that), so it may touch
   granted memory only with the fault-surviving copy, never with plain loads, stores or
   atomics.

Alternatives considered:

- **Request per operation** (as `fsring`): `RECV` and `SEND` descriptors with data in granted
  buffers, netd keeping a request until it can complete. Simple, but a round trip (two wakeups
  when both sides sleep) per `recv`, pending requests in netd with cancellation, and
  readiness still needs a separate path.
- **smoltcp's buffers in shared memory**, the server reading them directly (one copy fewer).
  Needs smoltcp's private ring positions (published after every poll) and depends on its
  internals (it resets a ring's start when it empties); netd's stores into granted memory
  would fault after a revoke. Rejected: fragile, and unsafe for netd.
- **Per-socket state in granted memory**: netd's atomics on it would fault after a revoke.
- **Readiness through the completion ring** (an event per change): needs room accounting per
  socket and a consumer for events while no thread of the server waits.

## Decision

- A channel gets an optional **shared area** (`chan_create`'s third argument, `Offer`'s
  `shared` pages): memory both ends map read and write that the client cannot take from the
  service. It holds a **control block** per socket (netd's line: event counter, state bits,
  receive tail, send head, error and its count, accept backlog, `rx_wait`; the server's line:
  receive head, send tail, waiters) and two bitmaps with doorbells (sockets netd changed, for
  the server; sockets the server changed, for netd).
- **Socket data in byte rings** (64 KiB each way) in granted memory of the server's buffer
  pool; positions in the control block. Each side copies only between its own memory and the
  rings; netd with `oxrt::copy`. TCP data moves without requests; datagrams arrive as records
  and are sent by a `SEND` request (its errors are the call's).
- **netd answers every request at once**; all waiting is the server's, on the socket's `seq`
  (a futex in the shared area that netd wakes directly). Signals, timeouts and
  `O_NONBLOCK` are the server's alone.
- **A net thread per instance** (`ROLE_NET`, a third service thread of the pager's process)
  turns the bitmap netd marks into `kfd_ready` reports, so the kernel's `poll` and `epoll` see
  every change without any thread of the program in the server.
- **One netd for all instances**, ports global in its stack; a channel names only its own
  control blocks.

## Consequences

- One copy per direction in the server and one in netd (rings ↔ smoltcp), beside smoltcp's
  own frame copies; no system call and no request per `send` or `recv` under load, a wakeup
  only for a side that sleeps.
- The kernel gains a small mechanism (the shared area: a slot count plus pages, mapped like the
  rings) and a third service thread per instance; it loses its socket layer, the poll source of
  server-announced files (`ipc_notify`) and the netd relay of interface records.
- Memory: 128 KiB of the server's pool per connected socket (in 2 MiB grants), smoltcp's
  buffers in netd (its heap grows to hold them), 33 pages of shared area per instance.
- The control block's layout and its memory ordering become part of the ABI between the server
  and netd (`crates/netring`, tested on the host).
- `SO_LINGER` with a timeout does not block `close` (the close reaches the server after the
  descriptor is gone); a zero timeout aborts as on Linux.
