//! Records per working-directory context: the cwd and umask of the threads that share them
//! (`CLONE_FS`; phase R6c, the server's own since R8).
//!
//! A thread's record is held by its thread record (`process::Thread::fs`) for as long as the
//! thread lives, and its address is in the thread's local block (`local::Local::fs`), so a
//! path call finds it without a lock or a kernel call. A clone without `CLONE_FS` (fork,
//! vfork, a thread without it) gets a copy made at the call: the child sees the directory of
//! the moment of the fork, whatever its parent does next.

use crate::local;
use crate::sync::Mutex;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub struct FsState {
    pub cwd: String,
    pub umask: u32,
    /// For the test calls (`TEST_FS_VALUE`).
    pub test: u64,
}

pub struct FsContext {
    pub state: Mutex<FsState>,
}

/// A new record at the root, with Linux's default umask (the tree's first process).
pub fn root() -> Arc<FsContext> {
    Arc::new(FsContext { state: Mutex::new(FsState { cwd: String::from("/"), umask: 0o022, test: 0 }) })
}

/// A copy of `of` (a clone without `CLONE_FS`).
pub fn copy(of: &FsContext) -> Arc<FsContext> {
    let st = of.state.lock();
    let copy = FsState { cwd: st.cwd.clone(), umask: st.umask, test: st.test };
    drop(st);
    Arc::new(FsContext { state: Mutex::new(copy) })
}

/// The calling thread's record (a service thread, which has none, gets a new one at the
/// root).
pub fn current() -> Arc<FsContext> {
    let ptr = local::get().fs.load(Ordering::Acquire);
    if ptr.is_null() {
        return root();
    }
    // The thread record's reference keeps it alive while this thread runs; this one is
    // ours.
    unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
}

pub fn test_value(value: u64) -> i64 {
    let context = current();
    let mut st = context.state.lock();
    if value != 0 {
        st.test = value;
    }
    st.test as i64
}

/// Records `TEST_FS_RECORDS` watches (one list per instance, as test calls go): weak
/// references, which tell whether a thread still holds them without keeping them.
static WATCHED: Mutex<Vec<Weak<FsContext>>> = Mutex::new(Vec::new());

/// `TEST_FS_RECORDS`: `watch` set: watches the caller's record; else how many watched
/// records are still held (forgetting the released ones). Unlike a count of all the
/// instance's records, this does not depend on what other processes of the tree do
/// meanwhile.
pub fn test_records(watch: bool) -> i64 {
    if watch {
        WATCHED.lock().push(Arc::downgrade(&current()));
        return 0;
    }
    let mut watched = WATCHED.lock();
    watched.retain(|w| w.strong_count() > 0);
    watched.len() as i64
}
