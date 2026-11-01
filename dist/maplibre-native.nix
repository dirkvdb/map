{ pkgs }:
let
  lib = pkgs.lib;
  revision = "6718eff7beeb927c6c629b4da617338b0b8fab03";
  platform = pkgs.stdenv.hostPlatform;
  architecture = if platform.isAarch64 then "arm64" else "x64";
  backend = if platform.isDarwin then "metal" else "vulkan";
  preset = "${if platform.isDarwin then "macos" else "linux-gnu"}-${architecture}-${backend}";
  rustTarget = platform.rust.rustcTarget;
  rustToolchain = pkgs.rust-bin.stable."1.99.0".minimal.override {
    extensions = [ "llvm-tools" ];
  };
  rustPlatform = pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };
  hostLinker = pkgs.writeShellScript "maplibre-rust-host-linker" ''
    exec ${pkgs.stdenv.cc}/bin/cc "$@" -fuse-ld=bfd
  '';
  source = pkgs.fetchgit {
    url = "https://github.com/maplibre/maplibre-native-ffi.git";
    rev = revision;
    hash = "sha256-PykjG5ZWnJVThr63PyslFwQ08odvGzMMOYRg8U4pyeU=";
    fetchSubmodules = false;
    # Fetch only upstream's selected engine dependencies, not test/SDK trees.
    # Networking is confined to this fixed-output source fetch.
    postCheckout = ''
      (
        cd "$out"
        bash tools/deps/sync-submodules
        # Retain provenance for the SBOM without shipping Git object databases.
        git rev-parse HEAD > .map-source-revision
        git submodule foreach --quiet --recursive \
          'git rev-parse HEAD > .map-source-revision'
      )
    '';
  };
  extensions = lib.fileset.toSource {
    root = ../crates;
    fileset = lib.fileset.unions [
      ../crates/maplibre-custom-layer/cmake
      ../crates/maplibre-custom-layer/native
      ../crates/maplibre-custom-layer/patches
      ../crates/maplibre-raster-compositor/cmake
      ../crates/maplibre-raster-compositor/native
    ];
  };
  injection = pkgs.writeText "map-extensions.cmake" ''
    include("${extensions}/maplibre-custom-layer/cmake/inject.cmake")
    include("${extensions}/maplibre-raster-compositor/cmake/inject.cmake")
    if(CMAKE_HOST_SYSTEM_NAME STREQUAL "Linux")
      get_filename_component(linker_dir "''${CMAKE_LINKER}" DIRECTORY)
      get_filename_component(tools_dir "''${linker_dir}" DIRECTORY)
      set(real_linker "''${tools_dir}/gcc-ld-unwrapped/ld.lld")
      if(EXISTS "''${real_linker}")
        set(CMAKE_LINKER "''${real_linker}" CACHE FILEPATH "Native archive linker" FORCE)
        set(CMAKE_LINKER "''${real_linker}")
      endif()
    endif()
  '';
  zlibSource = pkgs.fetchzip {
    url = "https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz";
    hash = "sha256-acY8yFzIRYbrZ2CGODoxLnZuppsP6KZy19I9Yy77pfc=";
  };
  libuvSource = pkgs.fetchzip {
    url = "https://dist.libuv.org/dist/v1.48.0/libuv-v1.48.0.tar.gz";
    hash = "sha256-U68BmIQNpmIy3prS7LkYl+wvDJQNikoeFiKh50yQFoA=";
  };
  compositorSymbols = map (name: "map_compositor_${name}") [
    "new"
    "prepare"
    "render"
    "deinitialize"
    "context_lost"
    "delete"
  ];
in
assert lib.assertMsg (
  (platform.isLinux || platform.isDarwin) && (platform.isx86_64 || platform.isAarch64)
) "MapLibre Native supports native x86_64/aarch64 Linux and macOS builds";
pkgs.stdenv.mkDerivation {
  pname = "maplibre-native-ffi-map";
  version = "0.1.0-${builtins.substring 0 7 revision}";
  src = source;
  cargoDeps = rustPlatform.fetchCargoVendor {
    src = source;
    hash = "sha256-eIy2V7X7pjt4VEQqo/VtF7pLYClwZrH9dEEcTuIY23g=";
  };
  nativeBuildInputs = [
    pkgs.cmake
    pkgs.ninja
    pkgs.git
    pkgs.python3
    pkgs.cargo-about
    rustToolchain
    rustPlatform.cargoSetupHook
  ]
  ++ lib.optionals platform.isLinux [ pkgs.zig ];
  buildInputs = lib.optionals platform.isDarwin [ pkgs.apple-sdk_15 ];
  enableParallelBuilding = true;
  dontStrip = true;
  dontUseCmakeConfigure = true;
  dontUseNinjaBuild = true;
  dontUseNinjaInstall = true;
  CARGO_NET_OFFLINE = "true";
  postPatch = ''
    for patch in ${extensions}/maplibre-custom-layer/patches/*.patch; do
      (cd third_party/maplibre-native && git apply "$patch")
    done
    # Nix sources have no .git; retain the real pinned revision in the descriptor.
    substituteInPlace cmake/mln_ffi_artifact_commit.cmake \
      --replace-fail 'set(descriptor_commit "")' 'set(descriptor_commit "${revision}")'
    # Resolve embedded dependencies against the nearest recorded repository,
    # just as upstream resolves them against the nearest .git directory.
    substituteInPlace tools/release/sbom.py \
      --replace-fail 'commit = git(source, "rev-parse", "HEAD")' \
        'checkout = next(path for path in (source, *source.parents) if (path / ".map-source-revision").is_file())' \
      --replace-fail 'checkout = pathlib.Path(git(source, "rev-parse", "--show-toplevel"))' \
        'commit = (checkout / ".map-source-revision").read_text().strip()'
    ${lib.optionalString platform.isLinux ''
      # Same-triple Cargo build scripts must run in the Nix sandbox, not use
      # Zig's /lib64 ELF interpreter. Keep C/C++ on the official toolchain.
      substituteInPlace cmake/mln_ffi_rust.cmake \
        --replace-fail '"CARGO_TARGET_''${rust_target_env}_LINKER=''${rust_linker}"' \
          '"CARGO_TARGET_''${rust_target_env}_LINKER=${hostLinker}"'
    ''}
  '';
  configurePhase = ''
    runHook preConfigure
    export HOME="$TMPDIR/home"
    mkdir -p "$HOME"
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
    ${lib.optionalString platform.isLinux ''
      # zlib's CMake configure renames a header in its source tree.
      cp -R ${zlibSource} "$TMPDIR/zlib"
      chmod -R u+w "$TMPDIR/zlib"
    ''}
    cmake --preset ${preset} -B build/nix \
      -DCMAKE_INSTALL_PREFIX="$out" \
      -DBUILD_TESTING=OFF \
      -DCMAKE_PROJECT_INCLUDE=${injection} \
      ${lib.optionalString platform.isLinux ''-DFETCHCONTENT_SOURCE_DIR_MLN_FFI_ZLIB_SOURCE="$TMPDIR/zlib" -DFETCHCONTENT_SOURCE_DIR_MLN_FFI_LIBUV_SOURCE=${libuvSource}''} \
      -DFETCHCONTENT_FULLY_DISCONNECTED=ON
    runHook postConfigure
  '';
  buildPhase = ''
    runHook preBuild
    cmake --build build/nix --config Release --parallel "$NIX_BUILD_CORES"
    ${lib.optionalString platform.isLinux ''
      objcopy=$(sed -n 's/^CMAKE_OBJCOPY:FILEPATH=//p' build/nix/CMakeCache.txt)
      "$objcopy" ${
        lib.concatMapStringsSep " " (symbol: "--globalize-symbol=${symbol}") compositorSymbols
      } \
        build/nix/maplibre_native_c_static-complete-static/libmaplibre-native-c.a
    ''}
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    cmake --install build/nix --config Release
    runHook postInstall
  '';
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    ${pkgs.jq}/bin/jq -e \
      '.commit == "${revision}" and .platform.rustTarget == "${rustTarget}" and .backend == "${backend}"' \
      "$out/share/maplibre-native-c/artifact.json"
    test -s "$out/lib/libmaplibre-native-c.a"
    test -s "$out/lib/libmln_ffi_platform.a"
    test -s "$out/share/maplibre-native-c/sbom.cdx.json"
    ${pkgs.llvmPackages.llvm}/bin/llvm-nm --defined-only --extern-only \
      "$out/lib/libmaplibre-native-c.a" > symbols.txt
    for symbol in mln_custom_layer_add ${lib.optionalString platform.isLinux "mln_custom_layer_vulkan_context"} ${lib.concatStringsSep " " compositorSymbols}; do
      grep -Eq " [A-Z] _?$symbol$" symbols.txt
    done
    runHook postInstallCheck
  '';
  passthru = { inherit revision preset; };
  meta = {
    description = "Official MapLibre Native FFI with Map's custom layer and raster compositor";
    license = lib.licenses.bsd2;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ];
  };
}
