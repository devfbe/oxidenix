#!/bin/sh
# Guest script (OXIDENIX_AUTORUN) for a boot with much more memory than the
# self-tests' 256 MiB (CI: `-m 12G`): the kernel's boot-time tables sized by
# the memory map (the frame table) must not depend on the initial heap, as on
# a real machine. Passes when the boot reaches this shell and sees the memory.
set -e
total=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo)
echo "MemTotal: $total kB"
# 11 GiB: what is left of 12 GiB after the firmware's and the kernel's share.
[ "$total" -ge $((11 * 1024 * 1024)) ]
