# Map

A GPUI desktop raster viewer with a vector basemap rendered by
[`maplibre_native`](https://crates.io/crates/maplibre_native).

## Run

```sh
devenv shell -- cargo run
```

Logging uses `log` with `env_logger` and defaults to errors. Set `RUST_LOG` to
adjust verbosity, for example `RUST_LOG=map=debug devenv shell -- cargo run`.

Both GPUI and MapLibre use wgpu 29. The development shell supplies the native
build tools and libraries and sets `MLN_PRECOMPILE=0`: MapLibre 0.10 does not
publish a precompiled wgpu core for this release. The first build fetches the
pinned MapLibre Native source and its submodules into `.devenv/maplibre-native`,
then compiles its wgpu backend. `scripts/prepare_maplibre.py` derives the required
native revision from the locked crate and verifies the cached checkout.
This first build takes longer than subsequent cached builds.

The app uses [CARTO Dark Matter](https://basemaps.cartocdn.com/gl/dark-matter-gl-style/style.json),
the same dark vector basemap as the `maplibre-raster-layer` playground. Internet
access is required for the style, tiles, and fonts.

The initial view shows a world map with mouse and keyboard navigation.
Zooming keeps the geographic point under the pointer in place.

MapLibre keeps a native renderer on a dedicated thread and reuses the loaded
style and tiles between frames. GPUI displays each available frame immediately,
while tile loading and label refinement continue in the background. Navigation
does not wait for the map to become idle. Mouse and resize requests are coalesced
to the latest camera and viewport, and only the newest completed frame is queued
for display. Display-scale changes rebuild the native renderer.
Loading failures appear in the window with a Retry button.

The continuous MapLibre renderer produces a wgpu texture. The current display
bridge reads that texture through Rust's wgpu API and uploads its pixels through
GPUI's image API. Channel conversion runs on the GPU through a cached BGRA render
target. The readback buffer is reused at a fixed viewport size, and the CPU only
copies whole rows to remove GPU alignment padding. The renderer does not yet
share GPUI's device or queue.

## Raster sources

Use **Open…** (`Ctrl+O` / `Cmd+O`), drop files onto the map, or pass paths
at startup:

```sh
devenv shell -- cargo run -- /path/to/elevation.tif /path/to/other.vrt
```

GeoTIFF/COG, VRT, NetCDF, ASCII grids, and other GDAL-readable raster inputs are
tiled on demand. PNG/JPEG raster MBTiles can also be opened. Each numeric band
is selectable independently; the latest opened band becomes active and the map
fits its geographic extent. **Fit raster** (`Ctrl+0` / `Cmd+0`) returns to that
extent. **Clear rasters** removes the loaded sources without changing the basemap.

Controls float over the top-right of the full-window map. The Layers panel holds
the band and color-map selectors, legend, and opacity slider. The separate Value
range panel provides a two-thumb slider and native numeric editors. Changes apply
immediately while dragging or editing a valid range; invalid edits keep the last
valid range on the map.
**Reset** restores the band's data range. Collapse the controls with the header
button or `Ctrl+L` / `Cmd+L` without changing the map viewport.
Native controls follow the system light/dark appearance, including changes while
the app is open; the Dark Matter basemap stays dark.

Change opacity to compare the raster with the basemap. The legend reflects the applied
range; nodata is transparent. Initial ranges use GDAL's approximate band
statistics. Already-colored MBTiles retain their original colors.

Move the pointer over the map to see latitude/longitude and the active band's
pixel value at the bottom-left. Values are read from numeric tiles at the band's
native maximum zoom, independently of its palette, display range, and opacity.
No data is shown for nodata/outside the raster; image-only layers have no numeric
value. Cached values update immediately without background scheduling. Background
reads prefetch neighboring tiles into a bounded cache and prioritize the latest
cursor position when an uncached tile is needed. The previous value stays visible
until the new sample arrives; changing bands clears it.

The app reuses the `infra-rs` tiler from the Tauri proof of concept. MapLibre
Native requests `raster://` tiles through an in-process custom file source. Two
background workers read/reproject only the requested regions and return
color-mapped PNGs to a standard MapLibre raster source/layer, with nearest-neighbor
sampling. Web-tile-aligned COGs use their native tile size; other rasters use
256-pixel XYZ tiles, scaled for display DPI. No tile pyramid, HTTP server, or
Tauri runtime is needed. Palette/range changes use new URL identities, while
camera changes and opacity updates retain the native renderer and tile cache.

Internet access is still needed for the vector basemap. The local-raster
rendering smoke test below uses an offline background style instead.

## Native geospatial dependencies

The development shell uses the `VITO-RMA/nix-pkgs` input with its static overlay,
matching `infra-rs`. It provides GDAL, PROJ, and OpenSSL for static linking, along
with their headers and pkg-config metadata. Packages are fetched from the
`geo-overlay` Cachix cache when available.

The shell puts a static libcurl configured with Nix's CA bundle ahead of the
copy propagated by GDAL. MapLibre and GDAL therefore retain HTTPS certificate
verification when linked together; no certificate-verification bypass or manual
SSL environment variables are required.

To inspect the available GDAL version and static linker flags:

```sh
devenv shell -- pkg-config --modversion gdal
devenv shell -- pkg-config --static --libs gdal
```

These dependencies do not make the entire GPUI application a fully static binary.

## Verify

```sh
devenv shell -- cargo fmt --check
devenv shell -- cargo clippy --all-targets --locked -- -D warnings
devenv shell -- cargo test --locked
devenv shell -- cargo test local_raster_tiles_render_restyle_and_clear_without_network -- --ignored --nocapture
devenv shell -- cargo test vector_basemap_renders_geographic_content -- --ignored --nocapture
devenv shell -- cargo test retained_renderer_updates_camera_and_viewport -- --ignored --nocapture
devenv shell -- cargo test streaming_worker_delivers_frames_and_accepts_new_viewports -- --ignored --nocapture
RUST_LOG=map=info devenv shell -- cargo test --release navigation_frame_timings -- --ignored --nocapture
```

The opt-in rendering smoke test requires internet access and a working native
graphics device. It checks image dimensions, opacity, and that the vector map
contains more than a flat background.
