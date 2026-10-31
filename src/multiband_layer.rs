//! Dedicated raw-value temporal raster renderer.
//!
//! Register only `mod multiband_layer;`: the clock and worker modules are private
//! children. The host supplies playback commands and calls `poll_tiles` from its
//! existing repaint pump. All positions/window keys are zero-based; source reads
//! use the actual one-based, half-open dataset band numbers from the request metadata.

#[path = "multiband_clock.rs"]
mod clock;
#[cfg(test)]
#[path = "multiband_extrusion_tests.rs"]
mod extrusion_tests;
#[cfg(test)]
#[path = "multiband_layer_fade_tests.rs"]
mod fade_tests;
#[path = "multiband_tiles.rs"]
mod tiles;
#[cfg(test)]
#[path = "multiband_layer_zoom_tests.rs"]
mod zoom_tests;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::{Result, ensure};
use bytemuck::{Pod, Zeroable};
use geo::Tile;
use inf::colormap::{ColorMapDirection, ColorMapPreset, ProcessedColorMap};
use maplibre_native::{WgpuLayerParameters, texture_format_from_webgpu};

use crate::raster_layer::geometry;
use crate::raster_playback::{PlaybackFrame, PlaybackRequest, PlaybackStyle, validate_bands};
use crate::raster_source::RasterSource;
use crate::raster_tiles::{VisibleTile, extrusion_view, tile_matrix, visible_tiles};
use clock::{BandFade, DEFAULT_WINDOW, Endpoints, PlaybackClock, endpoints, next_window, window_range, window_start};
use tiles::{CachedWindow, MAX_CACHE_ENTRIES, MAX_GRID_BYTES, MAX_JOBS, MAX_VISIBLE_TILES, WindowKey, WindowTexture, WindowWorkers};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DrawUniform {
    matrix: [[f32; 4]; 4],
    scale: [f32; 2],
    next_scale: [f32; 2],
    range: [f32; 2],
    next_range: [f32; 2],
    // blend, opacity, screen-space tile size, manual color crossfade
    parameters: [f32; 4],
    // current array layer, next array layer, clamp colors, debug status
    layers: [u32; 4],
    extrusion: [f32; 4],
    // X/Y strides, traversal flags, current/next averaging bits.
    geometry: [u32; 4],
    outgoing_scale: [f32; 2],
    outgoing_range: [f32; 2],
    // Outgoing temporal fraction, extra array layer, reserved, averaging bit.
    outgoing: [f32; 4],
}

struct DrawTile {
    sampled: [Option<WindowKey>; 3],
    uniform: wgpu::Buffer,
    bindings: wgpu::BindGroup,
}

struct RetainedFrame {
    // Bind groups keep the old raw textures alive even when an adaptive window
    // resize clears the cache. Only one displayed grid is ever retained.
    draws: HashMap<VisibleTile, DrawTile>,
    frame: Endpoints,
    window_size: usize,
    fade_from: Option<Endpoints>,
}

struct MultibandPipeline {
    formats: (u32, u32, bool),
    pipeline: wgpu::RenderPipeline,
    extrusion_pipeline: wgpu::RenderPipeline,
    extrusion_indices: wgpu::Buffer,
    layout: wgpu::BindGroupLayout,
    palette: wgpu::Texture,
    palette_view: wgpu::TextureView,
    palette_key: Option<(ColorMapPreset, ColorMapDirection)>,
    palette_opaque: bool,
    empty: WindowTexture,
    #[cfg(test)]
    palette_uploads: u64,
}

/// GPU playback with a global visible-tile barrier and bounded window caches.
pub struct MultibandLayer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    request: Option<PlaybackRequest>,
    opacity: f32,
    clock: PlaybackClock,
    band_fade: Option<BandFade>,
    workers: WindowWorkers,
    generation: u64,
    pending: HashSet<WindowKey>,
    wanted: HashSet<WindowKey>,
    coverage: HashSet<Tile>,
    cache: HashMap<WindowKey, CachedWindow>,
    cache_bytes: usize,
    draws: HashMap<VisibleTile, DrawTile>,
    retained: Option<RetainedFrame>,
    pipeline: Option<MultibandPipeline>,
    window_size: usize,
    max_window_size: usize,
    tile_size: u32,
    frame: u64,
    reduce_motion: bool,
    debug: bool,
    #[cfg(test)]
    requests: u64,
    #[cfg(test)]
    uploads: u64,
    #[cfg(test)]
    validations: u64,
    #[cfg(test)]
    uniform_writes: u64,
}

impl MultibandLayer {
    /// Starts two bounded I/O workers. GPU pipelines are lazy, matching the pass formats.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self> {
        let max_window_size = DEFAULT_WINDOW.min(device.limits().max_texture_array_layers as usize).max(1);
        Ok(Self {
            device,
            queue,
            request: None,
            opacity: 1.0,
            clock: PlaybackClock::new(Instant::now()),
            band_fade: None,
            workers: WindowWorkers::new()?,
            generation: 0,
            pending: HashSet::new(),
            wanted: HashSet::new(),
            coverage: HashSet::new(),
            cache: HashMap::new(),
            cache_bytes: 0,
            draws: HashMap::new(),
            retained: None,
            pipeline: None,
            window_size: max_window_size,
            max_window_size,
            tile_size: 256,
            frame: 0,
            reduce_motion: false,
            debug: false,
            #[cfg(test)]
            requests: 0,
            #[cfg(test)]
            uploads: 0,
            #[cfg(test)]
            validations: 0,
            #[cfg(test)]
            uniform_writes: 0,
        })
    }

    /// Only a new dataset or command revision seeks. Style snapshots with the
    /// same revision replace styling without rewinding time or invalidating raw tiles.
    /// Per-band display ranges interpolate independently of nodata. Palette and
    /// color-clamping policy follow the current band, changing at integer boundaries.
    pub fn set_playback(&mut self, request: PlaybackRequest, opacity: f32) {
        let dataset_changed = self
            .request
            .as_ref()
            .is_none_or(|old| old.dataset_id() != request.dataset_id() || old.bands().len() != request.bands().len());
        let command_changed = dataset_changed || self.request.as_ref().is_none_or(|old| old.revision() != request.revision());
        self.opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
        if let Err(error) = self.validate_request(&request) {
            self.clear_dataset();
            if let Some(source) = request.bands().first() {
                source.report_tile_error(&error);
            }
            log::error!("Multiband playback: {error:#}");
            self.request = None;
            return;
        }
        if dataset_changed {
            self.clear_dataset();
            self.tile_size = request.bands().first().and_then(|source| source.metadata().tile_size).unwrap_or(256);
        }
        if command_changed {
            self.prepare_band_fade(&request);
            self.retained = None;
            self.cancel_jobs();
            self.clock.seek(request.position(), request.bands().len(), Instant::now());
        }
        self.request = Some(request);
        if command_changed && !self.coverage.is_empty() {
            let ready = self.frame_ready(self.clock.position());
            self.clock.resolve(self.clock.position(), ready);
        }
    }

    fn prepare_band_fade(&mut self, request: &PlaybackRequest) {
        let position = request.position();
        if self.reduce_motion || request.is_playing() || !position.is_finite() || position.fract() != 0.0 {
            self.band_fade = None;
            return;
        }
        let to = endpoints(position, request.bands().len(), false).current;
        if let Some(fade) = &mut self.band_fade {
            fade.retarget(to);
        } else if !self.clock.is_buffering() {
            let from = self.endpoints(self.clock.position());
            self.band_fade = (from.current != to || from.fraction > 0.0).then(|| BandFade::from_frame(from, to));
        }
    }

    /// Turns off temporal interpolation, not user-requested playback.
    pub fn set_reduce_motion(&mut self, enabled: bool) {
        self.reduce_motion = enabled;
        if enabled {
            self.band_fade = None;
        }
    }

    /// Shows tile boundaries, orange while the global readiness barrier is closed.
    pub fn set_debug(&mut self, enabled: bool) {
        self.debug = enabled;
    }

    /// Drains completed windows. Returns true while repainting can make progress,
    /// including explicit playback; it does not itself advance the clock.
    pub fn poll_tiles(&mut self) -> bool {
        let mut changed = false;
        while let Some(result) = self.workers.completed() {
            if result.generation != self.generation || !self.pending.remove(&result.key) {
                continue;
            }
            if !self.wanted.contains(&result.key) {
                continue;
            }
            let expected = self
                .request
                .as_ref()
                .map_or(0, |request| window_range(result.key.start, self.window_size, request.bands().len()).len());
            let texture = result.data.and_then(|data| {
                ensure!(data.len() == expected, "Multiband tile returned {} bands, expected {expected}", data.len());
                tiles::upload_window(&self.device, &self.queue, data, self.tile_size)
            });
            let texture = match texture {
                Ok(texture) => texture,
                Err(error) => {
                    self.report_error(result.key, &error);
                    None
                }
            };
            #[cfg(test)]
            if texture.is_some() {
                self.uploads += 1;
            }
            self.make_room(texture.as_ref().map_or(0, |texture| texture.bytes));
            self.cache_bytes += texture.as_ref().map_or(0, |texture| texture.bytes);
            self.cache.insert(result.key, CachedWindow { texture, used: self.frame });
            changed = true;
        }
        changed || self.is_loading() || self.request.as_ref().is_some_and(PlaybackRequest::is_playing)
    }

    /// Loading/buffering and finite manual fades, never the perpetual fact of playing.
    pub fn is_loading(&self) -> bool {
        self.request.as_ref().is_some_and(|request| !request.bands().is_empty())
            && (self.band_fade.is_some()
                || self.clock.is_buffering()
                || !self.pending.is_empty()
                || self.wanted.iter().any(|key| !self.cache.contains_key(key)))
    }

    /// Feedback carries the accepted position, never the speculative buffered frame.
    pub fn playback_frame(&self) -> Option<PlaybackFrame> {
        let request = self.request.as_ref()?;
        Some(PlaybackFrame::new(
            request.dataset_id()?,
            request.revision(),
            self.clock.position(),
            self.clock.is_buffering(),
        ))
    }

    /// Paints the latest accepted frame. The surrounding host owns the render pass.
    pub fn render(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
        let Some(request) = self.request.clone() else { return };
        let Some(source) = request.bands().first() else { return };
        let now = Instant::now();
        self.frame = self.frame.wrapping_add(1);
        let visible = self.update_coverage(self.view_tiles(parameters, &request, source), now);
        let current_ready = self.frame_ready(self.clock.position());
        if current_ready || visible.is_empty() {
            self.retained = None;
        }
        if !current_ready {
            self.clock.freeze(now);
        }
        let proposed = self.clock.propose(now, request.bands_per_second(), request.bands().len(), request.is_playing());
        self.plan_windows(proposed);
        let ready = current_ready && self.frame_ready(proposed);
        self.clock.resolve(proposed, ready);
        self.schedule_windows(&request, &visible);
        self.prepare_pipeline(parameters);
        let frame = self.display_frame(now);
        if self.retained.is_some() {
            self.draw_retained_frame(pass, parameters, &request);
        } else {
            self.draw_frame(pass, parameters, &request, &visible, frame);
        }
        self.make_room(0);
    }

    fn display_frame(&mut self, now: Instant) -> Endpoints {
        let target = self.endpoints(self.clock.position());
        let Some(mut fade) = self.band_fade.take() else { return target };
        let ready = self.frame_ready(fade.from.current as f64) && self.frame_ready(fade.from.next as f64) && self.frame_ready(fade.to as f64);
        let frame = fade.frame(now, ready);
        if frame.fraction < 1.0 {
            self.band_fade = Some(fade);
        } else if fade.to != target.current {
            self.band_fade = Some(BandFade::new(fade.to, target.current));
        }
        frame
    }

    fn view_tiles(&self, parameters: &WgpuLayerParameters, request: &PlaybackRequest, source: &RasterSource) -> Vec<VisibleTile> {
        let height = playback_height(request);
        if height <= 0.0 {
            return visible_tiles(parameters, source);
        }

        let bytes_per_band = self.tile_size as usize * self.tile_size as usize * 5;
        let limit = (MAX_GRID_BYTES / (3 * bytes_per_band)).clamp(1, MAX_VISIBLE_TILES);
        // Cached/retained grid heights must not cap the replacement's request LOD.
        extrusion_view::visible_tiles(parameters, &[(source, height)], limit)
    }

    fn validate_request(&mut self, request: &PlaybackRequest) -> Result<()> {
        // Sources and their allocation are immutable. Matching dataset IDs alone
        // cannot establish validity: a different allocation may reorder bands.
        if self.request.as_ref().is_some_and(|old| old.shares_bands(request)) {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.validations += 1;
        }
        validate_bands(request.bands())?;
        let Some(first) = request.bands().first() else { return Ok(()) };
        let size = first.metadata().tile_size.unwrap_or(256);
        ensure!(
            size > 0 && size <= self.device.limits().max_texture_dimension_2d,
            "Unsupported multiband tile size {size}"
        );
        ensure!(
            size as u64 * size as u64 * 15 <= MAX_GRID_BYTES as u64,
            "Multiband tile size exceeds cache budget"
        );
        Ok(())
    }

    fn clear_dataset(&mut self) {
        self.band_fade = None;
        self.cancel_jobs();
        self.cache.clear();
        self.cache_bytes = 0;
        self.coverage.clear();
        self.draws.clear();
        self.retained = None;
        self.window_size = self.max_window_size;
    }

    fn cancel_jobs(&mut self) {
        self.generation = self.workers.reset();
        self.pending.clear();
        self.wanted.clear();
    }

    fn update_coverage(&mut self, mut visible: Vec<VisibleTile>, now: Instant) -> Vec<VisibleTile> {
        let bytes_per_band = self.tile_size as usize * self.tile_size as usize * 5;
        // Reserve a third window for an accepted frame crossing into its prefetch.
        let tile_limit = (MAX_GRID_BYTES / (3 * bytes_per_band)).clamp(1, MAX_VISIBLE_TILES);
        coarsen_coverage(&mut visible, tile_limit);
        let coverage: HashSet<_> = visible.iter().map(|tile| tile.canonical()).collect();
        let size = adaptive_window_size(self.window_size, self.max_window_size, bytes_per_band, coverage.len());
        let resized = size != self.window_size;
        let changed_zoom = self.coverage.iter().next().map(|tile| tile.z) != visible.first().map(|tile| tile.z);
        if (changed_zoom || resized) && self.retained.is_none() && self.draws.values().any(|draw| draw.sampled.iter().any(Option::is_some)) {
            let frame = self.display_frame(now);
            self.retained = Some(RetainedFrame {
                draws: std::mem::take(&mut self.draws),
                frame,
                window_size: self.window_size,
                fade_from: self.band_fade.as_ref().map(|fade| fade.from),
            });
        }
        if resized {
            self.window_size = size;
            self.cache.clear();
            self.cache_bytes = 0;
            self.draws.clear();
        }
        if coverage != self.coverage || resized {
            self.cancel_jobs();
            self.clock.freeze(now);
            self.coverage = coverage;
        }
        self.draws.retain(|tile, _| visible.contains(tile));
        visible
    }

    fn endpoints(&self, position: f64) -> Endpoints {
        self.request.as_ref().map_or(endpoints(0.0, 0, false), |request| {
            endpoints(position, request.bands().len(), request.interpolation() && !self.reduce_motion)
        })
    }

    fn frame_ready(&self, position: f64) -> bool {
        let frame = self.endpoints(position);
        let starts = [window_start(frame.current, self.window_size), window_start(frame.next, self.window_size)];
        self.coverage
            .iter()
            .all(|tile| starts.iter().all(|start| self.cache.contains_key(&WindowKey { tile: *tile, start: *start })))
    }

    fn plan_windows(&mut self, proposed: f64) {
        let Some(request) = &self.request else { return };
        let count = request.bands().len();
        let current = self.endpoints(self.clock.position());
        let incoming = self.endpoints(proposed);
        let start = window_start(incoming.current, self.window_size);
        let mut starts = [
            window_start(current.current, self.window_size),
            window_start(current.next, self.window_size),
            start,
            window_start(incoming.next, self.window_size),
            next_window(start, self.window_size, count),
        ];
        if let Some(fade) = &self.band_fade {
            // A fractional outgoing frame can span two windows. Reserve the
            // third for this fade's target; queued selections wait their turn.
            starts = [
                window_start(fade.from.current, self.window_size),
                window_start(fade.from.next, self.window_size),
                window_start(fade.to, self.window_size),
                window_start(fade.from.current, self.window_size),
                window_start(fade.to, self.window_size),
            ];
        }
        self.wanted.clear();
        for tile in &self.coverage {
            for start in starts {
                self.wanted.insert(WindowKey { tile: *tile, start });
            }
        }
        for key in &self.wanted {
            if let Some(cached) = self.cache.get_mut(key) {
                cached.used = self.frame;
            }
        }
    }

    fn schedule_windows(&mut self, request: &PlaybackRequest, visible: &[VisibleTile]) {
        let frame = self.endpoints(self.clock.position());
        let mut starts: Vec<_> = self.wanted.iter().map(|key| key.start).collect();
        starts.sort_unstable();
        starts.dedup();
        // Finish the accepted frame everywhere before spending jobs on prefetch.
        starts.sort_by_key(|start| {
            (
                *start != window_start(frame.current, self.window_size),
                *start != window_start(frame.next, self.window_size),
            )
        });
        for start in starts {
            for tile in visible {
                self.request_window(request, WindowKey { tile: tile.canonical(), start });
            }
        }
    }

    fn request_window(&mut self, request: &PlaybackRequest, key: WindowKey) {
        if self.cache.contains_key(&key) || self.pending.contains(&key) || self.pending.len() >= MAX_JOBS {
            return;
        }
        let Some(source) = request.bands().first() else { return };
        let Some(first_band) = source.metadata().band_nr else { return };
        let range = window_range(key.start, self.window_size, request.bands().len());
        if self
            .workers
            .request(self.generation, key, source.clone(), first_band + range.start..first_band + range.end)
        {
            self.pending.insert(key);
            #[cfg(test)]
            {
                self.requests += 1;
            }
        }
    }

    fn report_error(&self, key: WindowKey, error: &anyhow::Error) {
        if let Some(source) = self.request.as_ref().and_then(|request| request.bands().first()) {
            source.report_tile_error(error);
        }
        log::error!("Multiband tile {:?}, window {}: {error:#}", key.tile, key.start);
    }

    fn make_room(&mut self, incoming: usize) {
        while self.cache_bytes + incoming > MAX_GRID_BYTES || self.cache.len() >= MAX_CACHE_ENTRIES {
            let victim = self
                .cache
                .iter()
                .filter(|(key, _)| !self.wanted.contains(key))
                .min_by_key(|(_, cached)| cached.used)
                .map(|(key, _)| *key);
            let Some(victim) = victim else { break };
            if let Some(cached) = self.cache.remove(&victim) {
                self.cache_bytes -= cached.bytes();
            }
            // Bind groups own texture references too; eviction must release them.
            self.draws.clear();
        }
    }

    fn prepare_pipeline(&mut self, parameters: &WgpuLayerParameters) {
        let formats = (parameters.color_format, parameters.depth_format, parameters.depth_test);
        if self.pipeline.as_ref().is_none_or(|pipeline| pipeline.formats != formats) {
            self.draws.clear();
            self.retained = None;
            self.pipeline = Some(MultibandPipeline::new(&self.device, formats));
        }
    }

    fn draw_retained_frame(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters, request: &PlaybackRequest) {
        let Some(retained) = &self.retained else { return };
        let Some(source) = request.bands().get(retained.frame.current) else { return };
        let Some(next) = request.bands().get(retained.frame.next) else { return };
        let debug_status = self.debug_status();
        let Some(pipeline) = &mut self.pipeline else { return };
        pipeline.update_palette(
            &self.queue,
            (request.style().palette_for(source), request.style().color_map_direction_for(source)),
        );
        pass.set_pipeline(if request.style().extrusion_for(source).0 && request.style().extrusion_for(source).1 > 0.0 {
            &pipeline.extrusion_pipeline
        } else {
            &pipeline.pipeline
        });
        let mut tiles: Vec<_> = retained.draws.keys().copied().collect();
        pipeline.sort_tiles(
            parameters,
            &mut tiles,
            self.opacity,
            retained.frame.fraction,
            request.style().extrusion_for(source).0,
        );
        for tile in tiles {
            let draw = &retained.draws[&tile];
            let mut data = draw_uniform(
                parameters,
                tile,
                [source, next],
                retained.frame,
                retained.window_size,
                self.opacity,
                request.style(),
            );
            apply_band_fade(&mut data, retained.fade_from, request, retained.window_size);
            if !extrusion_visible(parameters, tile, &data) {
                continue;
            }
            paint_draw(&self.queue, pass, draw, data, debug_status, pipeline, parameters);
        }
    }

    fn debug_status(&self) -> u32 {
        if self.debug { 1 + u32::from(self.clock.is_buffering()) } else { 0 }
    }

    fn draw_frame(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        parameters: &WgpuLayerParameters,
        request: &PlaybackRequest,
        visible: &[VisibleTile],
        frame: Endpoints,
    ) {
        let Some(source) = request.bands().get(frame.current) else { return };
        let Some(next) = request.bands().get(frame.next) else { return };
        let fade_from = self.band_fade.as_ref().map(|fade| fade.from);
        let debug_status = self.debug_status();
        let Some(pipeline) = &mut self.pipeline else { return };
        pipeline.update_palette(
            &self.queue,
            (request.style().palette_for(source), request.style().color_map_direction_for(source)),
        );
        pass.set_pipeline(if request.style().extrusion_for(source).0 && request.style().extrusion_for(source).1 > 0.0 {
            &pipeline.extrusion_pipeline
        } else {
            &pipeline.pipeline
        });
        let mut tiles = visible.to_vec();
        pipeline.sort_tiles(parameters, &mut tiles, self.opacity, frame.fraction, request.style().extrusion_for(source).0);
        for tile in tiles {
            let keys = [frame.current, frame.next, fade_from.map_or(frame.current, |from| from.next)].map(|band| WindowKey {
                tile: tile.canonical(),
                start: window_start(band, self.window_size),
            });
            let sampled = keys.map(|key| self.cache.get(&key).and_then(|cached| cached.texture.as_ref()).map(|_| key));
            let textures = sampled.map(|key| key.and_then(|key| self.cache.get(&key)?.texture.as_ref()));
            let draw = self.draws.entry(tile).or_insert_with(|| pipeline.draw_tile(&self.device, sampled, textures));
            if draw.sampled != sampled {
                *draw = pipeline.draw_tile(&self.device, sampled, textures);
            }
            let mut data = draw_uniform(parameters, tile, [source, next], frame, self.window_size, self.opacity, request.style());
            apply_band_fade(&mut data, fade_from, request, self.window_size);
            paint_draw(&self.queue, pass, draw, data, debug_status, pipeline, parameters);
            #[cfg(test)]
            {
                self.uniform_writes += 1;
            }
        }
    }
}

fn extrusion_visible(parameters: &WgpuLayerParameters, tile: VisibleTile, data: &DrawUniform) -> bool {
    data.extrusion[3] <= 0.0 || extrusion_view::tile_visible(parameters, tile, f64::from(data.extrusion[2]))
}

fn playback_height(request: &PlaybackRequest) -> f32 {
    if let Some((enabled, height)) = request.style().extrusion() {
        return if enabled { height } else { 0.0 };
    }
    request
        .bands()
        .iter()
        .map(|source| {
            let (enabled, height) = request.style().extrusion_for(source);
            if enabled { height } else { 0.0 }
        })
        .fold(0.0, f32::max)
}

fn paint_draw(
    queue: &wgpu::Queue,
    pass: &mut wgpu::RenderPass<'_>,
    draw: &DrawTile,
    mut data: DrawUniform,
    debug_status: u32,
    pipeline: &MultibandPipeline,
    parameters: &WgpuLayerParameters,
) {
    for (layer, sampled) in data.layers[..2].iter_mut().zip(draw.sampled) {
        *layer = sampled.map_or(0, |_| *layer);
    }
    if draw.sampled[2].is_none() {
        data.outgoing[1] = 0.0;
    }
    data.layers[3] = debug_status;
    data.geometry = geometry::with_averaging(
        geometry::plan(
            &data.matrix,
            [parameters.width, parameters.height],
            data.extrusion,
            pipeline.opaque(data.parameters[1], data.parameters[0]) && data.outgoing[0] <= 0.0,
        ),
        data.geometry[3],
        data.parameters[0],
    );
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    pass.set_bind_group(0, &draw.bindings, &[]);
    crate::raster_layer::draw_pixels(pass, data.extrusion, data.geometry, &pipeline.extrusion_indices);
}

fn adaptive_window_size(current: usize, maximum: usize, bytes_per_band: usize, tile_count: usize) -> usize {
    let capacity = (MAX_GRID_BYTES / (3 * bytes_per_band * tile_count.max(1))).max(1);
    // Shrink immediately for safety. Only grow after capacity doubles, roughly
    // halving coverage, so small viewport fluctuations do not invalidate caches.
    // Compare the unbounded capacity: a near-maximum window can still recover.
    if capacity < current || capacity >= current.saturating_mul(2) {
        capacity.min(maximum)
    } else {
        current
    }
}

fn coarsen_coverage(visible: &mut Vec<VisibleTile>, limit: usize) {
    while visible.iter().map(|tile| tile.canonical()).collect::<HashSet<_>>().len() > limit {
        for tile in visible.iter_mut() {
            if tile.z > 0 {
                tile.z -= 1;
                tile.x = tile.x.div_euclid(2);
                tile.y /= 2;
            }
        }
        let mut seen = HashSet::new();
        visible.retain(|tile| seen.insert(*tile));
    }
}

fn averaging_flags(style: PlaybackStyle, sources: [&RasterSource; 2], extruded: bool) -> u32 {
    if !extruded {
        return 0;
    }
    let enabled = sources.map(|source| style.extrusion_averaging_for(source));
    u32::from(enabled[0]) | (u32::from(enabled[1]) << 1)
}

fn draw_uniform(
    parameters: &WgpuLayerParameters,
    tile: VisibleTile,
    sources: [&RasterSource; 2],
    frame: Endpoints,
    size: usize,
    opacity: f32,
    style: PlaybackStyle,
) -> DrawUniform {
    let scales = sources.map(|source| style.scale_for(source));
    let ranges = sources.map(|source| style.range_for(source));
    let (enabled, height) = style.extrusion_for(sources[0]);
    let pixels = sources[0].metadata().tile_size.unwrap_or(256);
    DrawUniform {
        extrusion: crate::raster_layer::extrusion_uniform(parameters, sources[0], [pixels; 2], if enabled { height } else { 0.0 }),
        geometry: geometry::with_averaging([0; 4], averaging_flags(style, sources, enabled), frame.fraction),
        matrix: tile_matrix(&parameters.projection, parameters.zoom, tile),
        scale: [scales[0].scale as f32, scales[0].offset as f32],
        next_scale: [scales[1].scale as f32, scales[1].offset as f32],
        range: [ranges[0].0, ranges[0].1],
        next_range: [ranges[1].0, ranges[1].1],
        parameters: [
            frame.fraction,
            opacity,
            (512.0 * parameters.zoom.exp2() / f64::from(1_u32 << tile.z)) as f32,
            0.0,
        ],
        layers: [
            (frame.current % size) as u32,
            (frame.next % size) as u32,
            u32::from(style.clamp_colors_for(sources[0])),
            0,
        ],
        outgoing_scale: [0.0; 2],
        outgoing_range: [0.0; 2],
        outgoing: [0.0; 4],
    }
}

fn apply_band_fade(data: &mut DrawUniform, from: Option<Endpoints>, request: &PlaybackRequest, size: usize) {
    let Some(from) = from else { return };
    let Some(source) = request.bands().get(from.next) else { return };
    let style = request.style();
    let scale = style.scale_for(source);
    let range = style.range_for(source);
    data.parameters[3] = 1.0;
    data.outgoing_scale = [scale.scale as f32, scale.offset as f32];
    data.outgoing_range = [range.0, range.1];
    data.outgoing = [from.fraction, (from.next % size) as f32, 0.0, f32::from(style.extrusion_averaging_for(source))];
}

impl MultibandPipeline {
    fn sort_tiles(&self, parameters: &WgpuLayerParameters, tiles: &mut [VisibleTile], opacity: f32, blend: f32, extruded: bool) {
        if extruded && self.opaque(opacity, blend) {
            geometry::sort_tiles(parameters, tiles);
        }
    }

    fn opaque(&self, opacity: f32, blend: f32) -> bool {
        self.palette_opaque && opacity >= 1.0 && (blend <= 0.0 || blend >= 1.0)
    }

    fn new(device: &wgpu::Device, formats: (u32, u32, bool)) -> Self {
        let layout = create_layout(device);
        let pipeline = create_pipeline(device, formats, &layout, false);
        let palette = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Multiband palette"),
            size: wgpu::Extent3d {
                width: 256,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let palette_view = palette.create_view(&Default::default());
        let view = wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        };
        let empty = WindowTexture {
            values: tiles::texture(device, 1, 1, wgpu::TextureFormat::R32Float).create_view(&view),
            mask: tiles::texture(device, 1, 1, wgpu::TextureFormat::R8Unorm).create_view(&view),
            bytes: 5,
        };
        Self {
            formats,
            pipeline,
            extrusion_pipeline: create_pipeline(device, formats, &layout, true),
            extrusion_indices: crate::raster_layer::create_extrusion_indices(device),
            layout,
            palette,
            palette_view,
            palette_key: None,
            palette_opaque: false,
            empty,
            #[cfg(test)]
            palette_uploads: 0,
        }
    }

    fn update_palette(&mut self, queue: &wgpu::Queue, key: (ColorMapPreset, ColorMapDirection)) {
        if self.palette_key == Some(key) {
            return;
        }
        let palette = ProcessedColorMap::<256>::create_for_preset(key.0, key.1);
        let rgba: Vec<_> = (0..=255)
            .flat_map(|index| {
                let color = palette.get_color_by_value(index);
                [color.r, color.g, color.b, color.a]
            })
            .collect();
        queue.write_texture(
            self.palette.as_image_copy(),
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(1024),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 256,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.palette_key = Some(key);
        self.palette_opaque = rgba.as_chunks::<4>().0.iter().all(|color| color[3] == 255);
        #[cfg(test)]
        {
            self.palette_uploads += 1;
        }
    }

    fn draw_tile(&self, device: &wgpu::Device, sampled: [Option<WindowKey>; 3], textures: [Option<&WindowTexture>; 3]) -> DrawTile {
        let textures = textures.map(|texture| texture.unwrap_or(&self.empty));
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Multiband frame uniform"),
            size: std::mem::size_of::<DrawUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let views = [
            &textures[0].values,
            &textures[0].mask,
            &textures[1].values,
            &textures[1].mask,
            &self.palette_view,
            &textures[2].values,
            &textures[2].mask,
        ];
        let mut entries = vec![wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform.as_entire_binding(),
        }];
        entries.extend(views.iter().enumerate().map(|(index, view)| wgpu::BindGroupEntry {
            binding: index as u32 + 1,
            resource: wgpu::BindingResource::TextureView(view),
        }));
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Multiband tile bindings"),
            layout: &self.layout,
            entries: &entries,
        });
        DrawTile { sampled, uniform, bindings }
    }
}

fn create_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let mut entries = vec![wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }];
    entries.extend((1..=7).map(|binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: if binding == 5 {
                wgpu::TextureViewDimension::D2
            } else {
                wgpu::TextureViewDimension::D2Array
            },
            multisampled: false,
        },
        count: None,
    }));
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("Multiband layout"),
        entries: &entries,
    })
}

fn create_pipeline(device: &wgpu::Device, formats: (u32, u32, bool), layout: &wgpu::BindGroupLayout, extruded: bool) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Multiband pipeline layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let entry_points = if extruded {
        concat!(include_str!("multiband_extrusion_sample.wgsl"), "\n", include_str!("raster_extrusion.wgsl"))
    } else {
        include_str!("multiband_layer.wgsl")
    };
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(if extruded { "Multiband extrusion shader" } else { "Multiband shader" }),
        source: wgpu::ShaderSource::Wgsl(format!("{}\n{}", include_str!("multiband_sample.wgsl"), entry_points).into()),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(if extruded { "Multiband raster extrusion" } else { "Multiband raster" }),
        layout: Some(&layout),
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
            // Preserve the nearer surface when finite depth precision ties.
            depth_compare: Some(if extruded {
                wgpu::CompareFunction::Less
            } else if formats.2 {
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
            entry_point: Some(if extruded { "extruded_fragment" } else { "fragment" }),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: texture_format_from_webgpu(formats.0),
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use std::sync::Arc;
    use std::time::Duration;

    fn gpu_device() -> Result<(wgpu::Device, wgpu::Queue)> {
        futures_lite::future::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance.request_adapter(&Default::default()).await?;
            adapter
                .request_device(&Default::default())
                .await
                .context("Could not create the multiband test GPU device")
        })
    }

    pub(super) fn request(bands: &[Arc<RasterSource>], position: f64, revision: u64) -> PlaybackRequest {
        PlaybackRequest::new(bands.to_vec(), position, revision, false, 10.0, true)
    }

    pub(super) fn parameters() -> WgpuLayerParameters {
        WgpuLayerParameters {
            width: 256.0,
            height: 256.0,
            latitude: 0.0,
            longitude: 0.0,
            zoom: 2.0,
            projection: [1.0 / 128.0, 0.0, 0.0, 0.0, 0.0, -1.0 / 128.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, -8.0, 8.0, 0.0, 1.0],
            color_format: 0x16,
            depth_format: 0,
            depth_test: false,
        }
    }

    fn paint_layer(layer: &mut MultibandLayer) {
        let output = test_target(&layer.device);
        let view = output.create_view(&Default::default());
        let mut encoder = layer.device.create_command_encoder(&Default::default());
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
            layer.render(&mut pass, &parameters());
        }
        layer.queue.submit([encoder.finish()]);
    }

    pub(super) fn load_until_idle(layer: &mut MultibandLayer) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            paint_layer(layer);
            layer.poll_tiles();
            if !layer.is_loading() {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "Multiband layer did not finish loading before deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn adaptive_windows_recover_to_configured_maximum_after_coverage_reductions() {
        let bytes = 256 * 256 * 5;
        let mut size = DEFAULT_WINDOW;
        for (tiles, expected) in [(64, 2), (48, 2), (32, 4), (24, 4), (16, 8), (13, 8), (8, 17), (4, 20)] {
            size = adaptive_window_size(size, DEFAULT_WINDOW, bytes, tiles);
            assert_eq!(size, expected, "Coverage {tiles}");
            assert!(3 * bytes * tiles * size <= MAX_GRID_BYTES);
        }
    }

    #[test]
    fn adaptive_windows_do_not_oscillate_on_small_coverage_fluctuations() {
        let bytes = 256 * 256 * 5;
        let mut size = 2;
        for tiles in [64, 60, 56, 48, 56, 64] {
            size = adaptive_window_size(size, DEFAULT_WINDOW, bytes, tiles);
            assert_eq!(size, 2);
        }
    }

    #[test]
    fn adaptive_window_recovery_respects_device_ceiling_and_empty_coverage() {
        let bytes = 256 * 256 * 5;
        assert_eq!(adaptive_window_size(2, 6, bytes, 16), 6);
        assert_eq!(adaptive_window_size(4, 6, bytes, 0), 6);
        assert_eq!(adaptive_window_size(1, 1, bytes, 0), 1);
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_adaptive_window_growth_cancels_old_layout_and_recovers_default_capacity() -> Result<()> {
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        let visible: Vec<_> = (0..64)
            .map(|index| VisibleTile {
                z: 3,
                x: index % 8,
                y: (index / 8) as i32,
            })
            .collect();
        let now = Instant::now();
        layer.update_coverage(visible.clone(), now);
        assert_eq!(layer.window_size, 2);
        let key = WindowKey {
            tile: visible[0].canonical(),
            start: 0,
        };
        layer.cache.insert(key, CachedWindow { texture: None, used: 0 });
        layer.pending.insert(key);
        let generation = layer.generation;
        layer.update_coverage(visible[..32].to_vec(), now);
        assert_eq!(layer.window_size, 4);
        assert!(layer.cache.is_empty() && layer.pending.is_empty());
        assert!(layer.generation > generation);
        assert!(layer.clock.is_buffering());
        layer.update_coverage(visible[..4].to_vec(), now);
        assert_eq!(layer.window_size, layer.max_window_size);
        Ok(())
    }

    #[test]
    fn validation_accepts_gdal_multiband_sources_but_rejects_mixed_paths_and_order() -> Result<()> {
        let (directory, bands) = tiles::cog_fixture(3)?;
        let (_other_directory, other) = tiles::cog_fixture(3)?;
        assert!(validate_bands(&bands).is_ok());
        let plain = RasterSource::open(&directory.path().join("input.tif"))?;
        assert_ne!(plain[0].metadata().source_format, tiler::LayerSourceType::CloudOptimizedGeoTiff);
        assert!(validate_bands(&plain).is_ok());
        let mixed = vec![bands[0].clone(), other[1].clone(), bands[2].clone()];
        let error = validate_bands(&mixed).unwrap_err();
        assert!(error.to_string().contains("same dataset path"));
        let reordered = vec![bands[0].clone(), bands[2].clone(), bands[1].clone()];
        assert!(validate_bands(&reordered).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_paused_seeks_cross_windows_and_wrap_last_to_first_using_cached_data() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(43)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        layer.set_playback(request(&bands, 0.0, 1), 1.0);
        load_until_idle(&mut layer)?;
        assert!(!layer.coverage.is_empty());
        assert_eq!(layer.requests, 2 * layer.coverage.len() as u64);
        for (revision, (position, cached)) in [(19.5, true), (42.5, false), (20.5, true), (0.0, true)].into_iter().enumerate() {
            layer.set_playback(request(&bands, position, revision as u64 + 2), 1.0);
            assert_eq!(layer.playback_frame().unwrap().is_buffering(), !cached);
            let before = (layer.requests, layer.uploads);
            load_until_idle(&mut layer)?;
            assert_eq!(layer.playback_frame().unwrap().position(), position);
            if cached {
                assert_eq!((layer.requests, layer.uploads), before);
            }
            if position == 42.5 {
                assert_eq!(layer.endpoints(position).next, 0);
            }
            assert!(!layer.poll_tiles(), "Paused, cached playback must stop the repaint pump");
        }
        Ok(())
    }

    fn styled_bands(bands: &[Arc<RasterSource>]) -> Result<Vec<Arc<RasterSource>>> {
        bands
            .iter()
            .map(|band| {
                band.restyled(ColorMapPreset::Gray, (-10.0, 1000.0))?
                    .with_color_map_direction(ColorMapDirection::Reversed)?
                    .with_clamp_colors(false)
            })
            .collect()
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_same_window_frames_and_same_revision_styles_do_not_reread_or_reupload() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(43)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        layer.set_playback(request(&bands, 0.0, 7), 1.0);
        load_until_idle(&mut layer)?;
        let counts = (layer.requests, layer.uploads);
        let generation = layer.generation;
        std::fs::remove_file(&bands[0].metadata().path)?;
        for position in [1.25, 2.5, 18.5] {
            layer.clock.resolve(position, true);
            paint_layer(&mut layer);
            assert!(!layer.poll_tiles());
        }
        let styled = styled_bands(&bands)?;
        layer.set_playback(request(&styled, 0.0, 7), 0.5);
        load_until_idle(&mut layer)?;
        assert_eq!((layer.clock.position(), layer.generation), (18.5, generation));
        assert_eq!((layer.requests, layer.uploads), counts);
        assert!(layer.pipeline.as_ref().unwrap().palette_key == Some((ColorMapPreset::Gray, ColorMapDirection::Reversed)));
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_shared_band_range_changes_write_only_uniforms() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(43)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        let command = request(&bands, 3.25, 7);
        layer.set_playback(command.clone(), 1.0);
        load_until_idle(&mut layer)?;
        let counts = (layer.requests, layer.uploads, layer.validations, layer.generation);
        let pipeline = layer.pipeline.as_ref().unwrap().pipeline.clone();
        let palette_uploads = layer.pipeline.as_ref().unwrap().palette_uploads;
        let writes = layer.uniform_writes;
        let draws: Vec<_> = layer
            .draws
            .iter()
            .map(|(tile, draw)| (*tile, draw.bindings.clone(), draw.uniform.clone()))
            .collect();
        assert!(!draws.is_empty());
        std::fs::remove_file(&bands[0].metadata().path)?;
        for maximum in 601..=700 {
            let style = PlaybackStyle::default().with_range((0.0, maximum as f32))?;
            // Deliberately stale same-revision position must not rewind the clock.
            let updated = command.clone().with_controls(0.0, 7, false, 10.0).with_style(style);
            assert!(command.shares_bands(&updated));
            layer.set_playback(updated, 1.0);
            paint_layer(&mut layer);
            assert!(!layer.poll_tiles());
        }
        assert_eq!((layer.requests, layer.uploads, layer.validations, layer.generation), counts);
        assert_eq!(layer.validations, 1);
        assert_eq!(layer.clock.position(), 3.25);
        let current = layer.pipeline.as_ref().unwrap();
        assert_eq!(current.pipeline, pipeline);
        assert_eq!(current.palette_uploads, palette_uploads);
        assert_eq!(layer.uniform_writes - writes, 100 * draws.len() as u64);
        for (tile, bindings, uniform) in draws {
            assert_eq!(layer.draws[&tile].bindings, bindings);
            assert_eq!(layer.draws[&tile].uniform, uniform);
        }
        assert_eq!(layer.request.as_ref().unwrap().style().range(), Some((0.0, 700.0)));
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_distinct_band_allocations_with_matching_ids_are_always_validated() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(3)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        let command = request(&bands, 0.0, 1);
        layer.set_playback(command.clone(), 1.0);
        layer.set_playback(command.with_style(PlaybackStyle::default().with_range((0.0, 600.0))?), 1.0);
        assert_eq!(layer.validations, 1);
        layer.set_playback(request(&bands, 0.0, 1), 1.0);
        assert_eq!(layer.validations, 2);
        let malformed = vec![bands[0].clone(), bands[2].clone(), bands[1].clone()];
        layer.set_playback(request(&malformed, 0.0, 1), 1.0);
        assert_eq!(layer.validations, 3);
        assert!(layer.request.is_none());
        assert!(bands[0].errors().try_recv()?.contains("contiguous"));
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_runtime_palette_changes_upload_once_per_effective_key() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(3)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        let command = request(&bands, 0.25, 1);
        layer.set_playback(command.clone(), 1.0);
        load_until_idle(&mut layer)?;
        let counts = (layer.requests, layer.uploads, layer.generation);
        let uploads = layer.pipeline.as_ref().unwrap().palette_uploads;
        let style = PlaybackStyle::default()
            .with_palette(ColorMapPreset::Gray)
            .with_color_map_direction(ColorMapDirection::Reversed);
        layer.set_playback(command.clone().with_style(style), 1.0);
        paint_layer(&mut layer);
        let pipeline = layer.pipeline.as_ref().unwrap();
        assert!(pipeline.palette_key == Some((ColorMapPreset::Gray, ColorMapDirection::Reversed)));
        assert_eq!(pipeline.palette_uploads, uploads + 1);
        layer.set_playback(command.with_style(style.with_clamp_colors(false).with_unscaling(false)), 1.0);
        paint_layer(&mut layer);
        assert_eq!(layer.pipeline.as_ref().unwrap().palette_uploads, uploads + 1);
        assert_eq!((layer.requests, layer.uploads, layer.generation), counts);
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_reduced_motion_allows_explicit_playback_without_endless_loading() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(3)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        layer.set_playback(request(&bands, 0.0, 1), 1.0);
        load_until_idle(&mut layer)?;
        let counts = (layer.requests, layer.uploads);
        layer.set_reduce_motion(true);
        layer.set_playback(PlaybackRequest::new(bands, 0.0, 2, true, 10.0, true), 1.0);
        paint_layer(&mut layer);
        std::thread::sleep(Duration::from_millis(5));
        paint_layer(&mut layer);
        assert!(layer.clock.position() > 0.0);
        assert_eq!(layer.endpoints(layer.clock.position()).fraction, 0.0);
        assert!(!layer.is_loading());
        assert!(layer.poll_tiles());
        assert_eq!((layer.requests, layer.uploads), counts);
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_seek_cancels_old_jobs_without_accepting_stale_windows() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(43)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        layer.set_playback(request(&bands, 0.0, 1), 1.0);
        paint_layer(&mut layer);
        let old_generation = layer.generation;
        assert!(!layer.pending.is_empty());
        layer.set_playback(request(&bands, 42.5, 2), 1.0);
        assert!(layer.generation > old_generation && layer.pending.is_empty());
        load_until_idle(&mut layer)?;
        assert_eq!(layer.playback_frame().unwrap().position(), 42.5);
        assert!(layer.frame_ready(42.5));
        assert!(layer.cache.keys().all(|key| key.start == 0 || key.start == 40));
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_global_barrier_waits_for_every_tile_and_failures_resolve_it() -> Result<()> {
        let (_directory, bands) = tiles::cog_fixture(3)?;
        let (device, queue) = gpu_device()?;
        let mut layer = MultibandLayer::new(device, queue)?;
        layer.set_playback(request(&bands, 0.0, 1), 1.0);
        let first = Tile { z: 3, x: 3, y: 3 };
        let second = Tile { z: 3, x: 4, y: 3 };
        layer.coverage.extend([first, second]);
        let first_key = WindowKey { tile: first, start: 0 };
        let second_key = WindowKey { tile: second, start: 0 };
        layer.cache.insert(first_key, CachedWindow { texture: None, used: 0 });
        assert!(!layer.frame_ready(0.0), "One tile cannot open the global barrier");
        layer.wanted.insert(second_key);
        layer.pending.insert(second_key);
        assert!(layer.workers.request(layer.generation, second_key, bands[0].clone(), 99..100));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !layer.cache.contains_key(&second_key) {
            layer.poll_tiles();
            ensure!(Instant::now() < deadline, "Failed batch did not resolve its pending slot");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(layer.frame_ready(0.0));
        assert!(layer.cache[&second_key].texture.is_none());
        assert!(bands[0].errors().try_recv()?.contains("Invalid 1-based COG band range"));
        let count = layer.requests;
        layer.request_window(&request(&bands, 0.0, 1), second_key);
        assert_eq!(layer.requests, count, "Reported failure must not be retried every paint");
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device"]
    fn gpu_interpolates_scaled_values_before_palette_and_fades_single_invalid_pixels() -> Result<()> {
        let (device, queue) = gpu_device()?;
        // WebGPU's WGPUTextureFormat_RGBA8Unorm, as used by the native host.
        let pipeline = MultibandPipeline::new(&device, (0x16, 0, false));
        let cases = [
            (100.0, false, [0, 255, 0, 255, 0, 64, 0, 64, 0, 191, 0, 191, 0, 0, 0, 0]),
            (200.0, true, [41, 41, 41, 255, 26, 26, 26, 64, 40, 40, 40, 191, 0, 0, 0, 0]),
        ];
        for (next_max, gradient, expected) in cases {
            let palette = test_palette(gradient);
            queue.write_texture(
                pipeline.palette.as_image_copy(),
                &palette,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(1024),
                    rows_per_image: Some(1),
                },
                wgpu::Extent3d {
                    width: 256,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            let bands = vec![
                Some(crate::raster_source::NumericTile {
                    width: 2,
                    height: 2,
                    values: vec![0.0, f32::NAN, 8.0, f32::NAN],
                }),
                Some(crate::raster_source::NumericTile {
                    width: 2,
                    height: 2,
                    values: vec![10.0, 10.0, f32::NAN, f32::NAN],
                }),
            ];
            let texture = tiles::upload_window(&device, &queue, bands, 2)?.expect("nonempty test bands");
            let draw = pipeline.draw_tile(&device, [None; 3], [Some(&texture), Some(&texture), None]);
            queue.write_buffer(
                &draw.uniform,
                0,
                bytemuck::bytes_of(&DrawUniform {
                    matrix: [[2.0, 0.0, 0.0, 0.0], [0.0, -2.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [-1.0, 1.0, 0.0, 1.0]],
                    scale: [2.0, 10.0],
                    next_scale: [3.0, 20.0],
                    range: [0.0, 100.0],
                    next_range: [0.0, next_max],
                    parameters: [0.25, 1.0, 2.0, 0.0],
                    layers: [0, 1, 1, 0],
                    extrusion: [0.0; 4],
                    geometry: [0; 4],
                    ..DrawUniform::zeroed()
                }),
            );
            let output = render_test_quad(&device, &queue, &pipeline.pipeline, &draw.bindings);
            let image = crate::map_readback::GpuReadback::new(&device).read(&output, &device, &queue)?;
            assert!(
                image.as_raw().iter().zip(expected).all(|(actual, expected)| actual.abs_diff(expected) <= 1),
                "GPU UNORM rounding differs by more than one byte: {:?} versus {expected:?}",
                image.as_raw()
            );
        }
        Ok(())
    }

    fn test_palette(gradient: bool) -> Vec<u8> {
        if gradient {
            (0..=255_u8).flat_map(|index| [index, index, index, 255]).collect()
        } else {
            (0..=255_u8).flat_map(|index| [0, if index < 40 { 0 } else { 255 }, 0, 255]).collect()
        }
    }

    fn test_target(device: &wgpu::Device) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Multiband test output"),
            size: wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    fn render_test_quad(device: &wgpu::Device, queue: &wgpu::Queue, pipeline: &wgpu::RenderPipeline, bindings: &wgpu::BindGroup) -> wgpu::Texture {
        let output = test_target(device);
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
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bindings, &[]);
            pass.draw(0..6, 0..1);
        }
        queue.submit([encoder.finish()]);
        output
    }

    fn uniform_sources(directory: &std::path::Path) -> Result<Vec<Arc<RasterSource>>> {
        let path = directory.join("scaled.tif");
        crate::raster_source::test_support::write_raster(&path, 3857, 2)?;
        std::fs::write(
            directory.join("scaled.tif.aux.xml"),
            "<PAMDataset><PAMRasterBand band=\"1\"><Scale>2</Scale><Offset>10</Offset></PAMRasterBand>\
             <PAMRasterBand band=\"2\"><Scale>3</Scale><Offset>20</Offset></PAMRasterBand></PAMDataset>",
        )?;
        let mut bands = RasterSource::open(&path)?;
        bands[0] = bands[0].restyled(ColorMapPreset::Gray, (10.0, 100.0))?;
        bands[1] = bands[1].with_unscaling(false)?.restyled(ColorMapPreset::Gray, (20.0, 200.0))?;
        Ok(bands)
    }

    #[test]
    fn default_uniform_preserves_independent_endpoint_ranges_and_scales() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = uniform_sources(directory.path())?;
        let tile = VisibleTile { z: 3, x: 3, y: 3 };
        let data = draw_uniform(
            &parameters(),
            tile,
            [&bands[0], &bands[1]],
            endpoints(0.25, 2, true),
            20,
            0.5,
            PlaybackStyle::default(),
        );
        assert_eq!((data.range, data.next_range), ([10.0, 100.0], [20.0, 200.0]));
        assert_eq!((data.scale, data.next_scale), ([2.0, 10.0], [1.0, 0.0]));
        assert_eq!((data.parameters[0], data.parameters[1], data.layers[2]), (0.25, 0.5, 1));
        Ok(())
    }

    #[test]
    fn runtime_uniform_applies_range_clamping_and_unscaling_to_both_endpoints() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = uniform_sources(directory.path())?;
        let tile = VisibleTile { z: 3, x: 3, y: 3 };
        let style = PlaybackStyle::default()
            .with_range((-10.0, 700.0))?
            .with_clamp_colors(false)
            .with_unscaling(true);
        let data = draw_uniform(&parameters(), tile, [&bands[0], &bands[1]], endpoints(0.25, 2, true), 20, 1.0, style);
        assert_eq!((data.range, data.next_range), ([-10.0, 700.0], [-10.0, 700.0]));
        assert_eq!((data.scale, data.next_scale), ([2.0, 10.0], [3.0, 20.0]));
        assert_eq!(data.layers[2], 0);
        let raw = draw_uniform(
            &parameters(),
            tile,
            [&bands[0], &bands[1]],
            endpoints(0.25, 2, true),
            20,
            1.0,
            style.with_unscaling(false),
        );
        assert_eq!((raw.scale, raw.next_scale), ([1.0, 0.0], [1.0, 0.0]));
        Ok(())
    }

    #[test]
    fn uniform_layout_matches_wgsl_alignment() {
        assert_eq!(std::mem::size_of::<DrawUniform>(), 192);
    }

    #[test]
    fn coarsening_bounds_residency_and_preserves_world_copies() {
        let mut visible: Vec<_> = (-8..8).map(|x| VisibleTile { z: 3, x, y: 3 }).collect();
        coarsen_coverage(&mut visible, 2);
        assert_eq!(
            (visible.len(), visible.iter().map(|tile| tile.canonical()).collect::<HashSet<_>>().len()),
            (4, 2)
        );
    }
}
