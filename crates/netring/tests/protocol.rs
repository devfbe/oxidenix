//! The socket protocol's encodings and its shared memory: every request
//! survives encode and decode, malformed descriptors are refused with the
//! right errno (unknown operations, stray fields, areas, endpoints and
//! sockets out of range), the ring arithmetic wraps, records and link
//! records round-trip, and the wake protocols lose no wakeup with the two
//! sides on different threads while bytes stream through a ring intact.

use netring::errno::*;
use netring::*;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

fn area(grant: u32, offset: u32, size: u32) -> Area {
    Area { grant, offset, size }
}

fn ep(addr: u32, port: u16) -> Endpoint {
    Endpoint { addr, port }
}

fn every_request() -> Vec<Request> {
    vec![
        Request::Socket { sock: 0, kind: Kind::Tcp, area: None },
        Request::Socket { sock: 7, kind: Kind::Udp, area: Some(area(1, 0, 65536)) },
        Request::Socket { sock: MAX_SOCKETS as u32 - 1, kind: Kind::RawIcmp, area: Some(area(3, 4096, MIN_RING)) },
        Request::Bind { sock: 1, at: ep(0x7f00_0001, 8080), reuse: true },
        Request::Bind { sock: 1, at: ep(0, 0), reuse: false },
        Request::Listen { sock: 2, backlog: 511, reuse: true },
        Request::Connect { sock: 3, to: ep(0x0a00_0264, 7), area: Some(area(2, 1 << 21, MAX_RING)) },
        Request::Connect { sock: 3, to: ep(0, 0), area: None },
        Request::Accept { sock: 4, new: 5, area: area(9, 8192, 65536) },
        Request::Send { sock: 6, to: ep(0x7f00_0001, 53), len: 65507 },
        Request::Send { sock: 6, to: ep(0, 0), len: 0 },
        Request::Shutdown { sock: 8, how: SHUT_WR },
        Request::Shutdown { sock: 8, how: SHUT_RD | SHUT_WR },
        Request::Close { sock: 9, abort: true },
        Request::Close { sock: 9, abort: false },
        Request::Name { sock: 10, peer: true },
        Request::SetOpt { sock: 11, opt: opt::NODELAY, value: 1 },
        Request::Links { buf: Buf { grant: 1, offset: 100, len: MAX_LINKS_BUF } },
        Request::Forget { grant: 4095 },
    ]
}

#[test]
fn every_request_round_trips() {
    for (tag, r) in every_request().into_iter().enumerate() {
        let d = r.encode(tag as u64 + 100);
        assert_eq!(d.tag, tag as u64 + 100);
        assert_eq!(d.op, r.op());
        assert_eq!(Request::decode(&d), Ok(r), "{r:?}");
    }
}

#[test]
fn unknown_operations_are_refused() {
    for op in [0, 13, 99, u16::MAX] {
        assert_eq!(Request::decode(&Desc { op, ..Desc::default() }), Err(ENOSYS));
    }
}

#[test]
fn stray_fields_are_refused() {
    for r in every_request() {
        let d = r.encode(1);
        let mut variants = vec![Desc { flags: 1, ..d }];
        if matches!(r, Request::Links { .. } | Request::Forget { .. }) {
            variants.push(Desc { object: 1, ..d });
        }
        if d.offset == 0 && !matches!(r, Request::Bind { .. } | Request::Connect { .. } | Request::Send { .. }) {
            variants.push(Desc { offset: 1, ..d });
        }
        if d.grant == 0 && !matches!(r, Request::Socket { .. } | Request::Connect { .. }) {
            variants.push(Desc { grant: 1, ..d });
        }
        for k in 0..3 {
            if d.arg[k] == 0 {
                let mut v = d;
                v.arg[k] = 1 << 40;
                variants.push(v);
            }
        }
        for v in variants {
            assert_eq!(Request::decode(&v), Err(EINVAL), "{r:?} with {v:?}");
        }
    }
}

#[test]
fn areas_must_be_two_rings_of_a_power_of_two_on_a_page() {
    let good = Request::Socket { sock: 1, kind: Kind::Udp, area: Some(area(1, 0, 4096)) }.encode(1);
    assert!(Request::decode(&good).is_ok());
    for (grant, buf_off, len) in [(1, 0, 8191), (1, 0, 3 * 4096), (1, 0, 2048), (1, 0, 4 << 20), (1, 100, 8192), (0, 0, 8192), (1, 4096, 0)] {
        let d = Desc { grant, buf_off, len, ..good };
        assert_eq!(Request::decode(&d), Err(EINVAL), "area ({grant}, {buf_off}, {len})");
    }
    // TCP gets its rings later; a datagram socket at once.
    let tcp_with = Desc { arg: [Kind::Tcp as u64, 0, 0], ..good };
    assert_eq!(Request::decode(&tcp_with), Err(EINVAL));
    let udp_without = Request::Socket { sock: 1, kind: Kind::Tcp, area: None }.encode(1);
    assert_eq!(Request::decode(&Desc { arg: [Kind::Udp as u64, 0, 0], ..udp_without }), Err(EINVAL));
    assert_eq!(Request::decode(&Desc { arg: [4, 0, 0], ..udp_without }), Err(EINVAL));
    // Accept needs one, and two different sockets.
    let accept = Request::Accept { sock: 4, new: 5, area: area(1, 0, 4096) }.encode(1);
    assert_eq!(Request::decode(&Desc { grant: 0, buf_off: 0, len: 0, ..accept }), Err(EINVAL));
    assert_eq!(Request::decode(&Desc { arg: [4, 0, 0], ..accept }), Err(EINVAL));
}

#[test]
fn sockets_endpoints_and_values_must_be_in_range() {
    let bind = Request::Bind { sock: 1, at: ep(1, 1), reuse: false }.encode(1);
    assert_eq!(Request::decode(&Desc { object: MAX_SOCKETS as u64, ..bind }), Err(EINVAL));
    assert_eq!(Request::decode(&Desc { offset: 1 << 48, ..bind }), Err(EINVAL));
    assert_eq!(Request::decode(&Desc { arg: [2, 0, 0], ..bind }), Err(EINVAL));
    let accept = Request::Accept { sock: 4, new: 5, area: area(1, 0, 4096) }.encode(1);
    assert_eq!(Request::decode(&Desc { arg: [MAX_SOCKETS as u64, 0, 0], ..accept }), Err(EINVAL));
    let send = Request::Send { sock: 1, to: ep(0, 0), len: 1 }.encode(1);
    assert_eq!(Request::decode(&Desc { len: MAX_RING + 1, ..send }), Err(EINVAL));
    let shut = Request::Shutdown { sock: 1, how: SHUT_RD }.encode(1);
    for how in [0, 4, 7] {
        assert_eq!(Request::decode(&Desc { arg: [how, 0, 0], ..shut }), Err(EINVAL));
    }
    let close = Request::Close { sock: 1, abort: false }.encode(1);
    assert_eq!(Request::decode(&Desc { arg: [2, 0, 0], ..close }), Err(EINVAL));
    let name = Request::Name { sock: 1, peer: false }.encode(1);
    assert_eq!(Request::decode(&Desc { arg: [2, 0, 0], ..name }), Err(EINVAL));
    let links = Request::Links { buf: Buf { grant: 1, offset: 0, len: 1 } }.encode(1);
    assert_eq!(Request::decode(&Desc { len: 0, ..links }), Err(EINVAL));
    assert_eq!(Request::decode(&Desc { len: MAX_LINKS_BUF + 1, ..links }), Err(EINVAL));
    assert_eq!(Request::decode(&Desc { grant: 0, ..links }), Err(EINVAL));
    let listen = Request::Listen { sock: 1, backlog: 1, reuse: false }.encode(1);
    assert_eq!(Request::decode(&Desc { arg: [1 << 32, 0, 0], ..listen }), Err(EINVAL));
    assert_eq!(Request::decode(&Desc { arg: [1, 2, 0], ..listen }), Err(EINVAL));
}

#[test]
fn option_values_are_checked_before_netd_uses_them() {
    let ok = |o: u32, v: u64| Request::decode(&Request::SetOpt { sock: 1, opt: o, value: v }.encode(1));
    assert!(ok(opt::NODELAY, 0).is_ok() && ok(opt::NODELAY, 1).is_ok());
    assert_eq!(ok(opt::NODELAY, 2), Err(EINVAL));
    // Keep-alive: off, or whole seconds Linux takes (1..=32767 s).
    assert!(ok(opt::KEEPALIVE, 0).is_ok() && ok(opt::KEEPALIVE, 1000).is_ok() && ok(opt::KEEPALIVE, 32_767_000).is_ok());
    for v in [1, 999, 32_767_001, u64::MAX] {
        assert_eq!(ok(opt::KEEPALIVE, v), Err(EINVAL), "keep-alive {v}");
    }
    assert!(ok(opt::TTL, 1).is_ok() && ok(opt::TTL, 255).is_ok());
    assert_eq!(ok(opt::TTL, 0), Err(EINVAL));
    assert_eq!(ok(opt::TTL, 256), Err(EINVAL));
    assert_eq!(ok(99, 1), Err(ENOPROTOOPT));
    assert_eq!(ok(0, 0), Err(ENOPROTOOPT));
}

#[test]
fn ring_pieces_and_fill_wrap() {
    assert_eq!(pieces(0, 10, 16), [(0, 10), (0, 0)]);
    assert_eq!(pieces(12, 10, 16), [(12, 4), (0, 6)]);
    assert_eq!(pieces(u32::MAX - 1, 4, 16), [(14, 2), (0, 2)]);
    assert_eq!(pieces(5, 16, 16), [(5, 11), (0, 5)]);
    assert_eq!(pieces(5, 40, 16), [(5, 11), (0, 5)], "never more than the ring");
    assert_eq!(fill(10, 20, 16), Some(10));
    assert_eq!(fill(u32::MAX - 3, 4, 16), Some(8));
    assert_eq!(fill(0, 17, 16), None);
    assert_eq!(fill(5, 4, 16), None, "a producer behind the consumer");
}

#[test]
fn records_round_trip_and_stay_aligned() {
    let r = Record { len: 65507, from: ep(0x7f00_0001, 5353) };
    assert_eq!(Record::decode(&r.encode()), r);
    assert_eq!(Record::span(0), 16);
    assert_eq!(Record::span(1), 32);
    assert_eq!(Record::span(16), 32);
    // The largest UDP datagram fits a 64 KiB ring exactly.
    assert_eq!(Record::span(65507), 65536);
}

#[test]
fn links_round_trip() {
    let links = [
        Link { index: 1, kind: LINK_LOOPBACK, state: LINK_UP | LINK_RUNNING, mtu: 1500, mac: [0; 6], prefix: 8, address: 0x7f00_0001 },
        Link { index: 2, kind: LINK_ETHERNET, state: LINK_UP, mtu: 1500, mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56], prefix: 24, address: 0x0a00_020f },
    ];
    let mut payload: Vec<u8> = links.iter().flat_map(|l| l.encode()).collect();
    payload.push(0xff);
    let back: Vec<Link> = Link::decode_all(&payload).collect();
    assert_eq!(back, links);
}

fn holder(owner: u64, addr: Option<u32>, reuse: bool) -> PortHolder {
    PortHolder { owner, port: 8080, addr, reuse, listening: false, connected: false, closing: false }
}

fn claim(owner: u64, addr: Option<u32>, reuse: bool) -> PortClaim {
    PortClaim { owner, port: 8080, addr, reuse }
}

const LO: Option<u32> = Some(0x7f00_0001);

#[test]
fn tcp_ports_follow_linux_within_an_instance() {
    // Bound sockets: shared only if both allow reuse.
    assert!(tcp_port_conflict(&claim(1, LO, false), &holder(1, LO, false)));
    assert!(tcp_port_conflict(&claim(1, LO, true), &holder(1, LO, false)));
    assert!(tcp_port_conflict(&claim(1, LO, false), &holder(1, LO, true)));
    assert!(!tcp_port_conflict(&claim(1, LO, true), &holder(1, LO, true)));
    // A listener is never shared: the second of two reusing sockets that
    // listens conflicts (the re-check at listen).
    let listener = PortHolder { listening: true, ..holder(1, LO, true) };
    assert!(tcp_port_conflict(&claim(1, LO, true), &listener));
    // The wildcard overlaps every address; distinct addresses do not.
    assert!(tcp_port_conflict(&claim(1, None, false), &holder(1, LO, false)));
    assert!(tcp_port_conflict(&claim(1, LO, false), &holder(1, None, false)));
    assert!(!tcp_port_conflict(&claim(1, LO, false), &holder(1, Some(0x0a00_020f), false)));
    // Another port: nothing.
    assert!(!tcp_port_conflict(&PortClaim { port: 8081, ..claim(1, LO, false) }, &holder(1, LO, false)));
    // Connections and closing ones (TIME-WAIT) give way to reuse only.
    let conn = PortHolder { connected: true, ..holder(1, LO, true) };
    assert!(!tcp_port_conflict(&claim(1, LO, true), &conn));
    assert!(tcp_port_conflict(&claim(1, LO, false), &conn));
    let closing = PortHolder { closing: true, ..holder(1, LO, true) };
    assert!(!tcp_port_conflict(&claim(1, LO, true), &closing));
    assert!(tcp_port_conflict(&claim(1, LO, false), &closing));
}

#[test]
fn no_instance_takes_a_port_another_one_serves() {
    // Another instance's bound or listening socket: a conflict whatever
    // both opted in to.
    assert!(tcp_port_conflict(&claim(2, LO, true), &holder(1, LO, true)));
    assert!(tcp_port_conflict(&claim(2, LO, true), &PortHolder { listening: true, ..holder(1, LO, true) }));
    assert!(tcp_port_conflict(&claim(2, None, true), &holder(1, LO, true)));
    // Its connections and closing sockets only by Linux's rule.
    assert!(!tcp_port_conflict(&claim(2, LO, true), &PortHolder { connected: true, ..holder(1, LO, true) }));
    assert!(!tcp_port_conflict(&claim(2, LO, true), &PortHolder { closing: true, ..holder(1, LO, true) }));
    assert!(tcp_port_conflict(&claim(2, LO, false), &PortHolder { closing: true, ..holder(1, LO, true) }));
    // UDP: reuse within an instance only.
    assert!(!udp_port_conflict(&claim(1, LO, true), &holder(1, LO, true)));
    assert!(udp_port_conflict(&claim(1, LO, false), &holder(1, LO, true)));
    assert!(udp_port_conflict(&claim(2, LO, true), &holder(1, LO, true)));
    assert!(udp_port_conflict(&claim(2, None, true), &holder(1, LO, true)));
    assert!(!udp_port_conflict(&claim(2, LO, true), &holder(1, Some(0x0a00_020f), true)));
}

#[test]
fn budgets_bound_each_instance_and_all() {
    let mut b = Budget::new(100, 60);
    // One instance gets its share, not more, however it asks (many
    // channels of it are the same owner).
    assert_eq!(b.charge(1, 40), Ok(()));
    assert_eq!(b.charge(1, 30), Err(ENOBUFS));
    assert_eq!(b.held(1), 40, "a refused charge takes nothing");
    assert_eq!(b.charge(1, 20), Ok(()));
    assert_eq!(b.room(1), 0);
    // Another instance gets what is left of the whole.
    assert_eq!(b.room(2), 40);
    assert_eq!(b.charge(2, 41), Err(ENOBUFS));
    assert_eq!(b.charge(2, 40), Ok(()));
    assert_eq!(b.charge(3, 1), Err(ENOBUFS));
    // Given back to the instance charged, never more than it holds.
    b.uncharge(1, 1000);
    assert_eq!((b.held(1), b.used()), (0, 40));
    b.uncharge(4, 10);
    assert_eq!(b.used(), 40);
    assert_eq!(b.charge(3, 60), Ok(()));
    assert_eq!(b.room(1), 0, "the whole is used up");
    b.uncharge(2, 40);
    b.uncharge(3, 60);
    assert_eq!((b.used(), b.room(5)), (0, 60));
}

/// A shared area in host memory, page-aligned.
fn shared_area() -> &'static SharedArea {
    let layout = std::alloc::Layout::from_size_align(SHARED_PAGES as usize * PAGE, PAGE).unwrap();
    let p = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!p.is_null());
    unsafe { SharedArea::at(p) }
}

#[test]
fn bitmaps_hand_out_each_mark_once() {
    let area = shared_area();
    let h = &area.header;
    for i in [0, 63, 64, 500, MAX_SOCKETS - 1, MAX_SOCKETS + 5] {
        h.mark_client(i);
    }
    h.mark_client(63);
    let mut got = Vec::new();
    assert!(h.take_client(|i| got.push(i)));
    assert_eq!(got, vec![0, 63, 64, 500, MAX_SOCKETS - 1]);
    assert!(!h.take_client(|_| panic!("taken twice")));
    assert!(!h.service_pending());
    h.mark_service(7);
    assert!(h.service_pending());
    assert!(h.take_service(|i| assert_eq!(i, 7)));
    assert!(!h.service_pending());
}

/// A futex for the host: one condition variable for all words (wakes are
/// broadcast, as a futex wake of every waiter of a word would be at worst).
#[derive(Default)]
struct Futex {
    lock: Mutex<()>,
    cond: Condvar,
    /// Waits that ran into their timeout: a lost wakeup (the tests' waits
    /// should always be woken long before).
    timeouts: AtomicUsize,
}

impl Futex {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let guard = self.lock.lock().unwrap();
        if word.load(Ordering::SeqCst) == value {
            let (_guard, r) = self.cond.wait_timeout(guard, Duration::from_secs(5)).unwrap();
            if r.timed_out() {
                self.timeouts.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn wake(&self) {
        let _guard = self.lock.lock().unwrap();
        self.cond.notify_all();
    }
}

struct Waker(Arc<Futex>);

impl Wait for Waker {
    fn wait(&self, word: &AtomicU32, value: u32) {
        self.0.wait(word, value);
    }

    fn wake(&self, _word: &AtomicU32) {
        self.0.wake();
    }
}

/// netd's side of a stream's receive ring against a reader that sleeps on
/// the control block whenever the ring is empty, and netd's own wait for
/// room (`rx_wait`) against the reader's doorbell: every byte arrives in
/// order, the ring never holds more than its size, nobody sleeps through
/// a change.
#[test]
fn bytes_stream_through_a_ring_and_no_wakeup_is_lost() {
    const SIZE: u32 = 4096;
    const TOTAL: u64 = 4 << 20;
    let area = shared_area();
    let ctl = area.ctl(3).unwrap();
    ctl.reset_netd(state::ESTABLISHED | state::SEND_OPEN);
    ctl.reset_client();
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..SIZE).map(|_| AtomicU32::new(0)).collect());
    let futex = Arc::new(Futex::default());
    // netd's doorbell in the test: a counter it sleeps on (in netd, the
    // submission ring's tail).
    let doorbell: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
    let producer = {
        let (ring, futex) = (ring.clone(), futex.clone());
        thread::spawn(move || {
            let w = Waker(futex.clone());
            let mut sent = 0u64;
            let mut tail = 0u32;
            while sent < TOTAL {
                let head = ctl.client.rx_head.load(Ordering::SeqCst);
                let room = SIZE - fill(head, tail, SIZE).expect("the reader stays behind");
                if room == 0 {
                    // Announce the wait for room, look again, sleep on the
                    // doorbell (the reader rings after taking bytes).
                    let seen = doorbell.load(Ordering::SeqCst);
                    ctl.netd.rx_wait.store(1, Ordering::SeqCst);
                    if ctl.client.rx_head.load(Ordering::SeqCst) == head {
                        futex.wait(doorbell, seen);
                    }
                    ctl.netd.rx_wait.store(0, Ordering::SeqCst);
                    continue;
                }
                let n = room.min(((TOTAL - sent) as u32).min(1 + (sent as u32 % 1000)));
                let [(at, len), (at2, len2)] = pieces(tail, n, SIZE);
                for k in 0..len {
                    ring[(at + k) as usize].store((sent + k as u64) as u32, Ordering::Relaxed);
                }
                for k in 0..len2 {
                    ring[(at2 + k) as usize].store((sent + (len + k) as u64) as u32, Ordering::Relaxed);
                }
                tail = tail.wrapping_add(n);
                sent += n as u64;
                ctl.netd.rx_tail.store(tail, Ordering::SeqCst);
                ctl.changed(&w);
            }
        })
    };
    let w = Waker(futex.clone());
    let mut got = 0u64;
    let mut head = 0u32;
    while got < TOTAL {
        let seen = ctl.seen();
        let tail = ctl.netd.rx_tail.load(Ordering::SeqCst);
        let n = fill(head, tail, SIZE).expect("never more than the ring");
        if n == 0 {
            ctl.sleep(seen, |word, value| -> Result<(), ()> {
                w.wait(word, value);
                Ok(())
            })
            .unwrap();
            continue;
        }
        let take = n.min(1 + (got as u32 % 777));
        let [(at, len), (at2, len2)] = pieces(head, take, SIZE);
        for k in 0..len {
            assert_eq!(ring[(at + k) as usize].load(Ordering::Relaxed), (got + k as u64) as u32);
        }
        for k in 0..len2 {
            assert_eq!(ring[(at2 + k) as usize].load(Ordering::Relaxed), (got + (len + k) as u64) as u32);
        }
        head = head.wrapping_add(take);
        got += take as u64;
        ctl.client.rx_head.store(head, Ordering::SeqCst);
        if ctl.netd.rx_wait.load(Ordering::SeqCst) != 0 {
            doorbell.fetch_add(1, Ordering::SeqCst);
            futex.wake();
        }
    }
    producer.join().unwrap();
    assert_eq!(futex.timeouts.load(Ordering::Relaxed), 0, "a wakeup was lost");
}

/// netd marks sockets and wakes the net thread at most once per round; the
/// net thread takes the marks and sleeps when there are none: every mark
/// is taken, none is lost while the thread sleeps.
#[test]
fn the_net_thread_sees_every_mark() {
    const ROUNDS: usize = 100_000;
    let area = shared_area();
    let futex = Arc::new(Futex::default());
    let taken = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let thread = {
        let (futex, taken, done) = (futex.clone(), taken.clone(), done.clone());
        thread::spawn(move || {
            let w = Waker(futex);
            let h = &area.header;
            loop {
                let seen = h.client_seen();
                let mut n = 0;
                h.take_client(|i| {
                    assert!(i < MAX_SOCKETS);
                    n += 1;
                });
                taken.fetch_add(n, Ordering::SeqCst);
                if n > 0 {
                    continue;
                }
                if done.load(Ordering::SeqCst) {
                    return;
                }
                h.sleep_client(seen, &w);
            }
        })
    };
    let w = Waker(futex.clone());
    let h = &area.header;
    let mut marked = 0;
    for round in 0..ROUNDS {
        // One socket per round, waiting until the thread took the last one,
        // so that every mark counts once.
        while taken.load(Ordering::SeqCst) < marked {
            std::hint::spin_loop();
        }
        h.mark_client(round % MAX_SOCKETS);
        marked += 1;
        h.wake_client(&w);
        if round % 1000 == 0 {
            thread::sleep(Duration::from_micros(200));
        }
    }
    while taken.load(Ordering::SeqCst) < marked {
        thread::sleep(Duration::from_millis(1));
    }
    done.store(true, Ordering::SeqCst);
    h.poke_client(&w);
    thread.join().unwrap();
    assert_eq!(taken.load(Ordering::SeqCst), ROUNDS);
    assert_eq!(futex.timeouts.load(Ordering::Relaxed), 0, "a wakeup was lost");
}
