# Benchmark 9fe966d (ab-coherence-3)

- Date: 2026-10-15
- Commit: 9fe966d (treetest: /data coherence across process trees)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2 with an ext3 journal, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1261 | cycles |
| null_syscall_p99 | 1485 | cycles |
| forwarded_null_syscall_p50 | 1308 | cycles |
| forwarded_null_syscall_p99 | 1589 | cycles |
| fork_wait_p50 | 91 | us |
| fork_wait_p99 | 185 | us |
| fork_exec_wait_p50 | 152 | us |
| fork_exec_wait_p99 | 212 | us |
| signal_handled_p50 | 3968 | cycles |
| signal_handled_p99 | 5041 | cycles |
| fstat_disk_p50 | 14147 | cycles |
| fstat_disk_p99 | 20927 | cycles |
| fstat_tmpfs_p50 | 1654 | cycles |
| fstat_tmpfs_p99 | 2082 | cycles |
| stat_path_tmpfs_p50 | 2974 | cycles |
| stat_path_tmpfs_p99 | 4256 | cycles |
| proc_meminfo_pread_p50 | 30690 | cycles |
| proc_meminfo_pread_p99 | 43103 | cycles |
| proc_self_stat_pread_p50 | 5445 | cycles |
| proc_self_stat_pread_p99 | 7985 | cycles |
| proc_self_stat_open_read_close_p50 | 14047 | cycles |
| proc_self_stat_open_read_close_p99 | 18739 | cycles |
| seq_write | 132.0 | MB/s |
| seq_read_disk | 990.6 | MB/s |
| seq_read_cached | 7794.5 | MB/s |
| read_4k_cached_p50 | 1996 | ns |
| read_4k_cached_p99 | 2722 | ns |
| pread_4k_cached_p50 | 2709 | cycles |
| pread_4k_cached_p99 | 3603 | cycles |
| clock_gettime_p50 | 1525 | cycles |
| clock_gettime_p99 | 1542 | cycles |
| read_4k_disk_p50 | 27551 | ns |
| read_4k_disk_p99 | 93911 | ns |
| create_disk_p50 | 2504698 | ns |
| create_disk_p99 | 7896536 | ns |
| rename_disk_p50 | 2622677 | ns |
| rename_disk_p99 | 4340443 | ns |
| unlink_disk_p50 | 4968923 | ns |
| unlink_disk_p99 | 6595713 | ns |
| write_fsync_4k_p50 | 4782726 | ns |
| write_fsync_4k_p99 | 6049153 | ns |
| tcp_loopback | 1216.1 | MB/s |
| tcp_network_echo | 187.4 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 50.38 | 0.00 | 0.00 | 32.92 | 4.04 | 20.74 |
| fork_exec_wait | 89.84 | 0.00 | 0.00 | 52.81 | 8.04 | 30.16 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 9.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 335.78 | 0.00 | 0.00 | 4.07 | 131201.06 | 6.36 |
| seq_read_disk_64k | 11.05 | 0.00 | 0.00 | 4.01 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.01 | 65536.03 | 1.00 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 17.00 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| create_disk | 5799.26 | 0.00 | 0.00 | 15.99 | 4.02 | 4.00 |
| rename_disk | 5683.55 | 0.00 | 0.00 | 11.99 | 0.03 | 3.00 |
| unlink_disk | 11038.23 | 0.00 | 0.00 | 12.00 | 0.03 | 3.00 |
| write_fsync_4k | 11118.61 | 0.00 | 0.00 | 16.05 | 4120.04 | 10.65 |
| tcp_loopback_64k | 21.11 | 0.00 | 0.00 | 13.87 | 0.02 | 4.98 |
| tcp_network_echo_64k | 123.73 | 0.00 | 0.00 | 23.64 | 0.81 | 7.86 |
