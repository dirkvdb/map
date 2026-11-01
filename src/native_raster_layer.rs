//! Preserve raster shaders while composing their color/depth in MapLibre's native pass.

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use maplibre_custom_layer::{FrameParameters, NativeCustomLayer};
use maplibre_raster_compositor::{NativeCompositor, Surface};

use crate::raster_layer::RasterLayer;
use crate::raster_parameters::WgpuLayerParameters;

pub(crate) struct NativeRasterLayerHost {
    raster: Arc<Mutex<RasterLayer>>,
    target: Option<SurfaceTarget>,
    compositor: NativeCompositor,
}

impl NativeRasterLayerHost {
    pub(crate) fn new(raster: Arc<Mutex<RasterLayer>>) -> Self {
        Self {
            raster,
            target: None,
            compositor: NativeCompositor::new(),
        }
    }

    fn prepare_surface(&mut self, parameters: &FrameParameters) -> Result<Surface> {
        let size = surface_size(parameters)?;
        let mut raster = self.raster.lock().unwrap_or_else(|poison| poison.into_inner());
        let device = raster.device().clone();
        let queue = raster.queue().clone();
        if self.target.as_ref().is_none_or(|target| target.color.size() != size) {
            self.target = Some(SurfaceTarget::new(&device, size)?);
        }
        let target = self.target.as_ref().context("Missing raster surface target")?;
        raster.set_pixel_ratio(parameters.pixel_ratio as f32);
        let encoder = target.render(&device, |pass| raster.render(pass, &raster_parameters(parameters)));
        let extruded = raster.is_extruded();
        drop(raster);
        target.read(&device, &queue, extruded, encoder)
    }
}

impl NativeCustomLayer for NativeRasterLayerHost {
    fn pre_render(&mut self, parameters: &FrameParameters) -> maplibre_custom_layer::Result<()> {
        let surface = self.prepare_surface(parameters).map_err(host_error)?;
        // SAFETY: these handles belong to this host's active pre-render callback.
        // All work on the separate wgpu device is complete before native upload.
        unsafe { self.compositor.prepare(parameters, surface) }.map_err(host_error)
    }

    fn render(&mut self, parameters: &FrameParameters) -> maplibre_custom_layer::Result<()> {
        // SAFETY: native owns the active pass; the compositor only records draws.
        unsafe { self.compositor.render(parameters) }.map_err(host_error)
    }

    fn context_lost(&mut self) -> maplibre_custom_layer::Result<()> {
        self.compositor.context_lost();
        Ok(())
    }

    fn deinitialize(&mut self) -> maplibre_custom_layer::Result<()> {
        // Keep shared raw raster tiles and workers across native style reloads.
        // SAFETY: native keeps the retirement context active during this callback.
        unsafe { self.compositor.deinitialize() }.map_err(host_error)
    }
}

fn host_error(error: impl std::fmt::Display) -> maplibre_custom_layer::Error {
    maplibre_custom_layer::Error::Host(error.to_string())
}

fn raster_parameters(native: &FrameParameters) -> WgpuLayerParameters {
    WgpuLayerParameters {
        width: native.width,
        height: native.height,
        latitude: native.latitude,
        longitude: native.longitude,
        zoom: native.zoom,
        projection: wgpu_projection(native.raster_projection_matrix),
        native_projection: Some(wgpu_projection(native.near_clipped_projection_matrix)),
        color_format: 0x16,
        depth_format: 0x30,
        depth_test: native.depth_test,
        capture_depth: true,
    }
}

fn wgpu_projection(mut matrix: [f64; 16]) -> [f64; 16] {
    for column in matrix.as_chunks_mut::<4>().0 {
        column[2] = 0.5 * (column[2] + column[3]);
    }
    matrix
}

fn surface_size(parameters: &FrameParameters) -> Result<wgpu::Extent3d> {
    let dimensions = [parameters.width, parameters.height].map(|value| (value * parameters.pixel_ratio).round());
    ensure!(parameters.pixel_ratio.is_finite() && parameters.pixel_ratio > 0.0, "Invalid native pixel ratio");
    ensure!(
        dimensions
            .into_iter()
            .all(|value| value.is_finite() && value >= 1.0 && value <= f64::from(u32::MAX)),
        "Invalid native surface extent"
    );
    Ok(wgpu::Extent3d {
        width: dimensions[0] as u32,
        height: dimensions[1] as u32,
        depth_or_array_layers: 1,
    })
}

struct SurfaceTarget {
    color: wgpu::Texture,
    depth: wgpu::Texture,
    readback: wgpu::Buffer,
    stride: u32,
    plane_bytes: u64,
}

impl SurfaceTarget {
    fn new(device: &wgpu::Device, size: wgpu::Extent3d) -> Result<Self> {
        ensure!(
            size.width <= device.limits().max_texture_dimension_2d && size.height <= device.limits().max_texture_dimension_2d,
            "Raster surface exceeds texture limits"
        );
        let stride = size
            .width
            .checked_mul(4)
            .and_then(|row| row.checked_next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT))
            .context("Raster row size overflow")?;
        let plane_bytes = u64::from(stride) * u64::from(size.height);
        let bytes = plane_bytes.checked_mul(2).context("Raster readback size overflow")?;
        ensure!(bytes <= device.limits().max_buffer_size, "Raster surface exceeds readback buffer limits");
        let texture = |format, label| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        Ok(Self {
            color: texture(wgpu::TextureFormat::Rgba8Unorm, "Native raster color"),
            depth: texture(wgpu::TextureFormat::Depth32Float, "Native raster depth"),
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Native raster color/depth readback"),
                size: bytes,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            stride,
            plane_bytes,
        })
    }

    fn render(&self, device: &wgpu::Device, render: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> wgpu::CommandEncoder {
        let color = self.color.create_view(&Default::default());
        let depth = self.depth.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Native raster preparation"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Native raster surface"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            render(&mut pass);
        }
        encoder
    }

    fn read(&self, device: &wgpu::Device, queue: &wgpu::Queue, extruded: bool, mut encoder: wgpu::CommandEncoder) -> Result<Surface> {
        for (texture, offset, aspect) in [
            (&self.color, 0, wgpu::TextureAspect::All),
            (&self.depth, self.plane_bytes, wgpu::TextureAspect::DepthOnly),
        ] {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    aspect,
                    ..texture.as_image_copy()
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset,
                        bytes_per_row: Some(self.stride),
                        rows_per_image: Some(texture.height()),
                    },
                },
                texture.size(),
            );
        }
        let submission = queue.submit([encoder.finish()]);
        let (sender, receiver) = mpsc::channel();
        self.readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(30)),
            })
            .context("Raster GPU readback wait failed")?;
        receiver.recv_timeout(Duration::from_secs(30)).context("Raster GPU readback timed out")??;
        let mapped = self.readback.slice(..).get_mapped_range();
        let color = packed_rows(&mapped[..self.plane_bytes as usize], self.color.width(), self.stride);
        let depth = packed_depth_rows(&mapped[self.plane_bytes as usize..], self.color.width(), self.stride);
        drop(mapped);
        self.readback.unmap();
        Ok(Surface {
            width: self.color.width(),
            height: self.color.height(),
            color,
            depth,
            extruded,
        })
    }
}

fn packed_rows(bytes: &[u8], width: u32, stride: u32) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut packed = Vec::with_capacity(row_bytes * (bytes.len() / stride as usize));
    for row in bytes.chunks_exact(stride as usize) {
        packed.extend_from_slice(&row[..row_bytes]);
    }
    packed
}

fn packed_depth_rows(bytes: &[u8], width: u32, stride: u32) -> Vec<f32> {
    let mut packed = Vec::with_capacity(width as usize * (bytes.len() / stride as usize));
    for row in bytes.chunks_exact(stride as usize) {
        packed.extend(row[..width as usize * 4].as_chunks::<4>().0.iter().map(|bytes| f32::from_le_bytes(*bytes)));
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_conversion_preserves_xy_and_w_and_maps_clip_endpoints() {
        let matrix = [1.0, 2.0, -4.0, 4.0, 5.0, 6.0, 0.0, 4.0, 9.0, 10.0, 4.0, 4.0, 13.0, 14.0, 0.0, 2.0];
        assert_eq!(
            wgpu_projection(matrix),
            [1.0, 2.0, 0.0, 4.0, 5.0, 6.0, 2.0, 4.0, 9.0, 10.0, 4.0, 4.0, 13.0, 14.0, 1.0, 2.0]
        );
    }

    #[test]
    fn readback_removes_padding_without_reordering_rows_or_channels() {
        assert_eq!(
            packed_rows(&[1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0], 1, 8),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn depth_readback_decodes_rows_directly_without_padding() {
        let bytes: Vec<u8> = [0.25_f32, 99.0, 0.75, 99.0].into_iter().flat_map(f32::to_le_bytes).collect();
        assert_eq!(packed_depth_rows(&bytes, 1, 8), vec![0.25, 0.75]);
    }
}
