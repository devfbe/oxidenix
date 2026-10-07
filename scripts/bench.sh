#!/usr/bin/env bash
# Runs the I/O benchmarks (userspace/iobench.c) in oxidenix under QEMU/KVM with
# the fixed configuration of the builder (q35, 4 CPUs, 256 MiB, virtio-blk,
# virtio-net, user networking) on a fresh data disk, and writes the results
# with the commit hash to docs/benchmarks/<date>-<commit>.md.
#
# Usage: scripts/bench.sh [label]   (run from anywhere; the label goes into
#                                    the file name, e.g. "baseline")
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LABEL="${1:-}"
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
DIRTY=""
git -C "$ROOT" diff --quiet HEAD -- . ':!docs/benchmarks' || DIRTY="-dirty"
DATE="$(date +%Y-%m-%d)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "Benchmarking $COMMIT$DIRTY (fresh disk in $WORK)..."
set +e
(cd "$ROOT/kernel" && OXIDENIX_BENCH=1 OXIDENIX_DISK="$WORK/disk.img" timeout 1800 "${CARGO:-cargo}" run < /dev/null > "$WORK/serial.log" 2>&1)
code=$?
set -e
# The shell's exit status 0 becomes QEMU's 1.
if [ "$code" -ne 1 ] || ! grep -aq "^iobench: done" "$WORK/serial.log"; then
    echo "The benchmark run failed (exit $code):"
    tail -30 "$WORK/serial.log"
    exit 1
fi

OUT="$ROOT/docs/benchmarks/$DATE-$COMMIT$DIRTY${LABEL:+-$LABEL}.md"
results="$(sed -n '/^=== iobench/,/^iobench: done/p' "$WORK/serial.log" | tr -d '\r' | grep -av '^===\|^iobench:')"
{
    echo "# Benchmark $COMMIT$DIRTY${LABEL:+ ($LABEL)}"
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
