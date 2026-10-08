# Benchmark 841f4cf (baseline-quiet)

- Date: 2026-10-08
- Commit: 841f4cf (feat: compare the benchmarks with a Linux guest (scripts/bench.sh --linux))
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.3-zen1
- QEMU: QEMU emulator version 10.2.4, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 358 | cycles |
| null_syscall_p99 | 452 | cycles |
| fstat_disk_p50 | 14429 | cycles |
| fstat_disk_p99 | 23799 | cycles |
| fstat_tmpfs_p50 | 652 | cycles |
| fstat_tmpfs_p99 | 946 | cycles |
| seq_write | 4.3 | MB/s |
| seq_read_disk | 369.0 | MB/s |
| seq_read_cached | 8059.5 | MB/s |
| read_4k_cached_p50 | 968 | ns |
| read_4k_cached_p99 | 1419 | ns |
| read_4k_disk_p50 | 29031 | ns |
| read_4k_disk_p99 | 117166 | ns |
| tcp_loopback | 621.1 | MB/s |
| tcp_network_echo | 106.1 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |
| fstat_disk | 3.00 | 1.00 | 96.00 | 2.01 | 248.00 | 2.00 |
| fstat_tmpfs | 1.18 | 0.00 | 0.00 | 0.00 | 145.46 | 0.00 |
| seq_write_64k | 50221.15 | 2.00 | 65728.07 | 4.09 | 533555.39 | 7.64 |
| seq_read_disk_64k | 204.03 | 2.00 | 65728.09 | 4.00 | 132872.49 | 6.98 |
| seq_read_cached_64k | 1.01 | 0.00 | 0.09 | 0.00 | 65536.30 | 0.98 |
| read_4k_cached | 3.00 | 0.00 | 0.01 | 0.00 | 4128.02 | 1.00 |
| read_4k_disk | 21.84 | 1.00 | 4192.01 | 2.00 | 8462.74 | 4.00 |
| tcp_loopback_64k | 145.88 | 8.01 | 180992.69 | 14.40 | 313926.89 | 33.00 |
| tcp_network_echo_64k | 96.28 | 8.11 | 182227.94 | 14.28 | 314362.56 | 30.11 |
