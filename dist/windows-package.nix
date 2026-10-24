# Release-only MinGW inputs, isolated from the native development shell.
{
  inputs,
  pkgs,
  lib,
}:
let
  targetPkgs = (inputs.pkgs-mod.lib.mkBuildEnvMingwCross pkgs.system { }).pkgsMingw;
  compiler = targetPkgs.stdenv.cc;
  prefix = compiler.targetPrefix;
  libraries = lib.closePropagation (
    with targetPkgs;
    [
      pkg-mod-gdal
      pkg-mod-proj
      pkg-mod-curl
      pkg-mod-libpng
      pkg-mod-libjpeg
      pkg-mod-libwebp
      pkg-mod-sqlite
      pkg-mod-openssl
      pkg-mod-zlib-compat
      pkg-mod-libuv
      windows.mcfgthreads
      windows.pthreads
    ]
  );
  libraryOutput = package: lib.getLib (package // { outputSpecified = false; });
  # The upstream static libuv .pc duplicates its absolute prefix. Also expose
  # the conventional name expected by MapLibre's Windows CMake configuration.
  packagePkgConfig = pkgs.runCommand "map-windows-pkgconfig" { } ''
    mkdir -p "$out/lib/pkgconfig"
    sed -e 's|^libdir=.*|libdir=${libraryOutput targetPkgs.pkg-mod-libuv}/lib|' \
        -e 's|^includedir=.*|includedir=${lib.getDev targetPkgs.pkg-mod-libuv}/include|' \
        ${lib.getDev targetPkgs.pkg-mod-libuv}/lib/pkgconfig/libuv-static.pc \
        > "$out/lib/pkgconfig/libuv.pc"
    cp ${lib.getDev targetPkgs.pkg-mod-gdal}/lib/pkgconfig/gdal.pc "$out/lib/pkgconfig/gdal.pc"
    chmod u+w "$out/lib/pkgconfig/gdal.pc"
    printf '\nRequires.private: libcurl\n' >> "$out/lib/pkgconfig/gdal.pc"

  '';
  rustPkgs = pkgs.extend inputs.rust-overlay.overlays.default;
  rust = rustPkgs.rust-bin.stable."1.95.0".minimal.override {
    targets = [ "x86_64-pc-windows-gnu" ];
  };
  # Cargo's paths override changes source only, preserving the locked dependency
  # graph and leaving ordinary development builds on the upstream crate.
  patchWindowsCrate =
    name: patch:
    let
      package =
        lib.findFirst (package: package.name == name) (throw "${name} is missing from Cargo.lock")
          (builtins.fromTOML (builtins.readFile ../Cargo.lock)).package;
    in
    pkgs.applyPatches {
      name = "map-${name}-${package.version}";
      src = pkgs.fetchurl {
        name = "${name}-${package.version}.tar.gz";
        url = "https://static.crates.io/crates/${name}/${name}-${package.version}.crate";
        sha256 = package.checksum;
      };
      patches = [ patch ];
    };
  gpuiWindows = patchWindowsCrate "gpui-fast-windows" ./gpui-windows-shaders.patch;
  gpuiPreWindows = patchWindowsCrate "gpui-pre-windows" ./gpui-pre-windows-shaders.patch;
in
assert lib.assertMsg pkgs.stdenv.isLinux "Windows cross-compilation requires Linux";
pkgs.writeText "map-windows-package.json" (
  builtins.toJSON {
    target = "x86_64-pc-windows-gnu";
    cc = "${compiler}/bin/${prefix}gcc";
    cxx = "${compiler}/bin/${prefix}g++";
    ar = "${compiler.bintools}/bin/${prefix}ar";
    rc = "${compiler}/bin/${prefix}windres";
    objdump = "${compiler.bintools}/bin/${prefix}objdump";
    magick = "${pkgs.imagemagick}/bin/magick";
    toolPath = lib.makeBinPath [
      rust
      compiler
      compiler.bintools
    ];
    pkgConfigPath = lib.makeSearchPathOutput "dev" "lib/pkgconfig" ([ packagePkgConfig ] ++ libraries);
    libraryPath = lib.makeLibraryPath (map libraryOutput libraries);
    cmakePrefixPath = lib.concatStringsSep ":" (
      map (package: "${lib.getDev package}") libraries
      ++ map (package: "${libraryOutput package}") libraries
    );
    cflags = "-DCURL_STATICLIB -isystem${lib.getDev targetPkgs.windows.mcfgthreads}/include";
    bindgenArgs = "--target=x86_64-w64-windows-gnu -isystem ${lib.getDev compiler.libc}/include";
    gdal = "${targetPkgs.pkg-mod-gdal}";
    gpuiWindows = "${gpuiWindows}";
    gpuiPreWindows = "${gpuiPreWindows}";
  }
)
