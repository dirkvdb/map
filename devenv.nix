{
  pkgs,
  lib,
  inputs,
  config,
  ...
}:
let
  linuxHostLinker = pkgs.writeShellScript "map-linux-host-linker" ''
    # Override rustc's x86_64 LLD default: the bundled Nix wrapper contains
    # unresolved @mktemp@/@rm@ placeholders. Keep host code on Nix's binutils.
    # Same-triple C++ build dependencies inherit the portable compiler's libc++
    # selection, but host executables need the native library and runtime path.
    exec ${pkgs.stdenv.cc}/bin/cc "$@" -fuse-ld=bfd \
      -L${lib.getLib pkgs.llvmPackages.libcxx}/lib \
      -Wl,-rpath,${lib.getLib pkgs.llvmPackages.libcxx}/lib
  '';
  # MapLibre probes libuv, while the static overlay only ships libuv-static.pc.
  linuxLibuvPkgConfig = pkgs.runCommand "map-linux-libuv-pkgconfig" { } ''
    mkdir -p "$out/lib/pkgconfig"
    ln -s ${lib.getDev pkgs.pkg-mod-libuv}/lib/pkgconfig/libuv-static.pc "$out/lib/pkgconfig/libuv.pc"
  '';
  packageLibraries = lib.closePropagation [
    pkgs.pkg-mod-curl
    pkgs.pkg-mod-libwebp
    pkgs.pkg-mod-gdal
    pkgs.pkg-mod-proj
    pkgs.pkg-mod-libpng
    pkgs.pkg-mod-libjpeg
    pkgs.pkg-mod-sqlite
    pkgs.pkg-mod-openssl
    pkgs.pkg-mod-zlib-compat
  ];
in
{
  cachix.pull = [ "geo-overlay" ];

  overlays = [
    (inputs.pkgs-mod.lib.mkOverlay {
      static = true;
    })
  ];

  languages.rust = {
    enable = true;
    channel = "stable";
    version = "1.95.0";
    components = [
      "rustc"
      "cargo"
      "rust-src"
      "rustfmt"
      "clippy"
    ];
  };

  # Replace devenv's default SDK instead of adding a second one to the profile.
  apple.sdk = if pkgs.stdenv.isDarwin then pkgs.apple-sdk_15 else null;

  packages =
    with pkgs;
    [
      pkg-mod-curl
      cargo-nextest
      sccache
      pkg-config

      python3
      pkg-mod-zlib-compat
      cmake
      ninja
      pkg-mod-gdal
      pkg-mod-proj
      pkg-mod-openssl

      pkg-mod-libpng
      pkg-mod-libjpeg
      pkg-mod-libwebp
    ]
    ++ lib.optionals pkgs.stdenv.isLinux [
      pkg-mod-libuv
      linuxLibuvPkgConfig
      fontconfig
      fontconfig.dev
      vulkan-headers
      libxkbcommon
      libxcb
      libglvnd
      libglvnd.dev
    ]
    ++ lib.optionals pkgs.stdenv.isDarwin [
      cargo-bundle
    ];

  env = {
    NIX_CONFIG = ''
      max-jobs = auto
      cores = 0
    '';
    RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
    MLN_PRECOMPILE = "0";
    MLN_LOCAL_REPOSITORY = "${config.devenv.root}/.devenv/maplibre-native";
    MLN_CMAKE_CXX_LAUNCHER = "${pkgs.sccache}/bin/sccache";
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    CC = "${pkgs.llvmPackages.clang}/bin/clang";
    CXX = "${pkgs.llvmPackages.clang}/bin/clang++";
  }
  // lib.optionalAttrs pkgs.stdenv.isDarwin {
    # cc-rs uses macosx where Nix expects darwin for the same native target.
    NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";
    MAP_PACKAGE_PKG_CONFIG_PATH = lib.makeSearchPathOutput "dev" "lib/pkgconfig" packageLibraries;
    MAP_PACKAGE_GDAL_HOME = "${pkgs.pkg-mod-gdal}";
    # getLib otherwise preserves an explicitly selected -dev output. Reselect
    # lib (or out for packages without a lib output) for propagated dependencies.
    MAP_PACKAGE_LIBRARY_PATH = lib.makeLibraryPath (
      map (package: package // { outputSpecified = false; }) packageLibraries
    );
  }
  // lib.optionalAttrs pkgs.stdenv.isLinux {
    MAP_PACKAGE_HOST_LINKER = "${linuxHostLinker}";
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
    LD_LIBRARY_PATH = lib.makeLibraryPath [
      pkgs.wayland
      pkgs.libxkbcommon
      pkgs.xorg.libxcb
      pkgs.vulkan-loader
    ];
  };

  outputs = lib.optionalAttrs pkgs.stdenv.isLinux {
    linux-package = import ./dist/linux-package.nix {
      inherit inputs pkgs lib;
      hostLinker = linuxHostLinker;
    };
    windows-package = import ./dist/windows-package.nix { inherit inputs pkgs lib; };
    linux-ui = pkgs.linkFarm "map-linux-ui" (
      lib.mapAttrsToList (name: path: { inherit name path; })
        (import ./dist/linux-ui.nix { inherit inputs pkgs lib; }).libraries
    );
  };

  tasks."ci:checks" = {
    exec = "just build && just test";
  };

  tasks."maplibre:prepare" = {
    before = [ "devenv:enterShell" ];
    exec = ''python3 "$DEVENV_ROOT/scripts/prepare_maplibre.py" "$MLN_LOCAL_REPOSITORY"'';
  };

  enterShell = ''
    skill_dir="$DEVENV_ROOT/.agents/skills/rust-best-practices"

    ${pkgs.coreutils}/bin/mkdir -p "$DEVENV_ROOT/.agents/skills"
    ${pkgs.coreutils}/bin/rm -rf -- "$skill_dir"
    ${pkgs.coreutils}/bin/cp -rL --no-preserve=mode -- \
      "${inputs.s-stack}/skills/rust-best-practices" \
      "$skill_dir"
  '';
}
