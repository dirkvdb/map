use std::cell::{Cell, RefCell};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use gpui_kit::RenderImage;
use image::RgbaImage;
use maplibre_native::{
    AnyLayer, AnySource, Continuous, GeoJson, GeoJsonSource, ImageRenderer, ImageRendererBuilder, MapDebugOptions, RunLoopHandle, ScreenCoordinate, Size,
    WgpuCustomLayer, WgpuLayerParameters,
};

pub use crate::background::BasemapStyle;
use crate::map_camera::MapCamera;
use crate::map_readback::GpuReadback;
use crate::raster_layer::{RasterLayer, RasterLayerHost};
use crate::raster_playback::{PlaybackFrame, PlaybackRequest};
use crate::raster_source::{RasterSource, RasterTiles};
use crate::vector_source::VectorSource;

#[derive(Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub scale: f32,
}

#[derive(Clone, Copy, PartialEq)]
pub struct MapRequest {
    pub viewport: Viewport,
    pub camera: MapCamera,
}

pub struct MapFrame {
    pub viewport: Viewport,
    pub basemap: BasemapStyle,
    pub tile_boundaries: bool,
    pub raster_layer_id: Option<tiler::LayerId>,
    /// Requested source identity, not evidence that this frame contains its geometry.
    pub vector_layer_id: Option<u64>,
    /// Current-request image with native resources, render work, and raster fades idle.
    /// Source identity or a successful readback alone does not establish readiness.
    pub settled: bool,
    pub playback: Option<PlaybackFrame>,
    pub image: Result<Arc<RenderImage>>,
}

#[derive(Clone)]
pub enum MapSource {
    Raster(Arc<RasterSource>),
    Multiband(Arc<RasterSource>, PlaybackRequest),
    Vector(Arc<VectorSource>, String),
}

#[derive(Clone)]
struct RenderRequest {
    map: MapRequest,
    raster: Option<Arc<RasterSource>>,
    playback: Option<PlaybackRequest>,
    vector: Option<Arc<VectorSource>>,
    vector_color: String,
    opacity: f32,
    reduce_motion: bool,
    basemap: BasemapStyle,
    tile_boundaries: bool,
    enqueued_at: Option<Instant>,
}

impl RenderRequest {
    fn resume_playback(&mut self, map: Option<&NativeMap>) {
        if let Some(playback) = &self.playback {
            self.playback = Some(playback.resumed_from(map.and_then(|map| map.numeric.borrow().playback_frame())));
        }
    }

    fn playback_frame(&self, map: Option<&NativeMap>) -> Option<PlaybackFrame> {
        let request = self.playback.as_ref()?;
        let frame = map.and_then(|map| map.numeric.borrow().playback_frame());
        frame
            .filter(|frame| Some(frame.dataset_id()) == request.dataset_id() && frame.revision() == request.revision())
            .or_else(|| {
                request
                    .dataset_id()
                    .map(|id| PlaybackFrame::new(id, request.revision(), request.position(), true))
            })
    }
}

struct FeatureQuery {
    revision: u64,
    map: MapRequest,
    layer_id: u64,
    point: (f64, f64),
}

pub struct FeatureQueryResult {
    revision: u64,
    features: Result<Vec<serde_json::Value>, String>,
}

impl FeatureQueryResult {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn into_features(self) -> Result<Vec<serde_json::Value>, String> {
        self.features
    }
}

pub struct MapRenderer {
    sender: mpsc::Sender<RenderRequest>,
    frames: async_channel::Receiver<MapFrame>,
    queries: async_channel::Sender<Option<FeatureQuery>>,
    pending_query: async_channel::Receiver<Option<FeatureQuery>>,
    feature_results: async_channel::Receiver<FeatureQueryResult>,
}

impl MapRenderer {
    pub fn new() -> Result<Self> {
        Self::start_worker(None)
    }

    /// Uses the normal worker with local style JSON, including after DPI changes.
    /// The initial request supplies its BasemapStyle tag; keep that tag unchanged
    /// so set_basemap does not reload a remote style. JSON resources must also be
    /// local (a background-only style needs no network).
    #[cfg(test)]
    pub fn with_test_style(json: &str) -> Result<Self> {
        Self::start_worker(Some(json.to_owned()))
    }

    fn start_worker(style_json: Option<String>) -> Result<Self> {
        RasterTiles::init()?;
        let (sender, receiver) = mpsc::channel::<RenderRequest>();
        let (frame_sender, frames) = async_channel::bounded(1);
        let pending_frame = frames.clone();
        let (queries, query_receiver) = async_channel::bounded(1);
        let pending_query = query_receiver.clone();
        let (feature_sender, feature_results) = async_channel::bounded(1);
        let pending_result = feature_results.clone();
        // The !Send renderer and its run loop remain on this dedicated thread.
        // Dropping the view closes the channel; the worker finishes its bounded
        // in-flight request, drops all native resources, and exits.
        std::thread::Builder::new()
            .name("map-renderer".into())
            .spawn(move || {
                run_renderer(
                    receiver,
                    frame_sender,
                    pending_frame,
                    query_receiver,
                    feature_sender,
                    pending_result,
                    style_json,
                )
            })
            .context("Could not start the map rendering thread")?;
        Ok(Self {
            sender,
            frames,
            queries,
            pending_query,
            feature_results,
        })
    }

    pub fn render_layer(
        &self,
        map: MapRequest,
        source: Option<MapSource>,
        opacity: f32,
        reduce_motion: bool,
        basemap: BasemapStyle,
        tile_boundaries: bool,
    ) -> Result<()> {
        let (raster, playback, vector, vector_color) = match source {
            Some(MapSource::Raster(source)) => (Some(source), None, None, String::new()),
            Some(MapSource::Multiband(source, playback)) => (Some(source), Some(playback), None, String::new()),
            Some(MapSource::Vector(source, color)) => (None, None, Some(source), color),
            None => (None, None, None, String::new()),
        };
        self.sender
            .send(RenderRequest {
                map,
                raster,
                playback,
                vector,
                vector_color,
                opacity,
                reduce_motion,
                basemap,
                tile_boundaries,
                enqueued_at: log::log_enabled!(target: "map::vector_load", log::Level::Info).then(Instant::now),
            })
            .context("Map rendering thread stopped")?;
        Ok(())
    }

    #[cfg(test)]
    pub fn render_raster(
        &self,
        map: MapRequest,
        raster: Option<Arc<RasterSource>>,
        opacity: f32,
        reduce_motion: bool,
        basemap: BasemapStyle,
        tile_boundaries: bool,
    ) -> Result<()> {
        self.render_layer(map, raster.map(MapSource::Raster), opacity, reduce_motion, basemap, tile_boundaries)
    }

    #[cfg(test)]
    pub fn render(&self, request: MapRequest) -> Result<()> {
        self.render_raster(request, None, 1.0, false, BasemapStyle::Dark, false)
    }

    pub fn frames(&self) -> async_channel::Receiver<MapFrame> {
        self.frames.clone()
    }

    pub fn feature_results(&self) -> async_channel::Receiver<FeatureQueryResult> {
        self.feature_results.clone()
    }

    pub fn query_features(&self, revision: u64, map: MapRequest, layer_id: u64, point: (f64, f64)) {
        self.queue_query(Some(FeatureQuery {
            revision,
            map,
            layer_id,
            point,
        }));
    }

    pub fn clear_feature_query(&self) {
        self.queue_query(None);
    }

    fn queue_query(&self, query: Option<FeatureQuery>) {
        if let Err(async_channel::TrySendError::Full(query)) = self.queries.try_send(query) {
            let _ = self.pending_query.try_recv();
            let _ = self.queries.try_send(query);
        }
    }
}

fn run_renderer(
    requests: mpsc::Receiver<RenderRequest>,
    frames: async_channel::Sender<MapFrame>,
    pending_frame: async_channel::Receiver<MapFrame>,
    queries: async_channel::Receiver<Option<FeatureQuery>>,
    feature_results: async_channel::Sender<FeatureQueryResult>,
    pending_result: async_channel::Receiver<FeatureQueryResult>,
    style_json: Option<String>,
) {
    let mut map = None;
    let mut current_request: Option<RenderRequest> = None;
    let mut next_frame = Instant::now();
    let mut query_progress = FeatureQueryProgress {
        queries,
        results: feature_results,
        pending_result,
        query: None,
        previous_features: None,
    };
    let mut progress = FrameProgress::default();

    loop {
        let timeout = map.as_ref().map(|_| next_frame.saturating_duration_since(Instant::now()));
        let Ok(request) = receive_latest_request(&requests, timeout) else {
            break;
        };
        let request_changed = request.is_some();
        if let Some(request) = request {
            log_vector_request(&request, current_request.as_ref());
            // Even unchanged source IDs can accompany a new camera or style.
            progress.reset();
            current_request = Some(request);
        }
        let Some(current_request) = &mut current_request else { continue };

        let frame_started = Instant::now();
        let image = poll_map_image(&mut map, current_request, request_changed, next_frame, style_json.as_deref());
        query_progress.update(map.as_mut(), matches!(&image, Ok(Some(_))));
        let Some((image, settled)) = progress.update_result(&mut map, image) else {
            // Poll native resource/fade callbacks without busy-waiting.
            if Instant::now() >= next_frame {
                next_frame = Instant::now() + Duration::from_millis(16);
            }
            continue;
        };
        let frame = MapFrame {
            viewport: current_request.map.viewport,
            basemap: current_request.basemap,
            tile_boundaries: current_request.tile_boundaries,
            raster_layer_id: current_request.raster.as_ref().map(|source| source.metadata().id),
            vector_layer_id: current_request.vector.as_ref().map(|source| source.id()),
            settled,
            playback: current_request.playback_frame(map.as_ref()),
            image,
        };
        if let Err(async_channel::TrySendError::Full(frame)) = frames.try_send(frame) {
            // Keep one newest completed frame if GPUI is still busy.
            let _ = pending_frame.try_recv();
            let _ = frames.try_send(frame);
        }
        next_frame = frame_started + Duration::from_millis(16);
    }
}

fn log_vector_request(request: &RenderRequest, previous: Option<&RenderRequest>) {
    if let Some(source) = &request.vector
        && previous.and_then(|request| request.vector.as_ref().map(|source| source.id())) != Some(source.id())
        && let Some(enqueued_at) = request.enqueued_at
    {
        log::info!(target: "map::vector_load",
            "source={} renderer received new vector request: layer={:?}, queue_delay={:?} (latest retained request)",
            source.id(), source.name(), enqueued_at.elapsed());
    }
}

impl FeatureQuery {
    fn matches_map(&self, map: &NativeMap) -> bool {
        map.viewport == self.map.viewport && map.camera == Some(self.map.camera) && map.vector.as_ref().is_some_and(|source| source.id() == self.layer_id)
    }
}

struct FeatureQueryProgress {
    queries: async_channel::Receiver<Option<FeatureQuery>>,
    results: async_channel::Sender<FeatureQueryResult>,
    pending_result: async_channel::Receiver<FeatureQueryResult>,
    query: Option<FeatureQuery>,
    previous_features: Option<Result<Vec<serde_json::Value>, String>>,
}

impl FeatureQueryProgress {
    fn update(&mut self, map: Option<&mut NativeMap>, image_changed: bool) {
        let mut query_changed = false;
        while let Ok(latest) = self.queries.try_recv() {
            self.query = latest;
            self.previous_features = None;
            query_changed = true;
        }
        // Re-query after streamed tiles repaint, even if the pointer is stationary.
        if !query_changed && !image_changed {
            return;
        }
        let (Some(map), Some(query)) = (map, &self.query) else { return };
        if !query.matches_map(map) {
            return;
        }
        let features = map.query_vector_features(query.point).map_err(|error| format!("{error:#}"));
        if self.previous_features.as_ref() == Some(&features) {
            return;
        }
        self.previous_features = Some(features.clone());
        let result = FeatureQueryResult {
            revision: query.revision,
            features,
        };
        if let Err(async_channel::TrySendError::Full(result)) = self.results.try_send(result) {
            let _ = self.pending_result.try_recv();
            let _ = self.results.try_send(result);
        }
    }
}

// Retain only the newest image for this request, sharing its pixels with GPUI.
// An idle callback can arrive on a later tick without producing another texture.
#[derive(Default)]
struct FrameProgress {
    latest: Option<Arc<RenderImage>>,
    settled: bool,
}

impl FrameProgress {
    fn reset(&mut self) {
        self.latest = None;
        self.settled = false;
    }

    fn update_result(&mut self, map: &mut Option<NativeMap>, image: Result<Option<Arc<RenderImage>>>) -> Option<(Result<Arc<RenderImage>>, bool)> {
        match image {
            Ok(image) => self
                .update(image, map.as_ref().is_some_and(NativeMap::is_settled))
                .map(|(image, settled)| (Ok(image), settled)),
            Err(error) => {
                *map = None;
                self.reset();
                Some((Err(error), false))
            }
        }
    }

    fn update(&mut self, image: Option<Arc<RenderImage>>, settled: bool) -> Option<(Arc<RenderImage>, bool)> {
        let became_settled = settled && !self.settled;
        self.settled = settled;
        if let Some(image) = image {
            self.latest = Some(image.clone());
            return Some((image, settled));
        }
        if became_settled {
            return self.latest.clone().map(|image| (image, true));
        }
        None
    }
}

fn receive_latest_request(requests: &mpsc::Receiver<RenderRequest>, timeout: Option<Duration>) -> Result<Option<RenderRequest>, mpsc::RecvTimeoutError> {
    let incoming = match timeout {
        Some(timeout) => requests.recv_timeout(timeout),
        None => requests.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
    };
    let mut request = match incoming {
        Ok(request) => Some(request),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(error) => return Err(error),
    };
    for latest in requests.try_iter() {
        request = Some(latest);
    }
    Ok(request)
}

fn poll_map_image(
    map: &mut Option<NativeMap>,
    request: &mut RenderRequest,
    mut request_changed: bool,
    next_frame: Instant,
    style_json: Option<&str>,
) -> Result<Option<Arc<RenderImage>>> {
    request.resume_playback(map.as_ref());
    if map.as_ref().is_none_or(|map| map.viewport.scale != request.map.viewport.scale) {
        *map = Some(initialize_map(request, style_json)?);
        request_changed = true;
    }
    let map = map.as_mut().context("Map renderer unavailable")?;
    if request_changed {
        map.apply_render_request(request)?;
    }
    if Instant::now() < next_frame {
        return Ok(None);
    }
    let Some(texture) = map.poll_frame()? else { return Ok(None) };
    let pixels = map.read_frame(&texture)?;
    Ok(Some(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]))))
}

fn initialize_map(request: &RenderRequest, style_json: Option<&str>) -> Result<NativeMap> {
    let started = request.enqueued_at.map(|_| Instant::now());
    let map = match style_json {
        Some(json) => NativeMap::with_style_source(request.map.viewport, request.basemap, Some(json))?,
        None => NativeMap::with_basemap(request.map.viewport, request.basemap)?,
    };
    if let (Some(source), Some(started)) = (&request.vector, started) {
        log::info!(target: "map::vector_load", "source={} renderer initialization and basemap style load: elapsed={:?}",
            source.id(), started.elapsed());
    }
    Ok(map)
}

// Style reloads deinitialize native hosts, but this renderer keeps the same
// device and owns the raster state. Let source changes or NativeMap's drop free
// that state instead, retaining numeric tiles and in-flight jobs across reloads.
struct RetainedRasterLayerHost(RasterLayerHost);

impl WgpuCustomLayer for RetainedRasterLayerHost {
    fn render(&mut self, pass: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
        self.0.render(pass, parameters);
    }
}

// Enable with RUST_LOG=map::vector_load=info. Track installation through the
// first native idle only, never ordinary navigation/restyling frames. Pixel
// evidence belongs in the local timing test: native idle and source IDs alone
// cannot tell whether an on-screen frame actually contains vector geometry.
struct VectorLoadTiming {
    source_id: u64,
    started: Instant,
    frames: u64,
    readbacks: u64,
    rendering: Duration,
    readback: Duration,
}

impl VectorLoadTiming {
    fn new(source: &VectorSource) -> Self {
        log::info!(target: "map::vector_load", "source={} vector installation started: layer={:?}, geojson_bytes={:?}",
            source.id(), source.name(), source.geojson().map(str::len));
        Self {
            source_id: source.id(),
            started: Instant::now(),
            frames: 0,
            readbacks: 0,
            rendering: Duration::ZERO,
            readback: Duration::ZERO,
        }
    }
}

struct NativeMap {
    renderer: ImageRenderer<Continuous>,
    basemap: BasemapStyle,
    viewport: Viewport,
    camera: Option<MapCamera>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    idle: Rc<Cell<bool>>,
    needs_frame: Rc<Cell<bool>>,
    readback: GpuReadback,
    deadline: Instant,
    raster: Option<Arc<RasterSource>>,
    opacity: f32,
    playback_playing: bool,
    vector: Option<Arc<VectorSource>>,
    vector_opacity: f32,
    vector_color: String,
    vector_load: Option<VectorLoadTiming>,
    numeric: Rc<RefCell<RasterLayer>>,
    #[cfg(test)]
    camera_updates: u64,
    #[cfg(test)]
    vector_source_updates: u64,
}

impl NativeMap {
    #[cfg(test)]
    fn new(viewport: Viewport) -> Result<Self> {
        Self::with_basemap(viewport, BasemapStyle::Dark)
    }

    fn with_basemap(viewport: Viewport, basemap: BasemapStyle) -> Result<Self> {
        Self::with_style_source(viewport, basemap, None)
    }

    #[cfg(test)]
    fn with_style(viewport: Viewport, json: Option<&str>) -> Result<Self> {
        Self::with_style_source(viewport, BasemapStyle::Dark, json)
    }

    fn with_style_source(viewport: Viewport, basemap: BasemapStyle, json: Option<&str>) -> Result<Self> {
        RasterTiles::init()?;
        // The wgpu FFI backend requires a host-supplied device and queue. This
        // initialization can later accept GPUI's device instead of creating one.
        let (device, queue) = futures_lite::future::block_on(async {
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
                .request_adapter(&wgpu::RequestAdapterOptions::default())
                .await
                .context("Could not request wgpu adapter")?
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("MapLibre device"),
                    ..Default::default()
                })
                .await
                .context("Could not create the MapLibre wgpu device")
        })?;
        let mut renderer = ImageRendererBuilder::new()
            .with_size(viewport.width, viewport.height)
            .with_pixel_ratio(viewport.scale)
            .build_continuous_renderer();
        renderer.set_device_queue(device.clone(), queue.clone());
        let idle = Rc::new(Cell::new(false));
        renderer.map_observer().set_did_become_idle_callback({
            let idle = idle.clone();
            move || idle.set(true)
        });
        let needs_frame = Rc::new(Cell::new(true));
        renderer.set_render_requested_callback({
            let needs_frame = needs_frame.clone();
            move || needs_frame.set(true)
        });
        let numeric = Rc::new(RefCell::new(RasterLayer::new(device.clone(), queue.clone())?));
        let mut map = Self {
            renderer,
            basemap,
            viewport,
            camera: None,
            readback: GpuReadback::new(&device),
            device,
            queue,
            idle,
            needs_frame,
            deadline: Instant::now() + Duration::from_secs(30),
            raster: None,
            opacity: 1.0,
            playback_playing: false,
            vector: None,
            vector_opacity: 1.0,
            vector_color: String::new(),
            vector_load: None,
            numeric,
            #[cfg(test)]
            camera_updates: 0,
            #[cfg(test)]
            vector_source_updates: 0,
        };
        map.load_style(basemap, json)?;
        Ok(map)
    }

    fn set_basemap(&mut self, basemap: BasemapStyle) -> Result<()> {
        if self.basemap != basemap {
            self.load_style(basemap, None)?;
        }
        Ok(())
    }

    fn load_style(&mut self, basemap: BasemapStyle, json: Option<&str>) -> Result<()> {
        log::debug!("Loading background map style: {basemap:?}");
        self.idle.set(false);
        let run_loop = RunLoopHandle::current();
        let generated = basemap.style_json();
        let style = match json.or(generated.as_deref()) {
            Some(json) => self.renderer.load_style_from_json_str(json),
            None => self
                .renderer
                .load_style_from_url(&basemap.style_url().context("Missing background style URL")?.parse()?),
        };
        wait_until_ready(&run_loop, Instant::now() + Duration::from_secs(30), || style.is_ready())?;
        style.finish().context("Could not load the background map style")?;
        self.basemap = basemap;
        // Loading a style removes its sources and layers, not the native camera.
        // Reset only attachment bookkeeping so overlays are reinstalled without
        // clearing the retained numeric state, even at unchanged opacity.
        let raster = self.raster.take();
        self.set_raster(raster, self.opacity)?;
        let vector = self.vector.take();
        self.set_vector(vector, self.vector_opacity, &self.vector_color.clone())?;
        self.idle.set(false);
        self.needs_frame.set(true);
        self.deadline = Instant::now() + Duration::from_secs(30);
        Ok(())
    }

    fn apply_render_request(&mut self, request: &RenderRequest) -> Result<()> {
        {
            let mut numeric = self.numeric.borrow_mut();
            numeric.set_reduce_motion(request.reduce_motion);
            numeric.set_debug(request.tile_boundaries);
        }
        self.set_basemap(request.basemap)?;
        self.renderer.set_debug_flags(if request.tile_boundaries {
            MapDebugOptions::TileBorders
        } else {
            MapDebugOptions::NoDebug
        });
        // The displayed band/style is not a static source change during playback.
        // Keep attachment identity anchored to the immutable dataset instead.
        let raster = request
            .playback
            .as_ref()
            .map_or_else(|| request.raster.clone(), |playback| playback.bands().first().cloned());
        self.set_raster(raster, request.opacity)?;
        self.numeric.borrow_mut().set_playback(request.playback.clone(), request.opacity)?;
        self.playback_playing = request.playback.as_ref().is_some_and(PlaybackRequest::is_playing);
        self.set_vector(request.vector.clone(), request.opacity, &request.vector_color)?;
        self.apply_request(request.map);
        Ok(())
    }

    fn set_raster(&mut self, raster: Option<Arc<RasterSource>>, opacity: f32) -> Result<()> {
        let style_changed = self.raster.as_ref().map(|source| source.id()) != raster.as_ref().map(|source| source.id());
        if !style_changed && self.opacity == opacity {
            return Ok(());
        }
        let changed = self.raster.as_ref().map(|source| source.metadata().id) != raster.as_ref().map(|source| source.metadata().id);
        let retain_numeric = self.raster.as_ref().is_some_and(|source| source.is_numeric()) && raster.as_ref().is_some_and(|source| source.is_numeric());
        let dpi = (self.viewport.scale.ceil() as u8).clamp(1, 4);
        let numeric_source = raster.as_ref().filter(|source| source.is_numeric()).cloned();
        self.update_raster_attachment(raster.as_ref(), opacity, dpi, changed && !retain_numeric)?;
        self.numeric.borrow_mut().set_source(numeric_source, opacity, dpi);
        self.raster = raster;
        self.opacity = opacity;
        Ok(())
    }

    fn update_raster_attachment(&mut self, source: Option<&Arc<RasterSource>>, opacity: f32, dpi: u8, replace: bool) -> Result<()> {
        if let Some(source) = source.filter(|source| !source.is_numeric()) {
            RasterTiles::init()?.register(source)?;
        }
        if replace {
            self.renderer.style().remove_layer("local-raster-layer");
            self.renderer.style().remove_source("local-raster");
            if let Some(source) = source {
                self.add_raster_attachment(source, opacity, dpi)?;
            }
        } else if let Some(source) = source.filter(|source| !source.is_numeric()) {
            self.renderer.style().remove_layer("local-raster-layer");
            self.renderer.style().add_layer(AnyLayer::from_json_value(&source.layer_json(opacity))?)?;
        }
        Ok(())
    }

    fn add_raster_attachment(&mut self, source: &RasterSource, opacity: f32, dpi: u8) -> Result<()> {
        if source.is_numeric() {
            self.renderer
                .style()
                .add_wgpu_custom_layer("local-raster-layer", RetainedRasterLayerHost(RasterLayerHost::new(self.numeric.clone())), None)?;
        } else {
            self.renderer
                .style()
                .add_source(AnySource::from_json_value("local-raster", &source.source_json(dpi))?)?;
            self.renderer.style().add_layer(AnyLayer::from_json_value(&source.layer_json(opacity))?)?;
        }
        Ok(())
    }

    fn query_vector_features(&mut self, point: (f64, f64)) -> Result<Vec<serde_json::Value>> {
        let Some(source) = &self.vector else { return Ok(Vec::new()) };
        let layers = ["fill", "line", "circle"].map(|kind| format!("local-vector-{}-{kind}", source.id()));
        // MapLibre uses logical pixels from the viewport's top-left, independent
        // of the physical GPU readback size and its DPI scale.
        let point = ScreenCoordinate { x: point.0, y: point.1 };
        if source.points_only() {
            const HOVER_TOLERANCE: f64 = 4.0;
            Ok(self.renderer.query_rendered_features_with_tolerance(point, HOVER_TOLERANCE, &layers)?)
        } else {
            // Expanding polygon/line queries also hits neighboring geometries.
            Ok(self.renderer.query_rendered_features(point, &layers)?)
        }
    }

    fn set_vector(&mut self, vector: Option<Arc<VectorSource>>, opacity: f32, color: &str) -> Result<()> {
        let changed = self.vector.as_ref().map(|source| source.id()) != vector.as_ref().map(|source| source.id());
        let style_changed = self.vector.as_ref().map(|source| source.style_id()) != vector.as_ref().map(|source| source.style_id());
        if !changed && !style_changed && self.vector_opacity == opacity && self.vector_color == color {
            return Ok(());
        }
        if changed {
            self.vector_load = vector
                .as_deref()
                .filter(|_| log::log_enabled!(target: "map::vector_load", log::Level::Info))
                .map(VectorLoadTiming::new);
        }
        let installation_started = self.vector_phase_started().filter(|_| changed);
        self.remove_vector_layers();
        if changed {
            self.replace_vector_source(vector.as_deref())?;
        }
        self.add_vector_layers(vector.as_deref(), opacity, color, installation_started)?;
        self.log_vector_phase("vector installation complete (not yet rendered)", installation_started);
        self.vector = vector;
        self.vector_opacity = opacity;
        self.vector_color = color.to_owned();
        Ok(())
    }

    fn add_vector_layers(&mut self, source: Option<&VectorSource>, opacity: f32, color: &str, installation_started: Option<Instant>) -> Result<()> {
        if let Some(source) = source {
            let started = installation_started.map(|_| Instant::now());
            for layer in source.layers_json(opacity, color) {
                self.renderer.style().add_layer(AnyLayer::from_json_value(&layer)?)?;
            }
            self.log_vector_phase("layer creation/add", started);
        }
        Ok(())
    }

    fn remove_vector_layers(&mut self) {
        if let Some(source) = &self.vector {
            for kind in ["fill", "line", "circle"] {
                self.renderer.style().remove_layer(format!("local-vector-{}-{kind}", source.id()));
            }
        }
    }

    fn replace_vector_source(&mut self, source: Option<&VectorSource>) -> Result<()> {
        #[cfg(test)]
        {
            self.vector_source_updates += 1;
        }
        self.renderer.style().remove_source("local-vector");
        let Some(source) = source else { return Ok(()) };
        if let Some(json) = source.geojson() {
            let started = self.vector_phase_started();
            let data = GeoJson::from_json_str(json)?;
            self.log_vector_phase("GeoJson::from_json_str", started);
            let mut native_source = GeoJsonSource::new("local-vector");
            let started = self.vector_phase_started();
            native_source.set_geojson(&data);
            self.log_vector_phase("GeoJsonSource::set_geojson (including synchronous native copy)", started);
            let started = self.vector_phase_started();
            self.renderer.style().add_source(native_source)?;
            self.log_vector_phase("style source add", started);
            let started = self.vector_phase_started();
            drop(data);
            self.log_vector_phase("temporary native GeoJson drop", started);
        } else if let Some(url) = source.tile_url() {
            let started = self.vector_phase_started();
            self.renderer.style().add_source(AnySource::from_json_value(
                "local-vector",
                &serde_json::json!({ "type": "vector", "url": url }),
            )?)?;
            self.log_vector_phase("tiled style source creation/add", started);
        }
        Ok(())
    }

    #[cfg(test)]
    fn render(&mut self, request: MapRequest) -> Result<RgbaImage> {
        let texture = self.render_texture(request)?;
        self.read_frame(&texture)
    }

    #[cfg(test)]
    fn render_raster(&mut self, request: MapRequest, source: Option<Arc<RasterSource>>, opacity: f32) -> Result<RgbaImage> {
        self.set_raster(source, opacity)?;
        self.render(request)
    }

    #[cfg(test)]
    fn render_vector(&mut self, request: MapRequest, source: Option<Arc<VectorSource>>, opacity: f32, color: &str) -> Result<RgbaImage> {
        self.set_vector(source, opacity, color)?;
        self.render(request)
    }

    #[cfg(test)]
    fn required_raster_image(&mut self, message: &str) -> Result<RgbaImage> {
        self.poll_raster_image()?.context(message.to_owned())
    }

    #[cfg(test)]
    fn poll_raster_image(&mut self) -> Result<Option<RgbaImage>> {
        let Some(texture) = self.poll_frame()? else { return Ok(None) };
        self.readback.read(&texture, &self.device, &self.queue).map(Some)
    }

    fn vector_phase_started(&self) -> Option<Instant> {
        self.vector_load.as_ref().map(|_| Instant::now())
    }

    fn log_vector_phase(&self, phase: &str, started: Option<Instant>) {
        if let (Some(timing), Some(started)) = (&self.vector_load, started) {
            log::info!(target: "map::vector_load", "source={} {phase}: elapsed={:?}, since_install={:?}",
                timing.source_id, started.elapsed(), timing.started.elapsed());
        }
    }

    fn read_frame(&mut self, texture: &wgpu::Texture) -> Result<RgbaImage> {
        let started = self.vector_phase_started();
        let pixels = self.readback.read(texture, &self.device, &self.queue)?;
        if let (Some(timing), Some(started)) = (&mut self.vector_load, started) {
            let elapsed = started.elapsed();
            timing.readbacks += 1;
            timing.readback += elapsed;
            if timing.readbacks == 1 {
                log::info!(target: "map::vector_load",
                    "source={} first frame readback (vector pixels NOT verified; may be basemap-only): elapsed={:?}, since_install={:?}",
                    timing.source_id, elapsed, timing.started.elapsed());
            }
        }
        self.finish_vector_timing_if_idle();
        Ok(pixels)
    }

    fn is_settled(&self) -> bool {
        self.idle.get() && !self.needs_frame.get() && !self.numeric.borrow().is_loading()
    }

    fn finish_vector_timing_if_idle(&mut self) {
        if self.vector_load.as_ref().is_some_and(|timing| timing.readbacks > 0)
            && self.is_settled()
            && let Some(timing) = self.vector_load.take()
        {
            log::info!(target: "map::vector_load",
                "source={} native idle observed: since_install={:?}, frames={}, readbacks={}, native_render_total={:?}, readback_total={:?} \
                 (current viewport resources/fades settled; not pixel evidence or UI presentation)",
                timing.source_id, timing.started.elapsed(), timing.frames, timing.readbacks, timing.rendering, timing.readback);
        }
    }

    #[cfg(test)]
    fn render_texture(&mut self, request: MapRequest) -> Result<wgpu::Texture> {
        self.apply_request(request);
        let mut latest = None;
        while self.needs_frame.get() || !self.idle.get() || self.numeric.borrow().is_loading() {
            if let Some(texture) = self.poll_frame()? {
                latest = Some(texture)
            }
            if !self.idle.get() || self.numeric.borrow().is_loading() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        latest.context("MapLibre did not produce a wgpu texture")
    }

    fn apply_request(&mut self, request: MapRequest) {
        if self.viewport != request.viewport || self.camera != Some(request.camera) {
            self.idle.set(false);
            if self.viewport != request.viewport {
                self.renderer.set_map_size(Size {
                    width: request.viewport.width.get(),
                    height: request.viewport.height.get(),
                });
                self.viewport = request.viewport;
            }
            self.renderer.update_camera(&request.camera.update());
            self.camera = Some(request.camera);
            #[cfg(test)]
            {
                self.camera_updates += 1;
            }
        }
        // Shader-only changes repaint the retained scene without invalidating
        // the native camera, tile layout, or label placement.
        self.needs_frame.set(true);
        self.deadline = Instant::now() + Duration::from_secs(30);
    }

    fn record_vector_render_time(&mut self, started: Option<Instant>) {
        if let (Some(timing), Some(started)) = (&mut self.vector_load, started) {
            let elapsed = started.elapsed();
            timing.frames += 1;
            timing.rendering += elapsed;
            if timing.frames == 1 {
                log::info!(target: "map::vector_load",
                    "source={} first native frame render/take_texture (includes run-loop dispatch; vector pixels NOT verified): elapsed={:?}, since_install={:?}",
                    timing.source_id, elapsed, timing.started.elapsed());
            }
        }
    }

    fn poll_frame(&mut self) -> Result<Option<wgpu::Texture>> {
        let started = self.vector_phase_started();
        RunLoopHandle::current().tick();
        if self.numeric.borrow_mut().poll_tiles() {
            self.needs_frame.set(true);
        }
        // Dispatch a pending idle callback before judging the deadline. Keep the
        // deadline fixed so stalled native work or repeated repaints still fail.
        if self.playback_playing && self.idle.get() && !self.numeric.borrow().is_loading() {
            // Only active playback with native resources and tile work idle may
            // renew the deadline. Paused or stalled prefetch cannot mask a timeout.
            self.deadline = Instant::now() + Duration::from_secs(30);
        }
        ensure!(self.is_settled() || Instant::now() < self.deadline, "Map rendering timed out");
        if self.needs_frame.replace(false) {
            self.idle.set(false);
            self.renderer.render_once();
            let texture = self.renderer.take_texture().context("MapLibre did not produce a wgpu texture")?;
            self.record_vector_render_time(started);
            return Ok(Some(texture));
        }
        self.finish_vector_timing_if_idle();
        Ok(None)
    }
}

fn wait_until_ready(run_loop: &RunLoopHandle, deadline: Instant, is_ready: impl Fn() -> bool) -> Result<()> {
    while !is_ready() {
        ensure!(Instant::now() < deadline, "Map loading timed out. Check your connection and retry.");
        run_loop.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(test)]
#[path = "map_renderer_playback_tests.rs"]
mod playback_tests;

#[cfg(test)]
#[path = "map_renderer_progress_tests.rs"]
mod progress_tests;

#[cfg(test)]
#[path = "map_renderer_vector_timing_tests.rs"]
mod vector_timing_tests;

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const DARK_STYLE: &str =
        r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##;
    const LIGHT_STYLE: &str =
        r##"{"version":8,"center":[120,45],"zoom":12,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#f0f0f0"}}]}"##;

    #[test]
    fn basemap_style_urls() {
        assert_eq!(
            [BasemapStyle::Light.style_url(), BasemapStyle::Dark.style_url()],
            [
                Some("https://basemaps.cartocdn.com/gl/positron-gl-style/style.json"),
                Some("https://basemaps.cartocdn.com/gl/dark-matter-gl-style/style.json"),
            ]
        );
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; local tile data and styles, no network"]
    fn tile_background_switches_preserve_vector_overlay_and_camera() -> Result<()> {
        use gpui_kit::component::select::SelectItem;

        let directory = tempfile::tempdir()?;
        let basemap = crate::background::choices()
            .iter()
            .filter_map(|choice| *choice.value())
            .find(|style| matches!(style, BasemapStyle::Tiles(_)))
            .unwrap();
        let (_tiles, style) = local_tile_background_fixture(directory.path(), basemap)?;
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::default(),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        map.render(request)?;
        let camera_updates = map.camera_updates;
        map.load_style(basemap, Some(&style))?;
        let background = map.render(request)?;
        assert_eq!(background.get_pixel(128, 128).0, [0, 255, 0, 255]);
        let vector = open_mixed_vector_fixture(directory.path())?;
        let rendered = map.render_vector(request, Some(vector.clone()), 1.0, "#ff00ff")?;
        assert_ne!(rendered.as_raw(), background.as_raw());
        assert_tile_background_roundtrip(&mut map, request, vector.id(), basemap, &style, &rendered)?;
        assert_eq!(map.camera_updates, camera_updates);
        Ok(())
    }

    fn local_tile_background_fixture(directory: &std::path::Path, basemap: BasemapStyle) -> Result<(Arc<RasterSource>, String)> {
        let path = directory.join("background.mbtiles");
        write_color_mbtiles_fixture(&path)?;
        let tiles = RasterSource::open(&path)?.remove(0);
        RasterTiles::init()?.register(&tiles)?;
        let mut style: serde_json::Value = serde_json::from_str(&basemap.style_json().unwrap())?;
        style["sources"]["background-tiles"] = tiles.source_json(1);
        Ok((tiles, style.to_string()))
    }

    fn assert_tile_background_roundtrip(
        map: &mut NativeMap,
        request: MapRequest,
        vector_id: u64,
        basemap: BasemapStyle,
        style: &str,
        rendered: &RgbaImage,
    ) -> Result<()> {
        map.set_basemap(BasemapStyle::Empty)?;
        let empty = map.render(request)?;
        assert_ne!(empty.as_raw(), rendered.as_raw());
        assert_eq!(map.vector.as_ref().map(|source| source.id()), Some(vector_id));
        map.load_style(basemap, Some(style))?;
        assert_eq!(map.render(request)?.as_raw(), rendered.as_raw());
        map.load_style(BasemapStyle::Dark, Some(DARK_STYLE))?;
        assert_eq!(map.vector.as_ref().map(|source| source.id()), Some(vector_id));
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn basemap_reload_preserves_camera_and_cached_numeric_raster() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("raster.tif");
        let source = open_raster_fixture(&path, 4326, 1)?.remove(0);
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.metadata().bounds, 512.0, 512.0),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        let dark_background = map.render(request)?;
        let dark_raster = map.render_raster(request, Some(source.clone()), 0.65)?;
        let light_raster = assert_numeric_basemap_roundtrip(&mut map, request, &dark_raster, &dark_background, &path)?;
        map.load_style(BasemapStyle::Light, Some(LIGHT_STYLE))?;
        let light_background = map.render_raster(request, None, 1.0)?;
        assert!(light_background.pixels().all(|pixel| pixel.0 == [240, 240, 240, 255]));
        let covered = light_raster.pixels().zip(light_background.pixels()).filter(|(a, b)| a != b).count();
        assert!(
            covered > 25_000,
            "The numeric overlay must be reattached above the light basemap, got {covered} pixels"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn basemap_reload_reattaches_color_raster_source_and_layer() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("image.mbtiles");
        write_color_mbtiles_fixture(&path)?;
        let source = RasterSource::open(&path)?.remove(0);
        assert!(!source.is_numeric());
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::default(),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        let before = map.render_raster(request, Some(source), 1.0)?;
        assert_eq!(before.get_pixel(128, 128).0, [0, 255, 0, 255]);
        let camera_updates = map.camera_updates;

        map.load_style(BasemapStyle::Light, Some(LIGHT_STYLE))?;
        let after = map.render(request)?;
        assert_eq!(
            after.as_raw(),
            before.as_raw(),
            "The color source and layer must survive a basemap change at the same camera"
        );
        assert_eq!(map.camera_updates, camera_updates);
        let cleared = map.render_raster(request, None, 1.0)?;
        assert!(cleared.pixels().all(|pixel| pixel.0 == [240, 240, 240, 255]));
        Ok(())
    }

    fn assert_mixed_vector_coverage(rendered: &RgbaImage, background: &RgbaImage, camera: MapCamera) {
        let mut covered = [0; 3];
        for (x, y, pixel) in rendered.enumerate_pixels() {
            if pixel == background.get_pixel(x, y) {
                continue;
            }
            let longitude = camera.coordinate_at((f64::from(x) - 256.0, f64::from(y) - 256.0)).longitude;
            let geometry = if longitude < -1.0 {
                0
            } else if longitude > 1.0 {
                2
            } else {
                1
            };
            covered[geometry] += 1;
        }
        assert!(covered[0] > 1_000, "Polygon fill must produce pixels: {covered:?}");
        assert!(covered[1] > 100, "Line geometry must produce pixels: {covered:?}");
        assert!(covered[2] > 10, "Point geometry must produce pixels: {covered:?}");
    }

    fn assert_vector_opacity_roundtrip(
        map: &mut NativeMap,
        request: MapRequest,
        source: &Arc<VectorSource>,
        color: &str,
        translucent: &RgbaImage,
        background: &RgbaImage,
    ) -> Result<()> {
        map.set_vector(Some(source.clone()), 0.0, color)?;
        assert_eq!(map.render(request)?.as_raw(), background.as_raw(), "Zero opacity must reveal the basemap");
        map.set_vector(Some(source.clone()), 0.5, color)?;
        assert_eq!(map.render(request)?.as_raw(), translucent.as_raw());
        Ok(())
    }

    fn count_color(image: &RgbaImage, color: [u8; 4]) -> usize {
        image.pixels().filter(|pixel| pixel.0 == color).count()
    }

    fn assert_numeric_vector_palette(first: &RgbaImage) {
        assert!(
            count_color(first, [0, 0, 0, 255]) > 1_000,
            "Low-value polygon must use the palette's first color"
        );
        assert!(
            count_color(first, [255, 255, 255, 255]) > 5,
            "High-value point must use the palette's last color"
        );
        assert!(
            count_color(first, [255, 0, 255, 255]) > 10,
            "Null and missing values must use the fallback color"
        );
    }

    fn assert_numeric_vector_property_change(first: &RgbaImage, reversed: &RgbaImage) {
        assert!(
            count_color(reversed, [255, 255, 255, 255]) > 1_000,
            "Changing property must recolor the polygon"
        );
        assert!(count_color(reversed, [0, 0, 0, 255]) > 5, "Changing property must recolor the point");
        assert_eq!(count_color(first, [255, 0, 255, 255]), count_color(reversed, [255, 0, 255, 255]));
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local vector data"]
    fn local_vectors_render_restyle_switch_clear_and_survive_basemap_reload_without_network() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = open_mixed_vector_fixture(dir.path())?;
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.bounds(), 512.0, 512.0),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        let background = map.render(request)?;
        let rendered = map.render_vector(request, Some(source.clone()), 1.0, "#3b82f6")?;
        assert_mixed_vector_coverage(&rendered, &background, request.camera);
        assert_vector_restyles_survive_basemap_reload(&mut map, request, &source, &rendered, &background)?;
        assert_vector_switch_clear_and_reinstall(&mut map, request, source, dir.path(), &background, &rendered)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local vector data"]
    fn numeric_vectors_render_property_palette_and_range_changes_without_reloading_sources() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = open_numeric_vector_fixture(directory.path())?;
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds([-8.0, -4.0, 8.0, 4.0], 512.0, 512.0),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        let background = map.render(request)?;
        let first = map.render_vector(request, Some(source.clone()), 1.0, "#ff00ff")?;

        assert_numeric_vector_palette(&first);
        let camera_updates = map.camera_updates;
        let source_updates = map.vector_source_updates;

        let (colored, palette_changed) = assert_numeric_vector_color_restyles(&mut map, request, &source, &first)?;
        let translucent = map.render_vector(request, Some(colored.clone()), 0.5, "#ff00ff")?;
        assert_ne!(translucent.as_raw(), palette_changed.as_raw());
        assert_vector_opacity_roundtrip(&mut map, request, &colored, "#ff00ff", &translucent, &background)?;
        assert_eq!(map.vector_source_updates, source_updates, "Restyling must not replace the native source");
        assert_eq!(map.camera_updates, camera_updates, "Restyling must not move the native camera");

        assert_numeric_vector_basemap_reload(&mut map, request, &translucent, camera_updates)?;
        Ok(())
    }

    #[test]
    fn hover_requests_keep_only_the_newest_cursor_and_can_be_cancelled() {
        let (sender, _requests) = mpsc::channel();
        let (_frames, frames) = async_channel::bounded(1);
        let (queries, pending_query) = async_channel::bounded(1);
        let (_results, feature_results) = async_channel::bounded(1);
        let renderer = MapRenderer {
            sender,
            frames,
            queries,
            pending_query,
            feature_results,
        };
        let map = MapRequest {
            viewport: Viewport {
                width: NonZeroU32::new(256).unwrap(),
                height: NonZeroU32::new(256).unwrap(),
                scale: 2.0,
            },
            camera: MapCamera::default(),
        };
        for revision in 0..1000 {
            renderer.query_features(revision, map, 7, (revision as f64, 100.0));
        }
        assert_eq!(renderer.pending_query.len(), 1);
        let latest = renderer.pending_query.try_recv().unwrap().unwrap();
        assert_eq!(latest.revision, 999);
        assert_eq!(latest.point, (999.0, 100.0));
        renderer.query_features(1000, map, 7, (42.0, 100.0));
        renderer.clear_feature_query();
        assert!(renderer.pending_query.try_recv().unwrap().is_none());
    }

    fn open_query_fixture(directory: &std::path::Path, name: &str, features: serde_json::Value) -> Result<Arc<VectorSource>> {
        let path = directory.join(name);
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "type": "FeatureCollection", "features": features
            }))?,
        )?;
        Ok(VectorSource::open(&path)?.context("Expected query fixture vector data")?.remove(0))
    }

    fn query_fixture_properties(name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name, "number": 42.5, "boolean": true, "nullable": null
        })
    }

    fn add_query_basemap_decoy(map: &mut NativeMap) -> Result<()> {
        map.renderer.style().add_source(AnySource::from_json_value(
            "basemap-decoy",
            &serde_json::json!({
                "type": "geojson", "data": {
                    "type": "Feature", "properties": {"name": "basemap-decoy"},
                    "geometry": {"type": "Point", "coordinates": [0,0]}
                }
            }),
        )?)?;
        map.renderer.style().add_layer(AnyLayer::from_json_value(&serde_json::json!({
            "id": "basemap-decoy", "type": "circle", "source": "basemap-decoy",
            "paint": {"circle-radius": 8, "circle-color": "#ff0000"}
        }))?)?;
        Ok(())
    }

    fn assert_overlapping_vector_queries(map: &mut NativeMap, center: (f64, f64)) -> Result<()> {
        let features = map.query_vector_features(center)?;
        // Native queries may return the same geometry from multiple tiles.
        let names: std::collections::BTreeSet<_> = features
            .iter()
            .map(|feature| feature["properties"]["name"].as_str().expect("Expected a feature name"))
            .collect();
        assert_eq!(
            names,
            ["line", "point", "polygon"].into_iter().collect(),
            "Only the overlapping active geometries should be returned: {features:?}"
        );
        for feature in &features {
            let name = feature["properties"]["name"].as_str().unwrap();
            assert_eq!(
                feature["properties"],
                query_fixture_properties(name),
                "Native queries must preserve property types"
            );
        }
        for (name, geometry) in [("point", "Point"), ("line", "LineString"), ("polygon", "Polygon")] {
            let feature = features
                .iter()
                .find(|feature| feature["properties"]["name"] == name)
                .with_context(|| format!("Missing {name} at the logical center: {features:?}"))?;
            assert_eq!(feature["geometry"]["type"], geometry);
        }
        assert!(map.query_vector_features((0.0, 0.0))?.is_empty(), "The corner must miss all vector geometry");
        assert!(
            map.query_vector_features((center.0 + 7.0, center.1))?
                .iter()
                .all(|feature| feature["properties"]["name"] != "point"),
            "Points in mixed layers must not receive the extra hit tolerance"
        );
        Ok(())
    }

    fn assert_adjacent_polygon_queries(map: &mut NativeMap, request: MapRequest, directory: &std::path::Path, center: (f64, f64), mixed: bool) -> Result<()> {
        let mut adjacent = serde_json::json!([
            {"type": "Feature", "properties": {"name": "left"}, "geometry": {
                "type": "Polygon", "coordinates": [[[-2,-2],[0,-2],[0,2],[-2,2],[-2,-2]]]
            }},
            {"type": "Feature", "properties": {"name": "right"}, "geometry": {
                "type": "Polygon", "coordinates": [[[0,-2],[2,-2],[2,2],[0,2],[0,-2]]]
            }}
        ]);
        if mixed {
            adjacent.as_array_mut().unwrap().push(serde_json::json!({
                "type": "Feature", "properties": {"name": "point"},
                "geometry": {"type": "Point", "coordinates": [0,1.5]}
            }));
        }
        let source = open_query_fixture(directory, if mixed { "adjacent-mixed.geojson" } else { "adjacent-polygons.geojson" }, adjacent)?;
        let layers = [format!("local-vector-{}-fill", source.id())];
        let _ = map.render_vector(request, Some(source), 1.0, "#3b82f6")?;
        let near_edge = ScreenCoordinate {
            x: center.0 - 3.0,
            y: center.1,
        };
        let expanded = map.renderer.query_rendered_features_with_tolerance(near_edge, 4.0, &layers)?;
        assert!(
            expanded.iter().any(|feature| feature["properties"]["name"] == "right"),
            "The fixture must reproduce the neighboring polygon hit from expanded queries"
        );
        let hits = map.query_vector_features((near_edge.x, near_edge.y))?;
        assert!(!hits.is_empty(), "The pointer must hit the left polygon");
        assert!(
            hits.iter().all(|feature| feature["properties"]["name"] == "left"),
            "Polygon and mixed layers must not hit the neighboring polygon near its edge: {hits:?}"
        );
        Ok(())
    }

    fn assert_point_query_tolerance(map: &mut NativeMap, center: (f64, f64)) -> Result<()> {
        let near = ScreenCoordinate {
            x: center.0 + 7.0,
            y: center.1,
        };
        let circle = [format!("local-vector-{}-circle", map.vector.as_ref().unwrap().id())];
        assert!(
            map.renderer.query_rendered_features(near, &circle)?.is_empty(),
            "An exact query must miss outside the point symbol"
        );
        assert!(
            !map.query_vector_features((near.x, near.y))?.is_empty(),
            "Four logical pixels of tolerance must hit the nearby point at scale 2"
        );
        assert!(
            map.query_vector_features((center.0 + 10.0, center.1))?.is_empty(),
            "The tolerance must not extend beyond four logical pixels"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local vector data"]
    fn local_vector_queries_use_logical_pixels_and_only_the_active_layer_without_network() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = open_query_fixture(
            dir.path(),
            "mixed.geojson",
            serde_json::json!([
                {"type": "Feature", "properties": query_fixture_properties("point"), "geometry": {
                    "type": "Point", "coordinates": [0,0]
                }},
                {"type": "Feature", "properties": query_fixture_properties("line"), "geometry": {
                    "type": "LineString", "coordinates": [[-2,0],[2,0]]
                }},
                {"type": "Feature", "properties": query_fixture_properties("polygon"), "geometry": {
                    "type": "Polygon", "coordinates": [[[-2,-2],[2,-2],[2,2],[-2,2],[-2,-2]]]
                }}
            ]),
        )?;
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 2.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds([-4.0, -4.0, 4.0, 4.0], 256.0, 256.0),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        assert_active_query_geometry(&mut map, request, source)?;
        let center = (128.0, 128.0);
        for mixed in [false, true] {
            assert_adjacent_polygon_queries(&mut map, request, dir.path(), center, mixed)?;
        }

        let switched = install_and_query_replacement_point(&mut map, request, dir.path(), center)?;
        assert_queries_survive_reload_and_clear(&mut map, request, center, switched)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn local_raster_tiles_render_restyle_and_clear_without_network() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("raster.tif");
        let source = open_raster_fixture(&path, 4326, 1)?.remove(0);
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 2.0,
        };
        let camera = MapCamera::fit_bounds(source.metadata().bounds, 512.0, 512.0);
        let request = MapRequest { viewport, camera };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        let background = map.render(request)?;
        let rendered = map.render_raster(request, Some(source.clone()), 1.0)?;
        assert_raster_geographic_coverage(&rendered, &background, camera);
        assert_cached_raster_restyles(&mut map, request, &source, &path, &rendered, &background)?;
        Ok(())
    }

    fn assert_queued_band_fade(
        map: &mut NativeMap,
        request: MapRequest,
        sources: &[Arc<RasterSource>],
        fourth: &Arc<RasterSource>,
        first_image: &RgbaImage,
        deadline: Instant,
    ) -> Result<RgbaImage> {
        let mut intermediate = false;
        let mut queued = false;
        let mut previous = *first_image.get_pixel(256, 256);
        let replacement = loop {
            ensure!(Instant::now() < deadline, "Replacement band did not become ready");
            let Some(image) = map.poll_raster_image()? else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let pixel = *image.get_pixel(256, 256);
            if !queued {
                assert!(pixel[0] >= previous[0], "Fading must not flash the basemap or reverse direction");
            }
            assert_eq!(pixel[3], 255, "Blending two opaque bands must stay opaque");
            if !map.numeric.borrow().is_loading() {
                break image;
            }
            intermediate |= pixel != *first_image.get_pixel(256, 256);
            previous = pixel;
            if intermediate && !queued {
                map.set_raster(Some(sources[3].clone()), 1.0)?;
                map.set_raster(Some(fourth.clone()), 1.0)?;
                map.apply_request(request);
                queued = true;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(intermediate, "The shader must produce intermediate band colors, not an atomic swap");
        assert!(queued, "Exercise selection and styling during an active fade");
        Ok(replacement)
    }

    fn assert_reduced_motion_band_swap(map: &mut NativeMap, reference: &RgbaImage, first_image: &RgbaImage) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            ensure!(Instant::now() < deadline, "Reduced-motion band swap did not complete");
            let Some(image) = map.poll_raster_image()? else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            assert!(
                image.as_raw() == reference.as_raw() || image.as_raw() == first_image.as_raw(),
                "Reduced motion must display an endpoint, never an interpolated band"
            );
            if !map.numeric.borrow().is_loading() {
                assert_eq!(image.as_raw(), first_image.as_raw());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn band_changes_keep_the_outgoing_band_visible_then_crossfade() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bands.tif");
        let sources = open_raster_fixture(&path, 4326, 4)?;
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(sources[0].metadata().bounds, 512.0, 512.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        let first_image = render_gray_raster(&mut map, request, &sources[0], (0.0, 160.0))?;

        assert_band_loading_and_supersession(&mut map, request, &sources, &first_image)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let fourth = sources[3].restyled(inf::colormap::ColorMapPreset::Terrain, (20.0, 180.0))?;
        let replacement = assert_queued_band_fade(&mut map, request, &sources, &fourth, &first_image, deadline)?;
        assert_ne!(first_image.as_raw(), replacement.as_raw(), "The new band must eventually replace the old image");
        assert_eq!(replacement.as_raw(), map.render(request)?.as_raw());

        assert_band_clear_and_reduced_motion(&mut map, request, &sources, fourth, &first_image, &replacement)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn dataset_changes_skip_invisible_fallback_reads_but_initial_loading_keeps_them() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut sources = Vec::new();
        for name in ["outgoing", "incoming"] {
            let path = dir.path().join(format!("{name}.tif"));
            crate::raster_source::test_support::write_raster(&path, 4326, 1)?;
            sources.push(RasterSource::open(&path)?.remove(0));
        }
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(sources[0].metadata().bounds, 512.0, 512.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        assert_initial_fallback_loading(&mut map, request, &sources)?;
        map.set_raster(Some(sources[1].clone()), 1.0)?;
        map.apply_request(request);
        assert!(map.poll_frame()?.is_some());
        assert_eq!(
            map.numeric.borrow().pending_fallback_count(),
            0,
            "An outgoing raster already covers loading; fallback reads must not compete with the visible tiles needed to start the fade"
        );
        map.render(request)?;
        Ok(())
    }

    fn independent_grid_sources(directory: &std::path::Path) -> Result<Vec<Arc<RasterSource>>> {
        let mut sources = Vec::new();
        for (name, bounds, value, palette) in [
            ("coarse", [-40.0, -20.0, 40.0, 20.0], 64.0, inf::colormap::ColorMapPreset::Gray),
            ("fine", [-8.0, -8.0, 8.0, 8.0], 192.0, inf::colormap::ColorMapPreset::Terrain),
            ("shifted", [0.0, -16.0, 32.0, 16.0], 128.0, inf::colormap::ColorMapPreset::Gray),
        ] {
            let path = directory.join(format!("{name}.tif"));
            let mut dataset = new_fixture_dataset(&path, bounds, 1)?;
            let mut band = dataset.rasterband(1)?;
            band.set_no_data_value(Some(-9999.0))?;
            let values = (0..4096).map(|ix| if ix % 64 < 8 { -9999.0 } else { value }).collect();
            band.write((0, 0), (64, 64), &mut gdal::raster::Buffer::new((64, 64), values))?;
            dataset.flush_cache()?;
            drop(dataset);
            let opened = RasterSource::open(&path)?;
            assert_eq!(opened.len(), 1, "Each fixture must be a separate single-band dataset");
            sources.push(opened[0].restyled(palette, (0.0, 255.0))?);
        }
        Ok(sources)
    }

    fn assert_dataset_crossfade(
        map: &mut NativeMap,
        request: MapRequest,
        source: &Arc<RasterSource>,
        outgoing: &RgbaImage,
        incoming: &RgbaImage,
        (from, to): (usize, usize),
    ) -> Result<()> {
        let (probe, _) = outgoing
            .pixels()
            .zip(incoming.pixels())
            .enumerate()
            .max_by_key(|(_, (old, new))| old[0].abs_diff(new[0]))
            .context("The fixture must contain pixels")?;
        let old = outgoing.as_raw()[probe * 4];
        let new = incoming.as_raw()[probe * 4];
        assert!(
            old.abs_diff(new) > 15,
            "Dataset endpoints {from} -> {to} must be visibly different: {old} vs {new}"
        );
        map.set_raster(Some(source.clone()), 0.6)?;
        map.apply_request(request);
        let loading = map.required_raster_image("The outgoing dataset must remain renderable")?;
        assert_eq!(
            loading.as_raw(),
            outgoing.as_raw(),
            "Loading a dataset must preserve the entire outgoing footprint"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut intermediate = false;
        loop {
            ensure!(Instant::now() < deadline, "Dataset crossfade did not complete");
            let Some(image) = map.poll_raster_image()? else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let blend = ((f32::from(image.as_raw()[probe * 4]) - f32::from(old)) / (f32::from(new) - f32::from(old))).clamp(0.0, 1.0);
            intermediate |= (0.1..0.9).contains(&blend);
            for ((actual, old), new) in image.as_raw().iter().zip(outgoing.as_raw()).zip(incoming.as_raw()) {
                let expected = f32::from(*old) * (1.0 - blend) + f32::from(*new) * blend;
                assert!(
                    (f32::from(*actual) - expected).abs() <= 3.0,
                    "Dataset footprints, palettes, nodata and opacity must blend without a flash: {actual} vs {expected}"
                );
            }
            if !map.numeric.borrow().is_loading() {
                assert_eq!(image.as_raw(), incoming.as_raw());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(intermediate, "Dataset changes must interpolate rather than swap atomically");
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn single_band_dataset_changes_keep_outgoing_coverage_and_crossfade_independent_grids() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let sources = independent_grid_sources(dir.path())?;
        assert!(sources[0].metadata().max_zoom < sources[1].metadata().max_zoom);
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(sources[0].metadata().bounds, 256.0, 256.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        let background = map.render(request)?;
        let endpoints = raster_fade_endpoints(&mut map, request, &sources)?;
        // Exercise finer, shifted, and coarser replacements, including pixels
        // covered by only one dataset and independent nodata masks/palettes.
        for (from, to) in [(0, 1), (1, 2), (2, 0)] {
            assert_dataset_crossfade(&mut map, request, &sources[to], &endpoints[from], &endpoints[to], (from, to))?;
        }
        map.set_raster(None, 0.6)?;
        assert_eq!(map.render(request)?.as_raw(), background.as_raw());
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn band_fades_blend_independent_nodata_masks_and_opacity() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nodata-bands.tif");
        let sources = open_nodata_band_fixture(&path)?;
        let first = sources[0].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let second = sources[1].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(first.metadata().bounds, 256.0, 256.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        let (outgoing, incoming) = nodata_fade_endpoints(&mut map, request, first, &second)?;
        map.set_raster(Some(second), 0.6)?;
        map.apply_request(request);
        assert_nodata_band_fade(&mut map, &outgoing, &incoming)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn clamp_colors_masks_only_outside_raw_and_physical_ranges_without_reloading_tiles() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("clamp-scaled.tif");
        let source = scaled_band_sources(&path)?.remove(1);
        assert!(source.clamp_colors(), "Endpoint clamping must remain the default");
        assert_eq!(source.scale(), geo::RasterScale { scale: -2.0, offset: 200.0 });
        let (mut map, request, background) = clamp_test_map(&source)?;
        let counts = map.numeric.borrow().tile_counts();
        assert!(counts.0 > 0 && counts.1 > 0, "Warm the raw tile cache before toggling clamping");
        std::fs::rename(&path, path.with_extension("cached.tif"))?;

        for (physical, value) in [(false, 40.0), (true, 120.0)] {
            let units = source.with_unscaling(physical)?;
            for (range, visible, gray) in [
                ((value + 1.0, value + 10.0), false, 0),
                ((value - 10.0, value - 1.0), false, 255),
                ((value, value + 10.0), true, 0),
                ((value - 10.0, value), true, 255),
            ] {
                let clamped = units.restyled(inf::colormap::ColorMapPreset::Gray, range)?;
                assert_clamp_range_roundtrip(&mut map, request, clamped, &background, visible, gray)?;
                assert_eq!(
                    map.numeric.borrow().tile_counts(),
                    counts,
                    "Clamp/unit/range changes must reuse raw tiles without uploads"
                );
            }
        }
        Ok(())
    }

    fn clamp_test_map(source: &Arc<RasterSource>) -> Result<(NativeMap, MapRequest, RgbaImage)> {
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.metadata().bounds, 256.0, 256.0),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        let background = map.render(request)?;
        map.render_raster(request, Some(source.clone()), 1.0)?;
        Ok((map, request, background))
    }

    fn assert_clamp_range_roundtrip(
        map: &mut NativeMap,
        request: MapRequest,
        clamped: Arc<RasterSource>,
        background: &RgbaImage,
        visible: bool,
        gray: u8,
    ) -> Result<()> {
        let reference = map.render_raster(request, Some(clamped.clone()), 1.0)?;
        assert_legend_center(&reference, image::Rgba([gray, gray, gray, 255]));
        let unclamped = clamped.with_clamp_colors(false)?;
        assert!(!unclamped.clamp_colors());
        let clipped = map.render_raster(request, Some(unclamped.clone()), 1.0)?;
        let expected = if visible { &reference } else { background };
        assert_eq!(
            clipped.get_pixel(128, 128),
            expected.get_pixel(128, 128),
            "Only values strictly outside the display range must disappear: physical={}, range={:?}",
            clamped.unscaling_enabled(),
            clamped.range()
        );
        let restored = map.render_raster(request, Some(unclamped.with_clamp_colors(true)?), 1.0)?;
        assert_eq!(restored.as_raw(), reference.as_raw(), "Re-enabling clamping must restore the original pixels");
        for image in [&reference, &clipped, &restored] {
            assert_eq!(image.get_pixel(16, 128), background.get_pixel(16, 128), "Nodata must stay transparent");
        }
        assert!(clamped.clamp_colors(), "Toggling must not mutate the original snapshot");
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn band_fades_keep_current_and_previous_clamp_colors_independent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("clamp-fades.tif");
        let sources = open_nodata_band_fixture(&path)?;
        let first = sources[0].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 50.0))?;
        let second = sources[1].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 160.0))?;
        let (mut map, request, background) = clamp_test_map(&first)?;
        let endpoints = raster_fade_endpoints(&mut map, request, &[first.clone(), second.clone()])?;
        let counts = map.numeric.borrow().tile_counts();
        assert!(counts.0 > 0 && counts.1 > 0);
        std::fs::rename(&path, path.with_extension("cached.tif"))?;
        assert_cached_clamp_fades(&mut map, request, &first, &second, &endpoints, &background)?;
        assert_eq!(
            map.numeric.borrow().tile_counts(),
            counts,
            "Fading clamp snapshots must reuse both bands' raw tiles"
        );
        Ok(())
    }

    fn assert_cached_clamp_fades(
        map: &mut NativeMap,
        request: MapRequest,
        first: &RasterSource,
        second: &RasterSource,
        endpoints: &[RgbaImage],
        background: &RgbaImage,
    ) -> Result<()> {
        for (previous_clamps, current_clamps) in [(false, true), (true, false)] {
            let previous = first.with_clamp_colors(previous_clamps)?;
            let current = second.with_clamp_colors(current_clamps)?;
            let (outgoing, incoming) = nodata_fade_endpoints(map, request, previous, &current)?;
            let expected_outgoing = if previous_clamps { &endpoints[0] } else { background };
            let expected_incoming = if current_clamps { &endpoints[1] } else { background };
            assert_eq!(
                outgoing.as_raw(),
                expected_outgoing.as_raw(),
                "The previous band's clamp flag must control its endpoint"
            );
            assert_eq!(
                incoming.as_raw(),
                expected_incoming.as_raw(),
                "The current band's clamp flag must control its endpoint"
            );
            map.set_raster(Some(current), 0.6)?;
            map.apply_request(request);
            assert_nodata_band_fade(map, &outgoing, &incoming)?;
        }
        Ok(())
    }

    fn scaled_band_sources(path: &std::path::Path) -> Result<Vec<Arc<RasterSource>>> {
        crate::raster_source::test_support::write_raster(path, 4326, 2)?;
        let mut dataset = gdal::Dataset::open_ex(
            path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        for (nr, raw, scale, offset) in [(1, 100.0_f32, 0.5, -7.0), (2, 40.0, -2.0, 200.0)] {
            let mut band = dataset.rasterband(nr)?;
            band.set_scale(scale)?;
            band.set_offset(offset)?;
            let values = (0..4096).map(|ix| if ix % 64 < 8 { -9999.0 } else { raw }).collect();
            band.write((0, 0), (64, 64), &mut gdal::raster::Buffer::new((64, 64), values))?;
        }
        dataset.flush_cache()?;
        drop(dataset);
        RasterSource::open(path)
    }

    fn legend_pixel(source: &RasterSource, physical: f32) -> image::Rgba<u8> {
        let color = source.legend().color_for_value(physical, None);
        image::Rgba([color.b, color.g, color.r, 255])
    }

    fn assert_legend_center(image: &RgbaImage, expected: image::Rgba<u8>) {
        for (actual, expected) in image.get_pixel(128, 128).0.into_iter().zip(expected.0) {
            assert!(
                actual.abs_diff(expected) <= 1,
                "Rendered center must match the active-unit legend: {actual} vs {expected}"
            );
        }
    }

    fn assert_scaled_fade_pixel(pixel: image::Rgba<u8>, start: image::Rgba<u8>, expected: image::Rgba<u8>, reduce_motion: bool, previous: u8) {
        assert_eq!(pixel[3], 255, "Scaled opaque bands must remain opaque during a fade");
        for (actual, (old, new)) in pixel.0.into_iter().zip(start.0.into_iter().zip(expected.0)) {
            assert!(
                actual >= old.min(new).saturating_sub(1) && actual <= old.max(new).saturating_add(1),
                "Fade must stay between independently unscaled legend colors: {pixel:?}"
            );
        }
        if reduce_motion {
            assert!(
                pixel == start || pixel.0.into_iter().zip(expected.0).all(|(a, b)| a.abs_diff(b) <= 1),
                "Reduced motion must show only a physical-color endpoint"
            );
        } else {
            assert!(pixel[0].saturating_add(1) >= previous, "Per-band transforms must not cause the fade to reverse");
        }
    }

    fn assert_scaled_band_transition(
        map: &mut NativeMap,
        request: MapRequest,
        incoming: Arc<RasterSource>,
        outgoing: &RgbaImage,
        expected: image::Rgba<u8>,
        background: &RgbaImage,
        reduce_motion: bool,
    ) -> Result<RgbaImage> {
        map.numeric.borrow_mut().set_reduce_motion(reduce_motion);
        map.set_raster(Some(incoming), 1.0)?;
        map.apply_request(request);
        let loading = map.required_raster_image("The scaled outgoing band must remain renderable while loading")?;
        assert_eq!(
            loading.as_raw(),
            outgoing.as_raw(),
            "Loading must retain the outgoing band's own scale and offset"
        );
        let start = *outgoing.get_pixel(128, 128);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut previous = start[0];
        loop {
            ensure!(Instant::now() < deadline, "Scaled band transition did not complete");
            let Some(image) = map.poll_raster_image()? else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let pixel = *image.get_pixel(128, 128);
            assert_scaled_fade_pixel(pixel, start, expected, reduce_motion, previous);
            assert_eq!(
                image.get_pixel(16, 128),
                background.get_pixel(16, 128),
                "Nodata must not acquire the band's offset"
            );
            previous = pixel[0];
            if !map.numeric.borrow().is_loading() {
                assert_legend_center(&image, expected);
                return Ok(image);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_scaled_tiles_store_raw_values(first: &RasterSource, second: &RasterSource) -> Result<()> {
        for (source, raw) in [(first, 100.0_f32), (second, 40.0)] {
            let tile = geo::Tile::for_coordinate(geo::Coordinate::latlon(1.0, 1.0), source.metadata().max_zoom.clamp(0, 22));
            let numeric = source.numeric_tile(tile, 1)?.context("Missing scaled numeric tile")?;
            assert!(numeric.values.iter().any(|value| value.is_finite()), "The fixture must contain numeric data");
            assert!(
                numeric
                    .values
                    .iter()
                    .filter(|value| value.is_finite())
                    .all(|value| (*value - raw).abs() <= 0.0001),
                "Numeric tiles must retain stored values, not physical legend values"
            );
        }
        Ok(())
    }

    fn assert_unit_snapshot_metadata(snapshot: &RasterSource, first: &RasterSource, enabled: bool, range: (f32, f32)) {
        assert!(snapshot.has_scale_metadata());
        assert_eq!(snapshot.unscaling_enabled(), enabled, "Restyling must retain the snapshot's unit mode");
        assert_eq!(
            snapshot.scale(),
            if enabled {
                geo::RasterScale { scale: 0.5, offset: -7.0 }
            } else {
                geo::RasterScale::default()
            }
        );
        assert_eq!(snapshot.range(), range);
        assert_eq!(snapshot.palette(), inf::colormap::ColorMapPreset::Gray);
        assert_eq!(snapshot.metadata().id, first.metadata().id, "Unit changes must retain raw cache identity");
        assert_eq!(snapshot.metadata().scale, Some(geo::RasterScale { scale: 0.5, offset: -7.0 }));
        assert_ne!(snapshot.id(), first.id(), "Unit/style snapshots need their own style identity");
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn scaled_bands_render_physical_legend_colors_through_loading_and_fades() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("scaled-bands.tif");
        let sources = scaled_band_sources(&path)?;
        let first = sources[0].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let second = sources[1].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let (mut map, request, outgoing, background) = assert_scaled_band_display(&first, &second)?;
        assert_cached_unit_restyles(&mut map, request, &first, &path, &outgoing, &background)?;
        Ok(())
    }

    fn assert_cached_unit_restyles(
        map: &mut NativeMap,
        request: MapRequest,
        first: &Arc<RasterSource>,
        path: &std::path::Path,
        outgoing: &RgbaImage,
        background: &RgbaImage,
    ) -> Result<()> {
        let counts = map.numeric.borrow().tile_counts();
        assert!(counts.0 > 0 && counts.1 > 0, "Warm the numeric tile cache before toggling units");
        let camera_updates = map.camera_updates;
        // Every following snapshot and render must use metadata and cached raw tiles only.
        std::fs::rename(path, path.with_extension("cached.tif"))?;
        let (cases, raw_gray) = unit_style_cases(first)?;
        for UnitStyleCase {
            snapshot,
            enabled,
            range,
            expected,
            unchanged,
        } in cases
        {
            assert_unit_snapshot_metadata(&snapshot, first, enabled, range);
            let image = map.render_raster(request, Some(snapshot.clone()), 1.0)?;
            assert_legend_center(&image, image::Rgba([expected, expected, expected, 255]));
            if unchanged {
                assert_eq!(
                    image.as_raw(),
                    outgoing.as_raw(),
                    "Equivalent raw and physical ranges must render identical pixels"
                );
            }
            assert_eq!(
                image.get_pixel(16, 128),
                background.get_pixel(16, 128),
                "Toggling units or clipping must leave nodata transparent"
            );
            assert_eq!(
                map.numeric.borrow().tile_counts(),
                counts,
                "Unit/style changes must not request or upload numeric tiles"
            );
            assert_eq!(map.camera_updates, camera_updates, "Unit/style changes must not reset the camera");
        }
        assert!(first.unscaling_enabled(), "Creating raw snapshots must not mutate the physical source");
        assert_eq!(first.range(), (0.0, 255.0));
        assert!(!raw_gray.unscaling_enabled(), "Creating physical snapshots must not mutate the raw source");
        assert_eq!(raw_gray.range(), (0.0, 255.0));
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn custom_raster_respects_native_layer_order_and_band_changes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bands.tif");
        let sources = open_raster_fixture(&path, 4326, 2)?;
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(sources[0].metadata().bounds, 256.0, 256.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        let first = sources[0].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 160.0))?;
        let second = sources[1].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 160.0))?;
        let first_image = map.render_raster(request, Some(first.clone()), 1.0)?;
        let requests = map.numeric.borrow().tile_counts().0;
        let second_image = map.render_raster(request, Some(second), 1.0)?;
        assert_ne!(first_image.as_raw(), second_image.as_raw(), "A different band must load different values");
        assert!(map.numeric.borrow().tile_counts().0 > requests, "Band changes need new raw tiles");

        assert_native_cover_occludes_band_changes(&mut map, request, &first)?;
        assert_explicit_raster_layer_insertion(&mut map, request, first, &first_image)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only synthetic offline raster data"]
    fn global_raster_registration_matches_fitted_camera_at_low_zoom() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("global-latitudes.tif");
        write_global_latitude_fixture(&path)?;
        let source = RasterSource::open(&path)?
            .remove(0)
            .restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 1.0,
        };
        let camera = MapCamera::fit_bounds(source.metadata().bounds, 1200.0, 760.0);
        let request = MapRequest { viewport, camera };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        let image = map.render_raster(request, Some(source.clone()), 1.0)?;
        assert_eq!(image.dimensions(), (1200, 760));
        assert_global_latitude_registration(&image, camera, &source);

        // Recreate the old 85%-height world without changing production camera code.
        let mut unconstrained = camera;
        unconstrained.zoom_at(0.85_f64.log2(), (0.0, 0.0));
        let coordinate = unconstrained.coordinate_at((-39.5, 212.5));
        assert!(coordinate.latitude < -74.0, "The old camera must predict southern data: {coordinate:?}");
        let old_image = map.render_raster(
            MapRequest {
                camera: unconstrained,
                ..request
            },
            Some(source),
            1.0,
        )?;
        assert_eq!(
            *old_image.get_pixel(560, 592),
            image::Rgba([32, 32, 32, 255]),
            "Native HeightOnly zoom puts this old-camera southern-data probe over nodata"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only synthetic offline raster data"]
    fn global_raster_registration_matches_viewport_constrained_polar_pan() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("global-latitudes.tif");
        write_global_latitude_fixture(&path)?;
        let source = RasterSource::open(&path)?
            .remove(0)
            .restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 1.0,
        };
        let mut camera = MapCamera::fit_bounds(source.metadata().bounds, 1200.0, 760.0);
        camera.zoom_at(1.0, (0.0, 0.0));
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        for dy in [-10_000.0, 10_000.0] {
            camera.pan(0.0, dy);
            camera.constrain_to_viewport(760.0);
            let image = map.render_raster(MapRequest { viewport, camera }, Some(source.clone()), 1.0)?;
            assert_global_latitude_registration(&image, camera, &source);
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a graphics device and MAP_TEST_NETCDF pointing to the Abu Dhabi WBGT example"]
    fn local_wbgt_netcdf_places_the_coast_north_and_desert_south() -> Result<()> {
        let path = std::path::PathBuf::from(std::env::var("MAP_TEST_NETCDF")?);
        let source = RasterSource::open(&path)?.remove(0);
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let camera = MapCamera::fit_bounds(source.metadata().bounds, 512.0, 512.0);
        let request = MapRequest { viewport, camera };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#ff00ff"}}]}"##),
        )?;
        let image = map.render_raster(request, Some(source.clone()), 1.0)?;
        let origin = geo::crs::lat_lon_to_web_mercator(camera.coordinate_at((-256.0, -256.0)));
        let step = geo::crs::lat_lon_to_web_mercator(camera.coordinate_at((-255.0, -255.0)));
        for (coordinate, has_data) in [(geo::Coordinate::latlon(25.0, 54.37), false), (geo::Coordinate::latlon(23.14, 53.78), true)] {
            let tile = geo::Tile::for_coordinate(coordinate, source.metadata().max_zoom.clamp(0, 22));
            let data = source.numeric_tile(tile, 1)?.context("Missing WBGT tile")?;
            let grid = geo::GeoReference::from_tile(&tile, data.width as usize, 1);
            let projected = geo::crs::lat_lon_to_web_mercator(coordinate);
            let cell = grid.point_to_cell(projected);
            let value = data.values[(cell.row as u32 * data.width + cell.col as u32) as usize];
            assert_eq!(value.is_finite(), has_data, "Numeric WBGT tile is reversed at {coordinate:?}");
            let x = ((projected.x() - origin.x()) / (step.x() - origin.x())).round() as u32;
            let y = ((projected.y() - origin.y()) / (step.y() - origin.y())).round() as u32;
            assert_eq!(
                *image.get_pixel(x, y) != image::Rgba([255, 0, 255, 255]),
                has_data,
                "Rendered WBGT footprint is reversed at {coordinate:?}"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn byte_values_render_opaque_including_173_and_174() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("byte-values.tif");
        crate::raster_source::test_support::write_byte_raster(&path)?;
        let source = RasterSource::open(&path)?
            .remove(0)
            .restyled(inf::colormap::ColorMapPreset::Terrain, (0.0, 249.0))?;
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.metadata().bounds, 512.0, 512.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(
                r##"{"version":8,"sources":{"water":{"type":"geojson","data":{"type":"Polygon","coordinates":[[[60,-20],[70,-20],[70,20],[60,20],[60,-20]]]}}},"layers":[{"id":"background","type":"background","paint":{"background-color":"#ff00ff"}},{"id":"water","type":"fill","source":"water","paint":{"fill-color":"#102030","fill-antialias":false}}]}"##,
            ),
        )?;
        let image = map.render_raster(request, Some(source.clone()), 1.0)?;
        for value in 0..255 {
            let color = source.legend().color_for_value(value, None);
            let expected = image::Rgba([color.b, color.g, color.r, 255]);
            assert_eq!(
                *image.get_pixel(value * 2 + 1, 256),
                expected,
                "Byte {value} must be opaque and match the legend"
            );
        }
        assert_eq!(
            *image.get_pixel(511, 256),
            image::Rgba([255, 0, 255, 255]),
            "Only nodata should show the background"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and vector basemap access; uses the landuse fixture"]
    fn landuse_water_values_render_opaque_over_the_basemap() -> Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("infra-rs/tests/data/landusebyte.tif");
        let source = RasterSource::open(&path)?
            .remove(0)
            .restyled(inf::colormap::ColorMapPreset::Terrain, (0.0, 249.0))?;
        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 2.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds([4.20, 51.10, 4.45, 51.23], 1200.0, 760.0),
        };
        let mut map = NativeMap::new(viewport)?;
        let background = map.render(request)?;
        let rendered = map.render_raster(request, Some(source.clone()), 1.0)?;
        assert_landuse_water_coverage(&source, request, &rendered, &background)?;
        Ok(())
    }

    #[test]
    #[ignore = "manual GPU performance measurement; uses only local raster data"]
    fn raster_style_frame_timings() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("raster.tif");
        let source = open_raster_fixture(&path, 4326, 1)?.remove(0);
        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 2.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.metadata().bounds, 1200.0, 760.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        map.render_raster(request, Some(source.clone()), 1.0)?;
        let counts = map.numeric.borrow().tile_counts();
        let camera_updates = map.camera_updates;
        measure_raster_restyles(&mut map, request, &source)?;
        assert_eq!(map.numeric.borrow().tile_counts(), counts);
        assert_eq!(map.camera_updates, camera_updates);
        Ok(())
    }

    #[test]
    #[ignore = "manual GPU performance measurement; requires internet access"]
    fn navigation_frame_timings() -> Result<()> {
        let _ = env_logger::builder().is_test(true).try_init();

        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 1.5,
        };
        let mut camera = MapCamera::default();
        camera.zoom_at(1.5, (0.0, 0.0));
        let mut map = NativeMap::new(viewport)?;
        map.render(MapRequest { viewport, camera })?;
        let mut rendering = Duration::ZERO;
        let mut readback = Duration::ZERO;
        for _ in 0..12 {
            camera.pan(10.0, 5.0);
            camera.zoom_at(0.01, (0.0, 0.0));
            let start = Instant::now();
            map.apply_request(MapRequest { viewport, camera });
            let texture = wait_for_native_texture(&mut map)?;
            let rendered = Instant::now();
            let image = map.readback.read(&texture, &map.device, &map.queue)?;
            rendering += rendered - start;
            readback += rendered.elapsed();
            assert_eq!(image.dimensions(), (1800, 1140));
        }
        log::info!(
            "Average navigation frame: first-frame rendering={:?}, readback/copy={:?}",
            rendering / 12,
            readback / 12
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only a local background style"]
    fn streaming_worker_uses_local_test_style_and_preserves_it_after_dpi_changes() -> Result<()> {
        let renderer = {
            let json = DARK_STYLE.to_owned();
            MapRenderer::with_test_style(&json)?
        };
        let frames = renderer.frames();
        for scale in [1.0, 2.0] {
            let viewport = Viewport {
                width: NonZeroU32::new(32).unwrap(),
                height: NonZeroU32::new(24).unwrap(),
                scale,
            };
            // Even a Light tag must use the supplied JSON, not its remote URL.
            renderer.render_layer(
                MapRequest {
                    viewport,
                    camera: MapCamera::default(),
                },
                None,
                1.0,
                false,
                BasemapStyle::Light,
                false,
            )?;
            assert_local_style_worker_frame(&frames, viewport, scale)?;
        }
        drop(renderer);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !frames.is_closed() {
            ensure!(Instant::now() < deadline, "Local-style map worker did not shut down");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and internet access"]
    fn streaming_worker_delivers_frames_and_accepts_new_viewports() -> Result<()> {
        let renderer = MapRenderer::new()?;
        let frames = renderer.frames();
        let mut viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        renderer.render(MapRequest {
            viewport,
            camera: MapCamera::default(),
        })?;
        let first = wait_for_geographic_worker_frame(&frames, viewport)?;
        assert_eq!(first.size(0).width.0, 800);
        viewport.width = NonZeroU32::new(1000).unwrap();
        let mut camera = MapCamera::default();
        camera.zoom_at(1.0, (0.0, 0.0));
        renderer.render(MapRequest { viewport, camera })?;
        let resized = wait_for_geographic_worker_frame(&frames, viewport)?;
        assert_eq!(resized.size(0).width.0, 1000);
        drop(renderer);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !frames.is_closed() {
            ensure!(Instant::now() < deadline, "Map worker did not shut down");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device and internet access to OpenFreeMap"]
    fn openstreetmap_vector_background_renders_geographic_content() -> Result<()> {
        let viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        let mut map = NativeMap::with_basemap(viewport, BasemapStyle::OpenStreetMap)?;
        let camera = MapCamera::fit_bounds([4.30, 50.80, 4.45, 50.90], 800.0, 600.0);
        let image = map.render(MapRequest { viewport, camera })?;
        assert_eq!(image.dimensions(), (800, 600));
        let colors: std::collections::HashSet<_> = image.pixels().map(|pixel| pixel.0).collect();
        assert!(colors.len() > 100, "Expected OSM streets and labels, got {} colors", colors.len());
        assert!(image.pixels().all(|pixel| pixel[3] == 255));
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device and internet access to the demo vector tiles"]
    fn vector_basemap_renders_geographic_content() -> Result<()> {
        let viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        let mut map = NativeMap::new(viewport)?;
        let image = map.render(MapRequest {
            viewport,
            camera: MapCamera::default(),
        })?;
        assert_eq!(image.dimensions(), (800, 600));
        let colors: std::collections::HashSet<_> = image.pixels().map(|pixel| pixel.0).collect();
        assert!(colors.len() > 100, "Expected vector features and labels, got {} colors", colors.len());
        assert!(image.pixels().all(|pixel| pixel[3] == 255));
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and internet access"]
    fn retained_renderer_updates_camera_and_viewport() -> Result<()> {
        let mut viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        let mut camera = MapCamera::default();
        let mut map = NativeMap::new(viewport)?;
        let before = map.render(MapRequest { viewport, camera })?;
        camera.pan(150.0, 70.0);
        camera.zoom_at(1.0, (120.0, -80.0));
        let after = map.render(MapRequest { viewport, camera })?;
        assert!(before.as_raw() != after.as_raw(), "Camera changes must change the rendered image");
        viewport.width = NonZeroU32::new(1000).unwrap();
        let resized = map.render(MapRequest { viewport, camera })?;
        assert_eq!(resized.dimensions(), (1000, 600));
        Ok(())
    }

    fn new_fixture_dataset(path: &std::path::Path, bounds: [f64; 4], bands: usize) -> Result<gdal::Dataset> {
        let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f32, _>(path, 64, 64, bands)?;
        dataset.set_geo_transform(&[bounds[0], (bounds[2] - bounds[0]) / 64.0, 0.0, bounds[3], 0.0, -(bounds[3] - bounds[1]) / 64.0])?;
        dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
        Ok(dataset)
    }

    fn render_gray_raster(map: &mut NativeMap, request: MapRequest, source: &RasterSource, range: (f32, f32)) -> Result<RgbaImage> {
        let source = source.restyled(inf::colormap::ColorMapPreset::Gray, range)?;
        map.render_raster(request, Some(source), 1.0)
    }

    fn write_global_latitude_fixture(path: &std::path::Path) -> Result<()> {
        let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f32, _>(path, 720, 360, 1)?;
        dataset.set_geo_transform(&[-180.0, 0.5, 0.0, 90.0, 0.0, -0.5])?;
        dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
        let mut band = dataset.rasterband(1)?;
        band.set_no_data_value(Some(-9999.0))?;
        let mut values = vec![-9999.0_f32; 720 * 360];
        values[..720 * 36].fill(128.0);
        values[720 * 324..].fill(192.0);
        band.write((0, 0), (720, 360), &mut gdal::raster::Buffer::new((720, 360), values))?;
        Ok(dataset.close()?)
    }

    fn assert_global_latitude_registration(image: &RgbaImage, camera: MapCamera, source: &RasterSource) {
        let mut counts = [0; 2];
        let probes = (24..image.height() - 24)
            .step_by(19)
            .flat_map(|y| (24..image.width() - 24).step_by(97).map(move |x| (x, y)));
        for (x, y) in probes {
            let offset = (
                f64::from(x) + 0.5 - f64::from(image.width()) / 2.0,
                f64::from(y) + 0.5 - f64::from(image.height()) / 2.0,
            );
            let coordinate = camera.coordinate_at(offset);
            // Ignore warp resampling and native raster alignment near mask edges.
            if (coordinate.latitude.abs() - 72.0).abs() < 2.0 {
                continue;
            }
            let expected = if coordinate.latitude < -72.0 {
                legend_pixel(source, 192.0)
            } else if coordinate.latitude > 72.0 {
                legend_pixel(source, 128.0)
            } else {
                image::Rgba([32, 32, 32, 255])
            };
            counts[usize::from(coordinate.latitude.abs() > 72.0)] += 1;
            assert_eq!(
                *image.get_pixel(x, y),
                expected,
                "Rendered global raster must agree with coordinate_at at pixel ({x}, {y}), {coordinate:?}, camera {camera:?}"
            );
        }
        assert!(counts.into_iter().all(|count| count > 20), "Exercise both nodata and polar data: {counts:?}");
    }

    fn open_raster_fixture(path: &std::path::Path, epsg: u32, bands: usize) -> Result<Vec<Arc<RasterSource>>> {
        crate::raster_source::test_support::write_raster(path, epsg, bands)?;
        RasterSource::open(path)
    }

    fn assert_numeric_basemap_roundtrip(
        map: &mut NativeMap,
        request: MapRequest,
        dark_raster: &RgbaImage,
        dark_background: &RgbaImage,
        path: &std::path::Path,
    ) -> Result<RgbaImage> {
        let numeric = map.numeric.clone();
        let device = map.device.clone();
        let counts = numeric.borrow().tile_counts();
        let camera_updates = map.camera_updates;
        assert!(counts.0 > 0 && counts.1 > 0);
        assert_ne!(dark_raster.as_raw(), dark_background.as_raw());
        // A reload must reuse uploaded tiles even when their file is unavailable.
        std::fs::rename(path, path.with_extension("cached.tif"))?;

        map.load_style(BasemapStyle::Light, Some(LIGHT_STYLE))?;
        assert_eq!(map.basemap, BasemapStyle::Light);
        assert!(map.camera == Some(request.camera));
        assert_eq!(map.device, device);
        assert!(Rc::ptr_eq(&map.numeric, &numeric));
        let light_raster = map.render(request)?;
        assert_ne!(
            light_raster.as_raw(),
            dark_raster.as_raw(),
            "The new basemap must be visible through the overlay"
        );
        assert_eq!(numeric.borrow().tile_counts(), counts, "Reloading must not request or upload numeric tiles");

        // Requesting the current basemap must not replace the offline style.
        map.set_basemap(BasemapStyle::Light)?;
        map.load_style(BasemapStyle::Dark, Some(DARK_STYLE))?;
        assert_eq!(
            map.render(request)?.as_raw(),
            dark_raster.as_raw(),
            "A round trip must preserve the native camera and overlay"
        );
        assert_eq!(map.camera_updates, camera_updates);
        assert_eq!(numeric.borrow().tile_counts(), counts);
        Ok(light_raster)
    }

    fn write_color_mbtiles_fixture(path: &std::path::Path) -> Result<()> {
        let db = sqlite::Connection::new(path, sqlite::AccessMode::Create)?;
        db.execute_sql_statements(
            "CREATE TABLE metadata (name TEXT, value TEXT);
            INSERT INTO metadata VALUES ('format','png'), ('bounds','-180,-85,180,85'), ('minzoom','0'), ('maxzoom','0');
            CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB);",
        )?;
        let mut png = std::io::Cursor::new(Vec::new());
        RgbaImage::from_pixel(256, 256, image::Rgba([0, 255, 0, 255])).write_to(&mut png, image::ImageFormat::Png)?;
        let mut insert = db.prepare_statement("INSERT INTO tiles VALUES (0,0,0,?1) RETURNING zoom_level")?;
        insert.bind_blob(1, png.get_ref())?;
        ensure!(insert.next().is_some(), "Could not create fixture tile: {}", db.last_error());
        drop(insert);
        drop(db);
        Ok(())
    }

    fn open_mixed_vector_fixture(dir: &std::path::Path) -> Result<Arc<VectorSource>> {
        let path = dir.join("mixed.geojson");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "type": "FeatureCollection",
                "features": [
                    {"type": "Feature", "properties": {}, "geometry": {
                        "type": "Polygon", "coordinates": [[[-5,-2],[-2,-2],[-2,2],[-5,2],[-5,-2]]]
                    }},
                    {"type": "Feature", "properties": {}, "geometry": {
                        "type": "LineString", "coordinates": [[0,-2],[0,2]]
                    }},
                    {"type": "Feature", "properties": {}, "geometry": {
                        "type": "Point", "coordinates": [5,0]
                    }}
                ]
            }))?,
        )?;
        let source = VectorSource::open(&path)?.context("Expected mixed vector data")?.remove(0);
        Ok(source)
    }

    fn assert_vector_restyles_survive_basemap_reload(
        map: &mut NativeMap,
        request: MapRequest,
        source: &Arc<VectorSource>,
        rendered: &RgbaImage,
        background: &RgbaImage,
    ) -> Result<()> {
        let camera_updates = map.camera_updates;

        let translucent = map.render_vector(request, Some(source.clone()), 0.5, "#3b82f6")?;
        assert_ne!(translucent.as_raw(), rendered.as_raw(), "Opacity changes must repaint vector layers");
        assert_ne!(translucent.as_raw(), background.as_raw());
        assert_vector_opacity_roundtrip(map, request, source, "#3b82f6", &translucent, background)?;

        // Reattach at unchanged opacity and retain the native camera despite style defaults.
        map.load_style(BasemapStyle::Light, Some(LIGHT_STYLE))?;
        let light_vector = map.render(request)?;
        assert_ne!(light_vector.as_raw(), translucent.as_raw());
        map.load_style(BasemapStyle::Dark, Some(DARK_STYLE))?;
        assert_eq!(
            map.render(request)?.as_raw(),
            translucent.as_raw(),
            "A style round trip must restore all vector layers"
        );
        assert_eq!(map.camera_updates, camera_updates);
        Ok(())
    }

    fn assert_vector_switch_clear_and_reinstall(
        map: &mut NativeMap,
        request: MapRequest,
        source: Arc<VectorSource>,
        dir: &std::path::Path,
        background: &RgbaImage,
        rendered: &RgbaImage,
    ) -> Result<()> {
        let replacement_path = dir.join("replacement.geojson");
        std::fs::write(
            &replacement_path,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#,
        )?;
        let replacement = VectorSource::open(&replacement_path)?.context("Expected replacement vector data")?.remove(0);
        assert_ne!(source.id(), replacement.id());
        let switched = map.render_vector(request, Some(replacement), 1.0, "#3b82f6")?;
        let changed = switched.pixels().zip(background.pixels()).filter(|(a, b)| a != b).count();
        assert!(
            changed > 10 && changed < 200,
            "Switching must remove the previous source's polygon and line layers, got {changed} pixels"
        );
        map.set_vector(None, 1.0, "#3b82f6")?;
        assert_eq!(
            map.render(request)?.as_raw(),
            background.as_raw(),
            "Clearing must remove vector layers and their source"
        );
        map.set_vector(Some(source), 1.0, "#3b82f6")?;
        assert_eq!(map.render(request)?.as_raw(), rendered.as_raw(), "A cleared source must be installable again");
        Ok(())
    }

    fn open_numeric_vector_fixture(directory: &std::path::Path) -> Result<Arc<VectorSource>> {
        let path = directory.join("numeric.geojson");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "type": "FeatureCollection", "features": [
                    {"type":"Feature","properties":{"a":0.0,"b":10},"geometry":{"type":"Polygon","coordinates":[[[-5,-2],[-2,-2],[-2,2],[-5,2],[-5,-2]]]}},
                    {"type":"Feature","properties":{"a":10.0,"b":0},"geometry":{"type":"Point","coordinates":[0,0]}},
                    {"type":"Feature","properties":{"a":null,"b":null},"geometry":{"type":"Point","coordinates":[4,0]}},
                    {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[6,0]}}
                ]
            }))?,
        )?;
        let source = VectorSource::open(&path)?.context("Expected numeric vector data")?.remove(0);
        std::fs::remove_file(path)?;
        let source = source.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 10.0))?;
        Ok(source)
    }

    fn assert_numeric_vector_color_restyles(
        map: &mut NativeMap,
        request: MapRequest,
        source: &VectorSource,
        first: &RgbaImage,
    ) -> Result<(Arc<VectorSource>, RgbaImage)> {
        let selected = source.with_property("b")?;
        let reversed = map.render_vector(request, Some(selected.clone()), 1.0, "#ff00ff")?;
        assert_numeric_vector_property_change(first, &reversed);

        let widened = selected.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 20.0))?;
        let ranged = map.render_vector(request, Some(widened.clone()), 1.0, "#ff00ff")?;
        assert_ne!(ranged.as_raw(), reversed.as_raw(), "Range changes must affect numeric colors");
        assert!(count_color(&ranged, [255, 255, 255, 255]) < 100);
        let colored = widened.restyled(inf::colormap::ColorMapPreset::Turbo, widened.range().unwrap())?;
        let palette_changed = map.render_vector(request, Some(colored.clone()), 1.0, "#ff00ff")?;
        assert_ne!(palette_changed.as_raw(), ranged.as_raw(), "Palette changes must repaint");
        Ok((colored, palette_changed))
    }

    fn assert_numeric_vector_basemap_reload(map: &mut NativeMap, request: MapRequest, translucent: &RgbaImage, camera_updates: u64) -> Result<()> {
        map.load_style(BasemapStyle::Light, Some(LIGHT_STYLE))?;
        map.render(request)?;
        map.load_style(BasemapStyle::Dark, Some(DARK_STYLE))?;
        assert_eq!(
            map.render(request)?.as_raw(),
            translucent.as_raw(),
            "Basemap reload must preserve numeric styling"
        );
        assert_eq!(map.camera_updates, camera_updates);
        Ok(())
    }

    fn assert_active_query_geometry(map: &mut NativeMap, request: MapRequest, source: Arc<VectorSource>) -> Result<()> {
        add_query_basemap_decoy(map)?;
        let rendered = map.render_vector(request, Some(source), 1.0, "#3b82f6")?;
        assert_eq!(rendered.dimensions(), (512, 512), "HiDPI readback must use physical pixels");
        let center = (128.0, 128.0);
        let decoy = map
            .renderer
            .query_rendered_features(ScreenCoordinate { x: center.0, y: center.1 }, &["basemap-decoy".to_owned()])?;
        assert_eq!(decoy.len(), 1, "The excluded basemap feature must really be queryable");
        assert_eq!(decoy[0]["properties"]["name"], "basemap-decoy");

        assert_overlapping_vector_queries(map, center)?;
        Ok(())
    }

    fn install_and_query_replacement_point(
        map: &mut NativeMap,
        request: MapRequest,
        dir: &std::path::Path,
        center: (f64, f64),
    ) -> Result<Vec<serde_json::Value>> {
        let replacement = open_query_fixture(
            dir,
            "replacement.geojson",
            serde_json::json!([
                {"type": "Feature", "properties": {"name": "replacement", "new_attribute": "active"},
                 "geometry": {"type": "Point", "coordinates": [0,0]}}
            ]),
        )?;
        let _ = map.render_vector(request, Some(replacement), 1.0, "#3b82f6")?;
        let switched = map.query_vector_features(center)?;
        assert_eq!(switched.len(), 1, "Switching must exclude the old vector and basemap features: {switched:?}");
        assert_eq!(switched[0]["properties"], serde_json::json!({"name": "replacement", "new_attribute": "active"}));
        assert_point_query_tolerance(map, center)?;
        Ok(switched)
    }

    fn assert_queries_survive_reload_and_clear(map: &mut NativeMap, request: MapRequest, center: (f64, f64), switched: Vec<serde_json::Value>) -> Result<()> {
        map.load_style(BasemapStyle::Light, Some(LIGHT_STYLE))?;
        let _ = map.render(request)?;
        assert_eq!(
            map.query_vector_features(center)?,
            switched,
            "Active feature queries must survive a style reload"
        );
        let _ = map.render_vector(request, None, 1.0, "#3b82f6")?;
        assert!(
            map.query_vector_features(center)?.is_empty(),
            "Clearing must leave no queryable vector features"
        );
        Ok(())
    }

    fn assert_raster_geographic_coverage(rendered: &RgbaImage, background: &RgbaImage, camera: MapCamera) {
        assert_eq!(rendered.dimensions(), (1024, 1024));
        let changed_pixels = rendered.pixels().zip(background.pixels()).filter(|(a, b)| a != b).count();
        assert!(
            changed_pixels > 100_000,
            "Raster tiles must visibly cover their geographic footprint, got {changed_pixels} pixels"
        );
        // Check both the geographic footprint and the fixture's nodata strip.
        for x in 0..rendered.width() {
            let longitude = camera.coordinate_at((f64::from(x) / 2.0 - 256.0, 0.0)).longitude;
            if !(-8.0..=10.5).contains(&longitude) {
                assert_eq!(
                    rendered.get_pixel(x, 512),
                    background.get_pixel(x, 512),
                    "Nodata/outside at {longitude} must be transparent"
                );
            } else if longitude > -6.0 && longitude < 9.0 {
                assert_ne!(rendered.get_pixel(x, 512), background.get_pixel(x, 512), "Data at {longitude} must be visible");
            }
        }
    }

    fn assert_cached_raster_restyles(
        map: &mut NativeMap,
        request: MapRequest,
        source: &RasterSource,
        path: &std::path::Path,
        rendered: &RgbaImage,
        background: &RgbaImage,
    ) -> Result<()> {
        let gray = source.restyled(inf::colormap::ColorMapPreset::Gray, source.range())?;
        let (requests, uploads) = map.numeric.borrow().tile_counts();
        let camera_updates = map.camera_updates;
        assert!(requests > 0 && uploads > 0, "The custom layer must load and upload numeric tiles");
        // The following restyles must work even if the data is no longer readable.
        std::fs::rename(path, path.with_extension("cached.tif"))?;
        let restyled = map.render_raster(request, Some(gray.clone()), 1.0)?;
        assert_ne!(rendered.as_raw(), restyled.as_raw(), "A palette change must recolor cached numeric tiles");
        let range = gray.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 200.0))?;
        let rescaled = map.render_raster(request, Some(range), 1.0)?;
        assert_ne!(restyled.as_raw(), rescaled.as_raw(), "A range change must update the shader mapping");
        assert_eq!(
            map.numeric.borrow().tile_counts(),
            (requests, uploads),
            "Restyling must not request or upload tile data"
        );
        assert_eq!(map.camera_updates, camera_updates, "Restyling must not reset the native camera");
        assert_raster_hidden_and_cleared(map, request, gray, background)?;
        Ok(())
    }

    fn assert_raster_hidden_and_cleared(map: &mut NativeMap, request: MapRequest, gray: Arc<RasterSource>, background: &RgbaImage) -> Result<()> {
        map.set_raster(Some(gray), 0.0)?;
        assert_eq!(map.render(request)?.as_raw(), background.as_raw(), "Zero opacity must reveal the basemap");
        map.set_raster(None, 1.0)?;
        assert_eq!(
            map.render(request)?.as_raw(),
            background.as_raw(),
            "Clearing the raster must remove its source and layer"
        );
        Ok(())
    }

    fn assert_band_loading_and_supersession(map: &mut NativeMap, request: MapRequest, sources: &[Arc<RasterSource>], first_image: &RgbaImage) -> Result<()> {
        map.set_raster(Some(sources[1].clone()), 1.0)?;
        map.apply_request(request);
        // The first render schedules new tiles, but still draws the old slot.
        let loading = map.required_raster_image("The outgoing slot must remain renderable")?;
        assert_eq!(first_image.as_raw(), loading.as_raw(), "Loading a band must not expose missing tiles");

        // Supersede in-flight work, then restyle only the incoming slot.
        let third = sources[2].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 160.0))?;
        map.set_raster(Some(sources[2].clone()), 1.0)?;
        map.set_raster(Some(third), 1.0)?;
        map.apply_request(request);
        assert_eq!(
            first_image.as_raw(),
            map.required_raster_image("Rapid band changes must keep drawing")?.as_raw()
        );
        Ok(())
    }

    fn assert_band_clear_and_reduced_motion(
        map: &mut NativeMap,
        request: MapRequest,
        sources: &[Arc<RasterSource>],
        fourth: Arc<RasterSource>,
        first_image: &RgbaImage,
        replacement: &RgbaImage,
    ) -> Result<()> {
        // Clearing while a replacement is pending must immediately reveal the map.
        map.set_raster(Some(sources[0].clone()), 1.0)?;
        map.apply_request(request);
        assert!(map.poll_frame()?.is_some());
        map.set_raster(None, 1.0)?;
        map.apply_request(request);
        assert!(map.poll_frame()?.is_some(), "Clearing must not wait for cancelled band jobs");
        assert!(!map.numeric.borrow().is_loading());

        let reference = map.render_raster(request, Some(fourth), 1.0)?;
        assert_eq!(replacement.as_raw(), reference.as_raw(), "The latest queued band and style must win");
        map.numeric.borrow_mut().set_reduce_motion(true);
        let first = sources[0].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 160.0))?;
        map.set_raster(Some(first), 1.0)?;
        map.apply_request(request);
        assert_reduced_motion_band_swap(map, &reference, first_image)?;
        Ok(())
    }

    fn assert_initial_fallback_loading(map: &mut NativeMap, request: MapRequest, sources: &[Arc<RasterSource>]) -> Result<()> {
        map.render(request)?;
        map.set_raster(Some(sources[0].clone()), 1.0)?;
        map.apply_request(request);
        assert!(map.poll_frame()?.is_some());
        assert!(
            map.numeric.borrow().pending_fallback_count() > 0,
            "Initial loading still needs coarse tiles for progressive display"
        );
        map.render(request)?;
        Ok(())
    }

    fn raster_fade_endpoints(map: &mut NativeMap, request: MapRequest, sources: &[Arc<RasterSource>]) -> Result<Vec<RgbaImage>> {
        map.numeric.borrow_mut().set_reduce_motion(true);
        let mut endpoints = Vec::new();
        for source in sources {
            map.set_raster(Some(source.clone()), 0.6)?;
            endpoints.push(map.render(request)?);
        }
        map.render_raster(request, Some(sources[0].clone()), 0.6)?;
        map.numeric.borrow_mut().set_reduce_motion(false);
        Ok(endpoints)
    }

    fn nodata_band_values(nr: usize) -> Vec<f32> {
        (0..4096)
            .map(|ix| match (nr, ix % 64) {
                (1, 0..16) | (2, 48..64) => -9999.0,
                (1, _) => 64.0,
                _ => 192.0,
            })
            .collect()
    }

    fn open_nodata_band_fixture(path: &std::path::Path) -> Result<Vec<Arc<RasterSource>>> {
        let mut dataset = new_fixture_dataset(path, [-10.0, -10.0, 10.0, 10.0], 2)?;
        for nr in 1..=2 {
            let mut band = dataset.rasterband(nr)?;
            band.set_no_data_value(Some(-9999.0))?;
            let values = nodata_band_values(nr);
            band.write((0, 0), (64, 64), &mut gdal::raster::Buffer::new((64, 64), values))?;
        }
        dataset.flush_cache()?;
        drop(dataset);
        let sources = RasterSource::open(path)?;
        Ok(sources)
    }

    fn nodata_fade_endpoints(map: &mut NativeMap, request: MapRequest, first: Arc<RasterSource>, second: &Arc<RasterSource>) -> Result<(RgbaImage, RgbaImage)> {
        map.numeric.borrow_mut().set_reduce_motion(true);
        let outgoing = map.render_raster(request, Some(first.clone()), 0.6)?;
        let incoming = map.render_raster(request, Some(second.clone()), 0.6)?;
        map.render_raster(request, Some(first), 0.6)?;
        map.numeric.borrow_mut().set_reduce_motion(false);
        Ok((outgoing, incoming))
    }

    fn assert_nodata_band_fade(map: &mut NativeMap, outgoing: &RgbaImage, incoming: &RgbaImage) -> Result<()> {
        let a = f32::from(outgoing.get_pixel(128, 128)[0]);
        let b = f32::from(incoming.get_pixel(128, 128)[0]);
        assert!((b - a).abs() > 50.0, "The fixture needs distinguishable endpoint colors");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut intermediate = false;
        loop {
            ensure!(Instant::now() < deadline, "Nodata band fade did not complete");
            let Some(image) = map.poll_raster_image()? else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let blend = ((f32::from(image.get_pixel(128, 128)[0]) - a) / (b - a)).clamp(0.0, 1.0);
            intermediate |= (0.1..0.9).contains(&blend);
            for ((actual, old), new) in image.as_raw().iter().zip(outgoing.as_raw()).zip(incoming.as_raw()) {
                let expected = f32::from(*old) * (1.0 - blend) + f32::from(*new) * blend;
                assert!(
                    (f32::from(*actual) - expected).abs() <= 3.0,
                    "Nodata/opacity must blend endpoint colors without a dark dip: {actual} vs {expected}"
                );
            }
            if !map.numeric.borrow().is_loading() {
                assert_eq!(image.as_raw(), incoming.as_raw());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(intermediate, "Exercise fractional shader blending, not only endpoints");
        Ok(())
    }

    fn assert_scaled_band_display(first: &Arc<RasterSource>, second: &Arc<RasterSource>) -> Result<(NativeMap, MapRequest, RgbaImage, RgbaImage)> {
        let first_color = legend_pixel(first, 43.0);
        let second_color = legend_pixel(second, 120.0);
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(first.metadata().bounds, 256.0, 256.0),
        };
        let mut map = NativeMap::with_style(viewport, Some(DARK_STYLE))?;
        let background = map.render(request)?;
        let mut outgoing = map.render_raster(request, Some(first.clone()), 1.0)?;
        assert_legend_center(&outgoing, first_color);
        assert_eq!(
            outgoing.get_pixel(16, 128),
            background.get_pixel(16, 128),
            "Scaled nodata must remain transparent"
        );

        // Exercise both uniform slots, then return with reduced motion. No
        // particular intermediate blend is required on a slow graphics device.
        for (incoming, expected, reduce_motion) in [(second.clone(), second_color, false), (first.clone(), first_color, true)] {
            outgoing = assert_scaled_band_transition(&mut map, request, incoming, &outgoing, expected, &background, reduce_motion)?;
        }

        assert_scaled_tiles_store_raw_values(first, second)?;
        Ok((map, request, outgoing, background))
    }

    fn assert_native_cover_occludes_band_changes(map: &mut NativeMap, request: MapRequest, first: &Arc<RasterSource>) -> Result<()> {
        // A native opaque layer above the custom layer must cover it even when
        // MapLibre draws opaque geometry before its translucent/custom pass.
        map.renderer.style().add_source(AnySource::from_json_str(
            "cover",
            r##"{"type":"geojson","data":{"type":"Polygon","coordinates":[[[-20,-20],[20,-20],[20,20],[-20,20],[-20,-20]]]}}"##,
        )?)?;
        map.renderer.style().add_layer(AnyLayer::from_json_str(
            r##"{"id":"cover","type":"fill","source":"cover","paint":{"fill-color":"#00ff00","fill-antialias":false}}"##,
        )?)?;
        let covered = map.render(request)?;
        assert_eq!(*covered.get_pixel(128, 128), image::Rgba([0, 255, 0, 255]));
        map.set_raster(Some(first.clone()), 1.0)?;
        assert_eq!(
            *map.render(request)?.get_pixel(128, 128),
            image::Rgba([0, 255, 0, 255]),
            "Band changes must preserve the custom layer's position below native layers"
        );
        Ok(())
    }

    fn assert_explicit_raster_layer_insertion(map: &mut NativeMap, request: MapRequest, first: Arc<RasterSource>, first_image: &RgbaImage) -> Result<()> {
        // The same host API supports explicit insertion before a native layer.
        map.set_raster(None, 1.0)?;
        map.numeric.borrow_mut().set_source(Some(first), 1.0, 1);
        map.renderer
            .style()
            .add_wgpu_custom_layer("below-cover", RasterLayerHost::new(map.numeric.clone()), Some("cover"))?;
        let below = map.render(request)?;
        assert_eq!(*below.get_pixel(128, 128), image::Rgba([0, 255, 0, 255]));
        map.renderer.style().remove_layer("cover");
        map.renderer.style().add_layer(AnyLayer::from_json_str(
            r##"{"id":"cover","type":"fill","source":"cover","paint":{"fill-color":"#00ff00","fill-opacity":0.5,"fill-antialias":false}}"##,
        )?)?;
        let blended = map.render(request)?;
        let raster_pixel = first_image.get_pixel(128, 128);
        let expected = [raster_pixel[0] / 2, 128 + raster_pixel[1] / 2, raster_pixel[2] / 2, 255];
        for (actual, expected) in blended.get_pixel(128, 128).0.into_iter().zip(expected) {
            assert!(
                actual.abs_diff(expected) <= 1,
                "Native translucent layers must still draw correctly after the custom callback"
            );
        }
        Ok(())
    }

    fn assert_landuse_water_coverage(source: &RasterSource, request: MapRequest, rendered: &RgbaImage, background: &RgbaImage) -> Result<()> {
        let mut tiles = std::collections::HashMap::new();
        let mut checked = 0;
        let coordinates = (0..rendered.height())
            .step_by(8)
            .flat_map(|y| (0..rendered.width()).step_by(8).map(move |x| (x, y)));
        for (x, y) in coordinates {
            let coordinate = request.camera.coordinate_at((f64::from(x) / 2.0 - 600.0, f64::from(y) / 2.0 - 380.0));
            let tile = geo::Tile::for_coordinate(coordinate, source.metadata().max_zoom);
            if let std::collections::hash_map::Entry::Vacant(entry) = tiles.entry(tile) {
                entry.insert(source.numeric_tile(tile, 2)?.context("Missing landuse tile")?);
            }
            let data = &tiles[&tile];
            let grid = geo::GeoReference::from_tile(&tile, data.width as usize, 1);
            let cell = grid.point_to_cell(geo::crs::lat_lon_to_web_mercator(coordinate));
            let value = data.values[(cell.row as u32 * data.width + cell.col as u32) as usize];
            if value == 173.0 || value == 174.0 {
                checked += 1;
                assert_ne!(
                    rendered.get_pixel(x, y),
                    background.get_pixel(x, y),
                    "Landuse {value} at {coordinate:?} must not reveal the basemap"
                );
            }
        }
        assert!(checked > 100, "The test must exercise the reported water values, checked {checked}");
        Ok(())
    }

    fn measure_raster_restyles(map: &mut NativeMap, request: MapRequest, source: &Arc<RasterSource>) -> Result<()> {
        let mut styling = Duration::ZERO;
        let mut rendering = Duration::ZERO;
        let mut readback = Duration::ZERO;
        for ix in 0..30 {
            let start = Instant::now();
            let source = source.restyled(source.palette(), (ix as f32, 160.0))?;
            map.set_raster(Some(source), 1.0)?;
            let styled = Instant::now();
            // Follow the production path: publish the first available frame,
            // rather than waiting for all native fades to become idle.
            map.apply_request(request);
            let texture = map.poll_frame()?.context("Range update did not produce a frame")?;
            let rendered = Instant::now();
            let _image = map.readback.read(&texture, &map.device, &map.queue)?;
            styling += styled - start;
            rendering += rendered - styled;
            readback += rendered.elapsed();
        }
        eprintln!(
            "Range update at 2400×1520: style {:?}, render {:?}, GPU wait/readback {:?} per frame",
            styling / 30,
            rendering / 30,
            readback / 30
        );
        Ok(())
    }

    fn wait_for_geographic_worker_frame(frames: &async_channel::Receiver<MapFrame>, viewport: Viewport) -> Result<Arc<RenderImage>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            ensure!(Instant::now() < deadline, "Streaming map frame timed out");
            let Ok(frame) = frames.try_recv() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let image = frame.image?;
            if worker_frame_has_geography(&image, frame.viewport, viewport) {
                return Ok(image);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_native_texture(map: &mut NativeMap) -> Result<wgpu::Texture> {
        loop {
            if let Some(texture) = map.poll_frame()? {
                return Ok(texture);
            }
        }
    }

    fn worker_frame_has_geography(image: &RenderImage, actual: Viewport, requested: Viewport) -> bool {
        if actual != requested {
            return false;
        }
        let colors: std::collections::HashSet<_> = image.as_bytes(0).unwrap().as_chunks::<4>().0.iter().collect();
        colors.len() > 100
    }

    struct UnitStyleCase {
        snapshot: Arc<RasterSource>,
        enabled: bool,
        range: (f32, f32),
        expected: u8,
        unchanged: bool,
    }

    fn unit_style_cases(first: &RasterSource) -> Result<([UnitStyleCase; 9], Arc<RasterSource>)> {
        let raw_equivalent = first.with_unscaling(false)?;
        let physical_roundtrip = raw_equivalent.with_unscaling(true)?;
        let raw_gray = raw_equivalent.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let physical_equivalent = raw_gray.with_unscaling(true)?;
        let physical_gray = physical_equivalent.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?;
        let raw_again = physical_gray.with_unscaling(false)?;
        let raw_clipped = raw_gray.restyled(inf::colormap::ColorMapPreset::Gray, (110.0, 200.0))?;
        let physical_clipped = raw_clipped.with_unscaling(true)?;
        let clipped_roundtrip = physical_clipped.with_unscaling(false)?;
        let cases = [
            (raw_equivalent, false, (14.0, 524.0), 43, true),
            (physical_roundtrip, true, (0.0, 255.0), 43, true),
            (raw_gray.clone(), false, (0.0, 255.0), 100, false),
            (physical_equivalent, true, (-7.0, 120.5), 100, false),
            (physical_gray, true, (0.0, 255.0), 43, true),
            (raw_again, false, (14.0, 524.0), 43, true),
            (raw_clipped, false, (110.0, 200.0), 0, false),
            (physical_clipped, true, (48.0, 93.0), 0, false),
            (clipped_roundtrip, false, (110.0, 200.0), 0, false),
        ]
        .map(|(snapshot, enabled, range, expected, unchanged)| UnitStyleCase {
            snapshot,
            enabled,
            range,
            expected,
            unchanged,
        });

        Ok((cases, raw_gray))
    }

    fn assert_local_style_worker_frame(frames: &async_channel::Receiver<MapFrame>, viewport: Viewport, scale: f32) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            ensure!(Instant::now() < deadline, "Local-style worker frame timed out");
            if let Ok(frame) = frames.try_recv()
                && frame.viewport == viewport
            {
                assert_eq!(frame.basemap, BasemapStyle::Light);
                let image = frame.image?;
                assert_eq!(image.size(0).width.0, (32.0 * scale) as i32);
                assert!(image.as_bytes(0).unwrap().as_chunks::<4>().0.iter().all(|pixel| *pixel == [32, 32, 32, 255]));
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}
