# Map

A GPUI desktop geospatial viewer with a MapLibre Native vector basemap.

- Open raster or vector files as command-line arguments, with the file picker, or by dropping them onto the map; press `?` for keyboard shortcuts.

# Development
- Test: `devenv shell -- cargo test --locked` (debug) and `devenv shell -- just test` (release-mode Nextest, also used by CI).
- Package: `just exe` (Windows), `just appimage` (Linux), or `just dmg` (macOS).
