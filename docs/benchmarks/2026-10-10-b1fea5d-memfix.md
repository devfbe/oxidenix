# Benchmark b1fea5d (memfix)

- Date: 2026-10-10
- Commit: b1fea5d (Merge main into memfix)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1671 | cycles |
| null_syscall_p99 | 2042 | cycles |
| forwarded_null_syscall_p50 | 1698 | cycles |
| forwarded_null_syscall_p99 | 2088 | cycles |
| fork_wait_p50 | 95 | us |
| fork_wait_p99 | 134 | us |
| fork_exec_wait_p50 | 146 | us |
| fork_exec_wait_p99 | 207 | us |
| signal_handled_p50 | 5127 | cycles |
| signal_handled_p99 | 9358 | cycles |
| fstat_disk_p50 | 19075 | cycles |
| fstat_disk_p99 | 26209 | cycles |
| fstat_tmpfs_p50 | 2072 | cycles |
| fstat_tmpfs_p99 | 2397 | cycles |
| stat_path_tmpfs_p50 | 3693 | cycles |
| stat_path_tmpfs_p99 | 4177 | cycles |
| proc_meminfo_pread_p50 | 38170 | cycles |
| proc_meminfo_pread_p99 | 52055 | cycles |
| proc_self_stat_pread_p50 | 6981 | cycles |
| proc_self_stat_pread_p99 | 8209 | cycles |
| proc_self_stat_open_read_close_p50 | 17309 | cycles |
| proc_self_stat_open_read_close_p99 | 27645 | cycles |
| seq_write | 121.6 | MB/s |
| seq_read_disk | 669.4 | MB/s |
| seq_read_cached | 4575.1 | MB/s |
| read_4k_cached_p50 | 2700 | ns |
| read_4k_cached_p99 | 8605 | ns |
| pread_4k_cached_p50 | 3408 | cycles |
| pread_4k_cached_p99 | 4198 | cycles |
| clock_gettime_p50 | 2071 | cycles |
| clock_gettime_p99 | 2204 | cycles |
| read_4k_disk_p50 | 30908 | ns |
| read_4k_disk_p99 | 144466 | ns |
| tcp_loopback | 879.2 | MB/s |
| tcp_network_echo | 183.3 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 46.47 | 0.00 | 0.00 | 32.68 | 4.04 | 19.33 |
| fork_exec_wait | 79.29 | 0.00 | 0.00 | 49.33 | 8.04 | 27.71 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 8.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 3.99 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 300.88 | 0.00 | 0.00 | 4.05 | 131201.06 | 6.36 |
| seq_read_disk_64k | 10.04 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 1.00 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 16.00 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| tcp_loopback_64k | 22.27 | 0.00 | 0.00 | 13.12 | 0.02 | 4.53 |
| tcp_network_echo_64k | 99.55 | 0.00 | 0.00 | 18.61 | 0.56 | 5.78 |
