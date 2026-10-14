# Benchmark 9fe966d (ab-coherence-1)

- Date: 2026-10-15
- Commit: 9fe966d (treetest: /data coherence across process trees)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2 with an ext3 journal, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1261 | cycles |
| null_syscall_p99 | 1278 | cycles |
| forwarded_null_syscall_p50 | 1310 | cycles |
| forwarded_null_syscall_p99 | 1601 | cycles |
| fork_wait_p50 | 98 | us |
| fork_wait_p99 | 155 | us |
| fork_exec_wait_p50 | 152 | us |
| fork_exec_wait_p99 | 185 | us |
| signal_handled_p50 | 3981 | cycles |
| signal_handled_p99 | 4764 | cycles |
| fstat_disk_p50 | 13573 | cycles |
| fstat_disk_p99 | 20933 | cycles |
| fstat_tmpfs_p50 | 1650 | cycles |
| fstat_tmpfs_p99 | 1669 | cycles |
| stat_path_tmpfs_p50 | 2976 | cycles |
| stat_path_tmpfs_p99 | 4253 | cycles |
| proc_meminfo_pread_p50 | 30562 | cycles |
| proc_meminfo_pread_p99 | 42880 | cycles |
| proc_self_stat_pread_p50 | 5451 | cycles |
| proc_self_stat_pread_p99 | 6906 | cycles |
| proc_self_stat_open_read_close_p50 | 14133 | cycles |
| proc_self_stat_open_read_close_p99 | 18482 | cycles |
| seq_write | 130.0 | MB/s |
| seq_read_disk | 932.4 | MB/s |
| seq_read_cached | 8517.4 | MB/s |
| read_4k_cached_p50 | 1989 | ns |
| read_4k_cached_p99 | 2608 | ns |
| pread_4k_cached_p50 | 2737 | cycles |
| pread_4k_cached_p99 | 3749 | cycles |
| clock_gettime_p50 | 1519 | cycles |
| clock_gettime_p99 | 1534 | cycles |
| read_4k_disk_p50 | 27400 | ns |
| read_4k_disk_p99 | 143304 | ns |
| create_disk_p50 | 2534660 | ns |
| create_disk_p99 | 7665674 | ns |
| rename_disk_p50 | 2616800 | ns |
| rename_disk_p99 | 3386573 | ns |
| unlink_disk_p50 | 4893144 | ns |
| unlink_disk_p99 | 6436985 | ns |
| write_fsync_4k_p50 | 4689812 | ns |
| write_fsync_4k_p99 | 5674301 | ns |
| tcp_loopback | 1200.3 | MB/s |
| tcp_network_echo | 174.9 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 50.27 | 0.00 | 0.00 | 32.98 | 4.04 | 21.00 |
| fork_exec_wait | 89.53 | 0.00 | 0.00 | 52.70 | 8.04 | 30.20 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 9.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 298.10 | 0.00 | 0.00 | 4.06 | 131201.06 | 6.36 |
| seq_read_disk_64k | 11.05 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 1.00 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 17.00 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| create_disk | 5926.99 | 0.00 | 0.00 | 15.99 | 4.02 | 4.00 |
| rename_disk | 5602.93 | 0.00 | 0.00 | 11.99 | 0.03 | 3.00 |
| unlink_disk | 10889.27 | 0.00 | 0.00 | 12.01 | 0.03 | 3.00 |
| write_fsync_4k | 11516.08 | 0.00 | 0.00 | 16.14 | 4120.04 | 10.68 |
| tcp_loopback_64k | 19.92 | 0.00 | 0.00 | 13.08 | 0.02 | 4.53 |
| tcp_network_echo_64k | 153.67 | 0.00 | 0.00 | 20.97 | 1.19 | 6.81 |
