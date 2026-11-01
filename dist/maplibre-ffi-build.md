# Official MapLibre FFI native build handoff

## Nix build (current workflow)

`dist/maplibre-native.nix` is the shared native FFI derivation for the flake and
devenv. It pins revision `6718eff7beeb927c6c629b4da617338b0b8fab03`, applies
upstream's engine patches and Map's core patch, and includes both native
extensions. Linux uses Vulkan; macOS uses Metal (x86_64 and aarch64 hosts).

```sh
nix build .#maplibre-native        # Native install prefix is ./result
just maplibre                     # Native prefix is .devenv/maplibre-native-nix
devenv build outputs.maplibre-native # Build with devenv's locked package set
devenv shell -- cargo build       # Prefix exported automatically

# Cargo outside devenv, after just maplibre:
export MAPLIBRE_NATIVE_FFI_LOCAL_BUILD="$(readlink -f .devenv/maplibre-native-nix)"
```

Sources (including selected submodules), CMake downloads, and Rust dependencies
are fixed-output Nix fetches; the native build runs offline. The result contains
the official headers, complete static archive, platform archive and artifact
descriptor. Install checks validate the revision, target, backend and extension
symbols. Only the native extension sources/hooks/patches enter the native
derivation's local source hash: changing application Rust code or this handoff
does not rebuild C++. A changed pin, patch, native source or toolchain does.

Nix retains successful builds locally and can distribute them through a binary
cache. Publishing to Cachix is a separate action; no cache upload is automatic.
Flake and devenv share the recipe, but differing locked nixpkgs/toolchain versions
can produce different store paths. Release cross-compilation remains separate.

Validated on x86_64 Linux: sandboxed configure/build/install and all install
checks pass, including the SBOM and extension symbols. The flake and devenv build
commands reuse the same cached store output with the current locks. Flake
evaluation passes for all four
platforms; macOS and aarch64 native builds have not been run here.

The former Python preparation/build wrapper and its dedicated tests have been
removed: the Nix derivation owns source preparation, extension injection,
archive-symbol fixes and install validation. There is no longer a maintained
manual non-Nix build wrapper. Existing local checkouts and build outputs are
untouched.

The historical manual-build and runtime-validation log below records commands
for the removed wrapper; those commands are not current build instructions.
Its `.devenv/maplibre-native-ffi-build` paths are not the Nix installation.

**Latest test status:** the full Linux application suite passes **693 tests with
zero failures and zero ignored tests**. The earlier image-test blockers below are
resolved; see [test closure](#test-closure) for the fixes and reproduction command.
Windows/macOS native runtime validation and release packaging remain separate work.

The app dependency and `dist/maplibre-native.nix` must use the same official
`maplibre-native-ffi` git revision, currently
`6718eff7beeb927c6c629b4da617338b0b8fab03`. The native derivation does not edit
Cargo manifests or app source.

## Extension coordination contract

The Nix derivation composes these store-path includes into a generated
`map-extensions.cmake` (passed as `CMAKE_PROJECT_INCLUDE`):

- `crates/maplibre-custom-layer/cmake/inject.cmake`
- `crates/maplibre-raster-compositor/cmake/inject.cmake`

Both hooks exist. The compositor now includes `<string>` directly; the native
build no longer needs the temporary source-specific forced-header flag.

Hook contract: Follow the custom-layer hook's
`include_guard(GLOBAL)` + `cmake_language(DEFER CALL ...)` pattern and inject
sources into `maplibre_native_c_objects`; that target is not available at
`project()` time. Do not compile a second FFI or replace official presets.
The hooks must use distinct function names. If additional generated shaders or
libraries are needed, arrange them from that hook. Both extensions are required
for a full build; there is deliberately no silent custom-layer-only fallback.

The fixed-output source fetch runs official `tools/deps/sync-submodules` in an
isolated checkout. The native derivation then applies
`crates/maplibre-custom-layer/patches/*.patch` to `third_party/maplibre-native`.
It uses upstream CMake presets, builds Release, installs the official artifact
layout, and validates the descriptor revision/target/backend, static archives,
SBOM and extension symbols. Tests/fixtures are disabled for the application
library build. Linux selects Vulkan; macOS selects Metal. The Nix derivation
supports native x86_64/aarch64 Linux and macOS, not Windows cross-compilation.
Windows requires the official MSVC/clang toolchain, not the legacy MinGW/wgpu
packaging setup.

## Install environment

`MAPLIBRE_NATIVE_FFI_LOCAL_BUILD` must be an **absolute install prefix**, not a
source or build directory. Devenv exports the native derivation's store path
automatically; for Cargo outside devenv, use the export in the Nix commands above.
The prefix contains `share/maplibre-native-c/artifact.json`,
`lib/libmaplibre-native-c.a` and `lib/libmln_ffi_platform.a`. Upstream prebuilt
archives do not contain Map's extensions and must not be used instead.

## Historical manual-build validation / blockers

- Original `.devenv/maplibre-native-ffi` root remains untouched at `d47a1fb...`.
  Its core and several nested submodules have pre-existing patch changes despite
  the clean FFI root; preparation correctly refuses to discard those changes.
- Fresh `.devenv/maplibre-native-ffi-map` checkout is at the latest pinned commit.
  Official dependencies/patches and the custom-layer core patch prepared
  successfully within a bounded command.
- Python regression suite: 152 tests, passing, 17 optional native tests skipped
  (22 preparation/build tests).
  New preparation tests cover safe revision checkout, staged/unstaged/untracked
  and nested edits, idempotent/conflicting patches, recorded sync, composed hooks,
  presets/build/install commands, and install validation.
- Direct Knots scan of the script and its new tests passes cognitive 15, McCabe
  10, nesting 4, function SLOC 80 thresholds; no new violations.
- `nixfmt --check devenv.nix`, `just --show maplibre`, and `git diff --check`
  pass. `devenv info` evaluates successfully and confirms the absolute
  `MAPLIBRE_NATIVE_FFI_LOCAL_BUILD` value, Zig and cargo-about packages. It emits
  only the existing deprecated `xorg.libxcb` warning. No Rust files were changed;
  Cargo formatting, Clippy and application tests are outside this scoped task.
- Resumed command `devenv shell -- python3 -B scripts/prepare_maplibre.py --jobs 4`
  configured both hooks but failed compiling compositor `native/vulkan.cpp`
  (exit 1): `implicit instantiation of undefined template 'std::basic_string<char>'`
  at line 22 and `no member named 'to_string' in namespace 'std'` at line 23.
  Root cause is the missing `<string>` include. Crate sources were not edited.
- Native build and installation now pass. Successful final command:
  `devenv shell -- python3 -B scripts/prepare_maplibre.py --jobs 4`.
  Every native build call was bounded to 600 seconds; no timeout occurred.
  Builds resumed existing Ninja outputs and never reran forced source preparation.
- Descriptor validation confirms commit
  `6718eff7beeb927c6c629b4da617338b0b8fab03`, target
  `x86_64-unknown-linux-gnu`, and backend `vulkan`.
- `llvm-nm --defined-only --extern-only` on the installed complete archive confirms
  global definitions for `mln_custom_layer_add`, `mln_custom_layer_vulkan_context`,
  and all six `map_compositor_*` functions. Upstream's ELF archive originally
  localized compositor symbols (`t`, not `T`) because its allowlist is `mln_*`.
  The script now globalizes only the six compositor C ABI symbols on the generated
  complete archive and checks all required symbols before installation.
- A native C static-link/run smoke test passed. It linked the installed complete
  archive and `libmln_ffi_platform.a`, retained a reference to
  `mln_custom_layer_add`, called compositor creation/deletion, and printed
  `extension static link smoke passed`. This proves actual extension symbol
  resolution, not only archive/file existence. No GPU rendering test was run.
- Shared-library compositor exports are not validated or repaired by this Linux
  static-archive finalization. The parent official Rust binding uses the complete
  static archive. macOS/Windows builds remain unvalidated.

### Resolved build failures and temporary compatibility input

1. The initial devenv build failed in compositor `vulkan.cpp` because `<string>`
   was absent. That include is now present. The earlier workaround used a
   temporary ignored generated CMake file setting
   `COMPILE_OPTIONS "-include;string"` **only** for that source. It was selected by
   `CMAKE_PROJECT_INCLUDE_BEFORE` in this build directory's cache. No broad compiler
   flags, fake SDK/tooling options, or stub implementations were introduced.
2. Native archive assembly failed with exit 127:
   `gcc-ld/ld.lld: line 280: @mktemp@: command not found`. This is Nix's broken
   Rust linker wrapper, not a MapLibre compile failure. The generated project
   hook now detects and selects the adjacent `gcc-ld-unwrapped/ld.lld` when it
   exists. Both the cached and normal `CMAKE_LINKER` variables must be set:
   CMake's compiler metadata otherwise shadows the corrected cache value.
   Host/GPUI linking is unchanged, and prepared FFI sources were not patched.
3. Archive validation found local `map_compositor_*` symbols. The script's
   generated-artifact-only finalization fixes the actual exported ABI rather
   than relaxing validation or selecting a substitute library.

The compatibility configure/build command was run with devenv profile tools on
PATH (the original shell had no Zig/cargo-about). This exact Python invocation
selected the profile without launching the unconfigured preparation task first:

```sh
python3 -c 'import os; from pathlib import Path; env=dict(os.environ, PATH=str(Path(".devenv/profile/bin").resolve())+os.pathsep+os.environ["PATH"]); os.execvpe("python3", ["python3", "-B", "scripts/prepare_maplibre.py", "--jobs", "4", "--cmake-arg=-DCMAKE_PROJECT_INCLUDE_BEFORE=/home/dirk/dev/map/.devenv/maplibre-native-ffi-build/linux-gnu-x64-vulkan/compositor-header-compat.cmake"], env)'
```

The ignored compatibility file contains:

```cmake
include_guard(GLOBAL)
set_source_files_properties("/home/dirk/dev/map/crates/maplibre-raster-compositor/native/vulkan.cpp" PROPERTIES COMPILE_OPTIONS "-include;string")
```

An attempted CLI task override
`devenv shell --option 'tasks.maplibre:prepare.exec:string' 'true' -- ...`
failed before execution with:
`Unsupported type 'prepare.exec:string' for option 'tasks.maplibre'`.
It was abandoned; no task configuration or sources were changed for it.

The resumed migration cleared the cached temporary include and successfully
rebuilt/installed the native archive with the ordinary source on Linux:

```sh
devenv shell -- python3 -B scripts/prepare_maplibre.py --jobs 4 --cmake-arg=-DCMAKE_PROJECT_INCLUDE_BEFORE=
```

The archive linker and compositor-symbol fixes are permanent script changes;
the source-specific header workaround is not. No parent source, Cargo manifests,
or crate files were edited. The original and recorded prepared-source states
remain unchanged.

Legacy `scripts/package_{linux,macos,windows}.py` still contain the old MLN build
configuration and are outside this agent's ownership. The release packaging
pipeline needs a separate migration to build target-specific official FFI
prefixes before Cargo and propagate `MAPLIBRE_NATIVE_FFI_LOCAL_BUILD`; old
wgpu/MinGW toolchain assumptions must not be advertised as validated FFI builds.

## Resumed application migration (2026-11-01)

The application links the official FFI and both extensions against the Linux
Vulkan installation. GPUI and numeric raster shaders remain on wgpu; raster
color/depth are read back before native composition. No zero-copy sharing is
claimed. The requested platform split remains Vulkan for Linux/Windows and Metal
for macOS.

Application fixes in this continuation:

- Resize through the attached render session, which resizes its target and map
  together. Calling `MapHandle::resize` while a session is attached is rejected.
  DPI changes still recreate the worker's map: session resize cannot change its
  attachment scale factor.
- Hand completed images and raster/playback state to the caller before starting
  another native frame. Track the latest available native update so deferring
  frame demand cannot incorrectly mark an unrendered update settled.
- Add offline frame-handoff/resize regressions and simplify the extrusion growth
  test without removing its assertions.
- Refresh the standalone bridge lockfile to the manifest's official FFI pin.

Validation on the AMD Radeon 880M / RADV Linux host:

- Native configure/build/install succeeds after clearing
  `CMAKE_PROJECT_INCLUDE_BEFORE`; the missing-header workaround is no longer used.
- Default application tests: **556 passed, 127 ignored** with
  `cargo test --workspace --all-features --locked`.
- Frame/progress/resize tests: **9 passed**, including normally ignored GPU cases.
- Native camera/raster/extrusion tests: **5 passed**, covering pitched/rotated
  registration, static/multiband multi-tile opacity, culling, height and HiDPI.
- Standalone custom-layer tests: **23 passed**; compositor tests: **8 passed**,
  with `--features vulkan` and the real native installation (not `DOCS_RS`).
- Python packaging/preparation suite: **152 tests, 17 skipped**, passing.
- Application formatting and strict workspace Clippy pass. Direct Knots scans of
  application/extension Rust and supported native/preparation sources use
  cognitive 15, McCabe 10, nesting 4 and function SLOC 80 thresholds.

The broader local `map_renderer::` GPU run has **50 passing tests and two remaining
image failures**:

1. `shared_band_range_drag_renders_latest_colors_without_changing_anchor_or_clock`
   fails its final source-restyle image comparison.
2. `band_fades_keep_current_and_previous_clamp_colors_independent` fails its
   current-band endpoint comparison.

These assertions were not weakened or disabled. The local run explicitly omitted
seven network/local-file timing cases; the initial unfiltered run additionally
hit remote-map timeouts and missing `MAP_TEST_VECTOR`/NetCDF fixtures. The overall
migration is **not complete**. Remaining work includes these image regressions,
release packaging, and native Windows/MSVC Vulkan and macOS Metal build/runtime
validation. Existing root `Cargo.toml` trailing blank lines also make the full
`git diff --check` fail; this continuation's source/doc changes are whitespace-clean.

## Performance follow-up

The correctness handoff fix initially deferred the next frame demand until another
16 ms worker turn, effectively limiting warm playback to about 30 fps. The worker
now captures image/playback metadata first, then primes the next native frame.
Pending native frames are polled at 2 ms after a missed presentation tick; idle
polling and normal image delivery retain the 16 ms interval. Direct `NativeMap`
callers still receive a completed image before another frame starts.

The compositor also compiled shaders/pipelines and created pipeline layouts every
frame. Compatible consecutive Vulkan frames now share immutable pipeline/layout
state; Metal reuses compatible pipeline/depth state. Fresh frame upload buffers
are still retired through native fences/completion handlers, and retain shared
pipeline ownership. The Vulkan GPU smoke checks that consecutive compatible
preparations compile only once, changed depth modes compile again, and previously
recorded commands remain valid across replacement/deinitialization.

Raster drawing and readback copies now use one wgpu submission. Color packing uses
row-sized copies, and depth is decoded directly from padded rows without an
intermediate viewport-sized byte vector.

Release-mode warm worker measurements on the same RADV/Radeon 880M host, using
the local 43-band COG fixture, ten warmup frames and sixty measured frames:

| Physical viewport | Migrated baseline | After fixes | Mean interval before → after |
| --- | ---: | ---: | ---: |
| 512×512 | 31.1 fps | 59.3 fps | 32.13 → 16.86 ms |
| 1024×1024 | 29.1 fps | 45.6 fps | 34.33 → 21.92 ms |

Repeat with:

```sh
cargo test --release --locked worker_playback_frame_cadence -- --ignored --test-threads=1 --nocapture
```

These compare the migrated implementation before/after this follow-up, not the
pre-migration renderer or monitor presentation. GPU→CPU→GPU color/depth transfers,
fresh native buffer allocations and final GPUI image uploads remain; full
performance parity is not established. Linux Vulkan build/runtime validation is
available here. Metal pipeline reuse received manual review only; Apple SDK
compilation/runtime checks remain outstanding.

Follow-up validation: the release application binary and Linux native installation
build successfully; default tests pass (**557 passed, 128 ignored**). The broad
local renderer run remains **50 passed, the same two image failures** listed
above, with network/local-file cases and the release-only cadence measurement
omitted. Native camera/extrusion correctness and the Vulkan pipeline-reuse/
retirement smoke pass. No image assertions were relaxed for the performance work.

## Continuous mouse-rate camera input

Stationary playback did not exercise a separate request-fencing regression. The
worker applied every new input request while a native draw was pending, resetting
frame progress and advancing `required_update`. Completed draws were consequently
rejected as stale whenever another mouse update arrived before readback. A local
continuous pan reproduced **zero delivered images during two seconds of movement**
at both tested sizes; images arrived after input stopped.

`RenderInbox` now retains only the latest pending request. Native camera/style
changes are admitted after the in-flight image is handed off, retaining that
image's admitted source/viewport/playback tags. Input received during readback is
coalesced before deciding whether to prime another old-request frame. Pending input
also prevents an older frame from being marked settled. This does not weaken native
generation fencing or introduce an unbounded queue of native camera commands.

The continuous-input regression warms a local raster, submits about 500 camera
requests/sec for two seconds, counts distinct delivered image allocations during
movement, and waits for final settlement. Final release measurements on the same
Linux/RADV host:

| Viewport | Before admission fix | After admission fix |
| --- | ---: | ---: |
| 512×512 | 0 images / 0 fps | 102 images / 51.0 fps |
| 1024×1024 | 0 images / 0 fps | 92 images / 46.0 fps |

```sh
cargo test --release --locked continuous_camera_input_keeps_delivering_frames -- --ignored --test-threads=1 --nocapture
```

These measure worker delivery under simulated mouse-rate input, not physical mouse
events or monitor presentation. Three CPU regressions additionally verify deferred
admission/latest-only coalescing, retaining pending input through timeouts, and
keeping the latest request's tags when input arrives during readback. Default tests
pass (**560 passed, 129 ignored**); formatting, strict workspace Clippy and direct
Knots on `src/` pass cognitive 15, McCabe 10, nesting 4, SLOC 80 without suppressions.
The release application binary has been rebuilt with this fix.

The broader local renderer rerun passes **53 tests**, including source switching,
playback and frame/progress/resize coverage; the **same two existing image
comparisons still fail**. Network/local-file cases and the two release-only
measurements were omitted from that debug correctness run. No additional renderer
failures were observed.

## Test closure

The remaining image regressions are resolved:

- Static rasters retain the last band's raw GPU tiles for reuse when switching
  back. Retention is keyed by raw source identity and DPI, discarded on clear or
  an unrelated source change, and trimmed against the current cache's remaining
  **128 MiB / 256 tile** budget. It does not allocate an unlimited per-band cache.
  Clamp/scale/range changes now render correctly even after the original file is
  renamed, with the existing no-new-read/upload assertions intact.
- The playback range comparison explicitly sets its reference display range to
  0–700, instead of accidentally using the constructor's all-band data range.
  The exact full-image equality, anchor identity and clock assertions remain.
- The scaled-band transition test checks complete endpoints under reduced motion,
  including nodata, and accepts an already-cached replacement on its first frame.
  Animated loading still requires the exact outgoing image.
- The coast/desert NetCDF regression now creates its own georeferenced fixture
  and retains the original geographic probes and raw-tile/GPU orientation checks.
  It no longer requires an unshared Abu Dhabi input file.

Full application validation on Linux Vulkan: **693 passed, 0 failed, 0 ignored**,
including normally ignored GPU, network, local-file and cadence tests. Reproduce
from the repository root in the native development environment:

```sh
MAP_TEST_RASTER="$PWD/infra-rs/tests/data/landusebyte.tif" \
MAP_TEST_NETCDF="$PWD/infra-rs/tools/createcog/tests/data/uc-small.nc" \
MAP_TEST_VECTOR="$PWD/infra-rs/crates/geo/tests/data/boundaries.gpkg" \
cargo test --release --workspace --all-features --locked -- --include-ignored --test-threads=1
```

The default application run also passes: **563 passed, 130 ignored**. Standalone
Rust extension suites pass with the real native installation and `--features
vulkan`: custom-layer **23**, compositor **8**. The custom-layer core-patch suite
passes **4** tests, and the compositor's Vulkan GPU/retirement CTest passes.
Python discovery with `MAP_RUN_NATIVE_TESTS=1` passes **150 tests**, with only
**2 macOS-only tests skipped** on Linux. Formatting and strict workspace Clippy
pass; direct Knots on `src/` passes cognitive 15, McCabe 10, nesting 4, SLOC 80,
without new violations, suppressions or baseline changes.

Nextest's isolated processes additionally exposed an intermittent post-success
`SIGSEGV` during teardown of a multiband zoom test. Numeric and multiband tile
workers were detached, allowing native raster I/O/thread-local cleanup to outlive
their owning layers. Both pools now retain thread handles, cancel work, close the
job and result queues, and join workers during destruction. Constructing the
owning pool before spawning also makes partial startup failure use the same safe
cleanup. Two CPU regressions check that destruction releases all worker-held
shared state. No retry or skip was added to mask the process-exit failure.

After worker shutdown was fixed, **both Cargo and Nextest pass all 693 application
tests**. Nextest reports **693 passed, 0 skipped**, with ignored cases enabled and
one test process at a time. The formerly crashing multiband zoom test additionally
passes **20/20 isolated Nextest stress iterations**. For Nextest, use the same
three fixture environment assignments above with:

```sh
cargo nextest run --release --workspace --all-features --locked --run-ignored all --test-threads 1 --no-fail-fast
```
