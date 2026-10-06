#!/usr/bin/env bash
# Builds all userspace/*.c as static musl binaries into $1 and adds Bash
# and BusyBox with its applet symlinks.
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

BASH_BIN="$(nix-build '<nixpkgs>' -A pkgsStatic.bash --no-out-link)/bin/bash"
install -m 755 "$BASH_BIN" "$OUT/bash"

BUSYBOX="$(nix-build '<nixpkgs>' -A pkgsStatic.busybox --no-out-link)/bin/busybox"
install -m 755 "$BUSYBOX" "$OUT/busybox"
for applet in $("$OUT/busybox" --list); do
    [ -e "$OUT/$applet" ] || ln -s busybox "$OUT/$applet"
done
