//! The socket service: netd's end of the instances' channels (phase R7b,
//! ADR 0008, the protocol `netring`), mapped onto smoltcp's sockets. One
//! TCP/IP stack serves every channel: ports are global, and a channel
//! names only its own sockets (by their control blocks in its shared
//! area), so one instance can neither see nor touch another's.
//!
//! **Requests** (`netring::Request`) are answered at once: a connect starts
//! the handshake and completes, an accept takes a connection the
//! listener's count announced or answers EAGAIN. Whatever takes time shows
//! in the control block, which `pump` keeps current every round: TCP bytes
//! move between smoltcp's buffers and the client's rings (granted memory,
//! reached only with the fault-surviving copy, `oxrt::copy`), datagrams go
//! into the receive ring as records, the state bits, errors and the accept
//! backlog are published; what changed advances the block's `seq` (waking
//! the client's waiters) and is marked for the client's net thread.
//!
//! **Closing.** A socket the client closes leaves its channel at once (its
//! control block and area are the client's again when `CLOSE` completes):
//! a TCP connection keeps what its send ring still held, in netd's memory,
//! sends it, then its FIN, and goes once smoltcp is done with it (after
//! TIME-WAIT); its port stays taken meanwhile, as on Linux. Unread data or
//! `CLOSE_ABORT` resets it instead, as does data that arrives after the
//! close (nobody can read it: the peer's writes fail with EPIPE), and a
//! listener resets the connections nobody accepted. A channel whose client
//! went closes all of its sockets the same way (the rings went with the
//! grants).
//!
//! **Hostile clients.** Each descriptor is copied out of the ring once and
//! validated (`netring::Request::decode`); grants must exist, be writable
//! and hold the area; positions the client publishes are checked against
//! the ring's size (a violation resets the socket, which reports EIO); a
//! copy from or to a grant revoked meanwhile fails that socket the same
//! way, never netd. Resources are bounded in all and per instance (the
//! kernel's offer names the instance; `netring::Budget`, charged before
//! anything is allocated, with a reserve kept for every instance that has
//! a channel): `MAX_CHANNELS` channels and two per instance, `MAX_GRANTS`
//! grants a channel, smoltcp sockets (`MAX_SMOLTCP`), the bytes of their
//! buffers and of closed connections' leftovers (`BUDGET`), TIME-WAIT
//! records (`MAX_LINGERING`), connections closed but not finished
//! (`MAX_ORPHANS`), no more requests taken than the completion ring has
//! room for.
//!
//! **Memory follows use**, as Linux autotunes its socket buffers: a TCP
//! socket starts without buffers (a listener's backlog sockets cost only
//! their place in smoltcp until a connection comes), gets Linux's first
//! sizes (`TCP_RX_INIT`, `TCP_TX_INIT`; smaller ones down to `TCP_MIN` if
//! its instance's budget is short) when it connects or a connection
//! arrives (before its SYN-ACK goes, `arrivals`), and its buffers double
//! (up to `TCP_MAX`) while they are what limits the transfer: the receive
//! buffer when the peer fills half of it or more between two rounds and
//! the client keeps up, the send buffer when the network took half of it
//! and the client's ring holds more than it takes. Under pressure (half
//! of `BUDGET` in use, as Linux's tcp_mem) connections start with
//! `TCP_MIN`, grow no further than the first sizes, and one idle for
//! `TRIM_AFTER` gives its empty buffers back (`trim`: the send buffer
//! entirely, the receive buffer down to `TCP_MIN`), so idle connections
//! cost little, as on Linux, where they hold no buffers. The window scale
//! announced in the SYN is that of `TCP_MAX` (smoltcp's
//! `set_rx_capacity_max`), so a grown buffer opens the window.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering::SeqCst};
use netring::errno::*;
use netring::{
    fill, opt, pieces, state, tcp_port_conflict, udp_port_conflict, Area, Budget, Buf, Completion, Ctl, Endpoint, Kind, Link, PortClaim, PortHolder, Record, Request,
    SharedArea, RECORD_HEADER,
};
use ring::channel::{Header, Layout, Offer};
use ring::{Consumer, Producer, Ring, Wait};
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::{raw, tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpVersion, Ipv4Address, Ipv4Packet, Ipv4Repr};

const N: usize = netring::SLOTS as usize;
const PAGE: u64 = 4096;
/// Channels attached at once (one per Linux server instance).
const MAX_CHANNELS: usize = 64;
/// Grants of a channel netd keeps mapped (`FORGET` lets go).
const MAX_GRANTS: usize = 256;
/// Requests taken from one channel per round, for fairness.
const TAKE_PER_ROUND: usize = 16;
/// A TCP connection's buffers: what it gets when it connects or arrives
/// (Linux's tcp_rmem and tcp_wmem defaults give a first window of 64 KiB
/// and 16 KiB to send; less, down to `TCP_MIN`, if the budget is short),
/// and the most they grow to, each way.
const TCP_RX_INIT: usize = 64 * 1024;
const TCP_TX_INIT: usize = 16 * 1024;
const TCP_MIN: usize = 4096;
const TCP_MAX: usize = 1 << 20;
/// A UDP socket's: room for the largest datagram each way.
const UDP_BUFFER: usize = 64 * 1024;
const UDP_PACKETS: usize = 16;
const RAW_BUFFER: usize = 16 * 1024;
/// A raw socket's hop limit until IP_TTL sets one (Linux's
/// net.ipv4.ip_default_ttl).
const DEFAULT_TTL: u8 = 64;
/// The echo identifiers a raw socket remembers (the last it sent).
const ECHO_IDS: usize = 16;
/// The bytes of smoltcp's socket buffers (and leftovers) netd keeps at
/// most, in mappings of their own (`Region`).
const BUDGET: usize = 32 << 20;
/// Beyond this much of it, memory is under pressure (`pressure`).
const PRESSURE: usize = BUDGET / 2;
/// Under pressure, a connection idle this long gives its buffers back
/// (`trim`).
const TRIM_AFTER: Duration = Duration::from_millis(500);
/// The instances (with a channel) whose reserves the budgets keep: each
/// is guaranteed this share of every budget.
const RESERVED_INSTANCES: usize = 2 * MAX_CHANNELS;
/// smoltcp sockets at once (each takes its place in smoltcp's socket set,
/// on netd's heap, made room for at the start).
pub const MAX_SMOLTCP: usize = 4096;
/// Ports in TIME-WAIT kept at once, and for one instance: half the
/// ephemeral range at most, so connects of others always find a port.
const MAX_LINGERING: usize = 8192;
const INSTANCE_LINGERING: usize = 2048;
/// Connections closed but not finished (sending leftovers, FIN-WAIT,
/// LAST-ACK, CLOSING): beyond this a close resets the connection, as
/// Linux's tcp_max_orphans.
const MAX_ORPHANS: usize = 4096;
/// Channels of one instance (one, and a new one while netd still tears
/// down the old after its client gave it up).
const INSTANCE_CHANNELS: usize = 2;
/// The most connections one listener queues (Linux's net.core.somaxconn
/// before 5.4).
const MAX_BACKLOG: usize = 128;
/// The largest UDP payload (an IPv4 packet of 65535 bytes).
const MAX_UDP: usize = 65507;
const IPV4_HEADER: usize = 20;
/// The Ethernet header a frame adds to an IP packet.
const ETHERNET_HEADER: usize = 14;
/// A connection attempt gives up after this (Linux's 6 SYN retries,
/// tcp_syn_retries), a connection that arrived and is not completed
/// after this (5 SYN-ACK retries, tcp_synack_retries), unacknowledged
/// data after this (15 retries, tcp_retries2).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(127);
const SYN_RECEIVED_TIMEOUT: Duration = Duration::from_secs(63);
const TCP_TIMEOUT: Duration = Duration::from_secs(924);
/// A closed connection in FIN-WAIT-2 is reset after this (Linux's
/// tcp_fin_timeout); one whose leftovers or FIN make no progress (a zero
/// window, a peer that stopped acknowledging) after this (tcp_orphan_retries:
/// Linux gives up on an orphan after about 8 retries).
const FIN_TIMEOUT: Duration = Duration::from_secs(60);
const ORPHAN_TIMEOUT: Duration = Duration::from_secs(100);
const FIRST_EPHEMERAL: u16 = 49152;
/// How long a closed connection's port stays taken (smoltcp's TIME-WAIT).
const TIME_WAIT: Duration = Duration::from_secs(10);
/// A channel without sockets and requests for this long gives its slot
/// to another instance's if all are taken.
const CHANNEL_IDLE: Duration = Duration::from_secs(10);

/// A futex wake of one sleeper (a ring's doorbell: the client's reaper).
struct WakeOne;

impl Wait for WakeOne {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let _ = oxrt::futex_wait(word, value, None);
    }

    fn wake(&self, word: &AtomicU32) {
        let _ = oxrt::futex_wake(word, 1);
    }
}

/// A futex wake of every sleeper (a socket's waiters, the net thread).
struct WakeAll;

impl Wait for WakeAll {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let _ = oxrt::futex_wait(word, value, None);
    }

    fn wake(&self, word: &AtomicU32) {
        let _ = oxrt::futex_wake(word, i32::MAX as u32);
    }
}

/// A grant as netd knows it: mapped here until `FORGET` or the channel's
/// end.
#[derive(Clone, Copy)]
struct Grant {
    addr: *mut u8,
    bytes: u64,
    writable: bool,
}

/// Memory of its own, mapped (and committed) at once and unmapped when
/// dropped: a smoltcp buffer, a closed connection's leftovers. So netd's
/// memory follows its sockets (its heap stays small), and short memory is
/// ENOBUFS for the socket, never a failed allocation in netd.
struct Region {
    addr: *mut u8,
    len: usize,
    mapped: usize,
}

impl Region {
    fn new(len: usize) -> Result<Region, i64> {
        const PROT_RW: u64 = 3;
        const MAP_PRIVATE_ANON: u64 = 0x22;
        let mapped = len.max(1).next_multiple_of(PAGE as usize);
        let addr = oxrt::syscall(oxrt::sys::MMAP, [0, mapped as u64, PROT_RW, MAP_PRIVATE_ANON, u64::MAX, 0]);
        if addr < 0 {
            return Err(ENOBUFS);
        }
        Ok(Region { addr: addr as *mut u8, len, mapped })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.addr, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.addr, self.len) }
    }

    /// Its bytes as a smoltcp buffer. It lives as long as the region: the
    /// owner drops the region only after the socket that uses it left
    /// smoltcp or moved to another buffer (`Service::drop_socket`,
    /// `Service::grow`).
    fn buffer(&mut self) -> &'static mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.addr, self.len) }
    }
}

/// A smoltcp socket's memory: its place in smoltcp's socket set and its
/// buffers, charged to the instance `owner` (`Service::socks`,
/// `Service::bytes`).
struct Mem {
    owner: u64,
    rx: Option<Region>,
    tx: Option<Region>,
}

impl Mem {
    fn bytes(&self) -> usize {
        self.rx.as_ref().map_or(0, |r| r.mapped) + self.tx.as_ref().map_or(0, |r| r.mapped)
    }
}

/// A buffer of `len` bytes (none for 0) and its storage for smoltcp.
fn buffer(len: usize) -> Result<(Option<Region>, &'static mut [u8]), i64> {
    if len == 0 {
        return Ok((None, &mut []));
    }
    let mut r = Region::new(len)?;
    let b = r.buffer();
    Ok((Some(r), b))
}

/// The bytes `len` bytes of buffer take (whole pages).
fn mapped(len: usize) -> usize {
    if len == 0 { 0 } else { len.next_multiple_of(PAGE as usize) }
}

/// A TCP buffer that limits a transfer: one way of the socket.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Rx,
    Tx,
}

/// What a TCP connection's memory should do (`pump_one` asks, `pump`
/// does it).
#[derive(Clone, Copy)]
enum Want {
    /// A buffer limits the transfer (or the send buffer went while idle
    /// and the client writes again).
    Grow(Way),
    /// Idle under pressure: its buffers go back (`trim`).
    Trim,
}

impl Drop for Region {
    fn drop(&mut self) {
        let _ = oxrt::munmap(self.addr, self.mapped);
    }
}

/// A socket's rings, resolved: where they lie in netd's mapping.
#[derive(Clone, Copy)]
struct Rings {
    rx: *mut u8,
    tx: *mut u8,
    size: u32,
}

impl Rings {
    /// Copies `dst.len()` bytes from ring position `pos` of the ring at
    /// `base`; false if the grant went meanwhile.
    fn read(base: *const u8, size: u32, pos: u32, dst: &mut [u8]) -> bool {
        let [(a, n), (b, m)] = pieces(pos, dst.len() as u32, size);
        unsafe {
            oxrt::copy::copy(dst.as_mut_ptr(), base.add(a as usize), n as usize)
                && oxrt::copy::copy(dst.as_mut_ptr().add(n as usize), base.add(b as usize), m as usize)
        }
    }

    /// Copies `src` into the ring at `base` at position `pos`.
    fn write(base: *mut u8, size: u32, pos: u32, src: &[u8]) -> bool {
        let [(a, n), (b, m)] = pieces(pos, src.len() as u32, size);
        unsafe {
            oxrt::copy::copy(base.add(a as usize), src.as_ptr(), n as usize)
                && oxrt::copy::copy(base.add(b as usize), src.as_ptr().add(n as usize), m as usize)
        }
    }
}

struct Tcp {
    /// The connection's socket, or a listener's first.
    handle: SocketHandle,
    /// Where it is bound (by bind, listen, connect or its listener).
    local: Option<IpListenEndpoint>,
    reuse: bool,
    /// Listening: the sockets waiting for connections.
    backlog: Option<Vec<SocketHandle>>,
    /// A connect started then (until it completed or failed).
    connecting: Option<Instant>,
    /// It was connected.
    established: bool,
    /// The last connect failed (until the next one).
    failed: bool,
    /// SHUT_WR: the FIN goes once the send ring is drained; sent.
    fin_pending: bool,
    fin_sent: bool,
    /// smoltcp's state last round (a reset shows as a jump to Closed).
    last: tcp::State,
    /// Its options (SETOPT): a listener's apply to every socket of its
    /// backlog, so the connections it hands out have them.
    opts: Opts,
    /// Idle since then (nothing moved and nothing waits): under memory
    /// pressure its buffers go back after `TRIM_AFTER`.
    quiet: Option<Instant>,
}

/// A TCP socket's options.
#[derive(Clone, Copy)]
struct Opts {
    nodelay: bool,
    keepalive: Option<Duration>,
    ttl: Option<u8>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { nodelay: false, keepalive: None, ttl: None }
    }
}

impl Opts {
    fn apply(&self, s: &mut tcp::Socket) {
        s.set_nagle_enabled(!self.nodelay);
        s.set_keep_alive(self.keepalive);
        s.set_hop_limit(self.ttl);
    }
}

enum Proto {
    Tcp(Tcp),
    Udp { handle: SocketHandle, peer: Option<IpEndpoint>, reuse: bool },
    /// `ttl`: the hop limit of the packets the socket sends (IP_TTL);
    /// `ids`: the identifiers of the echo requests it sent last (their
    /// replies are its instance's, `IcmpOwners`).
    Raw { handle: SocketHandle, peer: Option<Ipv4Address>, ttl: u8, ids: Vec<u16> },
}

/// What netd publishes of a socket, as it last did.
#[derive(Default)]
struct Shown {
    state: u32,
    rx_tail: u32,
    tx_head: u32,
    backlog: u32,
    err_seq: u32,
}

/// A socket of a channel.
struct Sock {
    proto: Proto,
    area: Option<Area>,
    /// netd's positions (never read back from shared memory).
    rx_tail: u32,
    tx_head: u32,
    /// The state bits now, the connections ready to accept.
    state: u32,
    backlog: u32,
    /// Errors posted so far.
    err_seq: u32,
    /// netd waits for room in the receive ring (`rx_wait` is set).
    rx_wait: bool,
    shown: Shown,
}

impl Sock {
    fn new(proto: Proto, area: Option<Area>, state: u32) -> Sock {
        Sock { proto, area, rx_tail: 0, tx_head: 0, state, backlog: 0, err_seq: 0, rx_wait: false, shown: Shown { state, ..Shown::default() } }
    }
}

/// Posts error `e` (a positive errno) in `ctl`.
fn post_error(err_seq: &mut u32, ctl: &Ctl, e: i64) {
    *err_seq = err_seq.wrapping_add(1);
    ctl.netd.error.store(e as u32, SeqCst);
}

/// A TCP connection its owner closed, finishing (after its leftovers), or
/// a socket that was reset and goes once its reset went out.
struct Closing {
    handle: SocketHandle,
    /// What its send ring still held, sent before the FIN (charged to the
    /// owner's bytes until it goes).
    leftover: Option<Region>,
    sent: usize,
    local: Option<IpListenEndpoint>,
    reuse: bool,
    /// The instance it belonged to.
    owner: u64,
    /// It finishes in order (an orphan, `Service::orphans`), not by a
    /// reset.
    orphan: bool,
    /// Its state and what it still has to send when it last changed, and
    /// when that was: an orphan that makes no progress is reset
    /// (`FIN_TIMEOUT`, `ORPHAN_TIMEOUT`).
    mark: (tcp::State, usize),
    since: Instant,
}

impl Closing {
    /// When it is reset unless it makes progress (None: it is reset
    /// already).
    fn deadline(&self) -> Option<Instant> {
        match self.mark.0 {
            _ if !self.orphan => None,
            tcp::State::FinWait2 => Some(self.since + FIN_TIMEOUT),
            _ => Some(self.since + ORPHAN_TIMEOUT),
        }
    }

    /// The leftovers go (their bytes back to the owner).
    fn drop_leftover(&mut self, bytes: &mut Budget) {
        if let Some(r) = self.leftover.take() {
            bytes.uncharge(self.owner, r.mapped);
        }
        self.sent = 0;
    }
}

/// A connection in TIME-WAIT, as netd keeps it: its port (for the port
/// rules) and its endpoints (no new connection takes its 4-tuple), until
/// `until`.
struct Lingering {
    holder: PortHolder,
    tuple: Option<(IpEndpoint, IpEndpoint)>,
    until: Instant,
}

struct Chan {
    id: u64,
    /// The instance that connected it (`Offer::instance`): what the
    /// channel takes is charged to it, its ports are its.
    owner: u64,
    header: &'static Header,
    area: &'static SharedArea,
    requests: Consumer<'static, N>,
    completions: Producer<'static, N>,
    grants: BTreeMap<u32, Grant>,
    socks: BTreeMap<u32, Sock>,
    /// Completions pushed this round (ring the client's doorbell).
    rang: bool,
    /// Sockets marked for the net thread this round.
    marked: bool,
    /// When it last had a request (`evict_idle`).
    used: Instant,
}

/// Network configuration (DHCP's).
#[derive(Default, Clone, Copy)]
pub struct Config {
    pub address: u32,
    pub prefix: u8,
    pub gateway: u32,
    pub dns: u32,
    pub mac: [u8; 6],
}

pub struct Service {
    chans: Vec<Option<Chan>>,
    closing: Vec<Closing>,
    /// Closed connections in TIME-WAIT: only their port and endpoints, until
    /// then (their smoltcp socket and its buffers went).
    lingering: Vec<Lingering>,
    next_port: u16,
    /// What the instances hold, each within its share: the bytes of
    /// smoltcp's socket buffers (and of closed connections' leftovers),
    /// smoltcp sockets, records of ports in TIME-WAIT, channels.
    bytes: Budget,
    socks: Budget,
    lingering_records: Budget,
    orphans: Budget,
    channels: Budget,
    /// The memory of each smoltcp socket (its buffers).
    mem: BTreeMap<SocketHandle, Mem>,
    /// The TCP buffers that limit their transfer this round, to grow
    /// (kept for its capacity).
    wants: Vec<(SocketHandle, Want)>,
    /// A datagram on its way out.
    scratch: Vec<u8>,
    pub config: Config,
}

type Reply = Result<(i64, [u64; 4]), i64>;

fn ipv4(addr: u32) -> IpAddress {
    IpAddress::Ipv4(Ipv4Address::from_bits(addr))
}

fn bits(addr: IpAddress) -> u32 {
    match addr {
        IpAddress::Ipv4(a) => a.to_bits(),
    }
}

/// A destination: 0.0.0.0 means this host, as on Linux.
fn destination(addr: u32) -> IpAddress {
    ipv4(if addr == 0 { 0x7f00_0001 } else { addr })
}

fn done(status: i64) -> Reply {
    Ok((status, [0; 4]))
}

/// Whether a backlog socket holds a connection to hand out.
fn ready_to_accept(st: tcp::State) -> bool {
    !matches!(st, tcp::State::Listen | tcp::State::SynReceived | tcp::State::Closed)
}

/// The synchronized states a connection leaves for Closed only by a reset
/// (or its timeout): the orderly ends pass LAST-ACK, CLOSING or TIME-WAIT.
fn open(st: tcp::State) -> bool {
    matches!(st, tcp::State::Established | tcp::State::CloseWait | tcp::State::FinWait1 | tcp::State::FinWait2 | tcp::State::SynReceived)
}

impl Service {
    pub fn new() -> Service {
        Service {
            chans: (0..MAX_CHANNELS).map(|_| None).collect(),
            closing: Vec::new(),
            lingering: Vec::new(),
            next_port: FIRST_EPHEMERAL,
            bytes: Budget::new(BUDGET, BUDGET / RESERVED_INSTANCES, BUDGET),
            socks: Budget::new(MAX_SMOLTCP, MAX_SMOLTCP / RESERVED_INSTANCES, MAX_SMOLTCP),
            lingering_records: Budget::new(MAX_LINGERING, MAX_LINGERING / RESERVED_INSTANCES, INSTANCE_LINGERING),
            orphans: Budget::new(MAX_ORPHANS, MAX_ORPHANS / RESERVED_INSTANCES, MAX_ORPHANS),
            channels: Budget::new(MAX_CHANNELS, 0, INSTANCE_CHANNELS),
            mem: BTreeMap::new(),
            wants: Vec::new(),
            scratch: vec![0; MAX_UDP.max(RAW_BUFFER)],
            config: Config::default(),
        }
    }

    // ------------------------------------------------------------ channels

    /// An offer from the kernel: the status to answer with.
    pub fn offer(&mut self, message: &[u8], sockets: &mut SocketSet<'static>) -> i64 {
        let Some(offer) = Offer::decode(message) else { return -EINVAL };
        let want = Layout::with_shared(netring::SLOTS, netring::SHARED_PAGES);
        if offer.layout() != want {
            return -EINVAL;
        }
        let layout = want.expect("a valid layout");
        // All slots in use: one whose instance has had no socket for a
        // while makes room (its client makes a new channel when it wants
        // one again).
        if !self.chans.iter().any(Option::is_none) {
            self.evict_idle(sockets);
        }
        let Some(slot) = self.chans.iter().position(Option::is_none) else { return -ENOBUFS };
        // An instance gets a few channels (a new one after its old died),
        // never the slots of others.
        if self.channels.charge(offer.instance, 1).is_err() {
            return -ENOBUFS;
        }
        let base = match oxrt::chan_attach(offer.channel) {
            Ok(base) => base,
            Err(e) => {
                self.channels.uncharge(offer.instance, 1);
                return e;
            }
        };
        for b in [&mut self.bytes, &mut self.socks, &mut self.lingering_records, &mut self.orphans] {
            b.activate(offer.instance);
        }
        // Mapped until chan_detach, which comes after the Chan is dropped.
        let (sub, comp) = unsafe { (layout.ring::<N>(base, layout.submission), layout.ring::<N>(base, layout.completion)) };
        self.chans[slot] = Some(Chan {
            id: offer.channel,
            owner: offer.instance,
            header: unsafe { Header::at(base) },
            area: unsafe { SharedArea::at(base.add(layout.shared)) },
            requests: Ring::new(sub).consumer(),
            completions: Ring::new(comp).producer(),
            grants: BTreeMap::new(),
            socks: BTreeMap::new(),
            rang: false,
            marked: false,
            used: crate::now(),
        });
        0
    }

    /// Closes the channel that has had no socket and no request for the
    /// longest time, at least `CHANNEL_IDLE`.
    fn evict_idle(&mut self, sockets: &mut SocketSet<'static>) {
        let now = crate::now();
        let idle = self.chans.iter().enumerate().filter_map(|(c, chan)| {
            let chan = chan.as_ref()?;
            (chan.socks.is_empty() && now - chan.used >= CHANNEL_IDLE).then_some((chan.used, c))
        });
        if let Some((_, c)) = idle.min() {
            self.close_channel(c, sockets);
        }
    }

    /// Announces the sleep in every channel and arms its doorbell: false if
    /// work came meanwhile (then nothing sleeps).
    pub fn prepare_sleep(&mut self) -> bool {
        for chan in self.chans.iter_mut().flatten() {
            let Some(tail) = chan.requests.prepare_sleep() else { return false };
            // The client marks before it rings: a mark made before our
            // announcement is seen here, a later one rings.
            if chan.area.header.service_pending() || chan.header.state() != 0 {
                return false;
            }
            match oxrt::chan_watch(chan.id, tail) {
                Ok(true) => {}
                Ok(false) | Err(_) => return false,
            }
        }
        true
    }

    /// Ends a sleep: clients stop ringing while netd polls.
    pub fn awake(&mut self) {
        for chan in self.chans.iter_mut().flatten() {
            chan.requests.awake();
        }
    }

    /// Takes requests and the clients' marks; true if there were any.
    pub fn serve(&mut self, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> bool {
        let mut progress = false;
        for c in 0..MAX_CHANNELS {
            let Some(chan) = self.chans[c].as_mut() else { continue };
            if chan.header.state() != 0 {
                self.close_channel(c, sockets);
                progress = true;
                continue;
            }
            // The marks only say "look": every socket is pumped each round.
            progress |= chan.area.header.take_service(|_| {});
            for _ in 0..TAKE_PER_ROUND {
                let chan = self.chan(c);
                if chan.completions.room() == 0 {
                    break;
                }
                let Some(d) = chan.requests.pop() else { break };
                chan.used = crate::now();
                progress = true;
                let (status, values) = match Request::decode(&d) {
                    Ok(r) => self.handle(c, r, iface, sockets).unwrap_or_else(|e| (-e, [0; 4])),
                    Err(e) => (-e, [0; 4]),
                };
                let chan = self.chan(c);
                let reply = Completion { tag: d.tag, op: d.op, status, values }.to_desc();
                // Room was checked above.
                let _ = chan.completions.push(&reply);
                chan.rang = true;
            }
        }
        progress
    }

    /// The end of a round: completions and marks reach the clients.
    pub fn flush(&mut self) {
        for chan in self.chans.iter_mut().flatten() {
            if core::mem::take(&mut chan.rang) {
                chan.completions.ring_doorbell(&WakeOne);
            }
            if core::mem::take(&mut chan.marked) {
                chan.area.header.wake_client(&WakeAll);
            }
        }
    }

    /// The client of channel `c` is gone: its sockets close, the channel
    /// goes.
    fn close_channel(&mut self, c: usize, sockets: &mut SocketSet<'static>) {
        let mut chan = self.chans[c].take().expect("in use");
        for (_, s) in core::mem::take(&mut chan.socks) {
            // The rings went with the grants: nothing more of theirs.
            self.release(chan.owner, s, false, None, sockets);
        }
        for (_, g) in core::mem::take(&mut chan.grants) {
            let _ = oxrt::munmap(g.addr, g.bytes as usize);
        }
        let _ = oxrt::chan_detach(chan.id);
        self.channels.uncharge(chan.owner, 1);
        for b in [&mut self.bytes, &mut self.socks, &mut self.lingering_records, &mut self.orphans] {
            b.deactivate(chan.owner);
        }
    }

    fn chan(&mut self, c: usize) -> &mut Chan {
        self.chans[c].as_mut().expect("a channel in use")
    }

    /// Grant `id` of channel `c`, mapped now if it is new.
    fn grant(&mut self, c: usize, id: u32) -> Result<Grant, i64> {
        let chan = self.chan(c);
        if let Some(g) = chan.grants.get(&id) {
            return Ok(*g);
        }
        if chan.grants.len() >= MAX_GRANTS {
            return Err(ENOSPC);
        }
        let (addr, pages, writable) = oxrt::grant_map(chan.id, id).map_err(|_| EBADF)?;
        let g = Grant { addr, bytes: pages * PAGE, writable };
        chan.grants.insert(id, g);
        Ok(g)
    }

    /// Checks an area: a writable grant that holds it.
    fn check_area(&mut self, c: usize, area: &Area) -> Result<(), i64> {
        let g = self.grant(c, area.grant)?;
        if !g.writable || area.end() > g.bytes {
            return Err(EINVAL);
        }
        Ok(())
    }

    /// Where an area's rings are (None if its grant is not mapped).
    fn rings(grants: &BTreeMap<u32, Grant>, area: Option<Area>) -> Option<Rings> {
        let a = area?;
        let g = grants.get(&a.grant)?;
        Some(Rings { rx: unsafe { g.addr.add(a.rx() as usize) }, tx: unsafe { g.addr.add(a.tx() as usize) }, size: a.size })
    }

    // ------------------------------------------------------------ requests

    fn handle(&mut self, c: usize, r: Request, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Reply {
        match r {
            Request::Socket { sock, kind, area } => self.socket(c, sock, kind, area, sockets),
            Request::Bind { sock, at, reuse } => self.bind(c, sock, at, reuse, sockets),
            Request::Listen { sock, backlog, reuse } => self.listen(c, sock, backlog, reuse, sockets),
            Request::Connect { sock, to, area } => self.connect(c, sock, to, area, iface, sockets),
            Request::Accept { sock, new, area } => self.accept(c, sock, new, area, sockets),
            Request::Send { sock, to, len } => self.send(c, sock, to, len, iface, sockets),
            Request::Shutdown { sock, how } => self.shutdown(c, sock, how),
            Request::Close { sock, abort } => self.close(c, sock, abort, sockets),
            Request::Name { sock, peer } => self.name(c, sock, peer, iface, sockets),
            Request::SetOpt { sock, opt, value } => self.setopt(c, sock, opt, value, sockets),
            Request::Links { buf } => self.links(c, buf),
            Request::Forget { grant } => {
                let chan = self.chan(c);
                if chan.socks.values().any(|s| s.area.is_some_and(|a| a.grant == grant)) {
                    return Err(EBUSY);
                }
                if let Some(g) = chan.grants.remove(&grant) {
                    let _ = oxrt::munmap(g.addr, g.bytes as usize);
                }
                done(0)
            }
        }
    }

    /// Adds a socket to smoltcp for `owner`, with buffers of `rx` and `tx`
    /// bytes in memory of their own; `make` builds it from them. Its place
    /// and its bytes are charged first (ENOBUFS if the owner's share or
    /// netd's memory is short: nothing is taken then).
    fn add_socket<F>(&mut self, owner: u64, rx: usize, tx: usize, sockets: &mut SocketSet<'static>, make: F) -> Result<SocketHandle, i64>
    where
        F: FnOnce(&'static mut [u8], &'static mut [u8], &mut SocketSet<'static>) -> SocketHandle,
    {
        let bytes = mapped(rx) + mapped(tx);
        self.socks.charge(owner, 1)?;
        if let Err(e) = self.bytes.charge(owner, bytes) {
            self.socks.uncharge(owner, 1);
            return Err(e);
        }
        let made = buffer(rx).and_then(|(r, rb)| buffer(tx).map(|(t, tb)| (r, rb, t, tb)));
        let Ok((rx, rx_buffer, tx, tx_buffer)) = made else {
            self.socks.uncharge(owner, 1);
            self.bytes.uncharge(owner, bytes);
            return Err(ENOBUFS);
        };
        let h = make(rx_buffer, tx_buffer, sockets);
        self.mem.insert(h, Mem { owner, rx, tx });
        Ok(h)
    }

    /// A new TCP socket for `owner`, without buffers until it connects or
    /// a connection arrives (`grow`); its window scale is `TCP_MAX`'s.
    fn new_tcp(&mut self, owner: u64, sockets: &mut SocketSet<'static>) -> Result<SocketHandle, i64> {
        self.add_socket(owner, 0, 0, sockets, |rx, tx, sockets| {
            let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx));
            socket.set_rx_capacity_max(TCP_MAX);
            sockets.add(socket)
        })
    }

    /// A new UDP or raw socket (`raw`) for `owner`, with buffers for its
    /// largest datagram each way.
    fn new_datagram(&mut self, owner: u64, sockets: &mut SocketSet<'static>, raw: bool) -> Result<SocketHandle, i64> {
        let size = if raw { RAW_BUFFER } else { UDP_BUFFER };
        self.add_socket(owner, size, size, sockets, |rx, tx, sockets| {
            if raw {
                let rx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; UDP_PACKETS], rx);
                let tx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; UDP_PACKETS], tx);
                sockets.add(raw::Socket::new(Some(IpVersion::Ipv4), Some(IpProtocol::Icmp), rx, tx))
            } else {
                let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], rx);
                let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], tx);
                sockets.add(udp::Socket::new(rx, tx))
            }
        })
    }

    /// A socket leaves smoltcp: its buffers' memory goes back to the
    /// system, its place and bytes to its owner's budget.
    fn drop_socket(&mut self, sockets: &mut SocketSet<'static>, h: SocketHandle) {
        sockets.remove(h);
        if let Some(m) = self.mem.remove(&h) {
            self.bytes.uncharge(m.owner, m.bytes());
            self.socks.uncharge(m.owner, 1);
        }
    }

    /// Gives TCP socket `h`'s buffer `way` twice its size (at least
    /// `TCP_MIN`, at most `TCP_MAX`), with what it holds; false if it is
    /// as large as it gets or the owner's budget is short.
    fn grow(&mut self, h: SocketHandle, way: Way, sockets: &mut SocketSet<'static>) -> bool {
        grow(&mut self.mem, &mut self.bytes, h, way, sockets)
    }

    /// Gives a TCP socket without buffers its first ones (`TCP_MIN` each
    /// way): ENOBUFS if the budget is short.
    fn equip(&mut self, h: SocketHandle, sockets: &mut SocketSet<'static>) -> Result<(), i64> {
        equip(&mut self.mem, &mut self.bytes, h, sockets)
    }

    /// Takes `n` bytes (a closed connection's leftovers) for channel `c`'s
    /// instance before anything is allocated (ENOBUFS if they are not
    /// there).
    fn charge(&mut self, c: usize, n: usize) -> Result<(), i64> {
        let owner = self.chan(c).owner;
        self.bytes.charge(owner, n)
    }

    /// Gives `n` bytes back to the instance `owner` (whose channels may be
    /// gone already).
    fn uncharge(&mut self, owner: u64, n: usize) {
        self.bytes.uncharge(owner, n);
    }

    /// Enters a new socket under index `sock` of channel `c`: its control
    /// block starts over.
    fn enter(&mut self, c: usize, sock: u32, s: Sock) {
        let chan = self.chan(c);
        let ctl = chan.area.ctl(sock as usize).expect("decode checked the index");
        ctl.reset_netd(s.state);
        ctl.changed(&WakeAll);
        chan.socks.insert(sock, s);
    }

    fn socket(&mut self, c: usize, sock: u32, kind: Kind, area: Option<Area>, sockets: &mut SocketSet<'static>) -> Reply {
        if self.chan(c).socks.contains_key(&sock) {
            return Err(EBUSY);
        }
        if let Some(a) = &area {
            self.check_area(c, a)?;
        }
        let owner = self.chan(c).owner;
        let handle = match kind {
            Kind::Tcp => self.new_tcp(owner, sockets),
            Kind::Udp => self.new_datagram(owner, sockets, false),
            Kind::RawIcmp => self.new_datagram(owner, sockets, true),
        }?;
        let s = match kind {
            Kind::Tcp => {
                let t = Tcp {
                    handle,
                    local: None,
                    reuse: false,
                    backlog: None,
                    connecting: None,
                    established: false,
                    failed: false,
                    fin_pending: false,
                    fin_sent: false,
                    last: tcp::State::Closed,
                    opts: Opts::default(),
                    quiet: None,
                };
                Sock::new(Proto::Tcp(t), None, 0)
            }
            Kind::Udp => Sock::new(Proto::Udp { handle, peer: None, reuse: false }, area, state::SEND_OPEN | state::WRITABLE),
            Kind::RawIcmp => Sock::new(Proto::Raw { handle, peer: None, ttl: DEFAULT_TTL, ids: Vec::new() }, area, state::SEND_OPEN | state::WRITABLE),
        };
        self.enter(c, sock, s);
        done(0)
    }

    fn sock(&mut self, c: usize, sock: u32) -> Result<&mut Sock, i64> {
        self.chan(c).socks.get_mut(&sock).ok_or(EBADF)
    }

    /// Whether `addr` (None: any) is an address of this host.
    fn local_address(&self, addr: Option<IpAddress>) -> bool {
        match addr.map(bits) {
            None => true,
            Some(a) => a >> 24 == 127 || (a != 0 && a == self.config.address),
        }
    }

    /// The sockets that hold a port (TCP or UDP ones), of every channel and
    /// the closing ones; `except` (channel, socket) is left out.
    fn holders(&self, tcp: bool, sockets: &SocketSet<'static>, except: Option<(usize, u32)>) -> Vec<PortHolder> {
        let mut out = Vec::new();
        for (c, chan) in self.chans.iter().enumerate() {
            let Some(chan) = chan else { continue };
            for (&i, s) in &chan.socks {
                if Some((c, i)) == except {
                    continue;
                }
                let held = match &s.proto {
                    Proto::Tcp(t) if tcp => t.local.map(|l| (l, t.reuse, t.backlog.is_some(), t.established || t.connecting.is_some())),
                    Proto::Udp { handle, reuse, .. } if !tcp => {
                        let l = sockets.get::<udp::Socket>(*handle).endpoint();
                        (l.port != 0).then_some((l, *reuse, false, false))
                    }
                    _ => None,
                };
                if let Some((local, reuse, listening, connected)) = held {
                    let addr = local.addr.map(bits);
                    out.push(PortHolder { owner: chan.owner, port: local.port, addr, reuse, listening, connected, closing: false });
                }
            }
        }
        if tcp {
            for cl in &self.closing {
                if let Some(local) = cl.local {
                    let addr = local.addr.map(bits);
                    out.push(PortHolder { owner: cl.owner, port: local.port, addr, reuse: cl.reuse, listening: false, connected: false, closing: true });
                }
            }
            out.extend(self.lingering.iter().map(|l| l.holder));
        }
        out
    }

    /// Whether a TCP connection from `local` to `remote` exists: live (of
    /// any channel), closing, or in TIME-WAIT.
    fn tuple_in_use(&self, local: IpEndpoint, remote: IpEndpoint, sockets: &SocketSet<'static>) -> bool {
        let same = |h: SocketHandle| {
            let s = sockets.get::<tcp::Socket>(h);
            s.local_endpoint() == Some(local) && s.remote_endpoint() == Some(remote)
        };
        let live = self.chans.iter().flatten().flat_map(|c| c.socks.values()).any(|s| matches!(&s.proto, Proto::Tcp(t) if t.backlog.is_none() && same(t.handle)));
        let listening = self.chans.iter().flatten().flat_map(|c| c.socks.values()).any(|s| matches!(&s.proto, Proto::Tcp(Tcp { backlog: Some(set), .. }) if set.iter().any(|&h| same(h))));
        live || listening || self.closing.iter().any(|cl| same(cl.handle)) || self.lingering.iter().any(|l| l.tuple == Some((local, remote)))
    }

    /// Whether socket `sock` of channel `c` may claim `port` on `addr`
    /// (None: any) with `reuse` (netring's rules: Linux's, and never a port
    /// another instance serves).
    fn conflict(&self, tcp: bool, c: usize, sock: u32, port: u16, addr: Option<IpAddress>, reuse: bool, sockets: &SocketSet<'static>) -> bool {
        let owner = self.chans[c].as_ref().expect("in use").owner;
        let claim = PortClaim { owner, port, addr: addr.map(bits), reuse };
        let rule = if tcp { tcp_port_conflict } else { udp_port_conflict };
        self.holders(tcp, sockets, Some((c, sock))).iter().any(|h| rule(&claim, h))
    }

    /// A free ephemeral port: held by no TCP socket, or bound by no UDP one.
    fn ephemeral(&mut self, tcp: bool, sockets: &SocketSet<'static>) -> Result<u16, i64> {
        let held: alloc::collections::BTreeSet<u16> = self.holders(tcp, sockets, None).iter().map(|h| h.port).collect();
        for _ in 0..=(u16::MAX - FIRST_EPHEMERAL) {
            let port = self.next_port;
            self.next_port = if port == u16::MAX { FIRST_EPHEMERAL } else { port + 1 };
            if !held.contains(&port) {
                return Ok(port);
            }
        }
        Err(EADDRINUSE)
    }

    fn bind(&mut self, c: usize, sock: u32, at: Endpoint, reuse: bool, sockets: &mut SocketSet<'static>) -> Reply {
        let addr = (at.addr != 0).then(|| ipv4(at.addr));
        if !self.local_address(addr) {
            return Err(EADDRNOTAVAIL);
        }
        let is_tcp = match &self.sock(c, sock)?.proto {
            Proto::Tcp(t) if t.local.is_some() => return Err(EINVAL),
            Proto::Tcp(_) => true,
            Proto::Udp { handle, .. } if sockets.get::<udp::Socket>(*handle).is_open() => return Err(EINVAL),
            Proto::Udp { .. } => false,
            // A raw socket's address only selects the source; ours is fixed.
            Proto::Raw { .. } => return done(0),
        };
        let port = match at.port {
            0 => self.ephemeral(is_tcp, sockets)?,
            p if self.conflict(is_tcp, c, sock, p, addr, reuse, sockets) => return Err(EADDRINUSE),
            p => p,
        };
        let local = IpListenEndpoint { addr, port };
        match &mut self.sock(c, sock)?.proto {
            Proto::Tcp(t) => {
                t.local = Some(local);
                t.reuse = reuse;
            }
            Proto::Udp { handle, reuse: r, .. } => {
                sockets.get_mut::<udp::Socket>(*handle).bind(local).map_err(|_| EINVAL)?;
                *r = reuse;
            }
            Proto::Raw { .. } => {}
        }
        Ok((0, [port as u64, 0, 0, 0]))
    }

    fn listen(&mut self, c: usize, sock: u32, backlog: u32, reuse: bool, sockets: &mut SocketSet<'static>) -> Reply {
        let bound = match &mut self.sock(c, sock)?.proto {
            Proto::Tcp(t) if t.backlog.is_some() => return done(0),
            Proto::Tcp(t) if t.established || t.connecting.is_some() => return Err(EINVAL),
            Proto::Tcp(t) => {
                // SO_REUSEADDR as it is now, not as it was at bind
                // (Linux reads it at listen too).
                t.reuse = reuse;
                t.local.map(|l| (l, reuse))
            }
            _ => return Err(EOPNOTSUPP),
        };
        // The port is claimed again, as a listener's (Linux checks at
        // listen too): two sockets that shared it by SO_REUSEADDR cannot
        // both listen, and one that another instance came to listen on
        // since the bind is not taken over.
        if let Some((l, reuse)) = bound {
            if self.conflict(true, c, sock, l.port, l.addr, reuse, sockets) {
                return Err(EADDRINUSE);
            }
        }
        let port = if bound.is_none() { Some(self.ephemeral(true, sockets)?) } else { None };
        // The backlog's sockets beside the socket's own, as many as the
        // budget allows (a smaller backlog if it is short). They cost only
        // their place until a connection arrives.
        let want = (backlog as usize).clamp(1, MAX_BACKLOG);
        let owner = self.chan(c).owner;
        let mut set = Vec::with_capacity(want);
        let Proto::Tcp(t) = &mut self.sock(c, sock)?.proto else { unreachable!("checked above") };
        set.push(t.handle);
        while set.len() < want {
            match self.new_tcp(owner, sockets) {
                Ok(h) => set.push(h),
                Err(_) => break,
            }
        }
        let Proto::Tcp(t) = &mut self.sock(c, sock)?.proto else { unreachable!("checked above") };
        if let Some(port) = port {
            t.local = Some(IpListenEndpoint { addr: None, port });
        }
        let local = t.local.expect("bound");
        // A socket that never connected listens (no error is possible).
        for &h in &set {
            let s = sockets.get_mut::<tcp::Socket>(h);
            t.opts.apply(s);
            s.set_timeout(Some(SYN_RECEIVED_TIMEOUT));
            let _ = s.listen(local);
        }
        t.backlog = Some(set);
        t.failed = false;
        self.sock(c, sock)?.state = state::LISTENING;
        self.show(c, sock);
        done(0)
    }

    /// Publishes socket `sock`'s state now (a request changed it: the
    /// client sees it before the completion).
    fn show(&mut self, c: usize, sock: u32) {
        let chan = self.chan(c);
        let ctl = chan.area.ctl(sock as usize).expect("an index below MAX_SOCKETS");
        if let Some(s) = chan.socks.get_mut(&sock) {
            if publish(s, ctl) {
                chan.area.header.mark_client(sock as usize);
                chan.marked = true;
            }
        }
    }

    fn connect(&mut self, c: usize, sock: u32, to: Endpoint, area: Option<Area>, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Reply {
        let s = self.sock(c, sock)?;
        let has_area = s.area.is_some();
        match &s.proto {
            Proto::Tcp(t) => {
                if t.backlog.is_some() || t.established {
                    return Err(EISCONN);
                }
                if t.connecting.is_some() {
                    return Err(EALREADY);
                }
                let (bound, handle) = (t.local, t.handle);
                match (has_area, &area) {
                    (false, None) | (true, Some(_)) => return Err(EINVAL),
                    (false, Some(a)) => self.check_area(c, a)?,
                    (true, None) => {}
                }
                if to.port == 0 {
                    return Err(ECONNREFUSED);
                }
                let remote = IpEndpoint::new(destination(to.addr), to.port);
                let local = match bound {
                    Some(l) => {
                        // A bound socket (SO_REUSEADDR lets several share
                        // the port) never takes the 4-tuple of another
                        // connection, live, closing or in TIME-WAIT
                        // (Linux's __inet_check_established).
                        let addr = l.addr.or_else(|| match remote.addr {
                            IpAddress::Ipv4(dst) => iface.get_source_address_ipv4(&dst).map(IpAddress::Ipv4),
                        });
                        if addr.is_some_and(|a| self.tuple_in_use(IpEndpoint::new(a, l.port), remote, sockets)) {
                            return Err(EADDRNOTAVAIL);
                        }
                        l
                    }
                    // (An ephemeral port is held by no TCP socket at all.)
                    None => IpListenEndpoint { addr: None, port: self.ephemeral(true, sockets)? },
                };
                // Its buffers come now (the SYN announces the window).
                self.equip(handle, sockets)?;
                let s = self.sock(c, sock)?;
                let Proto::Tcp(t) = &mut s.proto else { unreachable!("checked above") };
                let socket = sockets.get_mut::<tcp::Socket>(handle);
                socket.set_timeout(Some(CONNECT_TIMEOUT));
                socket.connect(iface.context(), remote, local).map_err(|e| match e {
                    tcp::ConnectError::InvalidState => EISCONN,
                    tcp::ConnectError::Unaddressable => ENETUNREACH,
                })?;
                t.local = Some(local);
                t.connecting = Some(crate::now());
                t.failed = false;
                if area.is_some() {
                    s.area = area;
                }
                // Shown before the completion: the client's connect waits
                // for CONNECTING to end.
                s.state = state::CONNECTING;
                self.show(c, sock);
                done(0)
            }
            Proto::Udp { handle, .. } => {
                let handle = *handle;
                if area.is_some() {
                    return Err(EINVAL);
                }
                // Endpoint 0: AF_UNSPEC, no peer.
                let peer = (to != Endpoint::default()).then(|| IpEndpoint::new(destination(to.addr), to.port));
                if peer.is_some_and(|p| p.port == 0) {
                    return Err(EINVAL);
                }
                if peer.is_some() && !sockets.get::<udp::Socket>(handle).is_open() {
                    let port = self.ephemeral(false, sockets)?;
                    sockets.get_mut::<udp::Socket>(handle).bind(port).map_err(|_| EADDRINUSE)?;
                }
                if let Proto::Udp { peer: p, .. } = &mut self.sock(c, sock)?.proto {
                    *p = peer;
                }
                done(0)
            }
            Proto::Raw { .. } => {
                if area.is_some() {
                    return Err(EINVAL);
                }
                let peer = (to.addr != 0).then(|| Ipv4Address::from_bits(to.addr));
                if let Proto::Raw { peer: p, .. } = &mut self.sock(c, sock)?.proto {
                    *p = peer;
                }
                done(0)
            }
        }
    }

    fn accept(&mut self, c: usize, sock: u32, new: u32, area: Area, sockets: &mut SocketSet<'static>) -> Reply {
        if self.chan(c).socks.contains_key(&new) {
            return Err(EBUSY);
        }
        let (local, reuse, ready) = match &self.sock(c, sock)?.proto {
            Proto::Tcp(t) => {
                let Some(set) = &t.backlog else { return Err(EINVAL) };
                let ready = set.iter().position(|&h| ready_to_accept(sockets.get::<tcp::Socket>(h).state()));
                (t.local.expect("a listener is bound"), t.reuse, ready)
            }
            _ => return Err(EOPNOTSUPP),
        };
        let Some(i) = ready else { return Err(EAGAIN) };
        self.check_area(c, &area)?;
        let conn = {
            let Proto::Tcp(t) = &self.sock(c, sock)?.proto else { unreachable!("checked above") };
            t.backlog.as_ref().expect("listening")[i]
        };
        // (The pump gives an arriving connection its buffers.)
        self.equip(conn, sockets)?;
        // A fresh listening socket takes the connection's place, if the
        // budget allows; else the backlog shrinks (never below one).
        let owner = self.chan(c).owner;
        let fresh = self.new_tcp(owner, sockets).ok();
        let s = self.sock(c, sock)?;
        let Proto::Tcp(t) = &mut s.proto else { unreachable!("checked above") };
        let opts = t.opts;
        let set = t.backlog.as_mut().expect("listening");
        if fresh.is_none() && set.len() == 1 {
            return Err(ENOBUFS);
        }
        match fresh {
            Some(h) => {
                set[i] = h;
                let socket = sockets.get_mut::<tcp::Socket>(h);
                opts.apply(socket);
                socket.set_timeout(Some(SYN_RECEIVED_TIMEOUT));
                let _ = socket.listen(local);
            }
            None => {
                set.remove(i);
            }
        }
        t.handle = set[0];
        let socket = sockets.get_mut::<tcp::Socket>(conn);
        socket.set_timeout(Some(TCP_TIMEOUT));
        let peer = socket.remote_endpoint().unwrap_or(IpEndpoint::new(ipv4(0), 0));
        let t = Tcp {
            handle: conn,
            local: Some(local),
            reuse,
            backlog: None,
            connecting: None,
            established: true,
            failed: false,
            fin_pending: false,
            fin_sent: false,
            last: socket.state(),
            // The listener's options, as on Linux.
            opts,
            quiet: None,
        };
        self.enter(c, new, Sock::new(Proto::Tcp(t), Some(area), state::ESTABLISHED | state::SEND_OPEN));
        Ok((0, [bits(peer.addr) as u64, peer.port as u64, 0, 0]))
    }

    fn send(&mut self, c: usize, sock: u32, to: Endpoint, len: u32, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Reply {
        let (proto, rings) = {
            let chan = self.chan(c);
            let s = chan.socks.get(&sock).ok_or(EBADF)?;
            let rings = Self::rings(&chan.grants, s.area).ok_or(EINVAL)?;
            let proto = match &s.proto {
                Proto::Tcp(_) => return Err(EOPNOTSUPP),
                Proto::Udp { handle, peer, .. } => (true, *handle, peer.map(|p| (bits(p.addr), p.port)), 0),
                Proto::Raw { handle, peer, ttl, .. } => (false, *handle, peer.map(|p| (p.to_bits(), 0)), *ttl),
            };
            (proto, rings)
        };
        if len > rings.size {
            return Err(EMSGSIZE);
        }
        let len = len as usize;
        let (udp, handle, peer, ttl) = proto;
        if udp {
            if len > MAX_UDP {
                return Err(EMSGSIZE);
            }
            let dest = match (to != Endpoint::default(), peer) {
                (true, _) => IpEndpoint::new(destination(to.addr), to.port),
                (false, Some((a, p))) => IpEndpoint::new(ipv4(a), p),
                (false, None) => return Err(EDESTADDRREQ),
            };
            if dest.port == 0 {
                return Err(EINVAL);
            }
            let IpAddress::Ipv4(dst) = dest.addr;
            if iface.get_source_address_ipv4(&dst).is_none() {
                return Err(ENETUNREACH);
            }
            if !sockets.get::<udp::Socket>(handle).is_open() {
                let port = self.ephemeral(false, sockets)?;
                sockets.get_mut::<udp::Socket>(handle).bind(port).map_err(|_| EADDRINUSE)?;
            }
            let socket = sockets.get_mut::<udp::Socket>(handle);
            if len > socket.payload_send_capacity() {
                return Err(EMSGSIZE);
            }
            // Copied out first: a send that faults half-way sends nothing.
            if !Rings::read(rings.tx, rings.size, 0, &mut self.scratch[..len]) {
                return Err(EFAULT);
            }
            match socket.send_slice(&self.scratch[..len], dest) {
                Ok(()) => done(len as i64),
                Err(udp::SendError::BufferFull) => Err(EAGAIN),
                Err(udp::SendError::Unaddressable) => Err(ENETUNREACH),
            }
        } else {
            let dst = match (to.addr, peer) {
                (0, Some((a, _))) => Ipv4Address::from_bits(a),
                (0, None) => return Err(EDESTADDRREQ),
                (a, _) => Ipv4Address::from_bits(a),
            };
            if len + IPV4_HEADER > RAW_BUFFER {
                return Err(EMSGSIZE);
            }
            let src = iface.get_source_address_ipv4(&dst).ok_or(ENETUNREACH)?;
            // The application writes the ICMP message; the IP header is ours.
            let packet = &mut self.scratch[..IPV4_HEADER + len];
            if !Rings::read(rings.tx, rings.size, 0, &mut packet[IPV4_HEADER..]) {
                return Err(EFAULT);
            }
            let repr = Ipv4Repr { src_addr: src, dst_addr: dst, next_header: IpProtocol::Icmp, payload_len: len, hop_limit: ttl };
            repr.emit(&mut Ipv4Packet::new_unchecked(&mut packet[..IPV4_HEADER]), &ChecksumCapabilities::default());
            let socket = sockets.get_mut::<raw::Socket>(handle);
            if !socket.can_send() {
                return Err(EAGAIN);
            }
            socket.send_slice(packet).map_err(|_| EAGAIN)?;
            // An echo request: its replies are this instance's.
            let message = &self.scratch[IPV4_HEADER..IPV4_HEADER + len];
            if len >= 8 && message[0] == 8 {
                let id = u16::from_be_bytes([message[4], message[5]]);
                if let Proto::Raw { ids, .. } = &mut self.sock(c, sock)?.proto {
                    if !ids.contains(&id) {
                        if ids.len() == ECHO_IDS {
                            ids.remove(0);
                        }
                        ids.push(id);
                    }
                }
            }
            done(len as i64)
        }
    }

    fn shutdown(&mut self, c: usize, sock: u32, how: u64) -> Reply {
        if let Proto::Tcp(t) = &mut self.sock(c, sock)?.proto {
            if !t.established && t.connecting.is_none() {
                return Err(ENOTCONN);
            }
            if how & netring::SHUT_WR != 0 {
                // After what the send ring holds (`pump`).
                t.fin_pending = true;
            }
        }
        done(0)
    }

    fn close(&mut self, c: usize, sock: u32, abort: bool, sockets: &mut SocketSet<'static>) -> Reply {
        let chan = self.chan(c);
        let owner = chan.owner;
        let s = chan.socks.remove(&sock).ok_or(EBADF)?;
        // What the send ring still holds goes before the FIN, in netd's
        // memory: charged first (the client chooses its rings' size), else
        // the connection is reset rather than netd run out of memory.
        let mut abort = abort;
        let mut pending = None;
        if let (Proto::Tcp(t), Some(r)) = (&s.proto, Self::rings(&chan.grants, s.area)) {
            let ctl = chan.area.ctl(sock as usize).expect("decode checked the index");
            let tail = ctl.client.tx_tail.load(SeqCst);
            if !abort && t.established && !t.fin_sent {
                match fill(s.tx_head, tail, r.size) {
                    Some(0) => {}
                    Some(n) => pending = Some((r, n as usize)),
                    // Positions the client should never have published.
                    None => abort = true,
                }
            }
        }
        let mut leftover = None;
        if let Some((r, n)) = pending.filter(|_| !abort) {
            if self.charge(c, mapped(n)).is_err() {
                abort = true;
            } else {
                let copied = Region::new(n).ok().and_then(|mut region| Rings::read(r.tx, r.size, s.tx_head, region.bytes_mut()).then_some(region));
                match copied {
                    Some(region) => leftover = Some(region),
                    // Memory is short, or the grant went: a reset.
                    None => {
                        self.uncharge(owner, mapped(n));
                        abort = true;
                    }
                }
            }
        }
        self.release(owner, s, abort, leftover, sockets);
        done(0)
    }

    /// A socket leaves its channel: datagram sockets go, a listener resets
    /// the connections nobody accepted, a TCP connection finishes as an
    /// orphan (with `leftover`, charged to `owner`'s bytes, before its FIN),
    /// or is reset: with `abort`, when data it received is unread, or when
    /// there are too many orphans (beyond their limit or the instance's
    /// share, as Linux resets past tcp_max_orphans).
    fn release(&mut self, owner: u64, s: Sock, abort: bool, mut leftover: Option<Region>, sockets: &mut SocketSet<'static>) {
        match s.proto {
            Proto::Tcp(t) => {
                let listener = t.backlog.is_some();
                let handles = t.backlog.unwrap_or_else(|| vec![t.handle]);
                let now = crate::now();
                for h in handles {
                    let socket = sockets.get_mut::<tcp::Socket>(h);
                    let mut rest = None;
                    let orphan = t.established && !listener && !abort && !socket.can_recv() && self.orphans.charge(owner, 1).is_ok();
                    if orphan {
                        if leftover.is_none() {
                            socket.close();
                        } else {
                            // The FIN after the leftovers (`finish_closing`).
                            rest = leftover.take();
                        }
                    } else {
                        // Never connected, a listener's unaccepted
                        // connections, unread data: a reset (a socket that
                        // only listens or never connected just stops).
                        socket.abort();
                        if let Some(r) = leftover.take() {
                            self.bytes.uncharge(owner, r.mapped);
                        }
                    }
                    let (local, reuse) = if listener { (None, true) } else { (t.local, t.reuse) };
                    let mark = (socket.state(), socket.send_queue());
                    self.closing.push(Closing { handle: h, leftover: rest, sent: 0, local, reuse, owner, orphan, mark, since: now });
                }
            }
            Proto::Udp { handle, .. } | Proto::Raw { handle, .. } => {
                self.drop_socket(sockets, handle);
            }
        }
    }

    fn name(&mut self, c: usize, sock: u32, peer: bool, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Reply {
        let ep = match &self.sock(c, sock)?.proto {
            Proto::Tcp(t) => {
                let s = sockets.get::<tcp::Socket>(t.handle);
                if peer {
                    if !t.established || t.backlog.is_some() {
                        return Err(ENOTCONN);
                    }
                    s.remote_endpoint().ok_or(ENOTCONN)?
                } else {
                    let bound = t.local.map(|l| IpEndpoint::new(l.addr.unwrap_or(ipv4(0)), l.port));
                    let actual = if t.backlog.is_none() { s.local_endpoint() } else { None };
                    actual.or(bound).unwrap_or(IpEndpoint::new(ipv4(0), 0))
                }
            }
            Proto::Udp { handle, peer: p, .. } => {
                if peer {
                    p.ok_or(ENOTCONN)?
                } else {
                    let l = sockets.get::<udp::Socket>(*handle).endpoint();
                    // A connected socket's address is the source its peer
                    // sees.
                    let addr = l.addr.or_else(|| {
                        let IpAddress::Ipv4(dst) = p.as_ref()?.addr;
                        iface.get_source_address_ipv4(&dst).map(IpAddress::Ipv4)
                    });
                    IpEndpoint::new(addr.unwrap_or(ipv4(0)), l.port)
                }
            }
            Proto::Raw { peer: p, .. } => match (peer, p) {
                (true, Some(p)) => IpEndpoint::new(IpAddress::Ipv4(*p), 0),
                (true, None) => return Err(ENOTCONN),
                (false, _) => IpEndpoint::new(ipv4(0), 0),
            },
        };
        Ok((0, [bits(ep.addr) as u64, ep.port as u64, 0, 0]))
    }

    fn setopt(&mut self, c: usize, sock: u32, option: u32, value: u64, sockets: &mut SocketSet<'static>) -> Reply {
        let ttl = u8::try_from(value).ok().filter(|&t| t > 0);
        match &mut self.sock(c, sock)?.proto {
            Proto::Tcp(t) => {
                // (Values were checked by `Request::decode`.)
                match option {
                    opt::NODELAY => t.opts.nodelay = value != 0,
                    opt::KEEPALIVE => t.opts.keepalive = (value != 0).then(|| Duration::from_millis(value)),
                    opt::TTL => t.opts.ttl = Some(ttl.ok_or(EINVAL)?),
                    _ => return Err(ENOPROTOOPT),
                }
                match &t.backlog {
                    Some(set) => set.iter().for_each(|&h| t.opts.apply(sockets.get_mut::<tcp::Socket>(h))),
                    None => t.opts.apply(sockets.get_mut::<tcp::Socket>(t.handle)),
                }
            }
            Proto::Udp { handle, .. } => match option {
                opt::TTL => sockets.get_mut::<udp::Socket>(*handle).set_hop_limit(Some(ttl.ok_or(EINVAL)?)),
                _ => return Err(ENOPROTOOPT),
            },
            Proto::Raw { .. } => match option {
                opt::TTL => {
                    let t = ttl.ok_or(EINVAL)?;
                    if let Proto::Raw { ttl, .. } = &mut self.sock(c, sock)?.proto {
                        *ttl = t;
                    }
                }
                _ => return Err(ENOPROTOOPT),
            },
        }
        done(0)
    }

    fn links(&mut self, c: usize, buf: Buf) -> Reply {
        let g = self.grant(c, buf.grant)?;
        if !g.writable || buf.offset as u64 + buf.len as u64 > g.bytes {
            return Err(EINVAL);
        }
        // The loopback (frames to the host's own addresses come back in
        // `Nic`) and the card, both with Ethernet framing: the card's MTU
        // applies to both.
        let mtu = (crate::virtio_net::MTU - ETHERNET_HEADER) as u32;
        let cfg = self.config;
        let up = netring::LINK_UP | netring::LINK_RUNNING;
        let (address, prefix) = (crate::LOOPBACK.address().to_bits(), crate::LOOPBACK.prefix_len());
        let lo = Link { index: 1, kind: netring::LINK_LOOPBACK, state: up, mtu, mac: [0; 6], prefix, address };
        let card = Link { index: 2, kind: netring::LINK_ETHERNET, state: up, mtu, mac: cfg.mac, prefix: cfg.prefix, address: cfg.address };
        let mut out = Vec::with_capacity(2 * Link::SIZE);
        out.extend_from_slice(&lo.encode());
        out.extend_from_slice(&card.encode());
        let n = out.len().min(buf.len as usize);
        if !unsafe { oxrt::copy::copy(g.addr.add(buf.offset as usize), out.as_ptr(), n) } {
            return Err(EFAULT);
        }
        done(n as i64)
    }

    // ------------------------------------------------------------ the pump

    /// Moves data between smoltcp and the rings, finishes closing sockets,
    /// and publishes what changed; true if anything moved or changed.
    pub fn pump(&mut self, sockets: &mut SocketSet<'static>) -> bool {
        let mut progress = self.finish_closing(sockets);
        let mut wants = core::mem::take(&mut self.wants);
        let mut round = Round { now: crate::now(), trimming: pressure(&self.bytes), owner: 0, icmp: self.icmp_owners(sockets) };
        for chan in self.chans.iter_mut().flatten() {
            round.owner = chan.owner;
            for (&i, s) in chan.socks.iter_mut() {
                let ctl = chan.area.ctl(i as usize).expect("an index below MAX_SOCKETS");
                let rings = Self::rings(&chan.grants, s.area);
                match pump_one(s, ctl, rings, sockets, &round, &mut wants) {
                    Ok(moved) => progress |= moved,
                    Err(()) => {
                        // The client broke the protocol or took the grant
                        // away: the socket is reset and reports EIO.
                        if let Proto::Tcp(t) = &mut s.proto {
                            sockets.get_mut::<tcp::Socket>(t.handle).abort();
                            t.established = true;
                            t.last = tcp::State::Closed;
                        }
                        s.area = None;
                        post_error(&mut s.err_seq, ctl, EIO);
                        s.state = (s.state | state::CLOSED | state::RECV_EOF) & !(state::SEND_OPEN | state::WRITABLE);
                        progress = true;
                    }
                }
                if publish(s, ctl) {
                    chan.area.header.mark_client(i as usize);
                    chan.marked = true;
                    progress = true;
                }
            }
        }
        // Larger buffers for the connections they limit.
        for (h, want) in wants.drain(..) {
            progress |= match want {
                Want::Grow(way) => self.grow(h, way, sockets),
                Want::Trim => trim(&mut self.mem, &mut self.bytes, h, sockets),
            };
        }
        self.wants = wants;
        progress
    }

    /// Whose ICMP messages are whose, if a raw socket has any to take
    /// (rare: it is worked out only then).
    fn icmp_owners(&self, sockets: &SocketSet<'static>) -> Option<IcmpOwners> {
        let raw = || self.chans.iter().flatten().flat_map(|c| c.socks.values().map(move |s| (c.owner, s)));
        let pending = raw().any(|(_, s)| matches!(&s.proto, Proto::Raw { handle, .. } if sockets.get::<raw::Socket>(*handle).can_recv()));
        if !pending {
            return None;
        }
        let mut o = IcmpOwners::default();
        for (owner, s) in raw() {
            if let Proto::Raw { ids, .. } = &s.proto {
                o.echo.extend(ids.iter().map(|&id| (id, owner)));
            }
        }
        o.tcp.extend(self.holders(true, sockets, None).iter().map(|h| (h.port, h.owner)));
        o.udp.extend(self.holders(false, sockets, None).iter().map(|h| (h.port, h.owner)));
        Some(o)
    }

    /// Gives the connections that arrived at listeners since smoltcp last
    /// sent their first buffers, so that their SYN-ACK offers a window
    /// (called between smoltcp's taking frames and its sending). A
    /// connection the budget has no room for is refused with a reset (its
    /// socket listens again once the reset went).
    pub fn arrivals(&mut self, sockets: &mut SocketSet<'static>) {
        let Service { chans, mem, bytes, .. } = self;
        for chan in chans.iter().flatten() {
            for s in chan.socks.values() {
                let Proto::Tcp(Tcp { backlog: Some(set), .. }) = &s.proto else { continue };
                for &h in set {
                    let sk = sockets.get::<tcp::Socket>(h);
                    if matches!(sk.state(), tcp::State::Listen | tcp::State::Closed) || sk.recv_capacity() > 0 && sk.send_capacity() > 0 {
                        continue;
                    }
                    if equip(mem, bytes, h, sockets).is_err() {
                        sockets.get_mut::<tcp::Socket>(h).abort();
                    }
                }
            }
        }
    }

    /// Sends what closed connections still had, then their FIN; drops the
    /// ones smoltcp is done with; resets orphans that make no progress.
    fn finish_closing(&mut self, sockets: &mut SocketSet<'static>) -> bool {
        let mut progress = false;
        let mut done = Vec::new();
        let now = crate::now();
        let Service { lingering, lingering_records: records, closing, mem, bytes, orphans, .. } = self;
        lingering.retain(|l| {
            if now < l.until {
                return true;
            }
            records.uncharge(l.holder.owner, 1);
            false
        });
        closing.retain_mut(|cl| {
            let socket = sockets.get_mut::<tcp::Socket>(cl.handle);
            let st = socket.state();
            if st == tcp::State::TimeWait || st == tcp::State::Closed {
                if st == tcp::State::TimeWait {
                    // Both FINs went: what is left is the port, for
                    // TIME-WAIT (Linux keeps it as a small record too),
                    // within the instance's share of records (beyond it the
                    // port is free at once). The buffers go now; a segment
                    // of the old connection that still comes is answered
                    // with a reset instead of an ACK.
                    if let Some(local) = cl.local.filter(|_| records.charge(cl.owner, 1).is_ok()) {
                        let holder = PortHolder { owner: cl.owner, port: local.port, addr: local.addr.map(bits), reuse: cl.reuse, listening: false, connected: false, closing: true };
                        let tuple = socket.local_endpoint().zip(socket.remote_endpoint());
                        lingering.push(Lingering { holder, tuple, until: now + TIME_WAIT });
                    }
                }
                // (A reset made below goes out with the next poll, before
                // the socket goes: Closed is looked at first.)
                cl.drop_leftover(bytes);
                if cl.orphan {
                    orphans.uncharge(cl.owner, 1);
                }
                done.push(cl.handle);
                progress = true;
                return false;
            }
            if socket.can_recv() {
                // Data for a connection nobody can read any more: a reset,
                // as Linux answers it (the writer learns it with EPIPE).
                socket.abort();
                cl.drop_leftover(bytes);
                return true;
            }
            if let Some(left) = &cl.leftover {
                let data = &left.bytes()[cl.sent..];
                match socket.send_slice(data) {
                    Ok(n) => {
                        cl.sent += n;
                        progress |= n > 0;
                        // More than the send buffer takes: it grows, as for
                        // a socket that is open.
                        if n < data.len() && grow(mem, bytes, cl.handle, Way::Tx, sockets) {
                            progress = true;
                        }
                    }
                    // The connection went: what is left is lost, as on Linux.
                    Err(_) => cl.sent = left.bytes().len(),
                }
                if cl.leftover.as_ref().is_some_and(|l| cl.sent == l.bytes().len()) {
                    // Its memory goes back at once.
                    cl.drop_leftover(bytes);
                    sockets.get_mut::<tcp::Socket>(cl.handle).close();
                }
            }
            // An orphan that makes no progress is reset: one in FIN-WAIT-2
            // after FIN_TIMEOUT, one whose leftovers or FIN the peer does
            // not take after ORPHAN_TIMEOUT.
            let socket = sockets.get_mut::<tcp::Socket>(cl.handle);
            let left = cl.leftover.as_ref().map_or(0, |l| l.bytes().len() - cl.sent);
            let mark = (socket.state(), socket.send_queue() + left);
            if mark != cl.mark {
                cl.mark = mark;
                cl.since = now;
            }
            if cl.deadline().is_some_and(|d| now >= d) {
                socket.abort();
                cl.drop_leftover(bytes);
                progress = true;
            }
            true
        });
        for h in done {
            self.drop_socket(sockets, h);
        }
        progress
    }

    /// When netd must look at its sockets again although nothing happens
    /// (an orphan's or a TIME-WAIT record's time is up).
    pub fn deadline(&self) -> Option<Instant> {
        let orphans = self.closing.iter().filter_map(Closing::deadline);
        let records = self.lingering.iter().map(|l| l.until);
        orphans.chain(records).min()
    }
}

/// Moves one socket's data and computes its state; Err if its client broke
/// the protocol or took its grant away. A TCP buffer that limits the
/// transfer goes to `wants` (to grow), as does a connection idle since
/// `TRIM_AFTER` while `trimming` (memory is under pressure).
fn pump_one(s: &mut Sock, ctl: &Ctl, rings: Option<Rings>, sockets: &mut SocketSet<'static>, ctx: &Round, wants: &mut Vec<(SocketHandle, Want)>) -> Result<bool, ()> {
    let Round { now, trimming, owner, ref icmp } = *ctx;
    let icmp = icmp.as_ref();
    let Sock { proto, rx_tail, tx_head, state: st_bits, backlog, err_seq, rx_wait, .. } = s;
    match proto {
        Proto::Tcp(t) => {
            if let Some(set) = &t.backlog {
                let local = t.local.expect("a listener is bound");
                let mut ready = 0;
                for &h in set {
                    let sk = sockets.get_mut::<tcp::Socket>(h);
                    match sk.state() {
                        // A connection that went before it was accepted
                        // makes room for the next.
                        tcp::State::Closed => {
                            let _ = sk.listen(local);
                        }
                        // (A connection that arrived got its buffers before
                        // its SYN-ACK went: `Service::arrivals`.)
                        st if ready_to_accept(st) => ready += 1,
                        _ => {}
                    }
                }
                *st_bits = state::LISTENING;
                *backlog = ready;
                return Ok(false);
            }
            let socket = sockets.get_mut::<tcp::Socket>(t.handle);
            let mut moved = false;
            if let Some(r) = rings {
                // Send: from the ring into smoltcp.
                let tail = ctl.client.tx_tail.load(SeqCst);
                let queued = fill(*tx_head, tail, r.size).ok_or(())?;
                // The network took half of smoltcp's send buffer or more
                // since the last round and the client has more than fits:
                // the buffer is what limits the transfer and grows (a peer
                // that does not take the data keeps it from growing).
                let (cap, unsent) = (socket.send_capacity(), socket.send_queue());
                if t.established && queued > 0 && (cap == 0 || unsent <= cap / 2 && queued as usize > cap - unsent) {
                    wants.push((t.handle, Want::Grow(Way::Tx)));
                }
                let mut taken = 0u32;
                while taken < queued && socket.can_send() {
                    let pos = tx_head.wrapping_add(taken);
                    let left = (queued - taken) as usize;
                    let got = socket.send(|buf| {
                        let n = buf.len().min(left);
                        if Rings::read(r.tx, r.size, pos, &mut buf[..n]) { (n, Ok(n)) } else { (0, Err(())) }
                    });
                    match got {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(n)) => taken += n as u32,
                        Ok(Err(())) => return Err(()),
                    }
                }
                if taken > 0 {
                    *tx_head = tx_head.wrapping_add(taken);
                    moved = true;
                }
                if t.fin_pending && !t.fin_sent && *tx_head == tail {
                    socket.close();
                    t.fin_sent = true;
                    moved = true;
                }
                // The peer filled half of the receive buffer or more since
                // the last round: if the client keeps up (all of it fits the
                // ring now), the buffer is what limits the transfer and
                // grows.
                let held = socket.recv_queue();
                let cap = socket.recv_capacity();
                if cap > 0 && held >= cap / 2 {
                    let head = ctl.client.rx_head.load(SeqCst);
                    let room = r.size - fill(head, *rx_tail, r.size).ok_or(())?;
                    if room as usize >= held {
                        wants.push((t.handle, Want::Grow(Way::Rx)));
                    }
                }
                // Receive: from smoltcp into the ring, while there is room.
                loop {
                    if !socket.can_recv() {
                        *rx_wait = false;
                        break;
                    }
                    let head = ctl.client.rx_head.load(SeqCst);
                    let room = r.size - fill(head, *rx_tail, r.size).ok_or(())?;
                    if room == 0 {
                        if !*rx_wait {
                            // Announce the wait, then look again: the
                            // client rings after making room if it sees it.
                            *rx_wait = true;
                            ctl.netd.rx_wait.store(1, SeqCst);
                            if ctl.client.rx_head.load(SeqCst) != head {
                                continue;
                            }
                        }
                        break;
                    }
                    let pos = *rx_tail;
                    let got = socket.recv(|buf| {
                        let n = buf.len().min(room as usize);
                        if Rings::write(r.rx, r.size, pos, &buf[..n]) { (n, Ok(n)) } else { (0, Err(())) }
                    });
                    match got {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(n)) => {
                            *rx_tail = rx_tail.wrapping_add(n as u32);
                            *rx_wait = false;
                            moved = true;
                        }
                        Ok(Err(())) => return Err(()),
                    }
                }
                if !*rx_wait && ctl.netd.rx_wait.load(SeqCst) != 0 {
                    ctl.netd.rx_wait.store(0, SeqCst);
                }
                // Idle: nothing moved, nothing waits in either direction.
                let idle = !moved && taken == queued && socket.recv_queue() == 0 && socket.send_queue() == 0;
                match (idle, t.quiet) {
                    (false, _) => t.quiet = None,
                    (true, None) => t.quiet = Some(now),
                    (true, Some(since)) => {
                        if trimming && t.established && now - since >= TRIM_AFTER && (socket.send_capacity() > 0 || socket.recv_capacity() > TCP_MIN) {
                            wants.push((t.handle, Want::Trim));
                        }
                    }
                }
            }
            // The state.
            let st = socket.state();
            let mut b = 0;
            // A connection that ended by its timeout reports ETIMEDOUT, one
            // that was reset ECONNREFUSED (while connecting) or ECONNRESET.
            let timed_out = socket.ended_by_timeout();
            if t.connecting.is_some() {
                match st {
                    tcp::State::SynSent | tcp::State::SynReceived => b |= state::CONNECTING,
                    tcp::State::Closed | tcp::State::Listen => {
                        // The handshake failed.
                        t.connecting = None;
                        t.failed = true;
                        post_error(err_seq, ctl, if timed_out { ETIMEDOUT } else { ECONNREFUSED });
                    }
                    _ => {
                        t.connecting = None;
                        t.established = true;
                        // Unacknowledged data gives up as Linux's does.
                        socket.set_timeout(Some(TCP_TIMEOUT));
                    }
                }
            }
            if t.established {
                b |= state::ESTABLISHED;
                if socket.may_send() && !t.fin_pending {
                    b |= state::SEND_OPEN;
                }
                if !socket.may_recv() {
                    b |= state::RECV_EOF;
                }
                if st == tcp::State::Closed {
                    b |= state::CLOSED | state::RECV_EOF;
                    if open(t.last) && *st_bits & state::CLOSED == 0 {
                        post_error(err_seq, ctl, if timed_out { ETIMEDOUT } else { ECONNRESET });
                    }
                }
            }
            if t.failed {
                b |= state::CLOSED | state::RECV_EOF;
            }
            t.last = st;
            *st_bits = b;
            Ok(moved)
        }
        Proto::Udp { handle, peer, .. } => {
            let socket = sockets.get_mut::<udp::Socket>(*handle);
            let mut moved = false;
            if let Some(r) = rings {
                loop {
                    let (len, from) = match socket.peek() {
                        // A connected socket takes datagrams from its peer
                        // only, as on Linux.
                        Ok((_, meta)) if peer.is_some_and(|p| p != meta.endpoint) => {
                            let _ = socket.recv();
                            continue;
                        }
                        Ok((data, meta)) => (data.len() as u32, Endpoint { addr: bits(meta.endpoint.addr), port: meta.endpoint.port }),
                        Err(_) => {
                            *rx_wait = false;
                            break;
                        }
                    };
                    match place_record(rx_tail, rx_wait, ctl, &r, len, from, |pos| {
                        let (data, _) = socket.peek().expect("peeked above");
                        Rings::write(r.rx, r.size, pos, data)
                    })? {
                        Placed::Yes => {
                            let _ = socket.recv();
                            moved = true;
                        }
                        // Larger than the ring could ever hold: dropped.
                        Placed::Never => {
                            let _ = socket.recv();
                        }
                        Placed::NoRoom => break,
                    }
                }
                if !*rx_wait && ctl.netd.rx_wait.load(SeqCst) != 0 {
                    ctl.netd.rx_wait.store(0, SeqCst);
                }
            }
            *st_bits = state::SEND_OPEN | if socket.can_send() { state::WRITABLE } else { 0 };
            Ok(moved)
        }
        Proto::Raw { handle, peer, .. } => {
            let socket = sockets.get_mut::<raw::Socket>(*handle);
            let mut moved = false;
            if let Some(r) = rings {
                loop {
                    let (len, from) = match socket.peek() {
                        Ok(data) => {
                            let from = Ipv4Packet::new_checked(data).map_or(0, |p| p.src_addr().to_bits());
                            // A connected socket takes its peer's packets
                            // only (as on Linux), and an instance sees no
                            // other instance's ICMP traffic.
                            let theirs = icmp.and_then(|i| i.owner(data)).is_some_and(|o| o != owner);
                            if peer.is_some_and(|p| p.to_bits() != from) || theirs {
                                let _ = socket.recv();
                                continue;
                            }
                            // Whole IPv4 packets, header included, as on
                            // Linux.
                            (data.len() as u32, from)
                        }
                        Err(_) => {
                            *rx_wait = false;
                            break;
                        }
                    };
                    match place_record(rx_tail, rx_wait, ctl, &r, len, Endpoint { addr: from, port: 0 }, |pos| {
                        let data = socket.peek().expect("peeked above");
                        Rings::write(r.rx, r.size, pos, data)
                    })? {
                        Placed::Yes => {
                            let _ = socket.recv();
                            moved = true;
                        }
                        Placed::Never => {
                            let _ = socket.recv();
                        }
                        Placed::NoRoom => break,
                    }
                }
                if !*rx_wait && ctl.netd.rx_wait.load(SeqCst) != 0 {
                    ctl.netd.rx_wait.store(0, SeqCst);
                }
            }
            *st_bits = state::SEND_OPEN | if socket.can_send() { state::WRITABLE } else { 0 };
            Ok(moved)
        }
    }
}

/// What `pump_one` needs to know of the round.
struct Round {
    now: Instant,
    /// Memory is under pressure: idle connections give their buffers back.
    trimming: bool,
    /// The instance of the channel pumped.
    owner: u64,
    /// Whose ICMP messages are whose (made when a raw socket has some).
    icmp: Option<IcmpOwners>,
}

/// Which instance an ICMP message is for, so that none sees another's:
/// echo requests and replies by the identifier its raw sockets sent,
/// errors by the TCP or UDP port (or echo identifier) of the packet they
/// quote. Messages of no instance's (requests from other hosts, say) are
/// everyone's, as on Linux.
#[derive(Default)]
struct IcmpOwners {
    echo: BTreeMap<u16, u64>,
    tcp: BTreeMap<u16, u64>,
    udp: BTreeMap<u16, u64>,
}

impl IcmpOwners {
    /// The instance whose `packet` (an IPv4 packet carrying ICMP) is.
    fn owner(&self, packet: &[u8]) -> Option<u64> {
        let ip = Ipv4Packet::new_checked(packet).ok()?;
        let icmp = ip.payload();
        if icmp.len() < 8 {
            return None;
        }
        let echo_id = |m: &[u8]| (m.len() >= 8 && (m[0] == 0 || m[0] == 8)).then(|| u16::from_be_bytes([m[4], m[5]]));
        match icmp[0] {
            0 | 8 => self.echo.get(&echo_id(icmp)?).copied(),
            // Destination unreachable, source quench, redirect, time
            // exceeded, parameter problem: the IP header and the first 8
            // bytes of the packet that caused it follow.
            3 | 4 | 5 | 11 | 12 => {
                let quoted = &icmp[8..];
                let ihl = (*quoted.first()? as usize & 0xf) * 4;
                let l4 = quoted.get(ihl..)?;
                let port = || Some(u16::from_be_bytes([*l4.first()?, *l4.get(1)?]));
                match *quoted.get(9)? {
                    1 => self.echo.get(&echo_id(l4)?).copied(),
                    6 => self.tcp.get(&port()?).copied(),
                    17 => self.udp.get(&port()?).copied(),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

enum Placed {
    Yes,
    NoRoom,
    Never,
}

/// Puts a record of `len` bytes from `from` into the receive ring: its
/// header, then the data (`data(pos)` writes it at ring position `pos`).
/// NoRoom announces netd's wait for room (`rx_wait`) as for streams.
fn place_record(tail: &mut u32, wait: &mut bool, ctl: &Ctl, r: &Rings, len: u32, from: Endpoint, data: impl FnOnce(u32) -> bool) -> Result<Placed, ()> {
    let span = Record::span(len);
    if span > r.size as u64 {
        return Ok(Placed::Never);
    }
    let head = ctl.client.rx_head.load(SeqCst);
    let room = r.size - fill(head, *tail, r.size).ok_or(())?;
    if (room as u64) < span {
        if !*wait {
            *wait = true;
            ctl.netd.rx_wait.store(1, SeqCst);
            // Room made meanwhile: go on (the client may not ring).
            if ctl.client.rx_head.load(SeqCst) != head {
                *wait = false;
                return place_record(tail, wait, ctl, r, len, from, data);
            }
        }
        return Ok(Placed::NoRoom);
    }
    let header = Record { len, from }.encode();
    if !Rings::write(r.rx, r.size, *tail, &header) || !data(tail.wrapping_add(RECORD_HEADER)) {
        return Err(());
    }
    *tail = tail.wrapping_add(span as u32);
    *wait = false;
    Ok(Placed::Yes)
}

/// Whether netd's buffer memory is under pressure (as Linux's tcp_mem):
/// connections then start with `TCP_MIN`, grow no further than their
/// first sizes, and idle ones give their buffers back (`trim`).
fn pressure(bytes: &Budget) -> bool {
    bytes.used() >= PRESSURE
}

/// A TCP buffer's first size: Linux's (`TCP_RX_INIT`, `TCP_TX_INIT`), or
/// `TCP_MIN` under pressure.
fn first_size(bytes: &Budget, way: Way) -> usize {
    match way {
        _ if pressure(bytes) => TCP_MIN,
        Way::Rx => TCP_RX_INIT,
        Way::Tx => TCP_TX_INIT,
    }
}

fn capacity(socket: &tcp::Socket, way: Way) -> usize {
    if way == Way::Rx { socket.recv_capacity() } else { socket.send_capacity() }
}

/// `Service::grow`, for callers that hold other parts of the service: a
/// buffer doubles (one that went to nothing while idle comes back at its
/// first size), up to `TCP_MAX`, under pressure up to Linux's first sizes.
fn grow(mem: &mut BTreeMap<SocketHandle, Mem>, bytes: &mut Budget, h: SocketHandle, way: Way, sockets: &mut SocketSet<'static>) -> bool {
    let old = capacity(sockets.get::<tcp::Socket>(h), way);
    let most = match way {
        _ if !pressure(bytes) => TCP_MAX,
        Way::Rx => TCP_RX_INIT,
        Way::Tx => TCP_TX_INIT,
    };
    let new = if old == 0 { first_size(bytes, way) } else { (old * 2).min(most) };
    new > old && resize(mem, bytes, h, way, new, sockets)
}

/// `Service::equip`, likewise: the first sizes, or what the owner's budget
/// allows down to `TCP_MIN`.
fn equip(mem: &mut BTreeMap<SocketHandle, Mem>, bytes: &mut Budget, h: SocketHandle, sockets: &mut SocketSet<'static>) -> Result<(), i64> {
    for way in [Way::Rx, Way::Tx] {
        if capacity(sockets.get::<tcp::Socket>(h), way) > 0 {
            continue;
        }
        let mut size = first_size(bytes, way);
        while !resize(mem, bytes, h, way, size, sockets) {
            if size <= TCP_MIN {
                return Err(ENOBUFS);
            }
            size /= 2;
        }
    }
    Ok(())
}

/// An idle connection's buffers go back under pressure: the send buffer
/// entirely (it comes back with the next write, `grow`), the receive
/// buffer down to `TCP_MIN` (the window shrinks: see smoltcp's
/// `replace_rx_buffer`). Only empty buffers shrink; true if one did.
fn trim(mem: &mut BTreeMap<SocketHandle, Mem>, bytes: &mut Budget, h: SocketHandle, sockets: &mut SocketSet<'static>) -> bool {
    let socket = sockets.get::<tcp::Socket>(h);
    let (rx, tx) = (socket.recv_capacity() > TCP_MIN, socket.send_capacity() > 0);
    let rx = rx && resize(mem, bytes, h, Way::Rx, TCP_MIN, sockets);
    let tx = tx && resize(mem, bytes, h, Way::Tx, 0, sockets);
    rx || tx
}

/// Moves TCP socket `h`'s buffer `way`, with what it holds, into one of
/// `new` bytes (none for 0): more memory is charged to its owner first,
/// less is given back after. False if the size stays, the budget is short,
/// or smoltcp keeps its buffer (a smaller one for a buffer that is not
/// empty).
fn resize(mem: &mut BTreeMap<SocketHandle, Mem>, bytes: &mut Budget, h: SocketHandle, way: Way, new: usize, sockets: &mut SocketSet<'static>) -> bool {
    let Some(m) = mem.get_mut(&h) else { return false };
    let socket = sockets.get_mut::<tcp::Socket>(h);
    if new == capacity(socket, way) {
        return false;
    }
    let region = if way == Way::Rx { &mut m.rx } else { &mut m.tx };
    let (before, after) = (region.as_ref().map_or(0, |r| r.mapped), mapped(new));
    if after > before && bytes.charge(m.owner, after - before).is_err() {
        return false;
    }
    let taken = buffer(new).ok().and_then(|(new_region, storage)| {
        let ok = match way {
            Way::Rx => socket.replace_rx_buffer(storage).is_ok(),
            Way::Tx => socket.replace_tx_buffer(storage).is_ok(),
        };
        // (A refused storage came back and went; its region goes here.)
        ok.then_some(new_region)
    });
    let Some(new_region) = taken else {
        if after > before {
            bytes.uncharge(m.owner, after - before);
        }
        return false;
    };
    // The old buffer goes now: smoltcp no longer uses it.
    *region = new_region;
    if before > after {
        bytes.uncharge(m.owner, before - after);
    }
    true
}

/// Publishes what changed of `s` in its control block; true if anything
/// did (the block's `seq` advanced, its waiters woken).
fn publish(s: &mut Sock, ctl: &Ctl) -> bool {
    let now = Shown { state: s.state, rx_tail: s.rx_tail, tx_head: s.tx_head, backlog: s.backlog, err_seq: s.err_seq };
    let sh = &s.shown;
    if now.state == sh.state && now.rx_tail == sh.rx_tail && now.tx_head == sh.tx_head && now.backlog == sh.backlog && now.err_seq == sh.err_seq {
        return false;
    }
    let n = &ctl.netd;
    // The error before its count, the data before the state that ends it.
    n.err_seq.store(now.err_seq, SeqCst);
    n.rx_tail.store(now.rx_tail, SeqCst);
    n.tx_head.store(now.tx_head, SeqCst);
    n.backlog.store(now.backlog, SeqCst);
    n.state.store(now.state, SeqCst);
    ctl.changed(&WakeAll);
    s.shown = now;
    true
}
