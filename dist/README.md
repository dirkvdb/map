# Packaging

Release packaging follows TopQ's native bundle workflow, without Flatpak. Map's
MapLibre Native backend currently supports release builds on Linux and macOS, so
the workflow produces:

- `dist/build/release/bundle/osx/Map.app`
- `dist/build/release/bundle/dmg/Map.dmg`
- `dist/build/release/bundle/appimage/*.AppImage`

The toolchain is pinned with mise. `cargo-vcpkg` builds static GDAL, PROJ, and
their native dependencies; MapLibre Native is built from its locked source
revision. This makes the first package build substantially longer than TopQ's.
The standalone `dist/vcpkg/Cargo.toml` manifest keeps this dependency set
deterministic and normalizes metadata inherited from the `infra-rs` submodule.
Project-owned triplets under `dist/vcpkg/triplets/` explicitly set
`VCPKG_LIBRARY_LINKAGE` to `static`; the resulting application still uses the
platform's dynamic system runtime and UI libraries.
Intermediate vcpkg build trees and downloads are removed before the Rust build
to stay within GitHub-hosted runner disk limits; installed libraries are kept.
The vcpkg GDAL and `PKG_CONFIG_PATH` values are applied only while compiling
Map. The host `pkg-config` can therefore discover both vcpkg's static geospatial
libraries and MapLibre's system WebP, libuv, and ICU dependencies, while mise
can install its Cargo tools using the host's OpenSSL.
GitHub Actions uses its cache-backed sccache service for Rust, vcpkg C/C++, and
the MapLibre Native source build. Local builds do not require sccache.
Following infra-rs, vcpkg also writes compiled binary packages to a dedicated
cache. The macOS and Linux workflows persist that directory with `actions/cache`,
using separate OS/architecture keys derived from the vcpkg manifest, static
triplets, and overlay ports. Cache misses compile normally and populate the
cache for later runs.

## Local macOS build

Install mise and Apple's Xcode or Command Line Tools, including the Metal
toolchain, then run from the repository root outside devenv:

```sh
mise trust dist/mise.toml
MISE_DEFAULT_CONFIG_FILENAME=dist/mise.toml mise install
MISE_DEFAULT_CONFIG_FILENAME=dist/mise.toml mise exec -- just dmg
```

The app is ad-hoc signed but not Developer ID-signed or notarized.

## Local Linux build

Install the native packages listed in `.github/workflows/dist.yml`, then run:

```sh
mise trust dist/mise.linux.toml
MISE_DEFAULT_CONFIG_FILENAME=dist/mise.linux.toml mise install
MISE_DEFAULT_CONFIG_FILENAME=dist/mise.linux.toml mise exec -- just appimage
```

The AppImage is unsigned. GitHub's artifact ZIP may remove its executable bit;
restore it with `chmod +x <file>.AppImage` after downloading.

Pushing a semantic version tag such as `v0.1.0` builds both packages and creates
a GitHub release with the resulting files. Branch pushes, pull requests, and
manual runs build artifacts without publishing a release.

## Shared icons

Both macOS and Linux packages use the prepared icons in `dist/`, configured in
`Cargo.toml`. These are scaled from `data/icon.png` to 512×512 and 1024×1024
pixels; the `@2x` filename identifies the high-density variant for macOS.

After changing the source icon, regenerate both PNGs on macOS:

```sh
sips -z 512 512 data/icon.png --out dist/icon_512x512.png
sips -z 1024 1024 data/icon.png --out dist/icon_512x512@2x.png
```
