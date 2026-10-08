# The pinned nixpkgs every build uses (NIX_PATH=nixpkgs=<this file>): the
# toolchains, musl, Bash, BusyBox, OVMF and e2fsprogs are the same locally
# and in CI. To update: replace the revision and the hash
# (`nix-prefetch-url --unpack <url>`), then run the self-tests.
import (builtins.fetchTarball {
  # nixos-unstable, 2026-10-08 (nixpkgs 26.11pre, musl 1.2.6).
  url = "https://github.com/NixOS/nixpkgs/archive/151fa4e8ddfdd8dd25d945ad94ed54a13de9f6e4.tar.gz";
  sha256 = "0rm5v6n0kgqq5xrc34imn8nxx0y2xlmhpa0nag9sk6m6ys9slaij";
})
