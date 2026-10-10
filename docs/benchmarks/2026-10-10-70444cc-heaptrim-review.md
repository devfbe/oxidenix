# Benchmark 70444cc (heaptrim-review)

- Date: 2026-10-10
- Commit: 70444cc (docs: what an instance's heap holds of the commit limit)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 10.2.4, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1420 | cycles |
| null_syscall_p99 | 1713 | cycles |
| forwarded_null_syscall_p50 | 1443 | cycles |
| forwarded_null_syscall_p99 | 1764 | cycles |
| fork_wait_p50 | 98 | us |
| fork_wait_p99 | 123 | us |
| fork_exec_wait_p50 | 147 | us |
| fork_exec_wait_p99 | 193 | us |
| signal_handled_p50 | 4327 | cycles |
| signal_handled_p99 | 5278 | cycles |
| fstat_disk_p50 | 16926 | cycles |
| fstat_disk_p99 | 25712 | cycles |
| fstat_tmpfs_p50 | 1884 | cycles |
| fstat_tmpfs_p99 | 2355 | cycles |
| stat_path_tmpfs_p50 | 3131 | cycles |
| stat_path_tmpfs_p99 | 4730 | cycles |
| proc_meminfo_pread_p50 | 36165 | cycles |
| proc_meminfo_pread_p99 | 51983 | cycles |
| proc_self_stat_pread_p50 | 5808 | cycles |
| proc_self_stat_pread_p99 | 10285 | cycles |
| proc_self_stat_open_read_close_p50 | 14591 | cycles |
| proc_self_stat_open_read_close_p99 | 26217 | cycles |
| seq_write | 118.6 | MB/s |
| seq_read_disk | 558.9 | MB/s |
| seq_read_cached | 7523.1 | MB/s |
| read_4k_cached_p50 | 2304 | ns |
| read_4k_cached_p99 | 7786 | ns |
| pread_4k_cached_p50 | 3065 | cycles |
| pread_4k_cached_p99 | 4569 | cycles |
| clock_gettime_p50 | 1738 | cycles |
| clock_gettime_p99 | 1754 | cycles |
| read_4k_disk_p50 | 28644 | ns |
| read_4k_disk_p99 | 175472 | ns |
| tcp_loopback | 1234.8 | MB/s |
| tcp_network_echo | 189.6 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 47.94 | 0.00 | 0.00 | 33.28 | 4.04 | 20.18 |
| fork_exec_wait | 80.92 | 0.00 | 0.00 | 49.31 | 8.04 | 27.84 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 8.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 537.82 | 0.00 | 0.00 | 4.08 | 131201.03 | 6.35 |
| seq_read_disk_64k | 10.27 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 0.99 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 16.06 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| tcp_loopback_64k | 21.06 | 0.00 | 0.00 | 14.09 | 0.02 | 4.98 |
| tcp_network_echo_64k | 108.53 | 0.00 | 0.00 | 22.11 | 0.81 | 7.28 |
