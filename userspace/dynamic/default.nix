# The dynamically linked test programs (ADR 0021), unmodified as Nix builds
# them: their interpreter and libraries are named by /nix/store paths.
#
# $out/bin: dyntest (glibc), dynmusl (the same source against musl's dynamic
#   loader), dyncxx (C++ with the shared libstdc++), and gnu-hello, a link to
#   nixpkgs' GNU hello (a real program as nixpkgs ships it).
# $out/lib: libdyntls.so for glibc, musl/libdyntls.so for musl (each program's
#   RUNPATH names its own; dyntest loads it with dlopen).
# $out/closure: the store paths (files or directories) a data disk's
#   /nix/store needs for them: what each loader resolves (`--list`), what the
#   programs open beyond that (libm.so.6 by dlopen, the C.UTF-8 locale), and
#   $out itself. The builder copies exactly these, not the packages'
#   whole closures (glibc alone is 30 MB), onto the data disk.
{ pkgs ? import <nixpkgs> { } }:
let
  glibc = pkgs.stdenv.cc.libc;
  musl = pkgs.pkgsMusl.stdenv.cc.libc;
  # One program and its library, built by `stdenv`'s compiler.
  variant = stdenv: name: libdir: stdenv.mkDerivation {
    name = "oxidenix-${name}";
    dontUnpack = true;
    # The RUNPATH to $out/lib is for dlopen alone: patchelf's shrinking
    # would remove it (no DT_NEEDED names the library).
    dontPatchELF = true;
    buildPhase = ''
      mkdir -p $out/bin $out/${libdir}
      $CC -O2 -fPIC -shared -o $out/${libdir}/libdyntls.so ${./dyntls.c}
      $CC -O2 -pie -fPIE -o $out/bin/${name} ${./dyntest.c} -pthread -Wl,-rpath,$out/${libdir}
    '';
    installPhase = "true";
  };
  dyntest = variant pkgs.stdenv "dyntest" "lib";
  dynmusl = variant pkgs.pkgsMusl.stdenv "dynmusl" "lib/musl";
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
  mkdir -p $out/bin $out/lib/musl
  cp ${dyntest}/bin/dyntest ${dynmusl}/bin/dynmusl ${dyncxx}/bin/dyncxx $out/bin/
  cp ${dyntest}/lib/libdyntls.so $out/lib/
  cp ${dynmusl}/lib/musl/libdyntls.so $out/lib/musl/
  ln -s ${pkgs.hello}/bin/hello $out/bin/gnu-hello
  # (The copies keep their RUNPATHs: the variants' outputs stay referenced.)
  {
    for f in ${dyntest}/bin/dyntest ${dyntest}/lib/libdyntls.so ${dyncxx}/bin/dyncxx ${pkgs.hello}/bin/hello; do
      ${glibc}/lib/ld-linux-x86-64.so.2 --list "$f"
    done
    for f in ${dynmusl}/bin/dynmusl ${dynmusl}/lib/musl/libdyntls.so; do
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
  } | sort -u > $out/closure
''
