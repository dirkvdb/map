//! Shared offscreen raster targets and a depth-preserving composite into a borrowed host pass.

use inf::colormap::{ColorMapDirection, ColorMapPreset, ProcessedColorMap};
use wgpu::{Device, Queue, RenderPass, TextureFormat, TextureView};

/// CPU palette metadata survives switches between host and offscreen pipelines.
#[derive(Default)]
pub(crate) struct PaletteOpacity {
    entries: Vec<((ColorMapPreset, ColorMapDirection), bool)>,
}

impl PaletteOpacity {
    pub(crate) fn is_opaque(&mut self, preset: ColorMapPreset, direction: ColorMapDirection) -> bool {
        let key = (preset, direction);
        if let Some((_, opaque)) = self.entries.iter().find(|(cached, _)| *cached == key) {
            return *opaque;
        }
        let palette = ProcessedColorMap::<256>::create_for_preset(preset, direction);
        let opaque = (0..=255).all(|index| palette.get_color_by_value(index).a == 255);
        self.entries.push((key, opaque));
        opaque
    }
}

/// Render raster surfaces once offscreen, then apply layer opacity to the visible surface.
///
/// Callers must clear color to transparent and depth to 1.0, store both attachments,
/// and use `BlendState::REPLACE` in offscreen pipelines. Targets are single-sampled.
pub(crate) struct RasterComposite {
    pub color: TextureView,
    pub depth: TextureView,
    size: [u32; 2],
    color_format: TextureFormat,
    host_depth: Option<TextureFormat>,
    opacity: wgpu::Buffer,
    bindings: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
}

impl RasterComposite {
    /// Reuses viewport-sized targets. The native ABI reports logical dimensions.
    pub(crate) fn prepare(target: Option<Self>, device: &Device, parameters: &maplibre_native::WgpuLayerParameters, pixel_ratio: f32) -> Self {
        let size = [parameters.width, parameters.height].map(|value| (value * f64::from(pixel_ratio)).round().max(1.0) as u32);
        let color = maplibre_native::texture_format_from_webgpu(parameters.color_format);
        let depth = (parameters.depth_format != 0).then(|| maplibre_native::texture_format_from_webgpu(parameters.depth_format));
        target
            .filter(|target| target.matches(size, color, depth))
            .unwrap_or_else(|| Self::new(device, size, color, depth))
    }

    /// Submits the shared surface before the native host submits its borrowed pass.
    pub(crate) fn render_surface(
        &self,
        device: &Device,
        queue: &Queue,
        parameters: &maplibre_native::WgpuLayerParameters,
        render: impl FnOnce(&mut RenderPass<'_>, &maplibre_native::WgpuLayerParameters),
    ) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Raster surface") });
        let mut surface_parameters = parameters.clone();
        // WebGPU C ABI WGPUTextureFormat_Depth32Float, matching our private depth target.
        surface_parameters.depth_format = 0x30;
        surface_parameters.depth_test = true;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Shared raster surface"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            render(&mut pass, &surface_parameters);
        }
        queue.submit([encoder.finish()]);
    }

    /// Creates targets for a nonzero framebuffer size and the host attachment formats.
    pub(crate) fn new(device: &Device, size: [u32; 2], color_format: TextureFormat, host_depth: Option<TextureFormat>) -> Self {
        let color = create_target(device, size, color_format, "Raster composite color");
        let depth = create_target(device, size, TextureFormat::Depth32Float, "Raster composite depth");
        let opacity = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Raster composite opacity"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = create_binding_layout(device);
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Raster composite bindings"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&color),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&depth),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: opacity.as_entire_binding(),
                },
            ],
        });
        let pipeline = create_pipeline(device, &layout, color_format, host_depth);
        Self {
            color,
            depth,
            size,
            color_format,
            host_depth,
            opacity,
            bindings,
            pipeline,
        }
    }

    /// Whether these targets and the pipeline can be reused for the host framebuffer.
    pub(crate) fn matches(&self, size: [u32; 2], color_format: TextureFormat, host_depth: Option<TextureFormat>) -> bool {
        self.size == size && self.color_format == color_format && self.host_depth == host_depth
    }

    /// Composites into a same-sized, single-sampled host pass with matching formats.
    ///
    /// Opacity must be in 0..=1. The host viewport and scissor must cover the framebuffer.
    /// This replaces the pass's pipeline and bind group 0. Submit before changing opacity
    /// again: queue writes to this shared uniform are not per-draw snapshots.
    pub(crate) fn composite(&self, pass: &mut RenderPass<'_>, queue: &Queue, opacity: f32) {
        queue.write_buffer(&self.opacity, 0, bytemuck::cast_slice(&[opacity, 0.0_f32, 0.0, 0.0]));
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bindings, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn create_target(device: &Device, size: [u32; 2], format: TextureFormat, label: &str) -> TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

fn create_binding_layout(device: &Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("Raster composite binding layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(16),
                },
                count: None,
            },
        ],
    })
}

fn create_pipeline(device: &Device, layout: &wgpu::BindGroupLayout, color_format: TextureFormat, host_depth: Option<TextureFormat>) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Raster composite pipeline layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Raster composite shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("raster_composite.wgsl").into()),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Raster composite pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        // Extrusions always test/write depth, like the direct extrusion pipelines,
        // even when the native pass disables depth testing for flat custom layers.
        depth_stencil: host_depth.map(|format| wgpu::DepthStencilState {
            format,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some(if host_depth.is_some() { "fragment" } else { "fragment_color" }),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: color_format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
#[path = "raster_composite_tests.rs"]
mod tests;
