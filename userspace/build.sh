#!/usr/bin/env bash
# Baut alle userspace/*.c als statische musl-Binaries nach $1 und legt
# BusyBox samt Applet-Symlinks dazu.
set -euo pipefail
OUT="$1"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$OUT"
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

BUSYBOX="$(nix-build '<nixpkgs>' -A pkgsStatic.busybox --no-out-link)/bin/busybox"
install -m 755 "$BUSYBOX" "$OUT/busybox"
for applet in $("$OUT/busybox" --list); do
    [ -e "$OUT/$applet" ] || ln -s busybox "$OUT/$applet"
done
