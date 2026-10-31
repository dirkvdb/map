//! Shader-colored numeric rasters rendered by a native MapLibre custom layer.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use geo::Tile;
use inf::colormap::{ColorMapDirection, ColorMapPreset};
use maplibre_native::{WgpuCustomLayer, WgpuLayerParameters, texture_format_from_webgpu};
use wgpu::util::DeviceExt;

use crate::multiband_layer::MultibandLayer;
use crate::raster_composite::{PaletteOpacity, RasterComposite};
use crate::raster_extrusion_animation::ExtrusionAnimation;
use crate::raster_playback::{PlaybackFrame, PlaybackRequest};
use crate::raster_source::{NumericTile, RasterSource};
use crate::raster_tiles::{MAX_PENDING_TILES, TileWorkers, VisibleTile, extrusion_view, parent_uv, tile_matrix, transition_tiles, visible_tiles};

#[path = "raster_extrusion_geometry.rs"]
pub(crate) mod geometry;

const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;
const MAX_CACHE_TILES: usize = 256;
const BAND_FADE: Duration = Duration::from_millis(180);

// Corner bits encode x/y/z, matching raster_extrusion.wgsl. No bottom face.
// Outward faces project clockwise in the map's south-positive tile coordinates.
const EXTRUSION_INDICES: [u16; 30] = [
    4, 5, 6, 6, 5, 7, // top
    0, 1, 4, 4, 1, 5, // north
    1, 3, 5, 5, 3, 7, // east
    3, 2, 7, 7, 2, 6, // south
    2, 0, 6, 6, 0, 4, // west
];

#[cfg(test)]
#[path = "raster_layer_zoom_tests.rs"]
mod zoom_tests;

#[cfg(test)]
#[path = "raster_extrusion_tests.rs"]
pub(crate) mod extrusion_tests;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DrawUniform {
    matrix: [[f32; 4]; 4],
    uv: [f32; 4],
    previous_uv: [f32; 4],
    range: [f32; 2],
    previous_range: [f32; 2],
    scale_offset: [f32; 2],
    previous_scale_offset: [f32; 2],
    opacity: f32,
    blend: f32,
    tile_size: f32,
    previous_extrusion_height: f32,
    tile_id: [u32; 4],
    clamp_colors: [u32; 4],
    extrusion: [f32; 4],
    // X/Y strides, traversal flags, previous/current averaging bits.
    geometry: [u32; 4],
}

struct TileTexture {
    values: wgpu::TextureView,
    valid: wgpu::TextureView,
    bytes: usize,
    size: [u32; 2],
}

struct CachedTile {
    texture: Option<TileTexture>,
    used: u64,
}

struct DrawTile {
    sampled: (Option<Tile>, Option<Tile>),
    uniform: wgpu::Buffer,
    bindings: wgpu::BindGroup,
}

struct RasterPipeline {
    formats: (u32, u32, bool),
    pipeline: wgpu::RenderPipeline,
    extrusion_pipeline: wgpu::RenderPipeline,
    extrusion_indices: wgpu::Buffer,
    debug_pipeline: Option<wgpu::RenderPipeline>,
    layout: wgpu::BindGroupLayout,
    palettes: [wgpu::Texture; 2],
    palette_views: [wgpu::TextureView; 2],
    presets: [Option<(ColorMapPreset, ColorMapDirection)>; 2],
    palette_opaque: [bool; 2],
    empty: TileTexture,
}

struct PreviousRaster {
    source: Arc<RasterSource>,
    cache: HashMap<Tile, CachedTile>,
}

impl PreviousRaster {
    fn sampled(&self, mut tile: Tile) -> Option<Tile> {
        loop {
            if let Some(cached) = self.cache.get(&tile) {
                return cached.texture.as_ref().map(|_| tile);
            }
            tile = tile.parent()?;
        }
    }
}

pub struct RasterLayer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    source: Option<Arc<RasterSource>>,
    dpi: u8,
    opacity: f32,
    pixel_ratio: f32,
    composite: Option<RasterComposite>,
    compositing: bool,
    palette_opacity: PaletteOpacity,
    generation: u64,
    workers: TileWorkers,
    pending: HashSet<Tile>,
    coverage: HashSet<Tile>,
    cache: HashMap<Tile, CachedTile>,
    draws: HashMap<VisibleTile, DrawTile>,
    displayed: Vec<VisibleTile>,
    pipeline: Option<RasterPipeline>,
    frame: u64,
    cache_bytes: usize,
    waiting: bool,
    previous: Option<PreviousRaster>,
    fade_started: Option<Instant>,
    queued: Option<(Arc<RasterSource>, u8)>,
    reduce_motion: bool,
    extrusion_animation: ExtrusionAnimation,
    debug: bool,
    multiband: Option<MultibandLayer>,
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
            pixel_ratio: 1.0,
            composite: None,
            compositing: false,
            palette_opacity: PaletteOpacity::default(),
            generation: 0,
            workers: TileWorkers::new()?,
            pending: HashSet::new(),
            coverage: HashSet::new(),
            cache: HashMap::new(),
            draws: HashMap::new(),
            displayed: Vec::new(),
            pipeline: None,
            frame: 0,
            cache_bytes: 0,
            waiting: false,
            previous: None,
            fade_started: None,
            queued: None,
            reduce_motion: false,
            extrusion_animation: ExtrusionAnimation::default(),
            debug: false,
            multiband: None,
            #[cfg(test)]
            requests: 0,
            #[cfg(test)]
            uploads: 0,
        })
    }

    pub fn set_pixel_ratio(&mut self, pixel_ratio: f32) {
        self.pixel_ratio = pixel_ratio;
    }

    pub fn set_source(&mut self, source: Option<Arc<RasterSource>>, opacity: f32, dpi: u8) {
        let dpi = source
            .as_ref()
            .map_or(1, |source| if source.metadata().supports_dpi_ratio { dpi.clamp(1, 4) } else { 1 });
        let changed = self.source.as_ref().map(|s| s.metadata().id) != source.as_ref().map(|s| s.metadata().id) || dpi != self.dpi;
        self.opacity = opacity;
        if changed && source.is_some() && self.fade_started.is_some() && !self.reduce_motion {
            // Two slots are enough: finish the visible fade before recycling
            // its outgoing slot. Coalesce rapid selections to the newest band.
            self.queued = source.map(|source| (source, dpi));
            return;
        }
        self.queued = None;
        if changed {
            self.reset_for_source_change(source.is_some());
        } else if !self.reduce_motion
            && let (Some(previous), Some(target)) = (&self.source, &source)
        {
            self.extrusion_animation.retarget(
                (previous.is_extruded(), previous.extrusion_height()),
                (target.is_extruded(), target.extrusion_height()),
                Instant::now(),
            );
        }
        self.source = source;
        self.dpi = dpi;
    }

    fn reset_for_source_change(&mut self, has_source: bool) {
        self.extrusion_animation = ExtrusionAnimation::default();
        if has_source {
            // During loading, replace only the incoming slot, even across
            // datasets. The outgoing slot keeps its own data and style.
            if self.previous.is_none() && !self.cache.is_empty() {
                self.previous = self.source.clone().map(|source| PreviousRaster {
                    source,
                    cache: std::mem::take(&mut self.cache),
                });
            }
        } else {
            self.previous = None;
        }
        self.fade_started = None;
        self.generation = self.workers.reset();
        self.pending.clear();
        self.coverage.clear();
        self.cache.clear();
        self.draws.clear();
        self.displayed.clear();
        self.cache_bytes = 0;
        self.waiting = false;
    }

    pub fn set_playback(&mut self, request: Option<PlaybackRequest>, opacity: f32) -> Result<()> {
        if let Some(request) = request {
            if self.multiband.is_none() {
                self.reset_for_source_change(false);
                self.multiband = Some(MultibandLayer::new(self.device.clone(), self.queue.clone())?);
            }
            if let Some(layer) = &mut self.multiband {
                layer.set_reduce_motion(self.reduce_motion);
                layer.set_debug(self.debug);
                layer.set_playback(request, opacity);
            }
        } else if self.multiband.take().is_some() {
            self.reset_for_source_change(false);
        }
        Ok(())
    }

    pub fn playback_frame(&self) -> Option<PlaybackFrame> {
        self.multiband.as_ref().and_then(MultibandLayer::playback_frame)
    }

    pub fn set_reduce_motion(&mut self, reduce: bool) {
        self.reduce_motion = reduce;
        if reduce {
            self.extrusion_animation = ExtrusionAnimation::default();
        }
        if let Some(layer) = &mut self.multiband {
            layer.set_reduce_motion(reduce);
        }
    }

    pub fn set_debug(&mut self, debug: bool) {
        self.debug = debug;
        if let Some(layer) = &mut self.multiband {
            layer.set_debug(debug);
        }
    }

    /// Called by the renderer's existing frame pump, never from a tile worker.
    pub fn poll_tiles(&mut self) -> bool {
        if let Some(layer) = &mut self.multiband {
            return layer.poll_tiles();
        }
        let mut changed = false;
        for result in self.workers.completed() {
            if result.generation != self.generation {
                continue;
            }
            self.pending.remove(&result.tile);
            #[cfg(test)]
            if matches!(&result.data, Ok(Some(_))) {
                self.uploads += 1;
            }
            let texture = self.completed_texture(result.tile, result.data);
            self.cache_bytes += texture.as_ref().map_or(0, |texture| texture.bytes);
            self.cache.insert(result.tile, CachedTile { texture, used: self.frame });
            changed = true;
        }
        // A newly selected band can briefly find the bounded job queue full of
        // cancelled work. Retry once workers drain it, even without a result.
        changed || self.fade_started.is_some() || self.extrusion_animation.is_active() || (self.waiting && self.pending.is_empty())
    }

    fn completed_texture(&self, tile: Tile, data: Result<Option<NumericTile>>) -> Option<TileTexture> {
        match data {
            Ok(Some(data)) => Some(upload_tile(&self.device, &self.queue, data)),
            Ok(None) => None,
            Err(error) => {
                if let Some(source) = &self.source {
                    source.report_tile_error(&error);
                }
                log::error!("Numeric tile {tile:?}: {error:#}");
                None
            }
        }
    }

    pub fn is_loading(&self) -> bool {
        self.multiband.as_ref().map_or_else(
            || self.waiting || !self.pending.is_empty() || self.previous.is_some(),
            MultibandLayer::is_loading,
        )
    }

    #[cfg(test)]
    pub fn tile_counts(&self) -> (u64, u64) {
        (self.requests, self.uploads)
    }

    #[cfg(test)]
    pub fn pending_fallback_count(&self) -> usize {
        self.pending.iter().filter(|tile| !self.coverage.contains(tile)).count()
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
            tile = tile.parent()?;
        }
    }

    fn render(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
        if let Some(layer) = &mut self.multiband {
            layer.render_scaled(pass, parameters, self.pixel_ratio);
            return;
        }
        let now = Instant::now();
        self.extrusion_animation.advance(now);
        self.finish_fade(now);
        let extruded = self
            .source
            .as_ref()
            .is_some_and(|source| self.extrusion_animation.height(effective_height(source)) > 0.0)
            || self.previous.as_ref().is_some_and(|previous| effective_height(&previous.source) > 0.0);
        if !extruded || (parameters.depth_format != 0 && !self.needs_compositing()) {
            self.composite = None;
            self.render_content(pass, parameters, now);
            self.draw_debug_tiles(pass, parameters);
            return;
        }
        let target = RasterComposite::prepare(self.composite.take(), &self.device, parameters, self.pixel_ratio);
        let device = self.device.clone();
        let queue = self.queue.clone();
        self.compositing = true;
        target.render_surface(&device, &queue, parameters, |surface, parameters| self.render_content(surface, parameters, now));
        self.compositing = false;
        target.composite(pass, &self.queue, self.opacity);
        self.draw_debug_tiles(pass, parameters);
        self.composite = Some(target);
    }

    fn needs_compositing(&mut self) -> bool {
        // Missing endpoints can fade nodata in/out even with opaque palettes.
        if self.opacity < 1.0 || self.previous.is_some() {
            return true;
        }
        self.source
            .as_ref()
            .is_some_and(|source| !self.palette_opacity.is_opaque(source.palette(), source.color_map_direction()))
    }

    fn render_content(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters, now: Instant) {
        let Some(source) = self.source.clone() else { return };
        self.frame += 1;
        self.prepare_pipeline(parameters);
        let visible = self.view_tiles(parameters, &source, None);
        self.update_coverage(&visible, now);
        let blend = self.fade_started.map_or(if self.previous.is_some() { 0.0 } else { 1.0 }, |start| {
            (now.duration_since(start).as_secs_f32() / BAND_FADE.as_secs_f32()).clamp(0.0, 1.0)
        });
        // Before the fade, only exact incoming tiles can make progress: parent
        // tiles are invisible and would delay readiness on the shared workers.
        let loading_replacement = self.previous.is_some() && self.fade_started.is_none();
        self.request_visible_tiles(&visible, loading_replacement);
        // Loading only draws the outgoing grid. Once blending starts, draw both
        // footprints on the finer grid so either slot can sample ancestors.
        let visible = self.previous.as_ref().map_or(visible, |previous| {
            if loading_replacement {
                self.view_tiles(parameters, &previous.source, None)
            } else {
                self.view_tiles(parameters, &source, Some(&previous.source))
            }
        });
        // Swap zoom grids as a unit: drawing parents and children together
        // would double opacity and let old data show through new nodata.
        let ready = !self.waiting || self.previous.is_some();
        let visible = retained_zoom_tiles(&mut self.displayed, visible, ready);
        self.draw_visible_tiles(pass, parameters, &source, &visible, blend);
        self.evict();
    }

    fn view_tiles(&self, parameters: &WgpuLayerParameters, source: &RasterSource, previous: Option<&RasterSource>) -> Vec<VisibleTile> {
        let height = self.extrusion_animation.maximum_height(effective_height(source));
        let previous_bounds = previous.map_or((source, height), |source| (source, effective_height(source)));
        let sources = [(source, height), previous_bounds];
        if sources.iter().any(|(_, height)| *height > 0.0) {
            // Target request detail must not depend on temporary cached ancestors.
            extrusion_view::visible_tiles(parameters, &sources, MAX_CACHE_TILES)
        } else {
            previous.map_or_else(|| visible_tiles(parameters, source), |previous| transition_tiles(parameters, source, previous))
        }
    }

    fn draw_tiles(&self, parameters: &WgpuLayerParameters, source: &RasterSource, visible: &[VisibleTile], blend: f32) -> Vec<VisibleTile> {
        let height = self.extrusion_animation.height(effective_height(source));
        let previous = self
            .previous
            .as_ref()
            .map_or((source, height), |previous| (previous.source.as_ref(), effective_height(&previous.source)));
        let sources = [(source, height), previous];
        let mut tiles = visible.to_vec();
        if sources.iter().any(|(_, height)| *height > 0.0) {
            let height = extrusion_view::maximum_height(parameters, &sources);
            tiles.retain(|tile| extrusion_view::tile_visible(parameters, *tile, height));

            if self.opaque_draw(blend) {
                geometry::sort_tiles(parameters, &mut tiles);
            }
        }
        tiles
    }

    fn draw_opacity(&self) -> f32 {
        if self.compositing { 1.0 } else { self.opacity }
    }

    fn opaque_draw(&self, blend: f32) -> bool {
        self.draw_opacity() >= 1.0
            && (blend <= 0.0 || blend >= 1.0)
            && self
                .pipeline
                .as_ref()
                .is_some_and(|pipeline| pipeline.palette_opaque[usize::from(blend <= 0.0)])
    }

    fn finish_fade(&mut self, now: Instant) {
        if self
            .fade_started
            .is_some_and(|start| self.reduce_motion || now.duration_since(start) >= BAND_FADE)
        {
            self.previous = None;
            self.fade_started = None;
            self.draws.clear();
            if let Some((source, dpi)) = self.queued.take() {
                self.set_source(Some(source), self.opacity, dpi);
            }
        }
    }

    fn prepare_pipeline(&mut self, parameters: &WgpuLayerParameters) {
        let formats = (parameters.color_format, parameters.depth_format, parameters.depth_test);
        if self.pipeline.as_ref().is_none_or(|pipeline| pipeline.formats != formats) {
            self.pipeline = Some(RasterPipeline::new(&self.device, formats));
            self.draws.clear();
        }
    }

    fn update_coverage(&mut self, visible: &[VisibleTile], now: Instant) {
        let coverage = visible.iter().map(|tile| tile.canonical()).collect();
        if self.coverage != coverage {
            // Discard queued work for an old viewport, not cached data. Merely
            // moving within the same tiles or restyling keeps in-flight jobs.
            self.generation = self.workers.reset();
            self.pending.clear();
            self.coverage = coverage;
        }
        self.waiting = visible.iter().any(|tile| !self.cache.contains_key(&tile.canonical()));
        if !self.waiting && self.previous.is_some() && self.fade_started.is_none() {
            if self.reduce_motion {
                self.previous = None;
                self.draws.clear();
            } else {
                self.fade_started = Some(now);
            }
        }
    }

    fn request_visible_tiles(&mut self, visible: &[VisibleTile], loading_replacement: bool) {
        // Retain ancestors across navigation and initial progressive loading.
        for tile in visible {
            let tile = tile.canonical();
            if let Some(cached) = self.cache.get_mut(&tile) {
                cached.used = self.frame;
            }
            self.request(tile);
            if !loading_replacement
                && !self.cache.contains_key(&tile)
                && self.cached_ancestor(tile).is_none()
                && let Some(parent) = tile.parent()
            {
                self.request(parent);
            }
        }
    }

    fn draw_visible_tiles(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        parameters: &WgpuLayerParameters,
        source: &RasterSource,
        visible: &[VisibleTile],
        blend: f32,
    ) {
        let Some(pipeline) = &mut self.pipeline else { return };
        let previous_source = self.previous.as_ref().map_or(source, |previous| &previous.source);
        for (ix, source) in [source, previous_source].into_iter().enumerate() {
            let key = palette_key(source);
            if pipeline.presets[ix] != Some(key) {
                let rgba = source.palette_rgba();
                pipeline.palette_opaque[ix] = rgba.as_chunks::<4>().0.iter().all(|color| color[3] == 255);
                write_texture(&self.queue, &pipeline.palettes[ix], &rgba, 256, 1, 4);
                pipeline.presets[ix] = Some(key);
            }
        }
        pass.set_pipeline(&pipeline.pipeline);
        let visible_set: HashSet<_> = visible.iter().copied().collect();
        self.draws.retain(|tile, _| visible_set.contains(tile));
        for visible_tile in self.draw_tiles(parameters, source, visible, blend) {
            self.draw_visible_tile(pass, parameters, source, visible_tile, blend);
        }
    }

    fn draw_visible_tile(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        parameters: &WgpuLayerParameters,
        source: &RasterSource,
        visible_tile: VisibleTile,
        blend: f32,
    ) {
        let (opaque, opacity) = (self.opaque_draw(blend), self.draw_opacity());
        let current_height = self.extrusion_animation.height(effective_height(source));
        let previous_height = self.previous.as_ref().map_or(current_height, |previous| effective_height(&previous.source));
        let tile = visible_tile.canonical();
        let sampled = self.cached_ancestor(tile);
        let previous_sampled = self.previous.as_ref().and_then(|previous| previous.sampled(tile));
        if sampled.is_none() && previous_sampled.is_none() {
            self.draws.remove(&visible_tile);
            return;
        }
        let texture = sampled.and_then(|tile| self.cache.get_mut(&tile)).and_then(|cached| {
            cached.used = self.frame;
            cached.texture.as_ref()
        });
        let previous_texture = previous_sampled.and_then(|tile| self.previous.as_ref()?.cache.get(&tile)?.texture.as_ref());
        let Some(pipeline) = &self.pipeline else { return };
        let pair = (sampled, previous_sampled);
        let draw = self
            .draws
            .entry(visible_tile)
            .or_insert_with(|| pipeline.draw_tile(&self.device, pair, texture, previous_texture));
        if draw.sampled != pair {
            *draw = pipeline.draw_tile(&self.device, pair, texture, previous_texture);
        }
        let previous_source = self.previous.as_ref().map_or(source, |previous| &previous.source);
        let uv = sampled.map_or([0.0, 0.0, 1.0, 1.0], |sampled| parent_uv(tile, sampled));
        let previous_uv = previous_sampled.map_or([0.0, 0.0, 1.0, 1.0], |sampled| parent_uv(tile, sampled));
        let grid = extrusion_grid(texture.map(|t| t.size), uv, previous_texture.map(|t| t.size), previous_uv, blend);
        let height = previous_height * (1.0 - blend) + current_height * blend;
        let mut extrusion = extrusion_uniform(parameters, source, grid, current_height);
        extrusion[3] = height;
        pass.set_pipeline(if height > 0.0 { &pipeline.extrusion_pipeline } else { &pipeline.pipeline });
        let range = source.range();
        let previous_range = previous_source.range();
        let [scale_offset, previous_scale_offset] = [source, previous_source].map(|source| {
            let scale = source.scale();
            [scale.scale as f32, scale.offset as f32]
        });
        let previous_height = extrusion_uniform(parameters, previous_source, grid, previous_height)[2];
        let matrix = tile_matrix(&parameters.projection, parameters.zoom, visible_tile);
        let mut volume = extrusion;
        volume[2] = previous_height * (1.0 - blend) + extrusion[2] * blend;
        let averaging = [
            previous_source.is_extruded() && previous_source.is_extrusion_averaged(),
            current_height > 0.0 && source.is_extrusion_averaged(),
        ];
        let flags = u32::from(averaging[0]) | (u32::from(averaging[1]) << 1);
        let geometry = geometry::with_averaging(geometry::plan(&matrix, [parameters.width, parameters.height], volume, opaque), flags, blend);

        self.queue.write_buffer(
            &draw.uniform,
            0,
            bytemuck::bytes_of(&DrawUniform {
                matrix,
                uv,
                previous_uv,
                range: [range.0, range.1],
                previous_range: [previous_range.0, previous_range.1],
                scale_offset,
                previous_scale_offset,
                opacity,
                blend,
                tile_size: (512.0 * parameters.zoom.exp2() / f64::from(1_u32 << tile.z)) as f32,
                previous_extrusion_height: previous_height,
                tile_id: [tile.z as u32, tile.x as u32, tile.y as u32, 0],
                extrusion,
                geometry,
                clamp_colors: [u32::from(source.clamp_colors()), u32::from(previous_source.clamp_colors()), 0, 0],
            }),
        );
        pass.set_bind_group(0, &draw.bindings, &[]);
        draw_pixels(pass, extrusion, geometry, &pipeline.extrusion_indices);
    }

    fn draw_debug_tiles(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
        if self.debug
            && let Some(pipeline) = &mut self.pipeline
        {
            let debug_pipeline = pipeline.debug_pipeline.get_or_insert_with(|| {
                create_pipeline(
                    &self.device,
                    (parameters.color_format, parameters.depth_format, parameters.depth_test),
                    &pipeline.layout,
                    "debug_fragment",
                )
            });
            // Draw after every raster quad so neighboring tiles cannot cover
            // the grid or labels. Debug visibility is independent of opacity.
            pass.set_pipeline(debug_pipeline);
            for draw in self.draws.values() {
                pass.set_bind_group(0, &draw.bindings, &[]);
                pass.draw(0..6, 0..1);
            }
        }
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
        layer.composite = None;
        layer.draws.clear();
        layer.displayed.clear();
        layer.cache.clear();
        layer.cache_bytes = 0;
        layer.previous = None;
        layer.fade_started = None;
        layer.queued = None;
        layer.extrusion_animation = ExtrusionAnimation::default();
    }
}

fn retained_zoom_tiles(displayed: &mut Vec<VisibleTile>, target: Vec<VisibleTile>, ready: bool) -> Vec<VisibleTile> {
    let same_zoom = displayed.first().map(|tile| tile.z) == target.first().map(|tile| tile.z);
    if displayed.is_empty() || target.is_empty() || same_zoom || ready {
        displayed.clone_from(&target);
        target
    } else {
        displayed.clone()
    }
}

impl RasterPipeline {
    fn new(device: &wgpu::Device, formats: (u32, u32, bool)) -> Self {
        let texture_binding = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
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
                texture_binding(4),
                texture_binding(5),
                texture_binding(6),
            ],
        });
        let pipeline = create_pipeline(device, formats, &layout, "fragment");
        let palettes = std::array::from_fn(|_| create_texture(device, 256, 1, wgpu::TextureFormat::Rgba8Unorm));
        let palette_views = palettes.each_ref().map(|palette| palette.create_view(&Default::default()));
        Self {
            formats,
            pipeline,
            extrusion_pipeline: create_pipeline(device, formats, &layout, "extruded_fragment"),
            extrusion_indices: create_extrusion_indices(device),
            debug_pipeline: None,
            layout,
            palettes,
            palette_views,
            presets: [None; 2],
            palette_opaque: [false; 2],
            empty: TileTexture {
                values: create_texture(device, 1, 1, wgpu::TextureFormat::R32Float).create_view(&Default::default()),
                valid: create_texture(device, 1, 1, wgpu::TextureFormat::R8Unorm).create_view(&Default::default()),
                bytes: 5,
                size: [1, 1],
            },
        }
    }

    fn draw_tile(&self, device: &wgpu::Device, sampled: (Option<Tile>, Option<Tile>), tile: Option<&TileTexture>, previous: Option<&TileTexture>) -> DrawTile {
        // WebGPU zero-initializes the empty mask, making absent coverage fully
        // transparent without special shader branches or stale bindings.
        let tile = tile.unwrap_or(&self.empty);
        let previous = previous.unwrap_or(&self.empty);
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
                    resource: wgpu::BindingResource::TextureView(&self.palette_views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&previous.values),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&previous.valid),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&self.palette_views[1]),
                },
            ],
        });
        DrawTile { sampled, uniform, bindings }
    }
}

fn palette_key(source: &RasterSource) -> (ColorMapPreset, ColorMapDirection) {
    (source.palette(), source.color_map_direction())
}

fn create_pipeline(device: &wgpu::Device, formats: (u32, u32, bool), layout: &wgpu::BindGroupLayout, fragment: &str) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Numeric raster layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let extruded = fragment == "extruded_fragment";
    let entry_points = if extruded {
        concat!(include_str!("raster_extrusion_sample.wgsl"), "\n", include_str!("raster_extrusion.wgsl"))
    } else {
        concat!(include_str!("raster_layer.wgsl"), "\n", include_str!("raster_debug.wgsl"))
    };
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(if extruded {
            "Numeric raster extrusion shader"
        } else {
            "Numeric raster shader"
        }),
        source: wgpu::ShaderSource::Wgsl(format!("{}\n{}", include_str!("raster_sample.wgsl"), entry_points).into()),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(if extruded { "Numeric raster extrusion" } else { "Numeric raster" }),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some(if extruded { "extruded_vertex" } else { "vertex" }),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            front_face: wgpu::FrontFace::Cw,
            cull_mode: extruded.then_some(wgpu::Face::Back),
            ..Default::default()
        },
        depth_stencil: (formats.1 != 0).then(|| wgpu::DepthStencilState {
            format: texture_format_from_webgpu(formats.1),
            depth_write_enabled: Some(extruded),
            // Keep the first surface on depth ties; farther walls must not
            // overwrite tops after quantization in native depth attachments.
            depth_compare: Some(if extruded {
                wgpu::CompareFunction::Less
            } else if formats.2 && fragment == "fragment" {
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
            entry_point: Some(fragment),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: texture_format_from_webgpu(formats.0),
                blend: Some(if extruded {
                    wgpu::BlendState::REPLACE
                } else {
                    wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
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
        size: [data.width, data.height],
    }
}

fn effective_height(source: &RasterSource) -> f32 {
    if source.is_extruded() { source.extrusion_height() } else { 0.0 }
}

fn extrusion_grid(current: Option<[u32; 2]>, uv: [f32; 4], previous: Option<[u32; 2]>, previous_uv: [f32; 4], blend: f32) -> [u32; 2] {
    std::array::from_fn(|axis| {
        let a = current.filter(|_| blend > 0.0).map_or(1.0, |size| size[axis] as f32 * uv[axis + 2]);
        let b = previous.filter(|_| blend < 1.0).map_or(1.0, |size| size[axis] as f32 * previous_uv[axis + 2]);
        a.max(b).round().max(1.0) as u32
    })
}

pub(crate) fn extrusion_uniform(parameters: &WgpuLayerParameters, source: &RasterSource, grid: [u32; 2], height: f32) -> [f32; 4] {
    [
        grid[0] as f32,
        grid[1] as f32,
        crate::raster_tiles::extrusion_height::maximum(parameters, source, height) as f32,
        height,
    ]
}

pub(crate) fn create_extrusion_indices(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Raster extrusion corners"),
        contents: bytemuck::cast_slice(&EXTRUSION_INDICES),
        usage: wgpu::BufferUsages::INDEX,
    })
}

pub(crate) fn draw_pixels(pass: &mut wgpu::RenderPass<'_>, extrusion: [f32; 4], geometry: [u32; 4], indices: &wgpu::Buffer) {
    if extrusion[3] > 0.0 {
        pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed(0..EXTRUSION_INDICES.len() as u32, 0, 0..geometry::instance_count(extrusion, geometry));
    } else {
        pass.draw(0..6, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_keys_invalidate_current_and_outgoing_slots_independently_on_reversal() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("palette.tif");
        crate::raster_source::test_support::write_byte_raster(&path)?;
        let regular = RasterSource::open(&path)?.remove(0);
        let reversed = regular.with_color_map_direction(ColorMapDirection::Reversed)?;
        let cached = [Some(palette_key(&regular)); 2];
        for (sources, expected) in [([&*reversed, &*regular], [true, false]), ([&*regular, &*reversed], [false, true])] {
            let needs_upload: [bool; 2] = std::array::from_fn(|ix| cached[ix] != Some(palette_key(sources[ix])));
            assert_eq!(needs_upload, expected);
        }
        let restored = reversed.with_color_map_direction(ColorMapDirection::Regular)?;
        assert!(palette_key(&restored) == palette_key(&regular));
        Ok(())
    }

    #[test]
    fn palette_keys_reuse_range_only_snapshots_but_invalidate_palette_changes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("palette.tif");
        crate::raster_source::test_support::write_byte_raster(&path)?;
        let source = RasterSource::open(&path)?.remove(0).with_color_map_direction(ColorMapDirection::Reversed)?;
        let range_only = source.restyled(source.palette(), (10.0, 100.0))?;
        let recolored = range_only.restyled(ColorMapPreset::Gray, range_only.range())?;
        assert_ne!(range_only.id(), source.id());
        assert!(palette_key(&range_only) == palette_key(&source));
        assert!(palette_key(&recolored) != palette_key(&source));
        assert!(palette_key(&recolored) == (ColorMapPreset::Gray, ColorMapDirection::Reversed));
        Ok(())
    }
}
