{
  pkgs,
  lib,
  inputs,
  config,
  ...
}:
let
  # Linux still needs an explicit CA bundle; macOS uses the OS certificates.
  mapCurl =
    if pkgs.stdenv.isLinux then
      pkgs.pkg-mod-curl.overrideAttrs (old: {
        cmakeFlags = old.cmakeFlags ++ [
          "-DCURL_CA_BUNDLE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
        ];
      })
    else
      pkgs.pkg-mod-curl;

  packageLibraries = lib.closePropagation [
    mapCurl
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

  packages =
    with pkgs;
    [
      # Search this curl before the unconfigured copy propagated by GDAL.
      mapCurl
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
      pkg-mod-icu
      pkg-mod-libpng
      pkg-mod-libjpeg
      pkg-mod-libwebp
    ]
    ++ lib.optionals pkgs.stdenv.isLinux [
      fontconfig
      fontconfig.dev
      vulkan-headers
      libxkbcommon
      xorg.libxcb
      libuv
      libglvnd
      libglvnd.dev
    ]
    ++ lib.optionals pkgs.stdenv.isDarwin [
      apple-sdk_15
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
    MAP_PACKAGE_PKG_CONFIG_PATH = lib.makeSearchPathOutput "dev" "lib/pkgconfig" packageLibraries;
    MAP_PACKAGE_GDAL_HOME = "${pkgs.pkg-mod-gdal}";
    # getLib otherwise preserves an explicitly selected -dev output. Reselect
    # lib (or out for packages without a lib output) for propagated dependencies.
    MAP_PACKAGE_LIBRARY_PATH = lib.makeLibraryPath (
      map (package: package // { outputSpecified = false; }) packageLibraries
    );
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
