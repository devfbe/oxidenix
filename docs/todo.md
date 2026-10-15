# Open items

What is known to be missing or unfinished, collected from the reviews and agent reports up to
2026-10-10 (main after the ext3 journal). Items that are being worked on name their branch. When an item is
done, delete it here in the same commit.

## Performance

- **`fstat_disk` about 6 % slower than r6e** (A/B, three boots each, 2026-10-14:
  docs/benchmarks/README.md); cached reads are not (the R8 figures were noise between boots).
- **Forwarding cost.** A forwarded null system call costs ~1 330 cycles (sysretq return path
  since 2026-10-15; 1 460 before) against 269 on Linux, `fstat` on `/data` ~13 000 against
  409. The floor of the design is two kernel round trips and two CR3 loads; what lies above
  it (the entry and return paths' checks, register copies, the view switch's PCID lookup)
  needs a profile per step before it is cut. The clock calls no longer pay it (the vDSO,
  ADR 0014: `clock_gettime` 1 540 → 80 cycles); a faster path for other calls the Linux
  server answers without the kernel is the largest open lever.
- **TCP loopback** is at about 20 % of Linux (smoltcp's per-segment work and netd's two copies).

## Robustness

- **No per-tree memory limits.** The OOM killer chooses across all trees (ADR 0013); a tree
  with a limit of its own (as Linux's memory cgroups) would need every frame charged to its
  tree, with an OOM within the tree when it exceeds its limit.
- **/data across process trees: what stays per tree** (ADR 0015 made the page cache one per
  file for every tree): a FIFO's pipe and times, inotify events of other trees' changes, and
  `sendfile`/`copy_file_range` into an `O_APPEND` file are atomic within their tree only.
- `RLIMIT_NPROC` is not enforced (Linux exempts root; the kernel's process table is the bound).

## Linux features not implemented

- Processes and signals: core files, `ptrace`.
  `/proc/<pid>/maps` names the program file, the vDSO, the stack and the heap, but not other
  mapped files (it would need the server to know which file each mapped object is); no
  `smaps`.
  `TFD_TIMER_CANCEL_ON_SET` sees only the tree's own settings of the wall clock.
- Files: owners and groups are accepted but not stored (everyone is root), `io_uring`
  (`ENOSYS`).
- Credentials: everyone in a tree is root with every capability, so the checks Linux makes
  between processes of different users always pass and are not written yet: `kill`'s and
  `pidfd_send_signal`'s permission check, ptrace access mode (`PTRACE_MODE_READ` for
  `/proc/<pid>/maps`, `environ`, `mem`, the fd links; `PTRACE_MODE_ATTACH` for
  `pidfd_getfd`), the dumpable flag's effect on them. Users of their own need them all at
  once (a tree's processes see no other tree's).
- Terminal: packet mode (`TIOCPKT`), the Linux console's VT and keyboard ioctls, serial input.
- Network: IPv6, `MSG_OOB`, `SO_LINGER` with a non-zero time does not block `close`,
  `SO_REUSEPORT` is stored but unused, netlink multicast is never delivered, raw ICMP is shared
  between instances (documented in ADR 0008).
- Dynamic linking with glibc is untested (the ELF loader handles `PT_INTERP`).

## Real hardware

The kernel has only run in QEMU. Booting it on a real machine (for example from a USB stick)
needs at least: a UEFI boot medium without Secure Boot, delays and timer calibration that do
not depend on the PIT, x2APIC support if the firmware hands over in x2APIC mode (both done,
ADR 0016), and drivers for real storage and network (NVMe or USB mass storage via xHCI, a
wired NIC) instead of virtio. Without the drivers it boots to the shell on the framebuffer
with the PS/2 keyboard and runs from the initramfs, without `/data` and without network.

Planned target: the development machine, a Framework Laptop (12th Gen Intel, i5-1240P, UEFI,
PS/2 keyboard behind the embedded controller, Samsung NVMe, Intel Wi-Fi 7, xHCI USB). Steps:

1. **Live USB stick to the shell**. The kernel's part is done (ADR 0016): the TSC's frequency
   from CPUID leaf 0x15 (on the i5-1240P 38.4 MHz × 110 / 2 = 2112 MHz, checked against the
   host's leaves), else measured against the HPET or the ACPI PM timer, the PIT only as the last
   resort and never waited on without a bound; every delay and the APIC timer on the TSC
   (TSC-deadline mode on the target); x2APIC mode (also when handed over by the firmware) with
   the MADT's x2APIC entries; the boot log names the time source and the APIC mode; no serial
   port is detected, and boot and panic messages reach the framebuffer (a panic takes over the
   console's lock). All of it is tested in QEMU (KVM and emulation, xAPIC and x2APIC, x2APIC
   from OVMF, HPET, PM timer, PIT, kvmclock). What is left needs the machine:
   - Secure Boot off in the firmware setup (the image is unsigned).
   - The UEFI image (`cd kernel && OXIDENIX_BUILD_ONLY=1 cargo run`, then
     `target/x86_64-unknown-none/debug/oxidenix.img`) written to a stick
     (`dd if=... of=/dev/sdX bs=4M conv=fsync`), booted from the firmware's boot menu (F12 on
     the Framework).
   - Check on the screen: `[apic]` and `[time] clock source` lines, `[smp] 16 of 16 CPUs`
     (12 cores, 16 threads; `smp::MAX_CPUS` is 16), the keyboard at the shell. A hang before
     the first line or a panic photographed is the report to bring back. Untested in QEMU and
     to watch there: the I/O APIC (one with 120 pins; the kernel programs the first 24), the
     keyboard behind the embedded controller (PS/2 emulation, IRQ 1), the framebuffer's size
     (2256×1504) and stride.
2. **Node.js on the stick**: put the static `node` into the initramfs (about 100 MB), since
   there is no `/data` without a disk driver.
3. **`/data` on the stick** (large): an xHCI driver plus USB mass storage. An NVMe driver would
   be simpler but would touch the internal SSD; only with a dedicated partition, if at all.
4. **Network**: a USB Ethernet adapter after the xHCI driver; Wi-Fi is out of reach.
