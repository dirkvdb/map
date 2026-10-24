# Packaging

Packaging uses devenv and the static `pkg-mod-*` dependencies from the geo
overlay, building MapLibre Native from its locked source revision.

- macOS app: `dist/build/devenv/release/bundle/osx/Map.app`
- macOS DMG: `dist/build/devenv/release/bundle/dmg/Map.dmg`
- Linux AppImage output: `dist/build/devenv/release/bundle/appimage/Map-x86_64.AppImage`
- Windows executable: `dist/build/devenv/release/bundle/windows/Map.exe`

All three CI jobs install Nix and devenv pinned to `v2.4.0`, and pull from the
`geo-overlay`, `devenv`, and `cachix` binary caches without pushing. Linux CI
and local `just appimage` use the same `scripts/package_linux.py` pipeline;
neither uses mise or vcpkg. A first package build can take significantly longer
than incremental builds. Windows CI cross-compiles on Ubuntu with the same
`scripts/package_windows.py` pipeline as `just exe`, tests resource embedding,
and uploads `Map-Windows-X64`. Tagged releases include `Map.exe` alongside the
macOS DMG and Linux AppImage.

**Linux build and loader validation passed locally:** `just appimage` created
the image, which extracted and resolved its shared libraries in an offline
Debian/glibc-2.28 container without a Nix store. Full graphical application launch
and rendering have not yet been validated.

macOS CI retains sccache. AppImage builds explicitly disable Rust compiler
wrappers and C/C++ compiler launchers; they do not use sccache. Nix binary-cache
substitution and ordinary incremental Cargo/Ninja builds remain available.

## Local macOS build

Install Nix, devenv (CI pins `v2.4.0`), `just`, and Apple's Xcode or Command
Line Tools. From the repository root, preflight the packaging environment
without building:

```sh
devenv shell -- python3 scripts/package_macos.py --check
```

Then build:

```sh
just dmg
```

`just dmg` enters devenv and runs `scripts/package_macos.py`. This single script
prepares the environment, builds and checks the app, signs it, and creates and
verifies the DMG. CI uses the same entry point.
There is no macOS mise or vcpkg toolchain.

The build uses:

- **Devenv:** Rust, cargo-bundle, CMake, Ninja, Python, and the overlay's static
  third-party libraries. MapLibre's source checkout is pinned by the devenv
  preparation task. Packaging artifacts are isolated under `dist/build/devenv`.
- **Apple:** the native SDK, C/C++/Swift compilers, linker, signing, and disk-image
  tools. This avoids Nix compiler wrappers injecting Nix-store runtime libraries.
- **Explicit dependency selection:** isolated pkg-config paths and Rust library
  search paths select the overlay's archive outputs, including transitive
  dependencies. MapLibre requires static archives rather than falling back to
  shared libraries.

Two toolchain compatibility settings remain necessary:

- CMake's Python interpreter is pinned to devenv's Python (3.10+), avoiding the
  older Python found alongside Apple's tools and stale CMake interpreter caches.
- `CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none` prevents malformed proc-macro
  dylibs with the local Rust/Apple-linker combination. It changes only build-time
  dependencies, not the application's release profile.

The dependency guard runs on both the release binary and `Map.app`. It permits
only `/usr/lib/` and `/System/Library/` dependencies: Apple's runtime (including
libc++) and frameworks remain dynamic, but Nix-store and Homebrew dylibs are
rejected before signing. The bundle check also verifies the executable and icon.

A dependency-free native regression test builds and executes a tiny release
application using a proc-macro, without realizing Nix packages or downloading
Cargo dependencies:

```sh
MAP_RUN_NATIVE_TESTS=1 python3 -B -m unittest discover -s scripts/tests
```

The app is ad-hoc signed but not Developer ID-signed or notarized. The devenv
DMG build has completed successfully on Apple Silicon, including dependency
checks on the release binary and bundled executable, code-signature
verification, and DMG checksum verification. The executable in a read-only mount
of the generated DMG was also checked. It links only to Apple system libraries
and frameworks; it has no non-system dylib
dependencies. GitHub Actions and Intel macOS builds still require validation.

## Local Linux build

Requires x86_64 Linux, Nix, devenv, and `just`:

```sh
just appimage
```

The recipe uses the same command as CI:

```sh
devenv shell -- python3 scripts/package_linux.py
```

Release inputs are declared in `dist/linux-package.nix`; they are isolated from the native
libraries used by normal development builds. To realize/check the inputs
without compiling Map:

```sh
devenv shell -- python3 scripts/package_linux.py --check
```

The build uses stock Rust with pkgs-mod's Zig linker targeting glibc 2.28 and
static geospatial dependencies, in `dist/build/appimage/target`. The pkgs-mod pin
`90cb6215` fixes the old PROJ/GDAL pkg-config metadata blocker involving temporary
Zig-cache paths and host glibc paths. Release-only pkg-config metadata retains
GDAL's private `libcurl` dependency so static linking includes curl's transitive
dependencies and Brotli search paths. These adjustments do not affect ordinary
development builds.

Compiler wrappers and MLN/CMake launchers are explicitly cleared, including
Cargo user-config wrappers; no sccache or cache workaround is used. Native
dependencies use explicit static link mode. MapLibre selects static dependencies
and the target curl archive instead of a host CMake package. No host development
packages are needed for this pipeline.

After linking, `scripts/appimage_bundle.py` uses linuxdeploy and hash-pinned
AppImageKit 12 to create the AppDir and image. A separate hash-pinned **static
type2 runtime `20251108`** is supplied with `--runtime-file`, avoiding the old
AppImageKit bootstrap's dependency on host zlib. The AppDir includes UI loaders,
keyboard/Compose data and a fallback CA bundle; no external GDAL/PROJ data is shipped. ELF guards reject newer
glibc requirements, Nix interpreter/dependency paths, unresolved shared libraries,
and bundled host glibc or GPU drivers. Host GPU drivers remain external.

Rebuilding an existing AppDir restores directory write permissions before
removal, including directories copied read-only from Nix outputs or left by a
failed build. The AppDir root is explicitly mode `755`.

### CI loader/bootstrap smoke test

After packaging, CI pulls `debian:buster-slim` (glibc 2.28) and runs the test
with `--network none`, mounting only the artifact directory read-only. The
AppImage's own bootstrap extracts it under `/tmp` without FUSE. The test checks
that the extracted root is mode `755`, then runs `ldd` on `usr/bin/map` using
only its relative RPATHs, without `LD_LIBRARY_PATH`. A failed loader check,
`not found`, or `/nix/store` in the output fails the job. No host development
packages or Nix store are mounted into the container.

The local `just appimage` build and this clean non-Nix **loader/bootstrap test**
have both passed. The application and bundled libraries meet the glibc 2.28 ELF
checks; the bootstrap is statically linked. This is not a full graphical launch
or UI-rendering test, which still requires validation with suitable host GPU
drivers and an X11 or Wayland session.

### Portable devenv UI builds

The repository-local `dist/linux-ui.nix` builds `libxkbcommon`, `libxcb`,
Wayland, the Vulkan loader, and libglvnd with pkgs-mod's Zig/glibc-2.28
package set. Its small UI overrides stay in this repository; pkgs-mod is
unchanged. Build these libraries independently with:

```sh
devenv build outputs.linux-ui
```

The output contains a named link to each library package. Ordinary development
builds are unchanged. The UI overrides and runtime dependency selection remain
in this repository, not in pkgs-mod.

On x86_64 Linux, all five packages built successfully, and the shared libraries
in those package outputs had no GLIBC symbol requirements newer than 2.28.
The full application also links using devenv's stock Rust compiler and the
portable linker. The generated AppImage passed extraction and loader checks
on glibc 2.28 without a Nix store; graphical rendering remains unvalidated.


#### Runtime CA trust

Linux libcurl is built with `CURL_CA_BUNDLE=none`, `CURL_CA_PATH=none`, and
`CURL_CA_FALLBACK=ON`. It uses OpenSSL's standard `SSL_CERT_FILE` and
`SSL_CERT_DIR` handling, so MapLibre needs no CA patch. GDAL also supports
`SSL_CERT_FILE`. Certificate-chain and hostname verification remain enabled.
OpenSSL may also trust certificates from `SSL_CERT_DIR` (or its default CA
directory); an invalid CA file does not disable verification.

`dist/AppRun` only sets `XKB_CONFIG_ROOT`, `XLOCALEDIR`, and `SSL_CERT_FILE`.
The keyboard paths replace libxkbcommon's compiled-in Nix data paths. Relative
ELF RPATHs handle shared libraries, so the launcher does not modify library,
executable, or XDG search paths. It does not set GDAL data/plugin variables.

PROJ embeds its database; GDAL's enabled raster paths do not require the copied
GDAL data directory. These are separate resource mechanisms. The image ships
built-in GDAL format drivers, not plugins; the launcher leaves plugin policy
to GDAL rather than configuring a path for plugins we do not package.

An offline glibc-2.28 check loaded the bundled Wayland, EGL, and Vulkan libraries
without `LD_LIBRARY_PATH`. GDAL from the same package also reprojected an ASCII
grid to GeoTIFF in that container without external GDAL/PROJ data directories.

The launcher uses an explicit `SSL_CERT_FILE`, or selects a readable standard
host bundle, falling back to the PEM packaged under
`usr/share/map/certs/ca-bundle.crt`. For compatibility, a nonempty
`CURL_CA_BUNDLE` takes precedence and is copied into `SSL_CERT_FILE`, keeping
GDAL and MapLibre on the same bundle. `SSL_CERT_DIR` is left unchanged.
The fallback bundle must be updated with application releases. Ordinary
devenv builds use the same unmodified pkgs-mod curl and OpenSSL trust handling.

The offline regression tests compile an unpatched libcurl client and exercise
it against a local HTTPS server, including handle resets and rejection cases:

```sh
devenv shell -- env MAP_RUN_NATIVE_TESTS=1 python3 -B -m unittest discover -s scripts/tests -p test_ca_bundle.py -v
```

Packaging/launcher regression tests do not require an application build:

```sh
python3 -B -m unittest discover -s scripts/tests -v
```

The AppImage is unsigned. Direct mounting requires host FUSE support;
an extracted AppDir can instead be run through its `AppRun` launcher. GitHub's
artifact ZIP may remove the executable bit; restore it with
`chmod +x Map-x86_64.AppImage` after downloading.

## Local Windows cross-build

On Linux, with Nix, devenv, and `just` installed:

```sh
just exe
```

This enters devenv and runs `scripts/package_windows.py`, using the locked
pkgs-mod input's `mkBuildEnvMingwCross` setup and Rust 1.95.0 with the
`x86_64-pc-windows-gnu` standard library. Release-only inputs are declared in
`dist/windows-package.nix`; target libraries are not added to the native shell.
To realize and check the toolchain without compiling Map:

```sh
devenv shell -- python3 scripts/package_windows.py --check
```

Cargo/CMake build artifacts are isolated in `dist/build/windows`. The script
builds a locked release, checks the resulting x86_64 PE executable for accidental
MinGW/geospatial DLL imports, and copies it to
`dist/build/devenv/release/bundle/windows/Map.exe`. The `windows_subsystem`
attribute in `src/main.rs` controls whether Windows release builds open a console;
comment it out to retain console logs while debugging. Packaging accepts both
GUI and console executables. Rust and C/C++ compiler
wrappers are disabled for this distribution build, as for AppImage builds.
Native build scripts retain their host compilers; target C/C++ compilation,
resource compilation, bindgen, pkg-config, and linking use MinGW inputs.

Every Windows build regenerates `dist/build/windows/map.ico` from `data/icon.png`
with 16, 32, 48, 64, 128, and 256-pixel images. MinGW's `windres` compiles
`dist/windows.rc` into a COFF resource object, and `cargo rustc` links it only
into the Map executable. Packaging rejects an executable without both icon
and icon-group resources. The `.ico` and resource object are build artifacts;
no separate icon file needs to be shipped beside `Map.exe`. The `--check`
preflight also generates and compiles these resources.

MapLibre continues to use **wgpu**, not EGL or OpenGL. Its checked Windows
source patch enables MinGW cross-compilation and avoids probing WGL for the
wgpu backend. All platforms use MapLibre's bundled ICU, not an external ICU
package. The bundled implementation uses MapLibre's basic number-formatting
fallback rather than external ICU's locale/currency formatting.
The static MinGW libraries include GDAL, PROJ, curl, and their dependencies;
Windows system libraries remain dynamic. No GDAL/PROJ data directory is copied.
The pkgs-mod pin includes the upstream fixes for PROJ's MinGW pkg-config
archive name and C++ runtime flags; no local PROJ metadata rewrite is needed.
The MinGW pthread archive is included for static linking. The final executable
selects static libstdc++ with whole-archive retention before other native
libraries, preventing later unqualified runtime requests from selecting DLL
import thunks. Unused sections are still garbage-collected. It also resolves
HDF5's zlib dependency after its consumers.
GPUI separately uses Windows' system `icuuc.dll` API for jump-list strings
(Windows 10 version 1703 or later); this is not an external MapLibre ICU build.
These distribution settings do not affect native development builds.

GPUI's pinned Windows crates normally require a Windows-hosted FXC compiler for
release shaders. The distribution applies `dist/gpui-windows-shaders.patch` and
`dist/gpui-pre-windows-shaders.patch` to the exact crate archives selected by
`Cargo.lock`, verified by their checksums.
A Cargo `paths` override selects this source only for the packaging command;
normal development builds and the dependency lockfile remain unchanged. Only
`gpui-fast-windows` and `gpui-pre-windows` have release debug assertions enabled
to select their runtime shader compilers. The patches compile **embedded** HLSL
with optimization, rather
than opening build-machine source paths. Windows must supply
`d3dcompiler_47.dll`; a Windows SDK/FXC installation is not required to build.
The same distribution patches disable GPUI's DXGI debug-interface probe and
Direct3D debug layers, independently of the shader compiler's debug-assertion
switch. End users do not need Windows Graphics Tools or the SDK debug components;
missing optional debugging support no longer logs `0x887A002D` at startup.
Ordinary development builds retain upstream debug-layer detection.

The executable is unsigned. Cross-build input checks and offline regression
tests are separate from a full Windows graphical launch/rendering test, which
must still be performed on a Windows machine.

## Shared icons

`data/icon.png` is the source icon and the only icon asset checked into the
repository. `just dmg` regenerates the icons configured in `Cargo.toml`:

- `dist/icon_512x512.png` (512×512)
- `dist/icon_512x512@2x.png` (1024×1024; the high-density variant for macOS)

These generated PNGs are ignored by Git. Generation uses Apple's `sips` on
macOS and ImageMagick's `convert` on Linux. To generate only these icons, run
`just generate-package-icons`. The AppImage packager instead generates its
256×256 desktop icon inside the AppDir with devenv's ImageMagick.

The macOS build also verifies that the app's `CFBundleIconFile` points to a
nonempty icon resource before signing and creating the DMG.
