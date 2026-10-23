{
  pkgs,
  lib,
  inputs,
  config,
  ...
}:
let
  # The geo overlay's CMake-built libcurl has no CA bundle in the Nix sandbox.
  # Give the final executable a TLS-verifying static curl without rebuilding
  # the entire GDAL dependency tree just to change curl's default trust store.
  mapCurl = pkgs.pkg-mod-curl.overrideAttrs (old: {
    cmakeFlags = old.cmakeFlags ++ [
      "-DCURL_CA_BUNDLE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
    ];
  });
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

  packages =
    with pkgs;
    [
      # Search this curl before the unconfigured copy propagated by GDAL.
      mapCurl
      cargo-nextest
      sccache
      pkg-config
      pkg-mod-gdal
      pkg-mod-proj
      pkg-mod-openssl
      pkg-mod-maplibre-native
      fontconfig
      fontconfig.dev
      vulkan-headers
      libxkbcommon
      xorg.libxcb
      python3
      curl
      zlib
      libuv
      cmake
      ninja
      icu
      libpng
      libjpeg
      libwebp
    ]
    ++ lib.optionals pkgs.stdenv.isLinux [
      libglvnd
      libglvnd.dev
    ]
    ++ lib.optionals pkgs.stdenv.isDarwin [
      apple-sdk_15
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
  // lib.optionalAttrs pkgs.stdenv.isLinux {
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
    LD_LIBRARY_PATH = lib.makeLibraryPath [
      pkgs.wayland
      pkgs.libxkbcommon
      pkgs.xorg.libxcb
      pkgs.vulkan-loader
    ];
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
