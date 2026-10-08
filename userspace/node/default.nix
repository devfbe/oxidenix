# Node.js as one fully static musl binary (no npm), for the data disk
# (`OXIDENIX_NODE=1 cargo run` in kernel/ builds it and installs it as
# /data/bin/node; see README.md). Build by hand with
#   NIX_PATH=nixpkgs=$PWD/nix/nixpkgs.nix nix-build userspace/node
# The first build compiles V8 and runs the tests (about an hour on 12
# cores), later ones come from the Nix store.
#
# An override of the pinned nixpkgs' pkgsStatic.nodejs-slim:
# - Node is built with the dependencies bundled in its source (V8, OpenSSL,
#   ICU, libuv, ...) rather than nixpkgs' libraries: the static ones do not
#   link together (library order, versions), and Node tests its own. Hence
#   the --shared-* flags go. nixpkgs' libraries stay build inputs, so they
#   are built (and tested) all the same.
# - Node's configure rejects the --enable-static/--disable-shared that
#   pkgsStatic adds; it links fully statically with --fully-static.
# - Full ICU (all locales) instead of nixpkgs' system ICU (--with-intl=system-icu).
#
# Checks that are off, and why (everything else runs as in nixpkgs: the
# tooltest, cctest and test-ci-js targets, the JavaScript test suites):
# - ada (a build input, see above): its fuzzer test fails when linked
#   statically against musl; Node uses its own bundled ada anyway.
# - Node's native addon tests (build-js-native-api-tests and
#   build-node-api-tests in nixpkgs' checkTarget): they build addons as
#   shared objects, which the static toolchain cannot link (crtbeginT.o is
#   not position-independent) and a fully static node could not load: a
#   static musl program has no dlopen.
# - Three JavaScript tests that load a native addon and expect the dynamic
#   loader's error message; a static node fails earlier, with "Dynamic
#   loading not supported" (see staticSkips).
# - The tests that build and run single executable applications
#   (sea/test-single-executable-application*): Node's tests consider them
#   unsupported on Alpine Linux, the musl distribution, but they recognize
#   it by /etc/os-release, which the build sandbox lacks, so they fail
#   instead of skipping. (nixpkgs' own build skips them earlier, for its
#   shared OpenSSL.) The other SEA tests (blob generation) run.
let
  pkgs = import <nixpkgs> {
    overlays = [ (final: prev: { ada = prev.ada.overrideAttrs (_: { doCheck = false; }); }) ];
  };
  inherit (pkgs) lib;
  keep = f: !(builtins.elem f [ "--enable-static" "--disable-shared" ])
    && !(lib.hasPrefix "--shared-" f)
    && !(lib.hasPrefix "--with-intl" f);
  addonTests = [ "build-js-native-api-tests" "build-node-api-tests" ];
  # Matched as substrings of the test files' paths (tools/test.py --skip-tests).
  staticSkips = [
    "parallel/test-module-loading-error.js"
    "parallel/test-process-dlopen-error-message-crash.js"
    "sequential/test-module-loading.js"
    "sea/test-single-executable-application"
  ];
  addSkips = f:
    if lib.hasPrefix "CI_SKIP_TESTS=" f then f + "," + lib.concatStringsSep "," staticSkips else f;
in
pkgs.pkgsStatic.nodejs-slim.overrideAttrs (old: {
  dontAddStaticConfigureFlags = true;
  configureFlags = builtins.filter keep (old.configureFlags or [ ])
    ++ [ "--fully-static" "--with-intl=full-icu" ];
  checkTarget = lib.concatStringsSep " "
    (builtins.filter (t: !(builtins.elem t addonTests)) (lib.splitString " " old.checkTarget));
  checkFlags = map addSkips old.checkFlags;
})
