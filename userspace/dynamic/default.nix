# The dynamically linked programs (ADR 0021), unmodified as Nix builds them:
# their interpreters and libraries are named by /nix/store paths.
#
# $out/rootfs: what the root filesystem gets (the builder copies it, links as
#   links): in bin/ dyntest (glibc), dynmusl (the same source against musl's
#   dynamic loader), dyncxx (C++ with the shared libstdc++), gnu-hello (a link
#   to nixpkgs' GNU hello); usr/gnu/bin, a link to nixpkgs' coreutils (one
#   binary that is each tool by the name it is run as); and nix, a link to
#   /data/nix, where the data disk holds the store.
# $out/closure: the store paths (files, links or directories) the data disk's
#   /nix/store needs for them: what each loader resolves (`--list`), what the
#   programs open beyond that (libm.so.6 by dlopen, the C.UTF-8 locale), the
#   programs' own outputs (each program's RUNPATH names its library there:
#   dyntest loads it with dlopen) and coreutils' bin. The builder copies
#   exactly these, not the packages' whole closures (glibc alone is 30 MB),
#   onto the data disk.
{ pkgs ? import <nixpkgs> { } }:
let
  glibc = pkgs.stdenv.cc.libc;
  musl = pkgs.pkgsMusl.stdenv.cc.libc;
  # One program and its library, built by `stdenv`'s compiler.
  variant = stdenv: name: stdenv.mkDerivation {
    name = "oxidenix-${name}";
    dontUnpack = true;
    # The RUNPATH to $out/lib is for dlopen alone: patchelf's shrinking
    # would remove it (no DT_NEEDED names the library).
    dontPatchELF = true;
    buildPhase = ''
      mkdir -p $out/bin $out/lib
      $CC -O2 -fPIC -shared -o $out/lib/libdyntls.so ${./dyntls.c}
      $CC -O2 -pie -fPIE -o $out/bin/${name} ${./dyntest.c} -pthread -Wl,-rpath,$out/lib
    '';
    installPhase = "true";
  };
  dyntest = variant pkgs.stdenv "dyntest";
  dynmusl = variant pkgs.pkgsMusl.stdenv "dynmusl";
  dyncxx = pkgs.stdenv.mkDerivation {
    name = "oxidenix-dyncxx";
    dontUnpack = true;
    buildPhase = ''
      mkdir -p $out/bin
      $CXX -O2 -o $out/bin/dyncxx ${./dyncxx.cc} -pthread
    '';
    installPhase = "true";
  };
in
pkgs.runCommand "oxidenix-dynamic" { } ''
  mkdir -p $out/rootfs/bin $out/rootfs/usr/gnu
  cp ${dyntest}/bin/dyntest ${dynmusl}/bin/dynmusl ${dyncxx}/bin/dyncxx $out/rootfs/bin/
  ln -s ${pkgs.hello}/bin/hello $out/rootfs/bin/gnu-hello
  ln -s ${pkgs.coreutils}/bin $out/rootfs/usr/gnu/bin
  ln -s /data/nix $out/rootfs/nix
  {
    for f in ${dyntest}/bin/dyntest ${dyntest}/lib/libdyntls.so ${dyncxx}/bin/dyncxx ${pkgs.hello}/bin/hello ${pkgs.coreutils}/bin/coreutils; do
      ${glibc}/lib/ld-linux-x86-64.so.2 --list "$f"
    done
    for f in ${dynmusl}/bin/dynmusl ${dynmusl}/lib/libdyntls.so; do
      ${musl}/lib/libc.so --list "$f"
    done
  } | grep -o '/nix/store/[^ ]*' > list
  {
    cat list
    echo ${glibc}/lib/libm.so.6
    echo ${glibc}/lib/locale/C.utf8
    echo ${dyntest}
    echo ${dynmusl}
    echo ${dyncxx}
    echo ${pkgs.hello}/bin/hello
    echo ${pkgs.coreutils}/bin
  } | sort -u > $out/closure
''
