#!/usr/bin/env bash
# Installs the UEFI boot image on this machine's EFI system partition as a
# systemd-boot entry and makes it the next boot only (a one-shot entry: the
# boot after it is the default again), for trying oxidenix on real hardware
# without a USB stick. With Secure Boot keys of sbctl's (/var/lib/sbctl) the
# loader is signed with them.
#
# The bootloader crate's loader reads the kernel and the initramfs unverified
# from the root of its partition (\kernel-x86_64, \ramdisk), so a signed one
# lets whoever can write the ESP run code before the OS despite Secure Boot.
# Remove it after the test: scripts/efi-install.sh --uninstall.
#
# Usage: scripts/efi-install.sh              (build, confirm, install, next boot)
#        scripts/efi-install.sh --uninstall  (remove everything it installed)
# ESP=<path> overrides the partition's mount point (default: bootctl's).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export NIX_PATH="nixpkgs=$ROOT/nix/nixpkgs.nix"
ESP="${ESP:-$(bootctl --print-esp-path)}"
ENTRY=oxidenix.conf
DIR="$ESP/EFI/oxidenix"
# What the install puts on the ESP (the loader's two files must be at its root).
FILES=("$DIR/BOOTX64.EFI" "$ESP/kernel-x86_64" "$ESP/ramdisk" "$ESP/loader/entries/$ENTRY")

confirm() {
    local answer
    read -r -p "$1 [y/N] " answer
    [ "$answer" = y ] || [ "$answer" = Y ]
}

if [ "${1:-}" = --uninstall ]; then
    echo "Removing from $ESP:"
    for f in "${FILES[@]}"; do
        if [ -e "$f" ]; then echo "  $f"; fi
    done
    confirm "Remove these files?" || exit 1
    sudo rm -f "${FILES[@]}"
    sudo rmdir "$DIR" 2>/dev/null || true
    # A one-shot entry that was not booted yet points at nothing now.
    sudo bootctl set-oneshot "" 2>/dev/null || true
    echo "Removed."
    exit 0
fi

# Refuse files at the ESP's root that an earlier install of ours did not leave.
if [ ! -e "$DIR/BOOTX64.EFI" ]; then
    for f in "$ESP/kernel-x86_64" "$ESP/ramdisk"; do
        if [ -e "$f" ]; then
            echo "error: $f exists but was not installed by this script" >&2
            exit 1
        fi
    done
fi

(cd "$ROOT/kernel" && OXIDENIX_BUILD_ONLY=1 cargo run)
IMG="$ROOT/target/x86_64-unknown-none/debug/oxidenix.img"
# The image's EFI system partition (GPT, the first one), as an mtools offset.
OFFSET=$(( $(partx -g -o START -n 1 "$IMG") * 512 ))
MTOOLS="$(nix-build '<nixpkgs>' -A mtools --no-out-link)/bin"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
"$MTOOLS/mcopy" -n -i "$IMG@@$OFFSET" ::/efi/boot/bootx64.efi "$STAGE/BOOTX64.EFI"
"$MTOOLS/mcopy" -n -i "$IMG@@$OFFSET" ::/kernel-x86_64 ::/ramdisk "$STAGE/"
printf 'title oxidenix\nsort-key zz-oxidenix\nefi /EFI/oxidenix/BOOTX64.EFI\n' > "$STAGE/$ENTRY"

SIGN=""
if sudo test -e /var/lib/sbctl/keys/db/db.key; then
    SIGN="$(nix-build '<nixpkgs>' -A sbctl --no-out-link)/bin/sbctl"
fi
echo
echo "Installing on $ESP (the next boot only; the default entry stays):"
for f in "${FILES[@]}"; do
    echo "  $f"
done
if [ -n "$SIGN" ]; then
    echo "The loader is signed with sbctl's db key. It loads the kernel and the"
    echo "initramfs unverified: remove it after the test (just uninstall)."
else
    echo "No sbctl keys: the loader is unsigned (Secure Boot must be off)."
fi
confirm "Install and boot oxidenix next?" || exit 1

sudo install -d "$DIR"
sudo install -m 0644 "$STAGE/BOOTX64.EFI" "$DIR/BOOTX64.EFI"
sudo install -m 0644 "$STAGE/kernel-x86_64" "$STAGE/ramdisk" "$ESP/"
sudo install -m 0644 "$STAGE/$ENTRY" "$ESP/loader/entries/$ENTRY"
if [ -n "$SIGN" ]; then
    # Without -s: sbctl does not re-sign it on its own later.
    sudo "$SIGN" sign "$DIR/BOOTX64.EFI"
fi
sudo bootctl set-oneshot "$ENTRY"
echo "oxidenix is the next boot; reboot when ready (the one after is the default again)."
