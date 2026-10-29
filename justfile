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


# Use devenv's static overlay libraries, not host or mise/vcpkg dependencies.
[macos]
dmg:
    devenv shell -- python3 scripts/package_macos.py

[linux]
appimage:
    devenv shell -- python3 scripts/package_linux.py

# Cross-compile the Windows distribution with pkgs-mod's static MinGW libraries.
[linux]
exe:
    devenv shell -- python3 scripts/package_windows.py

# Regenerate bundle icons before every package build so they match the source.
[macos]
generate-package-icons:
    /usr/bin/sips --js scripts/macos_icons.js data/icon-iOS-Default-1024@1x.png --out dist

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

# Application policy checks; dependencies/submodules are not lint targets.
check: fmt complexity lint

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --locked --workspace --all-targets --all-features --no-deps -- -D warnings -D clippy::cognitive_complexity -D clippy::too_many_lines -D clippy::type_complexity

# Use the version and thresholds configured in devenv.nix, without overrides.
complexity:
    devenv tasks run code:complexity
