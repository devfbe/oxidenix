# Benchmark bdf16a2 (pre-procfs-rings)

- Date: 2026-10-09
- Commit: bdf16a2 (iobench: /proc reads (procfs's meminfo, a process's stat))
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 3784 | cycles |
| null_syscall_p99 | 5145 | cycles |
| forwarded_null_syscall_p50 | 2389 | cycles |
| forwarded_null_syscall_p99 | 3213 | cycles |
| fstat_disk_p50 | 20411 | cycles |
| fstat_disk_p99 | 60351 | cycles |
| fstat_tmpfs_p50 | 2383 | cycles |
| fstat_tmpfs_p99 | 2960 | cycles |
| stat_path_tmpfs_p50 | 4673 | cycles |
| stat_path_tmpfs_p99 | 5164 | cycles |
| proc_meminfo_pread_p50 | 54652 | cycles |
| proc_meminfo_pread_p99 | 80019 | cycles |
| proc_self_stat_pread_p50 | 46976 | cycles |
| proc_self_stat_pread_p99 | 75749 | cycles |
| proc_self_stat_open_read_close_p50 | 300685 | cycles |
| proc_self_stat_open_read_close_p99 | 487471 | cycles |
| seq_write | 106.3 | MB/s |
| seq_read_disk | 624.6 | MB/s |
| seq_read_cached | 6495.3 | MB/s |
| read_4k_cached_p50 | 2972 | ns |
| read_4k_cached_p99 | 3626 | ns |
| pread_4k_cached_p50 | 5420 | cycles |
| pread_4k_cached_p99 | 9625 | cycles |
| clock_gettime_p50 | 2153 | cycles |
| clock_gettime_p99 | 2171 | cycles |
| read_4k_disk_p50 | 32813 | ns |
| read_4k_disk_p99 | 155569 | ns |
| tcp_loopback | 984.0 | MB/s |
| tcp_network_echo | 121.7 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 3.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fstat_disk | 9.01 | 0.00 | 0.00 | 3.99 | 0.02 | 1.00 |
| fstat_tmpfs | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 1.00 |
| stat_path_tmpfs | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 10.00 | 2.00 | 777.01 | 8.01 | 2018.01 | 7.00 |
| proc_self_stat_pread | 13.00 | 2.00 | 337.01 | 8.02 | 1570.02 | 10.00 |
| proc_self_stat_open_read_close | 74.00 | 17.00 | 1787.01 | 44.13 | 4388.02 | 57.00 |
| seq_write_64k | 279.78 | 0.00 | 0.08 | 4.10 | 133890.54 | 6.32 |
| seq_read_disk_64k | 11.04 | 0.00 | 0.08 | 4.02 | 0.66 | 1.98 |
| seq_read_cached_64k | 4.02 | 0.00 | 0.08 | 2.02 | 65536.16 | 1.98 |
| read_4k_cached | 10.00 | 0.00 | 0.01 | 6.00 | 4096.02 | 2.00 |
| read_4k_disk | 17.00 | 0.00 | 0.01 | 8.00 | 0.02 | 2.00 |
| tcp_loopback_64k | 28.86 | 0.00 | 0.04 | 14.31 | 92.27 | 2.66 |
| tcp_network_echo_64k | 83.80 | 0.00 | 0.34 | 22.39 | 829.31 | 2.70 |
