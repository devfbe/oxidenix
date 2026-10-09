//! The interface between the kernel and the Linux server for restricted
//! mode (docs/design/linux-server.md): the layout of the server's shared
//! region, the per-thread register block, and the kernel calls the server
//! makes from normal mode.

#![no_std]

/// The shared region: PML4 slot 128 (512 GiB), above the 64 TiB the Linux
/// program owns (ADR 0003). Mapped in the normal view only. (Slots 129-255
/// stay free for a larger region.)
pub const SHARED_BASE: u64 = 0x4000_0000_0000;
pub const SHARED_END: u64 = SHARED_BASE + 0x80_0000_0000;
/// Where the server's program is linked; it must end below `MAPS_BASE`.
pub const IMAGE_BASE: u64 = SHARED_BASE;
/// Where the kernel maps memory objects into the region for the server
/// (channels, `SYS_CHAN_CREATE`), up to `HEAP_BASE`.
pub const MAPS_BASE: u64 = SHARED_BASE + 0x08_0000_0000;
/// The server's heap: grows from here (`SYS_SHARED_MAP`) up to the thread
/// areas.
pub const HEAP_BASE: u64 = SHARED_BASE + 0x10_0000_0000;
/// Per-thread areas: a guard page, the server's stack for the thread, and
/// the page with its `State`.
pub const THREADS_BASE: u64 = SHARED_BASE + 0x40_0000_0000;
pub const THREAD_AREA: u64 = 64 * 1024;
pub const THREAD_STACK: u64 = 32 * 1024;
/// Most threads one instance can run at once.
pub const MAX_THREADS: u64 = (SHARED_END - THREADS_BASE) / THREAD_AREA;

/// Stack of thread area `n` (top), and its `State` page.
pub const fn thread_stack_top(n: u64) -> u64 {
    THREADS_BASE + n * THREAD_AREA + 4096 + THREAD_STACK
}

pub const fn thread_state(n: u64) -> u64 {
    thread_stack_top(n)
}

/// Where in a thread's State page the server counts the locks it holds on
/// the thread (a u32; `servers/linux/src/sync.rs`): the kernel's scheduler
/// gives a thread holding one the weight of nice -20, so a program's low
/// priority never holds up the instance's other threads waiting for the
/// lock (priority inversion; docs/design/linux-server.md). Only the thread
/// itself writes it, by plain stores: the kernel zeroes it before it hands
/// out the slot to a new thread and reads it only while that thread's task
/// lives (the slot is free again only once the task is freed).
pub const SERVER_LOCKS_OFFSET: u64 = 2048;

/// The Linux program's registers while the server handles one of its
/// traps: the kernel writes them when the program traps and reads them
/// when the server enters restricted mode again. The server may change
/// them (a system call's result, a signal frame); the kernel accepts only
/// a user-mode instruction pointer and stack below 64 TiB and the flags a
/// program may set.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct State {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

/// Kernel calls of the server (from normal mode only).
///
/// `restricted_enter()`: runs the program with the registers in the
/// thread's `State` until it traps; returns the reason (`REASON_*`).
pub const SYS_RESTRICTED_ENTER: u64 = 1010;
/// `legacy_syscall(closed, cap) -> n`: has the kernel's own Linux
/// implementation carry out the system call in `State` (phase R1's
/// pass-through, which goes away as the server takes the calls over); its
/// result and any signal frame land in `State`. The ids of server files
/// whose last descriptor the call closed (up to `cap` of them) are stored
/// at `closed` (u64s in the server's memory) and counted in `n`, for the
/// server to drop them at once; more go to the service thread as events.
pub const SYS_LEGACY_SYSCALL: u64 = 1011;

/// The program executed `syscall`; `State::rax` holds its number.
pub const REASON_SYSCALL: u64 = 1;

/// System call numbers at and above this are not Linux's: the server
/// answers them with ENOSYS without asking the kernel (iobench uses one to
/// measure a forwarded call alone).
pub const FIRST_NON_LINUX: u64 = 1000;

// Kernel objects, by handle (one table per server instance, so an object
// can be used from any thread of the process tree).

/// `handle_close(handle)`.
pub const SYS_HANDLE_CLOSE: u64 = 1012;
/// `mo_create(pages) -> handle`: a zero-filled memory object of `pages`
/// pages (committed memory).
pub const SYS_MO_CREATE: u64 = 1013;
/// `mo_map(handle, addr, len, offset, prot, flags) -> addr`: maps `len`
/// bytes of the object from `offset` in the calling thread's program view,
/// shared (`MO_SHARED`: stores reach the object) or private
/// (copy-on-write). Handle 0 is anonymous private memory (demand-zero,
/// committed when writable unless `MO_NORESERVE`). With `MO_FIXED` the
/// mapping goes at `addr` (replacing what was there, or EEXIST with
/// `MO_NOREPLACE`); otherwise `addr` is a hint, taken if that range is
/// free, and the kernel finds a free range. `MO_POPULATE` makes the pages
/// present now. `prot`: mmap's PROT_ bits.
pub const SYS_MO_MAP: u64 = 1014;
/// `mo_unmap(addr, len)` in the calling thread's program view.
pub const SYS_MO_UNMAP: u64 = 1015;
/// `mo_protect(addr, len, prot)` in the calling thread's program view.
pub const SYS_MO_PROTECT: u64 = 1016;
/// `mo_read(handle, offset, buf, len) -> bytes`: from the object into the
/// server's memory.
pub const SYS_MO_READ: u64 = 1017;
/// `mo_write(handle, offset, buf, len) -> bytes`: from the server's memory
/// into the object (within its size).
pub const SYS_MO_WRITE: u64 = 1018;

pub const MO_SHARED: u64 = 1;
pub const MO_FIXED: u64 = 2;
pub const MO_NOREPLACE: u64 = 4;
pub const MO_NORESERVE: u64 = 8;
pub const MO_POPULATE: u64 = 16;
/// A shared mapping that may never become writable (a file object mapped
/// through a descriptor not open for writing: mprotect gives EACCES).
pub const MO_READONLY: u64 = 32;

// Bridges to state the kernel still owns, and address space operations
// with the contracts of the Linux calls of the same name (phase R4).

/// `vm_remap(old, old_len, new_len, flags, new_addr) -> addr`: mremap's
/// contract.
pub const SYS_VM_REMAP: u64 = 1026;
/// `vm_discard(addr, len)`: drops the pages of private mappings in the
/// range (zero or the file's again on the next access).
pub const SYS_VM_DISCARD: u64 = 1027;
/// `vm_sync(addr, len, flags, out, cap) -> n`: msync's contract for the
/// kernel's files. With MS_SYNC, the shared mappings of cached objects
/// (`SYS_MO_CREATE_CACHED`) in the range are the server's to write back:
/// `n` of them, the first `cap` stored at `out` as (key, first page, end
/// page) triples of u64s.
pub const SYS_VM_SYNC: u64 = 1028;
/// `kfile_object(fd, flags) -> handle`: the open file behind descriptor `fd` of
/// the calling process (the kernel's descriptor table, until files are the
/// server's), to map with `mo_map` as mmap maps a file: its page cache,
/// /dev/zero as anonymous memory; the mapping keeps the file and the
/// descriptor's write access. EBADF, or ENODEV for what cannot be mapped.
/// The handle is also how a descriptor travels between processes
/// (`SYS_KFD_INSTALL_FILE`): it keeps the open file description, whatever
/// it is (a file of the kernel's or a placeholder of the server's), alive
/// while it is in flight. With `KFILE_INFLIGHT` the handle is such a
/// descriptor in flight (it cannot be mapped): while one of a placeholder
/// is, a descriptor of it that goes or the end of a call that looked it up
/// (`kfd_lookup`) queues `EVENT_INFLIGHT` when nothing but such handles is
/// left.
pub const SYS_KFILE_OBJECT: u64 = 1029;
pub const KFILE_INFLIGHT: u64 = 1;

/// Test calls a program can make to its server (lxtest): they exercise the
/// kernel interface above on the calling process. Each returns 0 or a
/// negative errno.
/// `(addr)`: a 3-page object with "linux server" at its second page,
/// mapped shared and writable at `addr`.
pub const TEST_MAP: u64 = 1500;
/// `()`: the first byte of that object.
pub const TEST_READ: u64 = 1501;
/// `(addr)`: the mapping made read-only.
pub const TEST_PROTECT: u64 = 1502;
/// `(addr)`: the mapping removed and the object's handle closed.
pub const TEST_UNMAP: u64 = 1503;
/// `(addr)`: maps a fresh object at `addr` and returns the kernel's answer.
pub const TEST_MAP_AT: u64 = 1504;

// Paged memory objects: the server supplies their pages on demand, from a
// thread of its own (the pager thread), while the thread that needs a page
// sleeps in the kernel (it may be the kernel itself, copying from a
// mapping, so the request cannot go to that thread's server).

/// `mo_create_paged(pages, key) -> handle`: a memory object whose pages
/// the server supplies; requests name it by `key`.
pub const SYS_MO_CREATE_PAGED: u64 = 1019;
/// `event_wait(event, deadline) -> 0`: the instance's service thread (the
/// pager thread) waits for the next event of the instance and gets it as
/// an `Event`: a page someone needs, the last descriptor of one of the
/// server's files gone, a hold released, a cached object's first dirty
/// page, a request to write back; `EVENT_TIMER` once `deadline`
/// (monotonic nanoseconds; 0: none) passed without one. When the
/// instance's last program is gone it gets `EVENT_CLOSING` once (to write
/// its caches back), and the next wait ends the service thread's process.
pub const SYS_EVENT_WAIT: u64 = 1020;
/// `mo_supply(handle, offset, buf, len)`: the page at `offset` of a paged
/// object, from `len` bytes at `buf` (the rest zero), unless it is there
/// already; wakes whoever waits for it.
pub const SYS_MO_SUPPLY: u64 = 1021;
/// `mo_fail(handle, offset)`: the pager cannot supply the page at
/// `offset` (an I/O error): whoever waits for it gets an error (SIGBUS for
/// a program's access, as on Linux), and a later access asks again.
pub const SYS_MO_FAIL: u64 = 1022;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Event {
    pub kind: u64,
    /// `EVENT_PAGE`: the object's key; `EVENT_CLOSED`: the file's id;
    /// `EVENT_RELEASE`: the record.
    pub a: u64,
    /// `EVENT_PAGE`: byte offset of the page in the object.
    pub b: u64,
}

pub const EVENT_PAGE: u64 = 1;
pub const EVENT_CLOSED: u64 = 2;
/// A record of the server's (`SYS_FS_RECORD`) lost its last holder: `a` is
/// the record.
pub const EVENT_RELEASE: u64 = 3;
/// A cached object (`SYS_MO_CREATE_CACHED`) got its first dirty page: `a`
/// is its key. (Again once all were written back and one is dirtied.)
pub const EVENT_DIRTY: u64 = 4;
/// Dirty pages crowd out memory (reclaim cannot drop them, or more than a
/// tenth of the commit limit is dirty): write back about `a` pages. One is
/// queued at a time.
pub const EVENT_WRITEBACK: u64 = 5;
/// `event_wait`'s deadline passed.
pub const EVENT_TIMER: u64 = 6;
/// The instance's last program is gone: the service thread writes its
/// caches back; its next `event_wait` ends its process.
pub const EVENT_CLOSING: u64 = 7;
/// Write every cache back and flush (as sync(2)), then `sync_done(a)`: a
/// sync of another instance (`SYS_SYNC_OTHERS`) or a reboot waits for it.
/// One is queued at a time; `a` is the latest ticket it answers.
pub const EVENT_SYNC: u64 = 8;
/// A store through a shared mapping wants a page of a cached object whose
/// disk space is not secured (`a` key, `b` byte offset of the page): the
/// server promises it and answers with `mo_backed`.
pub const EVENT_MKWRITE: u64 = 9;
/// One of the server's files with descriptors in flight (`KFILE_INFLIGHT`)
/// lost a reference and has nothing but those left (told after the
/// reference is gone): sockets may be left that only messages in flight
/// keep, for the server's collector to find. One is queued at a time.
pub const EVENT_INFLIGHT: u64 = 10;
/// Input came to the console the instance holds: the service thread takes
/// it (`console_read` until it returns 0). Queued once until taken.
pub const EVENT_CONSOLE: u64 = 20;
/// The instance no longer holds the console (the kernel's monitor took it
/// back): its terminal on it hangs up.
pub const EVENT_CONSOLE_LOST: u64 = 21;
/// A session leader's process of the instance ended: `a` is the session.
/// Its controlling terminal is dissociated (Linux's `disassociate_ctty`).
pub const EVENT_SESSION_END: u64 = 22;

/// A server thread starts with its role in `rsi` (and its `State` in
/// `rdi`): it serves a program's thread, or it is the instance's pager, or
/// its worker: a second thread of the pager's process that serves neither
/// a program nor a page (it may wait for locks a thread copying to or
/// from program memory holds, which the pager never may), and ends with
/// the pager's process.
pub const ROLE_PROGRAM: u64 = 0;
pub const ROLE_PAGER: u64 = 1;
pub const ROLE_WORKER: u64 = 2;
/// The first thread of the first program of a new instance (a tree the
/// kernel started): its descriptor table is empty, and the server gives it
/// standard input, output and error on the console (as Linux's init gets
/// /dev/console) before the program runs; then it serves the program.
pub const ROLE_INIT: u64 = 3;

/// `(addr)`: a 4-page paged object mapped shared and readable at `addr`;
/// page n reads "paged n" (supplied by the pager thread when touched).
pub const TEST_PAGED: u64 = 1505;
/// `()`: how many pages the pager supplied so far.
pub const TEST_SUPPLIED: u64 = 1506;
/// `(addr)`: a 1-page paged object the pager never supplies, mapped at
/// `addr` (a thread touching it waits until it is killed).
pub const TEST_PAGED_STUCK: u64 = 1507;
/// `(addr)`: a 1-page paged object at `addr` whose first request the
/// pager fails (mo_fail) and whose second it answers with "retry".
pub const TEST_PAGED_FAIL: u64 = 1508;

// The server's runtime.

/// `shared_map(len) -> addr`: `len` more bytes (rounded to pages) of
/// zeroed, committed memory at the top of the server's heap, for every
/// thread of the instance.
pub const SYS_SHARED_MAP: u64 = 1023;
/// `server_futex_wait(addr, val, deadline_ns, flags)`: sleeps while the
/// word at `addr` (the server's memory) holds `val`, until woken, the
/// deadline (monotonic nanoseconds; 0: none) or, with
/// `FUTEX_INTERRUPTIBLE`, a signal for the program (EINTR). A dying thread
/// always stops waiting.
pub const SYS_SERVER_FUTEX_WAIT: u64 = 1024;
/// `server_futex_wake(addr, n) -> woken`.
pub const SYS_SERVER_FUTEX_WAKE: u64 = 1025;
pub const FUTEX_INTERRUPTIBLE: u64 = 1;

/// `(n)`: the server allocates and frees `n` blocks of many sizes from its
/// heap, checking their contents; 0 if all were right.
pub const TEST_ALLOC: u64 = 1509;
/// `(n)`: adds 1 to a counter of the instance `n` times, each under the
/// server's mutex with a pause inside; returns the counter afterwards.
pub const TEST_LOCKED_ADD: u64 = 1510;

// Program memory (phase R5): the server reads and writes the program's
// memory directly in its view. A fault there is resolved as the program's
// own would be (demand paging, copy-on-write); an access the program may
// not make resumes at the fixup the server registered, which reports
// EFAULT. The server must check every program pointer against 64 TiB
// (`SHARED_BASE`) first: its own memory lies above.

/// `set_usercopy(insn, fixup)`: a fault of the server's instruction at
/// `insn` (its copy loop) on program memory that cannot be resolved
/// resumes at `fixup` instead of killing the process. Once per instance.
pub const SYS_SET_USERCOPY: u64 = 1033;

// Time and sleeping (phase R5).

/// `clock_read(id) -> ns`: the clock with Linux's id `id` (wall clock,
/// monotonic, CPU-time clocks of this thread, this process or another).
pub const SYS_CLOCK_READ: u64 = 1030;
/// `sleep_until(deadline) -> 0`: sleeps until `deadline` (monotonic
/// nanoseconds), or EINTR when a signal for the program comes.
pub const SYS_SLEEP_UNTIL: u64 = 1031;
/// `yield()`: lets other threads run.
pub const SYS_YIELD: u64 = 1032;

/// `(dst)`: writes the 8 bytes "usercopy" to program memory at `dst` with
/// the server's copy routine; 0 or -EFAULT.
pub const TEST_USERCOPY: u64 = 1511;

// The server's files in the kernel's descriptor table (phase R6): until the
// table is the server's, a file the server implements is a placeholder
// there, so dup, close, fork, exec's close-on-exec and poll/epoll keep
// working; the server handles every other operation on it.

/// `kfd_install(id, flags, ready, kind) -> fd`: a new descriptor (the
/// lowest free one) for the server's file `id`, with open flags `flags`
/// (O_ACCMODE, O_NONBLOCK, O_APPEND; O_CLOEXEC for the descriptor) and
/// poll readiness `ready`; `kind` `KFD_ALWAYS_READY` for a file that is
/// always ready (a regular file or a directory: epoll refuses it with
/// EPERM, as Linux does), else 0. When its last descriptor goes,
/// `EVENT_CLOSED`.
pub const SYS_KFD_INSTALL: u64 = 1034;
pub const KFD_ALWAYS_READY: u64 = 1;
/// `kfd_lookup(fd, flags) -> id`: the server's file behind descriptor
/// `fd` (0: a file of the kernel's; EBADF, also for another instance's
/// file), its current open flags stored
/// at `flags` (a u32 in the server's memory) unless 0. The file found is
/// pinned until the thread enters the program again (as Linux's fdget
/// holds a file for a call): another thread's close of the descriptor
/// does not end it under the call. ENOMEM if the pin cannot be kept.
pub const SYS_KFD_LOOKUP: u64 = 1035;
/// `kfd_ready(id, ready)`: the readiness of the server's file `id` for
/// poll, select and epoll (POLLIN, POLLOUT, POLLERR, POLLHUP); wakes who
/// waits for it.
pub const SYS_KFD_READY: u64 = 1036;
/// `kfd_close(fd)`: closes a descriptor of the calling process.
pub const SYS_KFD_CLOSE: u64 = 1037;
/// `kfd_read(fd, buf, len) -> n`: reads a file of the kernel's (at its
/// offset) into the server's memory, as read(2) would (for sendfile into
/// the server's files).
pub const SYS_KFD_READ: u64 = 1038;
/// `kfd_write(fd, buf, len) -> n`: writes the server's memory to a file of
/// the kernel's, as write(2) would.
pub const SYS_KFD_WRITE: u64 = 1039;

// Records per working-directory context (phase R6c): until the process
// model is the server's (R8), the kernel's clone decides which processes
// share a working directory (CLONE_FS), and each such context of the
// kernel's carries a word of the server's, its record (cwd, umask).

/// `fs_record(op, word)`: `FS_GET` returns the record of the calling
/// thread's context (0: none yet); `FS_SET` makes `word` its record if it
/// has none (EEXIST otherwise: a record is never replaced while its context
/// lives, so a thread may use its own without further synchronization);
/// `FS_CHILD` gives `word` to the next context this thread's pass-through
/// call creates (a clone without CLONE_FS). Every record handed over comes
/// back exactly once as `EVENT_RELEASE`: when its context ends, or, a
/// child's record no clone took, when the call returns. (A refused
/// `FS_SET` hands nothing over.)
pub const SYS_FS_RECORD: u64 = 1040;
pub const FS_GET: u64 = 0;
pub const FS_SET: u64 = 1;
pub const FS_CHILD: u64 = 2;

/// `(value)`: sets the test value of the caller's record (0: leaves it);
/// returns it.
pub const TEST_FS_VALUE: u64 = 1512;
/// `(watch)`: nonzero: watches the caller's record; 0: how many watched
/// records the kernel still holds (released ones are forgotten).
pub const TEST_FS_RECORDS: u64 = 1513;

// The kernel's tree through handles (phase R6c.2b): until the server's own
// filesystems serve them, the server resolves paths in the kernel's tree
// through handles on its inodes. Names and paths are (pointer, length) in
// the server's memory, at most 4096 bytes; paths here are relative,
// without "." and "..", and walked name by name.

/// `inode_root() -> handle`: the root of the kernel's tree.
pub const SYS_INODE_ROOT: u64 = 1041;
/// `inode_walk(dir, path, len, out) -> handle`: walks the names of `path`
/// from `dir`. It stops after the first symlink it reaches (to be read by
/// the server) or at the end, and stores a `Walk` at `out`. ENOENT or
/// ENOTDIR for a name that is missing or below a file.
pub const SYS_INODE_WALK: u64 = 1042;
/// `inode_stat(handle, buf)`: the inode's `struct stat` (144 bytes).
pub const SYS_INODE_STAT: u64 = 1043;
/// `inode_readlink(handle, buf, cap) -> n`: a symlink's target.
pub const SYS_INODE_READLINK: u64 = 1044;
/// `inode_create(dir, name, len, kind, perm) -> handle`: a new file
/// (`INODE_FILE`) or directory (`INODE_DIR`); EEXIST if the name is taken.
pub const SYS_INODE_CREATE: u64 = 1045;
/// `inode_symlink(dir, name, len, target, target_len)`.
pub const SYS_INODE_SYMLINK: u64 = 1046;
/// `inode_unlink(dir, name, len, dir_only)`: removes a name (rmdir with
/// `dir_only`).
pub const SYS_INODE_UNLINK: u64 = 1047;
/// `inode_rename(odir, oname, olen, ndir, nname, nlen)`.
pub const SYS_INODE_RENAME: u64 = 1048;
/// `inode_chmod(handle, perm)`.
pub const SYS_INODE_CHMOD: u64 = 1049;
/// `inode_truncate(handle, len)`: as truncate(2) (ETXTBSY while it runs).
pub const SYS_INODE_TRUNCATE: u64 = 1050;
/// `inode_open(handle, flags, path, len) -> fd`: a descriptor of the
/// calling process for the inode, as open(2) would make it once the path
/// is resolved (O_ACCMODE, O_TRUNC, O_APPEND, O_NONBLOCK, O_DIRECTORY,
/// O_CLOEXEC; EISDIR, ENOTDIR, ETXTBSY); `path` is its absolute path.
pub const SYS_INODE_OPEN: u64 = 1051;
/// `inode_statfs(handle, buf)`: its filesystem's `struct statfs`.
pub const SYS_INODE_STATFS: u64 = 1052;
/// `kfd_inode(fd, path, cap, len) -> handle`: the inode behind a kernel
/// descriptor (ENOTDIR for one without, EBADF), its absolute path stored
/// at `path` (at most `cap` bytes) and the path's length at `len` (a u64).
pub const SYS_KFD_INODE: u64 = 1053;
/// `exec_target(handle, path, len)`: the program the thread's next
/// pass-through execve runs, resolved by the server (an inode of the
/// kernel's, or a file object: a held one keeps its hold while the program
/// runs), with its absolute path; dropped when that call returns.
pub const SYS_EXEC_TARGET: u64 = 1054;

pub const INODE_FILE: u64 = 0;
pub const INODE_DIR: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Walk {
    /// Bytes of the path walked (through the symlink, if it stopped at one).
    pub consumed: u64,
    /// The mode of the inode reached (S_IFLNK: a symlink).
    pub mode: u32,
    pub _pad: u32,
}

// File objects (phase R6c.2c): the contents of the server's tmpfs files,
// memory objects that grow and shrink like a file, charged to the tmpfs
// limit as the kernel's tmpfs files are.

/// `mo_create_file() -> handle`: a new, empty file object. `mo_read` and
/// `mo_write` work on it as on a file (a read ends at its end, a write
/// grows it).
pub const SYS_MO_CREATE_FILE: u64 = 1055;
/// `mo_hold(handle, word) -> handle`: another handle on the same file
/// object that carries a hold: whatever keeps it (a mapping made through
/// it, a program run from it) keeps the hold, and when the last holder is
/// gone the server gets `EVENT_RELEASE` with `word` (once; also if no one
/// ever took it; a refused call hands nothing over). For the server's
/// write access and ETXTBSY. With `word` 0: another handle without a hold.
pub const SYS_MO_HOLD: u64 = 1056;
/// `mo_file_read(handle, offset, buf, len, flags) -> n`: reads from the
/// file object into the program's memory at `buf` (up to its end). With
/// `MO_NOFILL`, a missing page of a cached object ends the read there
/// (EAGAIN if it is the first): the server fills it itself.
pub const SYS_MO_FILE_READ: u64 = 1057;
/// `mo_file_write(handle, offset, buf, len, flags, upto) -> n`: writes the
/// program's memory at `buf` into the file object (growing it; ENOSPC,
/// EFBIG). With `MO_NOFILL` as for reads: a missing page of a cached
/// object whose data the write needs (it does not cover the page's data)
/// ends the write there. A cached object's pages must have their disk
/// space secured (backed) up to the file's end before they take data:
/// with `MO_CHECK_BACKED` a page that is not ends the write there
/// (`ENOSPC` if it is the first: the server promises the space and writes
/// again); `MO_BACKED` first marks the pages it writes backed up to byte
/// `upto` (the space the server promised), then checks the same way.
pub const SYS_MO_FILE_WRITE: u64 = 1058;
pub const MO_NOFILL: u64 = 1;
pub const MO_CHECK_BACKED: u64 = 2;
pub const MO_BACKED: u64 = 4;
/// `mo_file_size(handle) -> size`.
pub const SYS_MO_FILE_SIZE: u64 = 1059;
/// `mo_truncate(handle, len)`: sets the file object's size (pages beyond
/// it go, also from mappings).
pub const SYS_MO_TRUNCATE: u64 = 1060;

/// `initramfs(size) -> handle`: the boot image's initramfs (a cpio
/// archive), read-only; its length stored at `size` (a u64). `mo_read`
/// reads it. ENOENT if the kernel booted without one.
pub const SYS_INITRAMFS: u64 = 1061;
/// `mo_from_image(image, offset, len) -> handle`: a new file object whose
/// contents start as `len` bytes of the image at `offset` (no copy until a
/// page is needed; writes stay the object's).
pub const SYS_MO_FROM_IMAGE: u64 = 1062;
/// `event_releases() -> n`: how many `EVENT_RELEASE` events the kernel has
/// queued for the instance so far. A server thread that finds a hold still
/// counted (ETXTBSY) waits until its service thread has handled that many,
/// so that whatever was released before (a program that ended and was
/// reaped) is no longer counted when it answers.
pub const SYS_EVENT_RELEASES: u64 = 1063;

// Channels to device servers (docs/design/io-rings.md, I/O rings step 2):
// the data plane is a pair of rings in a channel's memory, which the
// kernel maps into the server's region and into the service; buffers are
// pages of the server's memory objects that it grants to the channel. The
// layout is `ring::channel`. The service's side of these calls is
// `oxrt::sys::CHAN_ATTACH` and the following.

/// `chan_create(slots, addr) -> handle`: a new channel with `slots` slots
/// per ring (a power of two, 2..=4096), mapped into the server's region
/// (the header page read-only, the rings writable); its address is stored at `addr` (a u64 in the server's memory).
/// Futex waits and wakes on it (`server_futex_wait`) meet the service's on
/// its own mapping. Closing the handle (or the instance's end) tears the
/// channel down: the service sees `CLIENT_GONE`, every grant is revoked.
pub const SYS_CHAN_CREATE: u64 = 1064;
/// `chan_connect(handle, name, len) -> 0`: offers the channel to the
/// service registered as `name` (started again if it died) and waits until
/// it attached it (0) or refused it (its error, or ECONNREFUSED). EISCONN
/// if the channel was offered before, EOPNOTSUPP if the service does not
/// take channels, ENOENT if there is no such service, EIO if it died, EINTR
/// for a signal while the service has not attached it yet.
pub const SYS_CHAN_CONNECT: u64 = 1065;
/// `grant(handle, object, offset, pages, flags) -> grant id`: grants
/// `pages` pages of a memory object (`mo_create`, `mo_create_paged`, a file
/// object) from `offset` (page-aligned, within the object) to the
/// channel's service, writable with `GRANT_WRITE`. The pages are pinned:
/// present now (a paged object's must have been supplied: ENODATA) and
/// kept as the object's own until revoked (a truncation over them fails
/// with EBUSY). The service maps them (`grant_map`) and asks for their
/// device addresses (`grant_dma`). ENOTCONN before the service attached,
/// EPIPE once it is gone.
pub const SYS_GRANT: u64 = 1066;
pub const GRANT_WRITE: u64 = 1;
/// Grant a cached object's pages to be filled (with `GRANT_WRITE`): the
/// first run of missing pages among `pages` from `offset` (the run at most
/// 256 long), made pending: zeroed frames nobody sees until `mo_filled`.
/// ENOENT if none of them is missing.
pub const GRANT_FILL: u64 = 2;
/// Grant a cached object's pages to be written back (read-only): the
/// first run of dirty pages among `pages` from `offset` (at most 256),
/// clean from now on and write-protected in every mapping (a store marks
/// them dirty again). ENOENT if none of them is dirty. With `GRANT_FILL`
/// or `GRANT_DIRTY`, `out` (the sixth argument) gets the run's first page,
/// its length in pages and the file's size when it was taken (three
/// u64s): a run of dirty pages holds data up to that size (a write makes
/// a page dirty and the file longer at once). One call looks at a bounded
/// number of present pages: EAGAIN with the page to go on from at `out`
/// if it found no run among them.
pub const GRANT_DIRTY: u64 = 4;
/// `revoke(handle, grant) -> 0 | REVOKE_DRAINING`: takes a grant back. Its
/// mappings in the service are gone when the call returns. If the service
/// had device addresses of it that a device may still use (no IOMMU to take
/// them back), the pages stay pinned until the service lets go of them
/// (`grant_dma_unmap`) or its device is reset after its death; the call
/// then returns `REVOKE_DRAINING`. The id is reused only after that. (The
/// client revokes after the requests on the grant completed: the kernel
/// keeps memory safe, the protocol keeps data right.)
pub const SYS_REVOKE: u64 = 1067;
pub const REVOKE_DRAINING: u64 = 1;

// The server's page cache of disk files (I/O rings step 4, phase R6c.3):
// one cached object per file the server uses, filled and written back by
// the server over its channel to the disk server.

/// `mo_create_cached(size, key, limit) -> handle`: a file object for the
/// server's cache of a file of `size` bytes that may grow to `limit` (the
/// filesystem's largest file: EFBIG beyond). Reads, writes (`mo_file_*`,
/// `mo_read`/`mo_write`), mappings and programs use it as a file object;
/// a missing page is asked for with `EVENT_PAGE` (`key`), and the server
/// fills it by DMA into the pages it grants (`GRANT_FILL`, `mo_filled`).
/// Stores (writes, shared mappings) mark pages dirty (`EVENT_DIRTY`); the
/// server writes them back (`GRANT_DIRTY`, `mo_redirty`). Its pages are
/// cached memory: clean ones nothing pins or maps are reclaimed when
/// memory is short.
pub const SYS_MO_CREATE_CACHED: u64 = 1076;
/// `mo_filled(handle, offset, pages, ok)`: the pending pages among `pages`
/// (at most 256) from `offset` hold the file's data now (`ok` 1), or could
/// not be read (0: they go, and whoever waits for them or for missing pages
/// of the range now gets an error, SIGBUS for a mapping; nothing is kept
/// for later accesses, which ask again). Wakes the waiters.
pub const SYS_MO_FILLED: u64 = 1077;
/// `mo_redirty(handle, offset, pages)`: marks the present pages among
/// `pages` from `offset` dirty again (their write-back failed).
pub const SYS_MO_REDIRTY: u64 = 1078;
/// `mo_map_server(handle, pages) -> addr`: maps the first `pages` pages of
/// a memory object (`mo_create`) into the server's region, read and write,
/// for every thread of the instance (as buffers it grants and reads
/// itself). `mo_unmap_server(addr)` removes it.
pub const SYS_MO_MAP_SERVER: u64 = 1079;
pub const SYS_MO_UNMAP_SERVER: u64 = 1080;
/// `sync_others(ticket)`: sync(2) across instances (/data's page cache is
/// per instance). With 0 it asks every other instance's service thread to
/// write its caches back (`EVENT_SYNC`) and returns a ticket (> 0); with a
/// ticket it waits until each instance asked then has answered it
/// (`sync_done`) or its service thread is gone, or the caller is killed.
/// Not from the service thread (EPERM: two instances' service threads
/// would wait for each other).
pub const SYS_SYNC_OTHERS: u64 = 1081;
/// `sync_done(ticket)`: the service thread wrote back what `EVENT_SYNC`
/// with that ticket asked for. Service thread only.
pub const SYS_SYNC_DONE: u64 = 1082;
/// `mo_backed(handle, first, end, ok)`: the answer to `EVENT_MKWRITE` for
/// the pages from byte `first` (page-aligned) to `end` (at most 256
/// pages): backed up to `end` (`ok` 1: the space is promised), or not to
/// be had (0: a store waiting for it gets SIGBUS, as Linux's ENOSPC in
/// `page_mkwrite`). Wakes the waiting stores.
pub const SYS_MO_BACKED: u64 = 1083;
/// `mo_unback(handle, from, out) -> found`: after diskfs lost the
/// promises (it restarted), clears the backing of the cached object's
/// pages from page `from` on and writes the first run of dirty pages among
/// them to `out` (first page, end page): 1, or 0 when none is left (the
/// server promises each run again, then goes on from its end). EAGAIN
/// with the page to go on from in `out[0]` after looking at 1024 pages.
pub const SYS_MO_UNBACK: u64 = 1084;
/// `server_log(buf, len)`: prints the server's message (UTF-8, at most
/// `SERVER_LOG_MAX` bytes) on the kernel's console: what a program cannot
/// be told any more (a final write-back that failed).
pub const SYS_SERVER_LOG: u64 = 1085;
pub const SERVER_LOG_MAX: u64 = 256;
/// `thread_exists(tid) -> 0`: ESRCH unless a thread with id `tid` (not 0)
/// exists. Thread ids are the kernel's until the process model is the
/// server's (R8); the scheduling-policy calls check their target with it.
/// It sees every task of the kernel, other instances' and the servers'
/// threads included (as /proc does today); with R8 the server answers
/// from its own process table, scoped to its instance, and this goes.
/// `thread_exists(pid, THREAD_IN_INSTANCE)`: ESRCH unless a process with
/// id `pid` belongs to the caller's instance (a pid a socket's credentials
/// may name, SCM_CREDENTIALS).
pub const SYS_THREAD_EXISTS: u64 = 1090;
/// `kfd_stat(fd, buf) -> 0`: the `struct stat` (144 bytes) of one of the
/// kernel's descriptors (EBADF for another), also of one without an inode
/// (a socket, an epoll instance), at `buf`: fstat as the kernel answers it,
/// for the server's calls that describe a descriptor in another format
/// (statx). It goes with the descriptor table (R6e).
pub const SYS_KFD_STAT: u64 = 1093;
/// `net_links(buf, cap) -> len`: the network interfaces as netd describes
/// them (`netproto::Op::Links`: `netproto::Link` records), at most `cap`
/// bytes at `buf`; ENETDOWN without netd. The kernel only relays netd's
/// answer. It goes with the sockets (R7), when the server talks to netd
/// itself.
pub const SYS_NET_LINKS: u64 = 1094;
/// `thread_nice(scope, id, set, nice) -> lowest nice + 20`: the nice
/// values (-20..=19, the kernel scheduler's weights) of the threads in
/// `scope`, all of the caller's instance: `NICE_THREAD` the thread `id`
/// (0: the caller; ESRCH for another instance's), `NICE_PGROUP` every
/// thread of process group `id` (0: the caller's), `NICE_ALL` every thread
/// of the instance (one user: a user's processes). With `set` 1 they all
/// get `nice` (clamped; lowering it is allowed: everyone is root, with
/// CAP_SYS_NICE, and RLIMIT_NICE has no limit). The answer is the lowest
/// nice value among them, before a change, plus 20; ESRCH for no thread.
/// Thread ids and process groups are the kernel's until the process model
/// is the server's (R8).
pub const SYS_THREAD_NICE: u64 = 1095;
pub const NICE_THREAD: u64 = 0;
pub const NICE_PGROUP: u64 = 1;
pub const NICE_ALL: u64 = 2;
pub const THREAD_IN_INSTANCE: u64 = 1;

// Descriptors passed between processes (SCM_RIGHTS over the server's
// AF_UNIX sockets, phase R7a), and the bits of the kernel's process model
// sockets need until it is the server's (R6e moves the descriptor table,
// R8 processes and signals): a passed descriptor is a `kfile_object` handle
// on its open file description while it is in flight; the receiver gets a
// descriptor of its own for the same description.

/// `kfd_install_file(handle, flags) -> fd`: a new descriptor (the lowest
/// free one) of the calling process for the open file description behind a
/// `kfile_object` handle, shared with its other descriptors (offset, status
/// flags), close-on-exec with `O_CLOEXEC` (the only flag). The handle stays
/// the server's. EMFILE when the table is full.
pub const SYS_KFD_INSTALL_FILE: u64 = 1100;
/// `kfile_info(handle, out)`: about the open file description behind a
/// `kfile_object` handle, two u64s at `out`: how many references it has
/// now (descriptors in every process, the server's handles, this one
/// included, and calls that use it at the moment), and the id of the
/// server's file it is a placeholder of (0: a file of the kernel's). The
/// server's collector of descriptors in flight finds sockets that only
/// messages in flight keep alive with it.
pub const SYS_KFILE_INFO: u64 = 1101;
/// `signal_thread(sig)`: raises `sig` for the calling thread as a signal
/// of its own action (SIGPIPE for a write to a connection whose reader is
/// gone); delivered when the call returns to the program, after its result.
pub const SYS_SIGNAL_THREAD: u64 = 1102;
/// `thread_ids(out)`: the calling thread's process id, thread id, user id
/// and group id, four u64s at `out` (the credentials a socket passes,
/// SCM_CREDENTIALS and SO_PEERCRED).
pub const SYS_THREAD_IDS: u64 = 1103;

// Terminals (phase R6d, docs/design/linux-server.md "The terminal", ADR
// 0007): the console is a raw device the kernel grants to one instance at
// a time (the tree it started, until that tree's first process ends); the
// line discipline and job control's terminal side are the server's. Process
// groups and sessions are still the kernel's until the process model is the
// server's (R8): `proc_ids`, `signal_group`, `signal_state` and
// `EVENT_SESSION_END` go then.

/// `console_read(buf, cap) -> n`: takes up to `cap` bytes of the console's
/// input (typed on the keyboard, or the console's answers to queries written
/// to it): 0 when there is none. EIO unless the instance holds the console.
pub const SYS_CONSOLE_READ: u64 = 1110;
/// `console_write(buf, len, flags) -> n`: writes `len` bytes to the console as
/// they are (a VT100: a line feed keeps the column, `ONLCR` is the
/// terminal's), whole: another write's bytes do not come between them (the
/// caller waits its turn, first come first served; EINTR if a signal for the
/// program comes first, with nothing written). With `CONSOLE_ECHO` (an echo of the
/// line discipline, at most 512 bytes) it never waits: the bytes are queued
/// and go out between the pieces of a write in progress or at once; what does
/// not fit in the queue (4 KiB) is dropped, `n` says how much went. EIO
/// unless the instance holds the console.
pub const SYS_CONSOLE_WRITE: u64 = 1111;
pub const CONSOLE_ECHO: u64 = 1;
/// `console_turn(op)`: `CONSOLE_TURN_TAKE` waits for the console's writer turn
/// and keeps it for the calling thread (EINTR for a signal first; EBUSY if it
/// has it already): its `console_write`s go out in it, so a writer can process
/// its output knowing it will not have to wait any more (its column
/// bookkeeping moves only for output that goes out). `CONSOLE_TURN_GIVE`
/// gives it back; so does entering the program, or the thread's end.
pub const SYS_CONSOLE_TURN: u64 = 1116;
pub const CONSOLE_TURN_TAKE: u64 = 1;
pub const CONSOLE_TURN_GIVE: u64 = 0;
/// `console_info(out)`: the console's size, two u64s at `out` (columns,
/// rows). EIO unless the instance holds the console.
pub const SYS_CONSOLE_INFO: u64 = 1112;
/// `proc_ids(id, flags, out)`: four u64s at `out` about process `id` (0:
/// the caller's), or with `IDS_PGRP` about process group `id` (one of its
/// processes): the process id, its process group, its session, and
/// `IDS_ORPHANED` if asked with `IDS_ORPHANED` and the group is orphaned (no
/// member has a parent in another group of the same session; the kernel,
/// parent of the trees it starts, counts as init: not a parent). ESRCH for
/// none in the caller's instance.
pub const SYS_PROC_IDS: u64 = 1113;
pub const IDS_PGRP: u64 = 1;
pub const IDS_ORPHANED: u64 = 2;
/// `signal_group(scope, id, sig)`: sends `sig` from the terminal (no
/// permission checks) to process `id` (`SIGNAL_PROCESS`), every process of
/// group `id` (`SIGNAL_PGRP`), or the leader of session `id` if it still leads
/// it (`SIGNAL_LEADER`), of the caller's instance. ESRCH for none.
pub const SYS_SIGNAL_GROUP: u64 = 1114;
pub const SIGNAL_PROCESS: u64 = 0;
pub const SIGNAL_PGRP: u64 = 1;
pub const SIGNAL_LEADER: u64 = 2;
/// `signal_state(sig) -> bits`: `SIGNAL_IGNORED` if the calling process
/// ignores `sig` (SIG_IGN), `SIGNAL_BLOCKED` if the calling thread blocks it
/// (SIGTTIN and SIGTTOU of background reads and writes).
pub const SYS_SIGNAL_STATE: u64 = 1115;
pub const SIGNAL_IGNORED: u64 = 1;
pub const SIGNAL_BLOCKED: u64 = 2;

/// `(scenario)`: the server runs a channel scenario against the test
/// service (servers/ringtest, `ring::selftest`): 1 rings and doorbells, 2
/// grants and their bounds, 3 revoking, 4 the client's end going, 5 the
/// service dying, 6 the service executing a new program, 7 a service that
/// attaches but answers late, 8 a service in a crash loop (the kernel's
/// restart backoff, the service down, then up after the cooldown). 0 if
/// every check held, else the negative number of the first that failed.
pub const TEST_CHANNEL: u64 = 1514;
/// `(scenario)`: the server runs a scenario of the file protocol
/// (`fsring`) against diskfs over a channel: 1 reading a file of the disk
/// image and metadata, 2 writes, a flush and the file read back
/// (`/data/ringtest.bin` stays for the caller to read through /data and
/// remove), 3 malformed requests, 4 requests in flight, 5 a grant
/// revoked under diskfs and a client gone with requests in flight, 6 holds
/// across channels, 7 a stalled write with every operation slot busy, 8
/// requests left waiting for room in the completion ring (the caller
/// checks that diskfs sleeps), 9 their completions taken. 0 if
/// every check held, else the negative number of the first that failed.
pub const TEST_DISKRING: u64 = 1515;
/// `(scenario)`: the server checks the kernel's interface of its page cache
/// (`SYS_MO_CREATE_CACHED`) on a file of /data: 1 a failed fill beyond the
/// end of the file or over 256 pages leaves no trace (the file grown later
/// reads as zeros), 2 a write-back grant over more pages than one call
/// looks at goes on where the kernel says (`EAGAIN`) and finds the dirty
/// page at the end, 3 a truncation waiting for a page pinned by a grant
/// that is never let go of gives up with `EBUSY` after its wait, 4
/// `sync_others` hands out growing tickets and returns at once with no
/// other instance, and `sync_done` is the service thread's only. 0 if every check held, else the negative number of the
/// first that failed.
pub const TEST_CACHED: u64 = 1516;
/// `()`: the server passes a `getpid` through to the kernel in its place
/// and returns its result: a call that always counts as passed through
/// (`legacy_calls`), whatever the server comes to handle itself.
pub const TEST_PASS_THROUGH: u64 = 1517;
