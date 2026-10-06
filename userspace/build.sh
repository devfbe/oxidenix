#!/usr/bin/env bash
# Builds all userspace/*.c as static musl binaries into $1 (the rootfs's
# bin directory), adds Bash and BusyBox with its applet symlinks, and the
# servers/* programs into the sibling sbin directory.
set -euo pipefail
OUT="$1"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$OUT"
CC=x86_64-unknown-linux-musl-cc
cmds=""
for f in "$SRC"/*.c; do
    cmds+="$CC -static -O2 -s -o '$OUT/$(basename "$f" .c)' '$f' && "
done
cmds+="true"
if command -v "$CC" >/dev/null; then
    bash -c "$cmds"
else
    nix-shell -p pkgsStatic.stdenv.cc --run "$cmds"
fi

ROOT="$(dirname "$OUT")"
mkdir -p "$ROOT/sbin"
for server in "$SRC"/../servers/*/; do
    name="$(basename "$server")"
    (cd "$server" && CARGO_TARGET_DIR="$SRC/../target/servers" "${CARGO:-cargo}" build --release -q)
    install -m 755 "$SRC/../target/servers/x86_64-unknown-none/release/$name" "$ROOT/sbin/$name"
done

BASH_BIN="$(nix-build '<nixpkgs>' -A pkgsStatic.bash --no-out-link)/bin/bash"
install -m 755 "$BASH_BIN" "$OUT/bash"

BUSYBOX="$(nix-build '<nixpkgs>' -A pkgsStatic.busybox --no-out-link)/bin/busybox"
install -m 755 "$BUSYBOX" "$OUT/busybox"
for applet in $("$OUT/busybox" --list); do
    [ -e "$OUT/$applet" ] || ln -s busybox "$OUT/$applet"
done

# htop, with the terminfo entry for the console (TERM=linux).
HTOP="$(nix-build '<nixpkgs>' -A pkgsStatic.htop --no-out-link)"
install -m 755 "$HTOP/bin/htop" "$OUT/htop"
# The bootloader reads the initramfs at about 1 MB/s without KVM: no symbols.
OBJCOPY="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy"
[ -x "$OBJCOPY" ] && "$OBJCOPY" --strip-all "$OUT/htop"
TERMINFO_SRC="$(nix-build '<nixpkgs>' -A pkgsStatic.ncurses --no-out-link)/share/terminfo"
install -D -m 644 "$TERMINFO_SRC/l/linux" "$ROOT/usr/share/terminfo/l/linux"
