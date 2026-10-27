use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Disableable, Selectable, Sizable, Theme, TitleBar, alert::Alert};
use gpui_kit::{
    Anchor, App, AppContext, Bounds, Context, CursorStyle, DispatchPhase, Entity, ExternalPaths, FocusHandle, HoverListenerMode, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta,
    ScrollEvent, SharedString, StatefulInteractiveElement, Styled, StyledImage, Subscription, Task, TestSupportExt, Window, canvas, div, img, rems,
};

use crate::feature_info::FeatureInfo;
use crate::map_camera::MapCamera;
use crate::map_renderer::{BasemapStyle, MapFrame, MapRenderer, MapRequest, MapSource, Viewport};
use crate::pixel_info::PixelInfo;
use crate::raster_panel::{ClearRasters, FitRaster, OpenDataset, RasterPanel, ToggleRasterControls};
use crate::raster_source::RasterSource;
use crate::shortcuts::key_binding;
use crate::vector_source::VectorSource;

gpui_kit::actions!(map, [ZoomIn, ZoomOut, PanLeft, PanRight, PanUp, PanDown, ToggleTileBoundaries]);

pub fn init(cx: &mut App) -> anyhow::Result<()> {
    let workspace = workspace_key_bindings(cx)?;
    let navigation = navigation_key_bindings(cx)?;
    cx.bind_keys(workspace.into_iter().chain(navigation));
    Ok(())
}

fn workspace_key_bindings(cx: &App) -> anyhow::Result<[gpui_kit::KeyBinding; 7]> {
    Ok([
        key_binding("?", crate::shortcuts::ShowShortcuts, Some("RasterWorkspace"), cx)?,
        key_binding("secondary-,", crate::settings::OpenSettings, Some("RasterWorkspace"), cx)?,
        key_binding("secondary-\\", crate::appearance::CycleAppearance, Some("RasterWorkspace"), cx)?,
        key_binding("secondary-o", OpenDataset, Some("RasterWorkspace"), cx)?,
        key_binding("secondary-0", FitRaster, Some("RasterWorkspace"), cx)?,
        key_binding("secondary-l", ToggleRasterControls, Some("RasterWorkspace"), cx)?,
        key_binding("secondary-shift-d", ToggleTileBoundaries, Some("RasterWorkspace"), cx)?,
    ])
}

fn navigation_key_bindings(cx: &App) -> anyhow::Result<[gpui_kit::KeyBinding; 7]> {
    Ok([
        key_binding("+", ZoomIn, Some("MapNavigation"), cx)?,
        key_binding("=", ZoomIn, Some("MapNavigation"), cx)?,
        key_binding("-", ZoomOut, Some("MapNavigation"), cx)?,
        key_binding("left", PanLeft, Some("MapNavigation"), cx)?,
        key_binding("right", PanRight, Some("MapNavigation"), cx)?,
        key_binding("up", PanUp, Some("MapNavigation"), cx)?,
        key_binding("down", PanDown, Some("MapNavigation"), cx)?,
    ])
}

const DRAG_SAMPLE_WINDOW: Duration = Duration::from_millis(100);
const MIN_COAST_SPEED: f64 = 30.0;
const MAX_COAST_SPEED: f64 = 3_000.0;
const COAST_DECELERATION: f64 = 2_500.0;

struct MapDrag {
    position: Point<Pixels>,
    samples: VecDeque<(Instant, Point<Pixels>)>,
}

impl MapDrag {
    fn new(position: Point<Pixels>, now: Instant) -> Self {
        Self {
            position,
            samples: VecDeque::from([(now, position)]),
        }
    }

    fn record(&mut self, position: Point<Pixels>, now: Instant) {
        self.position = position;
        self.samples.push_back((now, position));
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        while self.samples.front().is_some_and(|(time, _)| now.duration_since(*time) > DRAG_SAMPLE_WINDOW) {
            self.samples.pop_front();
        }
    }

    fn release(mut self, now: Instant) -> Option<MapCoast> {
        self.prune(now);
        let (first_time, first) = self.samples.front()?;
        let (last_time, last) = self.samples.back()?;
        // Bound velocity for very short gestures and synthetic input bursts.
        let seconds = last_time.duration_since(*first_time).as_secs_f64().max(1.0 / 240.0);
        let delta = *last - *first;
        let velocity = (f64::from(f32::from(delta.x)) / seconds, f64::from(f32::from(delta.y)) / seconds);
        let speed = velocity.0.hypot(velocity.1);
        if speed < MIN_COAST_SPEED {
            return None;
        }
        let scale = (MAX_COAST_SPEED / speed).min(1.0);
        Some(MapCoast {
            velocity: (velocity.0 * scale, velocity.1 * scale),
            sampled_at: now,
        })
    }
}

struct MapCoast {
    velocity: (f64, f64),
    sampled_at: Instant,
}

impl MapCoast {
    fn advance(&mut self, now: Instant) -> (f64, f64) {
        let speed = self.velocity.0.hypot(self.velocity.1);
        if speed == 0.0 {
            return (0.0, 0.0);
        }
        let seconds = now.duration_since(self.sampled_at).as_secs_f64().min(speed / COAST_DECELERATION);
        self.sampled_at = now;
        let remaining = (speed - COAST_DECELERATION * seconds).max(0.0);
        let distance_scale = (speed + remaining) * 0.5 * seconds / speed;
        let delta = (self.velocity.0 * distance_scale, self.velocity.1 * distance_scale);
        self.velocity.0 *= remaining / speed;
        self.velocity.1 *= remaining / speed;
        delta
    }
}

fn basemap_style(cx: &App) -> BasemapStyle {
    if cx.theme().mode.is_dark() { BasemapStyle::Dark } else { BasemapStyle::Light }
}

#[derive(PartialEq)]
struct RenderKey {
    map: MapRequest,
    raster_id: Option<u64>,
    vector_id: Option<u64>,
    vector_style_id: Option<u64>,
    vector_color: String,
    opacity: f32,
    reduce_motion: bool,
    basemap: BasemapStyle,
    tile_boundaries: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct VectorHover {
    map: MapRequest,
    layer_id: u64,
    point: (f64, f64),
}

pub struct MapView {
    viewport: Option<Viewport>,
    bounds: Option<Bounds<Pixels>>,
    camera: MapCamera,
    requested: Option<RenderKey>,
    tile_boundaries: bool,
    raster_panel: Entity<RasterPanel>,
    pixel_info: Entity<PixelInfo>,
    feature_info: Entity<FeatureInfo>,
    vector_hover: Option<VectorHover>,
    hover_revision: u64,
    _feature_task: Option<Task<()>>,
    pointer: Option<Point<Pixels>>,
    raster: Option<Arc<RasterSource>>,
    vector: Option<Arc<VectorSource>>,
    opacity: f32,
    fit_pending: bool,
    _raster_subscription: Subscription,
    _raster_observation: Subscription,
    _theme_observation: Subscription,
    renderer: Option<MapRenderer>,
    drag: Option<MapDrag>,
    coast: Option<MapCoast>,
    coast_frame_pending: bool,
    focus: FocusHandle,
    image: Option<Arc<RenderImage>>,
    traced_frame_vector: Option<u64>,
    error: Option<SharedString>,
    _frames_task: Option<Task<()>>,
}

impl MapView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let renderer = MapRenderer::new().expect("Failed to create the map renderer");
        Self::with_renderer(Some(renderer), window, cx)
    }

    fn with_renderer(renderer: Option<MapRenderer>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let task = renderer.as_ref().map(|renderer| Self::watch_frames(renderer, cx));
        let feature_task = renderer.as_ref().map(|renderer| Self::watch_feature_results(renderer, cx));
        let raster_panel = cx.new(|cx| RasterPanel::new(window, cx));

        let raster_observation = cx.observe(&raster_panel, |_, _, cx| cx.notify());
        let theme_observation = cx.observe_global::<Theme>(|view, cx| view.request_render(cx));
        let raster_subscription = cx.subscribe(&raster_panel, |view, _, event, cx| view.apply_raster_event(event, cx));
        cx.on_release_in(window, |view, window, cx| {
            if let Some(image) = view.image.take() {
                cx.drop_image(image, Some(window));
            }
        })
        .detach();
        let focus = cx.focus_handle().tab_stop(true);
        window.focus(&focus, cx);
        Self {
            viewport: None,
            bounds: None,
            camera: MapCamera::default(),
            requested: None,
            tile_boundaries: false,
            raster_panel,
            pixel_info: cx.new(|_| PixelInfo::new()),
            feature_info: cx.new(|_| FeatureInfo::new()),
            vector_hover: None,
            hover_revision: 0,
            _feature_task: feature_task,
            pointer: None,
            raster: None,
            vector: None,
            opacity: 1.0,
            fit_pending: false,
            _raster_subscription: raster_subscription,
            _raster_observation: raster_observation,
            _theme_observation: theme_observation,
            renderer,
            drag: None,
            coast: None,
            coast_frame_pending: false,
            focus,
            image: None,
            traced_frame_vector: None,
            error: None,
            _frames_task: task,
        }
    }

    fn apply_raster_event(&mut self, event: &crate::raster_panel::RasterEvent, cx: &mut Context<Self>) {
        let raster = event.source();
        let vector = event.vector_source();
        let selection_changed = self.raster.as_ref().map(|source| source.metadata().id) != raster.as_ref().map(|source| source.metadata().id)
            || self.vector.as_ref().map(|source| source.id()) != vector.as_ref().map(|source| source.id());
        self.raster = raster;
        self.vector = vector;
        self.opacity = event.opacity();
        self.coast = None;
        self.drag = None;
        self.fit_pending = event.should_fit() || (selection_changed && self.source_is_offscreen());
        self.fit_raster();
        self.request_render(cx);
    }

    fn source_is_offscreen(&self) -> bool {
        let Some(bounds) = self.source_bounds() else { return false };
        self.viewport.is_none_or(|viewport| {
            !self
                .camera
                .intersects_bounds(bounds, f64::from(viewport.width.get()), f64::from(viewport.height.get()))
        })
    }

    pub fn open_paths(&self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.raster_panel.update(cx, |panel, cx| panel.open_paths(paths, cx));
    }

    fn source_bounds(&self) -> Option<[f64; 4]> {
        self.raster
            .as_ref()
            .map(|source| source.metadata().bounds)
            .or_else(|| self.vector.as_ref().map(|source| source.bounds()))
    }

    fn fit_raster(&mut self) {
        if self.fit_pending
            && let (Some(bounds), Some(viewport)) = (self.source_bounds(), self.viewport)
        {
            self.camera = MapCamera::fit_bounds(bounds, f64::from(viewport.width.get()), f64::from(viewport.height.get()));
            self.fit_pending = false;
        }
    }

    fn watch_frames(renderer: &MapRenderer, cx: &mut Context<Self>) -> Task<()> {
        let frames = renderer.frames();
        cx.spawn(async move |this, cx| {
            while let Ok(frame) = frames.recv().await
                && this.update_in(cx, |view, window, cx| view.apply_frame(frame, window, cx)).is_ok()
            {}
        })
    }

    fn watch_feature_results(renderer: &MapRenderer, cx: &mut Context<Self>) -> Task<()> {
        let results = renderer.feature_results();
        cx.spawn(async move |this, cx| {
            while let Ok(result) = results.recv().await
                && this
                    .update(cx, |view, cx| view.apply_feature_result(result.revision(), result.into_features(), cx))
                    .is_ok()
            {}
        })
    }

    fn apply_feature_result(&self, revision: u64, features: Result<Vec<serde_json::Value>, String>, cx: &mut Context<Self>) {
        if revision != self.hover_revision || self.vector_hover.is_none() {
            return;
        }
        match features {
            Ok(features) => self.show_vector_features(features, cx),
            Err(error) => {
                log::warn!("Could not query vector features: {error}");
                self.show_vector_features(Vec::new(), cx);
            }
        }
    }

    fn show_vector_features(&self, features: Vec<serde_json::Value>, cx: &mut Context<Self>) {
        let (Some(hover), Some(source)) = (self.vector_hover, &self.vector) else {
            return;
        };
        if hover.layer_id != source.id() {
            return;
        }
        self.feature_info.update(cx, |info, cx| info.show(source.name(), features, cx));
    }

    fn clear_vector_hover(&mut self, cx: &mut Context<Self>) {
        if self.vector_hover.take().is_some() {
            self.hover_revision = self.hover_revision.wrapping_add(1);
            if let Some(renderer) = &self.renderer {
                renderer.clear_feature_query();
            }
        }
        self.feature_info.update(cx, |info, cx| info.clear(cx));
    }

    fn apply_frame(&mut self, frame: MapFrame, window: &mut Window, cx: &mut Context<Self>) {
        if !self.frame_is_current(&frame, cx) {
            return;
        }
        match frame.image {
            Ok(image) => {
                self.trace_vector_frame(frame.vector_layer_id, cx);
                self.replace_image(image, window, cx);
                if frame.settled
                    && let Some(source) = frame.vector_layer_id
                {
                    self.raster_panel.update(cx, |panel, cx| panel.complete_vector_render(source, cx));
                }
                self.error = None;
            }
            Err(error) => {
                log::error!("Map rendering failed: {error:#}");
                self.error = Some(format!("Couldn’t load the map. {error}").into());
                if let Some(source) = frame.vector_layer_id {
                    self.raster_panel.update(cx, |panel, cx| panel.complete_vector_render(source, cx));
                }
            }
        }
        cx.notify();
    }

    fn frame_is_current(&self, frame: &MapFrame, cx: &App) -> bool {
        // The worker completes frames in order. Show progress for this band
        // even if another range/palette update arrived while it was rendering;
        // requiring an exact style revision starves the display during dragging.
        self.viewport == Some(frame.viewport)
            && frame.basemap == basemap_style(cx)
            && frame.tile_boundaries == self.tile_boundaries
            && self.raster.as_ref().map(|source| source.metadata().id) == frame.raster_layer_id
            && self.vector.as_ref().map(|source| source.id()) == frame.vector_layer_id
    }

    fn trace_vector_frame(&mut self, layer_id: Option<u64>, cx: &App) {
        if layer_id == self.traced_frame_vector {
            return;
        }
        if let Some(source) = layer_id {
            log::info!(target: "map::vector_load", "source={source} stage=ui-first-requested-source-frame-applied since_last_open_request={:?} (may still be background-only)", self.raster_panel.read(cx).load_started().map(|started| started.elapsed()));
        }
        self.traced_frame_vector = layer_id;
    }

    fn replace_image(&mut self, image: Arc<RenderImage>, window: &mut Window, cx: &mut Context<Self>) {
        if self.image.as_ref().is_none_or(|previous| !Arc::ptr_eq(previous, &image))
            && let Some(previous) = self.image.replace(image)
        {
            // Dropping the Arc does not evict GPUI's uploaded texture.
            cx.drop_image(previous, Some(window));
        }
    }

    fn request_render(&mut self, cx: &mut Context<Self>) {
        self.update_pixel_info(cx);
        let Some(viewport) = self.viewport else {
            return;
        };
        let request = MapRequest { viewport, camera: self.camera };
        let reduce_motion = cx.reduce_motion();
        let basemap = basemap_style(cx);
        let primary = cx.theme().primary;
        let vector_color = format!("hsla({}, {}%, {}%, {})", primary.h * 360.0, primary.s * 100.0, primary.l * 100.0, primary.a);
        let key = RenderKey {
            map: request,
            raster_id: self.raster.as_ref().map(|source| source.id()),
            vector_id: self.vector.as_ref().map(|source| source.id()),
            vector_style_id: self.vector.as_ref().map(|source| source.style_id()),
            vector_color: vector_color.clone(),
            opacity: self.opacity,
            reduce_motion,
            basemap,
            tile_boundaries: self.tile_boundaries,
        };
        if self.requested.as_ref() == Some(&key) {
            return;
        }
        self.requested = Some(key);
        let Some(renderer) = &self.renderer else {
            return;
        };
        self.error = None;
        let source = self
            .raster
            .clone()
            .map(MapSource::Raster)
            .or_else(|| self.vector.clone().map(|source| MapSource::Vector(source, vector_color)));
        if let Err(error) = renderer.render_layer(request, source, self.opacity, reduce_motion, basemap, self.tile_boundaries) {
            self.error = Some(format!("Couldn’t load the map. {error}").into());
            if let Some(source) = self.vector.as_ref().map(|source| source.id()) {
                self.raster_panel.update(cx, |panel, cx| panel.complete_vector_render(source, cx));
            }
        }
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let renderer = MapRenderer::new().expect("Failed to create the map renderer");
        self._frames_task = Some(Self::watch_frames(&renderer, cx));
        self._feature_task = Some(Self::watch_feature_results(&renderer, cx));
        self.vector_hover = None;
        self.renderer = Some(renderer);
        self.requested = None;
        self.request_render(cx);
    }

    fn toggle_tile_boundaries(&mut self, cx: &mut Context<Self>) {
        self.tile_boundaries = !self.tile_boundaries;
        self.request_render(cx);
        cx.notify();
    }

    fn pan(&mut self, dx: f64, dy: f64, cx: &mut Context<Self>) {
        self.coast = None;
        self.camera.pan(dx, dy);
        self.request_render(cx);
    }

    fn zoom(&mut self, delta: f64, offset: (f64, f64), cx: &mut Context<Self>) {
        self.coast = None;
        self.camera.zoom_at(delta, offset);
        self.request_render(cx);
    }

    fn end_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            self.coast = if cx.reduce_motion() { None } else { drag.release(Instant::now()) };
            self.schedule_coast(window, cx);
            cx.notify();
        }
    }

    fn schedule_coast(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.coast.is_none() || self.coast_frame_pending {
            return;
        }
        self.coast_frame_pending = true;
        let view = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |view, cx| {
                view.coast_frame_pending = false;
                view.advance_coast(Instant::now(), cx);
                view.schedule_coast(window, cx);
            });
        });
    }

    fn advance_coast(&mut self, now: Instant, cx: &mut Context<Self>) {
        if cx.reduce_motion() {
            self.coast = None;
            return;
        }
        let Some(coast) = &mut self.coast else { return };
        let (dx, dy) = coast.advance(now);
        if coast.velocity == (0.0, 0.0) {
            self.coast = None;
        }
        self.camera.pan(dx, dy);
        self.request_render(cx);
    }

    fn drag_map(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !event.dragging() {
            self.end_drag(window, cx);
            return;
        }
        // A drag begun outside this map must not start map navigation.
        if let Some(drag) = &mut self.drag {
            let delta = event.position - drag.position;
            drag.record(event.position, Instant::now());
            self.pan(f64::from(f32::from(delta.x)), f64::from(f32::from(delta.y)), cx);
        }
    }

    fn update_pixel_info(&mut self, cx: &mut Context<Self>) {
        let (Some(position), Some(bounds)) = (self.pointer, self.bounds) else {
            self.clear_vector_hover(cx);
            return;
        };
        if let (Some(source), Some(viewport)) = (&self.vector, self.viewport) {
            let offset = position - bounds.center();
            let coordinate = self.camera.coordinate_at((f64::from(f32::from(offset.x)), f64::from(f32::from(offset.y))));
            self.pixel_info.update(cx, |info, cx| info.show(coordinate, None, 1, cx));
            let offset = position - bounds.origin;
            let hover = VectorHover {
                map: MapRequest { viewport, camera: self.camera },
                layer_id: source.id(),
                point: (f64::from(f32::from(offset.x)), f64::from(f32::from(offset.y))),
            };
            self.update_vector_hover(hover, cx);
            return;
        }
        self.clear_vector_hover(cx);
        let offset = position - bounds.center();
        let coordinate = self.camera.coordinate_at((f64::from(f32::from(offset.x)), f64::from(f32::from(offset.y))));
        let dpi = self.viewport.map_or(1, |viewport| viewport.scale.round().clamp(1.0, 4.0) as u8);
        self.pixel_info.update(cx, |info, cx| info.show(coordinate, self.raster.clone(), dpi, cx));
    }

    fn update_vector_hover(&mut self, hover: VectorHover, cx: &mut Context<Self>) {
        if self.vector_hover == Some(hover) {
            return;
        }
        let same_layer = self.vector_hover.is_some_and(|previous| previous.layer_id == hover.layer_id);
        self.hover_revision = self.hover_revision.wrapping_add(1);
        self.vector_hover = Some(hover);
        // Retain the last hit while refreshing this layer, including
        // camera changes, so pending queries do not flash an empty pane.
        if !same_layer {
            self.show_vector_features(Vec::new(), cx);
        }
        if let Some(renderer) = &self.renderer {
            renderer.query_features(self.hover_revision, hover.map, hover.layer_id, hover.point);
        }
    }
}

impl MapView {
    fn render_map_canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let previous_bounds = self.bounds;
        let previous_viewport = self.viewport;
        let drag_move = cx.listener(|view, event: &MouseMoveEvent, window, cx| view.drag_map(event, window, cx));
        let drag_end = cx.listener(|view, event: &MouseUpEvent, window, cx| {
            if event.button == MouseButton::Left {
                view.end_drag(window, cx)
            }
        });

        canvas(
            move |bounds, window, cx| Self::measure_map(view, bounds, previous_bounds, previous_viewport, window, cx),
            move |_, _, window, _| Self::track_map_drag(window, drag_move, drag_end),
        )
        .absolute()
        .size_full()
    }

    fn measure_map(
        view: gpui_kit::WeakEntity<Self>,
        bounds: Bounds<Pixels>,
        previous_bounds: Option<Bounds<Pixels>>,
        previous_viewport: Option<Viewport>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Native raster dimensions follow resolved layout and display DPI,
        // rather than the application's rem-based control scale.
        let Some(width) = NonZeroU32::new(f32::from(bounds.size.width).round() as u32) else {
            return;
        };
        let Some(height) = NonZeroU32::new(f32::from(bounds.size.height).round() as u32) else {
            return;
        };
        let viewport = Viewport {
            width,
            height,
            scale: window.scale_factor(),
        };
        if previous_viewport != Some(viewport) || previous_bounds != Some(bounds) {
            window.defer(cx, move |_, cx| {
                let _ = view.update(cx, |view, cx| view.apply_viewport(bounds, viewport, cx));
            });
        }
    }

    fn apply_viewport(&mut self, bounds: Bounds<Pixels>, viewport: Viewport, cx: &mut Context<Self>) {
        self.bounds = Some(bounds);
        self.viewport = Some(viewport);
        self.fit_raster();
        self.request_render(cx);
    }

    fn track_map_drag(
        window: &mut Window,
        drag_move: impl Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static,
        drag_end: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
    ) {
        // Register window-wide drag tracking each frame so dragging
        // and release continue across the map's boundary.
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                drag_move(event, window, cx)
            }
        });
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                drag_end(event, window, cx)
            }
        });
    }

    fn render_surface(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let focus_ring = cx.theme().ring;
        let mut surface = div()
            .id("map-surface")
            .test_support()
            .track_focus(&self.focus)
            .key_context("MapNavigation")
            .on_action(cx.listener(|view, _: &ZoomIn, _, cx| view.zoom(0.5, (0.0, 0.0), cx)))
            .on_action(cx.listener(|view, _: &ZoomOut, _, cx| view.zoom(-0.5, (0.0, 0.0), cx)))
            .on_action(cx.listener(|view, _: &PanLeft, _, cx| view.pan(64.0, 0.0, cx)))
            .on_action(cx.listener(|view, _: &PanRight, _, cx| view.pan(-64.0, 0.0, cx)))
            .on_action(cx.listener(|view, _: &PanUp, _, cx| view.pan(0.0, 64.0, cx)))
            .on_action(cx.listener(|view, _: &PanDown, _, cx| view.pan(0.0, -64.0, cx)))
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .border_1()
            .border_color(cx.theme().border)
            .focus_visible(move |style| style.border_color(focus_ring))
            .cursor(if self.drag.is_some() { CursorStyle::ClosedHand } else { CursorStyle::Arrow })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    window.focus(&view.focus, cx);
                    view.coast = None;
                    view.drag = Some(MapDrag::new(event.position, Instant::now()));
                    cx.notify();
                }),
            )
            .on_scroll(cx.listener(|view, event: &ScrollEvent, _, cx| {
                let Some(bounds) = view.bounds else { return };
                let offset = event.position - bounds.center();
                let delta = match event.delta {
                    ScrollDelta::Lines(delta) => f64::from(delta.y) * 0.25,
                    ScrollDelta::Pixels(delta) => f64::from(f32::from(delta.y)) / 240.0,
                };
                view.zoom(delta, (f64::from(f32::from(offset.x)), f64::from(f32::from(offset.y))), cx);
                cx.stop_propagation();
            }))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                view.pointer = Some(event.position);
                view.update_pixel_info(cx);
            }))
            // Pointer presence must survive scroll/keyboard hover suppression.
            .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
            .on_hover(cx.listener(|view, hovered, _, cx| {
                if !hovered {
                    view.pointer = None;
                    view.clear_vector_hover(cx);
                    view.pixel_info.update(cx, |info, cx| info.clear(cx));
                }
            }))
            .child(self.render_map_canvas(cx));
        if let Some(image) = &self.image {
            surface = surface.child(img(image.clone()).absolute().size_full().object_fit(ObjectFit::Fill));
        }
        if self.error.is_none() && self.image.is_none() {
            surface = surface.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child("Loading map…"),
            );
        }

        surface
    }

    fn render_alerts(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let load_error = self.raster_panel.read(cx).load_error();
        let mut alerts = div().absolute().bottom_0().left_0().flex().flex_col().gap_3().w_full();
        if self.error.is_some() || load_error.is_some() {
            alerts = alerts.p_3();
        }
        if let Some(error) = &self.error {
            alerts = alerts.child(
                div()
                    .id("map-error-banner")
                    .test_support()
                    .bg(cx.theme().background)
                    .occlude()
                    .flex()
                    .items_center()
                    .child(
                        Alert::error("map-error", error.clone())
                            .banner()
                            .flex_1()
                            .min_w_0()
                            .on_close(cx.listener(|view, _, _, cx| {
                                view.error = None;
                                cx.notify();
                            })),
                    )
                    .child(Button::new("retry-map").label("Retry").on_click(cx.listener(|view, _, _, cx| view.retry(cx)))),
            );
        }
        if let Some(error) = load_error {
            alerts = alerts.child(div().id("raster-error-banner").test_support().bg(cx.theme().background).occlude().child(
                Alert::error("raster-error", error).banner().on_close(cx.listener(|view, _, _, cx| {
                    view.raster_panel.update(cx, |panel, cx| panel.dismiss_load_error(cx));
                    cx.notify();
                })),
            ));
        }
        alerts
    }

    fn render_title_sources(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("open-raster")
                    .small()
                    .ghost()
                    .icon(IconName::FolderOpen)
                    .accessibility_label("Open dataset")
                    .tooltip_with_action("Open dataset", &OpenDataset, Some("RasterWorkspace"))
                    .loading(self.raster_panel.read(cx).is_loading())
                    .disabled(self.raster_panel.read(cx).is_loading())
                    .on_click(cx.listener(|view, _, _, cx| view.raster_panel.update(cx, |panel, cx| panel.prompt(cx)))),
            )
            .child(self.raster_panel.update(cx, |panel, cx| panel.render_layer_selector(window, cx)))
            .children(self.raster_panel.read(cx).load_status().map(|status| {
                div()
                    .id("vector-load-status")
                    .role(gpui_kit::Role::Status)
                    .aria_label(status.clone())
                    .test_support()
                    .min_w_0()
                    .max_w(rems(12.0))
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(status)
            }))
    }

    fn render_title_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .pr_2()
            .child(
                Button::new("toggle-tile-boundaries")
                    .small()
                    .ghost()
                    .icon(IconName::Bug)
                    .accessibility_label("Debug mode")
                    .tooltip_with_action("Debug mode: tile boundaries, tile IDs, and FPS", &ToggleTileBoundaries, Some("RasterWorkspace"))
                    .selected(self.tile_boundaries)
                    .toggled(self.tile_boundaries)
                    .on_click(cx.listener(|view, _, _, cx| view.toggle_tile_boundaries(cx))),
            )
            .child(
                Button::new("open-settings")
                    .small()
                    .ghost()
                    .icon(IconName::Settings)
                    .accessibility_label("Settings")
                    .tooltip_with_action("Settings…", &crate::settings::OpenSettings, Some("RasterWorkspace"))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::settings::OpenSettings), cx)),
            )
    }

    fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        TitleBar::new()
            .child(self.render_title_sources(window, cx))
            .child(self.render_title_actions(cx))
    }

    fn render_map_content(&self, surface: impl IntoElement, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(surface)
            .child(div().absolute().bottom_3().left_3().max_w_full().child(self.pixel_info.clone()))
            .children(self.vector.is_some().then(|| {
                div()
                    .absolute()
                    .top_3()
                    .left_3()
                    // Leave the right-hand controls and their gap unobstructed.
                    .right(rems(21.5))
                    .bottom_12()
                    .flex()
                    .flex_col()
                    .max_w(rems(24.0))
                    .child(self.feature_info.clone())
            }))
            .children((self.raster.is_some() || self.vector.is_some()).then(|| {
                div()
                    .absolute()
                    .top_3()
                    .right_3()
                    .bottom_3()
                    .flex()
                    .flex_col()
                    .w_80()
                    .max_w_full()
                    .child(self.raster_panel.clone())
            }))
            .children(self.tile_boundaries.then(|| gpui_fps::fps_monitor(window, cx).anchor(Anchor::TopLeft)))
            .children(self.raster_panel.read(cx).render_load_progress())
    }
}

impl Render for MapView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let surface = self.render_surface(cx);
        let alerts = self.render_alerts(cx);
        div()
            .id("raster-workspace")
            .key_context("RasterWorkspace")
            .on_action(cx.listener(|view, _: &crate::shortcuts::ShowShortcuts, window, cx| {
                view.coast = None;
                view.drag = None;
                crate::shortcuts::open(window, cx);
            }))
            .on_action(cx.listener(|view, _: &crate::settings::OpenSettings, window, cx| {
                view.coast = None;
                view.drag = None;
                crate::settings::open(window, cx);
            }))
            .on_action(|_: &crate::appearance::CycleAppearance, window, cx| crate::appearance::cycle_mode(window, cx))
            .on_action(cx.listener(|view, _: &OpenDataset, _, cx| view.raster_panel.update(cx, |panel, cx| panel.prompt(cx))))
            .on_action(cx.listener(|view, _: &FitRaster, _, cx| view.raster_panel.update(cx, |panel, cx| panel.fit(cx))))
            .on_action(cx.listener(|view, _: &ClearRasters, window, cx| view.raster_panel.update(cx, |panel, cx| panel.clear(window, cx))))
            .on_action(cx.listener(|view, _: &ToggleRasterControls, window, cx| view.raster_panel.update(cx, |panel, cx| panel.toggle_controls(window, cx))))
            .on_action(cx.listener(|view, _: &ToggleTileBoundaries, _, cx| view.toggle_tile_boundaries(cx)))
            .on_drop(cx.listener(|view, paths: &ExternalPaths, _, cx| {
                log::info!(target: "map::vector_load", "stage=file-drop paths={:?}", paths.0);
                view.open_paths(paths.0.to_vec(), cx);
            }))
            .flex()
            .flex_col()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(window, cx))
            .child(self.render_map_content(surface, window, cx))
            .child(alerts)
    }
}

#[cfg(test)]
#[path = "map_view_progress_tests.rs"]
mod progress_tests;

#[cfg(test)]
#[path = "map_view_vector_timing_tests.rs"]
mod vector_timing_tests;

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, InputEvent as _, TestAppContext, point, px, size};

    use super::*;
    use crate::appearance::{Appearance, AppearanceMode};
    use gpui_kit::ElementId;
    use gpui_kit::component::{ThemeMode, ThemeRegistry};

    #[gpui_kit::test]
    fn appearance_shortcut_cycles_persists_and_replaces_the_mode_indicator(cx: &mut TestAppContext) {
        use crate::appearance::{self, Appearance, AppearanceMode};
        use gpui_kit::component::{ThemeMode, WindowExt};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("appearance.json");
        cx.update(|cx| {
            gpui_kit::init(cx);
            appearance::setup_test(Some(path.clone()), cx);
            init(cx).unwrap();
            cx.set_reduce_motion(true);
        });
        let mut view = None;
        let handle = cx.open_window(size(px(800.), px(600.)), |window, cx| {
            crate::follow_system_appearance(window, cx);
            appearance::select_theme(ThemeMode::Light, "Gruvbox Light", window, cx).unwrap();
            appearance::select_theme(ThemeMode::Dark, "Tokyo Night", window, cx).unwrap();
            let map = cx.new(|cx| new_test_map(window, cx));
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.press("\\", cx);
            assert_eq!(
                Appearance::mode(cx),
                AppearanceMode::System,
                "An unmodified backslash must not change appearance"
            );
            assert!(window.notifications(cx).is_empty());
        })
        .unwrap();
        cx.run_until_parked();
        let camera = cx.update(|cx| view.read(cx).camera);

        for (mode, saved_mode) in [
            (AppearanceMode::Light, "light"),
            (AppearanceMode::Dark, "dark"),
            (AppearanceMode::System, "system"),
            (AppearanceMode::Light, "light"),
        ] {
            cx.update_window(handle.into(), |_, window, cx| window.press("secondary-\\", cx)).unwrap();
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(Appearance::mode(cx), mode);
                let expected = match mode {
                    AppearanceMode::System => ThemeMode::from(window.appearance()),
                    AppearanceMode::Light => ThemeMode::Light,
                    AppearanceMode::Dark => ThemeMode::Dark,
                };
                assert_eq!(cx.theme().mode, expected);
                assert_eq!(cx.theme().theme_name().as_str(), Appearance::selected_theme(expected, cx));
                assert_eq!(view.read(cx).requested.as_ref().unwrap().basemap, basemap_style(cx));
                assert_eq!(view.read(cx).camera, camera);
                assert!(view.read(cx).focus.is_focused(window), "The indicator must not steal focus");
                assert!(window.find("notification").visible());
                assert_eq!(window.notifications(cx).len(), 1, "Repeated presses must replace the indicator");
                let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert_eq!(saved["mode"], saved_mode);
                assert_eq!(saved["light_theme"], "Gruvbox Light");
                assert_eq!(saved["dark_theme"], "Tokyo Night");
            })
            .unwrap();
        }

        // Advance through entrance, the auto-hide timeout, and exit separately.
        for delay in [Duration::from_secs(1), Duration::from_secs(6), Duration::from_secs(1)] {
            cx.executor().advance_clock(delay);
            cx.run_until_parked();
        }
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.notifications(cx).is_empty());
            assert!(window.try_find("notification").is_none());
            assert_eq!(Appearance::mode(cx), AppearanceMode::Light);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn appearance_changes_update_the_basemap_and_reject_obsolete_frames(cx: &mut TestAppContext) {
        use crate::appearance::{self, AppearanceMode};

        let directory = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            appearance::setup_test(Some(directory.path().join("appearance.json")), cx);
        });
        let mut view = None;
        let handle = cx.open_window(size(px(800.), px(600.)), |window, cx| {
            appearance::select_mode(AppearanceMode::Dark, window, cx).unwrap();
            let map = cx.new(|cx| new_test_map(window, cx));
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
        let camera = cx.update(|cx| {
            let map = view.read(cx);
            assert_eq!(map.requested.as_ref().unwrap().basemap, BasemapStyle::Dark);
            map.camera
        });

        for mode in [AppearanceMode::Light, AppearanceMode::Dark, AppearanceMode::System] {
            cx.update_window(handle.into(), |_, window, cx| {
                appearance::select_mode(mode, window, cx).unwrap();
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| {
                let expected = match mode {
                    AppearanceMode::Light => BasemapStyle::Light,
                    AppearanceMode::Dark => BasemapStyle::Dark,
                    AppearanceMode::System => {
                        if gpui_kit::component::ThemeMode::from(window.appearance()).is_dark() {
                            BasemapStyle::Dark
                        } else {
                            BasemapStyle::Light
                        }
                    }
                };
                view.update(cx, |map, cx| check_basemap_frame_identity(map, expected, camera, window, cx));
            })
            .unwrap();
        }
    }

    #[gpui_kit::test]
    fn selecting_offscreen_bands_fits_the_dataset_but_visible_bands_and_styles_keep_the_camera(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let directory = tempfile::tempdir().unwrap();
        let paths = write_separated_rasters(directory.path());
        let (view, handle) = open_test_map(size(px(900.), px(760.)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |map, cx| map.open_paths(paths, cx));
        })
        .unwrap();
        cx.run_until_parked();
        let panned = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let camera = view.update(cx, |map, _| {
                    assert_eq!(map.raster.as_ref().unwrap().metadata().bounds[0], 100.0);
                    let viewport = map.viewport.unwrap();
                    map.camera.zoom_at(1.0, (0.0, 0.0));
                    map.camera.pan(f64::from(viewport.width.get()) * 0.4, 0.0);
                    map.camera
                });
                window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
                camera
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert_eq!(view.read(cx).raster.as_ref().unwrap().metadata().band_nr, Some(2));
            assert_eq!(view.read(cx).camera, panned, "Overlapping bands must preserve the user's pan and zoom");
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let map = view.read(cx);
            let source = map.raster.as_ref().unwrap();
            assert_eq!(source.metadata().bounds[0], -10.0);
            let viewport = map.viewport.unwrap();
            let fitted = MapCamera::fit_bounds(source.metadata().bounds, f64::from(viewport.width.get()), f64::from(viewport.height.get()));
            assert_eq!(map.camera, fitted, "An offscreen dataset must be centered and fitted after selection");
            assert_eq!(map.requested.as_ref().unwrap().map.camera, fitted);
            view.update(cx, |map, _| map.camera.pan(10_000.0, 10_000.0));
            window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let map = view.read(cx);
            let viewport = map.viewport.unwrap();
            let bounds = map.raster.as_ref().unwrap().metadata().bounds;
            assert!(
                !map.camera
                    .intersects_bounds(bounds, f64::from(viewport.width.get()), f64::from(viewport.height.get())),
                "Restyling an offscreen dataset must not move the camera"
            );
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let map = view.read(cx);
            let viewport = map.viewport.unwrap();
            let bounds = map.raster.as_ref().unwrap().metadata().bounds;
            assert_eq!(
                map.camera,
                MapCamera::fit_bounds(bounds, f64::from(viewport.width.get()), f64::from(viewport.height.get())),
                "Selecting another offscreen band in the same file must also bring it into view"
            );
        });
    }

    #[gpui_kit::test]
    fn vector_property_and_palette_changes_invalidate_rendering_without_moving_the_camera(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("numeric.geojson");
        std::fs::write(
            &path,
            r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"a":0.125,"b":4},"geometry":{"type":"Point","coordinates":[4,50]}},
            {"type":"Feature","properties":{"a":0.875,"b":9},"geometry":{"type":"Point","coordinates":[6,52]}}
        ]}"#,
        )
        .unwrap();
        let (view, handle) = open_test_map(size(px(900.), px(900.)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |map, cx| map.open_paths(vec![path.clone()], cx));
        })
        .unwrap();
        cx.run_until_parked();
        let original = cx
            .update_window(handle.into(), |_, window, cx| {
                view.update(cx, |map, cx| {
                    map.camera = MapCamera::default();
                    map.request_render(cx);
                });
                window.render_frame(cx);
                let source = view.read(cx).vector.as_ref().unwrap().clone();
                assert_eq!(source.property(), Some("a"));
                window.click("vector-property", cx);
                window.press("down", cx);
                window.press("enter", cx);
                source
            })
            .unwrap();
        cx.run_until_parked();
        let property_style_id = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let map = view.read(cx);
                let source = map.vector.as_ref().unwrap();
                assert_eq!(source.property(), Some("b"));
                assert_eq!(source.id(), original.id());
                assert_ne!(source.style_id(), original.style_id());
                assert_eq!(map.camera, MapCamera::default());
                assert_eq!(map.requested.as_ref().unwrap().vector_id, Some(original.id()));
                assert_eq!(map.requested.as_ref().unwrap().vector_style_id, Some(source.style_id()));
                let style_id = source.style_id();
                window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
                style_id
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let map = view.read(cx);
            let source = map.vector.as_ref().unwrap();
            assert_ne!(source.style_id(), property_style_id);
            assert_eq!(source.palette(), inf::colormap::ColorMapPreset::Blues);
            assert_eq!(source.id(), original.id());
            assert_eq!(source.source_json(), original.source_json());
            assert_eq!(map.requested.as_ref().unwrap().vector_style_id, Some(source.style_id()));
            assert_eq!(map.requested.as_ref().unwrap().map.camera, MapCamera::default());
            assert_eq!(map.camera, MapCamera::default());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn opening_vectors_fits_the_camera_rejects_obsolete_frames_and_clears_layers(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extent.geojson");
        std::fs::write(
            &path,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[4,50],[6,52]]}}]}"#,
        )
        .unwrap();
        let (view, handle) = open_test_map(size(px(900.), px(760.)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |map, cx| map.open_paths(vec![path.clone()], cx));
        })
        .unwrap();
        cx.run_until_parked();
        let old_id = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let map = view.read(cx);
                let source = map.vector.as_ref().expect("The background open must install a vector source");
                assert!(map.raster.is_none());
                assert_eq!(source.bounds(), [4., 50., 6., 52.]);
                let viewport = map.viewport.unwrap();
                let fitted = MapCamera::fit_bounds(source.bounds(), f64::from(viewport.width.get()), f64::from(viewport.height.get()));
                assert_eq!(map.camera, fitted);
                assert_ne!(map.camera, MapCamera::default());
                assert!(!map.fit_pending);
                let requested = map.requested.as_ref().unwrap();
                assert_eq!(requested.vector_id, Some(source.id()));
                assert_eq!(requested.raster_id, None);
                assert_eq!(requested.map.camera, fitted);
                let old_id = source.id();
                for id in ["raster-palette", "raster-legend", "raster-range-slider", "raster-min", "raster-max"] {
                    assert!(window.try_find(id).is_none(), "Raster-only control {id} must be absent for vectors");
                }
                assert!(window.find("raster-band").visible());
                // Reopen through the normal worker to obtain a real obsolete source identity.
                view.update(cx, |map, cx| map.open_paths(vec![path], cx));
                old_id
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |map, cx| {
                check_vector_frame_identity(map, old_id, window, cx);
            });
            view.read(cx).raster_panel.clone().update(cx, |panel, cx| panel.clear(window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let map = view.read(cx);
            assert!(map.vector.is_none());
            assert!(map.raster.is_none());
            let requested = map.requested.as_ref().unwrap();
            assert_eq!(requested.vector_id, None);
            assert_eq!(requested.raster_id, None);
            assert!(window.try_find("raster-opacity").is_none());
            assert_eq!(window.find("raster-band").disabled(), Some(true));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn closing_a_raster_error_does_not_drag_the_map(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let directory = tempfile::tempdir().unwrap();
        let mut view = None;
        let handle = cx.open_window(size(px(640.0), px(400.0)), |window, cx| {
            let map = cx.new(|cx| {
                let map = new_test_map(window, cx);
                map.open_paths(vec![directory.path().join("missing.tif")], cx);
                map
            });
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let camera = view.read(cx).camera;
            let banner = window.find("raster-error-banner").bounds();
            window.click_at("raster-error-banner", point(banner.size.width - px(24.0), banner.size.height * 0.5), cx);
            window.render_frame(cx);
            assert!(window.try_find("raster-error-banner").is_none());
            assert!(view.read(cx).drag.is_none());
            assert_eq!(view.read(cx).camera, camera);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn vector_load_progress_overlays_an_empty_map_without_stealing_focus_or_navigation(cx: &mut TestAppContext) {
        use crate::raster_panel::progress_tests::ControlledLoad;

        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
            cx.set_reduce_motion(true);
        });
        let (view, handle) = open_test_map(size(px(800.0), px(600.0)), cx);
        let (load, map_bounds) = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                assert!(view.read(cx).raster.is_none() && view.read(cx).vector.is_none());
                assert!(window.try_find("vector-load-progress").is_none());
                assert!(window.try_find("raster-controls").is_none());
                let bounds = window.find("map-surface").bounds();
                let load = ControlledLoad::start(&view.read(cx).raster_panel.clone(), cx);
                (load, bounds)
            })
            .unwrap();
        load.report_conversion(0.45);
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert_vector_progress_layout(map_bounds, window, cx);

            let camera = view.read(cx).camera;
            window.scroll("vector-load-progress", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_ne!(view.read(cx).camera, camera, "The progress strip must not intercept map input");
            let camera = view.read(cx).camera;
            window.press("right", cx);
            assert_ne!(view.read(cx).camera, camera, "Progress cannot steal keyboard navigation");
            let camera = view.read(cx).camera;
            window.drag(map_bounds.center(), map_bounds.center() + point(px(40.0), px(20.0)), cx);
            assert_ne!(view.read(cx).camera, camera);
            assert_eq!(window.find("map-surface").focused(), Some(true));
            window.dispatch_action(Box::new(ClearRasters), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("vector-load-progress").is_none());
            assert_eq!(window.find("map-surface").bounds(), map_bounds);
            assert!(!view.read(cx).raster_panel.read(cx).is_loading());
        })
        .unwrap();
        assert!(load.is_closed());
    }

    #[gpui_kit::test]
    fn file_picker_loading_keeps_the_generic_spinner_without_vector_progress(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, handle) = open_test_map(size(px(640.0), px(400.0)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("open-raster", cx);
            assert!(view.read(cx).raster_panel.read(cx).is_loading());
            assert_eq!(window.find("open-raster").disabled(), Some(true));
            assert!(window.try_find("vector-load-progress").is_none());
        })
        .unwrap();
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|_| None);
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(!view.read(cx).raster_panel.read(cx).is_loading());
            assert!(window.try_find("vector-load-progress").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn loading_errors_overlay_the_map_even_with_collapsed_controls(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let directory = tempfile::tempdir().unwrap();
        let mut view = None;
        let handle = cx.open_window(size(px(640.0), px(400.0)), |window, cx| {
            let map = cx.new(|cx| {
                let map = new_test_map(window, cx);
                map.open_paths(vec![directory.path().join("missing.tif")], cx);
                map
            });
            map.read(cx).raster_panel.clone().update(cx, |panel, cx| panel.toggle_controls(window, cx));
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("raster-controls").is_none());
            let banner = window.find("raster-error-banner");
            assert!(banner.visible());
            let inset = window.rem_size() * 0.75;
            assert_eq!(banner.bounds().left(), inset);
            assert_eq!(banner.bounds().right(), px(640.0) - inset);
            assert_eq!(banner.bounds().bottom(), px(400.0) - inset);
            let map_bounds = window.find("map-surface").bounds();
            assert_eq!(map_bounds.top(), window.find("title-bar").bounds().bottom());
            assert_eq!(map_bounds.bottom(), px(400.0));
            assert!(banner.bounds().top() > map_bounds.top());

            view.update(cx, |view, cx| {
                view.apply_frame(
                    MapFrame {
                        viewport: view.viewport.unwrap(),
                        basemap: basemap_style(cx),
                        raster_layer_id: None,
                        vector_layer_id: None,
                        settled: false,
                        tile_boundaries: false,
                        image: Err(anyhow::anyhow!("Network unavailable")),
                    },
                    window,
                    cx,
                );
            });
            window.render_frame(cx);
            let banner = window.find("map-error-banner");
            assert!(banner.visible());
            assert_eq!(window.find("map-surface").bounds(), map_bounds);
            assert!(banner.bounds().top() > map_bounds.top());
            assert_eq!(window.find("raster-error-banner").bounds().top(), banner.bounds().bottom() + inset);
            assert!(window.find("retry-map").visible());

            view.read(cx).raster_panel.clone().update(cx, |panel, cx| panel.clear(window, cx));
            view.update(cx, |view, cx| {
                view.apply_frame(
                    MapFrame {
                        viewport: view.viewport.unwrap(),
                        basemap: basemap_style(cx),
                        raster_layer_id: None,
                        vector_layer_id: None,
                        settled: false,
                        tile_boundaries: false,
                        image: Ok(Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(32, 32))]))),
                    },
                    window,
                    cx,
                );
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("raster-error-banner").is_none());
            assert!(window.try_find("map-error-banner").is_none());
            assert_eq!(window.find("map-surface").bounds().bottom(), px(400.0));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn quit_shortcut_dispatches_globally_from_the_map_and_a_dialog(cx: &mut TestAppContext) {
        let dispatches = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::init_actions(cx).unwrap();
            init(cx).unwrap();
            cx.set_reduce_motion(true);
            // The headless platform's quit is a no-op. Observe dispatch while
            // allowing the production app handler to run.
            let dispatches = dispatches.clone();
            cx.on_action(move |_: &crate::Quit, cx| {
                dispatches.set(dispatches.get() + 1);
                cx.propagate();
            });
        });
        let handle = cx.open_window(size(px(640.), px(400.)), |window, cx| {
            let map = cx.new(|cx| new_test_map(window, cx));
            gpui_kit::base::Root::new(map, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("q", cx);
            assert_eq!(dispatches.get(), 0, "An unmodified Q must not quit");
            window.press("secondary-q", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(dispatches.get(), 1);

        cx.update_window(handle.into(), |_, window, cx| window.press("?", cx)).unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("keyboard-shortcuts").visible());
            window.press("secondary-q", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(dispatches.get(), 2, "The quit shortcut must remain available inside a dialog");
    }

    #[gpui_kit::test]
    fn question_mark_shows_shortcuts_and_dismissal_restores_focus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
            cx.set_reduce_motion(true);
        });
        let (view, handle) = open_test_map(size(px(640.0), px(400.0)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("keyboard-shortcuts").is_none());
            window.press("?", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("keyboard-shortcuts").visible());
            assert!(window.find("shortcut-open").visible());
            assert!(window.find("shortcut-appearance").visible());
            assert_eq!(
                window.find("shortcuts-title").bounds().center().x,
                px(320.0),
                "The dialog title must be centered"
            );
            assert!(window.try_find("shortcuts-done").is_none());
            let close = window.find("close");
            assert!(close.visible());
            assert!(close.bounds().bottom() <= px(400.0), "The close button must fit in a small window");
            let camera = view.read(cx).camera;
            window.press("right", cx);
            assert_eq!(view.read(cx).camera, camera, "The dialog must own keyboard input");
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("keyboard-shortcuts").is_none());
            assert!(view.read(cx).focus.is_focused(window));
            window.press("?", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("close", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("keyboard-shortcuts").is_none());
            assert!(view.read(cx).focus.is_focused(window));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn settings_dialog_is_centered_and_dismissal_restores_map_focus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::appearance::setup_test(None, cx);
            init(cx).unwrap();
        });
        for (viewport, escape, font_size) in [
            (size(px(640.), px(400.)), false, None),
            (size(px(1000.), px(800.)), true, None),
            (size(px(640.), px(400.)), true, Some(px(32.))),
        ] {
            let mut view = None;
            let handle = cx.open_window(viewport, |window, cx| {
                crate::appearance::apply(window, cx);
                if let Some(font_size) = font_size {
                    Theme::update(cx, |theme| theme.font_size = font_size);
                }
                let map = cx.new(|cx| new_test_map(window, cx));
                view = Some(map.clone());
                gpui_kit::base::Root::new(map, window, cx)
            });
            let view = view.unwrap();
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                window.press("secondary-,", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| {
                let bounds = assert_settings_dialog_bounds(viewport, font_size, window, cx);
                let mode_bounds = assert_mode_cards_layout(bounds, viewport, font_size, window);
                let first = assert_theme_grid_layout(&mode_bounds, font_size, window, cx);
                assert_theme_cards_layout(bounds, first, viewport, font_size, window, cx);
                if escape {
                    window.press("escape", cx);
                } else {
                    window.click("settings-done", cx);
                }
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("appearance-settings").is_none());
                assert!(view.read(cx).focus.is_focused(window));
            })
            .unwrap();
        }
    }

    #[gpui_kit::test]
    fn settings_shortcut_changes_appearance_and_escape_restores_map_focus(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("appearance.json");
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::appearance::setup_test(Some(path.clone()), cx);
            init(cx).unwrap();
        });
        let mut view = None;
        let handle = cx.open_window(size(px(1000.), px(800.)), |window, cx| {
            crate::appearance::apply(window, cx);
            let map = cx.new(|cx| new_test_map(window, cx));
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("open-settings").label(), Some("Settings"));
            assert!(window.find("open-settings").visible());
            window.press("secondary-,", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("appearance-settings").visible());
            assert_initial_appearance_modes(window);
            exercise_light_and_dark_modes(&path, window, cx);
            exercise_system_mode(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
        let themes = cx.update(|cx| {
            ThemeRegistry::global(cx)
                .sorted_themes()
                .into_iter()
                .map(|theme| theme.name.clone())
                .collect::<Vec<_>>()
        });
        let selected_cards = select_and_check_theme_cards(&path, &themes, handle.into(), cx);
        cx.update_window(handle.into(), |_, window, cx| window.press("escape", cx)).unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("appearance-settings").is_none());
            assert!(view.read(cx).focus.is_focused(window));
            window.press("secondary-,", cx);
        })
        .unwrap();
        cx.run_until_parked();
        check_inactive_system_theme(&path, &themes, &selected_cards, handle.into(), cx);
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("appearance-settings").is_none());
            assert!(view.read(cx).focus.is_focused(window));
            let before = view.read(cx).camera;
            window.press("+", cx);
            assert_ne!(view.read(cx).camera, before);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn title_bar_layer_selector_fits_loaded_names_and_caps_width_without_hiding_actions(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::appearance::setup_test(None, cx);
            init(cx).unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let short = directory.path().join("dem.tif");
        let long = directory.path().join("Regional elevation observations for winter.tif");
        let oversized = directory.path().join(format!("{}.tif", "regional_".repeat(20)));
        for path in [&short, &long, &oversized] {
            crate::raster_source::test_support::write_raster(path, 4326, 1).unwrap();
        }
        let (view, handle) = open_test_map(size(px(1200.), px(760.)), cx);
        let placeholder_width = cx.update_window(handle.into(), |_, window, cx| measure_empty_selector(window, cx)).unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![short]));
        cx.run_until_parked();
        let short_width = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let width = window.find("raster-band").bounds().size.width;
                assert!(width < placeholder_width, "A short loaded name must not retain the placeholder's width");
                assert!(width < window.rem_size() * 16., "Short names must not retain the old w_64 width");
                window.click("open-raster", cx);
                width
            })
            .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![long]));
        cx.run_until_parked();
        let long_width = cx
            .update_window(handle.into(), |_, window, cx| {
                measure_long_selector(&view, placeholder_width, short_width, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        check_selector_input_keeps_width(&view, long_width, handle.into(), cx);
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.find("raster-band").bounds().size.width > long_width,
                "Selector measurement must follow font zoom"
            );
            window.click("open-raster", cx);
        })
        .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![oversized]));
        cx.run_until_parked();
        let wide_capped_width = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let width = window.find("raster-band").bounds().size.width;
                assert!(width <= window.viewport_size().width * 0.5, "Very long names must leave title-bar space");
                width
            })
            .unwrap();
        check_capped_selector_actions(&view, wide_capped_width, handle.into(), cx);
    }

    #[gpui_kit::test]
    fn band_selection_preserves_the_camera_and_explicit_fit_restores_the_extent(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bands.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let (view, handle) = open_test_map(size(px(1200.0), px(760.0)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("open-raster", cx);
        })
        .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![path]));
        cx.run_until_parked();
        let (fitted, navigated, original_band) = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let fitted = view.read(cx).camera;
                assert_ne!(fitted, MapCamera::default(), "Opening a raster must still fit its extent");
                let focus = view.read(cx).focus.clone();
                window.focus(&focus, cx);
                window.press("right", cx);
                window.press("+", cx);
                let navigated = view.read(cx).camera;
                assert_ne!(navigated, fitted);
                let band = view.read(cx).raster.as_ref().unwrap().metadata().id;
                assert_eq!(view.read(cx).raster.as_ref().unwrap().metadata().band_nr, Some(1));
                window.click("raster-band", cx);
                window.press("down", cx);
                window.press("enter", cx);
                (fitted, navigated, band)
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_ne!(view.read(cx).raster.as_ref().unwrap().metadata().id, original_band);
            assert_eq!(view.read(cx).camera, navigated, "Changing bands must preserve pan and zoom");
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(view.read(cx).raster.as_ref().unwrap().metadata().id, original_band);
            assert_eq!(view.read(cx).camera, navigated, "Scrolling the selector must not zoom the map");
            window.press("secondary-0", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(view.read(cx).camera, fitted, "The explicit fit command must still fit the active band"));
    }

    #[gpui_kit::test]
    fn vector_pointer_preserves_pending_properties_invalidates_context_and_switches_to_raster(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let vector_path = directory.path().join("hover.geojson");
        let feature = serde_json::json!({
            "type": "Feature",
            "id": 7,
            "properties": {"name": "Main street", "open": true, "lanes": 2},
            "geometry": {"type": "LineString", "coordinates": [[4, 50], [6, 52]]}
        });
        std::fs::write(
            &vector_path,
            serde_json::json!({"type": "FeatureCollection", "features": [feature.clone()]}).to_string(),
        )
        .unwrap();
        let raster_path = directory.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&raster_path, 4326, 2).unwrap();
        let (view, handle) = open_test_map(size(px(1200.), px(760.)), cx);
        view.update(cx, |map, _| map._feature_task = None);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("feature-info").is_none(), "No dataset must not show vector properties");
            view.update(cx, |map, cx| map.open_paths(vec![vector_path.clone()], cx));
        })
        .unwrap();
        cx.run_until_parked();
        let (original_layer, revision) = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                assert_empty_vector_inspector(&view, window, cx);
                let (layer, revision) = query_center_feature(&view, &feature, window, cx);
                let bounds = view.read(cx).bounds.unwrap();
                window.render_frame(cx);
                let properties = assert_vector_inspector_layout(bounds, window);

                check_zoom_retains_properties(&view, &feature, properties, window, cx);

                check_pointer_queries_retain_properties(&view, &feature, revision, window, cx);
                check_empty_and_collapsed_properties(&view, &feature, window, cx);
                check_pan_query_invalidation(&view, &feature, window, cx);
                let revision = view.read(cx).hover_revision;
                view.update(cx, |map, cx| map.open_paths(vec![vector_path], cx));
                (layer, revision)
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            check_layer_switch_rejects_features(&view, &feature, original_layer, revision, window, cx);
            leave_vector_for_raster(&view, raster_path, window, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(view.read(cx).vector.is_none());
            assert!(view.read(cx).raster.is_some());
            window.hover("map-surface", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(view.read(cx).vector_hover.is_none());
            assert!(window.try_find("feature-info-properties").is_none());
            assert!(window.try_find("feature-info").is_none(), "Raster datasets must not show vector properties");
            let info = window.find("pixel-info");
            let text = info.label().unwrap();
            assert!(text.contains("Lat: 0.000000") && text.contains("Lon: 0.000000"), "{text}");
            let value = text.split_whitespace().nth(4).unwrap().parse::<f64>().unwrap();
            assert!((value - 74.0).abs() <= 1.0, "Expected the first band's center value, got {value}");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local vector data"]
    fn vector_properties_refresh_after_zoom_with_a_stationary_pointer(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            cx.set_reduce_motion(true);
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stationary.geojson");
        std::fs::write(
            &path,
            serde_json::json!({
                "type": "FeatureCollection",
                "features": [{
                    "type": "Feature", "id": 7, "properties": {"name": "Area"},
                    "geometry": {"type": "Polygon", "coordinates": [[[-10, -10], [10, -10], [10, 10], [-10, 10], [-10, -10]]]}
                }]
            })
            .to_string(),
        )
        .unwrap();
        let renderer = MapRenderer::with_test_style(
            r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##,
        )
        .unwrap();
        let frames = renderer.frames();
        let results = renderer.feature_results();
        let mut view = None;
        let handle = cx.open_window(size(px(1200.0), px(760.0)), |window, cx| {
            let map = cx.new(|cx| {
                let mut map = MapView::with_renderer(None, window, cx);
                // Pump worker channels on the deterministic test thread instead
                // of registering listeners that accept foreign-thread wakeups.
                map.renderer = Some(renderer);
                map
            });
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |map, cx| map.open_paths(vec![path], cx));
        })
        .unwrap();
        cx.run_until_parked();
        let position = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let position = window.find("map-surface").bounds().center();
                window.hover("map-surface", cx);
                let focus = view.read(cx).focus.clone();
                window.focus(&focus, cx);
                position
            })
            .unwrap();
        exercise_stationary_zoom(&view, &frames, &results, position, handle.into(), cx);
        view.update(cx, |map, _| map.renderer = None);
    }

    #[gpui_kit::test]
    fn vector_properties_scroll_in_short_windows_without_dismissing_the_hover_or_zooming(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
            gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(18.0));
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("properties.geojson");
        let properties: serde_json::Map<String, serde_json::Value> =
            (0..64).map(|ix| (format!("field_{ix:02}"), serde_json::json!(format!("Value {ix}")))).collect();
        let feature = serde_json::json!({
            "type": "Feature", "id": 1, "properties": properties,
            "geometry": {"type": "Point", "coordinates": [4, 51]}
        });
        std::fs::write(
            &path,
            serde_json::json!({"type": "FeatureCollection", "features": [feature.clone()]}).to_string(),
        )
        .unwrap();
        let (view, handle) = open_test_map(size(px(640.0), px(400.0)), cx);
        view.update(cx, |map, _| map._feature_task = None);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |map, cx| map.open_paths(vec![path], cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            show_many_properties(&view, feature, window, cx);
            let camera = view.read(cx).camera;
            window.hover("feature-info-heading", cx);
            assert!(view.read(cx).vector_hover.is_some());
            assert!(window.find("feature-info-properties").visible());
            let layout = assert_short_properties_layout(window);
            let scroll_extent = check_property_wheel_scroll(&view, &layout, camera, window, cx);

            check_property_scroll_limits(&view, &layout, camera, scroll_extent, window, cx);

            check_property_collapse(&view, layout.properties, camera, window, cx);
            check_property_keyboard_toggle(&view, layout.properties, camera, window, cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn mouse_move_reads_the_active_band_and_hides_info_over_controls(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let (view, handle) = open_test_map(size(px(1200.0), px(760.0)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.hover("map-surface", cx);
            let info = window.find("pixel-info");
            assert!(info.label().unwrap().contains("Lat:"));
            assert_eq!(info.label().unwrap().split_whitespace().count(), 4);
            assert!(
                info.bounds().right() < window.find("map-surface").bounds().right(),
                "The readout should be compact, not a full-width status bar"
            );
            window.click("open-raster", cx);
        })
        .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![path]));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.hover("map-surface", cx)).unwrap();
        cx.run_until_parked();
        let first_value = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let info = window.find("pixel-info");
                let text = info.label().unwrap();
                assert!(text.contains("Lat: 0.000000") && text.contains("Lon: 0.000000"), "{text}");
                let value: f64 = text.split_whitespace().nth(4).unwrap().parse().unwrap();
                assert!((value - 74.0).abs() <= 1.0, "Expected the first band's center value, got {value}");
                window.hover("raster-layers-card", cx);
                assert!(window.find("pixel-info").label().is_none(), "Controls must not sample the map underneath");
                window.click("raster-band", cx);
                window.press("down", cx);
                window.press("enter", cx);
                value
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.hover("map-surface", cx)).unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let info = window.find("pixel-info");
            let value: f64 = info.label().unwrap().split_whitespace().nth(4).unwrap().parse().unwrap();
            assert_eq!(value, first_value + 10.0, "Band changes must invalidate the cached numeric tile");
            assert_cached_pointer_value(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("pixel-info").label().is_none(), "Late reads must not restore a hidden readout");
            view.read(cx).raster_panel.clone().update(cx, |panel, cx| panel.clear(window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.hover("map-surface", cx)).unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("pixel-info").label().unwrap().split_whitespace().count(), 4);
            assert!(view.read(cx).raster.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn map_gestures_pass_through_below_floating_controls(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
        let (view, handle) = open_test_map(size(px(1200.0), px(760.0)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("open-raster", cx);
        })
        .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![path]));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let map = window.find("map-surface").bounds();
            let range = window.find("raster-range-card").bounds();
            let before = view.read(cx).camera;
            window.scroll("raster-layers-card", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            let layers = window.find("raster-layers-card").bounds();
            let from = layers.origin + point(px(20.0), px(20.0));
            window.drag(from, from + point(px(20.0), px(20.0)), cx);
            assert_eq!(view.read(cx).camera, before, "The cards must own their input");
            let below = point(range.center().x, (range.bottom() + map.bottom()) * 0.5);
            assert!(below.y > window.find("raster-controls").bounds().bottom());
            window.dispatch_event(
                ScrollEvent {
                    position: below,
                    delta: ScrollDelta::Lines(point(0.0, 1.0)),
                    modifiers: Default::default(),
                    touch_phase: Default::default(),
                    source: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            assert_ne!(view.read(cx).camera, before, "Scrolling below the cards must zoom the map");
            let before_drag = view.read(cx).camera;
            window.drag(below, below + point(px(-40.0), px(-20.0)), cx);
            assert_ne!(view.read(cx).camera, before_drag, "Dragging below the cards must pan the map");

            window.click("toggle-raster-controls", cx);
            let layers = window.find("raster-layers-card").bounds();
            let before = view.read(cx).camera;
            let below = point(layers.center().x, (layers.bottom() + map.bottom()) * 0.5);
            window.drag(below, below + point(px(-40.0), px(-20.0)), cx);
            assert_ne!(view.read(cx).camera, before, "Collapsed controls must not block the map below them");
            window.click("toggle-raster-controls", cx);
        })
        .unwrap();
        cx.simulate_window_resize(handle.into(), size(px(640.0), px(400.0)));
        cx.update(|cx| gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(18.0)));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let before = view.read(cx).camera;
            window.scroll("raster-layers-card", ScrollDelta::Lines(point(0.0, -30.0)), cx);
            assert!(window.find("raster-max").visible(), "Controls must remain scrollable in short windows");
            assert_eq!(view.read(cx).camera, before, "Scrolling the controls must not zoom the map");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn floating_controls_preserve_the_full_map_and_own_their_input(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
        let (view, handle) = open_test_map(size(px(1200.0), px(760.0)), cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("open-raster", cx);
        })
        .unwrap();
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|options| {
            assert!(options.files && options.multiple && !options.directories);
            None
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(!view.read(cx).raster_panel.read(cx).is_loading());
            assert!(view.read(cx).raster.is_none());
            assert!(window.try_find("raster-controls").is_none());
            window.within("title-bar").click("open-raster", cx);
            assert!(view.read(cx).drag.is_none(), "Opening a raster must not start a map gesture");
        })
        .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![path]));
        cx.run_until_parked();
        assert!(
            cx.update(|cx| view.read(cx).raster.is_some()),
            "Opening through the title bar must select the raster"
        );

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let layers = assert_floating_controls_layout(window);

            let before = view.read(cx).camera;
            window.click("title-bar", cx);
            assert_eq!(view.read(cx).camera, before, "Title bar input must not pan the map");
            assert!(view.read(cx).drag.is_none());
            window.scroll("raster-layers-card", ScrollDelta::Lines(point(0.0, 2.0)), cx);
            let from = point(layers.left() + px(20.0), layers.top() + px(20.0));
            window.drag(from, from + point(px(40.0), px(20.0)), cx);
            assert_eq!(view.read(cx).camera, before, "Panel gestures must not pan or zoom the map");
            assert!(view.read(cx).drag.is_none());

            assert!(window.try_find("raster-opacity-input").is_none());
            let opacity = window.find("raster-opacity").bounds();
            window.click_at("raster-opacity", point(opacity.size.width * 0.5, opacity.size.height * 0.5), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(
            (cx.update(|cx| view.read(cx).opacity) - 0.5).abs() < 0.001,
            "The opacity slider must update the raster"
        );

        exercise_range_slider(&view, handle.into(), cx);

        check_raster_controls_toggle(handle.into(), cx);

        check_floating_controls_resize(&view, handle.into(), cx);
    }

    #[gpui_kit::test]
    fn range_drag_displays_in_flight_frames_before_the_pointer_stops(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let mut view = None;
        let handle = cx.open_window(size(px(900.0), px(760.0)), |window, cx| {
            let map = cx.new(|cx| {
                let map = new_test_map(window, cx);
                map.open_paths(vec![path], cx);
                map
            });
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        start_range_slider_drag(handle.into(), cx);
        cx.run_until_parked();

        check_in_flight_range_frames(&view, handle.into(), cx);

        check_obsolete_band_frame(&view, handle.into(), cx);
    }

    #[gpui_kit::test]
    fn replacing_map_frames_evicts_obsolete_texture_cache_entries(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, handle) = open_test_map(size(px(800.0), px(600.0)), cx);
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();

        let last = cx
            .update_window(handle.into(), |_, window, cx| {
                let viewport = view.read(cx).viewport.unwrap();
                let mut previous: Option<Arc<RenderImage>> = None;
                for _ in 0..100 {
                    let image = Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(32, 32))]));
                    view.update(cx, |view, cx| {
                        view.apply_frame(
                            MapFrame {
                                viewport,
                                basemap: basemap_style(cx),
                                raster_layer_id: None,
                                vector_layer_id: None,
                                settled: false,
                                tile_boundaries: false,
                                image: Ok(image.clone()),
                            },
                            window,
                            cx,
                        );
                    });
                    window.render_frame(cx);
                    assert!(window.has_image_atlas_entry(&image), "Current map frame must be uploaded");
                    if let Some(previous) = previous {
                        assert!(!window.has_image_atlas_entry(&previous), "Replaced map frames must not remain cached");
                    }
                    previous = Some(image);
                }
                previous.unwrap()
            })
            .unwrap();

        cx.update_window(handle.into(), |_, window, cx| {
            window.replace_root(cx, new_test_map);
            window.render_frame(cx);
        })
        .unwrap();
        let released = view.downgrade();
        drop(view);
        cx.update(|_| {});
        cx.run_until_parked();
        assert!(released.upgrade().is_none(), "The old map view must be released");
        cx.update_window(handle.into(), |_, window, _| {
            assert!(!window.has_image_atlas_entry(&last), "Releasing the map must evict its last frame");
        })
        .unwrap();
    }

    #[test]
    fn release_preserves_recent_drag_velocity() {
        let now = Instant::now();
        let mut drag = MapDrag::new(point(px(0.0), px(0.0)), now);
        drag.record(point(px(40.0), px(-20.0)), now + Duration::from_millis(50));
        let coast = drag.release(now + Duration::from_millis(60)).unwrap();
        assert_eq!(coast.velocity, (800.0, -400.0));
    }

    #[test]
    fn release_after_a_pause_does_not_coast() {
        let now = Instant::now();
        let mut drag = MapDrag::new(point(px(0.0), px(0.0)), now);
        drag.record(point(px(100.0), px(0.0)), now + Duration::from_millis(50));
        assert!(drag.release(now + Duration::from_millis(200)).is_none());
    }

    #[test]
    fn click_without_movement_does_not_coast() {
        let now = Instant::now();
        let drag = MapDrag::new(point(px(0.0), px(0.0)), now);
        assert!(drag.release(now + Duration::from_millis(10)).is_none());
    }

    #[test]
    fn slow_drag_does_not_coast() {
        let now = Instant::now();
        let mut drag = MapDrag::new(point(px(0.0), px(0.0)), now);
        drag.record(point(px(1.0), px(0.0)), now + Duration::from_millis(50));
        assert!(drag.release(now + Duration::from_millis(50)).is_none());
    }

    #[test]
    fn fast_release_caps_coasting_speed() {
        let now = Instant::now();
        let mut drag = MapDrag::new(point(px(0.0), px(0.0)), now);
        drag.record(point(px(300.0), px(400.0)), now + Duration::from_millis(1));
        let coast = drag.release(now + Duration::from_millis(1)).unwrap();
        assert_eq!(coast.velocity.0.hypot(coast.velocity.1), MAX_COAST_SPEED);
    }

    #[test]
    fn coast_slows_and_stops_without_reversing() {
        let now = Instant::now();
        let mut coast = MapCoast {
            velocity: (1_000.0, 0.0),
            sampled_at: now,
        };
        let first = coast.advance(now + Duration::from_millis(100));
        let second = coast.advance(now + Duration::from_millis(200));
        assert!(first.0 > second.0 && second.0 > 0.0);
        coast.advance(now + Duration::from_secs(1));
        assert_eq!(coast.velocity, (0.0, 0.0));
        assert_eq!(coast.advance(now + Duration::from_secs(2)), (0.0, 0.0));
    }

    #[test]
    fn coast_distance_is_independent_of_frame_rate() {
        let now = Instant::now();
        let mut single = MapCoast {
            velocity: (1_000.0, -500.0),
            sampled_at: now,
        };
        let mut multiple = MapCoast {
            velocity: single.velocity,
            sampled_at: now,
        };
        let expected = single.advance(now + Duration::from_secs(1));
        let mut actual = (0.0, 0.0);
        for frame in 1..=100 {
            let delta = multiple.advance(now + Duration::from_millis(frame * 10));
            actual.0 += delta.0;
            actual.1 += delta.1;
        }
        assert!((actual.0 - expected.0).abs() < 1e-9 && (actual.1 - expected.1).abs() < 1e-9);
    }

    #[gpui_kit::test]
    fn mouse_navigation_updates_the_camera_and_ends_drag_outside_map(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
        });
        let mut view = None;
        let handle = cx.open_window(size(px(800.0), px(600.0)), |window, cx| {
            let map = cx.new(|cx| new_test_map(window, cx));
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.scroll("map-surface", ScrollDelta::Lines(point(0.0, 2.0)), cx);
            assert_ne!(view.read(cx).camera, MapCamera::default());
            let after_zoom = view.read(cx).camera;
            let bounds = window.find("map-surface").bounds();
            let from = bounds.center();
            let to = from + point(px(100.0), px(50.0));
            window.drag(from, to, cx);
            assert_ne!(view.read(cx).camera, after_zoom);
            assert!(view.read(cx).drag.is_none());
            assert!(view.read(cx).coast.is_some());
            let at_release = view.read(cx).camera;
            view.update(cx, |view, _| {
                view.coast.as_mut().unwrap().sampled_at -= Duration::from_millis(16);
            });
            window.simulate_next_frame(cx);
            assert_ne!(view.read(cx).camera, at_release);

            check_coast_interruptions(&view, from, to, bounds, window, cx);
        })
        .unwrap();
    }
    #[gpui_kit::test]
    fn ui_fixture_has_no_renderer_or_worker_listeners(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, _) = open_test_map(size(px(800.0), px(600.0)), cx);
        cx.update(|cx| {
            let map = view.read(cx);
            assert!(map.renderer.is_none());
            assert!(map._frames_task.is_none());
            assert!(map._feature_task.is_none());
        });
        cx.run_until_parked();
    }

    fn new_test_map(window: &mut Window, cx: &mut Context<MapView>) -> MapView {
        MapView::with_renderer(None, window, cx)
    }

    fn open_test_map(viewport: gpui_kit::Size<Pixels>, cx: &mut TestAppContext) -> (Entity<MapView>, gpui_kit::WindowHandle<gpui_kit::base::Root>) {
        let mut view = None;
        let handle = cx.open_window(viewport, |window, cx| {
            let map = cx.new(|cx| new_test_map(window, cx));
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        (view.unwrap(), handle)
    }

    fn assert_preview_layout(card: Bounds<Pixels>, preview: Bounds<Pixels>, label: Bounds<Pixels>, context: &str) {
        for (part, bounds) in [("preview", preview), ("label", label)] {
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "{context}: {part} must have nonzero geometry, got {bounds:?}"
            );
            assert!(
                bounds.left() >= card.left() && bounds.right() <= card.right() && bounds.top() >= card.top() && bounds.bottom() <= card.bottom(),
                "{context}: {part} {bounds:?} must be contained in card {card:?}"
            );
        }
        assert!(
            preview.bottom() <= label.top(),
            "{context}: preview {preview:?} must stack above label {label:?} without overlap"
        );
    }

    fn assert_nonoverlapping(bounds: Bounds<Pixels>, previous: &[Bounds<Pixels>], context: &str) {
        for previous in previous {
            assert!(
                bounds.left() >= previous.right()
                    || bounds.right() <= previous.left()
                    || bounds.top() >= previous.bottom()
                    || bounds.bottom() <= previous.top(),
                "{context}"
            );
        }
    }

    fn assert_settings_dialog_bounds(viewport: gpui_kit::Size<Pixels>, font_size: Option<Pixels>, window: &mut Window, cx: &mut App) -> Bounds<Pixels> {
        window.render_frame(cx);
        let bounds = window.within("dialog").find("surface").bounds();
        assert!((bounds.center().x - viewport.width / 2.).abs() <= px(1.));
        assert!((bounds.center().y - viewport.height / 2.).abs() <= px(1.));
        assert!(bounds.left() >= px(0.) && bounds.right() <= viewport.width);
        assert!(bounds.top() >= px(0.) && bounds.bottom() <= viewport.height);
        assert!(window.find("appearance-settings").visible());
        let done = window.find("settings-done");
        assert!(done.visible());
        assert!(done.bounds().left() >= bounds.left() && done.bounds().right() <= bounds.right());
        assert!(done.bounds().top() >= bounds.top() && done.bounds().bottom() <= bounds.bottom());
        if let Some(font_size) = font_size {
            assert_eq!(cx.theme().font_size, font_size);
            assert_eq!(window.rem_size(), font_size, "The zoom case must exercise Root's rem scaling");
        }

        bounds
    }

    fn assert_mode_cards_layout(
        bounds: Bounds<Pixels>,
        viewport: gpui_kit::Size<Pixels>,
        font_size: Option<Pixels>,
        window: &mut Window,
    ) -> Vec<Bounds<Pixels>> {
        let layout = format!("viewport {viewport:?}, font {:?}", window.rem_size());
        let mut mode_bounds = Vec::<Bounds<Pixels>>::new();
        for id in ["mode-light", "mode-dark", "mode-system"] {
            let mode = window.find(id).bounds();
            assert!(
                mode.size.width > px(0.) && mode.size.height > px(0.),
                "Mode card {id} must have nonzero geometry"
            );
            assert!(mode.left() >= bounds.left() && mode.right() <= bounds.right());
            let scope = window.within(id);
            assert_preview_layout(
                mode,
                scope.find("mode-preview").bounds(),
                scope.find("mode-label").bounds(),
                &format!("Mode {id}, {layout}"),
            );
            assert_nonoverlapping(
                mode,
                &mode_bounds,
                &format!("Mode cards must wrap without overlapping at {viewport:?}, font {font_size:?}"),
            );
            mode_bounds.push(mode);
        }

        mode_bounds
    }

    fn assert_theme_grid_layout(mode_bounds: &[Bounds<Pixels>], font_size: Option<Pixels>, window: &mut Window, cx: &App) -> Bounds<Pixels> {
        let themes = gpui_kit::component::ThemeRegistry::global(cx).sorted_themes();
        let light_themes = themes.iter().filter(|theme| !theme.mode.is_dark()).collect::<Vec<_>>();
        let first = window.find((ElementId::from("theme"), light_themes[0].name.clone())).bounds();
        let second = window.find((ElementId::from("theme"), light_themes[1].name.clone())).bounds();
        let last_light = light_themes
            .iter()
            .map(|theme| window.find((ElementId::from("theme"), theme.name.clone())).bounds().bottom())
            .fold(first.bottom(), |bottom, next| bottom.max(next));
        let first_dark = themes.iter().find(|theme| theme.mode.is_dark()).unwrap();
        let first_dark = window.find((ElementId::from("theme"), first_dark.name.clone())).bounds();
        assert!(first_dark.top() > last_light, "Dark themes must have their own section below all light themes");
        for mode in mode_bounds {
            assert!(first.top() >= mode.bottom(), "The theme grid must remain below the mode choices after wrapping");
        }
        if font_size.is_some() {
            assert!(second.top() >= first.bottom(), "At high zoom the narrow grid must reflow to a single column");
            assert!((first.left() - second.left()).abs() <= px(1.));
            assert!((first.right() - second.right()).abs() <= px(1.));
        } else {
            assert!(
                (first.top() - second.top()).abs() <= px(1.),
                "At normal zoom theme cards should share a grid row"
            );
            assert!(first.right() <= second.left(), "Adjacent theme cards must not overlap");
        }

        first
    }

    fn assert_theme_cards_layout(
        bounds: Bounds<Pixels>,
        first: Bounds<Pixels>,
        viewport: gpui_kit::Size<Pixels>,
        font_size: Option<Pixels>,
        window: &mut Window,
        cx: &App,
    ) {
        let layout = format!("viewport {viewport:?}, font {:?}", window.rem_size());
        let mut card_bounds = Vec::<Bounds<Pixels>>::new();
        for theme in gpui_kit::component::ThemeRegistry::global(cx).sorted_themes() {
            let card = window.find((ElementId::from("theme"), theme.name.clone()));
            let label = format!("{}, {} theme", theme.name, if theme.mode.is_dark() { "Dark" } else { "Light" });
            assert_eq!(card.label(), Some(label.as_str()));
            assert_eq!(card.selected(), Some(&theme.name == cx.theme().theme_name()));
            let (section, other_section) = if theme.mode.is_dark() {
                ("dark-themes", "light-themes")
            } else {
                ("light-themes", "dark-themes")
            };
            let id = (ElementId::from("theme"), theme.name.clone());
            assert!(
                window.within(section).try_find(id.clone()).is_some(),
                "Each section must contain only its matching themes"
            );
            assert!(window.within(other_section).try_find(id).is_none());
            let card = card.bounds();
            assert!(
                card.size.width > px(0.) && card.size.height > px(0.),
                "Theme {} must have nonzero geometry",
                theme.name
            );
            assert!(card.left() >= bounds.left() && card.right() <= bounds.right());
            assert!((card.size.width - first.size.width).abs() <= px(1.));
            assert!((card.size.height - first.size.height).abs() <= px(1.));
            let scope = window.within((ElementId::from("theme"), theme.name.clone()));
            assert_preview_layout(
                card,
                scope.find("theme-swatch").bounds(),
                scope.find("theme-label").bounds(),
                &format!("Theme {}, {layout}", theme.name),
            );
            assert_nonoverlapping(card, &card_bounds, &format!("Theme cards must not overlap at {viewport:?}, font {font_size:?}"));
            card_bounds.push(card);
        }
    }

    fn assert_initial_appearance_modes(window: &mut Window) {
        for (id, label, selected) in [
            ("mode-light", "Light", false),
            ("mode-dark", "Dark", false),
            ("mode-system", "Follow system", true),
        ] {
            let mode = window.find(id);
            assert_eq!(mode.label(), Some(label));
            assert_eq!(mode.selected(), Some(selected));
        }
    }

    fn exercise_light_and_dark_modes(path: &std::path::Path, window: &mut Window, cx: &mut App) {
        window.click("mode-light", cx);
        assert_eq!(Appearance::mode(cx), AppearanceMode::Light);
        assert_eq!(cx.theme().mode, ThemeMode::Light);
        assert_eq!(window.find("mode-light").selected(), Some(true));
        window.press("tab", cx);
        assert_eq!(window.find("mode-dark").focused(), Some(true));
        assert_eq!(window.find("mode-dark").selected(), Some(false), "Focusing a mode must not select it");
        window.press("space", cx);
        assert_eq!(Appearance::mode(cx), AppearanceMode::Dark);
        assert_eq!(cx.theme().mode, ThemeMode::Dark);
        assert_eq!(window.find("mode-dark").selected(), Some(true));
        assert_eq!(window.find("mode-light").selected(), Some(false));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap()["mode"],
            "dark"
        );
    }

    fn exercise_system_mode(window: &mut Window, cx: &mut App) {
        window.press("tab", cx);
        assert_eq!(window.find("mode-system").focused(), Some(true));
        window.press("enter", cx);
        assert_eq!(Appearance::mode(cx), AppearanceMode::System);
        assert_eq!(cx.theme().mode, ThemeMode::from(window.appearance()));
        assert_eq!(window.find("mode-system").selected(), Some(true));
        window.click("mode-dark", cx);
    }

    fn reveal_theme(name: &SharedString, themes: &[SharedString], window: &mut Window, cx: &mut App) {
        let id = (ElementId::from("theme"), name.clone());
        for _ in 0..100 {
            let card = window.find(id.clone());
            let dialog = window.within("dialog").find("surface").bounds();
            let top = dialog.top() + px(80.);
            let bottom = window.find("settings-done").bounds().top() - px(8.);
            if card.visible() && card.bounds().top() >= top && card.bounds().bottom() <= bottom {
                return;
            }
            // Wheel over a visible card so native hit testing reaches the
            // scrolling body, even when the requested card is clipped.
            let anchor = themes
                .iter()
                .find(|name| {
                    let card = window.find((ElementId::from("theme"), (*name).clone()));
                    card.visible() && card.bounds().center().y > top && card.bounds().center().y < bottom
                })
                .expect("The scrolling body must expose a theme card");
            let delta = if card.bounds().center().y < dialog.center().y { 80. } else { -80. };
            window.scroll((ElementId::from("theme"), anchor.clone()), ScrollDelta::Pixels(point(px(0.), px(delta))), cx);
        }
        panic!("Theme card {name} did not scroll into view");
    }

    fn select_and_check_theme_cards(
        path: &std::path::Path,
        themes: &[SharedString],
        handle: gpui_kit::AnyWindowHandle,
        cx: &mut TestAppContext,
    ) -> Vec<SharedString> {
        let mut selected_cards = Vec::<SharedString>::new();
        for (mode, appearance_mode, slot, saved_mode) in [
            (ThemeMode::Light, AppearanceMode::Light, "light_theme", "light"),
            (ThemeMode::Dark, AppearanceMode::Dark, "dark_theme", "dark"),
        ] {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                let name = ThemeRegistry::global(cx)
                    .sorted_themes()
                    .into_iter()
                    .find(|theme| theme.mode == mode && theme.name.as_str() != Appearance::selected_theme(mode, cx))
                    .unwrap()
                    .name
                    .clone();
                reveal_theme(&name, themes, window, cx);
                let id = (ElementId::from("theme"), name.clone());
                assert_eq!(window.find(id.clone()).selected(), Some(false));
                window.click(id.clone(), cx);
                assert_eq!(
                    Appearance::mode(cx),
                    appearance_mode,
                    "An explicit appearance must follow the clicked card's mode"
                );
                assert_eq!(cx.theme().mode, mode);
                assert_eq!(cx.theme().theme_name(), &name);
                assert_eq!(Appearance::selected_theme(mode, cx), name.as_str());
                assert_eq!(window.find(id).selected(), Some(true));
                assert_other_themes_unselected(&selected_cards, window);
                let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                assert_eq!(saved["mode"], saved_mode);
                assert_eq!(saved[slot], name.as_str());
                selected_cards.push(name);
            })
            .unwrap();
            cx.run_until_parked();
        }

        selected_cards
    }

    fn assert_other_themes_unselected(names: &[SharedString], window: &mut Window) {
        for name in names {
            assert_eq!(window.find((ElementId::from("theme"), name.clone())).selected(), Some(false));
        }
    }

    fn focus_inactive_system_theme(themes: &[SharedString], window: &mut Window, cx: &mut App) -> (Theme, ThemeMode, SharedString) {
        window.render_frame(cx);
        window.click("mode-system", cx);
        let before = cx.theme().clone();
        let inactive = if before.mode.is_dark() { ThemeMode::Light } else { ThemeMode::Dark };
        let name = ThemeRegistry::global(cx)
            .sorted_themes()
            .into_iter()
            .find(|theme| theme.mode == inactive && theme.name.as_str() != Appearance::selected_theme(inactive, cx))
            .unwrap()
            .name
            .clone();
        reveal_theme(&name, themes, window, cx);
        let id = (ElementId::from("theme"), name.clone());
        assert_eq!(window.find(id.clone()).selected(), Some(false));
        // Reach the card without clicking it: Enter must activate the real Radio.
        for _ in 0..themes.len() + 8 {
            if window.find(id.clone()).focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find(id.clone()).focused(), Some(true));

        (before, inactive, name)
    }

    fn assert_saved_inactive_theme(inactive: ThemeMode, selected_cards: &[SharedString], cx: &App) {
        assert_eq!(
            Appearance::selected_theme(inactive, cx),
            if inactive.is_dark() {
                selected_cards[1].as_str()
            } else {
                selected_cards[0].as_str()
            }
        );
    }

    fn assert_inactive_theme_preserves_palette(before: &Theme, inactive: ThemeMode, name: &SharedString, window: &mut Window, cx: &App) {
        assert_eq!(Appearance::mode(cx), AppearanceMode::System);
        assert_eq!(Appearance::selected_theme(inactive, cx), name.as_str());
        assert_eq!(cx.theme().mode, before.mode);
        assert_eq!(cx.theme().theme_name(), before.theme_name());
        assert_eq!(cx.theme().colors, before.colors, "An inactive System-mode choice must not change the palette");
        assert_eq!(cx.theme().tokens, before.tokens);
        assert_eq!(
            window.find((ElementId::from("theme"), name.clone())).selected(),
            Some(false),
            "Only the active theme card is selected, not both saved slots"
        );
        let active = SharedString::from(cx.theme().theme_name().to_string());
        assert_eq!(window.find((ElementId::from("theme"), active)).selected(), Some(true));
        assert_eq!(window.find("mode-system").selected(), Some(true));
    }

    fn assert_system_theme_persisted(path: &std::path::Path, inactive: ThemeMode, name: &SharedString, before: &Theme, cx: &App) {
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["mode"], "system");
        assert_eq!(saved[if inactive.is_dark() { "dark_theme" } else { "light_theme" }], name.as_str());
        assert_eq!(
            saved[if inactive.is_dark() { "light_theme" } else { "dark_theme" }],
            Appearance::selected_theme(before.mode, cx)
        );
    }

    fn check_inactive_system_theme(
        path: &std::path::Path,
        themes: &[SharedString],
        selected_cards: &[SharedString],
        handle: gpui_kit::AnyWindowHandle,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(handle, |_, window, cx| {
            let (before, inactive, name) = focus_inactive_system_theme(themes, window, cx);
            assert_saved_inactive_theme(inactive, selected_cards, cx);
            window.press("enter", cx);
            assert_inactive_theme_preserves_palette(&before, inactive, &name, window, cx);
            assert_system_theme_persisted(path, inactive, &name, &before, cx);
            reveal_theme(&name, themes, window, cx);
            window.click((ElementId::from("theme"), name), cx);
            assert_eq!(Appearance::mode(cx), AppearanceMode::System);
            assert_eq!(
                cx.theme().colors,
                before.colors,
                "Clicking an inactive System-mode card must also preserve the palette"
            );
            window.press("escape", cx);
        })
        .unwrap();
    }

    fn assert_empty_vector_inspector(view: &Entity<MapView>, window: &mut Window, cx: &mut App) {
        assert!(
            window.find("feature-info-heading").visible(),
            "Vector properties must be available before the first hit"
        );
        assert!(window.find("feature-info-empty").visible());
        window.click("toggle-feature-info", cx);
        assert_eq!(window.find("toggle-feature-info").label(), Some("Expand properties"));
        assert!(window.try_find("feature-info-empty").is_none());
        window.click("toggle-feature-info", cx);
        assert!(window.find("feature-info-empty").visible());
        assert!(view.read(cx).drag.is_none());
    }

    fn query_center_feature(view: &Entity<MapView>, feature: &serde_json::Value, window: &mut Window, cx: &mut App) -> (u64, u64) {
        let bounds = view.read(cx).bounds.unwrap();
        assert!(bounds.origin.y > px(0.), "The map must start below the title bar");
        let center = bounds.center();
        window.dispatch_event(
            MouseMoveEvent {
                position: center,
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        let (layer, revision) = view.update(cx, |map, cx| {
            let hover = map.vector_hover.expect("A native map pointer move must create a vector query");
            assert_eq!(map.pointer, Some(center));
            assert_eq!(hover.layer_id, map.vector.as_ref().unwrap().id());
            assert!(hover.map.viewport == map.viewport.unwrap());
            assert_eq!(hover.map.camera, map.camera);
            let local = center - bounds.origin;
            assert_eq!(hover.point, (f64::from(f32::from(local.x)), f64::from(f32::from(local.y))));
            assert_eq!(hover.point.0, f64::from(hover.map.viewport.width.get()) / 2.0);
            assert_eq!(hover.point.1, f64::from(hover.map.viewport.height.get()) / 2.0);
            assert_ne!(hover.point.1, f64::from(f32::from(center.y)), "Title-bar height must not enter the query point");
            map.apply_feature_result(map.hover_revision, Ok(vec![feature.clone()]), cx);
            (hover.layer_id, map.hover_revision)
        });

        (layer, revision)
    }

    fn assert_vector_inspector_layout(bounds: Bounds<Pixels>, window: &mut Window) -> Bounds<Pixels> {
        assert!(window.find("feature-info-properties").visible());
        assert!(window.find("feature-info").visible());
        assert!(window.find("feature-info").bounds().size.height > px(60.));
        let layers = window.find("raster-layers-card").bounds();
        let properties = window.find("feature-info").bounds();
        assert_eq!(window.find("feature-info-heading").label(), Some("Properties"));
        assert_eq!(properties.top(), layers.top());
        assert_eq!(properties.left() - bounds.left(), bounds.right() - layers.right());
        assert!(properties.right() < layers.left());
        assert!(properties.size.width > layers.size.width);
        let coordinates = window.find("pixel-info");
        assert_eq!(coordinates.bounds().left(), properties.left());
        assert!(coordinates.bounds().top() > properties.bottom());
        assert_eq!(
            coordinates.label().unwrap().split_whitespace().count(),
            4,
            "Vector hover must show coordinates without a raster value"
        );

        properties
    }

    fn check_zoom_retains_properties(view: &Entity<MapView>, feature: &serde_json::Value, properties: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let center = view.read(cx).bounds.unwrap().center();
        for keyboard in [false, true] {
            let camera = view.read(cx).camera;
            let revision = view.read(cx).hover_revision;
            if keyboard {
                let focus = view.read(cx).focus.clone();
                window.focus(&focus, cx);
                window.press("+", cx);
            } else {
                window.dispatch_event(
                    ScrollEvent {
                        position: center,
                        delta: ScrollDelta::Lines(point(0.0, 1.0)),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                window.render_frame(cx);
            }
            assert_ne!(view.read(cx).camera, camera);
            assert_eq!(view.read(cx).pointer, Some(center), "Zoom must retain the stationary pointer");
            let hover = view.read(cx).vector_hover.expect("Zoom must retain an active feature query");
            assert_eq!(hover.map.camera, view.read(cx).camera);
            assert!(view.read(cx).hover_revision > revision);
            assert!(
                window.find("feature-info-properties").visible(),
                "Pending zoom queries must not blank the properties"
            );
            assert_eq!(
                window.find("feature-info").bounds(),
                properties,
                "Zoom must not resize the pane while refreshing"
            );
            view.update(cx, |map, cx| map.apply_feature_result(revision, Ok(Vec::new()), cx));
            window.render_frame(cx);
            assert!(
                window.find("feature-info-properties").visible(),
                "An empty result from the previous camera must not clear retained properties"
            );
            view.update(cx, |map, cx| map.apply_feature_result(map.hover_revision, Ok(vec![feature.clone()]), cx));
            window.render_frame(cx);
            assert!(
                window.find("feature-info-properties").visible(),
                "Fresh properties must not require a mouse move"
            );
        }
    }

    fn check_pointer_queries_retain_properties(view: &Entity<MapView>, feature: &serde_json::Value, revision: u64, window: &mut Window, cx: &mut App) {
        let bounds = view.read(cx).bounds.unwrap();
        let center = bounds.center();
        let camera = view.read(cx).camera;
        window.hover("feature-info-heading", cx);
        assert!(
            view.read(cx).vector_hover.is_some(),
            "The properties pane must not dismiss its own hover snapshot"
        );
        assert!(window.find("feature-info-properties").visible());
        window.scroll("feature-info-heading", ScrollDelta::Lines(point(0.0, 1.0)), cx);
        assert_eq!(view.read(cx).camera, camera, "Scrolling properties must not zoom the map");

        // Pointer-only queries retain the inspector until the latest hit arrives.
        let info_bounds = window.find("feature-info").bounds();
        window.dispatch_event(
            MouseMoveEvent {
                position: center + point(px(80.), px(40.)),
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        assert!(window.find("feature-info-properties").visible(), "Pending queries must retain property rows");
        assert_eq!(window.find("feature-info").bounds(), info_bounds);
        view.update(cx, |map, cx| map.apply_feature_result(revision, Ok(Vec::new()), cx));
        window.render_frame(cx);
        assert!(
            window.find("feature-info-properties").visible(),
            "A late empty result from the previous cursor must not clear the inspector"
        );
        view.update(cx, |map, cx| {
            assert!(map.hover_revision > revision);
            let hover = map.vector_hover.unwrap();
            assert_eq!(
                hover.point,
                (
                    f64::from(f32::from(bounds.size.width)) / 2.0 + 80.0,
                    f64::from(f32::from(bounds.size.height)) / 2.0 + 40.0
                )
            );
            map.apply_feature_result(map.hover_revision, Ok(vec![feature.clone()]), cx);
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("feature-info").bounds(),
            info_bounds,
            "The same feature must not resize the inspector"
        );
    }

    fn check_empty_and_collapsed_properties(view: &Entity<MapView>, feature: &serde_json::Value, window: &mut Window, cx: &mut App) {
        // Reaching the collapse button can move off the feature and return no hits.
        window.hover("toggle-feature-info", cx);
        let heading = window.find("feature-info-heading").bounds();
        let toggle = window.find("toggle-feature-info").bounds();
        view.update(cx, |map, cx| map.apply_feature_result(map.hover_revision, Ok(Vec::new()), cx));
        window.render_frame(cx);
        assert!(
            window.try_find("feature-info-properties").is_none(),
            "A completed empty query must remove stale properties"
        );
        assert!(window.find("feature-info-empty").visible());
        assert_eq!(window.find("feature-info-heading").bounds(), heading);
        assert_eq!(window.find("toggle-feature-info").bounds(), toggle);
        window.click("toggle-feature-info", cx);
        assert_eq!(window.find("toggle-feature-info").label(), Some("Expand properties"));
        assert!(window.try_find("feature-info-empty").is_none());
        view.update(cx, |map, cx| map.apply_feature_result(map.hover_revision, Ok(vec![feature.clone()]), cx));
        window.render_frame(cx);
        assert!(
            window.try_find("feature-info-properties").is_none(),
            "New hits must not expand the collapsed pane"
        );
        window.click("toggle-feature-info", cx);
        assert!(window.find("feature-info-properties").visible());
    }

    fn check_pan_query_invalidation(view: &Entity<MapView>, feature: &serde_json::Value, window: &mut Window, cx: &mut App) {
        let revision = view.read(cx).hover_revision;
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        window.press("right", cx);
        assert!(
            view.read(cx).hover_revision > revision,
            "Camera changes must invalidate a stationary pointer's query"
        );
        assert_eq!(view.read(cx).vector_hover.unwrap().map.camera, view.read(cx).camera);
        view.update(cx, |map, cx| map.apply_feature_result(revision, Ok(Vec::new()), cx));
        window.render_frame(cx);
        assert!(
            window.find("feature-info-properties").visible(),
            "Panning must retain properties and reject empty results from the previous camera"
        );
        view.update(cx, |map, cx| map.apply_feature_result(map.hover_revision, Ok(Vec::new()), cx));
        window.render_frame(cx);
        assert!(
            window.try_find("feature-info-properties").is_none(),
            "A current-camera miss must still clear properties"
        );
        view.update(cx, |map, cx| map.show_vector_features(vec![feature.clone()], cx));
        window.render_frame(cx);
        assert!(window.find("feature-info").bounds().size.height > px(60.));
    }

    fn check_layer_switch_rejects_features(
        view: &Entity<MapView>,
        feature: &serde_json::Value,
        original_layer: u64,
        revision: u64,
        window: &mut Window,
        cx: &mut App,
    ) {
        assert!(view.read(cx).hover_revision > revision, "Layer switches must invalidate the previous query");
        view.update(cx, |map, cx| map.apply_feature_result(revision, Ok(vec![feature.clone()]), cx));
        window.render_frame(cx);
        assert!(
            window.try_find("feature-info-properties").is_none(),
            "A result from the previous layer must be rejected"
        );
        assert!(window.find("feature-info-heading").visible());
        assert!(window.find("feature-info-empty").visible());
        window.hover("map-surface", cx);
        view.update(cx, |map, cx| {
            let hover = map.vector_hover.unwrap();
            assert_ne!(hover.layer_id, original_layer);
            assert_eq!(hover.layer_id, map.vector.as_ref().unwrap().id());
            assert!(map.hover_revision > revision, "Layer switches must invalidate the previous query");
            map.show_vector_features(vec![feature.clone()], cx);
        });
        window.render_frame(cx);
        assert!(window.find("feature-info").bounds().size.height > px(60.));
    }

    fn leave_vector_for_raster(view: &Entity<MapView>, raster_path: PathBuf, window: &mut Window, cx: &mut App) {
        let revision = view.read(cx).hover_revision;
        window.hover("open-raster", cx);
        assert!(view.read(cx).pointer.is_none());
        assert!(view.read(cx).vector_hover.is_none(), "Leaving the map must clear vector hover");
        assert!(view.read(cx).hover_revision > revision);
        assert!(window.try_find("feature-info-properties").is_none());
        assert!(window.find("feature-info-heading").visible(), "Leaving the map must not hide the vector pane");
        assert!(window.find("feature-info-empty").visible());
        assert_eq!(window.find("toggle-feature-info").label(), Some("Collapse properties"));
        view.update(cx, |map, cx| {
            map.clear_vector_hover(cx);
            assert_eq!(map.hover_revision, revision + 1, "Redundant clears must not issue another revision");
            map.open_paths(vec![raster_path], cx);
        });
    }

    fn exercise_stationary_zoom(
        view: &Entity<MapView>,
        frames: &async_channel::Receiver<MapFrame>,
        results: &async_channel::Receiver<crate::map_renderer::FeatureQueryResult>,
        position: Point<Pixels>,
        handle: gpui_kit::AnyWindowHandle,
        cx: &mut TestAppContext,
    ) {
        let mut properties_bounds = None;
        for phase in 0..5 {
            wait_for_stationary_query(view, (frames, results), position, properties_bounds, phase, handle, cx);
            if phase == 4 {
                break;
            }
            cx.update_window(handle, |_, window, cx| {
                properties_bounds = Some(zoom_stationary_pointer(view, phase, position, window, cx));
            })
            .unwrap();
        }
    }

    struct PropertyLayout {
        properties: Bounds<Pixels>,
        heading: Bounds<Pixels>,
        body: Bounds<Pixels>,
        first: Bounds<Pixels>,
        last: Bounds<Pixels>,
    }

    fn show_many_properties(view: &Entity<MapView>, feature: serde_json::Value, window: &mut Window, cx: &mut App) {
        window.render_frame(cx);
        let bounds = window.find("map-surface").bounds();
        window.dispatch_event(
            MouseMoveEvent {
                position: point(bounds.left() + px(40.0), bounds.center().y),
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        view.update(cx, |map, cx| map.apply_feature_result(map.hover_revision, Ok(vec![feature]), cx));
        window.render_frame(cx);
        assert!(window.find("feature-info-heading").visible());
        assert!(window.within("feature-info-properties").find("field_00").visible());
        assert!(!window.within("feature-info-properties").find("field_63").visible());
    }

    fn assert_short_properties_layout(window: &mut Window) -> PropertyLayout {
        let bounds = window.find("map-surface").bounds();
        let properties = window.find("feature-info").bounds();
        let layers = window.find("raster-layers-card").bounds();
        assert_eq!(properties.top(), layers.top());
        assert_eq!(properties.left() - bounds.left(), bounds.right() - layers.right());
        assert!(properties.right() + window.rem_size() * 0.75 <= layers.left());
        let heading = window.find("feature-info-heading").bounds();
        assert!(window.try_find("feature-info-layer-name").is_none());
        let body = window.find("feature-info-properties").bounds();
        let first = window.within("feature-info-properties").find("field_00").bounds();
        let last = window.within("feature-info-properties").find("field_63").bounds();
        assert_eq!(body.top(), heading.bottom(), "The properties must sit directly below the centered header");
        assert_eq!(window.find("feature-info-title").bounds().center().y, heading.center().y);
        assert_eq!(window.find("toggle-feature-info").bounds().center().y, heading.center().y);
        assert!(body.bottom() <= properties.bottom());
        assert!(properties.bottom() < window.find("pixel-info").bounds().top());

        PropertyLayout {
            properties,
            heading,
            body,
            first,
            last,
        }
    }

    fn check_property_wheel_scroll(view: &Entity<MapView>, layout: &PropertyLayout, camera: MapCamera, window: &mut Window, cx: &mut App) -> Pixels {
        let PropertyLayout {
            properties,
            heading,
            body,
            first,
            last,
        } = *layout;
        window.scroll("feature-info-properties", ScrollDelta::Lines(point(0.0, -3.0)), cx);
        assert!(
            window.within("feature-info-properties").find("field_00").bounds().top() < first.top(),
            "Wheel input must move the property rows"
        );
        assert!(view.read(cx).vector_hover.is_some());
        assert_eq!(view.read(cx).camera, camera, "Scrolling properties must not zoom the map");

        // Wrapped values can make the list taller than a fixed line delta.
        let scroll_extent = last.bottom() - body.bottom();
        window.scroll("feature-info-properties", ScrollDelta::Pixels(point(px(0.0), -scroll_extent)), cx);
        let last = window.within("feature-info-properties").find("field_63");
        assert!(last.visible(), "Every property must remain reachable by scrolling");
        assert!(last.bounds().top() >= body.top());
        assert!(last.bounds().bottom() <= body.bottom());
        assert!(!window.within("feature-info-properties").find("field_00").visible());
        assert_eq!(
            window.find("feature-info-heading").bounds(),
            heading,
            "The heading must stay fixed while rows scroll"
        );
        assert_eq!(window.find("feature-info").bounds(), properties);

        scroll_extent
    }

    fn check_property_scroll_limits(
        view: &Entity<MapView>,
        layout: &PropertyLayout,
        camera: MapCamera,
        scroll_extent: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let PropertyLayout { body, first, .. } = *layout;
        // Wheel events at either limit must stay in the pane, not zoom the map.
        window.scroll("feature-info-properties", ScrollDelta::Lines(point(0.0, -3.0)), cx);
        assert!(window.within("feature-info-properties").find("field_63").visible());
        assert_eq!(view.read(cx).camera, camera, "Scrolling past the last property must not zoom the map");
        window.scroll(
            "feature-info-properties",
            ScrollDelta::Pixels(point(px(0.0), scroll_extent + body.size.height)),
            cx,
        );
        window.scroll("feature-info-properties", ScrollDelta::Lines(point(0.0, 3.0)), cx);
        assert_eq!(window.within("feature-info-properties").find("field_00").bounds(), first);
        assert!(window.within("feature-info-properties").find("field_00").visible());
        assert!(view.read(cx).vector_hover.is_some());
        assert_eq!(view.read(cx).camera, camera, "Scrolling past the first property must not zoom the map");
        assert!(view.read(cx).drag.is_none());
    }

    fn check_property_collapse(view: &Entity<MapView>, properties: Bounds<Pixels>, camera: MapCamera, window: &mut Window, cx: &mut App) {
        window.click("toggle-feature-info", cx);
        assert!(window.try_find("feature-info-properties").is_none());
        assert_eq!(window.find("toggle-feature-info").label(), Some("Expand properties"));
        assert!(window.find("feature-info").bounds().size.height < properties.size.height);
        assert!(view.read(cx).vector_hover.is_some(), "Collapsing must not dismiss the hover");
        assert_eq!(view.read(cx).camera, camera);
        assert!(view.read(cx).drag.is_none(), "The collapse button must not start a map drag");
        window.scroll("feature-info-heading", ScrollDelta::Lines(point(0.0, -3.0)), cx);
        assert_eq!(view.read(cx).camera, camera, "The collapsed header must not zoom the map");
    }

    fn check_property_keyboard_toggle(view: &Entity<MapView>, properties: Bounds<Pixels>, camera: MapCamera, window: &mut Window, cx: &mut App) {
        window.press("tab", cx);
        assert_eq!(window.find("toggle-feature-info").focused(), Some(true));
        window.press("enter", cx);
        assert!(window.find("feature-info-properties").visible());
        window.press("space", cx);
        assert!(window.try_find("feature-info-properties").is_none());

        window.click("toggle-feature-info", cx);
        assert!(window.find("feature-info-properties").visible());
        assert_eq!(window.find("toggle-feature-info").label(), Some("Collapse properties"));
        assert_eq!(window.find("feature-info").bounds(), properties);
        assert!(view.read(cx).vector_hover.is_some());
        assert_eq!(view.read(cx).camera, camera);
        assert!(view.read(cx).drag.is_none());
    }

    fn assert_vector_progress_layout(map_bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        window.render_frame(cx);
        let progress = window.find("vector-load-progress");
        assert!(progress.visible());
        assert_eq!(progress.role(), Some(gpui_kit::Role::ProgressIndicator));
        assert_eq!(progress.label(), Some("Converting vectors · 45%"));
        assert_eq!(window.find("vector-load-status").label(), Some("Converting vectors · 45%"));
        assert_eq!(progress.bounds().top(), window.find("title-bar").bounds().bottom());
        assert_eq!(progress.bounds().left(), map_bounds.left());
        assert_eq!(progress.bounds().right(), map_bounds.right());
        assert_eq!(progress.bounds().size.height, px(4.0), "Use the component's xsmall strip, not a card");
        assert_eq!(window.find("map-surface").bounds(), map_bounds);
        assert_eq!(window.find("map-surface").focused(), Some(true));
        assert!(
            window.try_find("raster-controls").is_none(),
            "The strip cannot depend on an already loaded layer"
        );
    }

    fn assert_cached_pointer_value(window: &mut Window, cx: &mut App) {
        // Cached moves must render a value before any background work runs.
        let center = window.find("map-surface").bounds().center();
        window.dispatch_event(
            MouseMoveEvent {
                position: center + point(px(5.0), px(0.0)),
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        assert!(
            window
                .find("pixel-info")
                .label()
                .unwrap()
                .split_whitespace()
                .nth(4)
                .unwrap()
                .parse::<f64>()
                .is_ok()
        );
        window.hover("raster-layers-card", cx);
    }

    fn assert_title_controls_layout(title_bar: Bounds<Pixels>, open: Bounds<Pixels>, band: Bounds<Pixels>, window: &mut Window) {
        assert!(band.left() < title_bar.center().x);
        assert!(band.top() >= title_bar.top() && band.bottom() <= title_bar.bottom());
        assert!(open.left() >= title_bar.left() && open.right() < band.left());
        assert!(open.top() >= title_bar.top() && open.bottom() <= title_bar.bottom());
        assert_eq!(band.left() - open.right(), window.rem_size() * 0.5);
        assert!(window.within("raster-layers-card").try_find("raster-band").is_none());
        assert!(window.within("raster-layers-card").try_find("open-raster").is_none());
    }

    fn assert_floating_controls_layout(window: &mut Window) -> Bounds<Pixels> {
        let title_bar = window.find("title-bar").bounds();
        let open = window.within("title-bar").find("open-raster").bounds();
        let map = window.find("map-surface").bounds();
        let layers = window.find("raster-layers-card").bounds();
        let range = window.find("raster-range-card").bounds();
        let band = window.within("title-bar").find("raster-band").bounds();
        assert_title_controls_layout(title_bar, open, band, window);
        assert_eq!(title_bar.origin, point(px(0.0), px(0.0)));
        assert_eq!(title_bar.size.width, px(1200.0));
        assert!(title_bar.size.height > px(0.0));
        assert_eq!(
            map.origin,
            point(px(0.0), title_bar.bottom()),
            "Only window chrome may reserve space above the map"
        );
        assert_eq!(map.size, size(px(1200.0), px(760.0) - title_bar.size.height));
        assert!(layers.left() > map.center().x && layers.right() < map.right());
        assert_eq!(layers.left(), range.left());
        assert_eq!(layers.right(), range.right());
        assert!(range.top() > layers.bottom() && range.bottom() < map.bottom());
        layers
    }

    fn continue_range_slider_drag(handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let range = window.find("raster-range-slider").bounds();
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: range.origin + point(range.size.width * 0.25, range.size.height * 0.5),
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            let thumb = window.within("raster-range-slider").find(("slider-thumb", 1_u32)).bounds();
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position: thumb.center(),
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            for fraction in [0.3, 0.4] {
                window.dispatch_event(
                    MouseMoveEvent {
                        position: range.origin + point(range.size.width * fraction, range.size.height * 0.5),
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Default::default(),
                    }
                    .to_platform_input(),
                    cx,
                );
                window.render_frame(cx);
            }
        })
        .unwrap();
    }

    fn exercise_range_slider(view: &Entity<MapView>, handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        let original_range = cx.update(|cx| view.read(cx).raster.as_ref().unwrap().range());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let range = window.find("raster-range-slider").bounds();
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position: range.origin + point(range.size.width * 0.25, range.size.height * 0.5),
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
        let pressed_range = cx.update(|cx| view.read(cx).raster.as_ref().unwrap().range());
        assert!(pressed_range.0 > original_range.0, "Range changes must apply before releasing the pointer");

        continue_range_slider_drag(handle, cx);
        cx.run_until_parked();
        assert!(
            cx.update(|cx| view.read(cx).raster.as_ref().unwrap().range().0) > pressed_range.0,
            "Dragging must continuously apply range changes"
        );

        cx.update_window(handle, |_, window, cx| {
            let range = window.find("raster-range-slider").bounds();
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: range.origin + point(range.size.width * 0.4, range.size.height * 0.5),
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.click("reset-raster-range", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| view.read(cx).raster.as_ref().unwrap().range()), original_range);
    }

    fn check_raster_controls_toggle(handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(handle, |_, window, cx| {
            window.click("toggle-raster-controls", cx);
            assert!(window.try_find("raster-range-card").is_none());
            assert!(window.within("title-bar").find("raster-band").visible());
            assert!(window.within("title-bar").find("open-raster").visible());
            let title_bar = window.find("title-bar").bounds();
            assert_eq!(window.find("map-surface").bounds().size, size(px(1200.0), px(760.0) - title_bar.size.height));
            window.press("secondary-l", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("raster-range-card").visible());
        })
        .unwrap();
    }

    fn check_floating_controls_resize(view: &Entity<MapView>, handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.simulate_window_resize(handle, size(px(640.0), px(400.0)));
        cx.update(|cx| gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(18.0)));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let map = window.find("map-surface").bounds();
            let layers = window.find("raster-layers-card").bounds();
            let title_bar = window.find("title-bar").bounds();
            let open = window.within("title-bar").find("open-raster");
            assert!(open.visible());
            let band = window.within("title-bar").find("raster-band");
            assert!(band.visible());
            assert!(band.bounds().left() < title_bar.center().x);
            assert!(open.bounds().left() >= title_bar.left() && open.bounds().right() < band.bounds().left());
            assert!(band.bounds().right() <= title_bar.right());
            assert_eq!(map.size, size(px(640.0), px(400.0) - title_bar.size.height));
            assert!(layers.right() < map.right() && layers.left() > map.left());
            window.scroll("raster-layers-card", ScrollDelta::Lines(point(0.0, -30.0)), cx);
            assert!(
                window.find("raster-max").visible(),
                "The floating controls must scroll in short windows at larger UI scales"
            );
            view.read(cx).raster_panel.clone().update(cx, |panel, cx| panel.clear(window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("raster-controls").is_none());
            assert!(window.within("title-bar").find("open-raster").visible());
        })
        .unwrap();
    }

    fn start_range_slider_drag(handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let thumb = window.within("raster-range-slider").find(("slider-thumb", 1_u32)).bounds();
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position: thumb.center(),
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            let range = window.find("raster-range-slider").bounds();
            // Cross the drag threshold before checking continuous updates.
            window.dispatch_event(
                MouseMoveEvent {
                    position: range.origin + point(range.size.width * 0.1, range.size.height * 0.5),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        })
        .unwrap();
    }

    fn check_in_flight_range_frames(view: &Entity<MapView>, handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        for fraction in [0.2, 0.3, 0.4] {
            // Emulate a frame that started rendering before this pointer event
            // and completes while the next range is already requested.
            let source = cx.update(|cx| view.read(cx).raster.clone().unwrap());
            cx.update_window(handle, |_, window, cx| {
                let range = window.find("raster-range-slider").bounds();
                let position = range.origin + point(range.size.width * fraction, range.size.height * 0.5);
                window.dispatch_event(
                    MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Default::default(),
                    }
                    .to_platform_input(),
                    cx,
                );
                window.render_frame(cx);
            })
            .unwrap();
            cx.run_until_parked();
            assert_ne!(cx.update(|cx| view.read(cx).raster.as_ref().unwrap().id()), source.id());
            let image = Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(32, 32))]));
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| apply_completed_range_frame(view, &source, &image, window, cx));
            })
            .unwrap();
        }
    }

    fn check_obsolete_band_frame(view: &Entity<MapView>, handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        let obsolete_source = cx.update(|cx| view.read(cx).raster.clone().unwrap());
        cx.update_window(handle, |_, window, cx| {
            let range = window.find("raster-range-slider").bounds();
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: range.origin + point(range.size.width * 0.4, range.size.height * 0.5),
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.click("raster-band", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |view, cx| {
                assert_ne!(view.raster.as_ref().unwrap().metadata().id, obsolete_source.metadata().id);
                let shown = view.image.clone().unwrap();
                let obsolete = Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(32, 32))]));
                view.apply_frame(
                    MapFrame {
                        viewport: view.viewport.unwrap(),
                        basemap: basemap_style(cx),
                        raster_layer_id: Some(obsolete_source.metadata().id),
                        vector_layer_id: None,
                        settled: false,
                        tile_boundaries: false,
                        image: Ok(obsolete),
                    },
                    window,
                    cx,
                );
                assert!(
                    Arc::ptr_eq(view.image.as_ref().unwrap(), &shown),
                    "An in-flight frame from another band must still be rejected"
                );
            });
        })
        .unwrap();
    }

    fn check_coast_interruptions(view: &Entity<MapView>, from: Point<Pixels>, to: Point<Pixels>, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let after_pan = view.read(cx).camera;
        // End a drag on the attribution footer, outside the map hitbox.
        window.drag(from, point(from.x, bounds.bottom() + px(5.0)), cx);
        assert_ne!(view.read(cx).camera, after_pan);
        assert!(view.read(cx).drag.is_none());
        assert!(view.read(cx).coast.is_some());

        let after_release = view.read(cx).camera;
        window.hover("map-surface", cx);
        assert_eq!(view.read(cx).camera, after_release);
        window.right_click("map-surface", cx);
        assert_eq!(view.read(cx).camera, after_release);
        window.press("-", cx);
        assert_ne!(view.read(cx).camera, after_release);
        assert!(view.read(cx).coast.is_none());

        window.drag(from, to, cx);
        assert!(view.read(cx).coast.is_some());
        window.click("map-surface", cx);
        assert!(view.read(cx).coast.is_none());
        let after_interrupt = view.read(cx).camera;
        window.simulate_next_frame(cx);
        assert_eq!(view.read(cx).camera, after_interrupt);

        cx.set_reduce_motion(true);
        window.drag(from, to, cx);
        assert!(view.read(cx).coast.is_none());
    }
    fn check_basemap_frame_identity(map: &mut MapView, expected: BasemapStyle, camera: MapCamera, window: &mut Window, cx: &mut Context<MapView>) {
        assert_eq!(map.requested.as_ref().unwrap().basemap, expected);
        assert_eq!(map.camera, camera);
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(32, 32))]));
        map.apply_frame(
            MapFrame {
                viewport: map.viewport.unwrap(),
                basemap: expected,
                tile_boundaries: false,
                raster_layer_id: None,
                vector_layer_id: None,
                settled: false,
                image: Ok(image.clone()),
            },
            window,
            cx,
        );
        map.apply_frame(
            MapFrame {
                viewport: map.viewport.unwrap(),
                basemap: if expected == BasemapStyle::Light {
                    BasemapStyle::Dark
                } else {
                    BasemapStyle::Light
                },
                raster_layer_id: None,
                vector_layer_id: None,
                settled: false,
                image: Err(anyhow::anyhow!("Obsolete basemap failed")),
                tile_boundaries: false,
            },
            window,
            cx,
        );
        assert!(Arc::ptr_eq(map.image.as_ref().unwrap(), &image));
        assert!(map.error.is_none(), "Obsolete basemap errors must not replace the current map");
    }

    fn write_separated_rasters(directory: &std::path::Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for (name, west) in [("near", -10.0), ("far", 100.0)] {
            let path = directory.join(format!("{name}.tif"));
            crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
            let mut dataset = gdal::Dataset::open_ex(
                &path,
                gdal::DatasetOptions {
                    open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                    ..Default::default()
                },
            )
            .unwrap();
            dataset.set_geo_transform(&[west, 20.0 / 64.0, 0.0, 10.0, 0.0, -20.0 / 64.0]).unwrap();
            dataset.close().unwrap();
            paths.push(path);
        }

        paths
    }

    fn check_vector_frame_identity(map: &mut MapView, old_id: u64, window: &mut Window, cx: &mut Context<MapView>) {
        let current_id = map.vector.as_ref().unwrap().id();
        assert_ne!(current_id, old_id);
        assert!(map.raster.is_none());
        assert_eq!(map.requested.as_ref().unwrap().vector_id, Some(current_id));
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(32, 32))]));
        map.apply_frame(
            MapFrame {
                viewport: map.viewport.unwrap(),
                basemap: basemap_style(cx),
                raster_layer_id: None,
                vector_layer_id: Some(old_id),
                settled: false,
                tile_boundaries: map.tile_boundaries,
                image: Ok(image.clone()),
            },
            window,
            cx,
        );
        assert!(map.image.is_none(), "A frame for the previous vector source must be rejected");
        map.apply_frame(
            MapFrame {
                viewport: map.viewport.unwrap(),
                basemap: basemap_style(cx),
                raster_layer_id: None,
                vector_layer_id: Some(current_id),
                settled: false,
                tile_boundaries: map.tile_boundaries,
                image: Ok(image.clone()),
            },
            window,
            cx,
        );
        assert!(Arc::ptr_eq(map.image.as_ref().unwrap(), &image), "The current vector frame must be accepted");
        assert!(map.error.is_none());
    }

    fn measure_empty_selector(window: &mut Window, cx: &mut App) -> Pixels {
        window.render_frame(cx);
        let selector = window.find("raster-band");
        assert!(selector.visible());
        assert_eq!(selector.disabled(), Some(true));
        let width = selector.bounds().size.width;
        let text = "Open a geospatial dataset";
        let text_width = window
            .text_system()
            .shape_line(text.into(), window.rem_size() * 0.875, &[window.text_style().to_run(text.len())], None)
            .width();
        assert!(width > text_width, "The empty placeholder must fit with room for the caret");
        window.click("open-raster", cx);
        width
    }

    fn measure_long_selector(view: &Entity<MapView>, placeholder_width: Pixels, short_width: Pixels, window: &mut Window, cx: &mut App) -> Pixels {
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).raster.as_ref().unwrap().metadata().name,
            "Regional elevation observations for winter"
        );
        let width = window.find("raster-band").bounds().size.width;
        assert!(width > placeholder_width && width > short_width, "Longer names must grow the selector");
        let text = &view.read(cx).raster.as_ref().unwrap().metadata().name;
        let text_width = window
            .text_system()
            .shape_line(text.clone().into(), window.rem_size() * 0.875, &[window.text_style().to_run(text.len())], None)
            .width();
        assert!(width > text_width, "An uncapped layer title must fit with room for the caret");
        window.click("raster-band", cx);
        window.press("up", cx);
        window.press("enter", cx);
        width
    }

    fn check_selector_input_keeps_width(view: &Entity<MapView>, long_width: Pixels, handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(view.read(cx).raster.as_ref().unwrap().metadata().name, "dem");
            assert_eq!(
                window.find("raster-band").bounds().size.width,
                long_width,
                "Keyboard selection must not change width"
            );
            window.scroll("raster-band", ScrollDelta::Lines(point(0., -1.)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).raster.as_ref().unwrap().metadata().name,
                "Regional elevation observations for winter"
            );
            assert_eq!(
                window.find("raster-band").bounds().size.width,
                long_width,
                "Wheel selection must not change width"
            );
            Theme::update(cx, |theme| theme.font_size = px(24.));
        })
        .unwrap();
    }

    fn check_capped_selector_actions(view: &Entity<MapView>, wide_capped_width: Pixels, handle: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.simulate_window_resize(handle, size(px(640.), px(400.)));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let title_bar = window.find("title-bar").bounds();
            let band = window.find("raster-band").bounds();
            let open = window.find("open-raster").bounds();
            let debug = window.find("toggle-tile-boundaries").bounds();
            let settings = window.find("open-settings").bounds();
            assert!(band.size.width < wide_capped_width && band.size.width <= title_bar.size.width * 0.5);
            assert!(open.right() <= band.left() && band.right() < debug.left());
            assert!(debug.right() <= settings.left() && settings.right() <= title_bar.right());
            let controls = window.find("window-controls").bounds();
            if controls.size.width > px(0.) {
                assert!(settings.right() <= controls.left() && controls.right() <= title_bar.right());
            }
            assert!(window.find("open-raster").visible() && window.find("open-settings").visible());
            window.click("toggle-tile-boundaries", cx);
            assert!(view.read(cx).tile_boundaries, "The trailing action must remain clickable after capping");
            window.click("open-settings", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("appearance-settings").visible(), "Settings must remain clickable after capping");
        })
        .unwrap();
    }

    fn drain_map_frames(view: &Entity<MapView>, frames: &async_channel::Receiver<MapFrame>, window: &mut Window, cx: &mut App) {
        while let Ok(frame) = frames.try_recv() {
            view.update(cx, |map, cx| map.apply_frame(frame, window, cx));
        }
    }

    fn drain_stationary_query_results(
        view: &Entity<MapView>,
        results: &async_channel::Receiver<crate::map_renderer::FeatureQueryResult>,
        phase: usize,
        properties_bounds: Option<Bounds<Pixels>>,
        received_current_hit: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        while let Ok(result) = results.try_recv() {
            let revision = result.revision();
            let features = result.into_features();
            *received_current_hit |= revision == view.read(cx).hover_revision && features.as_ref().is_ok_and(|features| !features.is_empty());
            view.update(cx, |map, cx| map.apply_feature_result(revision, features, cx));
            window.render_frame(cx);
            if phase > 0 {
                assert!(window.find("feature-info-properties").visible(), "Worker results must not cause zoom flicker");
                assert_eq!(Some(window.find("feature-info").bounds()), properties_bounds);
            }
        }
    }

    fn zoom_stationary_pointer(view: &Entity<MapView>, phase: usize, position: Point<Pixels>, window: &mut Window, cx: &mut App) -> Bounds<Pixels> {
        let properties_bounds = window.find("feature-info").bounds();
        let camera = view.read(cx).camera;
        let revision = view.read(cx).hover_revision;
        if phase < 2 {
            window.dispatch_event(
                ScrollEvent {
                    position,
                    delta: ScrollDelta::Lines(point(0.0, if phase == 0 { 1.0 } else { -1.0 })),
                    modifiers: Default::default(),
                    touch_phase: Default::default(),
                    source: Default::default(),
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        } else {
            window.press(if phase == 2 { "+" } else { "-" }, cx);
        }
        assert_ne!(view.read(cx).camera, camera);
        assert!(view.read(cx).hover_revision > revision);
        assert_eq!(view.read(cx).pointer, Some(position));
        assert!(
            window.find("feature-info-properties").visible(),
            "Zoom must retain properties while querying the new camera"
        );

        properties_bounds
    }

    fn assert_stationary_refresh(
        view: &Entity<MapView>,
        position: Point<Pixels>,
        phase: usize,
        properties_bounds: Option<Bounds<Pixels>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.render_frame(cx);
        assert_eq!(view.read(cx).pointer, Some(position), "Zoom must not need a mouse move (phase {phase})");
        assert!(view.read(cx).error.is_none(), "{:?}", view.read(cx).error);
        if phase > 0 {
            assert!(
                window.find("feature-info-properties").visible(),
                "Properties must remain visible throughout the refresh"
            );
            assert_eq!(Some(window.find("feature-info").bounds()), properties_bounds);
        }
    }

    fn wait_for_stationary_query(
        view: &Entity<MapView>,
        channels: (
            &async_channel::Receiver<MapFrame>,
            &async_channel::Receiver<crate::map_renderer::FeatureQueryResult>,
        ),
        position: Point<Pixels>,
        properties_bounds: Option<Bounds<Pixels>>,
        phase: usize,
        handle: gpui_kit::AnyWindowHandle,
        cx: &mut TestAppContext,
    ) {
        let (frames, results) = channels;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received_current_hit = false;
        loop {
            let populated = cx
                .update_window(handle, |_, window, cx| {
                    drain_map_frames(view, frames, window, cx);
                    drain_stationary_query_results(view, results, phase, properties_bounds, &mut received_current_hit, window, cx);
                    assert_stationary_refresh(view, position, phase, properties_bounds, window, cx);
                    received_current_hit && window.try_find("feature-info-properties").is_some_and(|properties| properties.visible())
                })
                .unwrap();
            if populated {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Properties did not populate in phase {phase} with a stationary pointer"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn apply_completed_range_frame(view: &mut MapView, source: &RasterSource, image: &Arc<RenderImage>, window: &mut Window, cx: &mut Context<MapView>) {
        view.apply_frame(
            MapFrame {
                viewport: view.viewport.unwrap(),
                basemap: basemap_style(cx),
                raster_layer_id: Some(source.metadata().id),
                vector_layer_id: None,
                settled: false,
                tile_boundaries: false,
                image: Ok(image.clone()),
            },
            window,
            cx,
        );
        assert!(
            view.image.as_ref().is_some_and(|shown| Arc::ptr_eq(shown, image)),
            "A continuous range drag must display completed frames, not wait for an exact style match"
        );
    }
}
