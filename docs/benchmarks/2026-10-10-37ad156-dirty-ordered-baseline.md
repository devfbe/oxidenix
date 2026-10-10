# Benchmark 37ad156-dirty (the ordered design: main before the journal, with 9d468e6's iobench)

- Date: 2026-10-10
- Commit: 37ad156-dirty (docs: regenerate the code map after merging)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1404 | cycles |
| null_syscall_p99 | 2126 | cycles |
| forwarded_null_syscall_p50 | 1701 | cycles |
| forwarded_null_syscall_p99 | 2100 | cycles |
| fork_wait_p50 | 99 | us |
| fork_wait_p99 | 190 | us |
| fork_exec_wait_p50 | 148 | us |
| fork_exec_wait_p99 | 247 | us |
| signal_handled_p50 | 5144 | cycles |
| signal_handled_p99 | 10058 | cycles |
| fstat_disk_p50 | 18999 | cycles |
| fstat_disk_p99 | 42851 | cycles |
| fstat_tmpfs_p50 | 2245 | cycles |
| fstat_tmpfs_p99 | 2629 | cycles |
| stat_path_tmpfs_p50 | 3332 | cycles |
| stat_path_tmpfs_p99 | 6799 | cycles |
| proc_meminfo_pread_p50 | 36795 | cycles |
| proc_meminfo_pread_p99 | 108678 | cycles |
| proc_self_stat_pread_p50 | 6739 | cycles |
| proc_self_stat_pread_p99 | 10221 | cycles |
| proc_self_stat_open_read_close_p50 | 17341 | cycles |
| proc_self_stat_open_read_close_p99 | 25814 | cycles |
| seq_write | 107.8 | MB/s |
| seq_read_disk | 437.8 | MB/s |
| seq_read_cached | 6956.2 | MB/s |
| read_4k_cached_p50 | 2781 | ns |
| read_4k_cached_p99 | 9868 | ns |
| pread_4k_cached_p50 | 3791 | cycles |
| pread_4k_cached_p99 | 13841 | cycles |
| clock_gettime_p50 | 2122 | cycles |
| clock_gettime_p99 | 6999 | cycles |
| read_4k_disk_p50 | 30521 | ns |
| read_4k_disk_p99 | 193592 | ns |
| create_disk_p50 | 10246603 | ns |
| create_disk_p99 | 19459692 | ns |
| rename_disk_p50 | 14970236 | ns |
| rename_disk_p99 | 43695725 | ns |
| unlink_disk_p50 | 18210868 | ns |
| unlink_disk_p99 | 23618330 | ns |
| write_fsync_4k_p50 | 6842730 | ns |
| write_fsync_4k_p99 | 8703990 | ns |
| tcp_loopback | 1180.7 | MB/s |
| tcp_network_echo | 138.9 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 47.62 | 0.00 | 0.00 | 32.94 | 4.04 | 19.89 |
| fork_exec_wait | 79.27 | 0.00 | 0.00 | 48.77 | 8.04 | 27.77 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 8.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 463.69 | 0.00 | 0.00 | 4.09 | 131201.03 | 6.35 |
| seq_read_disk_64k | 10.27 | 0.00 | 0.00 | 4.02 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 0.99 |
| read_4k_cached | 9.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 1.00 |
| read_4k_disk | 16.03 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| create_disk | 28181.37 | 0.00 | 0.00 | 15.96 | 4.02 | 4.00 |
| rename_disk | 41437.58 | 0.00 | 0.00 | 11.92 | 0.03 | 3.00 |
| unlink_disk | 45024.37 | 0.00 | 0.00 | 11.99 | 0.03 | 3.00 |
| write_fsync_4k | 15703.91 | 0.00 | 0.00 | 15.30 | 4120.04 | 10.16 |
| tcp_loopback_64k | 20.48 | 0.00 | 0.00 | 13.49 | 0.02 | 4.63 |
| tcp_network_echo_64k | 153.19 | 0.00 | 0.00 | 29.72 | 1.06 | 10.17 |
