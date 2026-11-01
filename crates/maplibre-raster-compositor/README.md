# Native raster compositor integration contract

Composites offscreen-rendered color and depth into MapLibre Native's Vulkan or
Metal render pass through `maplibre-custom-layer`. It brings the application's
wgpu output into the map's layer stack, preserving transparency and depth-based
occlusion without requiring cross-device GPU texture sharing.

Owned extension for official FFI revision `6718eff7beeb927c6c629b4da617338b0b8fab03`.
The parent keeps wgpu offscreen rendering/readback. This crate draws **inside** the
MapLibre custom layer's existing Vulkan/Metal pass; it never starts, ends, commits,
or submits that pass. Compose `cmake/inject.cmake` with the bridge injection hook.

## Rust API

```rust,ignore
use maplibre_raster_compositor::{NativeCompositor, Surface};
// Store the compositor in your existing NativeCustomLayer implementation.
let mut compositor = NativeCompositor::new(); // CPU-only, infallible
// In pre_render, after completed wgpu readback:
unsafe { compositor.prepare(parameters, Surface {
    width: physical_width, height: physical_height,
    color: packed_premultiplied_rgba8, depth: packed_f32_depth,
    extruded: is_extruded,
})?; }
// In render, with this callback's fresh borrowed FrameParameters:
unsafe { compositor.render(parameters)?; }
// In context_lost (no GPU calls):
compositor.context_lost();
// In deinitialize, before host release, while the bridge callback is active:
unsafe { compositor.deinitialize()?; }
```

Methods return this crate's `Result`; convert errors to bridge `Error::Host(e.to_string())`.
`NativeSurface` is an alias of `Surface`. Both GPU methods are unsafe because
`FrameParameters` exposes forgeable raw borrowed native handles. Calls must be
serialized, callback-scoped, and belong to the same native device/host. The object
is `Send`, not `Sync`. No work is done by `new`, and initialization is lazy.

## Surface contract

- `width,height: u32`: physical output pixels, **not logical** frame dimensions.
- `color: Vec<u8>`: tightly packed, top-left origin RGBA8 **premultiplied**, no
  wgpu row padding; channel values are linear UNORM (not an sRGB readback).
- `depth: Vec<f32>`: exactly width*height entries, finite in `[0,1]`, same origin.
  Supply depth for flat surfaces too: use native `raster_projection_matrix`
  including its already-applied clip-Z offset, plus the existing model transform.
  Extrusions use the native near-clipped projection and native world depth.
- `extruded: bool`: depth test/write enabled for extrusion; flat tests only when
  `parameters.depth_test` and never writes depth. Comparison is LessEqual.
- Fragment shader discards alpha zero before depth writes. Blending is
  `One, OneMinusSrcAlpha` for RGB **and alpha**. No extra premultiplication.
- wgpu projection conversion: native matrices use OpenGL clip Z `[-w,+w]`;
  pre-multiply by the conversion `z'=0.5*z+0.5*w` for wgpu `[0,w]`.
  Pass this **converted wgpu read-back depth unchanged** to `Surface.depth`.
  **Important pinned-core detail:** native Vulkan `applySurfaceTransform` only
  flips Y/rotates; native Metal raster/extrusion shaders likewise use raw
  `position.z/position.w` without a GL depth remap. The compositor therefore
  writes **`native_depth = 2 * wgpu_depth - 1`** and discards results outside
  native `[0,1]`, even if flat depth testing is disabled. Supplying raw-native
  depth instead would double-convert incorrectly. Near-clipped/raster matrices
  are world projections, not tile MVPs. Do not subtract `flat_depth_offset` twice.
- Readback is unrotated screen-space. Vulkan compositor compensates native
  clockwise screen pre-rotation when sampling. It matches pinned core's
  `GlobalPlatformParamsUBO` rotation followed by `applySurfaceTransform` Y flip.
  Viewport/scissor are explicitly set to the native physical attachment extent;
  custom drawables skip normal native pipeline setup, so inherited state can be stale.

## Synchronization and lifetime

Finish wgpu submissions and map/copy readback **before** calling `prepare` (the
parent owns that synchronization; preferably schedule outside native callbacks).
No cross-device GPU texture sharing or implicit wgpu synchronization is provided.
Vulkan uses host-visible storage buffers with explicit flush (coherent memory
also works; noncoherent memory is supported), one RGBA uint and one float
per pixel: upload is a CPU copy outside the native pass, with visibility provided
by the native queue submission's host-write dependency. Immutable per-frame
resources are never overwritten while a native command buffer may read them.
A small essential bridge extension exposes callback-scoped Vulkan retirement via
native `Context::enqueueDeletion`; the native frame fence retires buffers,
descriptors, and pipelines without waiting in callbacks. No fixed guessed
"N frames" lifetime, private queue submissions, or `vkDeviceWaitIdle` is used.

`deinitialize` schedules retirement, is idempotent, and should be called in the
bridge's deinitialize callback. `context_lost` abandons lost Vulkan handles
without driver calls. `Drop` releases CPU ownership only and abandons undeinitialized
GPU handles; skipping lifecycle cleanup leaks until device destruction rather
than risking use-after-free. Context loss must precede reuse with a new device.

Metal uses shared-storage buffers and the exact attachment color/depth/stencil
formats and sample count from the native descriptor. A command-buffer completion
handler captures each drawn frame's buffers/pipeline/depth state, including when
native command buffers do not retain resources. Encoding only; no commit or wait.
Apple SDK compilation/runtime validation remains for the parent/macOS builder.
Missing pre-render descriptor/device or active render encoder is an explicit error.

Compatible consecutive frames share immutable pipeline state instead of compiling
a pipeline per frame. Vulkan reuse requires the same render-pass handle and depth
test/write modes; Metal reuse requires matching attachment formats, sample count,
and depth modes. Viewport dimensions are dynamic and do not invalidate pipelines.
Each frame still owns fresh upload buffers and, on Vulkan, its descriptor set/pool.
Fence retirement/completion handlers retain the shared pipeline with every frame,
so replacing or deinitializing the host cannot invalidate recorded draws. Buffer
pooling and zero-copy sharing are not implemented. GPU storage-buffer size limits
are checked before allocation (no silent truncation).

Full native archive build and real map image/depth integration are performed by
the parent build task. Unsupported backends return explicit errors.

## Coordination: essential bridge changes

Only `../maplibre-custom-layer/native/custom_layer.cpp` and its `Cargo.toml` changed:
- Dependency revision corrected to the requested official pin.
- Internal C export `mln_custom_layer_vulkan_context()` exposes the active native
  Context **only inside this host's pre-render/render/deinitialize callback**.
  RAII thread-local scope restores prior state, supports serialized thread changes,
  and is cleared on context loss. No `FrameParameters`/callback ABI change.
- `native/vulkan_retirement.cpp` consumes that private pointer only in C++, invokes
  `Context::enqueueDeletion`, and queries native buffer limits/attachment extent.
  It never exposes or stores this context pointer in Rust.

## Validation and commands

Passed on this Linux checkout:
- `cargo fmt --all -- --check`.
- `DOCS_RS=1 cargo clippy --workspace --all-targets --features vulkan --offline --
  -D warnings -D clippy::too_many_lines -D clippy::type_complexity
  -D clippy::too_many_arguments`.
- `DOCS_RS=1 cargo test --workspace --features vulkan --offline`: eight CPU tests.
  `DOCS_RS` is upstream's documented no-native-link build path, **not** proof of
  a complete application/native archive link. Normal Cargo checks currently fail
  because the parent's install prefix lacks `artifact.json`. `--all-features`
  is deliberately unsuitable: upstream rejects Metal and Vulkan in one artifact.
- Real Vulkan command-buffer/image/depth readback smoke test through CTest:
  premultiplied RGB/alpha blend, zero-alpha discard, flat test on/off, extrusion
  depth writes, LessEqual equality, GL-to-wgpu depth inversion/native clipping,
  clockwise pre-rotation, and replacing/deinitializing/deleting before submission.
  It uses a **test-only retirement queue**, released after real queue completion;
  it does not claim to test the full MapLibre runtime's fence queue.
- CMake configure-only composed injection/idempotency contract.
- Clang C++20 `-Wall -Wextra -Werror` compilation/syntax checks, including the
  retirement adapter and modified bridge against prepared official core headers.
- Direct Knots scan of `src/`, `native/compositor.cpp`, `native/vulkan.cpp`,
  `native/vulkan_retirement.cpp`, `tests/vulkan_smoke.cpp`, and the modified
  bridge `.cpp`: cognitive 15, McCabe 10, nesting 4, function SLOC 80; **no
  violations**. Generated SPIR-V excluded. Installed Knots cannot analyze `.mm`;
  Metal received manual KISS review, not a claimed automated complexity pass.
- `git diff --check` for the owned crate and bridge.

To repeat native tests (Vulkan headers/loader must be discoverable by CMake):

```sh
cmake -S tests/cmake -B target/hook-check
cmake -S tests -B target/native-check \
  -DMAP_COMPOSITOR_FFI_SOURCE=/home/dirk/dev/map/.devenv/maplibre-native-ffi-map
cmake --build target/native-check -j 2
ctest --test-dir target/native-check --output-on-failure
```

Regenerate checked-in shader headers with a host `glslangValidator`:

```sh
glslangValidator -V --vn fullscreen_vert -o native/spirv/fullscreen_vert.hpp shaders/fullscreen.vert
glslangValidator -V --vn composite_frag -o native/spirv/composite_frag.hpp shaders/composite.frag
```

Still for the parent: full official archive/application link, actual map layer
ordering/native extrusion occlusion regression images, resize/context-loss runtime
exercise, and Apple SDK Metal compilation/GPU checks. No application sources,
root manifest, scripts, devenv configuration, or prepared core files were edited.
