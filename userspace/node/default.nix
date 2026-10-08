# Node.js as one static musl binary (no npm), for the software disk.
# - Built with the dependencies bundled in Node's source (V8, OpenSSL,
#   ICU, libuv, ...) rather than nixpkgs' libraries: the static ones do
#   not link together (library order, versions) and Node tests its own.
#   They stay build inputs, so ada's fuzzer test, which fails in the static
#   musl build, is skipped.
# - Node's configure rejects the --enable-static/--disable-shared that
#   pkgsStatic adds; it links fully statically with --fully-static.
let
  pkgs = import <nixpkgs> {
    overlays = [ (final: prev: { ada = prev.ada.overrideAttrs (_: { doCheck = false; }); }) ];
  };
  keep = f: !(builtins.elem f [ "--enable-static" "--disable-shared" ])
    && builtins.substring 0 9 f != "--shared-"
    && builtins.substring 0 11 f != "--with-intl";
in
pkgs.pkgsStatic.nodejs-slim.overrideAttrs (old: {
  dontAddStaticConfigureFlags = true;
  configureFlags = builtins.filter keep (old.configureFlags or [ ])
    ++ [ "--fully-static" "--with-intl=full-icu" ];
})
