# Map

A GPUI desktop raster viewer with a MapLibre Native vector basemap.

- Run: `devenv shell -- cargo run` (requires Nix and devenv).
- Open raster files as command-line arguments; press `?` for keyboard shortcuts.
- Vector basemaps require internet access.


# Development
- Test: `devenv shell -- cargo test --locked`.
- Package: `just exe` (Windows), `just appimage` (Linux), or `just dmg` (macOS).
