# Benchmarks

Every change to the I/O paths (see `docs/io-path-audit.md`) gets a before and after number
from the same benchmark in the same configuration.

## Running

```sh
scripts/bench.sh [label]
```

The script boots oxidenix in benchmark mode (`OXIDENIX_BENCH=1`: `/etc/autorun` runs
`/etc/bench.sh`) on a fresh data disk (`OXIDENIX_DISK`, a new 64 MiB ext2 image, so earlier
runs leave no trace), collects the output of `iobench` from the serial port and writes
`docs/benchmarks/<date>-<commit>[-label].md` (`-dirty` if the tree had uncommitted changes).
QEMU runs as for the self-tests: q35, 4 CPUs, 256 MiB, KVM, the data disk on virtio-blk,
virtio-net with user networking and the echo service at 10.0.2.100:7.

## What `iobench` measures (`userspace/iobench.c`)

| name | what |
|---|---|
| `null_syscall` | `getppid`, cycles (`rdtsc`), p50/p99 of 20000; from phase R1 on forwarded to the Linux server and back to the kernel's implementation |
| `forwarded_null_syscall` | system call 1999, which the Linux server answers itself (`ENOSYS`): the cost of forwarding alone; cycles, p50/p99 |
| `fstat_disk` | `fstat` of a file on the disk: one IPC round trip to diskfs with a small request and reply; cycles, p50/p99 |
| `fstat_tmpfs` | the same in tmpfs (no IPC), for comparison |
| `seq_write` | 16 MiB in 64 KiB `write`s to a new file, then `fsync`; MB/s |
| `seq_read_disk` | the file with `O_DIRECT` in 64 KiB `pread`s (past the page cache, from the server); MB/s |
| `seq_read_cached` | the file from the page cache in 64 KiB `pread`s; MB/s |
| `read_4k_cached` | 2000 random 4 KiB `pread`s from the page cache; ns, p50/p99 |
| `read_4k_disk` | the same with `O_DIRECT`; ns, p50/p99 |
| `tcp_loopback` | 32 MiB in 64 KiB `write`s over 127.0.0.1 to a forked receiver; MB/s |
| `tcp_network_echo` | 4 MiB through the network card to QEMU's echo service and back (sent while a forked reader drains the echo); MB/s. Bound by QEMU's user networking and the `cat` behind the echo service as much as by the guest |

For each benchmark a `counters` line gives the counts **per operation** (per call, or per
64 KiB for the throughput tests) from `/proc/counters`, over the whole system: the program,
the kernel and the servers:

| counter | counted where |
|---|---|
| `syscalls` | every system call entry (`process/syscall.rs`), servers' included |
| `ipc_calls`, `ipc_bytes` | requests the kernel sends to servers, and the bytes of requests and replies it copies (`process/ipc.rs`) |
| `address_space_switches` | page table root loads (`process/tlb.rs`) |
| `user_copy_bytes` | bytes copied between kernel and user memory (`process/uaccess.rs`) |
| `heap_allocs` | kernel heap allocations |

Copies inside the servers (they are user programs) are not in the counters; the audit counts
them from the code.

## Comparing with Linux

`scripts/bench.sh --linux` runs the same `iobench` (built static with musl) in a Linux guest
(`scripts/linux-guest.nix`: the nixpkgs kernel, BusyBox, the virtio and ext2 modules) with the
same QEMU configuration and a data disk made the same way; the counter columns are `-` there.
