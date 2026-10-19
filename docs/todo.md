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

- **/data across process trees: what stays per tree** (ADR 0015 made the page cache one per
  file for every tree): a FIFO's pipe and times, inotify events of other trees' changes, and
  `sendfile`/`copy_file_range` into an `O_APPEND` file are atomic within their tree only.
- `RLIMIT_NPROC` is not enforced (Linux exempts root; the kernel's process table is the bound).

## Linux features not implemented

- Core dumps (ADR 0022): a pipe in `core_pattern` (`|program`) runs no helper (nothing is
  written, a line in the kernel's log; Linux runs the program as root in the initial
  namespaces with the dump on its standard input, `core_pipe_limit`, `%F`); `NT_FILE` names
  only the program file's mappings (as `/proc/<pid>/maps`: the server does not know which
  file another mapped object is) and anonymous shared memory is told from a file only by
  the kernel's object (an unlinked tmpfs file counts as a file, Linux's `i_nlink == 0` as
  anonymous); `MADV_DONTDUMP` is accepted but not kept (its areas are dumped);
  `PROT_NONE` pages are holes (no `FOLL_FORCE` read of them, as for ptrace);
  `/proc/sys/fs/suid_dumpable` and the dumpable flag's other effects wait for credentials;
  a SIGKILL that ends a dump under way is not covered by the self-tests (its timing).
- ptrace (ADR 0019): `PTRACE_SINGLEBLOCK` (`EIO`: block steps would need the thread's
  `DEBUGCTL.BTF` switched like its debug registers), `NT_X86_XSTATE` (`ENODEV`, as Linux on
  a CPU without XSAVE: the kernel enables no AVX state; it comes with XSAVE), 32-bit tracees
  and `PTRACE_GET_THREAD_AREA`/`SET_THREAD_AREA`, seccomp and its requests and
  `PTRACE_O_SUSPEND_SECCOMP`, `PTRACE_SET_SYSCALL_USER_DISPATCH_CONFIG`; `FOLL_FORCE`
  access to `PROT_NONE` mappings (Linux reads and writes them; here `EIO`/`EFAULT`); a
  real parent learns of a process's end once its tracer reaped the traced main thread, but
  not after the other traced threads (Linux waits for all); hardware breakpoints are cleared
  at detach (Linux keeps them until exec or exit); a watchpoint on memory the server or the
  kernel touches for the program (a read(2) buffer) leaves no mark in the tracer's DR6
  (Linux records it there, without a signal).
  `/proc/<pid>/maps` names the program file, the vDSO, the stack and the heap, but not other
  mapped files (it would need the server to know which file each mapped object is); no
  `smaps`.
  `TFD_TIMER_CANCEL_ON_SET` sees only the tree's own settings of the wall clock.
- Files: owners and groups are accepted but not stored (everyone is root), `io_uring`
  (`ENOSYS`), extended attributes (no filesystem keeps them: the calls answer as on a Linux
  filesystem without them; Linux's tmpfs keeps `user.`, `trusted.` and `security.` ones),
  `posix_fadvise`'s advice is checked but not acted on (no readahead, no dropping of pages).
- Credentials: everyone in a tree is root with every capability, so the checks Linux makes
  between processes of different users always pass and are not written yet: `kill`'s and
  `pidfd_send_signal`'s permission check, ptrace access mode (`PTRACE_MODE_READ` for
  `/proc/<pid>/maps`, `environ`, the fd links; `PTRACE_MODE_ATTACH` for `ptrace`'s attach,
  `process_vm_readv`/`writev`, `/proc/<pid>/mem` and `pidfd_getfd`; Yama's scope), the
  dumpable flag's effect on them; the console's (Linux's `vt_ioctl`: the KD/VT setters only
  from a process whose controlling terminal it is or with `CAP_SYS_TTY_CONFIG`, the keymap,
  function string, accent and keycode setters only with `CAP_SYS_TTY_CONFIG`; today each
  tree's VT state is its server's own and only the console's holder gets keys). Users of
  their own need them all at once (a tree's processes see no other tree's).
- Terminal (ADR 0020): one virtual terminal only (more need a screen per VT in the kernel's
  console), no console fonts, screen or color maps (`GIO_SCRNMAP`, `PIO_CMAP`), blanking or
  scrollback; serial lines without carrier detection, `TIOCMIWAIT`, `TIOCGICOUNT`, received
  breaks and parity errors, and with polled output.
- Network: IPv6, raw ICMP is shared between instances (documented in ADR 0008).
- Memory groups (ADR 0024): one per tree, no sub-groups (`mkdir` in /sys/fs/cgroup is EPERM)
  and no other controllers; no `memory.stat`, `memory.swap.*`, `memory.oom.group`,
  `memory.reclaim` or pressure files; kernel heap objects are not charged; an allocation of
  a system call that finds the tree at its limit fails with ENOMEM after reclaim within the
  tree (Linux would invoke the group's OOM killer; page faults and copies into program memory
  do); no per-group dirty limits (a tree's writer is throttled by the machine's).
- `rseq` is `ENOSYS` (ADR 0021: glibc goes on without it). A real one needs the kernel to
  update the registered area's CPU number and abort a critical section on every return to
  user mode after a preemption or migration, and the server on signal delivery.
- Dynamically linked programs (ADR 0021) need their store paths on `/data/nix/store`; nothing
  in the guest installs a Nix closure there, and the builder's copies of earlier nixpkgs pins
  stay on the persistent `disk.img` (no garbage collection).

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
   - Booted from the machine's own ESP: `just install` (`scripts/efi-install.sh`: a
     systemd-boot entry for the next boot only, the loader signed with sbctl's keys where
     Secure Boot has them; `just uninstall` afterwards, since the signed loader loads the
     kernel and the initramfs unverified). Or the UEFI image
     (`target/x86_64-unknown-none/debug/oxidenix.img`) written to a stick
     (`dd if=... of=/dev/sdX bs=4M conv=fsync`) with Secure Boot off, booted from the
     firmware's boot menu (F12 on the Framework).
   - The first boot on the Framework (2026-10-19, 32 GiB) ended in a panic right after the
     first line: the frame table (reference counts and memory groups per frame) came from the
     initial 8 MiB heap, which a memory map up to 37 GiB outgrows. It is the allocator's own
     memory now, and CI boots with 12 GiB (`scripts/bigmem.sh`).
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
