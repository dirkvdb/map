//! Offline, deterministic GPU readbacks: tests decide when replacement tiles arrive.

use anyhow::Result;
use image::RgbaImage;
use maplibre_native::WgpuLayerParameters;

pub fn device() -> Result<(wgpu::Device, wgpu::Queue)> {
    futures_lite::future::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance.request_adapter(&Default::default()).await?;
        Ok(adapter.request_device(&Default::default()).await?)
    })
}

pub fn parameters(zoom: f64) -> WgpuLayerParameters {
    let world = 512.0 * zoom.exp2();
    WgpuLayerParameters {
        width: 64.0,
        height: 64.0,
        latitude: 0.0,
        longitude: 0.0,
        zoom,
        projection: [
            1.0 / 32.0,
            0.0,
            0.0,
            0.0,
            0.0,
            -1.0 / 32.0,
            0.0,
            0.0,
            0.0,
            0.0,
            1.0,
            0.0,
            -world / 64.0,
            world / 64.0,
            0.0,
            1.0,
        ],
        color_format: 0x16,
        depth_format: 0,
        depth_test: false,
    }
}

pub fn pan_right(parameters: &mut WgpuLayerParameters, pixels: f64) {
    parameters.longitude -= pixels * 360.0 / (512.0 * parameters.zoom.exp2());
    parameters.projection[12] += pixels / (parameters.width / 2.0);
}

pub fn paint(device: &wgpu::Device, queue: &wgpu::Queue, render: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> Result<RgbaImage> {
    let output = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Zoom coverage regression output"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = output.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        render(&mut pass);
    }
    queue.submit([encoder.finish()]);
    crate::map_readback::GpuReadback::new(device).read(&output, device, queue)
}

pub fn assert_half_opacity(image: &RgbaImage) {
    assert!(
        image.pixels().all(|pixel| pixel[3].abs_diff(128) <= 1),
        "Zoom coverage must not disappear or double opacity"
    );
}
