//! ringtest: the far end of the self-tests' channels (test mode only). It
//! accepts channel offers and serves one channel at a time over its rings
//! (`ring::selftest`), as a device server will: descriptors are copied out
//! of the ring once and validated, buffers are grants it maps, device
//! addresses come from `grant_dma`. It checks from the service's side what
//! the kernel promises: read-only grants stay read-only, revoked and torn
//! down grants are gone from its memory.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use oxrt::println;
use ring::channel::{Header, Layout, Offer};
use ring::selftest::*;
use ring::{Desc, Ring, Wait};

oxrt::entry!(main);

const EINVAL: i64 = 22;
const EACCES: i64 = 13;
const ENOENT: i64 = 2;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;

/// Channels that ended with their client gone and every grant mapping of
/// theirs removed (`CLEAN_ENDS`), and sleeps on a doorbell (`SLEEPS`).
static CLEAN: AtomicI64 = AtomicI64::new(0);
static SLEPT: AtomicU32 = AtomicU32::new(0);
/// `ANSWER_LATE`: the next offer is answered after its channel ends.
static LATE: AtomicU32 = AtomicU32::new(0);

/// The doorbell: a futex on the ring word, shared with the client.
struct Futex;

impl Wait for Futex {
    fn wait(&self, word: &AtomicU32, value: u32) {
        SLEPT.fetch_add(1, Ordering::Relaxed);
        // EPIPE once the client is gone: the caller looks at the state.
        let _ = oxrt::futex_wait(word, value, None);
    }

    fn wake(&self, word: &AtomicU32) {
        let _ = oxrt::futex_wake(word, 1);
    }
}

/// A grant as mapped here.
struct Mapped {
    grant: u32,
    addr: *mut u8,
    len: u64,
    writable: bool,
}

/// Value of `key=...` among the arguments.
fn arg(args: &[&str], key: &str) -> Option<u64> {
    args.iter().find_map(|a| a.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

/// The program after `EXEC`: tries to reach the grant of the channel the
/// process served before, then ends (the kernel starts the service again
/// for the next channel).
fn after_exec(args: &[&str]) -> i32 {
    let (Some(channel), Some(grant)) = (arg(args, "channel"), arg(args, "grant")) else { return 2 };
    if let Ok((addr, _, true)) = oxrt::grant_map(channel, grant as u32) {
        unsafe { addr.write_volatile(AFTER_EXEC) };
    }
    let _ = oxrt::grant_dma(channel, grant as u32, 0);
    0
}

/// execve(2) of this program with `args`.
fn exec_self(args: &[&str]) -> i64 {
    const EXECVE: u64 = 59;
    let path = b"/sbin/ringtest\0";
    let strings: Vec<Vec<u8>> = args.iter().map(|a| a.bytes().chain(core::iter::once(0)).collect()).collect();
    let mut argv: Vec<u64> = strings.iter().map(|s| s.as_ptr() as u64).collect();
    argv.push(0);
    let envp = [0u64];
    oxrt::syscall(EXECVE, [path.as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64, 0, 0, 0])
}

fn main(args: Vec<&'static str>) -> i32 {
    if args.contains(&"after-exec") {
        return after_exec(&args);
    }
    if let Err(e) = oxrt::ipc_register_with(SERVICE, 0, oxrt::IPC_CHANNELS) {
        println!("ringtest: cannot register: {}", e);
        return 1;
    }
    let mut message = [0u8; 64];
    loop {
        let (id, len) = match oxrt::ipc_receive(&mut message, None) {
            Ok(oxrt::Event::Control(id, len)) => (id, len),
            Ok(oxrt::Event::Request(id, _)) => {
                let _ = oxrt::ipc_reply(id, &(-EINVAL).to_le_bytes());
                continue;
            }
            _ => continue,
        };
        let offer = Offer::decode(&message[..len]).filter(|o| o.slots as usize == SLOTS);
        let attached = match offer {
            Some(o) => oxrt::chan_attach(o.channel).map(|base| (o, base)),
            None => Err(-EINVAL),
        };
        let status = attached.as_ref().map_or_else(|&e| e, |_| 0);
        let late = attached.is_ok() && LATE.swap(0, Ordering::Relaxed) != 0;
        if !late {
            let _ = oxrt::ipc_reply(id, &status.to_le_bytes());
        }
        if let Ok((offer, base)) = attached {
            serve(&offer, base);
        }
        if late {
            // Nobody waits for it any more.
            let _ = oxrt::ipc_reply(id, &status.to_le_bytes());
        }
    }
}

/// Serves the offered channel (mapped at `base`) until its client is gone.
fn serve(offer: &Offer, base: *mut u8) {
    let channel = offer.channel;
    let layout = offer.layout().expect("decode checked it");
    // The channel stays mapped until chan_detach below.
    let (sub, comp) = unsafe { (layout.ring::<SLOTS>(base, layout.submission), layout.ring::<SLOTS>(base, layout.completion)) };
    let header = unsafe { Header::at(base) };
    let (sub, comp) = (Ring::new(sub), Ring::new(comp));
    let (mut requests, mut completions) = (sub.consumer(), comp.producer());
    let mut mapped: Vec<Mapped> = Vec::new();
    while let Some(d) = requests.pop_wait_while(&Futex, || header.state() == 0) {
        let status = handle(channel, base, &layout, &d, &mut mapped);
        let reply = Desc { tag: d.tag, arg: [status as u64, 0, 0], ..Desc::default() };
        while !completions.push(&reply) {
            if header.state() != 0 {
                break;
            }
            oxrt::sched_yield();
        }
        completions.ring_doorbell(&Futex);
        if d.op == SHARED && status == 0 && d.arg[2] == 1 && layout.shared_pages > 0 {
            // The late store, with a wake the client's wait must meet.
            let _ = oxrt::futex_wait(&AtomicU32::new(0), 0, Some(50));
            let word = unsafe { &*(base.add(layout.shared + 4) as *const AtomicU32) };
            word.store(!(d.arg[1] as u32), Ordering::Release);
            let _ = oxrt::futex_wake(word, 1);
        }
    }
    // The client is gone: the kernel must have taken every grant back.
    if mapped.iter().all(|m| gone(m.addr)) {
        CLEAN.fetch_add(1, Ordering::Relaxed);
    }
    let _ = oxrt::chan_detach(channel);
}

/// Whether the grant mapped at `addr` is out of reach: its range is the
/// inaccessible reservation a revoke leaves (no access can be added:
/// EACCES), or nothing at all (ENOMEM).
fn gone(addr: *mut u8) -> bool {
    matches!(oxrt::mprotect(addr, PAGE as usize, PROT_READ), Err(e) if e == -ENOMEM || e == -EACCES)
}

/// The grant `grant`, mapped (once).
fn map(channel: u64, grant: u32, mapped: &mut Vec<Mapped>) -> Result<usize, i64> {
    if let Some(i) = mapped.iter().position(|m| m.grant == grant) {
        return Ok(i);
    }
    let (addr, pages, writable) = oxrt::grant_map(channel, grant)?;
    mapped.push(Mapped { grant, addr, len: pages * PAGE, writable });
    Ok(mapped.len() - 1)
}

/// The range of `d` within its grant (EINVAL beyond it).
fn range(m: &Mapped, d: &Desc) -> Result<*mut u8, i64> {
    let end = (d.buf_off as u64).checked_add(d.len as u64).ok_or(-EINVAL)?;
    if end > m.len {
        return Err(-EINVAL);
    }
    Ok(unsafe { m.addr.add(d.buf_off as usize) })
}

fn handle(channel: u64, base: *mut u8, layout: &Layout, d: &Desc, mapped: &mut Vec<Mapped>) -> i64 {
    let result: Result<i64, i64> = (|| match d.op {
        ECHO => Ok(d.arg[0] as i64 + 1),
        SHARED => {
            if layout.shared_pages as u64 != d.arg[0] {
                return Ok(1);
            }
            for n in 0..layout.shared_pages {
                let word = unsafe { &*(base.add(layout.shared + n * PAGE as usize) as *const AtomicU32) };
                if word.load(Ordering::Acquire) != (d.arg[1] as u32).wrapping_add(n as u32) {
                    return Ok(2);
                }
            }
            if d.arg[2] != 1 && layout.shared_pages > 0 {
                let word = unsafe { &*(base.add(layout.shared + 4) as *const AtomicU32) };
                word.store(!(d.arg[1] as u32), Ordering::Release);
            }
            Ok(0)
        }
        READ => {
            let i = map(channel, d.grant, mapped)?;
            let m = &mapped[i];
            let p = range(m, d)?;
            Ok((0..d.len as usize).map(|i| unsafe { p.add(i).read_volatile() } as i64).sum())
        }
        WRITE => {
            let i = map(channel, d.grant, mapped)?;
            let m = &mapped[i];
            if !m.writable {
                return Err(-EACCES);
            }
            let p = range(m, d)?;
            for i in 0..d.len as usize {
                unsafe { p.add(i).write_volatile(d.arg[0] as u8) };
            }
            Ok(d.len as i64)
        }
        PROBE_READ_ONLY => {
            let i = map(channel, d.grant, mapped)?;
            let m = &mapped[i];
            let len = m.len as usize;
            if oxrt::mprotect(m.addr, len, PROT_READ | PROT_WRITE) != Err(-EACCES) {
                return Ok(1);
            }
            if oxrt::mprotect(m.addr, len, PROT_READ | PROT_EXEC) != Err(-EACCES) {
                return Ok(2);
            }
            // The kernel stores a timespec there for us: it must refuse.
            const CLOCK_MONOTONIC: u64 = 1;
            if oxrt::syscall(oxrt::sys::CLOCK_GETTIME, [CLOCK_MONOTONIC, m.addr as u64, 0, 0, 0, 0]) != -EFAULT {
                return Ok(3);
            }
            let _ = unsafe { m.addr.read_volatile() };
            Ok(0)
        }
        DMA => oxrt::grant_dma(channel, d.grant, d.buf_off as u64).map(|a| a as i64),
        DMA_UNMAP => oxrt::grant_dma_unmap(channel, d.grant).map(|_| 0),
        CHECK_GONE => {
            let Some(i) = mapped.iter().position(|m| m.grant == d.grant) else { return Err(-EINVAL) };
            let m = mapped.swap_remove(i);
            if oxrt::grant_map(channel, d.grant).err() != Some(-ENOENT) {
                return Ok(1);
            }
            let len = m.len as usize;
            if oxrt::mprotect(m.addr, len, PROT_READ) != Err(-EACCES) {
                return Ok(2);
            }
            // The reservation goes only when the service unmaps it.
            if oxrt::munmap(m.addr, len).is_err() || oxrt::mprotect(m.addr, len, PROT_READ) != Err(-ENOMEM) {
                return Ok(3);
            }
            Ok(0)
        }
        CRASH => {
            let i = map(channel, d.grant, mapped)?;
            let m = &mapped[i];
            unsafe { m.addr.write_volatile(0x42) };
            // Not reached: the store is the service's end.
            Ok(-1)
        }
        CLEAN_ENDS => Ok(CLEAN.load(Ordering::Relaxed)),
        EXEC => {
            let channel = alloc::format!("channel={}", channel);
            let grant = alloc::format!("grant={}", d.grant);
            Ok(exec_self(&["ringtest", "after-exec", &channel, &grant]))
        }
        SLEEPS => Ok(SLEPT.load(Ordering::Relaxed) as i64),
        HEADER_READ_ONLY => {
            if oxrt::mprotect(base, PAGE as usize, PROT_READ | PROT_WRITE) != Err(-EACCES) {
                return Ok(1);
            }
            const CLOCK_MONOTONIC: u64 = 1;
            if oxrt::syscall(oxrt::sys::CLOCK_GETTIME, [CLOCK_MONOTONIC, base as u64, 0, 0, 0, 0]) != -EFAULT {
                return Ok(2);
            }
            Ok(0)
        }
        WATCH => {
            let layout = Layout::new(SLOTS as u32).expect("a valid slot count");
            let tail = unsafe { &*(base.add(layout.submission + ring::TAIL_OFFSET) as *const AtomicU32) };
            let other = unsafe { &*(base.add(layout.submission + ring::SLEEPING_OFFSET) as *const AtomicU32) };
            let value = tail.load(Ordering::Acquire);
            for _ in 0..2 {
                if oxrt::chan_watch(channel, value) != Ok(true) {
                    return Ok(1);
                }
            }
            // FUTEX_REQUEUE (shared): wake none, move all to another word.
            const FUTEX_REQUEUE: u64 = 3;
            let moved = oxrt::syscall(oxrt::sys::FUTEX, [tail as *const _ as u64, FUTEX_REQUEUE, 0, i32::MAX as u64, other as *const _ as u64, 0]);
            if moved != 0 {
                return Ok(2);
            }
            if oxrt::futex_wake(tail, i32::MAX as u32) != Ok(1) {
                return Ok(3);
            }
            let mut buf = [0u8; 64];
            match oxrt::ipc_receive(&mut buf, Some(1000)) {
                Ok(oxrt::Event::Doorbell) => Ok(0),
                _ => Ok(4),
            }
        }
        ANSWER_LATE => {
            LATE.store(1, Ordering::Relaxed);
            Ok(0)
        }
        _ => Err(-EINVAL),
    })();
    result.unwrap_or_else(|e| e)
}
