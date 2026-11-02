{
  description = "Geospatial data viewer";

  inputs = {
    infra-rs = {
      url = "https://github.com/VITObelgium/infra-rs/archive/refs/heads/main.tar.gz";
      flake = false;
    };

    pkgs-mod.url = "github:VITO-RMA/nix-pkgs/main";
    nixpkgs.follows = "pkgs-mod/nixpkgs";

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
      infra-rs,

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
        maplibreNative = import ./dist/maplibre-native.nix { inherit pkgs; };
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
        packages.maplibre-native = maplibreNative;
        packages.default = rustPlatform.buildRustPackage {
          pname = "map";
          version = (fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.runCommand "map-source" { } ''
            cp -R ${./.} "$out"
            chmod u+w "$out"
            rm -rf "$out/infra-rs"
            cp -R ${infra-rs} "$out/infra-rs"
          '';

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.pkg-config

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
              pkg-mod-gdal
              pkg-mod-proj
              pkg-mod-curl
              pkg-mod-libwebp
              pkg-mod-libpng
              pkg-mod-libjpeg
              pkg-mod-sqlite
              pkg-mod-openssl
              pkg-mod-zlib-compat
              fontconfig.dev
              vulkan-headers
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux (
              [
                pkgs.pkg-mod-libuv
                libuvPkgConfig
              ]
              ++ linuxRuntimeDeps
            )
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.apple-sdk_15 ];

          MLN_NATIVE_BUILD = "${maplibreNative}";
          MLN_STATIC_DEPS = "1";
          WEBGPU_SHIM_HEADER = "${maplibreNative.webgpuHeader}";
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          CC = "${pkgs.llvmPackages.clang}/bin/clang";
          CXX = "${pkgs.llvmPackages.clang}/bin/clang++";

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
