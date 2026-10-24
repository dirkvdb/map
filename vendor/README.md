# Local WebGPU custom-layer extensions

These are source snapshots of the crates.io packages, patched through the root
`[patch.crates-io]`. No global Cargo registry files are modified.

| Package | Version | Upstream commit | License |
| --- | --- | --- | --- |
| `maplibre_native` | 0.10.0 | `d5295457f5c0492c2a783becd0d38539b84545bc` | MIT OR Apache-2.0 |
| `webgpu-shim` | 0.1.2 | `9d3e8dc5f42062286a3166e0dbee0f33bd407aff` | MIT OR Apache-2.0 |

The native core stays at `core-a33d3f00fb9dbf9117ce18edd799aa0ca5bcce5b`.
`maplibre_native/webgpu-custom-layer.patch` contains the native rendering changes:

- Enable `CustomLayerFactory` in the default WebGPU layer manager.
- Pass the current WebGPU encoder and attachment formats to custom hosts.
- Supply raster-aligned projection and native flat-layer depth offset.

The depth offset uses the bottom-to-top upload index, not the reversed draw-pass
index. The callback also carries the native depth-test mode. Mixing those indices
can let opaque basemap polygons hide an otherwise opaque custom raster (notably
water classes in the land-use fixture).

`maplibre_native/darwin-no-swift.patch` removes the unused Swift language probe
from Darwin's CMake configuration. The CMake targets have no Swift sources, so
the core can build with devenv's Apple SDK without resolving a native Swift
toolchain through Nix's SDK-only `DEVELOPER_DIR`. Objective-C and Objective-C++
remain enabled.

Linux CA trust needs no MapLibre patch: pkgs-mod's libcurl uses OpenSSL's
standard trust paths and `SSL_CERT_FILE` / `SSL_CERT_DIR`. The AppImage launcher
selects the runtime CA bundle. macOS keeps its native trust-store handling.

`maplibre_native/build.rs` checks/applies these patches for source builds, including
the configured `.devenv/maplibre-native` checkout. It refuses conflicts; it does
not reset or overwrite checkout changes. The custom-layer extension requires
a source-built WebGPU core, not an upstream precompiled core.

The C++ bridge requires C++20, the core's RTTI configuration, and the WebGPU
implementation defines explicitly. They must not use optional flag probes:
Nix's compiler-wrapper warning on stderr makes cc-rs reject otherwise supported
flags, silently dropping settings required by the native headers and ABI.

The Rust additions are `WgpuCustomLayer`, `WgpuLayerParameters`, and
`StyleRef::add_wgpu_custom_layer(id, host, before)`, plus their CXX adapter in
`src/cpp/wgpu_custom_layer.*`. MapLibre owns the host. Its synchronous render
callback borrows the active pass on the renderer thread and must neither end
nor retain it. Application-created GPU resources must use the renderer's device.
The deinitialization callback releases resources on removal/context loss.

The shim adds only `with_render_pass` (an unsafe, scoped borrow of its encoder)
and `texture_format_from_webgpu`. It retains ownership of the native pass and
ends/submits it through the existing MapLibre path. There are no handle-layout
casts into Rust's private wgpu types, external render passes, or CPU pixel copies
in the custom-layer integration.

Application-specific tile scheduling, caches, and WGSL coloring live in
`src/raster_tiles.rs`, `src/raster_layer.rs`, and `src/raster_layer.wgsl`, not in
these dependency extensions. The extensions are intended to be replaceable by
equivalent upstream APIs.
