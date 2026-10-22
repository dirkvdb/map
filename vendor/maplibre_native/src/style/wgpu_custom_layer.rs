use crate::bridge::ffi;

/// Camera and attachment information for a custom WebGPU layer.
pub use ffi::WgpuLayerParameters;

/// A custom layer drawn inside MapLibre's active WebGPU render pass.
///
/// The renderer owns the host and invokes it on its thread. Resources must use
/// the same device supplied to `ImageRenderer::set_device_queue`. Tile loading
/// and repaint scheduling remain the application's responsibility.
pub trait WgpuCustomLayer: 'static {
    /// Records commands without ending or retaining the borrowed render pass.
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
    // SAFETY: the native CustomLayerHost supplies its current shim encoder only
    // during render(), on the renderer thread. The borrow cannot escape.
    unsafe { webgpu_shim::with_render_pass(encoder, |pass| host.0.render(pass, parameters)) };
}

pub(crate) fn wgpu_layer_deinitialize(host: &mut WgpuLayerHost) {
    host.0.deinitialize();
}
