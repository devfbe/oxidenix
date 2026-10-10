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
/// Bytes from here to the end of a thread's State page are the server's
/// own per-thread data (its role, tid, pid, records, read without a lock): the
/// kernel never reads or writes them, and the server sets them up when the
/// thread starts (a reused page holds a dead thread's).
pub const SERVER_LOCAL_OFFSET: u64 = 2560;

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
    /// `REASON_EXCEPTION`: the CPU's vector, its error code, the address
    /// (a page fault's) and, for a page fault, what the kernel found
    /// (`FAULT_*`). Written by the kernel with the registers; never read.
    pub trap_vector: u64,
    pub trap_error: u64,
    pub trap_addr: u64,
    pub trap_kind: u64,
}

/// Kernel calls of the server (from normal mode only).
///
/// `restricted_enter()`: runs the program with the registers in the
/// thread's `State` until it traps; returns the reason (`REASON_*`). With
/// the thread's kick flag set it returns `REASON_KICK` at once, without
/// running the program, and clears the flag. A thread marked dying
/// (`thread_kill`, or a process the kernel killed) gets `REASON_EXIT` once,
/// so that its server lets go of what the thread holds (its descriptor
/// table: before its parent learns of the end) and exits it
/// (`thread_exit`); a dying thread that enters again exits here.
pub const SYS_RESTRICTED_ENTER: u64 = 1010;
// 1011 was `legacy_syscall`, phase R1's pass-through: the kernel's own Linux
// implementation carried out the call in `State`. Since R9 the kernel
// implements no Linux system call; the server answers every one.

/// The program executed `syscall`; `State::rax` holds its number.
pub const REASON_SYSCALL: u64 = 1;
/// The thread was kicked (`thread_kick`): a signal or a stop for it, or its
/// process ends.
pub const REASON_KICK: u64 = 2;
/// The program raised a CPU exception the kernel does not resolve (a page
/// fault without a mapping or against its protection, a read beyond a
/// file's end, a division by zero, an invalid opcode, a breakpoint, ...):
/// `State::trap_*` say which, for the server's signal.
pub const REASON_EXCEPTION: u64 = 3;
/// The thread must die (`thread_kill`, or a process the kernel killed):
/// the server ends it with `thread_exit` (see `SYS_RESTRICTED_ENTER`).
pub const REASON_EXIT: u64 = 4;
/// `trap_kind` of a page fault: no mapping at the address, a mapping that
/// does not allow the access, an access the mapping's object cannot serve
/// (beyond the end of a file, an I/O error).
pub const FAULT_UNMAPPED: u64 = 1;
pub const FAULT_PROTECTION: u64 = 2;
pub const FAULT_BUS: u64 = 3;

/// System call numbers at and above this are not Linux's: the server
/// answers a program's with ENOSYS (iobench uses one to measure a
/// forwarded call alone), as it does every Linux call it does not
/// implement.
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
/// Anonymous private memory that grows down on demand, up to 8 MiB, when
/// the program touches the page below it (the stack `execve` makes).
pub const MO_GROWSDOWN: u64 = 64;

// Bridges to state the kernel still owns, and address space operations
// with the contracts of the Linux calls of the same name (phase R4).

/// `vm_remap(old, old_len, new_len, flags, new_addr) -> addr`: mremap's
/// contract.
pub const SYS_VM_REMAP: u64 = 1026;
/// `vm_discard(addr, len)`: drops the pages of private mappings in the
/// range (zero or the file's again on the next access).
pub const SYS_VM_DISCARD: u64 = 1027;
/// `vm_sync(addr, len, flags, out, cap) -> n`: msync's contract (flags and
/// range checked). With MS_SYNC, the shared mappings of cached objects
/// (`SYS_MO_CREATE_CACHED`) in the range are the server's to write back:
/// `n` of them, the first `cap` stored at `out` as (key, first page, end
/// page) triples of u64s.
pub const SYS_VM_SYNC: u64 = 1028;
// 1029 was `kfile_object`, a handle on an open file of the kernel's
// descriptor table: the table is the server's since R6e (and the kernel has
// no files for Linux programs since R9).

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
/// an `Event`: a page someone needs, a record or hold released, a cached
/// object's first dirty page, a request to write back; `EVENT_TIMER` once `deadline`
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
    /// `EVENT_PAGE`: the object's key; `EVENT_RELEASE`: the hold's word;
    /// `EVENT_THREAD_EXIT`: the thread's key.
    pub a: u64,
    /// `EVENT_PAGE`: byte offset of the page in the object.
    pub b: u64,
}

pub const EVENT_PAGE: u64 = 1;
// 2 was `EVENT_CLOSED` (a placeholder's last descriptor gone) and 10
// `EVENT_INFLIGHT`: the descriptor table is the server's since R6e.
/// A hold of the server's (`SYS_MO_HOLD`) lost its last holder: `a` is its
/// word. (The kernel's records of working directories and descriptor tables,
/// `fs_record` and `files_record`, went with R8: the server's thread table
/// holds them.)
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
/// Input came to the console the instance holds: the service thread takes
/// it (`console_read` until it returns 0). Queued once until taken.
pub const EVENT_CONSOLE: u64 = 20;
/// The instance no longer holds the console (the kernel's monitor took it
/// back): its terminal on it hangs up.
pub const EVENT_CONSOLE_LOST: u64 = 21;
/// A thread of one of the instance's programs is gone (`a`: its key): it
/// let go of its address space. The room for the
/// event was reserved when the thread was made, so none is ever lost.
pub const EVENT_THREAD_EXIT: u64 = 23;

/// A server thread starts with its `State` in `rdi`, its role in `rsi`,
/// and, serving a program, the `cookie` its creator gave `thread_create`
/// in `rdx` and its key in `rcx`. It serves a program's thread, or it is
/// the instance's pager, or one of its other service threads in the
/// pager's process, which serve neither a program nor a page (they may
/// wait for locks a thread copying to or from program memory holds, which
/// the pager never may) and end with the pager's process: the worker (the
/// collector of sockets in flight), the net thread (readiness of the
/// instance's internet sockets, and their closing, over its channel to
/// netd) and the timer thread (the processes' interval timers).
pub const ROLE_PROGRAM: u64 = 0;
pub const ROLE_PAGER: u64 = 1;
pub const ROLE_WORKER: u64 = 2;
/// The first thread of the first program of a new instance (a tree the
/// kernel started): its process has an empty address space. The server
/// makes it pid 1 with an empty descriptor table, gives it standard input,
/// output and error on the console (as Linux's init gets /dev/console) and
/// runs the program the kernel started the tree with (`init_args`); then
/// it serves the program.
pub const ROLE_INIT: u64 = 3;
pub const ROLE_NET: u64 = 4;
pub const ROLE_TIMER: u64 = 5;

/// `(addr)`: a 4-page paged object mapped shared and readable at `addr`;
/// page n reads "paged n" (supplied by the pager thread when touched).
pub const TEST_PAGED: u64 = 1505;
/// `()`: how many pages the pager supplied so far.
pub const TEST_SUPPLIED: u64 = 1506;
/// `(addr)`: a 1-page paged object the pager never supplies, mapped at
/// `addr` (a thread touching it waits until it is killed, or until the
/// server lets go of the object). `(0)`: the server closes its handle of
/// the latest such object: the kernel ends the waits for its page (EIO, a
/// fault SIGBUS), since no answer can come any more; ENOENT if none.
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
/// `FUTEX_INTERRUPTIBLE`, a kick (EINTR, see `thread_kick`). A dying thread
/// stops waiting (EINTR), but for `FUTEX_LOCK`: a lock's wait in the
/// server's own memory (EINVAL on an object's word: a word another party
/// writes never holds a dying thread), whose holder does only bounded work
/// and which a dying thread's server needs to end the thread: it ends only
/// when woken (or at the deadline), or with EINTR once the instance broke
/// (a server thread failed, `break_instance`; the server ends the waiter
/// then).
pub const SYS_SERVER_FUTEX_WAIT: u64 = 1024;
/// `server_futex_wake(addr, n) -> woken`.
pub const SYS_SERVER_FUTEX_WAKE: u64 = 1025;
pub const FUTEX_INTERRUPTIBLE: u64 = 1;
pub const FUTEX_LOCK: u64 = 2;
/// A sleeping lock's wait (the server's `SleepMutex`, `SleepRwLock`): ends
/// (EINTR) when the waiter dies, and when the instance broke (its holder
/// may be the server thread that failed: the instance's service threads,
/// which never die, must not wait for it; they give up what they wanted it
/// for and wind down as the instance closes. A service thread that waits
/// for a plain lock the failed thread held ends with `thread_exit`, which
/// ends the service threads' process once the instance broke).
pub const FUTEX_SLEEPLOCK: u64 = 4;

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

/// `clock_read(id) -> ns`: the kernel's clock `id` (`CLOCK_*`): the wall
/// clock (nanoseconds since the epoch), the monotonic clock (since boot),
/// the CPU time of the calling thread's process and of the thread; EINVAL
/// for another id. (Linux's clock ids are the server's: their aliases map
/// to these, another process's or thread's CPU clock comes from
/// `proc_info` and `thread_info`.)
pub const SYS_CLOCK_READ: u64 = 1030;
pub const CLOCK_WALL: u64 = 0;
pub const CLOCK_MONO: u64 = 1;
pub const CLOCK_PROCESS_CPU: u64 = 2;
pub const CLOCK_THREAD_CPU: u64 = 3;
/// `sleep_until(deadline, flags) -> 0`: sleeps until `deadline` (monotonic
/// nanoseconds), or EINTR when the thread is kicked (`thread_kick`) or dies.
/// With `SLEEP_NAP` a short pause that neither ends (EINVAL for a deadline
/// more than `NAP_MAX` ahead): what a dying thread polls with while it
/// waits for a request of its that a service still holds.
pub const SYS_SLEEP_UNTIL: u64 = 1031;
pub const SLEEP_NAP: u64 = 1;
pub const NAP_MAX: u64 = 10_000_000;
/// `yield()`: lets other threads run.
pub const SYS_YIELD: u64 = 1032;

/// `(dst)`: writes the 8 bytes "usercopy" to program memory at `dst` with
/// the server's copy routine; 0 or -EFAULT.
pub const TEST_USERCOPY: u64 = 1511;

// 1034-1039 were the placeholders of the server's files in the kernel's
// descriptor table (`kfd_install`, `kfd_lookup`, `kfd_ready`, `kfd_close`,
// `kfd_read`, `kfd_write`; phase R6): since R6e the descriptor table, poll,
// select and epoll are the server's (docs/design/linux-server.md, "The
// descriptor table").

// Records per working-directory context (phase R6c): the cwd and umask of
// the threads that share them (CLONE_FS). Since R8 the server's thread
// table holds them; 1040 was the kernel's `fs_record`.

/// `(value)`: sets the test value of the caller's record (0: leaves it);
/// returns it.
pub const TEST_FS_VALUE: u64 = 1512;
/// `(watch)`: nonzero: watches the caller's record; 0: how many watched
/// records some thread still holds (released ones are forgotten).
pub const TEST_FS_RECORDS: u64 = 1513;

// 1041-1052 were the kernel's tree through handles on its inodes (phase
// R6c.2b: `inode_root`, `inode_walk`, `inode_stat`, `inode_readlink`,
// `inode_create`, `inode_symlink`, `inode_unlink`, `inode_rename`,
// `inode_chmod`, `inode_truncate`, `inode_open`, `inode_statfs`), last for
// its /dev: since R9 /dev is the server's own tmpfs and the kernel has no
// tree. 1053 was `kfd_inode`, 1054 `exec_target`.

// File objects (phase R6c.2c): the contents of the server's tmpfs files,
// memory objects that grow and shrink like a file, charged to one limit
// (half of what may be committed, as Linux's tmpfs; `SYS_FILE_PAGES`).

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

/// `chan_create(slots, addr, shared) -> handle`: a new channel with `slots`
/// slots per ring (a power of two, 2..=4096) and `shared` pages of shared
/// area after the rings (0..=256: the protocol's own state, which the
/// service keeps mapped whatever the client does), mapped into the
/// server's region (the header page read-only, the rest writable); its
/// address is stored at `addr` (a u64 in the server's memory).
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
/// if it found no run among them. EBUSY if the instance's cached objects
/// have as many pages pinned as one instance may (a quarter of the commit
/// limit): the server asks again once its transfers in flight ended.
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
/// cached memory, not committed: clean ones nothing pins are reclaimed
/// when memory is short (mapped ones after reclaim removed them from the
/// mappings that did not use them lately).
pub const SYS_MO_CREATE_CACHED: u64 = 1076;
/// `mo_filled(handle, offset, pages, status)`: the pending pages among
/// `pages` (at most 256) from `offset` hold the file's data now
/// (`FILL_OK`), or could not be read (`FILL_FAILED`) or not be had for
/// want of memory (`FILL_NOMEM`): then they go, and whoever waits for them
/// or for missing pages of the range now gets an error (EIO: SIGBUS for a
/// mapping; ENOMEM: a mapping's toucher is killed, as by Linux's OOM
/// killer); nothing is kept for later accesses, which ask again. Wakes the
/// waiters. EINVAL for another status.
pub const SYS_MO_FILLED: u64 = 1077;
/// `mo_filled`'s statuses.
pub const FILL_FAILED: u64 = 0;
pub const FILL_OK: u64 = 1;
pub const FILL_NOMEM: u64 = 2;
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
/// be told any more (a final write-back that failed), and the Linux calls
/// the server does not implement.
pub const SYS_SERVER_LOG: u64 = 1085;
pub const SERVER_LOG_MAX: u64 = 256;
// 1090 was `thread_exists`: thread ids are the server's since R8.
// 1093 was `kfd_stat`.
// 1094 was `net_links`, the kernel's relay of netd's interface records:
// the server asks netd itself since R7b (`netring`'s `LINKS`).
/// `thread_nice(key, set, nice) -> nice + 20`: the nice value (-20..=19,
/// the kernel scheduler's weight) of the thread `key` (0: the caller),
/// before a change; with `set` 1 it gets `nice` (clamped). ESRCH for a
/// thread that is gone. Which threads a call names (a process group, a
/// user's) is the server's business.
pub const SYS_THREAD_NICE: u64 = 1095;

// 1100 `kfd_install_file` and 1101 `kfile_info` were how descriptors
// travelled with SCM_RIGHTS while the descriptor table was the kernel's:
// since R6e a descriptor in flight is a reference in the server's own
// memory. 1102 and 1103 were `signal_thread` and `thread_ids`: signals and
// ids are the server's since R8.

// Terminals (phase R6d, docs/design/linux-server.md "The terminal", ADR
// 0007): the console is a raw device the kernel grants to one instance at
// a time (the tree it started, until that tree's first process ends); the
// line discipline and job control's terminal side are the server's.

/// `console_read(buf, cap) -> n`: takes up to `cap` bytes of the console's
/// input (typed on the keyboard, or the console's answers to queries written
/// to it): 0 when there is none. EIO unless the instance holds the console.
pub const SYS_CONSOLE_READ: u64 = 1110;
/// `console_write(buf, len, flags) -> n`: writes the first `n` (at most
/// `CONSOLE_WRITE_MAX`) of `len` bytes to the console as they are (a VT100: a
/// line feed keeps the column, `ONLCR` is the terminal's), whole: another
/// write's bytes do not come between them. The kernel copies the bytes first,
/// then waits for the writer turn (first come first served, held only within
/// the call); a signal does not end that wait, a dying thread's does (EINTR,
/// nothing written), and so does the instance's loss of the console (EIO), so
/// output processed for the write goes out. With `CONSOLE_ECHO` (an echo of the
/// line discipline, at most `CONSOLE_ECHO_MAX` bytes) it never waits: the bytes
/// are queued and go out between the pieces of a write in progress or at once;
/// what does not fit in the queue (4 KiB) is dropped, `n` says how much went.
/// EIO unless the instance holds the console.
pub const SYS_CONSOLE_WRITE: u64 = 1111;
pub const CONSOLE_ECHO: u64 = 1;
pub const CONSOLE_WRITE_MAX: u64 = 4096;
pub const CONSOLE_ECHO_MAX: u64 = 512;
/// `console_info(out)`: the console's size, two u64s at `out` (columns,
/// rows). EIO unless the instance holds the console.
pub const SYS_CONSOLE_INFO: u64 = 1112;
// 1113-1115 were `proc_ids`, `signal_group` and `signal_state`: process
// groups, sessions and signals are the server's since R8.

// Processes and threads as containers (phase R8, docs/design/linux-server.md
// "Processes and signals", ADR 0010): the kernel keeps address spaces,
// threads and their scheduling; pids, the tree, signals, exec's loader,
// exit and wait are the server's (as the descriptor tables are since
// R6e). A thread is
// named by its key (`slot | generation << 32`, see `thread_create`), a
// process by a handle.

/// `proc_self() -> handle`: a handle on the calling thread's process.
pub const SYS_PROC_SELF: u64 = 1140;
/// `proc_create(flags) -> handle`: a new process of the instance without a
/// thread yet (`thread_create` gives it its first; closing the handle
/// before ends it). Its address space: a copy-on-write clone of the
/// caller's (`PROC_FORK`) or the caller's own (`PROC_SHARE_VM`, for
/// `CLONE_VM`); exactly one of the two. EAGAIN when the kernel's task
/// table is full, ENOMEM.
pub const SYS_PROC_CREATE: u64 = 1141;
pub const PROC_FORK: u64 = 1;
pub const PROC_SHARE_VM: u64 = 2;
/// `thread_create(process, state, flags, tls, ctid, cookie) -> key`: a new
/// thread in the process `process` (a handle from `proc_create` whose
/// process has no thread yet) or, with 0, in the caller's. Its program
/// starts with the registers of the `State` at `state` (server memory),
/// the caller's FPU registers, and the caller's FS base or, with
/// `THREAD_SETTLS`, `tls`; `ctid` (0: none) is its `CLONE_CHILD_CLEARTID`
/// word: zeroed and woken (futex) when it exits. Its server starts in
/// `ROLE_PROGRAM` with `cookie` and its key (see `ROLE_PROGRAM`). The key
/// names the thread for the calls below until it is gone; a key is never
/// reused (a thread area's generation counts up; an area whose 31 bits of
/// generations are used up is retired). EAGAIN when the kernel's task table
/// is full or the caller's process is ending, EINTR when the caller is
/// dying (`thread_kill`).
pub const SYS_THREAD_CREATE: u64 = 1142;
pub const THREAD_SETTLS: u64 = 1;
/// `thread_kick(key)`: the thread (0: the caller) looks at its signals: it
/// sets the thread's kick flag; a thread running its program returns from
/// `restricted_enter` with `REASON_KICK` (at once, by an interrupt if it
/// runs on another CPU), a thread in an interruptible wait (a server futex
/// with `FUTEX_INTERRUPTIBLE`, `sleep_until`, a passed-through call's
/// wait in the kernel) ends it with EINTR, and so does every such wait
/// until the flag is cleared: only `restricted_enter` clears it. ESRCH for
/// a thread that is gone.
pub const SYS_THREAD_KICK: u64 = 1143;
/// `thread_kill(key)`: the thread (of the caller's instance) dies: it is
/// kicked and marked dying, so that every wait of its ends (the server's
/// locks yield instead of sleeping), and it exits at its next
/// `restricted_enter`. ESRCH for a thread that is gone.
pub const SYS_THREAD_KILL: u64 = 1144;
/// `thread_exit(status, flags)`: the calling thread ends; with
/// `EXIT_GROUP`, every thread of its process (the others as by
/// `thread_kill`). `status` is the wait status the kernel keeps for the
/// tree's first process (the monitor waits for it). Does not return.
pub const SYS_THREAD_EXIT: u64 = 1145;
pub const EXIT_GROUP: u64 = 1;
/// `exec_space(exe, name, len)`: execve's point of no return for the
/// calling thread's process, which must have no other thread left
/// (EBUSY): a new, empty address space (attached to the instance) takes
/// the old one's place, with the program file `exe` (a file object handle
/// from `mo_hold`, whose hold it keeps while the program runs; 0: none);
/// the thread's FPU state and FS base are reset; its `CLONE_CHILD_CLEARTID`
/// word is zeroed and woken in the old space and forgotten; `name` (at
/// most 15 bytes) becomes the kernel's name of the thread. On failure
/// nothing changed.
pub const SYS_EXEC_SPACE: u64 = 1146;
/// `proc_info(handle, out)`: a `ProcInfo` about the process `handle` (0:
/// the caller's), also once it ended.
pub const SYS_PROC_INFO: u64 = 1147;
/// `thread_info(key, out)`: a `ThreadInfo` about the thread `key` (0: the
/// caller). ESRCH for a thread that is gone.
pub const SYS_THREAD_INFO: u64 = 1148;
/// `thread_affinity(key, set, mask) -> mask`: the CPUs the thread `key` (0:
/// the caller) may run on, as a bit mask of the CPUs that run, before a
/// change; with `set` 1 it gets `mask` (EINVAL if no CPU of it runs).
pub const SYS_THREAD_AFFINITY: u64 = 1149;
/// `thread_cleartid(addr)`: the calling thread's `CLONE_CHILD_CLEARTID`
/// word (set_tid_address; 0: none).
pub const SYS_THREAD_CLEARTID: u64 = 1150;
/// `thread_name(key, name, len)`: the kernel's name of the thread `key` (0:
/// the caller), at most 15 bytes (its monitor's `ps`, its messages).
pub const SYS_THREAD_NAME: u64 = 1151;
/// `init_args(buf, cap) -> len`: the command line the kernel started the
/// instance's tree with: the program's path, its arguments and its
/// environment, each NUL-terminated, the arguments and the environment
/// each ended by an empty string. ERANGE if it does not fit in `cap`.
pub const SYS_INIT_ARGS: u64 = 1152;
/// `vm_floor(addr)`: the lowest address the kernel places a mapping at
/// that the server did not place itself (`MO_FIXED`): the program break,
/// which the server keeps (brk).
pub const SYS_VM_FLOOR: u64 = 1153;
/// `random(buf, len)`: `len` (at most 256) bytes of the kernel's generator
/// into the server's memory (execve's AT_RANDOM).
pub const SYS_RANDOM: u64 = 1154;

// The last mechanisms the kernel's Linux code used to provide (R9): the
// server implements the Linux calls over them (futex, arch_prctl,
// clock_settime and settimeofday, reboot).

/// `futex_wait(addr, val, deadline, bitset, flags) -> 0`: sleeps while the
/// u32 at `addr` (the program's memory, below 64 TiB; a native server's
/// own) holds `val`, until a `futex_wake` of the word whose bitset shares a
/// bit with `bitset` (nonzero), the deadline (monotonic nanoseconds; 0:
/// none; ETIMEDOUT) or a kick (EINTR, see `thread_kick`); EAGAIN at once
/// if the word holds another value, EINVAL if `addr` is not 4-aligned,
/// EFAULT if it cannot be read. The word's key is the address space and the
/// address, or, with a shared mapping of a memory object there and without
/// `FUTEX_PRIVATE`, the object and the offset (so processes mapping it meet).
pub const SYS_FUTEX_WAIT: u64 = 1160;
/// `futex_wake(addr, n, bitset, flags) -> woken`: wakes at most `n` waiters
/// of the word at `addr` whose bitset shares a bit with `bitset`.
pub const SYS_FUTEX_WAKE: u64 = 1161;
/// `futex_requeue(addr, n_wake, n_move, addr2, val, flags) -> n`: wakes at
/// most `n_wake` waiters of `addr` and moves at most `n_move` more to wait
/// on `addr2`; with `FUTEX_CMP` only if the word at `addr` still holds
/// `val` (EAGAIN otherwise). Returns how many were woken and moved.
pub const SYS_FUTEX_REQUEUE: u64 = 1162;
/// The word is private to the address space (Linux's FUTEX_PRIVATE_FLAG).
pub const FUTEX_PRIVATE: u64 = 1;
/// `futex_requeue` compares the word first (FUTEX_CMP_REQUEUE).
pub const FUTEX_CMP: u64 = 2;
/// `thread_fs(set, base) -> old`: the calling thread's program's FS base
/// (its thread pointer), which stays in the CPU while the server runs:
/// returns it, and with `set` 1 replaces it by `base` (below 64 TiB, else
/// EINVAL). For arch_prctl.
pub const SYS_THREAD_FS: u64 = 1163;
/// `clock_set(ns)`: sets the wall clock (CLOCK_REALTIME) to `ns`
/// nanoseconds since the epoch, for every process of the machine. EPERM
/// without the host grant: the kernel gives a tree's instance that grant
/// when it starts the tree (the monitor's `run`, autorun); a tree without
/// it acts as a Linux pid namespace that is not the initial one (ADR 0011).
pub const SYS_CLOCK_SET: u64 = 1164;
/// `power(how) -> !`: every instance of the Linux server writes its caches
/// back (at most a minute, also if the caller is killed meanwhile), then the
/// machine powers off (`POWER_OFF`) or restarts (`POWER_RESTART`). EINVAL
/// for another `how`; EPERM without the host grant (see `SYS_CLOCK_SET`).
pub const SYS_POWER: u64 = 1165;
pub const POWER_OFF: u64 = 0;
pub const POWER_RESTART: u64 = 1;
/// `file_pages(out)`: the pages the contents of file objects (every
/// instance's tmpfs files: `mo_create_file`, `mo_from_image`) take and
/// their limit, two u64s at `out` in the server's memory (tmpfs's statfs).
pub const SYS_FILE_PAGES: u64 = 1166;

/// What `proc_info` tells about a process.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ProcInfo {
    /// CPU time in nanoseconds of its live and ended threads.
    pub user_ns: u64,
    pub system_ns: u64,
    /// Pages mapped now, pages of address space, the most ever mapped.
    pub pages: u64,
    pub virt_pages: u64,
    pub peak_pages: u64,
    /// The scheduler tick it was made at (`/proc/<pid>/stat`'s start time).
    pub start_ticks: u64,
    /// Live threads, and how many of them run or wait for a CPU.
    pub threads: u64,
    pub running: u64,
    /// The wait status of a process the kernel killed (out of memory, the
    /// monitor's `kill`, a failed server), else 0.
    pub killed: u64,
    /// The kernel's id of it (for its monitor and messages).
    pub kernel_pid: u64,
}

/// What `thread_info` tells about a thread.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ThreadInfo {
    pub user_ns: u64,
    pub system_ns: u64,
    /// Nanoseconds it ran, measured exactly (CPUCLOCK_SCHED).
    pub run_ns: u64,
    pub nice: i64,
    /// The CPU it ran on last.
    pub cpu: u64,
    /// 1 while it runs or waits for a CPU, 0 while it sleeps.
    pub running: u64,
    pub kernel_tid: u64,
}

// The descriptor table (phase R6e, docs/design/linux-server.md "The
// descriptor table"): the server's, per process, with poll, select and
// epoll; which threads share one is the server's thread table's (R8; 1130
// was `files_record`, the kernel's record of it until then).

// 1131 and 1132 were `kfile_call` and `kfile_inode`, the calls on an open
// file of the kernel's tree the server held by handle (its /dev): gone with
// the tree (R9).

/// `server_wait(words, n, deadline, flags) -> 0`: sleeps until one of the
/// `n` (1..=`WAIT_MAX`) words described at `words` (pairs of u64: the
/// address of a word in the server's memory, or in an object mapped there
/// such as a channel's shared area, and the value it is expected to hold)
/// is woken (`server_futex_wake`, or a service's wake of the object's word),
/// until the deadline (monotonic nanoseconds; 0: none) passes (ETIMEDOUT)
/// or, with `FUTEX_INTERRUPTIBLE`, the thread is kicked (EINTR, see
/// `thread_kick`); EAGAIN at once if a word holds another value. (The
/// temporary signal masks of ppoll, pselect6 and epoll_pwait are the
/// server's, R8; 1134 was `restore_sigmask`.)
pub const SYS_SERVER_WAIT: u64 = 1133;
pub const WAIT_MAX: u64 = 64;

// /proc (I/O rings step 5, docs/design/linux-server.md "/proc and /sys"):
// the system-wide files are procfs's, over the instance's channel; each
// process's own part of /proc is the server's, made from its own process
// table (R8) and the kernel's `proc_info`/`thread_info`, and its
// descriptors from its own tables (R6e).

/// `system_info(QUERY_SYSTEM, 0, buf, len) -> n`: the kernel's record of the
/// system (`procproto::System`: its tick rate, CPUs, memory, counters), as
/// the procfs server's `proc_query` gives it, into the server's memory;
/// ERANGE if it does not fit. (The processes' records are the server's
/// since R8: any other query is EINVAL.)
pub const SYS_SYSTEM_INFO: u64 = 1116;
// 1117 was `kfd_list`, the kernel's descriptors for /proc/self/fd: the
// table is the server's since R6e.
/// `test_mode() -> 0 | 1`: whether the kernel runs in test mode (it booted
/// into /etc/autorun: the self-tests, a benchmark or a scripted run). The
/// server's test hooks (`TEST_*`, which reach beyond their caller) answer
/// only then, ENOSYS otherwise.
pub const SYS_TEST_MODE: u64 = 1118;

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
/// `(name, len) -> ms`: the CPU time (user and system, in milliseconds) of
/// the running process of the kernel's server `name` (ESRCH if none), in
/// test mode only (ENOSYS otherwise); a program's call with a C string
/// `name` is the server's to pass on. The self-tests measure a server's
/// idleness with it: /proc shows only the caller's instance's processes.
pub const TEST_SERVER_TICKS: u64 = 1518;
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
// 1517 was `TEST_PASS_THROUGH`: nothing passes through since R9.
/// `(ino)`: the pager fails the next backing the kernel asks of it
/// (`EVENT_MKWRITE`) for the /data file with inode number `ino`, as a full
/// disk would (`mo_backed` not ok), and answers later ones as usual: the
/// store through a shared mapping that asked raises SIGBUS, a later one
/// asks again. Inode 0 disarms it; so does the inode leaving the server's
/// cache (its number may go to another file). Test mode only.
pub const TEST_MKWRITE_FAIL: u64 = 1519;
/// `()`: the server fails on the calling thread (an invalid opcode in its own
/// code) while it holds a plain lock (`TEST_LOCKED_ADD`'s) and a sleeping
/// one (`TEST_SLEEP_LOCKED`'s): its instance breaks (`break_instance`).
/// Test mode only; it ends the whole tree (`lxtest serverfail`, run on its
/// own by autorun, not by the self-tests).
pub const TEST_SERVER_FAIL: u64 = 1520;
/// `(ns)`: takes the sleeping lock `TEST_SERVER_FAIL` holds, keeps it `ns`
/// nanoseconds, and lets go (0 if it got it, EINTR if its wait ended).
/// Test mode only.
pub const TEST_SLEEP_LOCKED: u64 = 1521;
/// `(on) -> old`: the kernel gives the caller's instance the host grant
/// (`SYS_CLOCK_SET`, `SYS_POWER`) or takes it, and returns whether it had
/// it: lxtest checks the tree without it. Test mode only; a program's call
/// is the server's to pass on.
pub const TEST_HOST: u64 = 1522;
