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
/// `vm_sync(addr, len, flags)`: msync's contract.
pub const SYS_VM_SYNC: u64 = 1028;
/// `kfile_object(fd) -> handle`: the open file behind descriptor `fd` of
/// the calling process (the kernel's descriptor table, until files are the
/// server's), to map with `mo_map` as mmap maps a file: its page cache,
/// /dev/zero as anonymous memory; the mapping keeps the file and the
/// descriptor's write access. EBADF, or ENODEV for what cannot be mapped.
pub const SYS_KFILE_OBJECT: u64 = 1029;

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
/// `event_wait(event) -> 0`: the instance's service thread (the pager
/// thread) waits for the next event of the instance and gets it as an
/// `Event`: a page someone needs, or the last descriptor of one of the
/// server's files gone. When the instance's last program is gone, the
/// service thread's process ends here.
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

/// A server thread starts with its role in `rsi` (and its `State` in
/// `rdi`): it serves a program's thread, or it is the instance's pager.
pub const ROLE_PROGRAM: u64 = 0;
pub const ROLE_PAGER: u64 = 1;

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
/// at `flags` (a u32 in the server's memory) unless 0.
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
/// `()`: how many records the instance holds.
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
/// `mo_file_read(handle, offset, buf, len) -> n`: reads from the file
/// object into the program's memory at `buf` (up to its end).
pub const SYS_MO_FILE_READ: u64 = 1057;
/// `mo_file_write(handle, offset, buf, len) -> n`: writes the program's
/// memory at `buf` into the file object (growing it; ENOSPC, EFBIG).
pub const SYS_MO_FILE_WRITE: u64 = 1058;
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

/// `(scenario)`: the server runs a channel scenario against the test
/// service (servers/ringtest, `ring::selftest`): 1 rings and doorbells, 2
/// grants and their bounds, 3 revoking, 4 the client's end going, 5 the
/// service dying, 6 the service executing a new program, 7 a service that
/// attaches but answers late. 0 if every
/// check held, else the negative number of the first that failed.
pub const TEST_CHANNEL: u64 = 1514;
/// `(scenario)`: the server runs a scenario of the file protocol
/// (`fsring`) against diskfs over a channel: 1 reading a file of the disk
/// image and metadata, 2 writes, a flush and the file read back
/// (`/data/ringtest.bin` stays for the caller to read through the kernel
/// and remove), 3 malformed requests, 4 requests in flight, 5 a grant
/// revoked under diskfs and a client gone with requests in flight, 6 holds
/// across channels, 7 a stalled write with every operation slot busy, 8
/// requests left waiting for room in the completion ring (the caller
/// checks that diskfs sleeps), 9 their completions taken. 0 if
/// every check held, else the negative number of the first that failed.
pub const TEST_DISKRING: u64 = 1515;
