# maplibre-custom-layer

Connects Rust renderers to MapLibre Native's custom-layer lifecycle and exposes
native graphics and camera parameters. It lets application-rendered content join
MapLibre's layer stack with correct map alignment and layer ordering, rather than
being drawn as a separate overlay.

An isolated, generic `mln::style::CustomLayerHost` bridge. No wgpu dependency,
compositor, official-binding source copy, root Cargo change, or async runtime.
The application supplies its renderer/compositor as `NativeCustomLayer`. A small,
separate core patch adds aligned raster registration and exact flat-layer depth.

## Integration contract

- Rust API: `src/lib.rs`, graphics/camera records: `src/parameters.rs`.
- C ABI v2 (frame layout changed; rebuild the compositor): `native/include/maplibre_custom_layer.h`.
- Native implementation: `native/custom_layer.cpp`.
- Build hook: `cmake/inject.cmake`.
- Required core patch: `patches/0001-custom-layer-raster-parameters.patch`.
  Native patch marker: `MLN_CUSTOM_LAYER_RASTER_PARAMETERS_VERSION == 1`.
- Official Git dependency: `https://github.com/maplibre/maplibre-native-ffi`,
  revision `d47a1fb38533026ad6b6658511375c32ecb8c756`.
- Select **one** backend in the final application: feature `vulkan` or `metal`.
  Do not use `--all-features`: upstream explicitly rejects multiple backends.

```rust
pub trait NativeCustomLayer: Send + 'static {
    fn initialize(&mut self, graphics: &GraphicsContext) -> Result<()>; // default Ok
    fn pre_render(&mut self, parameters: &FrameParameters) -> Result<()>; // default Ok
    fn render(&mut self, parameters: &FrameParameters) -> Result<()>;
    fn context_lost(&mut self) -> Result<()>; // default Ok
    fn deinitialize(&mut self) -> Result<()>; // default Ok
}

pub fn add_custom_layer(
    map: &maplibre_native_ffi::MapHandle,
    id: &str,
    host: impl NativeCustomLayer,
    before: Option<&str>,
) -> Result<Pending>;

impl Pending {
    pub fn errors(&self) -> LayerErrors;
    pub fn wait(self) -> Result<Completion>;
    pub fn wait_timeout(&self, timeout: Duration) -> Result<Completion>;
}
```

Example in an application function returning this crate's `Result`:

```rust
let pending = add_custom_layer(map, "raster", compositor, Some("labels"))?;
let errors = pending.errors();
// Move pending to a non-driver worker if this thread must advance native work.
let committed = pending.wait()?;
// committed.generation / committed.render_update_generation are real native values.
// Later, including after removal/teardown:
if let Some(error) = errors.first_error() {
    // Display/log the owned error outside native callbacks.
}
```

`Pending` is an mpsc receiver, not the high-level FFI's future. `MapHandle` at
this revision has neither `with_raw` nor `as_raw`: its public `id()` and the
re-exported `sys::mln_map(id)` are the supported handle representation used here.
Native validates the ID and acquires its own map/control lease for queued work.
No private handle layout, transmute, pointer arithmetic, or internal Rust state
is accessed. Callers should depend on the same pinned official package to avoid
duplicate incompatible `MapHandle` types.

## Ownership, failure, and lifecycle

- Synchronous rejection transfers neither host userdata nor completion userdata.
  Rust releases both exactly once. IDs are validated and copied before return.
- `MLN_STATUS_OK` means **accepted**, not inserted. Native's own
  `submit_map_command` executes insertion through `map_native(live).getStyle()`.
  Duplicate IDs, missing `before` layers, native exceptions, and command
  cancellation reach the raw sys completion and then `Pending::wait` as errors.
  A non-committed disposition is never treated as success, even with status OK.
- The completion can run and release its userdata **before submission returns**.
  C++ holds a submitting reference to the host registration until acceptance is
  recorded, so early execution/rejection cannot leak or double-release userdata.
- Dropping `Pending` does not cancel insertion or free an adopted host. Native
  retains the host until all pending-command, style, renderer, and drawable
  references retire. Layer removal/style replacement is through the official
  FFI; host release may lag its completion. No separate host destroy API exists.
- Native lifecycle is forwarded, not synthesized. `deinitialize` may occur
  without `initialize`; context loss may replace deinitialization. Final Rust
  `Drop` is separate from those callbacks and must not assume a current device.
  After loss, native may initialize a new context. If a callback has failed,
  this bridge keeps the host disabled rather than retrying partially broken work.
- A mutex serializes Rust host calls (Send, not Sync). A host panic is caught,
  the first owned error is stored in the clonable `LayerErrors` monitor, and
  further initialize/pre-render/render calls are suppressed. Cleanup callbacks
  still run. Destructor panics are also caught. A panic payload is deliberately
  leaked to prevent a user-defined payload destructor causing a second unwind
  across C. `panic=abort`, aborting panic hooks, allocation failure, or invalid
  raw GPU usage cannot be recovered by `catch_unwind`.
- Callbacks must return promptly and **must not reenter/wait on the FFI or native
  graphics driver**. The C++ adapter uses upstream's `ForbidReentry` scope to
  reject status-returning reentry. It cannot protect a user blocking directly on
  other threads or calling unsupported raw GPU operations. Do not drop other
  native handles from these callbacks; defer that work outside the callback.
- `wait_timeout` preserves an operation after timeout, but consumes a terminal
  result when delivered. Waiting again after a terminal result is unsupported.

## Graphics/projection contract for the compositor

The matching C and Rust `repr(C)` records carry:

- Width, height, latitude, longitude, zoom, bearing, pitch, field of view.
  Bearing is **degrees**; pitch and field of view are **radians**, verbatim core
  values. Width/height are logical/native viewport units. `pixel_ratio` is the
  native logical-to-physical scale; attachment dimensions can involve rounding.
- `projection_matrix` and `near_clipped_projection_matrix`: 16 column-major
  doubles each, with their existing semantics unchanged. The standard projection
  comes from `TransformState::getProjMatrix`: it already includes the native
  world-to-camera transform, but not the application's tile/model transform.
- `raster_projection_matrix`: native `alignedProjMatrix` with the native flat
  layer offset already applied. This preserves the old raster custom layer's
  world registration, pixel-grid alignment and depth ordering without replacing
  either existing projection field. Continue applying the same tile/model matrix.
- Vulkan: device, physical device retained from initialization, render pass,
  command buffer, `PFN_vkGetDeviceProcAddr`-compatible loader,
  `PFN_vkGetPhysicalDeviceMemoryProperties` (instance-level staging-memory query),
  and clockwise screen pre-rotation. Non-dispatchable render-pass bits are `u64`, including on 32-bit
  systems. No C++ Vulkan dispatcher object crosses the ABI.
- Metal: raw device, encoder, command buffer, render-pass descriptor. Native's
  initialization has no Metal device; retrieve it on a frame callback. Objects
  are borrowed, not retained/transferred by the bridge.

Pre-render occurs **outside** the native pass. Do offscreen CPU color/depth
preparation there, but do not assume its encoder, render-pass descriptor, or
command buffer exists or is usable for in-pass drawing. Only the render callback
supplies the active native pass for compositor draw commands. Native-owned
handles are callback-scoped; do not end/submit/destroy them. Resource management,
pipeline compatibility, native-pass attachment formats/sample counts, GPU
synchronization, depth writes/tests, blending, upload staging and deferred GPU
resource retirement belong to the separate compositor. This crate intentionally
makes no claim that CPU depth can simply be copied into an active depth attachment.

### Native flat depth and layer order

The core patch supplies **new** native fields `rasterProjectionMatrix`,
`depthTest`, `flatDepthOffset` and `pixelRatio`; the bridge copies them to the
corresponding snake-case C/Rust fields, without recomputing or approximating them.
`default_flat_clip_z = 1.0` remains the general custom-host documentation value,
not a raster-specific override.

Let `N` be the current native layer-group count and `u` its **bottom-to-top upload
index**. The native draw index is `d = N - 1 - u`. For Vulkan/Metal:

```text
depth_test = d >= opaquePassCutoff
flat_depth_offset = depth_test ? (1 + u) * numSublayers * depthEpsilon : 0
raster_projection_matrix = alignedProjMatrix
raster_projection_matrix[14] -= flat_depth_offset
```

This matches native `LayerTweaker::multiplyWithProjectionMatrix` for raster
sublayer zero, including float arithmetic before promotion to a matrix double.
The offset is always finite and zero when the native flat depth test is disabled.
It is a **clip-Z translation**, not an NDC-depth subtraction: after projection
its depth effect is `flat_depth_offset / clip.w`. Do not subtract it a second
time, replace raster Z with the generic `1.0`, or apply it after perspective
division as an unscaled constant. Native raster shaders multiply the matrix by
`vec4(tile_position, 0, 1)`; preserve that path through the app's tile/model matrix.
Copying/compositing the resulting depth must not add another offset. Honor
`depth_test` for flat native-pass composition. The existing extrusion renderer's
own depth policy is separate; this patch does not flatten its geometry.

The patch also moves native `preRender` to a layer-group upload tweaker. The
original update-time callback ran before group changes were processed and before
indices/count were valid. Now both pre-render and render produce identical raster
matrix/depth values in the same frame, including the first insertion and layer
reordering. `PaintParameters::layerGroupUpload` disambiguates the two index
conventions, and `layerGroupCount` is set after `processChanges`. Pre-render still
runs before native drawing and outside the native render pass; initialization and
cleanup ownership are unchanged. The existing custom-layer factory is enabled on
Vulkan and Metal, matching the old WebGPU patch's registration intent.

The core patch leaves OpenGL on its native depth-range mechanism (matrix offset
zero); this bridge targets Vulkan/Metal and does not add an OpenGL compositor.

## Building with an isolated core patch

The adapter **must be compiled into the FFI object target**, not into a second
standalone C++ library: it uses the FFI's internal command submission seam and
its existing ABI/compile definitions/includes. The standalone project include
hook defers `target_sources(maplibre_native_c_objects ...)` until targets exist.
Both shared and complete-static FFI wrappers then receive the adapter object;
the `mln_*` export convention includes `mln_custom_layer_add`.

After the official FFI dependency/patch preparation, the build agent must apply
`patches/0001-custom-layer-raster-parameters.patch` to the **native core root**
(`.devenv/maplibre-native-ffi/third_party/maplibre-native`), not the FFI root.
The patch is generated against core `36132b0b0c541a9bed49e95e73b447a96d7aeee5`,
as pinned by the official FFI revision. This crate does not modify that checkout
or automatically apply the patch. The optional extension is mandatory when
building this bridge. `native/core_patch_required.hpp`, included by the injected
adapter, makes the CMake target's compilation fail with an explicit patch error
if its marker is absent, wrong, or its member types do not match. No NaN or
approximate fallback exists. Rebuild the native core and FFI together, because
the native parameter struct layout changes.

Add this to the build agent's existing official configure invocation:

```text
-DCMAKE_PROJECT_INCLUDE=/home/dirk/dev/map/crates/maplibre-custom-layer/cmake/inject.cmake
```

The hook does not replace presets, toolchains, renderer selection, official
patch preparation, or artifact installation. Use the official platform preset
and an out-of-tree build/install directory. For Linux the relevant preset is
`linux-gnu-x64-vulkan` (confirm against the pinned checkout's presets). For Metal,
use its Apple platform preset. If another extension already uses
`CMAKE_PROJECT_INCLUDE`, compose both with a project include file or CMake list;
do not overwrite another agent's hook. The deferred source inherits the
object target's backend definitions and private upstream include directories.

Set `MAPLIBRE_NATIVE_FFI_LOCAL_BUILD` to the resulting **absolute install
prefix**, containing the complete static archive and
`share/maplibre-native-c/artifact.json`. This is upstream's actual build-script
variable. Upstream prebuilt artifacts lack `mln_custom_layer_add` and cannot link
this bridge; the bridge intentionally has no fallback that pretends to work.
No `build.rs` compiles or vendors a second copy of the official FFI.

At implementation time the provided core is initialized but not fully prepared:
its nested geometry submodule is empty and the FFI's required
`patches/maplibre-native/0055-shared-glyph-cache.patch` has not supplied
`mln/text/glyph_cache.hpp` yet. A direct adapter syntax check stops at that
missing header. The build agent must perform the official pinned FFI dependency
and patch preparation before a full native build; the bridge never edits that
checkout itself.

## Validation

From this crate directory (all generated outputs remain under `target/`):

```sh
cargo fmt --all -- --check
python3 tests/native/test_core_patch.py -v
DOCS_RS=1 cargo test --workspace --features vulkan --locked --target-dir target
DOCS_RS=1 cargo clippy --workspace --all-targets --features vulkan --locked --target-dir target -- -D warnings -D clippy::too_many_lines -D clippy::type_complexity -D clippy::too_many_arguments -D clippy::perf
cmake -S tests/cmake -B target/cmake-smoke -DCMAKE_PROJECT_INCLUDE=/home/dirk/dev/map/crates/maplibre-custom-layer/cmake/inject.cmake
```

`DOCS_RS=1` uses upstream's documented no-native-link build-script branch for
**Rust-only seam tests/checks**. These tests do not assert that native insertion
or GPU rendering works. They exercise immediate rejection, accepted queued
failure, completion-before-return, cancellation/disposition handling, receiver
drop/timeouts/disconnect, ownership/drop, lifecycle forwarding, panic containment,
and error monitors. Repeat Rust checks with `--features metal` separately.

From the repository root, scan implementation and test functions directly:

```sh
knots -r crates/maplibre-custom-layer/src crates/maplibre-custom-layer/native/custom_layer.cpp crates/maplibre-custom-layer/native/core_patch_required.hpp crates/maplibre-custom-layer/tests/native --cognitive-threshold 15 --mccabe-threshold 10 --nesting-threshold 4 --sloc-threshold 80
```

The header has declarations only (Knots does not parse its FFI macros). Its C23
and C++20 syntax, 64-bit layouts, and Vulkan function-pointer signatures can be
checked against the official public headers from this crate directory:

```sh
clang -std=c23 -fsyntax-only -Wno-unused-command-line-argument -DMLN_CUSTOM_TEST_VULKAN -Inative/include -I../../.devenv/maplibre-native-ffi/include -I../../.devenv/maplibre-native-ffi/third_party/maplibre-native/include tests/native/abi_layout.c
clang++ -std=c++20 -fsyntax-only -Wno-unused-command-line-argument -DMLN_CUSTOM_TEST_VULKAN -Inative/include -I../../.devenv/maplibre-native-ffi/include -I../../.devenv/maplibre-native-ffi/third_party/maplibre-native/include -x c++ tests/native/abi_layout.c
```

`test_core_patch.py` stages only the six patch-touched **core** files under this
crate's `target/`, applies the relevant official FFI patches and then this
extension, and compiles/runs its real parameter constructor with small camera/paint doubles. Both backend selections
check every layer position and cutoff for group counts 1, 2, 5 and 9, equality
between upload/draw callbacks, unchanged standard/near matrices, aligned XY/W
registration, finite offsets and bottom-to-top depth order. It also checks the
pre-render scheduling edits and proves the unpatched header fails compilation,
both directly and through a CMake test target using the production guard.
These are constructor/scheduling seam tests, not full native/GPU integration.

The patched constructor and custom-layer/pre-render functions are also directly
scannable under `target/core-patch-tests/src/mln/` after running these tests.
Comparing Knots over all six staged core files before/after this extension finds
one existing upstream exception: `Renderer::Impl::render` remains McCabe 42,
cognitive 111, nesting 10; its SLOC increases from 309 to 312 for the three
required count/phase assignments. No other/new function exceeds the thresholds.
This bridge does not refactor the unrelated upstream renderer.

These optional signature checks require Vulkan C headers on the compiler include
path. Omit `-DMLN_CUSTOM_TEST_VULKAN` for a Metal-only host. The CMake smoke test
verifies deferred source injection, not native compilation.
After the build agent produces a prepared native artifact, repeat Cargo tests
without `DOCS_RS`, inspect `mln_custom_layer_add` in the archive, and exercise a
real map with duplicate/missing-before rejection, layer removal, style reload,
map teardown and context loss. Actual Vulkan/Metal rendering remains an
integration check with the separate compositor.
