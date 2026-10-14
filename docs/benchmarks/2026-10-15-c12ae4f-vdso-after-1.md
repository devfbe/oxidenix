# Benchmark c12ae4f (vdso-after-1)

- Date: 2026-10-15
- Commit: c12ae4f (Docs: the vDSO and the clock page (ADR 0014), /proc/<pid>/maps)
- Host: 12th Gen Intel(R) Core(TM) i5-1240P, 16 threads, Linux 7.2.9-zen1
- QEMU: QEMU emulator version 11.1.1, KVM: yes
- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2 with an ext3 journal, 1 KiB blocks), virtio-net with user networking

## Results

| benchmark | value | unit |
|---|---:|---|
| null_syscall_p50 | 1282 | cycles |
| null_syscall_p99 | 1506 | cycles |
| forwarded_null_syscall_p50 | 1334 | cycles |
| forwarded_null_syscall_p99 | 1634 | cycles |
| fork_wait_p50 | 94 | us |
| fork_wait_p99 | 123 | us |
| fork_exec_wait_p50 | 153 | us |
| fork_exec_wait_p99 | 188 | us |
| signal_handled_p50 | 4027 | cycles |
| signal_handled_p99 | 6054 | cycles |
| fstat_disk_p50 | 14135 | cycles |
| fstat_disk_p99 | 21299 | cycles |
| fstat_tmpfs_p50 | 1680 | cycles |
| fstat_tmpfs_p99 | 1705 | cycles |
| stat_path_tmpfs_p50 | 2997 | cycles |
| stat_path_tmpfs_p99 | 3119 | cycles |
| proc_meminfo_pread_p50 | 30586 | cycles |
| proc_meminfo_pread_p99 | 40955 | cycles |
| proc_self_stat_pread_p50 | 5870 | cycles |
| proc_self_stat_pread_p99 | 7914 | cycles |
| proc_self_stat_open_read_close_p50 | 14221 | cycles |
| proc_self_stat_open_read_close_p99 | 18682 | cycles |
| seq_write | 133.8 | MB/s |
| seq_read_disk | 855.9 | MB/s |
| seq_read_cached | 8367.1 | MB/s |
| read_4k_cached_p50 | 1519 | ns |
| read_4k_cached_p99 | 2348 | ns |
| pread_4k_cached_p50 | 2673 | cycles |
| pread_4k_cached_p99 | 3529 | cycles |
| clock_gettime_p50 | 79 | cycles |
| clock_gettime_p99 | 82 | cycles |
| gettimeofday_p50 | 85 | cycles |
| gettimeofday_p99 | 88 | cycles |
| read_4k_disk_p50 | 26172 | ns |
| read_4k_disk_p99 | 78250 | ns |
| create_disk_p50 | 2510553 | ns |
| create_disk_p99 | 7829889 | ns |
| rename_disk_p50 | 2650119 | ns |
| rename_disk_p99 | 3405295 | ns |
| unlink_disk_p50 | 4890633 | ns |
| unlink_disk_p99 | 6143729 | ns |
| write_fsync_4k_p50 | 4748086 | ns |
| write_fsync_4k_p99 | 6011903 | ns |
| tcp_loopback | 1219.1 | MB/s |
| tcp_network_echo | 187.0 | MB/s |

## Per operation (whole system: the program, the kernel and the servers)

| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |
|---|---:|---:|---:|---:|---:|---:|
| null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| forwarded_null_syscall | 2.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| fork_wait | 45.20 | 0.00 | 0.00 | 29.39 | 4.04 | 20.10 |
| fork_exec_wait | 87.84 | 0.00 | 0.00 | 48.35 | 8.04 | 31.16 |
| signal_handled | 6.00 | 0.00 | 0.00 | 4.00 | 0.00 | 0.00 |
| fstat_disk | 8.00 | 0.00 | 0.00 | 3.99 | 0.00 | 1.00 |
| fstat_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| stat_path_tmpfs | 3.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_meminfo_pread | 11.00 | 0.00 | 0.00 | 4.00 | 1264.00 | 3.00 |
| proc_self_stat_pread | 4.00 | 0.00 | 0.00 | 2.00 | 0.00 | 0.00 |
| proc_self_stat_open_read_close | 14.00 | 0.00 | 0.00 | 6.00 | 0.00 | 0.00 |
| seq_write_64k | 294.18 | 0.00 | 0.00 | 4.04 | 131201.03 | 6.35 |
| seq_read_disk_64k | 10.02 | 0.00 | 0.00 | 4.00 | 0.53 | 2.00 |
| seq_read_cached_64k | 3.01 | 0.00 | 0.00 | 2.00 | 65536.03 | 1.00 |
| read_4k_cached | 3.00 | 0.00 | 0.00 | 2.00 | 4096.00 | 1.00 |
| read_4k_disk | 10.00 | 0.00 | 0.00 | 4.00 | 0.00 | 2.00 |
| create_disk | 5690.76 | 0.00 | 0.00 | 11.98 | 4.02 | 3.99 |
| rename_disk | 5734.11 | 0.00 | 0.00 | 8.00 | 0.03 | 3.00 |
| unlink_disk | 10790.28 | 0.00 | 0.00 | 8.00 | 0.03 | 3.00 |
| write_fsync_4k | 10884.87 | 0.00 | 0.00 | 12.15 | 4120.04 | 10.70 |
| tcp_loopback_64k | 21.07 | 0.00 | 0.00 | 13.82 | 0.02 | 4.97 |
| tcp_network_echo_64k | 80.98 | 0.00 | 0.00 | 22.88 | 0.81 | 7.64 |
