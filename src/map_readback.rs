use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use image::RgbaImage;

// Writing RGBA shader output to a BGRA attachment performs the channel layout
// conversion on the GPU. Exact texel loads preserve colors, alpha and row order.
const COPY_SHADER: &str = r#"
@group(0) @binding(0) var source: texture_2d<f32>;

@vertex fn vertex_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}

@fragment fn fragment_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(source, vec2<i32>(position.xy), 0);
}
"#;

struct ReadbackTarget {
    texture: wgpu::Texture,
    buffer: wgpu::Buffer,
    stride: u32,
}

pub struct GpuReadback {
    pipeline: wgpu::RenderPipeline,
    target: Option<ReadbackTarget>,
}

impl GpuReadback {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Map BGRA conversion"),
            source: wgpu::ShaderSource::Wgsl(COPY_SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Map BGRA conversion"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, target: None }
    }

    pub fn read(&mut self, source: &wgpu::Texture, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<RgbaImage> {
        if self.target.as_ref().is_none_or(|target| target.texture.size() != source.size()) {
            let stride = source
                .width()
                .checked_mul(4)
                .and_then(|row| row.checked_next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT))
                .context("Map row size overflowed")?;
            let size = u64::from(stride) * u64::from(source.height());
            ensure!(size <= device.limits().max_buffer_size, "Map image exceeds the GPU readback buffer limit");
            self.target = Some(ReadbackTarget {
                texture: device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("Map BGRA target"),
                    size: source.size(),
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                }),
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Map readback buffer"),
                    size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                stride,
            });
        }
        let target = self.target.as_ref().context("Map readback target unavailable")?;
        let source_view = source.create_view(&Default::default());
        let target_view = target.texture.create_view(&Default::default());
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Map BGRA source"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source_view),
            }],
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Map readback encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Map BGRA conversion"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bindings, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            target.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &target.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.stride),
                    rows_per_image: Some(source.height()),
                },
            },
            source.size(),
        );
        let submission = queue.submit([encoder.finish()]);
        let (sender, receiver) = mpsc::channel();
        target.buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(30)),
            })
            .context("Could not wait for map readback")?;
        receiver.recv_timeout(Duration::from_secs(30)).context("Map readback timed out")??;
        let mapped = target.buffer.slice(..).get_mapped_range();
        let result = copy_rows(&mapped, source.width(), source.height(), target.stride);
        drop(mapped);
        target.buffer.unmap();
        result
    }
}

fn copy_rows(mapped: &[u8], width: u32, height: u32, stride: u32) -> Result<RgbaImage> {
    let row_bytes = usize::try_from(width)?.checked_mul(4).context("Map row size overflowed")?;
    let mut bytes = Vec::with_capacity(row_bytes.checked_mul(height as usize).context("Map image size overflowed")?);
    for row in mapped.chunks_exact(stride as usize).take(height as usize) {
        bytes.extend_from_slice(&row[..row_bytes]);
    }
    RgbaImage::from_raw(width, height, bytes).context("Invalid map image dimensions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_rows_preserve_pixels_without_copying_gpu_padding() -> Result<()> {
        let frame = copy_rows(&[34, 12, 255, 128, 99, 99, 99, 99, 255, 78, 56, 255, 99, 99, 99, 99], 1, 2, 8)?;
        assert_eq!(frame.as_raw(), &[34, 12, 255, 128, 255, 78, 56, 255]);
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_conversion_preserves_colors_alpha_and_row_order() -> Result<()> {
        let (device, queue) = futures_lite::future::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance.request_adapter(&Default::default()).await?;
            adapter
                .request_device(&Default::default())
                .await
                .context("Could not create the test GPU device")
        })?;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Known RGBA pixels"),
            size: wgpu::Extent3d {
                width: 1,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            texture.as_image_copy(),
            &[255, 12, 34, 128, 56, 78, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(2),
            },
            texture.size(),
        );
        let mut readback = GpuReadback::new(&device);
        for _ in 0..2 {
            let frame = readback.read(&texture, &device, &queue)?;
            assert_eq!(frame.as_raw(), &[34, 12, 255, 128, 255, 78, 56, 255]);
        }
        Ok(())
    }
}
