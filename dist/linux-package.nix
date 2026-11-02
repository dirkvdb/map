# Release-only inputs. Do not mix these target libraries into the native devenv shell.
{
  inputs,
  pkgs,
  lib,
  hostLinker,
}:
let
  arch = pkgs.stdenv.hostPlatform.parsed.cpu.name;
  artifacts =
    {
      x86_64 = {
        appimagetoolHash = "04ws94q71bwskmhizhwmaf41ma4wabvfgjgkagr8wf3vakgv866r";
        runtimeHash = "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d";
      };
      aarch64 = {
        appimagetoolHash = "06nl3ckryw91wf0iicrhlr1lkd2z5gzhyd41ppxvj12f18qmil69";
        runtimeHash = "0i3lv1j8d9yqssh2i97idmdg9xx11jgdairkdpzw1ikwj77xzjq0";
      };
    }
    .${arch};
  ui = import ./linux-ui.nix { inherit inputs pkgs lib; };
  targetPkgs = ui.uiPkgs;
  maplibreNative = import ./maplibre-native.nix { pkgs = targetPkgs; };
  nativeLibraries = lib.closePropagation (
    with targetPkgs;
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

      pkg-mod-libuv
    ]
  );
  # Zig outputs use SONAMEs without Nix RUNPATHs, so these dependencies are not
  # automatically retained by the Nix reference closure.
  uiLibraries =
    builtins.attrValues ui.libraries
    ++ (with targetPkgs; [
      libffi
      libxau
      libxdmcp
    ]);
  libraries = nativeLibraries ++ uiLibraries;
  libraryPath = lib.makeLibraryPath (
    map (package: package // { outputSpecified = false; }) libraries
  );
  # MapLibre probes the standard libuv name. GDAL's flattened curl flags omit
  # Brotli's search directory, so retain curl's own static dependency metadata.
  packagePkgConfig = pkgs.runCommand "map-release-pkgconfig" { } ''
    mkdir -p "$out/lib/pkgconfig"
    ln -s ${lib.getDev targetPkgs.pkg-mod-libuv}/lib/pkgconfig/libuv-static.pc "$out/lib/pkgconfig/libuv.pc"
    cp ${lib.getDev targetPkgs.pkg-mod-gdal}/lib/pkgconfig/gdal.pc "$out/lib/pkgconfig/gdal.pc"
    chmod u+w "$out/lib/pkgconfig/gdal.pc"
    printf '\nRequires.private: libcurl\n' >> "$out/lib/pkgconfig/gdal.pc"
  '';
  compiler = targetPkgs.stdenv.cc;
  prefix = compiler.targetPrefix;
  # rustc passes this erratum workaround to AArch64 linkers, but Zig's driver
  # rejects it. Keep the adapter target-only; host build scripts use hostLinker.
  rustLinker =
    if arch == "aarch64" then
      pkgs.writeShellScript "map-linux-target-linker" ''
        args=()
        for arg in "$@"; do
          if [[ "$arg" != "-Wl,--fix-cortex-a53-843419" ]]; then
            args+=("$arg")
          fi
        done
        exec ${compiler}/bin/${prefix}cc "''${args[@]}"
      ''
    else
      "${compiler}/bin/${prefix}cc";
  appimagetool = pkgs.appimageTools.wrapType2 {
    pname = "appimagetool";
    version = "12";
    src = pkgs.fetchurl {
      url = "https://github.com/AppImage/AppImageKit/releases/download/12/appimagetool-${arch}.AppImage";
      sha256 = artifacts.appimagetoolHash;
    };
    extraPkgs = packages: [ packages.squashfsTools ];
  };
in
assert lib.assertMsg (
  pkgs.stdenv.hostPlatform.isLinux
  && builtins.elem arch [
    "x86_64"
    "aarch64"
  ]
) "AppImage packaging supports x86_64 and aarch64 Linux";
pkgs.writeText "map-linux-package.json" (
  builtins.toJSON {
    target = targetPkgs.stdenv.hostPlatform.rust.rustcTarget;
    maplibreNativeBuild = "${maplibreNative}";
    inherit arch;
    glibc = "2.28";
    hostCc = "${hostLinker}";
    linker = "${rustLinker}";
    cc = "${compiler}/bin/${prefix}cc";
    cxx = "${compiler}/bin/${prefix}c++";
    ar = "${compiler}/bin/${prefix}ar";
    # GPUI's unused font-kit backend still probes fontconfig at build time. Its
    # native metadata is sufficient; the ELF guard rejects any runtime leakage.
    pkgConfigPath = lib.makeSearchPathOutput "dev" "lib/pkgconfig" (
      [ packagePkgConfig ] ++ libraries ++ lib.closePropagation [ pkgs.fontconfig ]
    );
    inherit libraryPath;
    includePaths = map (package: "${lib.getDev package}/include") libraries;
    cmakePrefixPath = lib.concatStringsSep ":" (
      map (package: "${lib.getDev package}") libraries
      ++ map (package: "${lib.getLib (package // { outputSpecified = false; })}") libraries
    );

    gdal = "${targetPkgs.pkg-mod-gdal}";
    caBundle = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    xkbData = "${targetPkgs.xkeyboard_config}/etc/X11/xkb";
    composeData = "${targetPkgs.libx11}/share/X11/locale";
    # Use the upstream static bootstrap, not AppImageKit 12's host-zlib runtime.
    runtime = "${pkgs.fetchurl {
      name = "appimage-runtime-${arch}-20251108";
      url = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-${arch}";
      sha256 = artifacts.runtimeHash;
    }}";
    runtimeClosure = "${pkgs.closureInfo { rootPaths = uiLibraries; }}/store-paths";
    runtimeLibraries = [
      "${targetPkgs.libxkbcommon}/lib/libxkbcommon.so.0"
      "${targetPkgs.libxkbcommon}/lib/libxkbcommon-x11.so.0"
      "${targetPkgs.libxcb}/lib/libxcb.so.1"
      "${targetPkgs.wayland}/lib/libwayland-client.so.0"
      "${targetPkgs.wayland}/lib/libwayland-egl.so.1"
      "${targetPkgs.vulkan-loader}/lib/libvulkan.so.1"
      "${targetPkgs.libglvnd}/lib/libEGL.so.1"
      "${targetPkgs.libglvnd}/lib/libGLdispatch.so.0"

    ];
    tools = {
      linuxdeploy = "${pkgs.linuxdeploy}/bin/linuxdeploy";
      appimagetool = "${appimagetool}/bin/appimagetool";
      patchelf = "${pkgs.patchelf}/bin/patchelf";
      readelf = "${pkgs.binutils}/bin/readelf";
      magick = "${pkgs.imagemagick}/bin/magick";
    };
  }
)
