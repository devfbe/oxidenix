//! Records per working-directory context: the cwd and umask of the
//! processes that share them (phase R6c).
//!
//! Until the process model is the server's (R8), the kernel's clone decides
//! which processes share a working directory (CLONE_FS), and each such
//! context of the kernel's carries a word of the server's: a pointer to its
//! `FsContext` (`SYS_FS_RECORD`). The kernel's context holds one strong
//! reference, given with `Arc::into_raw` and taken back when the kernel
//! reports `EVENT_RELEASE`. The kernel never replaces a context's record
//! and releases it only once no thread runs in the context any more (or
//! the call that was to create it returned without doing so), so a
//! thread's own record is alive while it runs: reading the word and then
//! taking a reference of our own cannot race with its release.
//!
//! A clone that makes a new context (fork, vfork, clone without CLONE_FS)
//! gets a copy of the caller's record, made before the call passes through:
//! the child sees the directory of the moment of the fork, whatever its
//! parent does next.

use crate::sync::Mutex;
use crate::syscall;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use restricted::*;

pub struct FsState {
    pub cwd: String,
    pub umask: u32,
    /// For the test calls (`TEST_FS_VALUE`).
    pub test: u64,
}

pub struct FsContext {
    pub state: Mutex<FsState>,
}

const SYS_CLONE: u64 = 56;
const SYS_FORK: u64 = 57;
const SYS_VFORK: u64 = 58;
const CLONE_FS: u64 = 0x200;

/// Hands a record to the kernel: one strong reference it holds.
fn give(context: Arc<FsContext>) -> u64 {
    Arc::into_raw(context) as u64
}

/// The calling thread's context; a new one at `/` for a context the kernel
/// made without the server (the tree's first process).
pub fn current() -> Arc<FsContext> {
    let word = syscall(SYS_FS_RECORD, [FS_GET, 0, 0, 0, 0, 0]) as u64;
    if word != 0 {
        let ptr = word as *const FsContext;
        // The kernel's reference keeps it alive while this thread runs in
        // the context (see the module comment); this one is ours.
        unsafe {
            Arc::increment_strong_count(ptr);
            return Arc::from_raw(ptr);
        }
    }
    let context = Arc::new(FsContext { state: Mutex::new(FsState { cwd: String::from("/"), umask: 0o022, test: 0 }) });
    let word = give(context.clone());
    const EEXIST: i64 = 17;
    match syscall(SYS_FS_RECORD, [FS_SET, word, 0, 0, 0, 0]) {
        0 => context,
        e => {
            // Not handed over after all.
            released(word);
            // Another thread of the context was first: take that one.
            if e == -EEXIST { current() } else { context }
        }
    }
}

/// Before a system call passes through: a clone that makes a new context
/// gets a copy of the caller's.
pub fn before_pass_through(s: &State) {
    let new_context = match s.rax {
        SYS_FORK | SYS_VFORK => true,
        SYS_CLONE => s.rdi & CLONE_FS == 0,
        _ => false,
    };
    if !new_context {
        return;
    }
    let copy = {
        let parent = current();
        let st = parent.state.lock();
        FsState { cwd: st.cwd.clone(), umask: st.umask, test: st.test }
    };
    let child = Arc::new(FsContext { state: Mutex::new(copy) });
    syscall(SYS_FS_RECORD, [FS_CHILD, give(child), 0, 0, 0, 0]);
}

/// `EVENT_RELEASE`: the kernel's reference goes.
pub fn released(word: u64) {
    drop(unsafe { Arc::from_raw(word as *const FsContext) });
}

pub fn test_value(value: u64) -> i64 {
    let context = current();
    let mut st = context.state.lock();
    if value != 0 {
        st.test = value;
    }
    st.test as i64
}

/// Records `TEST_FS_RECORDS` watches (one list per instance, as test calls
/// go): weak references, which tell whether the kernel still holds them
/// without keeping them.
static WATCHED: Mutex<Vec<Weak<FsContext>>> = Mutex::new(Vec::new());

/// `TEST_FS_RECORDS`: `watch` set: watches the caller's record; else how
/// many watched records are still held (forgetting the released ones).
/// Unlike a count of all the instance's records, this does not depend on
/// what other processes of the tree do meanwhile.
pub fn test_records(watch: bool) -> i64 {
    if watch {
        WATCHED.lock().push(Arc::downgrade(&current()));
        return 0;
    }
    let mut watched = WATCHED.lock();
    watched.retain(|w| w.strong_count() > 0);
    watched.len() as i64
}
