# The development shell: every tool a build, a test run or a benchmark needs
# (`nix develop`, or automatically with direnv: `.envrc`). Its packages come
# from the same pinned nixpkgs as the builder and CI (`nix/nixpkgs.nix`), so
# there is one pin; Rust comes from rustup, which follows `rust-toolchain.toml`.
{
  description = "oxidenix development shell";

  outputs = { self }:
    let
      system = "x86_64-linux";
      pkgs = import ./nix/nixpkgs.nix { inherit system; };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          # Rust: the nightly and components named in rust-toolchain.toml
          # (rust-src for build-std, rust-analyzer for editors and agents).
          rustup
          # Booting and testing: QEMU (UEFI firmware below), the host ext2
          # tests and e2fsck/debugfs on the data disk.
          qemu
          e2fsprogs
          # The code map, CI, scripts.
          python3
          gh
          git
          jq
        ];

        # The builder uses this firmware instead of building OVMF itself.
        OXIDENIX_OVMF = "${pkgs.OVMF.fd}/FV";

        shellHook = ''
          # Manual Nix calls (nix-shell -p ...) use the same pinned nixpkgs.
          export NIX_PATH="nixpkgs=$PWD/nix/nixpkgs.nix"
          # rustup's toolchains and proxies (cargo, rust-analyzer).
          export PATH="''${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
          rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install
        '';
      };
    };
}
