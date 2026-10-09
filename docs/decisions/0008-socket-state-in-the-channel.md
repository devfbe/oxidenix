# ADR 0008: Internet sockets: shared control blocks, byte rings, answers at once

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
  control blocks, and port sharing (`SO_REUSEADDR`) never lets one instance take a port
  another one serves (`netring`'s port rules, tested on the host); netd accounts per instance
  (the kernel's offer names it), whatever number of channels it opens: at most two channels,
  and of every other resource a reserve kept for each instance with a channel, the rest to
  whoever asks first.
- **netd's memory follows use**: smoltcp (vendored with small patches) lets a connection's
  buffers grow from Linux's first sizes while they limit the transfer, and under pressure
  connections stay small and idle ones give their send buffers back (a receive buffer never
  shrinks below the window it announced).

## Threat model: instances sharing netd

The instances (process trees, each with its own Linux server) share one netd, one address and
one port space, like containers in one Linux network namespace. netd assumes any instance's
server may be hostile (it can send any descriptor, publish any position, revoke grants at any
time) and so may anything on the network.

- **Isolation.** A channel names only its own sockets; the instance comes from the kernel's
  offer, which no client can forge; every budget charge, port claim, 4-tuple check and ICMP
  delivery is keyed by it.
- **What an instance learns of another**, kept to what one IP address makes unavoidable: that
  a port is in use (`EADDRINUSE` at bind, as on Linux; ports are never shared across
  instances, so the 4-tuple check of a connect only ever looks at the instance's own
  connections and cannot reveal whom another one talks to); nothing of other instances'
  peers, sequence numbers, buffers or traffic. Ephemeral ports follow RFC 6056's third
  algorithm (a keyed hash of the destination, the key from the kernel's generator, plus a
  counter; a random start for a bind to port 0) and TCP's initial sequence numbers RFC 6528
  (a 4-microsecond clock plus a keyed hash of the 4-tuple), so neither reveals another
  instance's activity nor can anyone predict them. ICMP
  echo identifiers are netd's on the wire (`netring::EchoIds`): an instance gets the replies
  to its own requests only, whatever identifier it picks, and the errors about its own ports.
- **What ICMP still shares**: messages that concern no instance go to every raw socket, as on
  Linux: echo requests from other hosts (which netd answers itself), and every type netd does
  not attribute (timestamp, router and address-mask messages, redirects for no port of
  anyone's). Errors about TCP or UDP ports go to the instance holding the port at the time
  they arrive; one that comes after the port changed hands (a late error about a closed
  connection whose port another instance took since) goes to the new holder, and one about
  a port nobody holds goes to nobody. Echo replies and errors about requests whose
  identifier netd no longer remembers (after 60 s unused, or beyond 64 per instance) go to
  nobody.
- **What an instance can deny another**: nothing below a reserve. Every shared resource
  (buffer memory, smoltcp sockets with those in TIME-WAIT, orphans, half-open connections) is a
  `netring::Budget` with a cap per instance and a reserve kept for every instance with a
  channel; channels are capped per instance and an idle one gives its slot up; the
  ephemeral range is wider than what one instance can hold (its share of smoltcp sockets). A flood from the network against one instance's listener spends that
  instance's share of half-open connections.
- **At each cap** (as Linux where it has one; none blocks an instance for good or leaks):

  | cap | what happens |
  |---|---|
  | buffer bytes, smoltcp sockets | `ENOBUFS` for the socket; a connection that arrives is reset; under pressure connections start and stay small |
  | smoltcp sockets, TIME-WAIT among them | the instance's own oldest TIME-WAIT connection goes, then others' beyond their reserve; an instance without room skips TIME-WAIT (closed at once, tcp_max_tw_buckets) |
  | orphans | a close resets the connection (tcp_max_orphans) |
  | a close's leftovers | the connection is reset |
  | half-open connections | the SYN is answered with a reset |
  | backlog | `listen` takes a smaller backlog (at least one) |
  | channels | an idle channel gives its slot up, else `ENOBUFS` |
  | echo identifiers | the instance's least recently used goes |

- **From the network**: every packet netd parses itself (ICMP for routing) is length-checked
  (tested over truncations and changed bytes on the host); SYNs cost a 4 KiB buffer until the
  handshake completes; closed connections that stop making progress are reset; the queues to
  the card and the loopback push back instead of growing.

## Consequences

- One copy per direction in the server and one in netd (rings ↔ smoltcp), beside smoltcp's
  own frame copies; no system call and no request per `send` or `recv` under load, a wakeup
  only for a side that sleeps.
- The kernel gains a small mechanism (the shared area: a slot count plus pages, mapped like the
  rings) and a third service thread per instance; it loses its socket layer, the poll source of
  server-announced files (`ipc_notify`) and the netd relay of interface records.
- Memory: 128 KiB of the server's pool per connected socket (in 2 MiB grants), smoltcp's
  buffers in netd (80 KiB for a new connection, up to 2 MiB under load, 4 KiB when idle under
  pressure; mappings of their own that go with the socket; at most 32 MiB in all), 33 pages
  of shared area per instance.
- netd carries a vendored smoltcp (`third_party/smoltcp`, its patches listed in its
  `Cargo.toml` and tested with smoltcp's own tests).
- The control block's layout and its memory ordering become part of the ABI between the server
  and netd (`crates/netring`, tested on the host).
- `SO_LINGER` with a timeout does not block `close` (the close reaches the server after the
  descriptor is gone); a zero timeout aborts as on Linux.
