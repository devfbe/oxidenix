//! `TEST_CHANNEL`: the client's side of channels to the test service
//! (servers/ringtest, protocol `ring::selftest`), as the page cache will
//! use them with diskfs: create, connect, grant, move descriptors through
//! the rings with futex doorbells, revoke, and see the far end go.

use crate::syscall;
use core::sync::atomic::AtomicU32;
use restricted::*;
use ring::channel::{Header, Layout, SERVICE_GONE};
use ring::selftest::*;
use ring::{Consumer, Desc, Producer, Ring, Wait};

const PAGE: u64 = 4096;
/// How long a request may take before the test fails instead of hanging.
const TIMEOUT: u64 = 5_000_000_000;
const ENOENT: i64 = 2;
const EIO: i64 = 5;
const EBUSY: i64 = 16;
const EINVAL: i64 = 22;
const EACCES: i64 = 13;
const EPIPE: i64 = 32;
const ENODATA: i64 = 61;
const EPROTO: i64 = 71;
const EOPNOTSUPP: i64 = 95;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ETIMEDOUT: i64 = 110;
const ENOSPC: i64 = 28;
/// A paged object key the pager never answers (see `main::pager`).
const NEVER_SUPPLIED: u64 = 0x7e57 + 1;

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]) as u64
}

fn pause_ms(ms: u64) {
    syscall(SYS_SLEEP_UNTIL, [now() + ms * 1_000_000, 0, 0, 0, 0, 0]);
}

/// The doorbell: the kernel's futex on the ring word, which the service's
/// futex on its own mapping of the channel meets.
struct Doorbell {
    deadline: u64,
}

impl Wait for Doorbell {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let addr = word as *const AtomicU32 as u64;
        syscall(SYS_SERVER_FUTEX_WAIT, [addr, value as u64, self.deadline, 0, 0, 0]);
    }

    fn wake(&self, word: &AtomicU32) {
        syscall(SYS_SERVER_FUTEX_WAKE, [word as *const AtomicU32 as u64, 1, 0, 0, 0, 0]);
    }
}

struct Client {
    handle: u64,
    header: &'static Header,
    requests: Producer<'static, SLOTS>,
    completions: Consumer<'static, SLOTS>,
    tag: u64,
    /// The shared area (if the channel has one).
    shared: *mut u8,
}

impl Client {
    fn create() -> Result<Client, i64> {
        Client::with_shared(0)
    }

    /// A channel with `shared` pages of shared area.
    fn with_shared(shared: u64) -> Result<Client, i64> {
        let mut addr = 0u64;
        let h = syscall(SYS_CHAN_CREATE, [SLOTS as u64, &mut addr as *mut u64 as u64, shared, 0, 0, 0]);
        if h < 0 {
            return Err(h);
        }
        let layout = Layout::with_shared(SLOTS as u32, shared as u32).expect("a valid layout");
        let base = addr as *const u8;
        // Mapped until the handle is closed (`drop`).
        let (sub, comp) = unsafe { (layout.ring::<SLOTS>(base, layout.submission), layout.ring::<SLOTS>(base, layout.completion)) };
        Ok(Client {
            handle: h as u64,
            header: unsafe { Header::at(base) },
            requests: Ring::new(sub).producer(),
            completions: Ring::new(comp).consumer(),
            tag: 0,
            shared: (addr + layout.shared as u64) as *mut u8,
        })
    }

    /// The u32 at word `n` of the shared area's page `page` (mapped while
    /// the test runs).
    fn shared_word(&self, page: u64, n: u64) -> &'static AtomicU32 {
        unsafe { &*(self.shared.add((page * PAGE + n * 4) as usize) as *const AtomicU32) }
    }

    fn connect(&self, name: &str) -> i64 {
        syscall(SYS_CHAN_CONNECT, [self.handle, name.as_ptr() as u64, name.len() as u64, 0, 0, 0])
    }

    /// A channel connected to the test service.
    fn open() -> Result<Client, i64> {
        let c = Client::create()?;
        match c.connect(SERVICE) {
            0 => Ok(c),
            e => Err(e),
        }
    }

    fn grant(&self, object: u64, offset: u64, pages: u64, flags: u64) -> i64 {
        syscall(SYS_GRANT, [self.handle, object, offset, pages, flags, 0])
    }

    fn revoke(&self, grant: i64) -> i64 {
        syscall(SYS_REVOKE, [self.handle, grant as u64, 0, 0, 0, 0])
    }

    /// Sends a request and waits for its completion: its status, or EPIPE
    /// once the service is gone.
    fn call(&mut self, op: u16, grant: i64, buf_off: u64, len: u64, arg: u64) -> Result<i64, i64> {
        self.request(op, grant, buf_off, len, [arg, 0, 0])
    }

    /// `call` with all three arguments.
    fn request(&mut self, op: u16, grant: i64, buf_off: u64, len: u64, arg: [u64; 3]) -> Result<i64, i64> {
        self.tag += 1;
        let d = Desc { op, tag: self.tag, grant: grant as u32, buf_off: buf_off as u32, len: len as u32, arg, ..Desc::default() };
        if !self.requests.push(&d) {
            return Err(-ENOSPC);
        }
        let deadline = now() + TIMEOUT;
        let doorbell = Doorbell { deadline };
        self.requests.ring_doorbell(&doorbell);
        let header = self.header;
        match self.completions.pop_wait_while(&doorbell, || header.state() == 0 && now() < deadline) {
            Some(c) if c.tag == self.tag => Ok(c.arg[0] as i64),
            Some(_) => Err(-EPROTO),
            None if header.state() != 0 => Err(-EPIPE),
            None => Err(-ETIMEDOUT),
        }
    }

    /// `call`, with any failure to get a status as -1.
    fn status(&mut self, op: u16, grant: i64, buf_off: u64, len: u64, arg: u64) -> i64 {
        self.call(op, grant, buf_off, len, arg).unwrap_or(-1)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
    }
}

/// A memory object of `pages` pages, page n filled with `fill(n)`.
fn object(pages: u64, fill: impl Fn(u64) -> u8) -> Result<u64, i64> {
    let h = syscall(SYS_MO_CREATE, [pages, 0, 0, 0, 0, 0]);
    if h < 0 {
        return Err(h);
    }
    let mut page = [0u8; PAGE as usize];
    for n in 0..pages {
        page.fill(fill(n));
        syscall(SYS_MO_WRITE, [h as u64, n * PAGE, page.as_ptr() as u64, PAGE, 0, 0]);
    }
    Ok(h as u64)
}

fn close(handle: u64) {
    syscall(SYS_HANDLE_CLOSE, [handle, 0, 0, 0, 0, 0]);
}

/// Fails the scenario with check number `n` unless `cond`.
macro_rules! check {
    ($n:expr, $cond:expr) => {
        if !$cond {
            return Err($n);
        }
    };
}

pub fn run(scenario: u64) -> i64 {
    let result = match scenario {
        1 => rings(),
        2 => grants(),
        3 => revoking(),
        4 => client_end(),
        5 => service_death(),
        6 => service_exec(),
        7 => attached_without_answer(),
        8 => crash_loop(),
        _ => Err(1000),
    };
    match result {
        Ok(()) => 0,
        Err(n) => -n,
    }
}

/// Requests and completions through the rings, both ends sleeping on the
/// doorbells; the errors of connecting.
fn rings() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 1)?;
    for i in 0..200u64 {
        check!(2, c.status(ECHO, 0, 0, 0, i) == i as i64 + 1);
        if i % 50 == 0 {
            // Long enough for the service to go to sleep.
            pause_ms(5);
        }
    }
    check!(3, c.status(SLEEPS, 0, 0, 0, 0) > 0);
    check!(12, c.status(HEADER_READ_ONLY, 0, 0, 0, 0) == 0);
    check!(13, c.status(WATCH, 0, 0, 0, 0) == 0);
    check!(4, c.connect(SERVICE) == -EISCONN);
    let d = Client::create().map_err(|_| 5)?;
    check!(6, d.connect("nosuchservice") == -ENOENT);
    check!(7, d.connect(PLAIN) == -EOPNOTSUPP);
    let obj = object(1, |_| 0).map_err(|_| 8)?;
    check!(9, d.grant(obj, 0, 1, 0) == -ENOTCONN);
    close(obj);
    let mut addr = 0u64;
    for slots in [0, 1, 3, 8192] {
        check!(10, syscall(SYS_CHAN_CREATE, [slots, &mut addr as *mut u64 as u64, 0, 0, 0, 0]) == -EINVAL);
    }
    check!(50, syscall(SYS_CHAN_CREATE, [SLOTS as u64, &mut addr as *mut u64 as u64, 257, 0, 0, 0]) == -EINVAL);
    // The channel still works after all that.
    check!(11, c.status(ECHO, 0, 0, 0, 41) == 42);
    // No shared area unless asked for; one is both ends' memory.
    check!(51, c.status(SHARED, 0, 0, 0, 0) == 0);
    // (The service serves one channel at a time.)
    drop(c);
    shared_area()
}

/// A channel's shared area: what the client wrote is the service's to read,
/// what the service stores the client's, and a futex wake of the service's
/// on its mapping wakes the client's wait on its own.
fn shared_area() -> Result<(), i64> {
    use core::sync::atomic::Ordering;
    const VALUE: u64 = 0x5eed_0000;
    let mut c = Client::with_shared(3).map_err(|_| 52)?;
    for page in 0..3 {
        c.shared_word(page, 0).store(VALUE as u32 + page as u32, Ordering::Release);
    }
    check!(53, c.connect(SERVICE) == 0);
    check!(54, c.request(SHARED, 0, 0, 0, [3, VALUE, 0]) == Ok(0));
    let word = c.shared_word(0, 1);
    check!(55, word.load(Ordering::Acquire) == !(VALUE as u32));
    word.store(0, Ordering::Release);
    // The service stores 50 ms after its answer and wakes: one wait with a
    // deadline of two seconds must end early (the wake met it), unless the
    // store came even before the wait.
    check!(56, c.request(SHARED, 0, 0, 0, [3, VALUE, 1]) == Ok(0));
    let start = now();
    syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, 0, start + 2_000_000_000, 0, 0, 0]);
    check!(57, word.load(Ordering::Acquire) == !(VALUE as u32) && now() - start < 1_000_000_000);
    Ok(())
}

/// Grants: what the service reads and writes is the object's memory;
/// read-only stays read-only; every bound is checked; granted pages are
/// pinned.
fn grants() -> Result<(), i64> {
    let obj = object(4, |n| n as u8 * 16 + 1).map_err(|_| 20)?;
    let mut c = Client::open().map_err(|_| 21)?;
    let g = c.grant(obj, 0, 4, GRANT_WRITE);
    check!(22, g > 0);
    check!(23, c.status(READ, g, PAGE, PAGE, 0) == 17 * PAGE as i64);
    check!(24, c.status(WRITE, g, 2 * PAGE + 100, 50, 0x5a) == 50);
    let mut buf = [0u8; 52];
    syscall(SYS_MO_READ, [obj, 2 * PAGE + 99, buf.as_mut_ptr() as u64, 52, 0, 0]);
    check!(25, buf[0] == 33 && buf[1..51].iter().all(|&b| b == 0x5a) && buf[51] == 33);
    check!(26, c.status(READ, g, 4 * PAGE - 10, 20, 0) == -EINVAL);
    let ro = c.grant(obj, PAGE, 1, 0);
    check!(27, ro > 0 && ro != g);
    check!(28, c.status(PROBE_READ_ONLY, ro, 0, 0, 0) == 0);
    check!(29, c.status(WRITE, ro, 0, 1, 1) == -EACCES);
    check!(30, c.status(READ, ro, 0, PAGE, 0) == 17 * PAGE as i64);
    // The kernel's bounds.
    check!(31, c.grant(obj, 0, 5, 0) == -EINVAL);
    check!(32, c.grant(obj, 4 * PAGE, 1, 0) == -EINVAL);
    check!(33, c.grant(obj, 100, 1, 0) == -EINVAL);
    check!(34, c.grant(obj, 0, 1, 2) == -EINVAL);
    check!(35, c.grant(c.handle, 0, 1, 0) == -EINVAL);
    check!(36, c.grant(obj, 0, 0, 0) == -EINVAL);
    check!(37, c.grant(obj, u64::MAX & !(PAGE - 1), 1, 0) == -EINVAL);
    // A service maps a grant only within the limit it gives (E2BIG before
    // anything is mapped, with the grant's size), and then at that size.
    let big = c.grant(obj, 0, 4, 0);
    check!(138, big > 0 && c.status(MAP_LIMITED, big, 0, 4, 3) == 0);
    check!(139, c.status(MAP_LIMITED, big, 0, 4, 4) == 1 && c.status(READ, big, 0, PAGE, 0) == PAGE as i64);
    // Device addresses: within the grant only.
    let a = c.status(DMA, g, PAGE + 5, 0, 0);
    check!(38, a > 0 && a % PAGE as i64 == 5);
    check!(39, c.status(DMA, g, 4 * PAGE, 0, 0) == -EINVAL);
    check!(40, c.status(DMA, 999, 0, 0, 0) == -ENOENT);
    check!(41, c.status(DMA_UNMAP, g, 0, 0, 0) == 0);
    // Granted pages stay the object's: no truncation over them.
    let f = syscall(SYS_MO_CREATE_FILE, [0; 6]);
    check!(42, f > 0);
    let f = f as u64;
    let page = [7u8; PAGE as usize];
    for n in 0..2 {
        syscall(SYS_MO_WRITE, [f, n * PAGE, page.as_ptr() as u64, PAGE, 0, 0]);
    }
    let fg = c.grant(f, PAGE, 1, GRANT_WRITE);
    check!(43, fg > 0);
    check!(44, syscall(SYS_MO_TRUNCATE, [f, PAGE, 0, 0, 0, 0]) == -EBUSY);
    check!(45, syscall(SYS_MO_TRUNCATE, [f, 3 * PAGE, 0, 0, 0, 0]) == 0);
    check!(46, c.revoke(fg) == 0);
    check!(47, syscall(SYS_MO_TRUNCATE, [f, 0, 0, 0, 0, 0]) == 0);
    // A page a pager never supplied cannot be granted.
    let p = syscall(SYS_MO_CREATE_PAGED, [1, NEVER_SUPPLIED, 0, 0, 0, 0]);
    check!(48, p > 0);
    check!(49, c.grant(p as u64, 0, 1, 0) == -ENODATA);
    for h in [obj, f, p as u64] {
        close(h);
    }
    Ok(())
}

/// Revoking: the service's mapping goes at once; a grant with device
/// addresses stays pinned (its id taken) until the service lets go.
fn revoking() -> Result<(), i64> {
    let obj = object(2, |_| 3).map_err(|_| 60)?;
    let mut c = Client::open().map_err(|_| 61)?;
    let g = c.grant(obj, 0, 2, GRANT_WRITE);
    check!(62, g > 0 && c.status(READ, g, 0, 1, 0) == 3);
    check!(63, c.revoke(g) == 0);
    check!(64, c.status(CHECK_GONE, g, 0, 0, 0) == 0);
    check!(65, c.revoke(g) == -EINVAL);
    check!(66, c.status(READ, g, 0, 1, 0) == -ENOENT);
    // The id comes back.
    let f = syscall(SYS_MO_CREATE_FILE, [0; 6]);
    check!(67, f > 0);
    let f = f as u64;
    let page = [9u8; PAGE as usize];
    syscall(SYS_MO_WRITE, [f, 0, page.as_ptr() as u64, PAGE, 0, 0]);
    let g2 = c.grant(f, 0, 1, GRANT_WRITE);
    check!(68, g2 == g);
    check!(69, c.status(DMA, g2, 0, 0, 0) > 0);
    check!(70, c.revoke(g2) == REVOKE_DRAINING as i64);
    // Draining: still pinned, its id still taken, no new device address.
    check!(71, syscall(SYS_MO_TRUNCATE, [f, 0, 0, 0, 0, 0]) == -EBUSY);
    let g3 = c.grant(obj, PAGE, 1, 0);
    check!(72, g3 > 0 && g3 != g2);
    check!(73, c.status(DMA, g2, 0, 0, 0) == -ENOENT);
    check!(74, c.status(DMA_UNMAP, g2, 0, 0, 0) == 0);
    check!(75, syscall(SYS_MO_TRUNCATE, [f, 0, 0, 0, 0, 0]) == 0);
    check!(76, c.grant(obj, 0, 1, 0) == g2);
    close(obj);
    close(f);
    Ok(())
}

/// The client's end goes (its handle closed) while the service sleeps:
/// the service wakes, finds every grant gone from its memory, and the
/// pins are released.
fn client_end() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 80)?;
    let before = c.status(CLEAN_ENDS, 0, 0, 0, 0);
    check!(81, before >= 0);
    let f = syscall(SYS_MO_CREATE_FILE, [0; 6]);
    check!(82, f > 0);
    let f = f as u64;
    let page = [5u8; PAGE as usize];
    syscall(SYS_MO_WRITE, [f, 0, page.as_ptr() as u64, PAGE, 0, 0]);
    let g = c.grant(f, 0, 1, GRANT_WRITE);
    check!(83, g > 0 && c.status(WRITE, g, 0, 8, 6) == 8);
    pause_ms(5);
    drop(c);
    // Pins released by the time the close returned.
    check!(84, syscall(SYS_MO_TRUNCATE, [f, 0, 0, 0, 0, 0]) == 0);
    close(f);
    let mut c = Client::open().map_err(|_| 85)?;
    check!(86, c.status(CLEAN_ENDS, 0, 0, 0, 0) == before + 1);
    Ok(())
}

/// The service dies (of a store into a read-only grant) while the client
/// waits for the completion: the client wakes and sees it, the page is
/// unchanged, the channel refuses new grants, and a new channel starts the
/// service again.
fn service_death() -> Result<(), i64> {
    let obj = object(1, |_| b'k').map_err(|_| 90)?;
    let mut c = Client::open().map_err(|_| 91)?;
    let ro = c.grant(obj, 0, 1, 0);
    check!(92, ro > 0);
    check!(93, c.call(CRASH, ro, 0, 0, 0) == Err(-EPIPE));
    check!(94, c.header.state() & SERVICE_GONE != 0);
    let mut byte = [0u8; 1];
    syscall(SYS_MO_READ, [obj, 0, byte.as_mut_ptr() as u64, 1, 0, 0]);
    check!(95, byte[0] == b'k');
    check!(96, c.grant(obj, 0, 1, 0) == -EPIPE);
    check!(97, c.revoke(ro) == 0);
    drop(c);
    let mut c = Client::open().map_err(|_| 98)?;
    check!(99, c.status(ECHO, 0, 0, 0, 1) == 2);
    close(obj);
    Ok(())
}

/// The service dies at once every time it is used (a crash loop), under
/// the kernel's restart policy (ADR 0006): restarted with a growing backoff
/// (0.1, 0.2, 0.4, 0.8, 1.6 s after its young deaths), then down after
/// the sixth young death in a row (EIO at once), and up again once the
/// 5 s cooldown is over. What it counts on is only that its deaths are
/// young (milliseconds after each restart, against half a second) and the
/// incarnation it starts with young or not: no runner is that slow.
fn crash_loop() -> Result<(), i64> {
    let obj = object(1, |_| b'k').map_err(|_| 130)?;
    let start = now();
    let mut cycles = 0;
    let refused = loop {
        let mut c = match Client::open() {
            Ok(c) => c,
            Err(e) => break e,
        };
        check!(131, cycles < 10);
        let ro = c.grant(obj, 0, 1, 0);
        check!(132, ro > 0 && c.call(CRASH, ro, 0, 0, 0) == Err(-EPIPE));
        cycles += 1;
    };
    let elapsed = now() - start;
    // Opened: the incarnation it found, and a restart after each young
    // death but the sixth (five), plus one at once if the first death
    // ended a long life (the usual case: so seven, else six).
    check!(133, refused == -EIO && (6..=7).contains(&cycles));
    // The five backoffs: 3.1 s at least.
    check!(134, elapsed >= 3_100_000_000);
    // Down: refused at once, no restart.
    let t = now();
    check!(135, Client::open().err() == Some(-EIO) && now() - t < 500_000_000);
    // Up again after the cooldown, with a clean history.
    pause_ms(5_200);
    let mut c = Client::open().map_err(|_| 136)?;
    check!(137, c.status(ECHO, 0, 0, 0, 1) == 2);
    close(obj);
    Ok(())
}

/// The service attaches a channel but answers the offer only after it
/// served it: the connect is complete once the service attached.
fn attached_without_answer() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 120)?;
    check!(121, c.status(ANSWER_LATE, 0, 0, 0, 0) == 0);
    drop(c);
    let mut c = Client::open().map_err(|_| 122)?;
    check!(123, c.status(ECHO, 0, 0, 0, 5) == 6);
    Ok(())
}

/// The service executes a new program: that ends its end of the channel
/// (the client wakes), and the new program can neither map the grant nor
/// get a device address of it.
fn service_exec() -> Result<(), i64> {
    let obj = object(1, |_| b'k').map_err(|_| 110)?;
    let mut c = Client::open().map_err(|_| 111)?;
    let g = c.grant(obj, 0, 1, GRANT_WRITE);
    check!(112, g > 0 && c.status(READ, g, 0, 1, 0) == b'k' as i64);
    check!(113, c.call(EXEC, g, 0, 0, 0) == Err(-EPIPE));
    // Time for the new program to try (and end).
    pause_ms(300);
    let mut byte = [0u8; 1];
    syscall(SYS_MO_READ, [obj, 0, byte.as_mut_ptr() as u64, 1, 0, 0]);
    check!(114, byte[0] == b'k');
    check!(115, c.revoke(g) == 0);
    drop(c);
    let mut c = Client::open().map_err(|_| 116)?;
    check!(117, c.status(ECHO, 0, 0, 0, 1) == 2);
    close(obj);
    Ok(())
}
