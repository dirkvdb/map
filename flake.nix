{
  description = "Geospatial data viewer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/stable";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        rustToolchain = pkgs.rust-bin.stable."1.98.1".minimal;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        linuxRuntimeDeps = with pkgs; [
          fontconfig
          libxcb
          libxkbcommon
          wayland
          vulkan-loader
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
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux linuxRuntimeDeps
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.apple-sdk_15 ];

          nativeCheckInputs = [ pkgs.cacert ];
          preCheck = ''
            # The build sandbox does not expose the host's trusted certificates.
            export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
          '';

          postInstall = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            install -Dm644 map.desktop $out/share/applications/map.desktop
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
