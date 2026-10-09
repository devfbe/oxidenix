//! The socket service: netd's end of the instances' channels (phase R7b,
//! ADR 0007, the protocol `netring`), mapped onto smoltcp's sockets. One
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
//! anything is allocated): `MAX_CHANNELS` channels and two per instance,
//! `MAX_GRANTS` grants a channel, the bytes of smoltcp's buffers and of
//! closed connections' leftovers (`BUDGET`, three quarters of it per
//! instance), TIME-WAIT records (`MAX_LINGERING`, a quarter per instance),
//! no more requests taken than the completion ring has room for.

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
const MAX_CHANNELS: usize = 16;
/// Grants of a channel netd keeps mapped (`FORGET` lets go).
const MAX_GRANTS: usize = 256;
/// Requests taken from one channel per round, for fairness.
const TAKE_PER_ROUND: usize = 16;
/// smoltcp's buffers per TCP socket, each way.
pub const TCP_BUFFER: usize = 64 * 1024;
/// A UDP socket's: room for the largest datagram each way.
const UDP_BUFFER: usize = 64 * 1024;
const UDP_PACKETS: usize = 16;
const RAW_BUFFER: usize = 16 * 1024;
/// The bytes of smoltcp's socket buffers netd keeps at most (its heap is
/// sized for them, see main).
pub const BUDGET: usize = 24 << 20;
/// What one channel (an instance) may take of it.
const INSTANCE_BUDGET: usize = BUDGET * 3 / 4;
/// Ports in TIME-WAIT kept at once, and for one instance: half the
/// ephemeral range at most, so connects of others always find a port.
const MAX_LINGERING: usize = 8192;
const INSTANCE_LINGERING: usize = 2048;
/// Channels of one instance (one, and a new one while netd still tears
/// down the old after its client gave it up).
const INSTANCE_CHANNELS: usize = 2;
const MAX_BACKLOG: usize = 8;
/// The largest UDP payload (an IPv4 packet of 65535 bytes).
const MAX_UDP: usize = 65507;
const IPV4_HEADER: usize = 20;
/// The Ethernet header a frame adds to an IP packet.
const ETHERNET_HEADER: usize = 14;
/// A connection attempt (or unacknowledged data) gives up after this.
const TCP_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_EPHEMERAL: u16 = 49152;
/// How long a closed connection's port stays taken (smoltcp's TIME-WAIT).
const TIME_WAIT: Duration = Duration::from_secs(10);
/// What a TCP socket's buffers cost.
const TCP_COST: usize = 2 * TCP_BUFFER;

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
/// dropped: a socket's smoltcp buffers, a closed connection's leftovers.
/// So netd's memory follows its sockets (its heap stays small), and short
/// memory is ENOBUFS for the socket, never a failed allocation in netd.
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

    /// Its two halves as smoltcp's buffers. They live as long as the
    /// region: the owner drops the region only after the socket that uses
    /// them left smoltcp (`Service::drop_socket`).
    fn halves(&mut self) -> (&'static mut [u8], &'static mut [u8]) {
        let half = self.len / 2;
        unsafe { (core::slice::from_raw_parts_mut(self.addr, half), core::slice::from_raw_parts_mut(self.addr.add(half), half)) }
    }
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
}

enum Proto {
    Tcp(Tcp),
    Udp { handle: SocketHandle, peer: Option<IpEndpoint>, reuse: bool },
    Raw { handle: SocketHandle, peer: Option<Ipv4Address> },
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
    /// The bytes of smoltcp buffers it holds (`BUDGET`).
    cost: usize,
}

impl Sock {
    fn new(proto: Proto, area: Option<Area>, state: u32, cost: usize) -> Sock {
        Sock { proto, area, rx_tail: 0, tx_head: 0, state, backlog: 0, err_seq: 0, rx_wait: false, shown: Shown { state, ..Shown::default() }, cost }
    }
}

/// Posts error `e` (a positive errno) in `ctl`.
fn post_error(err_seq: &mut u32, ctl: &Ctl, e: i64) {
    *err_seq = err_seq.wrapping_add(1);
    ctl.netd.error.store(e as u32, SeqCst);
}

/// A TCP connection its owner closed, finishing (after its leftovers).
struct Closing {
    handle: SocketHandle,
    /// What its send ring still held, sent before the FIN (charged to the
    /// budget with `cost`).
    leftover: Option<Region>,
    sent: usize,
    local: Option<IpListenEndpoint>,
    reuse: bool,
    /// The instance it belonged to.
    owner: u64,
    cost: usize,
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
    /// Closed connections in TIME-WAIT: only their port, until then (their
    /// smoltcp socket and its buffers went).
    lingering: Vec<(PortHolder, Instant)>,
    next_port: u16,
    /// What the instances hold, each within its share: the bytes of
    /// smoltcp's socket buffers (and of closed connections' leftovers),
    /// records of ports in TIME-WAIT, channels.
    bytes: Budget,
    lingering_records: Budget,
    channels: Budget,
    /// The memory of each smoltcp socket's buffers.
    regions: BTreeMap<SocketHandle, Region>,
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
            bytes: Budget::new(BUDGET, INSTANCE_BUDGET),
            lingering_records: Budget::new(MAX_LINGERING, INSTANCE_LINGERING),
            channels: Budget::new(MAX_CHANNELS, INSTANCE_CHANNELS),
            regions: BTreeMap::new(),
            scratch: vec![0; MAX_UDP.max(RAW_BUFFER)],
            config: Config::default(),
        }
    }

    // ------------------------------------------------------------ channels

    /// An offer from the kernel: the status to answer with.
    pub fn offer(&mut self, message: &[u8]) -> i64 {
        let Some(offer) = Offer::decode(message) else { return -EINVAL };
        let want = Layout::with_shared(netring::SLOTS, netring::SHARED_PAGES);
        if offer.layout() != want {
            return -EINVAL;
        }
        let layout = want.expect("a valid layout");
        let Some(slot) = self.chans.iter().position(Option::is_none) else { return -ENOSPC };
        // An instance gets a few channels (a new one after its old died),
        // never the slots of others.
        if self.channels.charge(offer.instance, 1).is_err() {
            return -ENOSPC;
        }
        let base = match oxrt::chan_attach(offer.channel) {
            Ok(base) => base,
            Err(e) => {
                self.channels.uncharge(offer.instance, 1);
                return e;
            }
        };
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
        });
        0
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
            Request::Listen { sock, backlog } => self.listen(c, sock, backlog, sockets),
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

    /// A new TCP socket in smoltcp, its buffers in memory of their own
    /// (ENOBUFS if memory is short).
    fn new_tcp(&mut self, sockets: &mut SocketSet<'static>) -> Result<SocketHandle, i64> {
        let mut region = Region::new(2 * TCP_BUFFER)?;
        let (rx, tx) = region.halves();
        let mut socket = tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx));
        socket.set_timeout(Some(TCP_TIMEOUT));
        let h = sockets.add(socket);
        self.regions.insert(h, region);
        Ok(h)
    }

    /// A new UDP or raw socket (`raw`), its payload buffers likewise.
    fn new_datagram(&mut self, sockets: &mut SocketSet<'static>, raw: bool) -> Result<SocketHandle, i64> {
        let mut region = Region::new(2 * if raw { RAW_BUFFER } else { UDP_BUFFER })?;
        let (rx, tx) = region.halves();
        let h = if raw {
            let rx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; UDP_PACKETS], rx);
            let tx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; UDP_PACKETS], tx);
            sockets.add(raw::Socket::new(Some(IpVersion::Ipv4), Some(IpProtocol::Icmp), rx, tx))
        } else {
            let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], rx);
            let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], tx);
            sockets.add(udp::Socket::new(rx, tx))
        };
        self.regions.insert(h, region);
        Ok(h)
    }

    /// A socket leaves smoltcp, and its buffers' memory goes back to the
    /// system.
    fn drop_socket(&mut self, sockets: &mut SocketSet<'static>, h: SocketHandle) {
        sockets.remove(h);
        self.regions.remove(&h);
    }

    /// Takes `cost` bytes for channel `c`'s instance before anything is
    /// allocated (ENOBUFS if they are not there): at most what is left of
    /// netd's budget and of the instance's share (whatever number of
    /// channels it has: no instance can take all of netd's memory from the
    /// others).
    fn charge(&mut self, c: usize, cost: usize) -> Result<(), i64> {
        let owner = self.chan(c).owner;
        self.bytes.charge(owner, cost)
    }

    /// Gives `cost` bytes back to the instance `owner` (whose channels may
    /// be gone already).
    fn uncharge(&mut self, owner: u64, cost: usize) {
        self.bytes.uncharge(owner, cost);
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
        let cost = match kind {
            Kind::Tcp => TCP_COST,
            Kind::Udp => 2 * UDP_BUFFER,
            Kind::RawIcmp => 2 * RAW_BUFFER,
        };
        self.charge(c, cost)?;
        let made = match kind {
            Kind::Tcp => self.new_tcp(sockets),
            Kind::Udp => self.new_datagram(sockets, false),
            Kind::RawIcmp => self.new_datagram(sockets, true),
        };
        let handle = match made {
            Ok(h) => h,
            Err(e) => {
                let owner = self.chan(c).owner;
                self.uncharge(owner, cost);
                return Err(e);
            }
        };
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
                };
                Sock::new(Proto::Tcp(t), None, 0, TCP_COST)
            }
            Kind::Udp => Sock::new(Proto::Udp { handle, peer: None, reuse: false }, area, state::SEND_OPEN | state::WRITABLE, cost),
            Kind::RawIcmp => Sock::new(Proto::Raw { handle, peer: None }, area, state::SEND_OPEN | state::WRITABLE, cost),
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
            out.extend(self.lingering.iter().map(|&(h, _)| h));
        }
        out
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

    fn listen(&mut self, c: usize, sock: u32, backlog: u32, sockets: &mut SocketSet<'static>) -> Reply {
        let bound = match &self.sock(c, sock)?.proto {
            Proto::Tcp(t) if t.backlog.is_some() => return done(0),
            Proto::Tcp(t) if t.established || t.connecting.is_some() => return Err(EINVAL),
            Proto::Tcp(t) => t.local.map(|l| (l, t.reuse)),
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
        // budget allows.
        let want = (backlog as usize).clamp(1, MAX_BACKLOG);
        let mut extra = Vec::new();
        while extra.len() + 1 < want && self.charge(c, TCP_COST).is_ok() {
            match self.new_tcp(sockets) {
                Ok(h) => extra.push(h),
                Err(_) => {
                    // Memory is short: a smaller backlog.
                    let owner = self.chan(c).owner;
                    self.uncharge(owner, TCP_COST);
                    break;
                }
            }
        }
        let s = self.sock(c, sock)?;
        s.cost += extra.len() * TCP_COST;
        let Proto::Tcp(t) = &mut s.proto else { unreachable!("checked above") };
        if let Some(port) = port {
            t.local = Some(IpListenEndpoint { addr: None, port });
        }
        let local = t.local.expect("bound");
        let mut set = vec![t.handle];
        set.extend(extra);
        for &h in &set {
            sockets.get_mut::<tcp::Socket>(h).listen(local).map_err(|_| EINVAL)?;
        }
        t.backlog = Some(set);
        t.failed = false;
        s.state = state::LISTENING;
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
                let bound = t.local;
                match (has_area, &area) {
                    (false, None) | (true, Some(_)) => return Err(EINVAL),
                    (false, Some(a)) => self.check_area(c, a)?,
                    (true, None) => {}
                }
                if to.port == 0 {
                    return Err(ECONNREFUSED);
                }
                let local = match bound {
                    Some(l) => l,
                    None => IpListenEndpoint { addr: None, port: self.ephemeral(true, sockets)? },
                };
                let s = self.sock(c, sock)?;
                let Proto::Tcp(t) = &mut s.proto else { unreachable!("checked above") };
                let remote = IpEndpoint::new(destination(to.addr), to.port);
                sockets.get_mut::<tcp::Socket>(t.handle).connect(iface.context(), remote, local).map_err(|e| match e {
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
        // A fresh listening socket takes the connection's place, if the
        // budget allows; else the backlog shrinks (never below one).
        let fresh = match self.charge(c, TCP_COST) {
            Ok(()) => match self.new_tcp(sockets) {
                Ok(h) => {
                    let _ = sockets.get_mut::<tcp::Socket>(h).listen(local);
                    Some(h)
                }
                Err(_) => {
                    let owner = self.chan(c).owner;
                    self.uncharge(owner, TCP_COST);
                    None
                }
            },
            Err(_) => None,
        };
        let s = self.sock(c, sock)?;
        let Proto::Tcp(t) = &mut s.proto else { unreachable!("checked above") };
        let set = t.backlog.as_mut().expect("listening");
        if fresh.is_none() && set.len() == 1 {
            return Err(ENOBUFS);
        }
        let conn = match fresh {
            Some(h) => core::mem::replace(&mut set[i], h),
            None => {
                // The connection's cost leaves with it.
                s.cost -= TCP_COST;
                set.remove(i)
            }
        };
        t.handle = set[0];
        let peer = sockets.get::<tcp::Socket>(conn).remote_endpoint().unwrap_or(IpEndpoint::new(ipv4(0), 0));
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
            last: sockets.get::<tcp::Socket>(conn).state(),
        };
        self.enter(c, new, Sock::new(Proto::Tcp(t), Some(area), state::ESTABLISHED | state::SEND_OPEN, TCP_COST));
        Ok((0, [bits(peer.addr) as u64, peer.port as u64, 0, 0]))
    }

    fn send(&mut self, c: usize, sock: u32, to: Endpoint, len: u32, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Reply {
        let (proto, rings) = {
            let chan = self.chan(c);
            let s = chan.socks.get(&sock).ok_or(EBADF)?;
            let rings = Self::rings(&chan.grants, s.area).ok_or(EINVAL)?;
            let proto = match &s.proto {
                Proto::Tcp(_) => return Err(EOPNOTSUPP),
                Proto::Udp { handle, peer, .. } => (true, *handle, peer.map(|p| (bits(p.addr), p.port))),
                Proto::Raw { handle, peer } => (false, *handle, peer.map(|p| (p.to_bits(), 0))),
            };
            (proto, rings)
        };
        if len > rings.size {
            return Err(EMSGSIZE);
        }
        let len = len as usize;
        let (udp, handle, peer) = proto;
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
            let repr = Ipv4Repr { src_addr: src, dst_addr: dst, next_header: IpProtocol::Icmp, payload_len: len, hop_limit: 64 };
            repr.emit(&mut Ipv4Packet::new_unchecked(&mut packet[..IPV4_HEADER]), &ChecksumCapabilities::default());
            let socket = sockets.get_mut::<raw::Socket>(handle);
            if !socket.can_send() {
                return Err(EAGAIN);
            }
            socket.send_slice(packet).map_err(|_| EAGAIN)?;
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
        let mut s = chan.socks.remove(&sock).ok_or(EBADF)?;
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
        if let Some((r, n)) = pending {
            if self.charge(c, n).is_err() {
                abort = true;
            } else {
                match Region::new(n) {
                    Ok(mut region) => {
                        // (Charged with the socket: given back when it goes.)
                        s.cost += n;
                        if Rings::read(r.tx, r.size, s.tx_head, region.bytes_mut()) {
                            leftover = Some(region);
                        }
                    }
                    Err(_) => {
                        self.uncharge(owner, n);
                        abort = true;
                    }
                }
            }
        }
        self.release(owner, s, abort, leftover, sockets);
        done(0)
    }

    /// A socket leaves its channel: datagram sockets go, a listener resets
    /// the connections nobody accepted, a TCP connection finishes (with
    /// `leftover` before its FIN), or is reset with `abort` or when data it
    /// received is unread.
    fn release(&mut self, owner: u64, s: Sock, abort: bool, mut leftover: Option<Region>, sockets: &mut SocketSet<'static>) {
        match s.proto {
            Proto::Tcp(t) => {
                let listener = t.backlog.is_some();
                let handles = t.backlog.unwrap_or_else(|| vec![t.handle]);
                let each = s.cost / handles.len();
                for h in handles {
                    let socket = sockets.get_mut::<tcp::Socket>(h);
                    let mut rest = None;
                    if listener || abort || socket.can_recv() {
                        // A listener's unaccepted connections, unread data:
                        // a reset (a socket that only listens just stops).
                        socket.abort();
                    } else if leftover.is_none() {
                        socket.close();
                    } else {
                        // The FIN after the leftovers (`finish_closing`).
                        rest = leftover.take();
                    }
                    let (local, reuse) = if listener { (None, true) } else { (t.local, t.reuse) };
                    self.closing.push(Closing { handle: h, leftover: rest, sent: 0, local, reuse, owner, cost: each });
                }
            }
            Proto::Udp { handle, .. } | Proto::Raw { handle, .. } => {
                self.drop_socket(sockets, handle);
                self.uncharge(owner, s.cost);
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
        match &self.sock(c, sock)?.proto {
            Proto::Tcp(t) => {
                let handles = t.backlog.clone().unwrap_or_else(|| vec![t.handle]);
                for h in handles {
                    let s = sockets.get_mut::<tcp::Socket>(h);
                    match option {
                        opt::NODELAY => s.set_nagle_enabled(value == 0),
                        opt::KEEPALIVE => s.set_keep_alive((value != 0).then(|| Duration::from_millis(value))),
                        opt::TTL => s.set_hop_limit(Some(ttl.ok_or(EINVAL)?)),
                        _ => return Err(ENOPROTOOPT),
                    }
                }
            }
            Proto::Udp { handle, .. } => match option {
                opt::TTL => sockets.get_mut::<udp::Socket>(*handle).set_hop_limit(Some(ttl.ok_or(EINVAL)?)),
                _ => return Err(ENOPROTOOPT),
            },
            Proto::Raw { .. } => return Err(ENOPROTOOPT),
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
        for chan in self.chans.iter_mut().flatten() {
            for (&i, s) in chan.socks.iter_mut() {
                let ctl = chan.area.ctl(i as usize).expect("an index below MAX_SOCKETS");
                let rings = Self::rings(&chan.grants, s.area);
                match pump_one(s, ctl, rings, sockets) {
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
        progress
    }

    /// Sends what closed connections still had, then their FIN; drops the
    /// ones smoltcp is done with.
    fn finish_closing(&mut self, sockets: &mut SocketSet<'static>) -> bool {
        let mut progress = false;
        let mut freed = Vec::new();
        let now = crate::now();
        let (lingering, records, regions) = (&mut self.lingering, &mut self.lingering_records, &mut self.regions);
        lingering.retain(|&(h, until)| {
            if now < until {
                return true;
            }
            records.uncharge(h.owner, 1);
            false
        });
        self.closing.retain_mut(|cl| {
            let socket = sockets.get_mut::<tcp::Socket>(cl.handle);
            if socket.state() == tcp::State::TimeWait {
                // Both FINs went: what is left is the port, for TIME-WAIT
                // (Linux keeps it as a small record too), within the
                // instance's share of records (beyond it the port is free
                // at once). The buffers go now; a segment of the old
                // connection that still comes is answered with a reset
                // instead of an ACK.
                if let Some(local) = cl.local.filter(|_| records.charge(cl.owner, 1).is_ok()) {
                    let holder = PortHolder { owner: cl.owner, port: local.port, addr: local.addr.map(bits), reuse: cl.reuse, listening: false, connected: false, closing: true };
                    lingering.push((holder, now + TIME_WAIT));
                }
                sockets.remove(cl.handle);
                regions.remove(&cl.handle);
                freed.push((cl.owner, cl.cost));
                progress = true;
                return false;
            }
            if socket.state() == tcp::State::Closed {
                // (Looked at before anything below closes it: a reset made
                // here goes out with the next poll, before the socket goes.)
                sockets.remove(cl.handle);
                regions.remove(&cl.handle);
                freed.push((cl.owner, cl.cost));
                progress = true;
                return false;
            }
            if socket.can_recv() {
                // Data for a connection nobody can read any more: a reset,
                // as Linux answers it (the writer learns it with EPIPE).
                socket.abort();
                cl.leftover = None;
                cl.sent = 0;
            }
            let mut all_sent = false;
            if let Some(left) = &cl.leftover {
                let bytes = left.bytes();
                match socket.send_slice(&bytes[cl.sent..]) {
                    Ok(n) => {
                        cl.sent += n;
                        progress |= n > 0;
                    }
                    // The connection went: what is left is lost, as on Linux.
                    Err(_) => cl.sent = bytes.len(),
                }
                all_sent = cl.sent == bytes.len();
            }
            if all_sent {
                // Its memory goes back at once (its charge with the
                // socket's).
                cl.leftover = None;
                socket.close();
            }
            true
        });
        for (owner, cost) in freed {
            self.uncharge(owner, cost);
        }
        progress
    }
}

/// Moves one socket's data and computes its state; Err if its client broke
/// the protocol or took its grant away.
fn pump_one(s: &mut Sock, ctl: &Ctl, rings: Option<Rings>, sockets: &mut SocketSet<'static>) -> Result<bool, ()> {
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
            }
            // The state.
            let st = socket.state();
            let mut b = 0;
            if let Some(started) = t.connecting {
                match st {
                    tcp::State::SynSent | tcp::State::SynReceived => b |= state::CONNECTING,
                    tcp::State::Closed | tcp::State::Listen => {
                        // The handshake failed.
                        t.connecting = None;
                        t.failed = true;
                        let e = if crate::now() - started >= TCP_TIMEOUT { ETIMEDOUT } else { ECONNREFUSED };
                        post_error(err_seq, ctl, e);
                    }
                    _ => {
                        t.connecting = None;
                        t.established = true;
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
                        post_error(err_seq, ctl, ECONNRESET);
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
        Proto::Udp { handle, .. } => {
            let socket = sockets.get_mut::<udp::Socket>(*handle);
            let mut moved = false;
            if let Some(r) = rings {
                loop {
                    let (len, from) = match socket.peek() {
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
        Proto::Raw { handle, .. } => {
            let socket = sockets.get_mut::<raw::Socket>(*handle);
            let mut moved = false;
            if let Some(r) = rings {
                loop {
                    let (len, from) = match socket.peek() {
                        // Whole IPv4 packets, header included, as on Linux.
                        Ok(data) => (data.len() as u32, Ipv4Packet::new_checked(data).map_or(0, |p| p.src_addr().to_bits())),
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
