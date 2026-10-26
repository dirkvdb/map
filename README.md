# Map

A GPUI desktop geospatial viewer with a MapLibre Native vector basemap.

- Run: `devenv shell -- cargo run` (requires Nix and devenv).
- Open raster or vector files as command-line arguments, with the file picker, or by dropping them onto the map; press `?` for keyboard shortcuts.
- Press `Ctrl+\` (`Cmd+\` on macOS) to cycle appearance through Light, Dark, and Auto (follow system). A brief on-screen indicator shows the selected mode.
- GDAL detects vector datasets, including PMTiles and MBTiles, and adds their layers to the selector. Extents and CRS transformations are provided by the `geo` crate’s GDAL API. Vector layers support fit-to-layer and opacity; raster color maps and value ranges remain raster-only.
- Vector PMTiles and MBTiles are streamed directly by MapLibre Native: only visible tiles at the appropriate zoom are loaded. GDAL supplies their layer metadata and extents without requesting a feature scan. Raster MBTiles keep their existing raster path.
- The full GDAL derivation in `nix-pkgs` enables MBTiles, MVT, and PMTiles support. After publishing changes there, run `devenv update pkgs-mod` and use a refreshed `devenv shell` to pick up the updated package.
- Other vector formats are converted to WGS84 GeoJSON in the background and loaded into memory; large non-tiled datasets can still require substantial memory.
- Hover a vector feature to inspect its properties. Native queries use a four-logical-pixel radius and only inspect the active layer; raster hover continues to show pixel values.
- Vector basemaps require internet access.


# Development
- Test: `devenv shell -- cargo test --locked`.
- Package: `just exe` (Windows), `just appimage` (Linux), or `just dmg` (macOS).
