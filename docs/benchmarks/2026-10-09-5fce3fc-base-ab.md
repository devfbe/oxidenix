# Benchmark 5fce3fc (base-ab)

- Date: 2026-10-09
- Commit: 5fce3fc (Merge R7b follow-ups: reserves for late instances, RFC 6056 alg. 4, SYN/RST and TIME-WAIT fixes, heap bound, RNG checks)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 2343 | cycles |
| null_syscall_p99 | 5088 | cycles |
| forwarded_null_syscall_p50 | 1511 | cycles |
| forwarded_null_syscall_p99 | 2500 | cycles |
| fstat_disk_p50 | 16453 | cycles |
| fstat_disk_p99 | 34809 | cycles |
| fstat_tmpfs_p50 | 2402 | cycles |
| fstat_tmpfs_p99 | 3500 | cycles |
| stat_path_tmpfs_p50 | 4094 | cycles |
| stat_path_tmpfs_p99 | 6578 | cycles |
| proc_meminfo_pread_p50 | 37305 | cycles |
| proc_meminfo_pread_p99 | 71237 | cycles |
| proc_self_stat_pread_p50 | 6926 | cycles |
| proc_self_stat_pread_p99 | 18315 | cycles |
| proc_self_stat_open_read_close_p50 | 23412 | cycles |
| proc_self_stat_open_read_close_p99 | 51456 | cycles |
| seq_write | 115.0 | MB/s |
| seq_read_disk | 387.3 | MB/s |
| seq_read_cached | 5818.7 | MB/s |
| read_4k_cached_p50 | 3719 | ns |
| read_4k_cached_p99 | 12823 | ns |
| pread_4k_cached_p50 | 3738 | cycles |
| pread_4k_cached_p99 | 11584 | cycles |
| clock_gettime_p50 | 1945 | cycles |
| clock_gettime_p99 | 2271 | cycles |
| read_4k_disk_p50 | 31308 | ns |
| read_4k_disk_p99 | 175936 | ns |
| tcp_loopback | 1152.0 | MB/s |
| tcp_network_echo | 161.1 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 3.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fstat_disk | 9.00 | 0.00 | 0.00 | 3.98 | 0.00 | 1.00 |
| fstat_tmpfs | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 1.00 |
| stat_path_tmpfs | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 12.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 6.00 | 0.00 | 0.00 | 2.00 | 0.00 | 2.00 |
| proc_self_stat_open_read_close | 17.00 | 0.00 | 0.00 | 8.00 | 0.00 | 8.00 |
| seq_write_64k | 264.91 | 0.00 | 0.00 | 4.08 | 133756.75 | 6.35 |
| seq_read_disk_64k | 11.32 | 0.00 | 0.00 | 4.02 | 0.53 | 2.01 |
| seq_read_cached_64k | 4.04 | 0.00 | 0.00 | 2.02 | 65536.03 | 2.00 |
| read_4k_cached | 10.00 | 0.00 | 0.00 | 6.00 | 4096.00 | 2.00 |
| read_4k_disk | 17.07 | 0.00 | 0.00 | 8.00 | 0.00 | 2.00 |
| tcp_loopback_64k | 27.49 | 0.00 | 0.00 | 13.66 | 83.59 | 2.54 |
| tcp_network_echo_64k | 100.33 | 0.00 | 0.00 | 18.88 | 1163.12 | 2.84 |
