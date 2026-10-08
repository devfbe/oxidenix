//! The ring's invariants (docs/design/io-rings.md): FIFO order, a full
//! ring refuses, positions wrap modulo 2^32, a consumer on another thread
//! sees every entry's contents, and the doorbell protocol loses no wakeup.

use ring::{Desc, Ring, RingMemory, Wait};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

fn desc(tag: u64) -> Desc {
    Desc { tag, offset: tag.wrapping_mul(7), len: tag as u32, ..Desc::default() }
}

#[test]
fn entries_come_out_in_order_and_a_full_ring_refuses() {
    let mem = RingMemory::<8>::new();
    let ring = Ring::new(&mem);
    let (mut p, mut c) = (ring.producer(), ring.consumer());
    for i in 0..8 {
        assert!(p.push(&desc(i)), "room for {i}");
    }
    assert!(!p.push(&desc(99)), "a full ring refuses");
    for i in 0..8 {
        assert_eq!(c.pop().map(|d| d.tag), Some(i));
    }
    assert_eq!(c.pop().map(|d| d.tag), None);
}

#[test]
fn positions_wrap_around_the_counter() {
    let mem = RingMemory::<4>::new();
    // Start just below the wrap of the 32-bit positions.
    mem.set_positions(u32::MAX - 2);
    let ring = Ring::new(&mem);
    let (mut p, mut c) = (ring.producer(), ring.consumer());
    for round in 0..10u64 {
        for i in 0..4 {
            assert!(p.push(&desc(round * 4 + i)));
        }
        assert!(!p.push(&desc(0)));
        for i in 0..4 {
            assert_eq!(c.pop().map(|d| d.tag), Some(round * 4 + i));
        }
    }
}

#[test]
fn a_consumer_on_another_thread_sees_every_entry_and_its_data() {
    const N: u64 = 200_000;
    let mem = Arc::new(RingMemory::<64>::new());
    // Data "in a granted buffer": written before the entry is published.
    // A buffer is the producer's again once the consumer is done with it
    // (in the protocol: its completion), here when `done` passes it.
    let data: Arc<Vec<AtomicU32>> = Arc::new((0..64).map(|_| AtomicU32::new(0)).collect());
    let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let consumer = {
        let (mem, data, done) = (mem.clone(), data.clone(), done.clone());
        thread::spawn(move || {
            let ring = Ring::new(&mem);
            let mut c = ring.consumer();
            let mut next = 0;
            while next < N {
                if let Some(d) = c.pop() {
                    assert_eq!(d.tag, next, "in order");
                    assert_eq!(d.offset, next.wrapping_mul(7), "the whole entry");
                    // Relaxed: the ring's Acquire must order it.
                    assert_eq!(data[(next % 64) as usize].load(Ordering::Relaxed), next as u32);
                    next += 1;
                    done.store(next, Ordering::Release);
                } else {
                    std::hint::spin_loop();
                }
            }
        })
    };
    let ring = Ring::new(&mem);
    let mut p = ring.producer();
    let mut i = 0;
    while i < N {
        if p.room() > 0 && i - done.load(Ordering::Acquire) < 64 {
            data[(i % 64) as usize].store(i as u32, Ordering::Relaxed);
            assert!(p.push(&desc(i)));
            i += 1;
        } else {
            std::hint::spin_loop();
        }
    }
    consumer.join().unwrap();
}

/// A futex stand-in: sleep while the word holds a value, woken by `wake`.
struct TestWait {
    lock: Mutex<()>,
    cond: Condvar,
    sleeps: AtomicUsize,
}

impl Wait for TestWait {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let guard = self.lock.lock().unwrap();
        if word.load(Ordering::SeqCst) == value {
            self.sleeps.fetch_add(1, Ordering::Relaxed);
            // Every push rings the doorbell: a sleep this long with nothing
            // new is a lost wakeup (a failure, not a hang).
            let (_guard, timeout) = self.cond.wait_timeout(guard, std::time::Duration::from_secs(5)).unwrap();
            assert!(!(timeout.timed_out() && word.load(Ordering::SeqCst) == value), "lost wakeup");
        }
    }

    fn wake(&self, _word: &AtomicU32) {
        let _guard = self.lock.lock().unwrap();
        self.cond.notify_all();
    }
}

#[test]
fn the_doorbell_loses_no_wakeup() {
    const N: u64 = 50_000;
    let mem = Arc::new(RingMemory::<16>::new());
    let wait = Arc::new(TestWait { lock: Mutex::new(()), cond: Condvar::new(), sleeps: AtomicUsize::new(0) });
    let consumer = {
        let (mem, wait) = (mem.clone(), wait.clone());
        thread::spawn(move || {
            let ring = Ring::new(&mem);
            let mut c = ring.consumer();
            for i in 0..N {
                // Sleeps whenever the ring is empty: every push must wake it.
                let d = c.pop_wait(&*wait);
                assert_eq!(d.tag, i);
            }
        })
    };
    let ring = Ring::new(&mem);
    let mut p = ring.producer();
    for i in 0..N {
        while !p.push(&desc(i)) {
            std::hint::spin_loop();
        }
        p.ring_doorbell(&*wait);
        if i % 1000 == 0 {
            thread::yield_now();
        }
    }
    consumer.join().unwrap();
    assert!(wait.sleeps.load(Ordering::Relaxed) > 0, "the consumer slept at least once");
}

/// An event loop's sleep (`prepare_sleep`, sleeping elsewhere, `awake`)
/// loses no wakeup either: the loop sleeps on the tail word only while it
/// holds the value `prepare_sleep` returned, as a futex (or the kernel's
/// doorbell watch) compares it.
#[test]
fn an_event_loop_sleep_loses_no_wakeup() {
    const N: u64 = 50_000;
    let mem = Arc::new(RingMemory::<16>::new());
    let wait = Arc::new(TestWait { lock: Mutex::new(()), cond: Condvar::new(), sleeps: AtomicUsize::new(0) });
    let consumer = {
        let (mem, wait) = (mem.clone(), wait.clone());
        thread::spawn(move || {
            let ring = Ring::new(&mem);
            let mut c = ring.consumer();
            let tail = |m: &RingMemory<16>| unsafe { &*(m as *const RingMemory<16> as *const u8).add(ring::TAIL_OFFSET).cast::<AtomicU32>() };
            let mut next = 0;
            while next < N {
                if let Some(d) = c.pop() {
                    assert_eq!(d.tag, next);
                    next += 1;
                    continue;
                }
                if !c.is_empty() {
                    continue;
                }
                if let Some(value) = c.prepare_sleep() {
                    wait.wait(tail(&mem), value);
                }
                c.awake();
            }
        })
    };
    let ring = Ring::new(&mem);
    let mut p = ring.producer();
    for i in 0..N {
        while !p.push(&desc(i)) {
            std::hint::spin_loop();
        }
        p.ring_doorbell(&*wait);
        if i % 1000 == 0 {
            thread::yield_now();
        }
    }
    consumer.join().unwrap();
    assert!(wait.sleeps.load(Ordering::Relaxed) > 0, "the consumer slept at least once");
}

#[test]
fn a_sleeper_wakes_when_the_peer_is_gone() {
    use std::sync::atomic::AtomicBool;
    let mem = Arc::new(RingMemory::<8>::new());
    let wait = Arc::new(TestWait { lock: Mutex::new(()), cond: Condvar::new(), sleeps: AtomicUsize::new(0) });
    let gone = Arc::new(AtomicBool::new(false));
    let consumer = {
        let (mem, wait, gone) = (mem.clone(), wait.clone(), gone.clone());
        thread::spawn(move || {
            let ring = Ring::new(&mem);
            let mut c = ring.consumer();
            let first = c.pop_wait_while(&*wait, || !gone.load(Ordering::SeqCst));
            let second = c.pop_wait_while(&*wait, || !gone.load(Ordering::SeqCst));
            (first.map(|d| d.tag), second)
        })
    };
    let ring = Ring::new(&mem);
    let mut p = ring.producer();
    assert!(p.push(&desc(7)));
    p.ring_doorbell(&*wait);
    while wait.sleeps.load(Ordering::Relaxed) == 0 {
        thread::yield_now();
    }
    // What a channel's teardown does: mark the peer gone, then wake.
    gone.store(true, Ordering::SeqCst);
    wait.wake(&AtomicU32::new(0));
    let (first, second) = consumer.join().unwrap();
    assert_eq!(first, Some(7));
    assert_eq!(second, None);
    // The sleep flag is cleared again.
    assert_eq!(unsafe { *(Arc::as_ptr(&mem) as *const u8).add(ring::SLEEPING_OFFSET) }, 0);
}
