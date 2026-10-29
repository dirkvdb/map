{
  description = "Geospatial data viewer";

  inputs = {
    self.submodules = true;
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    pkgs-mod = {
      url = "github:VITO-RMA/nix-pkgs/main";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    maplibre-native = {
      url = "git+https://github.com/maplibre/maplibre-native.git?ref=refs/tags/core-a33d3f00fb9dbf9117ce18edd799aa0ca5bcce5b&rev=a33d3f00fb9dbf9117ce18edd799aa0ca5bcce5b&submodules=1&shallow=1";
      flake = false;
    };
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/stable";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      pkgs-mod,
      maplibre-native,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            rust-overlay.overlays.default
            (pkgs-mod.lib.mkOverlay { static = true; })
          ];
        };
        rustToolchain = pkgs.rust-bin.stable."1.99.0".minimal;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        webgpuHeader = pkgs.fetchurl {
          url = "https://raw.githubusercontent.com/webgpu-native/webgpu-headers/673658bc2bd70ec39fc55ebe6bb0173cf6d0a603/webgpu.h";
          hash = "sha256-pIMDHD/tBepd0cdAgqcWdsRsWyuCDMyhDaUVwDPvyZc=";
        };
        # MapLibre probes libuv.pc; the static overlay provides libuv-static.pc.
        libuvPkgConfig = pkgs.runCommand "map-libuv-pkgconfig" { } ''
          mkdir -p $out/lib/pkgconfig
          ln -s ${pkgs.lib.getDev pkgs.pkg-mod-libuv}/lib/pkgconfig/libuv-static.pc $out/lib/pkgconfig/libuv.pc
        '';
        linuxRuntimeDeps = with pkgs; [
          fontconfig
          libxcb
          libxkbcommon
          wayland
          vulkan-loader
          libglvnd
        ];
      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "map";
          version = (fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.cmake
            pkgs.ninja
            pkgs.git
            pkgs.python3
            pkgs.llvmPackages.clang
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.makeWrapper
            pkgs.imagemagick
          ];
          buildInputs =
            with pkgs;
            [
              fontconfig.dev
              vulkan-headers
              pkg-mod-gdal
              pkg-mod-proj
              pkg-mod-curl
              pkg-mod-libwebp
              pkg-mod-libpng
              pkg-mod-libjpeg
              pkg-mod-sqlite
              pkg-mod-openssl
              pkg-mod-zlib-compat
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux (
              linuxRuntimeDeps
              ++ [
                pkgs.pkg-mod-libuv
                libuvPkgConfig
              ]
            )
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.apple-sdk_15 ];

          dontUseCmakeConfigure = true;
          dontUseNinjaBuild = true;
          dontUseNinjaCheck = true;
          dontUseNinjaInstall = true;
          MLN_PRECOMPILE = "0";
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          CC = "${pkgs.llvmPackages.clang}/bin/clang";
          CXX = "${pkgs.llvmPackages.clang}/bin/clang++";

          postPatch = ''
            # Seed the build script's verified header cache without sandbox networking.
            substituteInPlace vendor/webgpu-shim/build.rs \
              --replace-fail 'let file = out_path.join("webgpu.h");' \
                'let file = out_path.join("webgpu.h"); if !file.exists() { fs::copy("${webgpuHeader}", &file).expect("Could not copy the prefetched WebGPU header"); }'

            # The build script patches the native sources and runs git submodule.
            cp -R ${maplibre-native} "$TMPDIR/maplibre-native"
            chmod -R u+w "$TMPDIR/maplibre-native"
            git -C "$TMPDIR/maplibre-native" init
            export MLN_LOCAL_REPOSITORY="$TMPDIR/maplibre-native"
          '';

          nativeCheckInputs = [ pkgs.cacert ];
          preCheck = ''
            # The build sandbox does not expose the host's trusted certificates.
            export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
          '';

          postInstall = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            install -Dm644 dist/map.desktop $out/share/applications/map.desktop
            install -d $out/share/icons/hicolor/512x512/apps
            magick data/icon.png -resize 512x512 $out/share/icons/hicolor/512x512/apps/map.png
          '';

          postFixup = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            wrapProgram $out/bin/map \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath linuxRuntimeDeps}
          '';
        };
      }
    );
}
