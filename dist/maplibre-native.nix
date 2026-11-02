{ pkgs }:
let
  lib = pkgs.lib;
  platform = pkgs.stdenv.hostPlatform;
  release =
    (builtins.fromTOML (builtins.readFile ../vendor/maplibre_native/Cargo.toml))
    .package.metadata.mln.release;
  revision = lib.removePrefix "core-" release;
  source = pkgs.fetchgit {
    url = "https://github.com/maplibre/maplibre-native.git";
    rev = revision;
    hash = "sha256-pxqKzTRjXTKUoRmLynlIojKYx5dNwlTRjcHGuZplwXo=";
    fetchSubmodules = true;
  };
  webgpuHeader = pkgs.fetchurl {
    url = "https://raw.githubusercontent.com/webgpu-native/webgpu-headers/673658bc2bd70ec39fc55ebe6bb0173cf6d0a603/webgpu.h";
    hash = "sha256-pIMDHD/tBepd0cdAgqcWdsRsWyuCDMyhDaUVwDPvyZc=";
  };

  libuvPkgConfig = pkgs.runCommand "maplibre-libuv-pkgconfig" { } ''
    mkdir -p "$out/lib/pkgconfig"
    sed -e 's|^libdir=.*|libdir=${lib.getLib pkgs.pkg-mod-libuv}/lib|' \
        -e 's|^includedir=.*|includedir=${lib.getDev pkgs.pkg-mod-libuv}/include|' \
        ${lib.getDev pkgs.pkg-mod-libuv}/lib/pkgconfig/libuv-static.pc \
        > "$out/lib/pkgconfig/libuv.pc"
  '';
  # Preserve portable/cross compilers; native builds match the shell's Clang.
  stdenv =
    if pkgs.stdenv.buildPlatform == platform && !platform.isDarwin then
      pkgs.overrideCC pkgs.stdenv pkgs.llvmPackages.clang
    else
      pkgs.stdenv;
  includeRoots = [
    "include"
    "src"
    "platform/default/include"
    "vendor/icu/include"
    "vendor/maplibre-native-base/include"
    "vendor/maplibre-native-base/deps/variant/include"
    "vendor/maplibre-native-base/deps/geometry.hpp/include"
    "vendor/maplibre-native-base/deps/geojson.hpp/include"
    "vendor/metal-cpp"
    "vendor/expected-lite/include"
    "vendor/webgpu-cpp"
  ];
  archiveTools = pkgs.buildPackages.llvmPackages.llvm;
  archives = [
    "mbgl-core"
    "mbgl-harfbuzz"
    "mbgl-freetype"
    "mbgl-vendor-parsedate"
    "mbgl-vendor-csscolorparser"
    "mbgl-vendor-icu"
    "mlt-cpp"
    "fastpfor-lib"
  ]
  ++ lib.optionals (!platform.isDarwin) [ "mbgl-vendor-nunicode" ];
in
assert lib.assertMsg (
  platform.isLinux || platform.isDarwin || platform.isMinGW
) "MapLibre Native builds support Linux, macOS, and MinGW";
stdenv.mkDerivation {
  pname = "maplibre-native-map-wgpu";
  version = builtins.substring 0 7 revision;
  src = source;
  patches = [
    ../vendor/maplibre_native/bundled-icu.patch
    ../vendor/maplibre_native/bundled-icu-condition.patch
    ../vendor/maplibre_native/webgpu-custom-layer.patch
    ../vendor/maplibre_native/debug-tile-boundaries.patch
  ]
  ++ lib.optionals platform.isDarwin [ ../vendor/maplibre_native/darwin-no-swift.patch ]
  ++ lib.optionals platform.isMinGW [ ../vendor/maplibre_native/windows-mingw.patch ];
  nativeBuildInputs = [
    pkgs.cmake
    pkgs.ninja
    pkgs.pkg-config
    pkgs.python3
  ];
  buildInputs = [
    pkgs.pkg-mod-curl
    pkgs.pkg-mod-libpng
    pkgs.pkg-mod-libjpeg
    pkgs.pkg-mod-libwebp
    pkgs.pkg-mod-zlib-compat
    pkgs.pkg-mod-sqlite
  ]
  ++ lib.optionals (platform.isLinux || platform.isMinGW) [
    pkgs.pkg-mod-libuv
    libuvPkgConfig
  ]
  ++ lib.optionals platform.isLinux [ pkgs.libglvnd ]
  ++ lib.optionals platform.isDarwin [ pkgs.apple-sdk_15 ];
  # Match the existing source build: generate the native wrapper from its
  # vendored header. Do not set MLN_WEBGPU_IMPL_FFI_BUNDLED_WEBGPU_HEADER (even
  # OFF selects the external header); native wgpu.h has an older extension API.
  # The Rust bridge uses the separately pinned webgpu-shim header installed below.
  cmakeFlags = [
    "-DCMAKE_BUILD_TYPE=Release"
    "-DBUILD_TESTING=OFF"
    "-DMLN_WITH_GLFW=OFF"
    "-DMLN_WITH_WEBGPU=ON"
    "-DMLN_WEBGPU_IMPL_FFI=ON"
    "-DMLN_WEBGPU_IMPL_WGPU=ON"

    "-DMLN_USE_BUILTIN_ICU=ON"
    # Upstream's merger requires a separate native WGPU and does not support
    # MinGW. Merge internal archives below instead; WGPU stays Rust-owned and
    # SQLite stays external so MapLibre and GDAL share one implementation.
    "-DMLN_CREATE_AMALGAMATION=OFF"
    "-DMLN_WITH_SANITIZER=OFF"
    "-DPYTHON_EXECUTABLE=${pkgs.buildPackages.python3}/bin/python3"
    "-DCURL_NO_CURL_CMAKE=ON"
    "-DPKG_CONFIG_ARGN=--static"
    "-DFETCHCONTENT_FULLY_DISCONNECTED=ON"
  ]
  ++ lib.optionals platform.isLinux [
    "-DMLN_WITH_EGL=ON"
    "-DMLN_WITH_X11=OFF"
    "-DMLN_WITH_WAYLAND=OFF"
  ];
  ninjaFlags = [ "mbgl-core" ];
  enableParallelBuilding = true;
  dontUseNinjaInstall = true;
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/lib" "$out/include" "$out/source" "$out/share/maplibre-native"
    printf 'CREATE %s/lib/libmbgl-core-amalgam.a\n' "$out" > amalgam.mri
    # installPhase leaves the build directory before installCheckPhase runs.
    : > "$TMPDIR/amalgam-input-members.txt"
    ${lib.concatMapStringsSep "\n" (archive: ''
      archive=$(find . -name 'lib${archive}.a' -type f -print -quit)
      test -n "$archive"
      printf 'ADDLIB %s\n' "$archive" >> amalgam.mri
      ${archiveTools}/bin/llvm-ar t "$archive" >> "$TMPDIR/amalgam-input-members.txt"
    '') archives}
    printf 'SAVE\nEND\n' >> amalgam.mri
    ${archiveTools}/bin/llvm-ar -M < amalgam.mri
    cp -R webgpu-cpp/. "$out/include/"
    cp ${webgpuHeader} "$out/include/webgpu.h"
    # Upstream's generated shim otherwise refers to the temporary build tree.
    substituteInPlace "$out/include/webgpu/webgpu_cpp.h" \
      --replace-fail "$PWD/webgpu-cpp/webgpu.hpp" "$out/include/webgpu.hpp"
    cd ..
    ${lib.concatMapStringsSep "\n" (root: ''
      mkdir -p "$out/source/${builtins.dirOf root}"
      cp -R ${root} "$out/source/${root}"
    '') includeRoots}
    cat > "$out/share/maplibre-native/build.toml" <<'EOF'
    backend = "wgpu"
    library = "mbgl-core-amalgam"
    target = "${platform.rust.rustcTarget}"
    release = "${release}"
    ndebug = true
    EOF
    runHook postInstall
  '';
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    test -s "$out/lib/libmbgl-core-amalgam.a"
    ${archiveTools}/bin/llvm-ar t "$out/lib/libmbgl-core-amalgam.a" > amalgam-members.txt
    test -s amalgam-members.txt
    sort "$TMPDIR/amalgam-input-members.txt" > amalgam-expected.txt
    sort amalgam-members.txt > amalgam-actual.txt
    cmp amalgam-expected.txt amalgam-actual.txt
    test -s "$out/include/webgpu.hpp"
    test -s "$out/include/webgpu.h"
    test -s "$out/source/include/mln/map/map.hpp"
    if grep -R -F "$TMPDIR" "$out/include"; then
      echo "Installed WebGPU headers reference the temporary build directory" >&2
      exit 1
    fi
    runHook postInstallCheck
  '';
  passthru = { inherit release revision webgpuHeader; };
  meta = {
    description = "MapLibre Native wgpu internal amalgam with Map's custom-layer patches";
    license = lib.licenses.bsd2;
    platforms = lib.platforms.linux ++ lib.platforms.darwin ++ lib.platforms.windows;
  };
}
