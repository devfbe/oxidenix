#!/bin/sh
# Benchmark mode (OXIDENIX_BENCH=1, see docs/benchmarks/README.md): runs the
# I/O benchmarks on the data disk; the exit status ends QEMU as in test mode.
echo "=== iobench"
iobench /data
