#!/usr/bin/env bash
# Runs the I/O benchmarks (userspace/iobench.c) in oxidenix under QEMU/KVM with
# the fixed configuration of the builder (q35, 4 CPUs, 256 MiB, virtio-blk,
# virtio-net, user networking) on a fresh data disk, and writes the results
# with the commit hash to docs/benchmarks/<date>-<commit>.md.
#
# Usage: scripts/bench.sh [label]   (run from anywhere; the label goes into
#                                    the file name, e.g. "baseline")
#        scripts/bench.sh --linux   (the same benchmarks in a Linux guest
#                                    with the same QEMU configuration:
#                                    scripts/linux-guest.nix)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The pinned nixpkgs (nix/nixpkgs.nix), for the Linux guest and the tools.
export NIX_PATH="nixpkgs=$ROOT/nix/nixpkgs.nix"
LINUX=""
if [ "${1:-}" = --linux ]; then
    LINUX=1
    shift
fi
LABEL="${1:-}"
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
DIRTY=""
git -C "$ROOT" diff --quiet HEAD -- . ':!docs/benchmarks' || DIRTY="-dirty"
DATE="$(date +%Y-%m-%d)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [ -n "$LINUX" ]; then
    # The data disk as the builder makes it, and QEMU as the builder runs it
    # (keep in sync with builder/src/main.rs), booting Linux instead.
    nix-shell -p e2fsprogs --run "mke2fs -q -t ext2 -b 1024 -I 128 -O none,filetype,sparse_super,large_file -F '$WORK/disk.img' 65536"
    GUEST="$(nix-build "$ROOT/scripts/linux-guest.nix" --no-out-link -A initrd)"
    KERNEL="$(nix-instantiate --eval --raw "$ROOT/scripts/linux-guest.nix" -A kernel)"
    nix-build "$ROOT/scripts/linux-guest.nix" -A kernel --no-out-link > /dev/null 2>&1 || true
    KVERSION="$(basename "$(dirname "$KERNEL")" | sed 's/^[a-z0-9]*-linux-//')"
    echo "Benchmarking Linux $KVERSION..."
    set +e
    timeout 1800 qemu-system-x86_64 -machine q35 -cpu max -m 256M -smp 4 -accel kvm -accel tcg \
        -kernel "$KERNEL" -initrd "$GUEST/initrd" -append "console=ttyS0 quiet panic=-1" \
        -drive "format=raw,file=$WORK/disk.img,if=none,id=data" -device virtio-blk-pci,drive=data,disable-modern=on \
        -netdev user,id=net0,guestfwd=tcp:10.0.2.100:7-cmd:cat -device virtio-net-pci,netdev=net0,disable-modern=on \
        -serial stdio -no-reboot -display gtk,zoom-to-fit=on < /dev/null > "$WORK/serial.log" 2>&1
    code=$?
    set -e
    ok=0
else
    echo "Benchmarking $COMMIT$DIRTY (fresh disk in $WORK)..."
    set +e
    (cd "$ROOT/kernel" && OXIDENIX_BENCH=1 OXIDENIX_DISK="$WORK/disk.img" timeout 1800 "${CARGO:-cargo}" run < /dev/null > "$WORK/serial.log" 2>&1)
    code=$?
    set -e
    # The shell's exit status 0 becomes QEMU's 1.
    ok=1
fi
if [ "$code" -ne "$ok" ] || ! grep -aq "^iobench: done" "$WORK/serial.log"; then
    echo "The benchmark run failed (exit $code):"
    tail -30 "$WORK/serial.log"
    exit 1
fi

if [ -n "$LINUX" ]; then
    NAME="linux-$KVERSION"
    TITLE="Linux $KVERSION (comparison, oxidenix at $COMMIT)"
else
    NAME="$COMMIT$DIRTY"
    TITLE="$COMMIT$DIRTY"
fi
OUT="$ROOT/docs/benchmarks/$DATE-$NAME${LABEL:+-$LABEL}.md"
results="$(sed -n '/^=== iobench/,/^iobench: done/p' "$WORK/serial.log" | tr -d '\r' | grep -av '^===\|^iobench:')"
{
    echo "# Benchmark $TITLE${LABEL:+ ($LABEL)}"
    echo
    echo "- Date: $DATE"
    echo "- Commit: $COMMIT$DIRTY ($(git -C "$ROOT" log -1 --format=%s HEAD))"
    echo "- Host: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //'), $(nproc) threads, Linux $(uname -r)"
    echo "- QEMU: $(qemu-system-x86_64 --version | head -1), KVM: $([ -w /dev/kvm ] && echo yes || echo no)"
    echo "- Guest: q35, 4 CPUs, 256 MiB, virtio-blk data disk (fresh 64 MiB ext2, 1 KiB blocks), virtio-net with user networking"
    echo
    echo "## Results"
    echo
    echo "| benchmark | value | unit |"
    echo "|---|---:|---|"
    echo "$results" | grep -av '^counters' | awk '{ $1=$1; printf "| %s | %s | %s |\n", $1, $2, substr($0, index($0, $3)) }'
    echo
    echo "## Per operation (whole system: the program, the kernel and the servers)"
    echo
    echo "| operation | syscalls | IPC calls | IPC bytes | address space switches | user copy bytes | kernel heap allocations |"
    echo "|---|---:|---:|---:|---:|---:|---:|"
    echo "$results" | grep -a '^counters' | awk '{ printf "| %s", $2; for (i = 3; i <= NF; i++) { split($i, kv, "="); printf " | %s", kv[2] } print " |" }'
} > "$OUT"
echo "Wrote $OUT"
