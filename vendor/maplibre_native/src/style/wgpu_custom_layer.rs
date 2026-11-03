use crate::bridge::ffi;

/// Camera and attachment information for a custom WebGPU layer.
pub use ffi::WgpuLayerParameters;

/// A custom layer drawn inside MapLibre's active WebGPU render pass.
///
/// The renderer owns the host and invokes it on its thread. Resources must use
/// the same device supplied to `ImageRenderer::set_device_queue`. Tile loading
/// and repaint scheduling remain the application's responsibility.
///
/// # Surface raster projection
///
/// This is the surface (zero-elevation, no pole sentinels) path from native's
/// `shaders/webgpu/common.hpp::interpolateProjection`, not the 3D depth path.
/// Upload the fields below as f32 uniforms, with `globe` as a u32 flag. Do not
/// upload the FFI struct directly: its f64 fields and bool are not WGSL layout.
///
/// ```wgsl
/// // m = (tile_xy + uv) / exp2(tile_z), plus world wrap in m.x.
/// // Native tile-local positions use uv = position / 8192.0.
/// fn project_surface(m: vec2<f32>) -> vec4<f32> {
///     let flat = u.mercator_projection * vec4<f32>(m, 0.0, 1.0);
///     if (u.globe == 0u) { return flat; }
///     let pi = 3.141592653589793;
///     let longitude = m.x * (2.0 * pi) + pi;
///     let t = exp(pi - m.y * (2.0 * pi));
///     let t2 = t * t;
///     let cos_lat = 2.0 * t / (t2 + 1.0);
///     let sphere = vec3<f32>(sin(longitude) * cos_lat,
///                            (t2 - 1.0) / (t2 + 1.0),
///                            cos(longitude) * cos_lat);
///     var g = u.globe_projection * vec4<f32>(sphere, 1.0);
///     g.z = (1.0 - (dot(sphere, u.globe_clipping_plane.xyz)
///                   + u.globe_clipping_plane.w)) * g.w - u.depth_offset;
///     let blend = u.projection_transition;
///     if (blend > 0.999) { return g; }
///     let xyw = mix(flat.xyw, g.xyw, blend);
///     let z = mix(0.0, g.z, clamp((blend - 0.2) / 0.8, 0.0, 1.0));
///     return vec4<f32>(xyw.xy, z, xyw.z);
/// }
/// ```
///
/// Blend homogeneous XYW *before* perspective division. Do not mix the matrices
/// or all four clip components, remap Z from OpenGL, or hard-discard against the
/// horizon plane during blending. Native uses the blended Z for clipping.
/// Sphere axes are +X at 90°E, +Y north, +Z at (0°, 0°).
///
/// Tessellate globe tiles; a four-vertex quad cannot follow the sphere. Keep
/// texture UV independent of geographic position. Native raster pole caps use
/// extra vertices (raw tile Y < -32767.5 or > 32766.5), forced to (0, ±1, 0),
/// with a separate pole-hiding transition; ordinary XYZ tiles stop at ±85.05113°.
/// Mercator fallback X includes world wrap, while sphere longitude is periodic.
/// With native depth (`private_depth` is false), use `LessEqual` with depth
/// writes disabled when `depth_test` is true; otherwise disable depth
/// testing/writes (native globe 2D layers use painter's order).
///
/// Override [`Self::private_depth`] to request an isolated depth attachment
/// while drawing directly into native's color attachment. When the request is
/// supported, `render` receives `private_depth = true`, `depth_format = 0x30`
/// (`Depth32Float`), and `depth_test = false`. Use reversed depth with `Greater`
/// and depth writes; the private attachment is cleared to zero. Native depth
/// and stencil are preserved, not copied into the private attachment.
pub trait WgpuCustomLayer: 'static {
    /// Requests private depth immediately before the render pass is borrowed.
    ///
    /// Called only when the active native pass supports private depth. `size`
    /// is its actual color attachment extent in physical pixels, not the
    /// logical viewport dimensions in `parameters`. Return `None` (the default)
    /// to keep the native attachments. Unsupported passes skip this request.
    ///
    /// The returned view must be a same-device, single-sampled `Depth32Float`
    /// render attachment with exactly this extent. Keep its resources valid
    /// and undestroyed throughout the scoped draw. The shim validates the view
    /// before switching; if rejected, `render` still runs once with the original
    /// native parameters and attachments. Check `parameters.private_depth` in
    /// `render` rather than assuming success.
    fn private_depth(
        &mut self,
        _parameters: &WgpuLayerParameters,
        _size: [u32; 2],
    ) -> Option<wgpu::TextureView> {
        None
    }

    /// Records commands without ending or retaining the borrowed render pass.
    /// The parameters describe the actual native or private depth attachment.
    fn render(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters);

    /// Releases device resources on layer removal or context loss.
    fn deinitialize(&mut self) {}
}

pub(crate) struct WgpuLayerHost(pub Box<dyn WgpuCustomLayer>);

pub(crate) fn wgpu_layer_render(
    host: &mut WgpuLayerHost,
    encoder: usize,
    parameters: &WgpuLayerParameters,
) {
    if encoder == 0 {
        return;
    }
    // SAFETY: native supplies its live, exclusive shim handle on the renderer
    // thread. This query does not change the active pass.
    let size = unsafe { webgpu_shim::private_depth_target_size(encoder) };
    if let Some(depth) = size.and_then(|size| host.0.private_depth(parameters, size)) {
        let mut private_parameters = parameters.clone();
        private_parameters.depth_format = 0x30; // WebGPU C ABI Depth32Float.
        private_parameters.depth_test = false;
        private_parameters.private_depth = true;
        // SAFETY: native supplies a live, exclusive shim handle on its renderer
        // thread. The layer supplies a same-device, framebuffer-matching depth
        // view per the trait contract; it stays alive through the scoped draw.
        // The FFI host rebinds pipeline/buffer state after custom drawing; the
        // shim resumes the same handle and reapplies native viewport/scissor.
        let rendered = unsafe {
            webgpu_shim::with_render_pass_with_depth(encoder, &depth, |pass| {
                host.0.render(pass, &private_parameters);
            })
        };
        if rendered {
            return;
        }
    }
    // SAFETY: the native CustomLayerHost supplies its current shim encoder only
    // during render(), on the renderer thread. The borrow cannot escape. An
    // unsupported private-depth request leaves this pass untouched.
    unsafe { webgpu_shim::with_render_pass(encoder, |pass| host.0.render(pass, parameters)) };
}

pub(crate) fn wgpu_layer_deinitialize(host: &mut WgpuLayerHost) {
    host.0.deinitialize();
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use super::{WgpuCustomLayer, WgpuLayerParameters};
    use crate::{CameraUpdate, Continuous, ImageRenderer, ImageRendererBuilder, LatLng};

    type CapturedParameters = Rc<RefCell<Vec<WgpuLayerParameters>>>;

    struct CaptureLayer(CapturedParameters);

    impl WgpuCustomLayer for CaptureLayer {
        fn render(&mut self, _: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
            self.0.borrow_mut().push(parameters.clone());
        }
    }

    fn test_renderer(captured: &CapturedParameters) -> ImageRenderer<Continuous> {
        let (device, queue) = futures::executor::block_on(async {
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
                .request_adapter(&wgpu::RequestAdapterOptions::default())
                .await
                .unwrap()
                .request_device(&wgpu::DeviceDescriptor::default())
                .await
                .unwrap()
        });
        let mut renderer = ImageRendererBuilder::new().build_continuous_renderer();
        renderer.set_device_queue(device, queue);
        renderer
            .load_style_from_json_str(r#"{"version":8,"sources":{},"layers":[]}"#)
            .wait()
            .unwrap();
        for id in ["first", "second"] {
            renderer
                .style()
                .add_wgpu_custom_layer(id, CaptureLayer(Rc::clone(captured)), None)
                .unwrap();
        }
        renderer
    }

    fn capture_frame(
        renderer: &mut ImageRenderer<Continuous>,
        captured: &CapturedParameters,
        globe: bool,
        zoom: f64,
    ) -> Vec<WgpuLayerParameters> {
        renderer.set_globe(globe);
        renderer.update_camera(
            &CameraUpdate::new().center(LatLng { lat: 0.0, lng: 0.0 }).zoom(zoom).pitch(40.0),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            captured.borrow_mut().clear();
            renderer.render_once();
            let parameters = captured.borrow();
            if parameters.len() == 2 && (parameters[0].zoom - zoom).abs() < 1e-6 {
                return parameters.clone();
            }
            assert!(Instant::now() < deadline, "custom layers did not render at zoom {zoom}");
        }
    }

    fn transform(matrix: &[f64; 16], point: [f64; 4]) -> [f64; 4] {
        std::array::from_fn(|row| (0..4).map(|col| matrix[col * 4 + row] * point[col]).sum())
    }

    fn assert_mercator_contract(parameters: &WgpuLayerParameters) {
        let world_size = 512.0 * parameters.zoom.exp2();
        let point = [0.375, 0.625, 0.0, 1.0];
        let world = [point[0] * world_size, point[1] * world_size, 0.0, 1.0];
        let normalized = transform(&parameters.mercator_projection, point);
        let pixels = transform(&parameters.projection, world);
        for (normalized, pixels) in normalized.into_iter().zip(pixels) {
            assert!((normalized - pixels).abs() < 1e-7);
        }
        if !parameters.globe {
            assert_eq!(parameters.globe_projection, parameters.mercator_projection);
            assert_eq!(parameters.globe_clipping_plane, [0.0; 4]);
        }
    }

    #[test]
    fn native_parameters_follow_projection_blend_and_layer_order() {
        let captured = CapturedParameters::default();
        let mut renderer = test_renderer(&captured);
        for (globe, zoom, transition) in [
            (false, 0.0, 0.0),
            (true, 0.0, 1.0),
            (true, 11.5, 0.5),
            (true, 12.0, 0.0),
            (false, 0.0, 0.0),
        ] {
            let parameters = capture_frame(&mut renderer, &captured, globe, zoom);
            let first = &parameters[0];
            let second = &parameters[1];
            assert!(CaptureLayer(Rc::clone(&captured)).private_depth(first, [512; 2]).is_none());
            assert!(!first.private_depth && !second.private_depth);
            assert_eq!(first.globe, transition > 0.0);
            assert!(
                (first.projection_transition - transition).abs() < 1e-7,
                "zoom={zoom}, transition={}",
                first.projection_transition,
            );
            assert!(
                (second.depth_offset - first.depth_offset - 3.0 / 2048.0).abs() < 1e-12,
                "globe={globe}, zoom={zoom}, offsets=({}, {})",
                first.depth_offset,
                second.depth_offset,
            );
            assert!(second.depth_offset > 0.0);
            assert_mercator_contract(first);
            if first.globe {
                assert!(!first.depth_test);
                let center = transform(&first.globe_projection, [0.0, 0.0, 1.0, 1.0]);
                assert!(center[3] > 0.0);
                assert!((center[0] / center[3]).abs() < 1e-6);
                assert!((center[1] / center[3]).abs() < 1e-6);
                let plane = first.globe_clipping_plane;
                assert!(plane[2] + plane[3] > 0.0);
                assert!(-plane[2] + plane[3] < 0.0);
            }
        }
    }
}
