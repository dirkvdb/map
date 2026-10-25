# Release-only inputs. Do not mix these target libraries into the native devenv shell.
{
  inputs,
  pkgs,
  lib,
  hostLinker,
}:
let
  ui = import ./linux-ui.nix { inherit inputs pkgs lib; };
  targetPkgs = ui.uiPkgs;
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
  # Fix libuv's duplicated absolute prefix and expose MapLibre's standard name.
  # GDAL's flattened curl flags omit Brotli's search directory, so retain curl's
  # own static dependency metadata.
  packagePkgConfig = pkgs.runCommand "map-release-pkgconfig" { } ''
    mkdir -p "$out/lib/pkgconfig"
    sed -e 's|^libdir=.*|libdir=${lib.getLib (targetPkgs.pkg-mod-libuv // { outputSpecified = false; })}/lib|' \
        -e 's|^includedir=.*|includedir=${lib.getDev targetPkgs.pkg-mod-libuv}/include|' \
        ${lib.getDev targetPkgs.pkg-mod-libuv}/lib/pkgconfig/libuv-static.pc \
        > "$out/lib/pkgconfig/libuv.pc"
    cp ${lib.getDev targetPkgs.pkg-mod-gdal}/lib/pkgconfig/gdal.pc "$out/lib/pkgconfig/gdal.pc"
    chmod u+w "$out/lib/pkgconfig/gdal.pc"
    printf '\nRequires.private: libcurl\n' >> "$out/lib/pkgconfig/gdal.pc"
  '';
  compiler = targetPkgs.stdenv.cc;
  prefix = compiler.targetPrefix;
  appimagetool = pkgs.appimageTools.wrapType2 {
    pname = "appimagetool";
    version = "12";
    src = pkgs.fetchurl {
      url = "https://github.com/AppImage/AppImageKit/releases/download/12/appimagetool-x86_64.AppImage";
      sha256 = "04ws94q71bwskmhizhwmaf41ma4wabvfgjgkagr8wf3vakgv866r";
    };
    extraPkgs = packages: [ packages.squashfsTools ];
  };
in
assert lib.assertMsg pkgs.stdenv.hostPlatform.isx86_64
  "AppImage packaging currently supports x86_64 Linux";
pkgs.writeText "map-linux-package.json" (
  builtins.toJSON {
    target = "x86_64-unknown-linux-gnu";
    arch = "x86_64";
    glibc = "2.28";
    hostCc = "${hostLinker}";
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
      name = "appimage-runtime-x86_64-20251108";
      url = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64";
      sha256 = "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d";
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
