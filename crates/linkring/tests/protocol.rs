//! The link protocol on the host: the description survives and malformed
//! ones are refused, frames come out as they went in (wrapping with pad
//! records), a full ring refuses, a hostile peer's positions and records
//! are refused, and with two threads no frame is lost or reordered and no
//! wakeup is lost.

use linkring::*;
use ring::channel::Layout;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A channel's memory, page aligned and zeroed.
struct Memory {
    ptr: *mut u8,
    layout: std::alloc::Layout,
    channel: Layout,
}

unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

impl Memory {
    fn new() -> Arc<Memory> {
        let channel = Layout::with_shared(SLOTS, SHARED_PAGES).expect("a valid layout");
        let layout = std::alloc::Layout::from_size_align(channel.bytes(), PAGE).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Arc::new(Memory { ptr, layout, channel })
    }

    fn area(&self) -> Area {
        unsafe { Area::new(self.ptr, &self.channel) }.expect("a link channel")
    }

    /// The shared area's header page as words (for a hostile peer's
    /// writes).
    fn word(&self, offset: usize) -> &AtomicU32 {
        unsafe { &*(self.ptr.add(self.channel.shared + offset) as *const AtomicU32) }
    }

    fn rx_byte(&self, offset: usize) -> *mut u8 {
        unsafe { self.ptr.add(self.channel.shared + PAGE + offset) }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

// Offsets of the header's lines (see `Header`).
const RX_TAIL: usize = 128 + 64;
const RX_HEAD: usize = RX_TAIL + 64;

/// A futex stand-in: waiting spins (with yields) while the word holds the
/// value, at most a few seconds (a lost wakeup shows as that timeout).
struct Spin;

impl Wait for Spin {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let until = Instant::now() + Duration::from_secs(5);
        while word.load(Ordering::SeqCst) == value && Instant::now() < until {
            std::thread::yield_now();
        }
    }
    fn wake(&self, _word: &AtomicU32) {}
}

fn info() -> Info {
    Info { mac: [0x02, 0x11, 0x22, 0x33, 0x44, 0x55], mtu: 1500, driver: Info::driver_field("cdc_ether") }
}

fn frame(seq: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| (seq as usize * 31 + i * 7) as u8).collect()
}

#[test]
fn the_description_survives() {
    let m = Memory::new();
    let area = m.area();
    assert_eq!(area.info(), Err(Violation::Info), "nothing written yet");
    area.init(&info());
    let got = area.info().unwrap();
    assert_eq!(got, info());
    assert_eq!(got.driver_name(), "cdc_ether");
}

#[test]
fn malformed_descriptions_are_refused() {
    for bad in [
        Info { mtu: 100, ..info() },
        Info { mtu: (MAX_FRAME - MIN_FRAME + 1) as u32, ..info() },
        Info { mac: [0x01, 0, 0, 0, 0, 1], ..info() },
        Info { mac: [0; 6], ..info() },
        Info { driver: Info::driver_field("bad name"), ..info() },
    ] {
        let m = Memory::new();
        m.area().init(&bad);
        assert_eq!(m.area().info(), Err(Violation::Info), "{bad:?}");
    }
}

#[test]
fn a_layout_of_another_protocol_is_refused() {
    let other = Layout::with_shared(4, SHARED_PAGES).unwrap();
    assert!(unsafe { Area::new(std::ptr::null_mut(), &other) }.is_none());
}

#[test]
fn frames_come_out_as_they_went_in_across_many_wraps() {
    let m = Memory::new();
    let area = m.area();
    let (mut tx, mut rx) = (area.rx_producer(), area.rx_consumer());
    let mut buf = vec![0u8; MAX_FRAME];
    let mut seq = 0u32;
    // Lengths that do not divide the ring: records end at every offset.
    for round in 0..4000 {
        let len = MIN_FRAME + (round * 37) % (MAX_FRAME - MIN_FRAME + 1);
        let f = frame(seq, len);
        assert!(tx.push(&f));
        assert_eq!(rx.pop(&mut buf), Ok(Some(len)));
        assert_eq!(&buf[..len], &f[..]);
        seq += 1;
    }
    assert_eq!(rx.pop(&mut buf), Ok(None));
}

#[test]
fn a_full_ring_refuses_and_takes_again_once_drained() {
    let m = Memory::new();
    let area = m.area();
    let (mut tx, mut rx) = (area.rx_producer(), area.rx_consumer());
    let mut n = 0;
    while tx.push(&frame(n, 1514)) {
        n += 1;
    }
    assert_eq!(n as usize, RING_BYTES / span(1514));
    assert!(!tx.has_room(1514));
    let mut buf = vec![0u8; MAX_FRAME];
    assert_eq!(rx.pop(&mut buf), Ok(Some(1514)));
    assert!(tx.push(&frame(n, 1514)));
    assert!(!tx.push(&frame(n, MAX_FRAME + 1)), "too long");
    assert!(!tx.push(&frame(n, MIN_FRAME - 1)), "too short");
}

#[test]
fn hostile_positions_and_records_are_refused() {
    let mut buf = vec![0u8; MAX_FRAME];
    // A tail beyond what the ring holds.
    let m = Memory::new();
    let mut rx = m.area().rx_consumer();
    m.word(RX_TAIL).store(RING_BYTES as u32 + 8, Ordering::Release);
    assert_eq!(rx.pop(&mut buf), Err(Violation::Position));
    // A record longer than published, of an unknown kind, too long.
    for (len, kind, tail) in [(1000u32, KIND_FRAME, 64u32), (64, 7, 72), (MAX_FRAME as u32 + 8, KIND_FRAME, 4096), (4, KIND_FRAME, 16)] {
        let m = Memory::new();
        let mut rx = m.area().rx_consumer();
        unsafe {
            std::ptr::copy_nonoverlapping(len.to_le_bytes().as_ptr(), m.rx_byte(0), 4);
            std::ptr::copy_nonoverlapping(kind.to_le_bytes().as_ptr(), m.rx_byte(4), 4);
        }
        m.word(RX_TAIL).store(tail, Ordering::Release);
        assert_eq!(rx.pop(&mut buf), Err(Violation::Record), "len {len} kind {kind}");
    }
    // A pad record where a whole record would still fit.
    let m = Memory::new();
    let mut rx = m.area().rx_consumer();
    unsafe { std::ptr::copy_nonoverlapping(KIND_PAD.to_le_bytes().as_ptr(), m.rx_byte(4), 4) };
    m.word(RX_TAIL).store(RING_BYTES as u32, Ordering::Release);
    assert_eq!(rx.pop(&mut buf), Err(Violation::Record));
    // A consumer that publishes a head beyond the producer's tail: no
    // room, nothing written.
    let m = Memory::new();
    let mut tx = m.area().rx_producer();
    m.word(RX_HEAD).store(4096, Ordering::Release);
    assert!(!tx.push(&frame(0, 100)));
}

#[test]
fn two_threads_lose_and_reorder_nothing() {
    const FRAMES: u32 = 200_000;
    let m = Memory::new();
    m.area().init(&info());
    let producer = {
        let m = m.clone();
        std::thread::spawn(move || {
            let area = m.area();
            let mut tx = area.rx_producer();
            let mut seq = 0;
            while seq < FRAMES {
                let len = MIN_FRAME + (seq as usize * 101) % 1500;
                let f = frame(seq, len);
                if tx.push(&f) {
                    seq += 1;
                    area.ring_netd(&Spin);
                } else {
                    std::thread::yield_now();
                }
            }
        })
    };
    let area = m.area();
    let mut rx = area.rx_consumer();
    let mut buf = vec![0u8; MAX_FRAME];
    let mut seq = 0;
    while seq < FRAMES {
        match rx.pop(&mut buf).unwrap() {
            Some(len) => {
                assert_eq!(&buf[..len], &frame(seq, MIN_FRAME + (seq as usize * 101) % 1500)[..]);
                seq += 1;
            }
            None => std::thread::yield_now(),
        }
    }
    producer.join().unwrap();
}

/// netd's side of a sleep as its event loop does it: announce, then arm a
/// watch that fails if the counter moved, then wait for the counter.
fn netd_sleep(area: &Area, bell: &AtomicU32, rx: &Consumer) -> bool {
    let Some(value) = area.prepare_netd_sleep(rx, area.carrier()) else { return false };
    // The kernel's watch: EAGAIN if the counter moved.
    if bell.load(Ordering::SeqCst) != value {
        area.netd_awake();
        return false;
    }
    Spin.wait(bell, value);
    area.netd_awake();
    true
}

#[test]
fn no_wakeup_is_lost() {
    const FRAMES: u32 = 20_000;
    let m = Memory::new();
    let done = Arc::new(AtomicBool::new(false));
    let producer = {
        let (m, done) = (m.clone(), done.clone());
        std::thread::spawn(move || {
            let area = m.area();
            let mut tx = area.rx_producer();
            for seq in 0..FRAMES {
                while !tx.push(&frame(seq, 64)) {
                    std::thread::yield_now();
                }
                area.ring_netd(&Spin);
                // Now and then a pause, so that the consumer sleeps.
                if seq % 64 == 0 {
                    std::thread::sleep(Duration::from_micros(50));
                }
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let area = m.area();
    let bell = unsafe { &*(m.ptr.add(m.channel.submission + ring::TAIL_OFFSET) as *const AtomicU32) };
    let mut rx = area.rx_consumer();
    let mut buf = vec![0u8; MAX_FRAME];
    let mut seq = 0;
    let mut sleeps = 0;
    while seq < FRAMES {
        match rx.pop(&mut buf).unwrap() {
            Some(64) => seq += 1,
            Some(n) => panic!("a frame of {n} bytes"),
            None => {
                let start = Instant::now();
                if netd_sleep(&area, bell, &rx) {
                    sleeps += 1;
                    // A sleep that ran into the stand-in's timeout while
                    // frames waited lost its wakeup.
                    assert!(start.elapsed() < Duration::from_secs(4) || rx.is_empty() && done.load(Ordering::SeqCst), "a wakeup was lost");
                }
            }
        }
    }
    producer.join().unwrap();
    assert!(sleeps > 0, "the consumer never slept: the test proved nothing");
}

#[test]
fn a_carrier_change_wakes_netd() {
    let m = Memory::new();
    let area = m.area();
    let rx = area.rx_consumer();
    assert!(!area.carrier());
    // Announced with the carrier down; the driver raises it before the
    // watch: the announcement sees it.
    area.set_carrier(true, &Spin);
    assert_eq!(area.prepare_netd_sleep(&rx, false), None);
    assert!(area.carrier());
}
