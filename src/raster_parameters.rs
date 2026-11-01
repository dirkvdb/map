//! Application-internal parameters for the wgpu raster renderer, not a MapLibre API.

use std::borrow::Cow;

/// Camera and attachment values shared by raster rendering and standalone test hosts.
#[derive(Debug, Clone, Default)]
pub(crate) struct WgpuLayerParameters {
    pub width: f64,
    pub height: f64,
    pub latitude: f64,
    pub longitude: f64,
    pub zoom: f64,
    /// Column-major world-pixel-to-clip matrix, including the flat layer depth offset.
    pub projection: [f64; 16],
    /// Legacy WebGPU format identifiers used by the application's standalone hosts.
    pub color_format: u32,
    /// Depth/stencil format identifier, or zero when the pass has no depth attachment.
    pub depth_format: u32,
    pub depth_test: bool,
    /// Write flat raster depth for native compositing; standalone hosts leave this false.
    pub capture_depth: bool,
    /// Optional near-clipped extrusion projection supplied by the native host.
    ///
    /// The host puts the raster matrix in `projection` and the near-clipped matrix here.
    /// Both must already convert native clip Z with `z = 0.5*z + 0.5*w` (each column's
    /// Z entry becomes half the sum of its Z and W entries). Rendering selects this
    /// matrix after advancing presentation state, before coverage, geometry and uniforms.
    pub native_projection: Option<[f64; 16]>,
}

impl WgpuLayerParameters {
    pub(crate) fn for_extrusion(&self, extruded: bool) -> Cow<'_, Self> {
        let Some(projection) = self.native_projection.filter(|_| extruded) else {
            return Cow::Borrowed(self);
        };
        Cow::Owned(Self { projection, ..self.clone() })
    }

    pub(crate) fn pipeline_formats(&self) -> (u32, u32, bool, bool) {
        (self.color_format, self.depth_format, self.depth_test, self.capture_depth)
    }
}

/// Local contract for standalone borrowed-pass hosts; native MapLibre uses its compositor.
#[cfg(test)]
pub(crate) trait WgpuCustomLayer: 'static {
    fn render(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters);
    fn deinitialize(&mut self) {}
}

/// Converts the color/depth attachment identifiers accepted by raster test hosts.
///
/// Panics for unsupported attachment formats, as a mismatched render pass is a host bug.
pub(crate) fn texture_format_from_webgpu(format: u32) -> wgpu::TextureFormat {
    use wgpu::TextureFormat;
    const FORMATS: [(u32, TextureFormat); 10] = [
        (0x16, TextureFormat::Rgba8Unorm),
        (0x17, TextureFormat::Rgba8UnormSrgb),
        (0x1b, TextureFormat::Bgra8Unorm),
        (0x1c, TextureFormat::Bgra8UnormSrgb),
        (0x2c, TextureFormat::Stencil8),
        (0x2d, TextureFormat::Depth16Unorm),
        (0x2e, TextureFormat::Depth24Plus),
        (0x2f, TextureFormat::Depth24PlusStencil8),
        (0x30, TextureFormat::Depth32Float),
        (0x31, TextureFormat::Depth32FloatStencil8),
    ];
    FORMATS
        .into_iter()
        .find(|(identifier, _)| *identifier == format)
        .map(|(_, format)| format)
        .unwrap_or_else(|| panic!("Unsupported raster attachment format: {format}"))
}

/// Shared depth policy: standalone flats do not write, captured flats do, and
/// extrusions always test/write depth. Debug outlines never affect captured depth.
pub(crate) fn depth_stencil(formats: (u32, u32, bool, bool), extruded: bool, debug: bool) -> Option<wgpu::DepthStencilState> {
    let (_, depth_format, depth_test, capture_depth) = formats;
    if depth_format == 0 {
        return None;
    }
    let compare = match (extruded, depth_test && !debug) {
        (true, _) => wgpu::CompareFunction::Less,
        (false, true) => wgpu::CompareFunction::LessEqual,
        (false, false) => wgpu::CompareFunction::Always,
    };
    Some(wgpu::DepthStencilState {
        format: texture_format_from_webgpu(depth_format),
        depth_write_enabled: Some(extruded || (capture_depth && !debug)),
        // Preserve the nearer surface when finite depth precision ties.
        depth_compare: Some(compare),
        stencil: Default::default(),
        bias: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_defaults_do_not_capture_depth_or_override_projection() {
        let parameters = WgpuLayerParameters::default();
        assert!(!parameters.capture_depth && parameters.native_projection.is_none());
    }

    #[test]
    fn native_projection_is_selected_only_for_extruded_frames() {
        let parameters = WgpuLayerParameters {
            projection: [1.0; 16],
            native_projection: Some([2.0; 16]),
            ..Default::default()
        };
        assert_eq!(parameters.for_extrusion(false).projection, [1.0; 16]);
        assert_eq!(parameters.for_extrusion(true).projection, [2.0; 16]);
        assert_eq!(parameters.projection, [1.0; 16]);
    }

    #[test]
    fn standalone_extrusion_keeps_the_supplied_projection() {
        let parameters = WgpuLayerParameters {
            projection: [3.0; 16],
            ..Default::default()
        };
        assert_eq!(parameters.for_extrusion(true).projection, [3.0; 16]);
    }

    #[test]
    fn depth_capture_changes_the_pipeline_cache_key() {
        let standalone = WgpuLayerParameters::default();
        let native = WgpuLayerParameters {
            capture_depth: true,
            ..standalone.clone()
        };
        assert_ne!(standalone.pipeline_formats(), native.pipeline_formats());
    }

    #[test]
    fn flat_depth_capture_preserves_standalone_host_testing_policy() {
        for depth_test in [false, true] {
            for capture_depth in [false, true] {
                let state = depth_stencil((0x16, 0x30, depth_test, capture_depth), false, false).unwrap();
                assert_eq!(state.depth_write_enabled, Some(capture_depth));
                assert_eq!(
                    state.depth_compare,
                    Some(if depth_test {
                        wgpu::CompareFunction::LessEqual
                    } else {
                        wgpu::CompareFunction::Always
                    })
                );
            }
        }
    }

    #[test]
    fn extrusions_always_write_depth_and_debug_outlines_never_capture_it() {
        for capture_depth in [false, true] {
            let formats = (0x16, 0x30, false, capture_depth);
            let extrusion = depth_stencil(formats, true, false).unwrap();
            assert_eq!(extrusion.depth_write_enabled, Some(true));
            assert_eq!(extrusion.depth_compare, Some(wgpu::CompareFunction::Less));
            assert_eq!(depth_stencil(formats, false, true).unwrap().depth_write_enabled, Some(false));
        }
    }

    #[test]
    fn depth_capture_does_not_require_a_depth_attachment_in_standalone_hosts() {
        assert!(depth_stencil((0x16, 0, false, true), false, false).is_none());
    }

    #[test]
    fn native_surface_formats_are_linear_rgba_and_float_depth() {
        assert_eq!(texture_format_from_webgpu(0x16), wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(texture_format_from_webgpu(0x30), wgpu::TextureFormat::Depth32Float);
    }
}
