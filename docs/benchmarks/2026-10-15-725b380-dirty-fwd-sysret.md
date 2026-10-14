# Benchmark 725b380-dirty (fwd-sysret)

- Date: 2026-10-15
- Commit: 725b380-dirty (Merge an OOM killer by badness and Linux's overcommit modes (ADR 0013))
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2 with an ext3 journal, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1281 | cycles |
| null_syscall_p99 | 1557 | cycles |
| forwarded_null_syscall_p50 | 1329 | cycles |
| forwarded_null_syscall_p99 | 1588 | cycles |
| fork_wait_p50 | 94 | us |
| fork_wait_p99 | 147 | us |
| fork_exec_wait_p50 | 153 | us |
| fork_exec_wait_p99 | 221 | us |
| signal_handled_p50 | 3984 | cycles |
| signal_handled_p99 | 4828 | cycles |
| fstat_disk_p50 | 12984 | cycles |
| fstat_disk_p99 | 19888 | cycles |
| fstat_tmpfs_p50 | 1678 | cycles |
| fstat_tmpfs_p99 | 1994 | cycles |
| stat_path_tmpfs_p50 | 3003 | cycles |
| stat_path_tmpfs_p99 | 3165 | cycles |
| proc_meminfo_pread_p50 | 30344 | cycles |
| proc_meminfo_pread_p99 | 37247 | cycles |
| proc_self_stat_pread_p50 | 5483 | cycles |
| proc_self_stat_pread_p99 | 6792 | cycles |
| proc_self_stat_open_read_close_p50 | 14218 | cycles |
| proc_self_stat_open_read_close_p99 | 20124 | cycles |
| seq_write | 135.1 | MB/s |
| seq_read_disk | 980.1 | MB/s |
| seq_read_cached | 8001.9 | MB/s |
| read_4k_cached_p50 | 2002 | ns |
| read_4k_cached_p99 | 2555 | ns |
| pread_4k_cached_p50 | 2673 | cycles |
| pread_4k_cached_p99 | 3331 | cycles |
| clock_gettime_p50 | 1535 | cycles |
| clock_gettime_p99 | 1556 | cycles |
| read_4k_disk_p50 | 27394 | ns |
| read_4k_disk_p99 | 132649 | ns |
| create_disk_p50 | 2561973 | ns |
| create_disk_p99 | 7919164 | ns |
| rename_disk_p50 | 2627454 | ns |
| rename_disk_p99 | 3466648 | ns |
| unlink_disk_p50 | 4977164 | ns |
| unlink_disk_p99 | 6511301 | ns |
| write_fsync_4k_p50 | 4742402 | ns |
| write_fsync_4k_p99 | 5835773 | ns |
| tcp_loopback | 1216.1 | MB/s |
| tcp_network_echo | 175.9 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 50.34 | 0.00 | 0.00 | 32.99 | 4.04 | 20.88 |
| fork_exec_wait | 89.77 | 0.00 | 0.00 | 52.83 | 8.04 | 30.21 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 8.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 329.02 | 0.00 | 0.00 | 4.07 | 131201.06 | 6.35 |
| seq_read_disk_64k | 10.05 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 1.00 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 16.00 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| create_disk | 5773.22 | 0.00 | 0.00 | 16.00 | 4.02 | 4.00 |
| rename_disk | 5460.83 | 0.00 | 0.00 | 11.98 | 0.03 | 3.00 |
| unlink_disk | 11113.11 | 0.00 | 0.00 | 12.00 | 0.03 | 3.00 |
| write_fsync_4k | 11212.44 | 0.00 | 0.00 | 16.04 | 4120.04 | 10.61 |
| tcp_loopback_64k | 21.14 | 0.00 | 0.00 | 13.80 | 0.02 | 4.79 |
| tcp_network_echo_64k | 100.86 | 0.00 | 0.00 | 20.61 | 0.94 | 6.62 |
