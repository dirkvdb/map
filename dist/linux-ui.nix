# Portable UI libraries for Linux packaging; ordinary development builds stay native.
{
  inputs,
  pkgs,
  lib,
}:
let
  portablePkgs =
    (inputs.pkgs-mod.lib.mkBuildEnvWith {
      system = pkgs.stdenv.hostPlatform.system;
      glibcVersion = "2.28";
    }).pkgsStatic;

  uiPkgs = portablePkgs.extend (
    final: prev: {
      wayland = prev.wayland.override {
        withTests = false;
        withDocumentation = false;
      };

      vulkan-loader = prev.vulkan-loader.overrideAttrs (old: {
        cmakeFlags = lib.filter (flag: !(lib.hasPrefix "-DSYSCONFDIR=" flag)) old.cmakeFlags ++ [
          "-DSYSCONFDIR=/etc"
        ];
      });

      libxkbcommon = (prev.libxkbcommon.override { withWaylandTools = false; }).overrideAttrs (old: {
        outputs = [
          "out"
          "dev"
        ];
        nativeBuildInputs = lib.filter (
          package:
          !(lib.elem (lib.getName package) [
            "doxygen"
            "xvfb"
          ])
        ) old.nativeBuildInputs;
        mesonFlags = lib.filter (flag: flag != "-Denable-docs=true") old.mesonFlags ++ [
          "-Denable-docs=false"
          "-Denable-tools=false"
        ];
        # Cross-built executables use the distro loader, which is absent in Nix builds.
        doCheck = false;
      });
    }
  );
in
{
  inherit uiPkgs;
  libraries = {
    inherit (uiPkgs)
      libxkbcommon
      libxcb
      wayland
      vulkan-loader
      libglvnd
      ;
  };
}
