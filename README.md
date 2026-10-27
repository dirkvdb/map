# Map

A GPUI desktop geospatial viewer with a MapLibre Native vector basemap.

- Run: `devenv shell -- cargo run` (requires Nix and devenv).
- Open raster or vector files as command-line arguments, with the file picker, or by dropping them onto the map; press `?` for keyboard shortcuts.
- Raster scale/offset metadata is applied in the shader (`physical = stored × scale + offset`), while numeric tile caches retain raw values. For layers declaring scale or offset metadata, the value-range pane shows an **Descale values** checkbox (on by default). Its per-layer setting switches rendering, hover values, and ranges between physical and raw units without reloading tiles; nodata remains excluded. This includes NetCDF `scale_factor`/`add_offset` and GeoTIFF band scaling.
- The title-bar layer selector fits the widest loaded layer name and adapts to font zoom and available window space.
- NetCDF files automatically add each georeferenced 2D grid variable to the raster selector. Extra dimensions (such as time or level) become separately selectable layers, labeled with the variable and dimension values; coordinate and scalar variables are not added. Files with a variable-level `GeoTransform` attribute are supported without modifying the source file.
- Press `Ctrl+\` (`Cmd+\` on macOS) to cycle appearance through Light, Dark, and Auto (follow system). A brief on-screen indicator shows the selected mode.
- GDAL detects vector datasets, including PMTiles and MBTiles, and adds their layers to the selector. Extents and CRS transformations are provided by the `geo` crate’s GDAL API. Vector layers support fit-to-layer and opacity. Layers with numeric properties also show a searchable **Color by** combobox, color-map selection, legend, and editable value range. Integer and floating-point values color points, polygon fills, outlines, and lines; missing, null, and non-numeric values keep the theme’s uniform color. Property, palette, and range choices stay with each layer; changing them does not reload the data or move the camera.
- Vector PMTiles and MBTiles are streamed directly by MapLibre Native: only visible tiles at the appropriate zoom are loaded. GDAL supplies their layer metadata and extents without requesting a feature scan. Raster MBTiles keep their existing raster path. Numeric tiled attributes come from GDAL’s schema; their range defaults to 0–1 (editable) to avoid scanning tiles. Materialized datasets use each property’s finite minimum/maximum, expanding constant-value ranges automatically.
- The full GDAL derivation in `nix-pkgs` enables MBTiles, MVT, and PMTiles support. After publishing changes there, run `devenv update pkgs-mod` and use a refreshed `devenv shell` to pick up the updated package.
- GeoPackage, GeoJSON, shapefile, and other GDAL vector formats are converted to EPSG:4326 (WGS84, longitude/latitude) GeoJSON in the background, preserving feature properties. Conversion uses GDAL’s in-memory filesystem, without temporary files on disk; the resulting shared GeoJSON string is parsed directly by MapLibre. A thin progress bar below the titlebar and a quiet phase label show opening, GDAL conversion, data preparation, and rendering without blocking map navigation. Percentages describe conversion only; preparation and rendering are indeterminate. The indicator stays visible until a matching, settled map frame is applied, rather than disappearing at GDAL’s 100% callback. Every dataset layer is added to the selector. Spatial layers without a declared CRS are rejected rather than guessed; large non-tiled datasets can require substantial memory.
- CSV files with `lat` or `latitude` and `lon`, `long`, or `longitude` columns (case-insensitive) open as point layers. Coordinates are assumed to be WGS84 unless a CRS is declared. GDAL handles quoted fields and delimiter detection; all columns, including the coordinates, remain feature properties. Untyped attribute columns stay strings to preserve identifiers such as `00123`; a `.csvt` sidecar can declare numeric and other field types. Missing or invalid coordinates, ambiguous coordinate headers, and conflicting geometry definitions produce a loading error. Keep a shapefile’s `.prj`, `.dbf`, and other sidecars alongside its `.shp` file.
- Hover a vector feature to inspect its properties in the **Properties** pane at the top left of the map. The pane stays visible while a vector layer is active, even between hits, so its header can always collapse or expand it. Coordinates remain in the lower-left overlay. Native queries only inspect the active layer. Point-only layers use four logical pixels of extra hit tolerance; polygon, line, mixed, and unknown-geometry layers use exact queries to avoid neighboring hits. Raster hover continues to show pixel values.
- Vector basemaps require internet access.


# Development
- Test: `devenv shell -- cargo test --locked` (debug) and `devenv shell -- just test` (release-mode Nextest, also used by CI).
- Test a local NetCDF file: `MAP_TEST_NETCDF=/path/to/file.nc devenv shell -- cargo test --locked netcdf_local_file -- --ignored --nocapture`.
- Package: `just exe` (Windows), `just appimage` (Linux), or `just dmg` (macOS).

## Complexity checks

Run `devenv shell -- just check` after Rust changes, followed by `cargo test --workspace --all-features` and `just test` in the development shell. The devenv `ci:checks` task also runs this gate before building/testing. The individual checks are:

| Command | Check |
| --- | --- |
| `just fmt` | `cargo fmt --all -- --check` |
| `just lint` | Workspace Clippy, all targets/features, `--no-deps`, with the warning/complexity errors required by `AGENTS.md` |
| `just complexity` | `devenv tasks run code:complexity`, using its configured Knots version and thresholds unchanged |

`Cargo.toml` enables `clippy::cognitive_complexity`, `clippy::too_many_lines`, and `clippy::excessive_nesting` at **forbid** level. Their thresholds in `clippy.toml` are **15**, **80**, and **4**, respectively. These are Clippy's own metrics: function length excludes blank/comment-only lines, while nesting includes enclosing inline modules/impl blocks. `forbid` rejects source-level `allow`/`expect` overrides and is compatible with the required command-line `-D` flags. The lint command also denies all warnings and `clippy::type_complexity`, which uses the existing threshold of **200**.

Clippy does not provide cyclomatic complexity. The existing `scripts.knots` command in `devenv.nix` pins **Knots 2.0.0** through `uvx`; `code:complexity` enforces per-function **cognitive ≤ 15**, **McCabe ≤ 10**, **nesting ≤ 4**, and **AIRD ≤ 85**. AIRD is **AI Reasoning Difficulty** (lower is better), not a minimum readiness score. No baseline is currently passed to this task: a clean gate requires zero over-threshold functions. Do not introduce suppressions, broad exclusions, a baseline, or threshold overrides to hide violations.

Knots scans application source under `src/`, including test modules and platform-specific source, without building Rust/native dependencies. It does not traverse `vendor/`, the `infra-rs/` submodule, packaging/tooling scripts, or build output under `target/` and `.devenv/`. Generated `src/bundled_themes.rs` contains only declarations and contributes no functions; no handwritten functions are excluded. Clippy is scoped to this application's workspace, not its dependencies. Source analyzers and Clippy use different metrics, so neither substitutes for the other.

The development shell supplies `uv`/`uvx`. Its first Knots invocation may download the pinned tool; analysis runs locally without uploading source and adds no Rust runtime dependency. For a machine-readable, report-only snapshot, run `devenv shell -- knots -r src/ --format json --completion-report .devenv/knots-completion.json > .devenv/knots-metrics.json`. This exports metrics and scan completeness; always run `just complexity` separately for the configured threshold gate.

## Vector loading diagnostics

Enable `RUST_LOG=map::vector_load=info` when running the application to trace file drop/open, background queueing, GDAL conversion and progress milestones, reopening the converted GeoJSON for bounds, buffer copying, Rust JSON parsing/metadata/cleanup, UI handoff, native parsing/source copying, frame readback, and native idle. Logs include layer/source identities and elapsed times; they do not log feature-property values. A frame's requested source ID is not proof that its geometry is visible.

Set `MAP_TEST_VECTOR` to a local spatial vector file and run these opt-in measurements in release mode:

```sh
RUST_LOG=map::vector_load=info devenv shell -- cargo test --release --locked local_vector_drop_to_display_timings -- --ignored --nocapture
RUST_LOG=map::vector_load=info devenv shell -- cargo test --release --locked local_vector_load_frame_timings -- --ignored --nocapture
```

The first dispatches a real file-drop event through the production view and loader, uses the real renderer worker with a local background-only style, and verifies that the image drawn by headless GPUI contains vector pixels. The deterministic test pumps the frame channel on the test thread, so this is not a measurement of production foreground scheduling or physical monitor presentation. The second isolates native installation, first data-bearing readback, and idle, excluding renderer warmup and network basemap traffic. Neither test asserts a machine-dependent timing threshold.

For the 9,195-feature Statbel statistical-sectors GeoPackage, the 55.4 MB input expanded to 102.5 MB of GeoJSON. One release-mode drop measurement took 5.61 s: 3.05 s GDAL conversion, 0.57 s reopening GeoJSON for bounds, 0.05 s buffer copying, 0.45 s Rust JSON parsing, 0.09 s Rust JSON cleanup/metadata, 0.86 s native installation, and about 0.54 s until a data-bearing frame was drawn. These are local measurements, not general timing guarantees.

On GDAL 3.12.4, that file uses the default Arrow batch conversion path and emits only a terminal 100% progress callback. Production loads now scope `OGR2OGR_USE_ARROW_API=NO` to the conversion thread to obtain per-feature callbacks, restoring the previous thread-local value on success, failure, or unwind. The opt-in Arrow comparison in `src/map_renderer_vector_timing_tests.rs` retains the default path for diagnostics and confirms byte-identical output for this file. This is not a general performance guarantee; drivers without fast feature counts may still provide no intermediate conversion progress.

Progress updates remain coalesced and throttled to avoid a UI event per feature. Conversion completion switches to indeterminate preparation before GeoJSON finalization, extent extraction, and metadata parsing. The rendering phase includes native parsing/source installation and remains visible until native resources, render work, and fades settle and the corresponding image is applied. An idle-only completion republishes the latest shared image, without copying pixels or reading the GPU again. Selection changes, clearing, and errors dismiss the relevant rendering progress; stale source frames cannot finish a newer load.

A release-mode rerun with the phased loader reported conversion milestones at roughly 0.84 s (25%), 1.61 s (50%), 2.27 s (75%), and 2.98 s (100%). Loading finished at 4.17 s, the first data-bearing headless frame appeared at 5.52 s, and the settled frame dismissed progress at 6.22 s. Settlement is deliberately conservative and includes fades/resource work; it does not assert that every dataset must have visible pixels (empty or off-screen datasets can also be ready). The same warm-renderer, local-basemap, and headless scheduling limitations apply.
