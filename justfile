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

# Linux packaging still uses the mise/vcpkg toolchain.
mise_bin := env("MISE_BIN", "mise")


# Linux: build static dependencies and prepare MapLibre Native's locked source.
[linux]
package-dependencies:
    [ -z "${VCPKG_BINARY_CACHE:-}" ] || mkdir -p "$VCPKG_BINARY_CACHE"
    cargo vcpkg --verbose build --manifest-path dist/vcpkg/Cargo.toml --target "$(rustc -vV | sed -n 's/^host: //p')"
    test -f "$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET/lib/libgdal.a"
    test -f "$VCPKG_ROOT/installed/$VCPKG_DEFAULT_TRIPLET/lib/libproj.a"
    rm -rf dist/build/vcpkg/buildtrees dist/build/vcpkg/downloads dist/build/vcpkg/packages
    python3 scripts/prepare_maplibre.py dist/build/maplibre-native

# Use devenv's static overlay libraries, not host or mise/vcpkg dependencies.
[macos]
dmg:
    devenv shell -- python3 scripts/package_macos.py

[linux]
appimage:
    MISE_DEFAULT_CONFIG_FILENAME=dist/mise.linux.toml {{quote(mise_bin)}} exec -- just _appimage

# cargo-bundle 0.11 locates the binary via CARGO_TARGET_DIR (no --binary-path flag).
[linux]
_appimage:
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
