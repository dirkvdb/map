# Map

A minimal GPUI desktop app with a vector basemap rendered by
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

The app loads the public [MapLibre demo style](https://demotiles.maplibre.org/),
which uses vector tiles and Natural Earth data and needs no API key. Internet
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

The demo dataset contains country-level features; zooming in does not add
street-level detail.

## Verify

```sh
devenv shell -- cargo fmt --check
devenv shell -- cargo clippy --all-targets --locked -- -D warnings
devenv shell -- cargo test --locked
devenv shell -- cargo test vector_basemap_renders_geographic_content -- --ignored --nocapture
devenv shell -- cargo test retained_renderer_updates_camera_and_viewport -- --ignored --nocapture
devenv shell -- cargo test streaming_worker_delivers_frames_and_accepts_new_viewports -- --ignored --nocapture
RUST_LOG=map=info devenv shell -- cargo test --release navigation_frame_timings -- --ignored --nocapture
```

The opt-in rendering smoke test requires internet access and a working native
graphics device. It checks image dimensions, opacity, and that the vector map
contains more than a flat background.
