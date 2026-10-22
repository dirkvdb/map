//! Shader-colored numeric rasters rendered by a native MapLibre custom layer.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use geo::Tile;
use inf::colormap::ColorMapPreset;
use maplibre_native::{WgpuCustomLayer, WgpuLayerParameters, texture_format_from_webgpu};

use crate::raster_source::{NumericTile, RasterSource};
use crate::raster_tiles::{MAX_PENDING_TILES, TileWorkers, VisibleTile, parent, parent_uv, tile_matrix, visible_tiles};

const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;
const MAX_CACHE_TILES: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DrawUniform {
    matrix: [[f32; 4]; 4],
    uv: [f32; 4],
    range: [f32; 2],
    opacity: f32,
    padding: f32,
}

struct TileTexture {
    values: wgpu::TextureView,
    valid: wgpu::TextureView,
    bytes: usize,
}

struct CachedTile {
    texture: Option<TileTexture>,
    used: u64,
}

struct DrawTile {
    sampled: Tile,
    uniform: wgpu::Buffer,
    bindings: wgpu::BindGroup,
}

struct RasterPipeline {
    formats: (u32, u32, bool),
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    palette: wgpu::Texture,
    palette_view: wgpu::TextureView,
    preset: Option<ColorMapPreset>,
}

pub struct RasterLayer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    source: Option<Arc<RasterSource>>,
    dpi: u8,
    opacity: f32,
    generation: u64,
    workers: TileWorkers,
    pending: HashSet<Tile>,
    coverage: HashSet<Tile>,
    cache: HashMap<Tile, CachedTile>,
    draws: HashMap<VisibleTile, DrawTile>,
    pipeline: Option<RasterPipeline>,
    frame: u64,
    cache_bytes: usize,
    waiting: bool,
    #[cfg(test)]
    requests: u64,
    #[cfg(test)]
    uploads: u64,
}

impl RasterLayer {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self> {
        Ok(Self {
            device,
            queue,
            source: None,
            dpi: 1,
            opacity: 1.0,
            generation: 0,
            workers: TileWorkers::new()?,
            pending: HashSet::new(),
            coverage: HashSet::new(),
            cache: HashMap::new(),
            draws: HashMap::new(),
            pipeline: None,
            frame: 0,
            cache_bytes: 0,
            waiting: false,
            #[cfg(test)]
            requests: 0,
            #[cfg(test)]
            uploads: 0,
        })
    }

    pub fn set_source(&mut self, source: Option<Arc<RasterSource>>, opacity: f32, dpi: u8) {
        let dpi = source
            .as_ref()
            .map_or(1, |source| if source.metadata().supports_dpi_ratio { dpi.clamp(1, 4) } else { 1 });
        let changed = self.source.as_ref().map(|s| s.metadata().id) != source.as_ref().map(|s| s.metadata().id) || dpi != self.dpi;
        if changed {
            self.generation = self.workers.reset();
            self.pending.clear();
            self.coverage.clear();
            self.cache.clear();
            self.draws.clear();
            self.cache_bytes = 0;
            self.waiting = false;
        }
        self.source = source;
        self.opacity = opacity;
        self.dpi = dpi;
    }

    /// Called by the renderer's existing frame pump, never from a tile worker.
    pub fn poll_tiles(&mut self) -> bool {
        let mut changed = false;
        for result in self.workers.completed() {
            if result.generation != self.generation {
                continue;
            }
            self.pending.remove(&result.tile);
            let texture = match result.data {
                Ok(Some(data)) => {
                    #[cfg(test)]
                    {
                        self.uploads += 1;
                    }
                    Some(upload_tile(&self.device, &self.queue, data))
                }
                Ok(None) => None,
                Err(error) => {
                    if let Some(source) = &self.source {
                        source.report_tile_error(&error);
                    }
                    log::error!("Numeric tile {:?}: {error:#}", result.tile);
                    None
                }
            };
            self.cache_bytes += texture.as_ref().map_or(0, |texture| texture.bytes);
            self.cache.insert(result.tile, CachedTile { texture, used: self.frame });
            changed = true;
        }
        // A newly selected band can briefly find the bounded job queue full of
        // cancelled work. Retry once workers drain it, even without a result.
        changed || (self.waiting && self.pending.is_empty())
    }

    pub fn is_loading(&self) -> bool {
        self.waiting || !self.pending.is_empty()
    }

    #[cfg(test)]
    pub fn tile_counts(&self) -> (u64, u64) {
        (self.requests, self.uploads)
    }

    fn request(&mut self, tile: Tile) {
        if self.cache.contains_key(&tile) || self.pending.contains(&tile) || self.pending.len() >= MAX_PENDING_TILES {
            return;
        }
        let Some(source) = &self.source else { return };
        if self.workers.request(self.generation, source.clone(), tile, self.dpi) {
            self.pending.insert(tile);
            #[cfg(test)]
            {
                self.requests += 1;
            }
        }
    }

    fn cached_ancestor(&self, mut tile: Tile) -> Option<Tile> {
        loop {
            // An empty tile is authoritative: do not cover its footprint with a
            // coarser ancestor, which might contain data outside this child.
            if let Some(cached) = self.cache.get(&tile) {
                return cached.texture.as_ref().map(|_| tile);
            }
            tile = parent(tile)?;
        }
    }

    fn render(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
        let Some(source) = self.source.clone() else { return };
        self.frame += 1;
        let formats = (parameters.color_format, parameters.depth_format, parameters.depth_test);
        if self.pipeline.as_ref().is_none_or(|pipeline| pipeline.formats != formats) {
            self.pipeline = Some(RasterPipeline::new(&self.device, formats));
            self.draws.clear();
        }
        let visible = visible_tiles(parameters, &source);
        let coverage = visible.iter().map(|tile| tile.canonical()).collect();
        if self.coverage != coverage {
            // Discard queued work for an old viewport, not cached data. Merely
            // moving within the same tiles or restyling keeps in-flight jobs.
            self.generation = self.workers.reset();
            self.pending.clear();
            self.coverage = coverage;
        }
        self.waiting = visible.iter().any(|tile| !self.cache.contains_key(&tile.canonical()));
        // Retain ancestors across navigation. Only request a new parent when
        // there is no cached fallback at all, and prioritize the visible tile.
        for tile in &visible {
            let tile = tile.canonical();
            if let Some(cached) = self.cache.get_mut(&tile) {
                cached.used = self.frame;
            }
            self.request(tile);
            if !self.cache.contains_key(&tile)
                && self.cached_ancestor(tile).is_none()
                && let Some(parent) = parent(tile)
            {
                self.request(parent);
            }
        }
        let Some(pipeline) = &mut self.pipeline else { return };
        if pipeline.preset != Some(source.palette()) {
            write_texture(&self.queue, &pipeline.palette, &source.palette_rgba(), 256, 1, 4);
            pipeline.preset = Some(source.palette());
        }
        pass.set_pipeline(&pipeline.pipeline);
        let visible_set: HashSet<_> = visible.iter().copied().collect();
        self.draws.retain(|tile, _| visible_set.contains(tile));
        for visible_tile in visible {
            let tile = visible_tile.canonical();
            let Some(sampled) = self.cached_ancestor(tile) else {
                self.draws.remove(&visible_tile);
                continue;
            };
            let Some(cached) = self.cache.get_mut(&sampled) else { continue };
            cached.used = self.frame;
            let Some(texture) = &cached.texture else { continue };
            let Some(pipeline) = &self.pipeline else { continue };
            let draw = self
                .draws
                .entry(visible_tile)
                .or_insert_with(|| pipeline.draw_tile(&self.device, sampled, texture));
            if draw.sampled != sampled {
                *draw = pipeline.draw_tile(&self.device, sampled, texture);
            }
            let range = source.range();
            self.queue.write_buffer(
                &draw.uniform,
                0,
                bytemuck::bytes_of(&DrawUniform {
                    matrix: tile_matrix(&parameters.projection, parameters.zoom, visible_tile),
                    uv: parent_uv(tile, sampled),
                    range: [range.0, range.1],
                    opacity: self.opacity,
                    padding: 0.0,
                }),
            );
            pass.set_bind_group(0, &draw.bindings, &[]);
            pass.draw(0..6, 0..1);
        }
        self.evict();
    }

    fn evict(&mut self) {
        while self.cache.len() > MAX_CACHE_TILES || self.cache_bytes > MAX_CACHE_BYTES {
            let oldest = self
                .cache
                .iter()
                .filter(|(_, tile)| tile.used != self.frame)
                .min_by_key(|(_, tile)| tile.used)
                .map(|(key, _)| *key);
            let Some(oldest) = oldest else { break };
            if let Some(tile) = self.cache.remove(&oldest) {
                self.cache_bytes -= tile.texture.as_ref().map_or(0, |texture| texture.bytes);
            }
        }
    }
}

/// MapLibre owns this host; the renderer retains the shared state to apply
/// style updates and pump completed tile jobs without rebuilding the layer.
pub struct RasterLayerHost(Rc<RefCell<RasterLayer>>);

impl RasterLayerHost {
    pub fn new(layer: Rc<RefCell<RasterLayer>>) -> Self {
        Self(layer)
    }
}

impl WgpuCustomLayer for RasterLayerHost {
    fn render(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
        self.0.borrow_mut().render(pass, parameters);
    }

    fn deinitialize(&mut self) {
        let mut layer = self.0.borrow_mut();
        layer.pipeline = None;
        layer.draws.clear();
        layer.cache.clear();
        layer.cache_bytes = 0;
    }
}

impl RasterPipeline {
    fn new(device: &wgpu::Device, formats: (u32, u32, bool)) -> Self {
        let texture_binding = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Numeric raster bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_binding(1),
                texture_binding(2),
                texture_binding(3),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Numeric raster layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Numeric raster shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("raster_layer.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Numeric raster"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: (formats.1 != 0).then(|| wgpu::DepthStencilState {
                format: texture_format_from_webgpu(formats.1),
                depth_write_enabled: Some(false),
                depth_compare: Some(if formats.2 {
                    wgpu::CompareFunction::LessEqual
                } else {
                    wgpu::CompareFunction::Always
                }),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: texture_format_from_webgpu(formats.0),
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let palette = create_texture(device, 256, 1, wgpu::TextureFormat::Rgba8Unorm);
        let palette_view = palette.create_view(&Default::default());
        Self {
            formats,
            pipeline,
            layout,
            palette,
            palette_view,
            preset: None,
        }
    }

    fn draw_tile(&self, device: &wgpu::Device, sampled: Tile, tile: &TileTexture) -> DrawTile {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Raster tile transform and style"),
            size: std::mem::size_of::<DrawUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Numeric raster tile"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&tile.values),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&tile.valid),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&self.palette_view),
                },
            ],
        });
        DrawTile { sampled, uniform, bindings }
    }
}

fn create_texture(device: &wgpu::Device, width: u32, height: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Raster data"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_texture(queue: &wgpu::Queue, texture: &wgpu::Texture, bytes: &[u8], width: u32, height: u32, bytes_per_pixel: u32) {
    queue.write_texture(
        texture.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * bytes_per_pixel),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

fn upload_tile(device: &wgpu::Device, queue: &wgpu::Queue, mut data: NumericTile) -> TileTexture {
    // A separate mask avoids relying on NaN comparisons in WGSL, where NaN
    // behavior is not guaranteed. Nodata never participates in color mapping.
    let valid: Vec<u8> = data
        .values
        .iter_mut()
        .map(|value| {
            if value.is_finite() {
                255
            } else {
                *value = 0.0;
                0
            }
        })
        .collect();
    let values = create_texture(device, data.width, data.height, wgpu::TextureFormat::R32Float);
    let mask = create_texture(device, data.width, data.height, wgpu::TextureFormat::R8Unorm);
    write_texture(queue, &values, bytemuck::cast_slice(&data.values), data.width, data.height, 4);
    write_texture(queue, &mask, &valid, data.width, data.height, 1);
    TileTexture {
        values: values.create_view(&Default::default()),
        valid: mask.create_view(&Default::default()),
        bytes: data.values.len() * 5,
    }
}
