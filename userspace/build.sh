#!/usr/bin/env bash
# Baut alle userspace/*.c als statische musl-Binaries nach $1.
set -euo pipefail
OUT="$1"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CC=x86_64-unknown-linux-musl-cc
cmds=""
for f in "$SRC"/*.c; do
    cmds+="$CC -static -O2 -o '$OUT/$(basename "$f" .c)' '$f' && "
done
cmds+="true"
if command -v "$CC" >/dev/null; then
    bash -c "$cmds"
else
    nix-shell -p pkgsStatic.stdenv.cc --run "$cmds"
fi
