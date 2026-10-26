# Map

A GPUI desktop geospatial viewer with a MapLibre Native vector basemap.

- Run: `devenv shell -- cargo run` (requires Nix and devenv).
- Open raster or vector files as command-line arguments, with the file picker, or by dropping them onto the map; press `?` for keyboard shortcuts.
- Raster scale/offset metadata is applied in the shader (`physical = stored × scale + offset`), while numeric tile caches retain raw values. For layers declaring scale or offset metadata, the value-range pane shows an **Unscale values** checkbox (on by default). Its per-layer setting switches rendering, hover values, and ranges between physical and raw units without reloading tiles; nodata remains excluded. This includes NetCDF `scale_factor`/`add_offset` and GeoTIFF band scaling.
- The title-bar layer selector fits the widest loaded layer name and adapts to font zoom and available window space.
- NetCDF files automatically add each georeferenced 2D grid variable to the raster selector. Extra dimensions (such as time or level) become separately selectable layers, labeled with the variable and dimension values; coordinate and scalar variables are not added. Files with a variable-level `GeoTransform` attribute are supported without modifying the source file.
- Press `Ctrl+\` (`Cmd+\` on macOS) to cycle appearance through Light, Dark, and Auto (follow system). A brief on-screen indicator shows the selected mode.
- GDAL detects vector datasets, including PMTiles and MBTiles, and adds their layers to the selector. Extents and CRS transformations are provided by the `geo` crate’s GDAL API. Vector layers support fit-to-layer and opacity; raster color maps and value ranges remain raster-only.
- Vector PMTiles and MBTiles are streamed directly by MapLibre Native: only visible tiles at the appropriate zoom are loaded. GDAL supplies their layer metadata and extents without requesting a feature scan. Raster MBTiles keep their existing raster path.
- The full GDAL derivation in `nix-pkgs` enables MBTiles, MVT, and PMTiles support. After publishing changes there, run `devenv update pkgs-mod` and use a refreshed `devenv shell` to pick up the updated package.
- Other vector formats are converted to WGS84 GeoJSON in the background and loaded into memory; large non-tiled datasets can still require substantial memory.
- Hover a vector feature to inspect its properties. Native queries use a four-logical-pixel radius and only inspect the active layer; raster hover continues to show pixel values.
- Vector basemaps require internet access.


# Development
- Test: `devenv shell -- cargo test --locked`.
- Test a local NetCDF file: `MAP_TEST_NETCDF=/path/to/file.nc devenv shell -- cargo test --locked netcdf_local_file -- --ignored --nocapture`.
- Package: `just exe` (Windows), `just appimage` (Linux), or `just dmg` (macOS).
