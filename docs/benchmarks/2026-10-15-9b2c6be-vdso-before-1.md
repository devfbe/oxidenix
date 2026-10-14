# Benchmark 9b2c6be (vdso-before-1)

- Date: 2026-10-15
- Commit: 9b2c6be (iobench: gettimeofday alone, in cycles)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2 with an ext3 journal, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1171 | cycles |
| null_syscall_p99 | 1534 | cycles |
| forwarded_null_syscall_p50 | 1330 | cycles |
| forwarded_null_syscall_p99 | 1585 | cycles |
| fork_wait_p50 | 93 | us |
| fork_wait_p99 | 179 | us |
| fork_exec_wait_p50 | 152 | us |
| fork_exec_wait_p99 | 183 | us |
| signal_handled_p50 | 3988 | cycles |
| signal_handled_p99 | 4719 | cycles |
| fstat_disk_p50 | 13402 | cycles |
| fstat_disk_p99 | 20969 | cycles |
| fstat_tmpfs_p50 | 1686 | cycles |
| fstat_tmpfs_p99 | 1764 | cycles |
| stat_path_tmpfs_p50 | 2987 | cycles |
| stat_path_tmpfs_p99 | 3775 | cycles |
| proc_meminfo_pread_p50 | 36426 | cycles |
| proc_meminfo_pread_p99 | 51160 | cycles |
| proc_self_stat_pread_p50 | 5488 | cycles |
| proc_self_stat_pread_p99 | 6465 | cycles |
| proc_self_stat_open_read_close_p50 | 14114 | cycles |
| proc_self_stat_open_read_close_p99 | 18349 | cycles |
| seq_write | 132.8 | MB/s |
| seq_read_disk | 756.0 | MB/s |
| seq_read_cached | 8209.9 | MB/s |
| read_4k_cached_p50 | 1989 | ns |
| read_4k_cached_p99 | 2743 | ns |
| pread_4k_cached_p50 | 2685 | cycles |
| pread_4k_cached_p99 | 3341 | cycles |
| clock_gettime_p50 | 1536 | cycles |
| clock_gettime_p99 | 1837 | cycles |
| gettimeofday_p50 | 1546 | cycles |
| gettimeofday_p99 | 1848 | cycles |
| read_4k_disk_p50 | 27172 | ns |
| read_4k_disk_p99 | 158852 | ns |
| create_disk_p50 | 2499498 | ns |
| create_disk_p99 | 7621796 | ns |
| rename_disk_p50 | 2579778 | ns |
| rename_disk_p99 | 3393470 | ns |
| unlink_disk_p50 | 4845521 | ns |
| unlink_disk_p99 | 6346194 | ns |
| write_fsync_4k_p50 | 4639141 | ns |
| write_fsync_4k_p99 | 5536638 | ns |
| tcp_loopback | 1193.4 | MB/s |
| tcp_network_echo | 185.0 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 50.19 | 0.00 | 0.00 | 33.35 | 4.04 | 20.87 |
| fork_exec_wait | 89.55 | 0.00 | 0.00 | 52.94 | 8.04 | 30.20 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 8.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 367.00 | 0.00 | 0.00 | 4.06 | 131201.06 | 6.36 |
| seq_read_disk_64k | 10.05 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 1.00 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 16.00 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| create_disk | 5679.99 | 0.00 | 0.00 | 15.99 | 4.02 | 4.00 |
| rename_disk | 5547.75 | 0.00 | 0.00 | 11.99 | 0.03 | 3.00 |
| unlink_disk | 10796.40 | 0.00 | 0.00 | 12.00 | 0.03 | 3.00 |
| write_fsync_4k | 10908.46 | 0.00 | 0.00 | 15.96 | 4120.04 | 10.62 |
| tcp_loopback_64k | 21.06 | 0.00 | 0.00 | 14.15 | 0.02 | 4.99 |
| tcp_network_echo_64k | 148.34 | 0.00 | 0.00 | 23.20 | 1.06 | 7.72 |
