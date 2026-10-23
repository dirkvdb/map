build_debug:
    cargo build

build_release:
    cargo build --release

build: build_release

test_debug test_name='' $RUST_LOG="debug":
    cargo nextest run --workspace --no-capture {{ test_name }}

test_release test_name='':
    cargo nextest run --workspace --release {{ test_name }}

test: test_release

run args='':
    cargo run --release -- {{ args }}

[windows]
set shell := ["pwsh", "-NoLogo", "-NoProfile", "-Command"]

# mise must be installed outside devenv; MISE_BIN can select an absolute executable.
mise_bin := env("MISE_BIN", "mise")
macos_path := env("HOME", env("USERPROFILE", "")) + "/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

# Install the pinned native packaging tools without inheriting devenv's environment.
setup-macos:
    /usr/bin/env -i HOME={{quote(env("HOME"))}} PATH={{quote(macos_path)}} MISE_DEFAULT_CONFIG_FILENAME=dist/mise.toml {{quote(mise_bin)}} install

# Build static GDAL/PROJ dependencies and prepare MapLibre Native's locked source.
package-dependencies:
    [ -z "${VCPKG_BINARY_CACHE:-}" ] || mkdir -p "$VCPKG_BINARY_CACHE"
    cargo vcpkg --verbose build --manifest-path dist/vcpkg/Cargo.toml --target "$(rustc -vV | sed -n 's/^host: //p')"
    test -f "$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET/lib/libgdal.a"
    test -f "$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET/lib/libproj.a"
    rm -rf dist/build/vcpkg/buildtrees dist/build/vcpkg/downloads dist/build/vcpkg/packages
    python3 scripts/prepare_maplibre.py dist/build/maplibre-native

# Build with mise-managed Rust and Apple's tools, using separate Cargo artifacts.
dmg:
    just generate-package-icons
    just package-dependencies
    GDAL_STATIC=1 GDAL_HOME="$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET" PKG_CONFIG_PATH="$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET/lib/pkgconfig" cargo build --release --locked --bin map
    cargo bundle --release --format osx --binary-path dist/build/release/map
    python3 -c 'import pathlib, plistlib; app = pathlib.Path("dist/build/release/bundle/osx/Map.app/Contents"); icon = plistlib.loads((app / "Info.plist").read_bytes())["CFBundleIconFile"]; path = app / "Resources" / icon; path = path if path.suffix else path.with_suffix(".icns"); assert path.stat().st_size > 0, "App bundle icon is empty"'
    /usr/bin/codesign --force --sign - dist/build/release/bundle/osx/Map.app
    /usr/bin/codesign --verify --deep --strict --verbose=2 dist/build/release/bundle/osx/Map.app
    /bin/ln -sfn /Applications dist/build/release/bundle/osx/Applications
    /bin/mkdir -p dist/build/release/bundle/dmg
    /usr/bin/hdiutil create -ov -volname Map -srcfolder dist/build/release/bundle/osx -format UDZO dist/build/release/bundle/dmg/Map.dmg
    /usr/bin/hdiutil verify dist/build/release/bundle/dmg/Map.dmg

# Linux: select dist/mise.linux.toml via MISE_DEFAULT_CONFIG_FILENAME.
# cargo-bundle 0.11 locates the binary via CARGO_TARGET_DIR (no --binary-path flag).
appimage:
    just generate-package-icons
    just package-dependencies
    GDAL_STATIC=1 GDAL_HOME="$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET" PKG_CONFIG_PATH="$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET/lib/pkgconfig" cargo build --release --locked --bin map
    CARGO_BUNDLE_SKIP_BUILD=1 cargo bundle --release --format appimage

# Regenerate bundle icons before every package build so they match the source.
[macos]
generate-package-icons:
    sips -z 512 512 data/icon.png --out dist/icon_512x512.png
    sips -z 1024 1024 data/icon.png --out dist/icon_512x512@2x.png

[linux]
generate-package-icons:
    convert data/icon.png -resize '512x512!' dist/icon_512x512.png
    convert data/icon.png -resize '1024x1024!' dist/icon_512x512@2x.png

# Refresh bundled themes from the source revision of gpui-kit in Cargo.lock.
sync-themes:
    python3 scripts/sync_gpui.py themes

# Replace the copied GPUI agent skills with those from the locked release.
sync-skills:
    python3 scripts/sync_gpui.py skills

# Refresh both themes and agent skills from a single locked source download.
sync-gpui:
    python3 scripts/sync_gpui.py all
