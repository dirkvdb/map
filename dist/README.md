# Packaging

The distribution workflow produces native macOS and Linux packages:

- macOS: `dist/build/devenv/release/bundle/osx/Map.app`
- macOS: `dist/build/devenv/release/bundle/dmg/Map.dmg`
- Linux: `dist/build/release/bundle/appimage/*.AppImage`

macOS packaging uses devenv and the static `pkg-mod-*` dependencies from the
geo overlay, without mise or vcpkg. Linux packaging retains the pinned
mise/cargo-vcpkg toolchain. Both build MapLibre Native from its locked source
revision. A first package build can take significantly longer than incremental
builds.
GitHub Actions retains its cache-backed sccache service for Rust and native
C/C++ compilation. macOS also reads the `geo-overlay` Cachix cache without
pushing to it; Linux persists vcpkg binary packages with `actions/cache`.

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

The Linux toolchain is pinned with mise. `cargo-vcpkg` builds static GDAL,
PROJ, and their native dependencies using `dist/vcpkg/Cargo.toml` and the static
triplets in `dist/vcpkg/triplets/`. Intermediate vcpkg build trees and downloads
are removed before the Rust build to fit GitHub-hosted runner disk limits;
installed libraries are kept. The vcpkg GDAL and `PKG_CONFIG_PATH` settings
apply only while compiling Map, allowing host `pkg-config` to discover
MapLibre's system WebP, libuv, and ICU dependencies and mise to install Cargo
tools with the host's OpenSSL. The application still uses dynamic system
runtime and UI libraries.

Linux CI caches vcpkg binary packages with OS/architecture keys derived from
the vcpkg manifest, triplets, and overlay ports. Cache misses compile normally
and populate the cache for later runs. Local Linux builds do not require
sccache. `MISE_BIN` can select an absolute mise executable.

Install mise and the native packages listed in `.github/workflows/dist.yml`,
then run:

```sh
mise trust dist/mise.linux.toml
MISE_DEFAULT_CONFIG_FILENAME=dist/mise.linux.toml mise install
just appimage
```

`just appimage` activates the pinned Linux mise environment automatically.
The AppImage is unsigned. GitHub's artifact ZIP may remove its executable bit;
restore it with `chmod +x <file>.AppImage` after downloading.

Pushing a semantic version tag such as `v0.1.0` builds both packages and creates
a GitHub release with the resulting files. Branch pushes, pull requests, and
manual runs build artifacts without publishing a release.

## Shared icons

`data/icon.png` is the source icon and the only icon asset checked into the
repository. Both `just dmg` and `just appimage` regenerate the icons configured
in `Cargo.toml` before building:

- `dist/icon_512x512.png` (512×512)
- `dist/icon_512x512@2x.png` (1024×1024; the high-density variant for macOS)

These generated PNGs are ignored by Git. Generation uses Apple's `sips` on
macOS and ImageMagick's `convert` on Linux (provided by the `imagemagick`
package listed in the workflow). To generate only the icons, run
`just generate-package-icons`.

The macOS build also verifies that the app's `CFBundleIconFile` points to a
nonempty icon resource before signing and creating the DMG.
