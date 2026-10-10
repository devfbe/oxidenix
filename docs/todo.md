# Open items

What is known to be missing or unfinished, collected from the reviews and agent reports up to
2026-10-10 (main 5ecdff9). Items that are being worked on name their branch. When an item is
done, delete it here in the same commit.

## In progress

- **ext3-compatible journal for `/data`** (branch `ext3-journal`, ADR to follow). JBD2 format,
  one transaction per operation, ordered data, recovery at mount. Replaces the ordered-commit
  steps of `crates/ext2fs` (a `mv` costs 4 flushes today, and an interrupted rename can leave
  link counts too high that only `e2fsck` repairs, which makes such directories impossible to
  `rmdir`).

## Performance

- **Cached 4 KiB reads got slower with R8** (`read_4k_cached` about +30–46 %, `fstat_disk`
  +14 % against r6e). Not measured A/B on a quiet host yet; candidates are in
  `kernel/src/process/linux.rs` `enter()` (per-entry `dying()` and `kicked` checks).
- **Forwarding cost.** A null system call costs ~1 450–1 700 cycles against 269 on Linux,
  `fstat` on `/data` ~14 000 against 409. A faster path for calls the Linux server answers
  without the kernel is the largest open lever.
- **TCP loopback** is at about 20 % of Linux (smoltcp's per-segment work and netd's two copies).

## Robustness

- **A failed allocation in the Linux server breaks its whole tree** (panic → `ud2` →
  `break_instance`). Paths a program can drive (socket queues, pipes, directory snapshots,
  epoll, inotify) should use fallible allocation and answer `ENOMEM`.
- **No OOM killer that picks a victim by size.** Strict commit accounting stays the default;
  thread stacks (musl commits 8 MiB per thread on `mprotect`) leave Node little headroom. A
  heuristic overcommit mode needs the OOM killer first.
- **No test with two process trees at once.** Isolation between instances (netd ports and
  budgets, procfs, the heap reserve pool) is covered by host tests only.
- **No QEMU test with a read-only data disk** (covered by an `ext2fs` host test).
- `RLIMIT_NPROC` is not enforced (Linux exempts root; the kernel's process table is the bound).

## Linux features not implemented

- Processes and signals: pidfds (`CLONE_PIDFD`, `P_PIDFD`), POSIX timers, `ITIMER_VIRTUAL` and
  `ITIMER_PROF`, core files, `ptrace`, a vDSO.
- Futexes: `FUTEX_WAKE_OP`, the priority-inheritance operations (`ENOSYS`).
- Files: `mknod`/`mknodat`, owners and groups are accepted but not stored (everyone is root),
  `io_uring` (`ENOSYS`), `/proc/<pid>/exe` is a path link, not a magic link; `statm`'s data
  field and `VmData`/`VmStk`/`VmExe`/`VmLib` are missing.
- Terminal: packet mode (`TIOCPKT`), the Linux console's VT and keyboard ioctls, serial input.
- Network: IPv6, `MSG_OOB`, `SO_LINGER` with a non-zero time does not block `close`,
  `SO_REUSEPORT` is stored but unused, netlink multicast is never delivered, raw ICMP is shared
  between instances (documented in ADR 0008).
- Dynamic linking with glibc is untested (the ELF loader handles `PT_INTERP`).

## Real hardware

The kernel has only run in QEMU. Booting it on a real machine (for example from a USB stick)
needs at least: a UEFI boot medium without Secure Boot, delays and timer calibration that do
not depend on the PIT (often gated on recent Intel chipsets; use the TSC frequency from CPUID
leaf 0x15), x2APIC support if the firmware hands over in x2APIC mode, and drivers for real
storage and network (NVMe or USB mass storage via xHCI, a wired NIC) instead of virtio. Without
them it boots to the shell on the framebuffer with the PS/2 keyboard and runs from the
initramfs, without `/data` and without network.

Planned target: the development machine, a Framework Laptop (12th Gen Intel, i5-1240P, UEFI,
PS/2 keyboard behind the embedded controller, Samsung NVMe, Intel Wi-Fi 7, xHCI USB). Steps:

1. **Live USB stick to the shell** (small, about half a day plus test rounds on the machine):
   TSC-based delays and APIC timer calibration (no PIT), x2APIC mode, Secure Boot off, the UEFI
   image written to a stick, boot via the firmware's boot menu. No serial port there, so boot
   messages must be readable on the framebuffer.
2. **Node.js on the stick**: put the static `node` into the initramfs (about 100 MB), since
   there is no `/data` without a disk driver.
3. **`/data` on the stick** (large): an xHCI driver plus USB mass storage. An NVMe driver would
   be simpler but would touch the internal SSD; only with a dedicated partition, if at all.
4. **Network**: a USB Ethernet adapter after the xHCI driver; Wi-Fi is out of reach.

## Housekeeping

- About 15 finished agent worktrees remain under `.claude/worktrees`.
