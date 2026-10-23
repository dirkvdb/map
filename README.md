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
During continuous range/palette edits, completed frames for the active band are
displayed even when a newer style is already requested. Intermediate requests
are coalesced, not used to hide every in-flight frame until dragging stops;
frames from a different band or viewport are still rejected.
Numeric band changes retain the custom layer and use two shader inputs. The
outgoing band's cached tiles stay visible while the incoming band loads, then
their independently styled colors crossfade over 180 ms. Nodata masks are blended
in premultiplied alpha, without a basemap flash between opaque pixels. Rapid
selections replace the loading slot, or queue the latest band until an active
fade finishes. Reduced motion swaps immediately once the new tiles are ready.
Initial loads and ordinary navigation still display progressively.

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
is selectable independently; the first band of the latest opened raster becomes
active and the map fits its geographic extent. Switching bands preserves the
current pan and zoom. `Ctrl+0` / `Cmd+0` explicitly fits the active band's extent.
Scroll over the band selector to move to the next or previous band without
opening the dropdown or moving the camera. Selection stops at either end.

Controls float over the top-right of the full-window map. The Layers panel holds
the band and color-map selectors, legend, and opacity slider. The separate Value
range panel provides a two-thumb slider and native numeric editors. Changes apply
immediately while dragging or editing a valid range; invalid edits keep the last
valid range on the map.
**Reset** restores the band's data range. Collapse the controls with the header
button or `Ctrl+L` / `Cmd+L` without changing the map viewport.
Press `Ctrl+,` / `Cmd+,` to open Settings and choose System, Light, or Dark appearance
and separate light and dark themes, using the same bundled themes as TopQ.
Changes apply immediately and are saved in the platform configuration directory
(`$XDG_CONFIG_HOME/map/config.json`, or `~/.config/map/config.json` on Linux).
Like TopQ, saves merge into that JSON object and preserve unrelated settings.
Read-only configuration (including Nix-managed symlinks) uses a writable override
at `$XDG_STATE_HOME/map/config.json`, or `~/.local/state/map/config.json` when unset.
An existing state override takes precedence for both reads and writes. Files are
replaced atomically; malformed configuration is never overwritten.
System mode follows appearance changes while the app is open. The Dark Matter
basemap stays dark. Escape closes Settings and returns focus to the previous control.

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
Native renders numeric rasters through a custom WebGPU layer, inside its own
render pass and style-layer ordering. Two background workers read/reproject
only requested regions and return raw numeric tiles. The layer caches values
in `R32Float` GPU textures with separate nodata masks; a WGSL shader applies the
same 256-color palette as the legend, display range, and opacity. Range/opacity
updates change uniforms, while palette updates upload only the small palette
texture. **Restyling does not read, request, encode, or re-upload numeric tiles.**

Web-tile-aligned COGs use their native tile size; other rasters use 256-pixel XYZ
tiles, scaled for display DPI. The layer selects tiles for the application's
north-up, unpitched camera and projects them with MapLibre's own matrix. It
supports wrapped world copies and cached parent-tile fallback during zooming.
Queued work for obsolete viewports/bands is cancelled. The cache evicts unused
tiles above 256 entries or 128 MiB per slot, keeping the visible working set even
if it exceeds that budget. During a band transition, the outgoing cache is
retained until the fade finishes, then released. GPU display values use float32
precision; the cursor-value inspector still retains the original numeric types
at native maximum zoom.
No tile pyramid, HTTP server, or Tauri runtime is needed.

Image-only PNG/JPEG MBTiles continue through the in-process `raster://` file
source and a standard MapLibre raster layer, retaining their original colors.

### Custom-layer integration

The pinned upstream versions do not expose a Rust WebGPU custom-layer API.
Local Cargo patches in `vendor/` extend `maplibre_native` and `webgpu-shim` with
a borrowed render-pass callback. A small, checked native-source patch enables
the WebGPU custom-layer factory and supplies the active encoder, attachment
formats, and layer-adjusted projection. It is applied idempotently during the
native source build; conflicting checkout changes fail rather than being
overwritten. See `vendor/README.md` for provenance and the extension points.

Raster rendering does not depend on the CPU display bridge: MapLibre's output
texture already contains the raster. Future direct GPUI/wgpu texture integration
can therefore remove readback without changing the custom layer.

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
devenv shell -- cargo test custom_raster_respects_native_layer_order_and_band_changes -- --ignored --nocapture
devenv shell -- cargo test byte_values_render_opaque_including_173_and_174 -- --ignored --nocapture
devenv shell -- cargo test landuse_water_values_render_opaque_over_the_basemap -- --ignored --nocapture
devenv shell -- cargo test vector_basemap_renders_geographic_content -- --ignored --nocapture
devenv shell -- cargo test retained_renderer_updates_camera_and_viewport -- --ignored --nocapture
devenv shell -- cargo test streaming_worker_delivers_frames_and_accepts_new_viewports -- --ignored --nocapture
RUST_LOG=map=info devenv shell -- cargo test --release navigation_frame_timings -- --ignored --nocapture
devenv shell -- cargo test --release raster_style_frame_timings -- --ignored --nocapture
```

The opt-in rendering smoke test requires internet access and a working native
graphics device. It checks image dimensions, opacity, and that the vector map
contains more than a flat background.

The synthetic local-raster GPU tests are offline. They check nodata/footprint alignment,
band switching, native layer ordering, palette/range updates with unchanged tile
request/upload counts (even after moving the source file), opacity, and clearing.
An additional test uses the land-use fixture and vector basemap to verify that
valid water classes 173/174 are not hidden by the basemap's depth buffer.
The headless UI integration tests also cover continuous slider dragging while
older-style frames arrive, including rejection of frames from another band.
The offline range-update timing test reports style construction, rendering, and
GPU wait/readback separately at a 2400×1520 physical viewport.
