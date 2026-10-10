# Benchmark 4afe2fa (ext3 journal)

- Date: 2026-10-10
- Commit: 4afe2fa (ext2fs: bounded replay, gathered writes, a journal added without caching it)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2 with an ext3 journal, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1513 | cycles |
| null_syscall_p99 | 2160 | cycles |
| forwarded_null_syscall_p50 | 1520 | cycles |
| forwarded_null_syscall_p99 | 2301 | cycles |
| fork_wait_p50 | 97 | us |
| fork_wait_p99 | 221 | us |
| fork_exec_wait_p50 | 147 | us |
| fork_exec_wait_p99 | 247 | us |
| signal_handled_p50 | 5193 | cycles |
| signal_handled_p99 | 10395 | cycles |
| fstat_disk_p50 | 15685 | cycles |
| fstat_disk_p99 | 33862 | cycles |
| fstat_tmpfs_p50 | 2259 | cycles |
| fstat_tmpfs_p99 | 2962 | cycles |
| stat_path_tmpfs_p50 | 3683 | cycles |
| stat_path_tmpfs_p99 | 5384 | cycles |
| proc_meminfo_pread_p50 | 38806 | cycles |
| proc_meminfo_pread_p99 | 124710 | cycles |
| proc_self_stat_pread_p50 | 5665 | cycles |
| proc_self_stat_pread_p99 | 12756 | cycles |
| proc_self_stat_open_read_close_p50 | 18089 | cycles |
| proc_self_stat_open_read_close_p99 | 39755 | cycles |
| seq_write | 110.4 | MB/s |
| seq_read_disk | 721.7 | MB/s |
| seq_read_cached | 7867.6 | MB/s |
| read_4k_cached_p50 | 2294 | ns |
| read_4k_cached_p99 | 6207 | ns |
| pread_4k_cached_p50 | 3231 | cycles |
| pread_4k_cached_p99 | 5970 | cycles |
| clock_gettime_p50 | 1742 | cycles |
| clock_gettime_p99 | 2196 | cycles |
| read_4k_disk_p50 | 27857 | ns |
| read_4k_disk_p99 | 190680 | ns |
| create_disk_p50 | 5910061 | ns |
| create_disk_p99 | 13383150 | ns |
| rename_disk_p50 | 5730139 | ns |
| rename_disk_p99 | 8108016 | ns |
| unlink_disk_p50 | 9961076 | ns |
| unlink_disk_p99 | 14865460 | ns |
| write_fsync_4k_p50 | 6989113 | ns |
| write_fsync_4k_p99 | 10768789 | ns |
| tcp_loopback | 1187.5 | MB/s |
| tcp_network_echo | 197.8 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 48.23 | 0.00 | 0.00 | 33.05 | 4.04 | 20.03 |
| fork_exec_wait | 79.89 | 0.00 | 0.00 | 49.42 | 8.04 | 27.98 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 8.00 | 0.00 | 0.00 | 3.98 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 436.27 | 0.00 | 0.00 | 4.06 | 131201.06 | 6.35 |
| seq_read_disk_64k | 10.04 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 0.99 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 16.00 | 0.00 | 0.00 | 8.00 | 0.01 | 2.00 |
| create_disk | 14805.92 | 0.00 | 0.00 | 15.82 | 4.03 | 3.99 |
| rename_disk | 13974.85 | 0.00 | 0.00 | 11.88 | 0.03 | 2.99 |
| unlink_disk | 25370.08 | 0.00 | 0.00 | 11.91 | 0.03 | 2.99 |
| write_fsync_4k | 15394.11 | 0.00 | 0.00 | 15.17 | 4120.04 | 10.13 |
| tcp_loopback_64k | 20.01 | 0.00 | 0.00 | 12.58 | 0.02 | 4.31 |
| tcp_network_echo_64k | 67.09 | 0.00 | 0.00 | 17.36 | 0.69 | 5.25 |
