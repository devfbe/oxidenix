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
| `stat_path_tmpfs` | `stat` of a path of four names in tmpfs: path resolution (the Linux server's since R6c.2b) and the attributes; cycles, p50/p99 |
| `proc_meminfo_pread` | `pread` of `/proc/meminfo` (a system-wide file, procfs's) from offset 0 on an open descriptor, as top and htop re-read it; cycles, p50/p99 of 2000 |
| `proc_self_stat_pread` | the same for `/proc/self/stat` (a process's own file) |
| `proc_self_stat_open_read_close` | `/proc/self/stat` opened by path, read and closed; cycles, p50/p99 of 2000 |
| `seq_write` | 16 MiB in 64 KiB `write`s to a new file, then `fsync`; MB/s |
| `seq_read_disk` | the file with `O_DIRECT` in 64 KiB `pread`s (past the page cache, from the server); MB/s |
| `seq_read_cached` | the file from the page cache in 64 KiB `pread`s; MB/s |
| `read_4k_cached` | 2000 random 4 KiB `pread`s from the page cache; ns, p50/p99 |
| `pread_4k_cached` | random 4 KiB `pread`s from the page cache alone, in cycles (no clock calls around each) |
| `clock_gettime` | `clock_gettime(CLOCK_MONOTONIC)` alone, cycles |
| `read_4k_disk` | the same with `O_DIRECT`; ns, p50/p99 |
| `tcp_loopback` | 32 MiB in 64 KiB `write`s over 127.0.0.1 to a forked receiver; MB/s |
| `fork_wait` | `fork` of the benchmark (a small process), the child's `_exit(0)` and the parent's `waitpid`; µs, p50/p99 |
| `fork_exec_wait` | the same with the child running `/bin/hello` (stdout to `/dev/null`): fork, execve with its ELF loading (the Linux server's since R8), exit and wait; µs, p50/p99 |
| `signal_handled` | `kill` of the caller itself with `SIGUSR1` and a handler that counts: posting, the frame, the handler and `rt_sigreturn`; cycles, p50/p99 |
| `tcp_network_echo` | 4 MiB through the network card to QEMU's echo service and back (sent while a forked reader drains the echo); MB/s. Bound by QEMU's user networking and the `cat` behind the echo service as much as by the guest |

For each benchmark a `counters` line gives the counts **per operation** (per call, or per
64 KiB for the throughput tests) from `/proc/counters`, over the whole system: the program,
the kernel and the servers:

| counter | counted where |
|---|---|
| `syscalls` | every system call entry (`process/syscall.rs`), servers' included |
| `ipc_calls`, `ipc_bytes` | requests the kernel sends to servers, and the bytes of requests and replies it copies (`process/ipc.rs`): since I/O rings step 5 only channel offers, so 0 per operation for everything on the rings |
| `address_space_switches` | page table root loads (`process/tlb.rs`) |
| `user_copy_bytes` | bytes copied between kernel and user memory (`process/uaccess.rs`) |
| `heap_allocs` | kernel heap allocations |

Copies inside the servers (they are user programs) are not in the counters; the audit counts
them from the code.

## Comparing with Linux

`scripts/bench.sh --linux` runs the same `iobench` (built static with musl) in a Linux guest
(`scripts/linux-guest.nix`: the nixpkgs kernel, BusyBox, the virtio and ext2 modules) with the
same QEMU configuration and a data disk made the same way; the counter columns are `-` there.

## Validity

The host must be idle while a benchmark runs. Runs of 2026-10-07 (and the first one of phase
R1) were taken while a compiler kept every host CPU busy and are deleted; every stage up to
R5 was measured again on an idle host on 2026-10-08 (the files ending in `-quiet`).

## I/O rings step 4: `/data` from the Linux server's page cache

`2026-10-09-802b396-quiet.md` against `2026-10-08-2589c94-slab.md` (before step 4) and Linux
(`2026-10-08-linux-6.18.54-quiet.md`). It also includes the fair scheduler, AF_UNIX, and the
leak fixes:

| benchmark | before | now | Linux |
|---|---:|---:|---:|
| `seq_write` (MB/s) | 4.1 | 137.7 | 208.1 |
| `seq_read_disk` (MB/s) | 586.5 | 615.3 | 839.5 |
| `seq_read_cached` (MB/s) | 7131.1 | 8309.8 | 6097.8 |
| `read_4k_cached` p50 (ns) | 2680 | 2503 | 1114 |
| `read_4k_disk` p50 (ns) | 28348 | 26356 | 18627 |
| `fstat_disk` p50 (cycles) | 14983 | 14072 | 409 |
| `stat_path_tmpfs` p50 (cycles) | 4436 | 5143 | - |
| `tcp_loopback` (MB/s) | 619.4 | 647.1 | 5831.5 |

What this shows:

- **Sequential writes are 33× faster.** Writes now go into the server's page cache, with
  ext2 blocks promised up front, instead of each `write` waiting for diskfs.
- **`fstat` on `/data` costs no IPC any more** (0 IPC calls per operation). It still costs
  10 system calls per operation, the forwarding. That is where the remaining gap to Linux
  is, not the disk path.
- **Path resolution in tmpfs got 16 % slower** (4436 → 5143 cycles). Fixed since; see the
  next section.

## Path resolution: the server's lock bookkeeping

`2026-10-09-1337e20-statpath.md` against `2026-10-09-802b396-quiet.md` (host load about 1:
other builds ran).

The cause of the regression above was the priority-inversion boost of the fair scheduler
(`769bad4`): each server lock counted itself in the thread's State page with `fetch_add`
and `fetch_sub`, two locked instructions per lock. A `stat` of a four-name tmpfs path took
48 server locks (most of them the heap's slab locks), `fstat` of a tmpfs file 5. Bisected
over the merges (only the merge with the fair scheduler moved `stat_path_tmpfs` relative
to `forwarded_null_syscall`), then measured in one boot with the server switching between
variants every round (paired medians of 41 rounds): no count at all saved 743 cycles per
`stat`, a count by plain load and store 689, the size of the regression.

The fix (`b223846`): only the thread itself writes its count (the kernel zeroes it before
handing out the slot and only reads it after), so a relaxed load and store count exactly,
with compiler fences keeping the count around the critical section. A second step
(`1337e20`) took locks and copies off the path: `resolve` no longer copies the components
twice per walk, a tmpfs inode's file type is fixed and read without its lock, `status`
takes one lock instead of three, and `fstat` looks the descriptor up once (it was twice: a
kernel call less, also for `/data`). Now 22 locks per `stat`, 2 per `fstat`.

| benchmark | 802b396 | 1337e20 |
|---|---:|---:|
| `stat_path_tmpfs` p50 (cycles) | 5143 | 3386 |
| `stat_path_tmpfs` p99 (cycles) | 6824 | 3509 |
| `fstat_tmpfs` p50 (cycles) | 2927 | 2385 |
| `fstat_disk` p50 (cycles) | 14072 | 13919 |
| `fstat_disk` system calls per operation | 10 | 9 |

Interleaved runs on the same host (A B C A B C): `1e89b0e` (before) 5188 and 5241 cycles,
`b223846` (the count fixed) 4467 and 4734, `1337e20` 3384 and 3545.

## R7b: internet sockets in the Linux server

`2026-10-09-714815d-r7b-final.md` against `2026-10-09-802b396-quiet.md`. The host was not
entirely idle (load about 1.4 on 16 threads: another guest), but the figures this change does
not touch came out as before (`seq_read_cached` 8372 MB/s, `null_syscall` p50 2361 cycles).

| benchmark | before | now | Linux |
|---|---:|---:|---:|
| `tcp_loopback` (MB/s) | 647.1 | 1240.6 | 5831.5 |
| `tcp_network_echo` (MB/s) | 112.2 | 180.2 | - |

Per 64 KiB written over loopback: 34 system calls instead of 112, no IPC call instead of 8 (and
129 bytes copied by the kernel instead of 313217): the data moves between the program, the
server's rings and netd without the kernel, and no request per `send` or `recv`. What is left
of the gap to Linux is netd's per-segment work (smoltcp over an emulated Ethernet loopback with
1500-byte frames) and the two copies in netd (rings ↔ smoltcp).

After the review (`2026-10-09-40aac17-r7b-review.md`: netd's buffers follow use, starting at
Linux's first sizes and growing to 1 MiB; the loopback pushes back instead of dropping frames)
`tcp_loopback` stays at 1209 MB/s and `tcp_network_echo` at 158 MB/s on a busier host (load
3.5: the disk figures of that run are low for the same reason). Starting connections at 4 KiB
instead cost half the loopback throughput (565 MB/s: the growth took longer than the transfer)
and, against QEMU's user network, a one-second stall (its TCP waits for a larger window).
After the second review (`2026-10-09-29b0502-r7b-rereview.md`: real randomness, RFC 6528
sequence numbers, half-open connections with small buffers, TIME-WAIT kept in smoltcp):
`tcp_loopback` 1192 MB/s, `tcp_network_echo` 176 MB/s (host load about 3.7).

## R6e: the descriptor table in the Linux server

`2026-10-09-27a096a-r6e.md` against `2026-10-09-714815d-r7b-final.md`: no kernel call for a
descriptor's lookup (`kfd_lookup` and its pin), a readiness change (`kfd_ready`) or dup,
close and fcntl any more.

| per operation | before | now |
|---|---:|---:|
| `fstat_tmpfs` system calls | 5 | 3 |
| `fstat_tmpfs` p50 (cycles) | 2940 | 1941 |
| `fstat_disk` system calls | 10 | 8 |
| `read_4k_cached` system calls | 10 | 9 |
| `seq_read_cached_64k` system calls | 4.02 | 3.02 |
| `tcp_loopback_64k` system calls | 33.96 | 22.11 |
| `tcp_network_echo_64k` system calls | 153.05 | 39.09 |
| kernel heap allocations per `fstat` | 1 | 0 |

What `fstat_disk` keeps is /data's own (its status asked of diskfs over the ring), no
bridge. `tcp_network_echo` rose to 213 MB/s (from 180); `tcp_loopback` (1131 MB/s against
1241) is within this host's spread for it.

## Open: PCIDs and small cached reads

Turning PCIDs on (commit `fae8292`, before restricted mode) made random 4 KiB reads from the
page cache 2.5× slower (968 → 2524 ns p50), while null system calls, `fstat`, sequential
reads and the cost per kernel entry stayed the same; the same commit with PCIDs left off
measures 1022 ns, and `-cpu max` alone changes nothing (978 ns at the baseline). The direct
map of physical memory uses 2 MiB pages, so direct-map TLB misses do not explain it. Since
R1, the figure is dominated by forwarding instead (pread 3932 cycles, `clock_gettime` 1770,
p50, at `92bb179`; without PCIDs 5689 and 2573).
