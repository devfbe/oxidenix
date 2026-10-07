//! The kernel's native process and system information (syscall 1005,
//! `proc_query`), from which the procfs server builds Linux's /proc. The
//! records are plain `repr(C)` structs of fixed size, copied as bytes.

#![no_std]

/// What `proc_query(op, arg, buf, len)` returns in `buf`:
/// a `System` record.
pub const QUERY_SYSTEM: u64 = 0;
/// The pids of all processes as `u64`s (as many as fit).
pub const QUERY_PIDS: u64 = 1;
/// The `Process` record of pid `arg`.
pub const QUERY_PROCESS: u64 = 2;
/// The command line of pid `arg` (NUL-terminated arguments).
pub const QUERY_CMDLINE: u64 = 3;
/// The absolute path of the program pid `arg` runs.
pub const QUERY_EXE: u64 = 4;
/// The mount table in /proc/mounts format.
pub const QUERY_MOUNTS: u64 = 5;

pub const MAX_CPUS: usize = 16;

/// Root inode of the /sys tree the procfs server also provides (its /proc
/// root is the argument it registers with).
pub const SYSFS_ROOT: u32 = 100;

/// Timer ticks a CPU spent in user mode, in the kernel and idle.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CpuTimes {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct System {
    /// Timer ticks per second; all times below are in ticks.
    pub hz: u64,
    pub uptime: u64,
    /// Wall-clock time of the boot, in seconds since the epoch.
    pub boot_time: u64,
    pub cpus: u64,
    pub cpu: [CpuTimes; MAX_CPUS],
    pub page_size: u64,
    pub mem_total: u64,
    pub mem_free: u64,
    /// Memory the kernel itself uses (heap), in bytes.
    pub kernel_heap: u64,
    /// Load averages over 1, 5 and 15 minutes in fixed point.
    pub load: [u64; 3],
    pub load_shift: u64,
    pub processes: u64,
    /// Threads of all processes, and those running or runnable.
    pub threads: u64,
    pub running: u64,
    pub context_switches: u64,
    /// Processes created since boot.
    pub forks: u64,
    pub max_pid: u64,
    /// Memory promised to processes and the most that may be (bytes).
    pub committed: u64,
    pub commit_limit: u64,
    /// File pages in memory (bytes): all of them, and those of tmpfs and
    /// shared memory (which cannot be dropped).
    pub cached: u64,
    pub shmem: u64,
    /// Cached file pages stored to and not yet written back (bytes).
    pub dirty: u64,
    pub counters: Counters,
}

/// Hot-path event counters since boot (for benchmarks; /proc/counters).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Counters {
    pub syscalls: u64,
    /// Linux system calls the Linux server passed back to the kernel.
    pub legacy_calls: u64,
    /// IPC requests the kernel sent to servers, and the bytes of the
    /// requests and replies it copied.
    pub ipc_calls: u64,
    pub ipc_bytes: u64,
    /// Page table root loads (CR3 writes).
    pub address_space_switches: u64,
    /// Bytes copied between the kernel and user memory.
    pub user_copy_bytes: u64,
    pub heap_allocs: u64,
}

/// Process states, as the letters of /proc/<pid>/stat.
pub const STATE_RUNNING: u8 = b'R';
pub const STATE_SLEEPING: u8 = b'S';
pub const STATE_STOPPED: u8 = b'T';
pub const STATE_ZOMBIE: u8 = b'Z';

/// Flags of a process.
pub const FLAG_SERVER: u64 = 1;
pub const FLAG_KERNEL: u64 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Process {
    pub pid: u64,
    pub ppid: u64,
    pub pgid: u64,
    pub sid: u64,
    /// One of the STATE_ letters.
    pub state: u64,
    pub utime: u64,
    pub stime: u64,
    /// Tick it was created at.
    pub start: u64,
    /// Mapped pages (resident: there is no swapping).
    pub pages: u64,
    /// Pages of address space (all areas, mapped or not yet).
    pub virt_pages: u64,
    pub nice: i64,
    pub threads: u64,
    pub cpu: u64,
    pub flags: u64,
    /// Name (comm), NUL-padded.
    pub name: [u8; 16],
}

/// The bytes of a record.
pub fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// A record from bytes, if there are enough.
pub fn from_bytes<T: Copy>(b: &[u8]) -> Option<T> {
    (b.len() >= core::mem::size_of::<T>()).then(|| unsafe { core::ptr::read_unaligned(b.as_ptr() as *const T) })
}
