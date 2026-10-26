# Map

A GPUI desktop geospatial viewer with a MapLibre Native vector basemap.

- Run: `devenv shell -- cargo run` (requires Nix and devenv).
- Open raster or vector files as command-line arguments, with the file picker, or by dropping them onto the map; press `?` for keyboard shortcuts.
- GDAL detects vector datasets (including vector PMTiles) and adds their layers to the layer selector. Vector layers support fit-to-layer and opacity; raster color maps and value ranges remain raster-only.
- Vector PMTiles is streamed directly by MapLibre Native: only visible tiles at the appropriate zoom are loaded. Opening reads layer metadata and archive bounds without converting features to GeoJSON.
- Other vector formats are converted to WGS84 GeoJSON in the background and loaded into memory; large non-tiled datasets can still require substantial memory.
- Hover a vector feature to inspect its properties. Native queries use a four-logical-pixel radius and only inspect the active layer; raster hover continues to show pixel values.
- Vector basemaps require internet access.


# Development
- Test: `devenv shell -- cargo test --locked`.
- Package: `just exe` (Windows), `just appimage` (Linux), or `just dmg` (macOS).
