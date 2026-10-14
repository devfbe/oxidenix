# Benchmark 9fe966d (ab-coherence-2)

- Date: 2026-10-15
- Commit: 9fe966d (treetest: /data coherence across process trees)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2 with an ext3 journal, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1261 | cycles |
| null_syscall_p99 | 1517 | cycles |
| forwarded_null_syscall_p50 | 1305 | cycles |
| forwarded_null_syscall_p99 | 1585 | cycles |
| fork_wait_p50 | 92 | us |
| fork_wait_p99 | 202 | us |
| fork_exec_wait_p50 | 152 | us |
| fork_exec_wait_p99 | 227 | us |
| signal_handled_p50 | 3984 | cycles |
| signal_handled_p99 | 5115 | cycles |
| fstat_disk_p50 | 13556 | cycles |
| fstat_disk_p99 | 20890 | cycles |
| fstat_tmpfs_p50 | 1652 | cycles |
| fstat_tmpfs_p99 | 1949 | cycles |
| stat_path_tmpfs_p50 | 2972 | cycles |
| stat_path_tmpfs_p99 | 3182 | cycles |
| proc_meminfo_pread_p50 | 30749 | cycles |
| proc_meminfo_pread_p99 | 41321 | cycles |
| proc_self_stat_pread_p50 | 5439 | cycles |
| proc_self_stat_pread_p99 | 6916 | cycles |
| proc_self_stat_open_read_close_p50 | 14111 | cycles |
| proc_self_stat_open_read_close_p99 | 18034 | cycles |
| seq_write | 122.8 | MB/s |
| seq_read_disk | 834.0 | MB/s |
| seq_read_cached | 8858.9 | MB/s |
| read_4k_cached_p50 | 1994 | ns |
| read_4k_cached_p99 | 2594 | ns |
| pread_4k_cached_p50 | 2691 | cycles |
| pread_4k_cached_p99 | 3373 | cycles |
| clock_gettime_p50 | 1504 | cycles |
| clock_gettime_p99 | 1530 | cycles |
| read_4k_disk_p50 | 27570 | ns |
| read_4k_disk_p99 | 157921 | ns |
| create_disk_p50 | 2501050 | ns |
| create_disk_p99 | 7493752 | ns |
| rename_disk_p50 | 2617967 | ns |
| rename_disk_p99 | 3390960 | ns |
| unlink_disk_p50 | 4879013 | ns |
| unlink_disk_p99 | 7641041 | ns |
| write_fsync_4k_p50 | 4633529 | ns |
| write_fsync_4k_p99 | 5914043 | ns |
| tcp_loopback | 887.5 | MB/s |
| tcp_network_echo | 182.2 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 51.29 | 0.00 | 0.00 | 33.09 | 4.04 | 20.95 |
| fork_exec_wait | 89.76 | 0.00 | 0.00 | 52.85 | 8.04 | 30.09 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 9.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 430.68 | 0.00 | 0.00 | 4.05 | 131201.06 | 6.36 |
| seq_read_disk_64k | 11.07 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 1.00 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 17.00 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| create_disk | 5703.69 | 0.00 | 0.00 | 16.00 | 4.02 | 4.00 |
| rename_disk | 5604.61 | 0.00 | 0.00 | 11.99 | 0.03 | 3.00 |
| unlink_disk | 11028.37 | 0.00 | 0.00 | 12.00 | 0.03 | 3.00 |
| write_fsync_4k | 11204.49 | 0.00 | 0.00 | 15.76 | 4120.04 | 10.55 |
| tcp_loopback_64k | 24.63 | 0.00 | 0.00 | 14.21 | 0.04 | 5.13 |
| tcp_network_echo_64k | 99.28 | 0.00 | 0.00 | 23.56 | 1.19 | 7.95 |
