//! The server's heap: one allocator for every thread of the instance, in
//! the heap area of the shared region (`pageheap`: page runs and slabs of
//! size classes over an arena whose pages are committed and decommitted
//! with the kernel's `SYS_SHARED_COMMIT` and `SYS_SHARED_DECOMMIT`). Its
//! metadata lies at `HEAP_BASE`, the arena after it.
//!
//! Memory goes back: when the heap holds more free committed memory than it
//! keeps, the timer thread trims it (at most every `TRIM_INTERVAL`, so a
//! workload that frees and allocates in turn does not commit and decommit
//! all the time), and the service thread gives back all of it when the
//! kernel says memory is short (`EVENT_SHRINK`, `shrink`). Its locks are
//! the server's futex mutex (`sync::RawMutex`, in the image: never in
//! memory the heap decommits).
//!
//! **Which allocations may fail.** A failed allocation the server cannot
//! answer breaks its whole tree (the global allocator's null is a panic, a
//! `ud2`, `break_instance`). So the rule is:
//!
//! - Memory a program's call makes the server hold whose size or number
//!   the program chooses, or which grows with what programs do (queued
//!   data, buffers, copies of the program's arguments and paths, listings,
//!   descriptors, watches, queued signals, records of what programs made)
//!   is allocated *fallibly*, through this module's helpers (`TryGrow`,
//!   `try_box`, `try_arc`, `try_to_vec`, `try_format`, `preload` before a
//!   `BTreeMap` insert, which has no fallible form), and the call answers
//!   ENOMEM (or what Linux answers there: ENOBUFS for a socket's send,
//!   EAGAIN where Linux does), with nothing of it left behind.
//! - The rest may stay infallible: small allocations of fixed size on
//!   paths no program makes grow without bound (the instance's start,
//!   the service threads, a call's fixed bookkeeping whose total another
//!   limit bounds), and what must not fail at all (an exit, a close, a
//!   wake: what frees memory).
//!
//! The two take memory from different places. A fallible allocation runs
//! in a `fallible` scope, in which the heap commits with
//! `COMMIT_NO_RESERVE`: against the commit limit only, never from the
//! instance's reserve (up to 2 MiB of commitment its other allocations
//! fall back on), so no program can drain what the infallible ones rely on.
//! A `BTreeMap` insert can only be made likely to succeed: `preload` makes
//! sure, fallibly, that the heap has a free committed page beyond the
//! reserve for its nodes (Linux's `radix_tree_preload`), so a program's
//! inserts never take the reserve but by a race with another thread.
//! The helpers' failures are `NoMem`, ENOMEM as an `i64` with `?`.
//!
//! The test hook `TEST_FAIL_NTH` makes the calling thread's n-th fallible
//! allocation fail (Linux's `fail-nth`), so a test can fail each of a
//! call's allocations in turn and check that every one is answered.

use crate::sync::RawMutex;
use crate::syscall;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use pageheap::{Backing, Heap, CHUNK};
use restricted::{COMMIT_NO_RESERVE, HEAP_BASE, SYS_CLOCK_READ, SYS_SHARED_COMMIT, SYS_SHARED_DECOMMIT, SYS_SHARED_DECOMMIT_RUNS, THREADS_BASE};

/// Most chunks of arena (64 GiB: far beyond any commit limit).
const MAX_CHUNKS: usize = 32 * 1024;
const META: usize = HEAP_BASE as usize;
const ARENA: usize = (META + pageheap::meta_len(MAX_CHUNKS)).next_multiple_of(CHUNK);
const _: () = assert!(ARENA + MAX_CHUNKS * CHUNK <= THREADS_BASE as usize);
/// A trim waits at least this long after the last (nanoseconds).
const TRIM_INTERVAL: u64 = 500_000_000;

/// The kernel's commits of the heap area.
pub struct Kernel;

impl Backing for Kernel {
    fn commit(&self, addr: usize, len: usize) -> bool {
        let flags = if in_fallible() { COMMIT_NO_RESERVE } else { 0 };
        syscall(SYS_SHARED_COMMIT, [addr as u64, len as u64, flags, 0, 0, 0]) >= 0
    }

    fn decommit(&self, addr: usize, len: usize) {
        syscall(SYS_SHARED_DECOMMIT, [addr as u64, len as u64, 0, 0, 0, 0]);
    }

    fn decommit_runs(&self, runs: &[(usize, usize)]) {
        // (In the image's stack: never memory the heap decommits.)
        let mut list = [0u64; 2 * pageheap::TRIM_RUNS];
        for chunk in runs.chunks(pageheap::TRIM_RUNS) {
            for (i, &(addr, len)) in chunk.iter().enumerate() {
                (list[2 * i], list[2 * i + 1]) = (addr as u64, len as u64);
            }
            syscall(SYS_SHARED_DECOMMIT_RUNS, [list.as_ptr() as u64, chunk.len() as u64, 0, 0, 0, 0]);
        }
    }

    fn trim_wanted(&self) {
        TRIM.store(true, Ordering::Release);
        crate::timer::changed();
    }
}

pub struct ServerHeap(Heap<RawMutex, Kernel>);

impl ServerHeap {
    pub const fn new() -> Self {
        ServerHeap(Heap::new(Kernel, META, ARENA, MAX_CHUNKS))
    }

    pub fn stats(&self) -> pageheap::Stats {
        self.0.stats()
    }
}

unsafe impl GlobalAlloc for ServerHeap {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if INJECTING.load(Ordering::Relaxed) && injected_failure() {
            return core::ptr::null_mut();
        }
        self.0.alloc(layout)
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) }
    }
}

/// A trim was asked for (`Kernel::trim_wanted`), and when the last ran.
static TRIM: AtomicBool = AtomicBool::new(false);
static LAST_TRIM: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [restricted::CLOCK_MONO, 0, 0, 0, 0, 0]).max(0) as u64
}

/// When the timer thread should trim (monotonic nanoseconds), 0 if no trim
/// is asked for.
pub fn trim_due() -> u64 {
    if !TRIM.load(Ordering::Acquire) {
        return 0;
    }
    (LAST_TRIM.load(Ordering::Relaxed) + TRIM_INTERVAL).max(1)
}

/// The timer thread: trims the heap if that is asked for and due.
pub fn trim_if_due() {
    let due = trim_due();
    if due == 0 || now() < due {
        return;
    }
    TRIM.store(false, Ordering::Relaxed);
    crate::HEAP.0.trim();
    LAST_TRIM.store(now(), Ordering::Relaxed);
}

/// Pages `EVENT_SHRINK` asked for and the worker has not given yet (the
/// largest request), and how many shrinks the worker made (for the tests).
static SHRINK_ASKED: AtomicU64 = AtomicU64::new(0);
pub static SHRINKS: AtomicU64 = AtomicU64::new(0);

/// `EVENT_SHRINK` (on the service thread, which must not wait for the
/// heap's locks or for diskfs): the worker shrinks.
pub fn ask_shrink(pages: u64) {
    SHRINK_ASKED.fetch_max(pages.max(1), Ordering::AcqRel);
    crate::scm::request();
}

/// The worker: gives back about the pages asked for, of the heap's free
/// memory beyond its floor and of /data's unused clean inodes.
pub fn shrink_if_asked() {
    let pages = SHRINK_ASKED.swap(0, Ordering::AcqRel);
    if pages == 0 {
        return;
    }
    crate::HEAP.0.shrink(pages as usize);
    LAST_TRIM.store(now(), Ordering::Relaxed);
    // Counted once the heap gave back (the inodes' releases may wait for
    // diskfs a while).
    SHRINKS.fetch_add(1, Ordering::Relaxed);
    crate::datafs::shrink(pages);
}

/// A fallible allocation was refused: ENOMEM (`?` into an `i64` error),
/// or what the call answers instead (`map_err`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoMem;

pub const ENOMEM: i64 = 12;

impl From<NoMem> for i64 {
    fn from(_: NoMem) -> i64 {
        ENOMEM
    }
}

/// Whether the calling thread is in a `fallible` scope.
fn in_fallible() -> bool {
    crate::local::get().fallible.load(Ordering::Relaxed) != 0
}

/// Runs `f`, whose allocations the caller handles failing (Rust's `try_*`
/// forms): their commits leave the instance's reserve alone, and the test
/// hook `TEST_FAIL_NTH` counts them. Only the allocation itself belongs in
/// it: an infallible one inside would panic where the reserve could have
/// served it.
#[inline]
pub fn fallible<R>(f: impl FnOnce() -> R) -> R {
    let l = crate::local::get();
    l.fallible.fetch_add(1, Ordering::Relaxed);
    let r = f();
    l.fallible.fetch_sub(1, Ordering::Relaxed);
    r
}

/// Growth of a collection that may fail (in a `fallible` scope): ENOMEM
/// with the collection as it was.
pub trait TryGrow<T> {
    /// Room for `n` more.
    fn try_grow(&mut self, n: usize) -> Result<(), NoMem>;
    /// Appends `x` (dropped if there is no room).
    fn try_push(&mut self, x: T) -> Result<(), NoMem>;
}

impl<T> TryGrow<T> for Vec<T> {
    fn try_grow(&mut self, n: usize) -> Result<(), NoMem> {
        fallible(|| self.try_reserve(n)).map_err(|_| NoMem)
    }

    fn try_push(&mut self, x: T) -> Result<(), NoMem> {
        self.try_grow(1)?;
        self.push(x);
        Ok(())
    }
}

impl<T> TryGrow<T> for VecDeque<T> {
    fn try_grow(&mut self, n: usize) -> Result<(), NoMem> {
        fallible(|| self.try_reserve(n)).map_err(|_| NoMem)
    }

    fn try_push(&mut self, x: T) -> Result<(), NoMem> {
        self.try_grow(1)?;
        self.push_back(x);
        Ok(())
    }
}

/// `Vec`'s fallible appends of slices.
pub trait TryExtend<T: Clone> {
    fn try_extend(&mut self, items: &[T]) -> Result<(), NoMem>;
}

impl<T: Clone> TryExtend<T> for Vec<T> {
    fn try_extend(&mut self, items: &[T]) -> Result<(), NoMem> {
        self.try_grow(items.len())?;
        self.extend_from_slice(items);
        Ok(())
    }
}

/// `String`'s fallible appends.
pub trait TryPushStr {
    fn try_push_str(&mut self, s: &str) -> Result<(), NoMem>;
}

impl TryPushStr for String {
    fn try_push_str(&mut self, s: &str) -> Result<(), NoMem> {
        fallible(|| self.try_reserve(s.len())).map_err(|_| NoMem)?;
        self.push_str(s);
        Ok(())
    }
}

/// An empty `Vec` with room for `n`.
pub fn try_vec<T>(n: usize) -> Result<Vec<T>, NoMem> {
    let mut v = Vec::new();
    v.try_grow(n)?;
    Ok(v)
}

/// A copy of `items`.
pub fn try_to_vec<T: Clone>(items: &[T]) -> Result<Vec<T>, NoMem> {
    let mut v = try_vec(items.len())?;
    v.extend_from_slice(items);
    Ok(v)
}

/// A copy of `s`.
pub fn try_string(s: &str) -> Result<String, NoMem> {
    let mut out = String::new();
    out.try_push_str(s)?;
    Ok(out)
}

/// `Box::new` that may fail (`x` dropped then).
pub fn try_box<T>(x: T) -> Result<Box<T>, NoMem> {
    fallible(|| Box::try_new(x)).map_err(|_| NoMem)
}

/// `Arc::new` that may fail (`x` dropped then).
pub fn try_arc<T>(x: T) -> Result<Arc<T>, NoMem> {
    fallible(|| Arc::try_new(x)).map_err(|_| NoMem)
}

/// `format!` that may fail (`try_format!` builds the arguments).
pub fn try_format(args: core::fmt::Arguments) -> Result<String, NoMem> {
    struct Out(String, bool);
    impl core::fmt::Write for Out {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0.try_push_str(s).map_err(|_| {
                self.1 = true;
                core::fmt::Error
            })
        }
    }
    let mut out = Out(String::new(), false);
    let r = core::fmt::write(&mut out, args);
    if out.1 {
        return Err(NoMem);
    }
    // (Only a refused allocation fails: the server's `Display`s do not.)
    debug_assert!(r.is_ok());
    Ok(out.0)
}

/// `format!` that may fail: `try_format!("{}", x)?`.
#[macro_export]
macro_rules! try_format {
    ($($arg:tt)*) => { $crate::heap::try_format(format_args!($($arg)*)) };
}

/// Before an insert into a structure with no fallible form (`BTreeMap`,
/// `BTreeSet`) that a program drives: makes sure the heap has a free page
/// committed beyond the instance's reserve for its nodes (enough for maps of
/// small entries, whose nodes are a few hundred bytes; an insert allocates
/// at most one per level and rarely more than one). The insert itself is
/// infallible: it takes the reserve only if another thread took the page in
/// between.
pub fn preload() -> Result<(), NoMem> {
    preload_bytes(pageheap::PAGE)
}

/// `preload` for an insert into a `BTreeMap<K, V>` of any entry size.
pub fn preload_map<K, V>() -> Result<(), NoMem> {
    preload_bytes(map_insert_bytes::<K, V>())
}

/// What an insert into a `BTreeMap<K, V>` allocates when it splits a leaf
/// and the node above it (11 entries a node, an internal one with 12 edges,
/// and their headers): a map of large entries (a process's record) has
/// nodes of several pages.
pub const fn map_insert_bytes<K, V>() -> usize {
    let node = 11 * (core::mem::size_of::<K>() + core::mem::size_of::<V>()) + 64;
    2 * node + 12 * 8
}

/// `preload` for inserts (several, summed by the caller) that allocate
/// `bytes`: that many free pages committed beyond the reserve (a run of
/// them committed if fewer are free; a node larger than a page needs its
/// pages in one run, which the free ones need not be: then its insert
/// commits, from the reserve at worst).
pub fn preload_bytes(bytes: usize) -> Result<(), NoMem> {
    let pages = bytes.div_ceil(pageheap::PAGE).max(1);
    if fallible(|| !injected_failure() && crate::HEAP.0.ensure_free(pages)) {
        Ok(())
    } else {
        Err(NoMem)
    }
}

/// `TEST_FAIL_NTH` was used (in test mode): the allocator checks the
/// calling thread's count from then on.
static INJECTING: AtomicBool = AtomicBool::new(false);

/// `TEST_FAIL_NTH(n)`: the calling thread's n-th fallible allocation from
/// now fails (0 disarms); returns what was left of the previous count (0:
/// it fired, or none was armed).
pub fn fail_nth(n: u64) -> i64 {
    if n > 0 {
        INJECTING.store(true, Ordering::Relaxed);
    }
    crate::local::get().fail_nth.swap(n.min(u32::MAX as u64) as u32, Ordering::Relaxed) as i64
}

/// Whether the allocation being made is the one `TEST_FAIL_NTH` fails.
#[cold]
fn injected_failure() -> bool {
    let l = crate::local::get();
    if l.fallible.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let left = l.fail_nth.load(Ordering::Relaxed);
    if left == 0 {
        return false;
    }
    l.fail_nth.store(left - 1, Ordering::Relaxed);
    left == 1
}
