use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;

use gpui_kit::component::input::{InputEvent, InputState, NumberInput};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme, Icon, IndexPath, Sizable};
use gpui_kit::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    PathPromptOptions, Render, ScrollDelta, ScrollEvent, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, WeakEntity,
    Window, div, rgba,
};
use inf::colormap::{ColorMapDirection, ColorMapPreset, ProcessedColorMap};

use crate::raster_source::RasterSource;
use crate::vector_source::{VectorLoadProgress, VectorSource};

gpui_kit::actions!(raster, [OpenDataset, FitRaster, ClearRasters, ToggleRasterControls]);

const PALETTES: [&str; 5] = ["Terrain", "Blues", "Gray", "Turbo", "Spectral"];
const LOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(60);

#[derive(Clone, Copy, Debug, PartialEq)]
enum LoadProgress {
    Opening,
    Converting(f64),
    Preparing,
    Rendering(u64),
}

impl LoadProgress {
    fn conversion_milestone(self) -> Option<u8> {
        match self {
            Self::Converting(fraction) => Some((fraction * 4.0) as u8),
            _ => None,
        }
    }

    fn advance_from(self, previous: Option<Self>) -> Option<Self> {
        match self {
            Self::Converting(fraction) if fraction.is_finite() => {
                let previous = match previous {
                    Some(Self::Converting(previous)) => previous,
                    _ => 0.0,
                };
                Some(Self::Converting(fraction.clamp(0.0, 1.0).max(previous)))
            }
            Self::Converting(_) | Self::Rendering(_) => None,
            progress => Some(progress),
        }
    }

    fn status(self) -> SharedString {
        match self {
            Self::Opening => "Opening datasets".into(),
            Self::Converting(fraction) => format!("Converting vectors · {:.0}%", fraction * 100.0).into(),
            Self::Preparing => "Preparing vector data".into(),
            Self::Rendering(_) => "Rendering vectors".into(),
        }
    }
}

#[derive(Default)]
struct LoadedDatasets {
    sources: Vec<Arc<RasterSource>>,
    vectors: Vec<Arc<VectorSource>>,
    errors: Vec<String>,
    selected: Option<SelectionId>,
}

pub struct RasterEvent {
    source: Option<Arc<RasterSource>>,
    vector_source: Option<Arc<VectorSource>>,
    opacity: f32,
    fit: bool,
}

impl RasterEvent {
    pub fn source(&self) -> Option<Arc<RasterSource>> {
        self.source.clone()
    }
    pub fn vector_source(&self) -> Option<Arc<VectorSource>> {
        self.vector_source.clone()
    }
    pub fn opacity(&self) -> f32 {
        self.opacity
    }
    pub fn should_fit(&self) -> bool {
        self.fit
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionId {
    Raster(tiler::LayerId),
    Vector(u64),
}

#[derive(Clone)]
struct BandChoice {
    id: SelectionId,
    title: SharedString,
}

impl SelectItem for BandChoice {
    type Value = SelectionId;
    fn title(&self) -> SharedString {
        self.title.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

pub struct RasterPanel {
    sources: Vec<Arc<RasterSource>>,
    vectors: Vec<Arc<VectorSource>>,
    selected: Option<SelectionId>,
    band_scroll: f32,
    palette_scroll: f32,
    bands: Entity<SelectState<Vec<BandChoice>>>,
    palette: Entity<SelectState<Vec<&'static str>>>,
    property: Entity<SelectState<SearchableVec<SharedString>>>,
    opacity: Entity<SliderState>,
    range_slider: Entity<SliderState>,
    range_domain: (f64, f64),
    min: Entity<InputState>,
    max: Entity<InputState>,
    error: Option<SharedString>,
    load_error: Option<SharedString>,
    loading: bool,
    load_progress: Option<LoadProgress>,
    load_generation: u64,
    load_started: Option<Instant>,
    collapsed: bool,
    focus: FocusHandle,
    load_task: Option<Task<()>>,
    tile_errors_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RasterEvent> for RasterPanel {}

impl RasterPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let bands = cx.new(|cx| SelectState::new(Vec::<BandChoice>::new(), None, window, cx));
        let palette = cx.new(|cx| SelectState::new(PALETTES.to_vec(), Some(IndexPath::default()), window, cx));
        let property = cx.new(|cx| SelectState::new(SearchableVec::new(Vec::<SharedString>::new()), None, window, cx).searchable(true));
        let opacity = cx.new(|_| SliderState::new().min(0.0).max(100.0).step(1.0).default_value(100.0));
        // A normalized domain keeps slider stepping useful for tiny or very
        // large data ranges without replacing retained component state.
        let range_slider = cx.new(|_| SliderState::new().min(0.0).max(1000.0).step(1.0).default_value(0.0..1000.0));
        let min = cx.new(|cx| InputState::new(window, cx).placeholder("Minimum"));
        let max = cx.new(|cx| InputState::new(window, cx).placeholder("Maximum"));
        let subscriptions = vec![
            cx.subscribe_in(&bands, window, |this, _, event, window, cx| {
                let SelectEvent::Confirm(id) = event;
                this.select(*id, false, window, cx);
            }),
            cx.subscribe_in(&palette, window, |this, _, _: &SelectEvent<Vec<&'static str>>, window, cx| {
                this.apply_style(window, cx)
            }),
            cx.subscribe_in(&property, window, |this, _, event, window, cx| {
                let SelectEvent::Confirm(property) = event;
                this.select_property(property.as_deref(), window, cx);
            }),
            cx.subscribe_in(&opacity, window, |this, _, event: &SliderEvent, _, cx| {
                if let SliderEvent::Change(_) = event {
                    this.emit(false, cx);
                }
            }),
            cx.subscribe_in(&range_slider, window, |this, _, event: &SliderEvent, window, cx| {
                if let SliderEvent::Change(value) = event {
                    let (min, max) = this.range_domain;
                    let start = range_value((min, max), f64::from(value.start()) / 1000.0);
                    let end = range_value((min, max), f64::from(value.end()) / 1000.0);
                    this.set_range_inputs((start, end), window, cx);
                    this.apply_style(window, cx);
                }
            }),
            cx.subscribe_in(&min, window, Self::on_range_input),
            cx.subscribe_in(&max, window, Self::on_range_input),
        ];
        Self {
            sources: Vec::new(),
            vectors: Vec::new(),
            selected: None,
            band_scroll: 0.0,
            palette_scroll: 0.0,
            bands,
            palette,
            property,
            opacity,
            range_slider,
            range_domain: (0.0, 1.0),
            min,
            max,
            error: None,
            load_error: None,
            loading: false,
            load_progress: None,
            load_generation: 0,
            load_started: None,
            collapsed: false,
            focus: cx.focus_handle(),
            load_task: None,
            tile_errors_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn active(&self) -> Option<&Arc<RasterSource>> {
        self.sources
            .iter()
            .find(|source| Some(SelectionId::Raster(source.metadata().id)) == self.selected)
    }

    fn active_vector(&self) -> Option<&Arc<VectorSource>> {
        self.vectors.iter().find(|source| Some(SelectionId::Vector(source.id())) == self.selected)
    }

    fn choices(&self) -> Vec<BandChoice> {
        self.sources
            .iter()
            .map(|source| BandChoice {
                id: SelectionId::Raster(source.metadata().id),
                title: source.metadata().name.clone().into(),
            })
            .chain(self.vectors.iter().map(|source| BandChoice {
                id: SelectionId::Vector(source.id()),
                title: source.name().to_owned().into(),
            }))
            .collect()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Start of the most recently accepted load, for drop-to-frame diagnostics.
    pub fn load_started(&self) -> Option<Instant> {
        self.load_started
    }

    pub fn load_status(&self) -> Option<SharedString> {
        self.load_progress.map(LoadProgress::status)
    }

    pub fn render_load_progress(&self) -> Option<Progress> {
        self.load_progress.map(|progress| {
            let fraction = match progress {
                LoadProgress::Converting(fraction) => Some(fraction),
                _ => None,
            };
            Progress::new("vector-load-progress")
                .xsmall()
                .value((fraction.unwrap_or(0.0) * 100.0) as f32)
                .loading(fraction.is_none())
                .accessibility_label(progress.status())
                .absolute()
                .top_0()
                .left_0()
                .w_full()
        })
    }

    pub fn load_error(&self) -> Option<SharedString> {
        self.load_error.clone()
    }

    pub fn dismiss_load_error(&mut self, cx: &mut Context<Self>) {
        self.load_error = None;
        cx.notify();
    }

    fn emit(&self, fit: bool, cx: &mut Context<Self>) {
        let opacity = self.opacity.read(cx).value().end() / 100.0;
        cx.emit(RasterEvent {
            source: self.active().cloned(),
            vector_source: self.active_vector().cloned(),
            opacity,
            fit,
        });
        cx.notify();
    }

    fn select(&mut self, id: Option<SelectionId>, fit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(LoadProgress::Rendering(source)) = self.load_progress
            && id != Some(SelectionId::Vector(source))
        {
            self.load_progress = None;
        }
        self.selected = id;
        self.palette_scroll = 0.0;
        self.error = None;
        self.load_error = None;
        self.collapsed = false;
        self.sync_color_controls(window, cx);
        self.watch_tile_errors(cx);
        self.emit(fit, cx);
    }

    fn watch_tile_errors(&mut self, cx: &mut Context<Self>) {
        self.tile_errors_task = self.active().map(|source| {
            let errors = source.errors();
            cx.spawn(async move |this, cx| receive_tile_errors(this, errors, cx).await)
        });
    }

    fn scroll_bands(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.choices();
        let Some(ix) = items.iter().position(|item| Some(item.id) == self.selected) else {
            return;
        };
        let next = scroll_index(event, &mut self.band_scroll, ix, items.len(), cx);
        if next != ix {
            let id = items[next].id;
            self.bands.update(cx, |state, cx| state.set_selected_value(&id, window, cx));
            self.select(Some(id), false, window, cx);
        }
    }

    fn scroll_palette(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.color_range().is_none() {
            return;
        }
        let Some(ix) = PALETTES.iter().position(|palette| Some(palette) == self.palette.read(cx).selected_value()) else {
            return;
        };
        let next = scroll_index(event, &mut self.palette_scroll, ix, PALETTES.len(), cx);
        if next != ix {
            self.palette.update(cx, |state, cx| state.set_selected_value(&PALETTES[next], window, cx));
            self.apply_style(window, cx);
        }
    }

    pub fn prompt(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open dataset".into()),
        });
        self.loading = true;
        self.load_progress = None;
        self.load_generation += 1;
        let generation = self.load_generation;
        cx.notify();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = match paths.await {
                Ok(Ok(paths)) => Ok(paths),
                error => Err(format!("Couldn’t open the file picker: {error:?}").into()),
            };
            let _ = this.update_in(cx, |this, _, cx| this.complete_path_prompt(generation, result, cx));
        }));
    }

    fn complete_path_prompt(&mut self, generation: u64, result: Result<Option<Vec<PathBuf>>, SharedString>, cx: &mut Context<Self>) {
        if self.load_generation != generation {
            return;
        }
        self.load_task = None;
        self.loading = false;
        match result {
            Ok(Some(paths)) => self.open_paths(paths, cx),
            Ok(None) => cx.notify(),
            Err(error) => {
                self.load_error = Some(error);
                cx.notify();
            }
        }
    }

    pub fn open_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.loading || paths.is_empty() {
            return;
        }
        let enqueued = Instant::now();
        log::info!(target: "map::vector_load", "stage=open-request paths={paths:?}");
        let (progress_tx, progress_rx) = async_channel::bounded(1);
        let work = cx.background_spawn(async move {
            log::info!(target: "map::vector_load", "stage=loader-worker-start queue_delay={:?}", enqueued.elapsed());
            load_paths(paths, progress_tx)
        });
        self.start_load(work, progress_rx, cx);
    }

    fn start_load(&mut self, work: Task<LoadedDatasets>, progress: async_channel::Receiver<LoadProgress>, cx: &mut Context<Self>) {
        let started = Instant::now();
        self.load_started = Some(started);
        self.loading = true;
        self.load_progress = Some(LoadProgress::Opening);
        self.load_generation += 1;
        let generation = self.load_generation;
        self.error = None;
        self.load_error = None;
        cx.notify();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            // One retained task owns both completion and progress. Finishing or
            // clearing the load drops the updater, its timer, and its receiver.
            let updates = receive_load_progress(&this, generation, progress, cx);
            let loaded = futures_lite::future::or(work, updates).await;
            log::info!(target: "map::vector_load", "load={generation} stage=loader-finished since_request={:?} vectors={} rasters={} errors={}", started.elapsed(), loaded.vectors.len(), loaded.sources.len(), loaded.errors.len());
            let _ = this.update_in(cx, |this, window, cx| this.install_loaded_datasets(generation, loaded, started, window, cx));
        }));
    }

    fn install_loaded_datasets(&mut self, generation: u64, loaded: LoadedDatasets, started: Instant, window: &mut Window, cx: &mut Context<Self>) {
        if self.load_generation != generation || !self.loading {
            return;
        }
        self.load_task = None;
        self.loading = false;
        self.load_progress = None;
        let installed = Instant::now();
        self.sources.extend(loaded.sources);
        self.vectors.extend(loaded.vectors);
        let items = self.choices();
        self.bands.update(cx, |state, cx| state.set_items(items, window, cx));
        if let Some(id) = loaded.selected {
            self.bands.update(cx, |state, cx| state.set_selected_value(&id, window, cx));
            self.select(Some(id), true, window, cx);
        }
        self.load_progress = match loaded.selected {
            Some(SelectionId::Vector(source)) => Some(LoadProgress::Rendering(source)),
            _ => None,
        };
        self.load_error = (!loaded.errors.is_empty()).then(|| loaded.errors.join("\n").into());
        log::info!(target: "map::vector_load", "load={generation} stage=ui-install-loaded-layers elapsed={:?} since_request={:?} awaiting_vector_frame={}", installed.elapsed(), started.elapsed(), self.load_progress.is_some());
        cx.notify();
    }

    fn update_load_progress(&mut self, generation: u64, progress: LoadProgress, cx: &mut Context<Self>) {
        if self.load_generation != generation || !self.loading {
            return;
        }
        let Some(progress) = progress.advance_from(self.load_progress) else { return };
        if self.load_progress == Some(progress) {
            return;
        }
        if self.load_progress.and_then(LoadProgress::conversion_milestone) != progress.conversion_milestone()
            || !matches!(progress, LoadProgress::Converting(_))
        {
            log::info!(target: "map::vector_load", "load={generation} stage=ui-progress progress={progress:?} since_request={:?}", self.load_started.map(|started| started.elapsed()));
        }
        self.load_progress = Some(progress);
        cx.notify();
    }

    /// Finish only the displayed load; late frames cannot clear a newer load.
    pub fn complete_vector_render(&mut self, source: u64, cx: &mut Context<Self>) {
        if self.load_progress == Some(LoadProgress::Rendering(source)) {
            log::info!(target: "map::vector_load", "source={source} stage=ui-vector-render-finished since_request={:?}", self.load_started.map(|started| started.elapsed()));
            self.load_progress = None;
            cx.notify();
        }
    }

    pub fn fit(&self, cx: &mut Context<Self>) {
        self.emit(true, cx);
    }

    pub fn toggle_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        // Collapsing may remove the focused numeric editor. Keep shortcuts
        // working on the retained panel instead of leaving focus on hidden UI.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn on_range_input(&mut self, _: &Entity<InputState>, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
            self.apply_style(window, cx);
        }
    }

    fn color_range(&self) -> Option<(f64, f64)> {
        self.active()
            .map(|source| {
                let (min, max) = source.range();
                (f64::from(min), f64::from(max))
            })
            .or_else(|| self.active_vector().and_then(|source| source.range()))
    }

    fn data_range(&self) -> Option<(f64, f64)> {
        self.active()
            .map(|source| {
                let (min, max) = source.data_range();
                (f64::from(min), f64::from(max))
            })
            .or_else(|| self.active_vector().and_then(|source| source.data_range()))
    }

    fn color_palette(&self) -> Option<ColorMapPreset> {
        self.active()
            .map(|source| source.palette())
            .or_else(|| self.active_vector().filter(|source| source.property().is_some()).map(|source| source.palette()))
    }

    fn set_range_inputs(&self, range: (f64, f64), window: &mut Window, cx: &mut Context<Self>) {
        let format = |value: f64| if self.active().is_some() { format!("{value:.2}") } else { value.to_string() };
        self.min.update(cx, |input, cx| input.set_value(format(range.0), window, cx));
        self.max.update(cx, |input, cx| input.set_value(format(range.1), window, cx));
    }

    fn sync_color_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raster = self.active().is_some();
        for input in [&self.min, &self.max] {
            input.update(cx, |input, cx| {
                input.set_validator(
                    move |value, _| !raster || value.split_once('.').is_none_or(|(_, fraction)| fraction.len() <= 2),
                    cx,
                );
            });
        }
        let items: Vec<SharedString> = self
            .active_vector()
            .into_iter()
            .flat_map(|source| source.numeric_properties())
            .map(|property| property.name().to_owned().into())
            .collect();
        let selected = self
            .active_vector()
            .and_then(|source| source.property())
            .map(|name| SharedString::from(name.to_owned()));
        self.property.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(items), window, cx);
            if let Some(selected) = selected {
                state.set_selected_value(&selected, window, cx);
            } else {
                state.set_selected_index(None, window, cx);
            }
        });
        if let Some(range) = self.color_range() {
            self.set_range_inputs(range, window, cx);
        }
        if let Some(preset) = self.color_palette() {
            let palette = match preset {
                ColorMapPreset::Gray => "Gray",
                ColorMapPreset::Blues => "Blues",
                ColorMapPreset::Turbo => "Turbo",
                ColorMapPreset::Spectral => "Spectral",
                _ => "Terrain",
            };
            self.palette.update(cx, |state, cx| state.set_selected_value(&palette, window, cx));
        }
        self.sync_range_slider(window, cx);
    }

    fn select_property(&mut self, property: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.active_vector() else { return };
        let Some(property) = property else {
            self.sync_color_controls(window, cx);
            return;
        };
        if source.property() == Some(property) {
            return;
        }
        match source.with_property(property) {
            Ok(source) => {
                self.replace_vector(source);
                self.error = None;
                self.sync_color_controls(window, cx);
                self.emit(false, cx);
            }
            Err(error) => {
                self.error = Some(format!("Could not select property: {error}").into());
                cx.notify();
            }
        }
    }

    fn replace_raster(&mut self, source: Arc<RasterSource>) {
        if let Some(current) = self
            .sources
            .iter_mut()
            .find(|current| Some(SelectionId::Raster(current.metadata().id)) == self.selected)
        {
            *current = source;
        }
    }

    fn replace_vector(&mut self, source: Arc<VectorSource>) {
        if let Some(current) = self.vectors.iter_mut().find(|current| current.id() == source.id()) {
            *current = source;
        }
    }

    fn sync_range_slider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((data_min, data_max)) = self.data_range() else { return };
        let Some((min, max)) = self.color_range() else { return };
        self.range_domain = (data_min.min(min), data_max.max(max));
        let start = range_position(self.range_domain, min) as f32 * 1000.0;
        let end = range_position(self.range_domain, max) as f32 * 1000.0;
        self.range_slider.update(cx, |slider, cx| slider.set_value(start..end, window, cx));
    }

    fn set_unscaling(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.active() else { return };
        if !source.has_scale_metadata() || source.unscaling_enabled() == enabled {
            return;
        }
        match source.with_unscaling(enabled) {
            Ok(source) => {
                let range = source.range();
                self.replace_raster(source);
                self.min.update(cx, |input, cx| input.set_value(format!("{:.2}", range.0), window, cx));
                self.max.update(cx, |input, cx| input.set_value(format!("{:.2}", range.1), window, cx));
                self.error = None;
                self.sync_range_slider(window, cx);
                self.watch_tile_errors(cx);
                self.emit(false, cx);
            }
            Err(error) => {
                self.error = Some(format!("Could not change value units: {error}").into());
                cx.notify();
            }
        }
    }

    fn reset_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.data_range() else { return };
        self.set_range_inputs(range, window, cx);
        self.apply_style(window, cx);
    }

    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_generation += 1;
        self.load_task = None;
        self.tile_errors_task = None;
        self.loading = false;
        self.load_progress = None;
        self.sources.clear();
        self.vectors.clear();
        self.band_scroll = 0.0;
        self.palette_scroll = 0.0;
        self.selected = None;
        self.error = None;
        self.load_error = None;
        self.bands.update(cx, |state, cx| {
            state.set_items(Vec::new(), window, cx);
            state.set_selected_index(None, window, cx);
        });
        self.sync_color_controls(window, cx);
        self.emit(false, cx);
    }

    fn selected_palette(&self, cx: &App) -> ColorMapPreset {
        match self.palette.read(cx).selected_value().copied() {
            Some("Gray") => ColorMapPreset::Gray,
            Some("Blues") => ColorMapPreset::Blues,
            Some("Turbo") => ColorMapPreset::Turbo,
            Some("Spectral") => ColorMapPreset::Spectral,
            _ => ColorMapPreset::Terrain,
        }
    }

    fn restyle_active_source(&mut self, cx: &App) -> anyhow::Result<()> {
        let palette = self.selected_palette(cx);
        if self.active().is_some() {
            return self.restyle_raster(palette, cx);
        }
        if let Some(source) = self.active_vector() {
            let min = self.min.read(cx).value().trim().parse::<f64>()?;
            let max = self.max.read(cx).value().trim().parse::<f64>()?;
            let source = source.restyled(palette, (min, max))?;
            self.replace_vector(source);
        }
        Ok(())
    }

    fn restyle_raster(&mut self, palette: ColorMapPreset, cx: &App) -> anyhow::Result<()> {
        let Some(source) = self.active() else { return Ok(()) };
        let min = self.min.read(cx).value().trim().parse::<f32>()?;
        let max = self.max.read(cx).value().trim().parse::<f32>()?;
        // Repeated commits must not invalidate raster tiles.
        if source.palette() != palette || source.range() != (min, max) {
            let source = source.restyled(palette, (min, max))?;
            self.replace_raster(source);
        }
        Ok(())
    }

    fn apply_style(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.color_range().is_none() {
            return;
        }
        match self.restyle_active_source(cx) {
            Ok(()) => {
                self.error = None;
                self.watch_tile_errors(cx);
                self.sync_range_slider(window, cx);
                self.emit(false, cx);
            }
            Err(_) => {
                self.error = Some("Enter a finite minimum and maximum, with minimum less than maximum.".into());
                cx.notify();
            }
        }
    }
}

impl RasterPanel {
    pub fn render_layer_selector(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement + use<> {
        const PLACEHOLDER: &str = "Open a geospatial dataset";

        let rem = window.rem_size();
        let text_style = window.text_style();
        let measure = |text: &str| {
            text.split('\n')
                .map(|line| {
                    window
                        .text_system()
                        .shape_line(line.to_owned().into(), rem * 0.875, &[text_style.to_run(line.len())], None)
                        .width()
                })
                .fold(gpui_kit::Pixels::ZERO, gpui_kit::Pixels::max)
        };
        // Size to all loaded layers so changing selection does not move the actions.
        let text_width = self
            .sources
            .iter()
            .map(|source| source.metadata().name.as_str())
            .chain(self.vectors.iter().map(|source| source.name()))
            .map(measure)
            .reduce(gpui_kit::Pixels::max)
            .unwrap_or_else(|| measure(PLACEHOLDER));
        // Match small Select padding, its rem-based caret + gap, and physical borders.
        let chrome_width = gpui_kit::component::Size::Small.input_px() * 2.0 + rem * 1.125 + gpui_kit::px(2.0);
        let viewport_width = window.viewport_size().width;
        // Keep a drag region and room for Open, trailing actions, and window controls.
        let max_width = (viewport_width * 0.5).min((viewport_width - rem * 14.0).max(gpui_kit::Pixels::ZERO));

        div()
            .id("raster-band-scroll")
            .w((text_width + chrome_width).ceil().min(max_width))
            .min_w_0()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll(cx.listener(Self::scroll_bands))
            .on_hover(cx.listener(|this, hovered, _, _| {
                if !hovered {
                    this.band_scroll = 0.0;
                }
            }))
            .child(
                Select::new(&self.bands)
                    .id("raster-band")
                    .small()
                    .accessibility_label("Layer")
                    .placeholder(PLACEHOLDER)
                    .disabled(self.sources.is_empty() && self.vectors.is_empty()),
            )
    }
}

impl RasterPanel {
    fn render_layers_card(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let focus_border = if self.focus.is_focused(window) { cx.theme().ring } else { cx.theme().border };
        let mut layers = overlay_card("raster-layers-card", cx).border_color(focus_border).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(Icon::new(IconName::Layers).small())
                .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Layer"))
                .child(
                    Button::new("toggle-raster-controls")
                        .small()
                        .ghost()
                        .icon(if self.collapsed { IconName::ChevronDown } else { IconName::ChevronUp })
                        .accessibility_label(if self.collapsed { "Show layer controls" } else { "Hide layer controls" })
                        .tooltip("Show or hide controls (Ctrl/⌘+L)")
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_controls(window, cx))),
                ),
        );
        if !self.collapsed {
            if self.active_vector().is_some_and(|source| source.property().is_some()) {
                layers = layers.child(
                    div().flex().flex_col().gap_1().child(field_label("Color by", cx)).child(
                        Select::new(&self.property)
                            .id("vector-property")
                            .accessibility_label("Color by")
                            .placeholder("Select a numeric property")
                            .search_placeholder("Search properties…")
                            .w_full(),
                    ),
                );
            }
            if self.color_range().is_some() {
                layers = layers.child(self.render_palette(cx)).child(self.render_legend(cx));
            }
            if self.selected.is_some() {
                layers = layers.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(field_label("Opacity", cx))
                        .child(div().id("raster-opacity").test_support().px_2().child(Slider::new(&self.opacity))),
                );
            }
            if let Some(error) = &self.error {
                layers = layers.child(div().text_sm().text_color(cx.theme().danger).child(error.clone()));
            }
        }

        layers
    }

    fn render_palette(&self, cx: &Context<Self>) -> impl IntoElement {
        div().flex().flex_col().gap_1().child(field_label("Color map", cx)).child(
            div()
                .id("raster-palette-scroll")
                .on_scroll(cx.listener(Self::scroll_palette))
                .on_hover(cx.listener(|this, hovered, _, _| {
                    if !hovered {
                        this.palette_scroll = 0.0;
                    }
                }))
                .child(Select::new(&self.palette).id("raster-palette").accessibility_label("Color map")),
        )
    }
}

impl Render for RasterPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .h_auto()
            .min_h_0()
            .child(self.render_layers_card(window, cx));
        if !self.collapsed && self.color_range().is_some() {
            panel = panel.child(self.render_value_range(cx));
        }
        div()
            .id("raster-controls")
            .test_support()
            .track_focus(&self.focus)
            .occlude()
            .flex()
            .flex_col()
            .w_full()
            .max_h_full()
            .min_h_0()
            .child(panel.overflow_y_scrollbar().id("raster-controls-scroll"))
    }
}

impl RasterPanel {
    fn render_legend(&self, cx: &App) -> impl IntoElement {
        let mut legend = div().id("raster-legend").test_support().flex().flex_col().gap_1();
        if let Some(((min, max), palette)) = self.color_range().zip(self.color_palette()) {
            legend = legend
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .children((0..5).map(|ix| format_value(range_value((min, max), ix as f64 / 4.0)))),
                )
                .child(self.render_legend_swatches((min, max), palette, cx));
        }
        legend
    }

    fn render_legend_swatches(&self, range: (f64, f64), palette: ColorMapPreset, cx: &App) -> impl IntoElement {
        let colors = ProcessedColorMap::<256>::create_for_preset(palette, ColorMapDirection::Regular);
        let swatches = (0..256).map(|ix| {
            let color = if let Some(source) = self.active() {
                source.legend().color_for_value(range_value(range, ix as f64 / 255.0) as f32, None)
            } else {
                colors.get_color_by_value(ix as u8)
            };
            // These colors encode data, not application chrome.
            div().flex_1().h_3().bg(rgba(u32::from_be_bytes([color.r, color.g, color.b, color.a])))
        });
        div()
            .flex()
            .w_full()
            .rounded(cx.theme().radius_tokens().sm)
            .overflow_hidden()
            .children(swatches)
    }

    fn render_value_range(&self, cx: &mut Context<Self>) -> impl IntoElement {
        overlay_card("raster-range-card", cx)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(IconName::SlidersHorizontal).small())
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Value range"))
                    .child(
                        Button::new("reset-raster-range")
                            .small()
                            .ghost()
                            .label("Reset")
                            .on_click(cx.listener(|this, _, window, cx| this.reset_range(window, cx))),
                    ),
            )
            .child(div().id("raster-range-slider").test_support().px_2().child(Slider::new(&self.range_slider)))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div().flex().flex_col().flex_1().min_w_0().gap_1().child(field_label("Minimum", cx)).child(
                            div()
                                .id("raster-min")
                                .test_support()
                                .child(NumberInput::new(&self.min).small().accessibility_label("Minimum")),
                        ),
                    )
                    .child(
                        div().flex().flex_col().flex_1().min_w_0().gap_1().child(field_label("Maximum", cx)).child(
                            div()
                                .id("raster-max")
                                .test_support()
                                .child(NumberInput::new(&self.max).small().accessibility_label("Maximum")),
                        ),
                    ),
            )
            .children(self.active().filter(|source| source.has_scale_metadata()).map(|source| {
                Checkbox::new("raster-unscaling")
                    .small()
                    .label("Descale values")
                    .checked(source.unscaling_enabled())
                    .tooltip("Apply the raster’s scale and offset to pixel values")
                    .on_change(cx.listener(|this, enabled, window, cx| this.set_unscaling(*enabled, window, cx)))
            }))
    }
}

async fn receive_tile_errors(this: WeakEntity<RasterPanel>, errors: async_channel::Receiver<String>, cx: &mut AsyncApp) {
    while let Ok(error) = errors.recv().await {
        if this
            .update(cx, |this, cx| {
                this.load_error = Some(error.into());
                cx.notify();
            })
            .is_err()
        {
            break;
        }
    }
}

async fn receive_load_progress(
    this: &WeakEntity<RasterPanel>,
    generation: u64,
    progress: async_channel::Receiver<LoadProgress>,
    cx: &mut AsyncApp,
) -> LoadedDatasets {
    while let Ok(progress) = progress.recv().await {
        if this.update(cx, |this, cx| this.update_load_progress(generation, progress, cx)).is_err() {
            break;
        }
        cx.background_executor().timer(LOAD_PROGRESS_INTERVAL).await;
    }
    futures_lite::future::pending().await
}

fn load_paths(paths: Vec<PathBuf>, progress: async_channel::Sender<LoadProgress>) -> LoadedDatasets {
    let mut loaded = LoadedDatasets::default();
    let path_count = paths.len() as f64;

    for (index, path) in paths.into_iter().enumerate() {
        load_path(path, index, path_count, &progress, &mut loaded);
    }
    loaded
}

fn load_path(path: PathBuf, index: usize, path_count: f64, progress: &async_channel::Sender<LoadProgress>, loaded: &mut LoadedDatasets) {
    let opened = Instant::now();
    let _ = progress.force_send(LoadProgress::Opening);
    let mut progress_bucket = None;
    let mut report = |event| {
        let event = match event {
            VectorLoadProgress::Converting(fraction) if fraction.is_finite() => {
                let bucket = (fraction.clamp(0.0, 1.0) * 4.0) as u8;
                if progress_bucket != Some(bucket) {
                    progress_bucket = Some(bucket);
                    log::info!(target: "map::vector_load", "path={path:?} stage=gdal-progress fraction={fraction:.3} since_path_open={:?}", opened.elapsed());
                }
                LoadProgress::Converting((index as f64 + fraction.clamp(0.0, 1.0)) / path_count)
            }
            VectorLoadProgress::Converting(_) => return,
            VectorLoadProgress::Preparing => LoadProgress::Preparing,
        };
        // Replace pending samples instead of blocking GDAL or queuing one
        // UI event per feature. Percentages describe conversion only.
        let _ = progress.force_send(event);
    };
    match VectorSource::open_with_load_progress(&path, &mut report) {
        Ok(Some(opened)) => {
            loaded.selected = opened.first().map(|source| SelectionId::Vector(source.id()));
            loaded.vectors.extend(opened);
        }
        Ok(None) => match RasterSource::open(&path) {
            Ok(opened) => {
                loaded.selected = opened.first().map(|source| SelectionId::Raster(source.metadata().id));
                loaded.sources.extend(opened);
            }
            Err(error) => loaded.errors.push(format!("{}: {error:#}", path.display())),
        },
        Err(error) => loaded.errors.push(format!("{}: {error:#}", path.display())),
    }
    log::info!(target: "map::vector_load", "path={path:?} stage=path-loaded elapsed={:?}", opened.elapsed());
}

fn scroll_index(event: &ScrollEvent, scroll: &mut f32, ix: usize, count: usize, cx: &mut App) -> usize {
    let delta = match event.delta {
        ScrollDelta::Lines(delta) => delta.y,
        // Accumulate precise device pixels so small trackpad events don't
        // race through choices. This is an input threshold, not layout size.
        ScrollDelta::Pixels(delta) => f32::from(delta.y) / 40.0,
    };
    if !delta.is_finite() || delta == 0.0 {
        return ix;
    }
    cx.stop_propagation();
    if scroll.signum() != delta.signum() {
        *scroll = 0.0;
    }
    *scroll += delta;
    // A wheel notch can report several lines; navigation still moves by only
    // one index. Keep only the fractional trackpad remainder.
    let steps = scroll.trunc().clamp(-1.0, 1.0);
    *scroll = scroll.fract();
    (ix as f32 - steps).clamp(0.0, (count - 1) as f32) as usize
}

fn overlay_card(id: &'static str, cx: &App) -> impl IntoElement + ParentElement + Styled {
    div()
        .id(id)
        .test_support()
        .block_mouse_except_scroll()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap_4()
        .p_4()
        .bg(cx.theme().popover)
        .text_color(cx.theme().popover_foreground)
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius_tokens().lg)
        .shadow(cx.theme().shadow_tokens().sm)
}

fn field_label(label: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

// Keep interpolation finite even for ranges spanning both f64 extremes.
fn range_value((min, max): (f64, f64), position: f64) -> f64 {
    if position <= 0.0 {
        return min;
    }
    if position >= 1.0 {
        return max;
    }
    if (max - min).is_finite() {
        min + (max - min) * position
    } else {
        min * (1.0 - position) + max * position
    }
}

fn range_position((min, max): (f64, f64), value: f64) -> f64 {
    if (max - min).is_finite() {
        (value - min) / (max - min)
    } else {
        (value / 2.0 - min / 2.0) / (max / 2.0 - min / 2.0)
    }
}

fn format_value(value: f64) -> String {
    if value != 0.0 && (value.abs() < 0.001 || value.abs() >= 100_000.0) {
        format!("{value:.2e}")
    } else {
        format!("{value:.3}").trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

#[cfg(test)]
#[path = "raster_panel_progress_tests.rs"]
pub(crate) mod progress_tests;

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, point, px, size};

    use super::*;

    struct PanelHarness(Entity<RasterPanel>);

    impl Render for PanelHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let selector = self.0.update(cx, |panel, cx| panel.render_layer_selector(window, cx));
            div().flex().flex_col().size_full().child(selector).child(self.0.clone())
        }
    }

    type PanelWindow = gpui_kit::WindowHandle<gpui_kit::base::Root>;

    fn open_test_panel(cx: &mut TestAppContext, height: f32) -> (Entity<RasterPanel>, PanelWindow) {
        let mut panel = None;
        let handle = cx.open_window(size(px(900.0), px(height)), |window, cx| {
            let view = cx.new(|cx| RasterPanel::new(window, cx));
            panel = Some(view.clone());
            let harness = cx.new(|_| PanelHarness(view));
            gpui_kit::base::Root::new(harness, window, cx)
        });
        (panel.unwrap(), handle)
    }

    fn record_events(panel: &Entity<RasterPanel>, cx: &mut TestAppContext) -> Entity<Vec<RasterEvent>> {
        cx.new(|cx: &mut Context<Vec<RasterEvent>>| {
            cx.subscribe(panel, |events, _, event, _| {
                events.push(RasterEvent {
                    source: event.source(),
                    vector_source: event.vector_source(),
                    opacity: event.opacity(),
                    fit: event.should_fit(),
                });
            })
            .detach();
            Vec::new()
        })
    }

    #[test]
    fn normalized_ranges_preserve_endpoints_and_remain_finite() {
        for range in [(1e-12, 9e-12), (-f64::MAX, f64::MAX), (0.0, f64::from_bits(1)), (1.0, 1.0 + f64::EPSILON)] {
            assert_eq!(range_value(range, 0.0), range.0);
            assert_eq!(range_value(range, 1.0), range.1);
            assert_eq!(range_position(range, range.0), 0.0);
            assert_eq!(range_position(range, range.1), 1.0);
            assert!(range_value(range, 0.5).is_finite());
        }
    }

    #[gpui_kit::test]
    fn numeric_vectors_support_search_palette_ranges_and_per_layer_selection_through_the_ui(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let directory = tempfile::tempdir().unwrap();
        let paths = numeric_vector_paths(&directory);
        let (panel, handle) = open_test_panel(cx, 900.0);
        let events = record_events(&panel, cx);
        cx.update_window(handle.into(), |_, _, cx| panel.update(cx, |panel, cx| panel.open_paths(paths, cx)))
            .unwrap();
        cx.run_until_parked();
        let original = open_numeric_property_search(&panel, handle, cx);
        search_property(handle, "fraction", cx);
        let styled = style_fraction_property(&panel, &original, handle, cx);
        verify_vector_style_across_layers(&panel, &styled, handle, cx);
        reset_vector_range_and_reopen_property(&panel, &styled, handle, cx);
        search_property(handle, "tiny", cx);
        verify_tiny_property_and_clear(&panel, &events, handle, cx);
    }

    fn numeric_vector_paths(directory: &tempfile::TempDir) -> Vec<PathBuf> {
        let raster = directory.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&raster, 4326, 1).unwrap();
        let plain = directory.path().join("plain.geojson");
        std::fs::write(&plain, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"label":"00123","active":true},"geometry":{"type":"Point","coordinates":[4,51]}}]}"#).unwrap();
        let mut paths = vec![raster, plain];
        for name in ["first", "second"] {
            let path = directory.path().join(format!("{name}.geojson"));
            std::fs::write(&path, serde_json::to_vec(&serde_json::json!({
                "type": "FeatureCollection", "features": [
                    {"type":"Feature", "properties":{"a_count":1,"b_fraction":0.125,"constant":7,"tiny":1e-12,"label":"42","active":true,"empty":null},"geometry":{"type":"Point","coordinates":[4,51]}},
                    {"type":"Feature", "properties":{"a_count":9,"b_fraction":0.875,"constant":7,"tiny":9e-12},"geometry":{"type":"Point","coordinates":[5,52]}}
                ]
            })).unwrap()).unwrap();
            paths.push(path);
        }
        paths
    }

    fn open_numeric_property_search(panel: &Entity<RasterPanel>, handle: PanelWindow, cx: &mut TestAppContext) -> Arc<VectorSource> {
        let original = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let source = panel.read(cx).active_vector().unwrap().clone();
                assert_eq!(source.property(), Some("a_count"));
                assert_eq!(
                    source.numeric_properties().iter().map(|property| property.name()).collect::<Vec<_>>(),
                    ["a_count", "b_fraction", "constant", "tiny"]
                );
                assert_eq!(window.find("vector-property").label(), Some("Color by"));
                assert_eq!(window.find("vector-property").role(), Some(gpui_kit::Role::ComboBox));
                assert_eq!(window.find("vector-property").value(), Some("a_count"));
                assert!(window.find("raster-palette").visible());
                assert!(window.find("raster-legend").visible());
                assert!(window.find("raster-range-slider").visible());
                assert!(window.try_find("raster-unscaling").is_none());
                assert!(window.find("raster-palette").bounds().top() >= window.find("vector-property").bounds().bottom());
                window.click("vector-property", cx);
                window.press("escape", cx);
                source
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("vector-property").expanded(), Some(false));
            assert_eq!(window.find("vector-property").focused(), Some(true));
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        original
    }

    fn search_property(handle: PanelWindow, query: &str, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("vector-property").expanded(), Some(true));
            window.input(query, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.background_executor.advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);

            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn style_fraction_property(panel: &Entity<RasterPanel>, original: &VectorSource, handle: PanelWindow, cx: &mut TestAppContext) -> Arc<VectorSource> {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let source = panel.read(cx).active_vector().unwrap();
            assert_eq!(source.id(), original.id());
            assert_ne!(source.style_id(), original.style_id());
            assert_eq!(source.property(), Some("b_fraction"));
            assert_eq!(source.range(), Some((0.125, 0.875)));
            assert_eq!(
                panel.read(cx).property.read(cx).selected_value().map(|value| value.as_str()),
                Some("b_fraction")
            );
            window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            window.click("raster-min", cx);
            window.press("secondary-a", cx);
            window.input("0.200001", cx);
            window.click("raster-max", cx);
            window.press("secondary-a", cx);
            window.input("0.800001", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, _, cx| {
            let source = panel.read(cx).active_vector().unwrap().clone();
            assert_eq!(source.palette(), ColorMapPreset::Blues);
            assert_eq!(source.range(), Some((0.200001, 0.800001)));
            assert_eq!(source.source_json(), original.source_json());
            assert_eq!(panel.read(cx).range_domain, (0.125, 0.875));
            source
        })
        .unwrap()
    }

    fn verify_vector_style_across_layers(panel: &Entity<RasterPanel>, source: &VectorSource, handle: PanelWindow, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            // Cross another numeric layer, a nonnumeric vector, and a raster.
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_eq!(panel.read(cx).active_vector().unwrap().property(), Some("a_count"));
            assert_eq!(panel.read(cx).active_vector().unwrap().palette(), ColorMapPreset::Terrain);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert!(window.try_find("vector-property").is_none());
            assert!(window.try_find("raster-palette").is_none());
            assert!(window.try_find("raster-range-slider").is_none());
            assert!(window.find("raster-opacity").visible());
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert!(panel.read(cx).active().is_some());
            assert!(window.try_find("vector-property").is_none());
            assert!(window.find("raster-palette").visible());
            for _ in 0..3 {
                window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            }
            assert_eq!(panel.read(cx).active_vector().unwrap().style_id(), source.style_id());
            assert_eq!(
                panel.read(cx).property.read(cx).selected_value().map(|value| value.as_str()),
                Some("b_fraction")
            );
            assert_eq!(panel.read(cx).min.read(cx).value().as_str(), "0.200001");
            window.click("raster-min", cx);
            window.press("secondary-a", cx);
            window.input("2", cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn reset_vector_range_and_reopen_property(panel: &Entity<RasterPanel>, styled: &VectorSource, handle: PanelWindow, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(panel.read(cx).error.is_some());
            assert_eq!(panel.read(cx).active_vector().unwrap().style_id(), styled.style_id());
            window.click("reset-raster-range", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(panel.read(cx).error.is_none());
            assert_eq!(panel.read(cx).active_vector().unwrap().range(), Some((0.125, 0.875)));
            window.click("toggle-raster-controls", cx);
            assert!(window.try_find("vector-property").is_none());
            window.click("toggle-raster-controls", cx);
            window.click("vector-property", cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn verify_tiny_property_and_clear(panel: &Entity<RasterPanel>, events: &Entity<Vec<RasterEvent>>, handle: PanelWindow, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).active_vector().unwrap().property(), Some("tiny"));
            let range = panel.read(cx).active_vector().unwrap().range().unwrap();
            assert_eq!(Some(range), panel.read(cx).active_vector().unwrap().data_range());
            assert!((range.0 - 1e-12).abs() < 1e-26 && (range.1 - 9e-12).abs() < 1e-26);
            assert_eq!(panel.read(cx).min.read(cx).value().as_str().parse::<f64>().unwrap(), range.0);
            assert_eq!(panel.read(cx).max.read(cx).value().as_str().parse::<f64>().unwrap(), range.1);
            assert!(events.read(cx).iter().skip(1).all(|event| !event.should_fit()));
            panel.update(cx, |panel, cx| panel.clear(window, cx));
            window.render_frame(cx);
            assert!(window.try_find("vector-property").is_none());
            assert!(panel.read(cx).property.read(cx).selected_value().is_none());
        })
        .unwrap();
    }

    fn unscaling_paths(directory: &tempfile::TempDir) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for (name, coefficients) in [("plain", None), ("identity", Some((1.0, 0.0))), ("scaled", Some((0.5, -7.0)))] {
            let path = directory.path().join(format!("{name}.tif"));
            write_raster_with_coefficients(&path, coefficients);
            paths.push(path);
        }
        paths
    }

    fn write_raster_with_coefficients(path: &std::path::Path, coefficients: Option<(f64, f64)>) {
        crate::raster_source::test_support::write_raster(path, 4326, 1).unwrap();
        if let Some((scale, offset)) = coefficients {
            let dataset = gdal::Dataset::open_ex(
                path,
                gdal::DatasetOptions {
                    open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut band = dataset.rasterband(1).unwrap();
            band.set_scale(scale).unwrap();
            band.set_offset(offset).unwrap();
            dataset.close().unwrap();
            if (scale, offset) == (1.0, 0.0) {
                // GeoTIFF omits identity coefficients; PAM retains their explicit presence.
                std::fs::write(
                    path.with_extension("tif.aux.xml"),
                    "<PAMDataset><PAMRasterBand band=\"1\"><Scale>1</Scale><Offset>0</Offset></PAMRasterBand></PAMDataset>",
                )
                .unwrap();
            }
        }
    }

    #[gpui_kit::test]
    fn unscaling_checkbox_tracks_metadata_units_and_per_layer_state_through_the_ui(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = unscaling_paths(&dir);
        let (panel, handle) = open_test_panel(cx, 760.0);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("raster-unscaling").is_none());
            panel.update(cx, |panel, cx| panel.open_paths(paths, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.within("raster-range-card").find("raster-unscaling").label(), Some("Descale values"));
            assert_eq!(window.find("raster-unscaling").checked(), Some(true));
            let checkbox = window.find("raster-unscaling").bounds();
            let minimum = window.find("raster-min").bounds();
            let maximum = window.find("raster-max").bounds();
            assert!(
                checkbox.top() >= minimum.bottom() && checkbox.top() >= maximum.bottom(),
                "Unscaling belongs below both range fields"
            );
            assert!(checkbox.bottom() <= window.find("raster-range-card").bounds().bottom());
            assert_eq!(panel.read(cx).active().unwrap().range(), (2.0, 61.0));
            window.click("raster-min", cx);
            window.press("secondary-a", cx);
            window.input("5", cx);
            window.click("raster-max", cx);
            window.press("secondary-a", cx);
            window.input("20", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert_eq!(panel.read(cx).active().unwrap().range(), (5.0, 20.0));
            let id = panel.read(cx).active().unwrap().metadata().id;
            window.click("raster-unscaling", cx);
            assert_eq!(window.find("raster-unscaling").checked(), Some(false));
            assert_eq!(panel.read(cx).active().unwrap().metadata().id, id);
            assert_raw_range_controls(&panel, cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert!(
                panel.read(cx).active().unwrap().has_scale_metadata(),
                "Explicit identity scaling must show the toggle"
            );
            assert_eq!(window.find("raster-unscaling").checked(), Some(true));
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert!(window.try_find("raster-unscaling").is_none(), "Unscaled layers must not show the toggle");
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            assert_eq!(window.find("raster-unscaling").checked(), Some(false), "Mode must be retained per layer");
            window.click("raster-unscaling", cx);
            assert_eq!(window.find("raster-unscaling").checked(), Some(true));
            verify_unscaling_keyboard_toggle(window, cx);
            assert_eq!(panel.read(cx).active().unwrap().range(), (5.0, 20.0));
            window.click("reset-raster-range", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).active().unwrap().range(), (2.0, 61.0));
            window.click("raster-unscaling", cx);
            window.click("reset-raster-range", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(panel.read(cx).active().unwrap().range(), (18.0, 136.0));
            assert!(!panel.read(cx).active().unwrap().unscaling_enabled());
        });
    }

    fn assert_raw_range_controls(panel: &Entity<RasterPanel>, cx: &App) {
        let state = panel.read(cx);
        assert_eq!(state.active().unwrap().range(), (24.0, 54.0));
        assert_eq!(state.range_domain, (18.0, 136.0));
        assert_eq!(state.min.read(cx).value().as_str(), "24.00");
        assert_eq!(state.max.read(cx).value().as_str(), "54.00");
    }

    fn verify_unscaling_keyboard_toggle(window: &mut Window, cx: &mut App) {
        for _ in 0..20 {
            if window.find("raster-unscaling").focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find("raster-unscaling").focused(), Some(true), "Checkbox must be reachable with Tab");
        window.press("space", cx);
        assert_eq!(window.find("raster-unscaling").checked(), Some(false), "Space must toggle the focused checkbox");
        window.press("space", cx);
        assert_eq!(window.find("raster-unscaling").checked(), Some(true));
    }

    #[gpui_kit::test]
    fn mixed_datasets_support_selection_controls_opacity_and_clear_through_the_ui(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let raster = dir.path().join("bands.tif");
        crate::raster_source::test_support::write_raster(&raster, 4326, 2).unwrap();
        let vector = dir.path().join("places.geojson");
        std::fs::write(
            &vector,
            r#"{"type":"FeatureCollection","name":"Places","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[4,51]}}]}"#,
        )
        .unwrap();
        let (panel, handle) = open_test_panel(cx, 760.0);
        let events = record_events(&panel, cx);
        cx.update_window(handle.into(), |_, _, cx| {
            panel.update(cx, |panel, cx| panel.prompt(cx));
        })
        .unwrap();
        cx.simulate_path_prompt_response(|options| {
            assert_eq!(options.prompt.as_deref(), Some("Open dataset"));
            assert!(options.files && options.multiple && !options.directories);
            Some(vec![vector, raster])
        });
        cx.run_until_parked();
        cx.update(|cx| assert_mixed_datasets_loaded(&panel, &events, cx));
        select_vector_and_change_opacity(&panel, handle, cx);
        cx.update(|cx| {
            let event = events.read(cx).last().unwrap();
            assert!(event.source().is_none());
            assert_eq!(event.vector_source().unwrap().id(), panel.read(cx).active_vector().unwrap().id());
            assert!((event.opacity() - 0.5).abs() < 0.05);
            assert!(!event.should_fit());
            assert!(events.read(cx).iter().all(|event| event.source().is_none() || event.vector_source().is_none()));
        });
        verify_mixed_dataset_keyboard_selection_and_clear(&panel, &events, handle, cx);
    }

    fn assert_mixed_datasets_loaded(panel: &Entity<RasterPanel>, events: &Entity<Vec<RasterEvent>>, cx: &App) {
        let panel = panel.read(cx);
        assert!(!panel.loading);
        assert!(panel.load_error.is_none());
        assert_eq!(panel.sources.len(), 2);
        assert_eq!(panel.vectors.len(), 1);
        assert_eq!(panel.active().unwrap().metadata().band_nr, Some(1));
        let event = events.read(cx).last().unwrap();
        assert!(event.source().is_some());
        assert!(event.vector_source().is_none());
        assert!(event.should_fit());
    }

    fn assert_nonnumeric_vector_range_controls_absent(window: &Window) {
        assert!(window.try_find("raster-palette").is_none());
        assert!(window.try_find("raster-legend").is_none());
        assert!(window.try_find("raster-range-slider").is_none());
        assert!(window.try_find("raster-min").is_none());
        assert!(window.try_find("raster-max").is_none());
    }

    fn select_vector_and_change_opacity(panel: &Entity<RasterPanel>, handle: PanelWindow, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("raster-band").label(), Some("Layer"));
            assert!(window.try_find("raster-palette").is_some());
            assert!(window.try_find("raster-legend").is_some());
            assert!(window.try_find("raster-range-slider").is_some());
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -6.0)), cx);
            assert_eq!(panel.read(cx).active().unwrap().metadata().band_nr, Some(2));
            window.scroll("raster-band", ScrollDelta::Pixels(point(px(0.0), px(-240.0))), cx);
            assert!(panel.read(cx).active().is_none());
            assert!(panel.read(cx).active_vector().is_some());
            assert_eq!(panel.read(cx).bands.read(cx).selected_value().copied(), panel.read(cx).selected);
            assert_nonnumeric_vector_range_controls_absent(window);
            assert_eq!(window.find("toggle-raster-controls").label(), Some("Hide layer controls"));
            window.click("toggle-raster-controls", cx);
            assert!(window.try_find("raster-opacity").is_none());
            window.click("toggle-raster-controls", cx);
            let opacity = window.find("raster-opacity").bounds();
            window.click_at("raster-opacity", point(opacity.size.width * 0.5, opacity.size.height * 0.5), cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn verify_mixed_dataset_keyboard_selection_and_clear(
        panel: &Entity<RasterPanel>,
        events: &Entity<Vec<RasterEvent>>,
        handle: PanelWindow,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(handle.into(), |_, window, cx| {
            // Dropdown keyboard navigation must use the same order as scrolling.
            window.click("raster-band", cx);
            window.press("up", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).active().unwrap().metadata().band_nr, Some(2));
            assert!(window.try_find("raster-palette").is_some());
            assert!(window.try_find("raster-range-slider").is_some());
            panel.update(cx, |panel, cx| panel.clear(window, cx));
            window.render_frame(cx);
            assert!(panel.read(cx).sources.is_empty());
            assert!(panel.read(cx).vectors.is_empty());
            assert!(panel.read(cx).selected.is_none());
            assert!(panel.read(cx).bands.read(cx).selected_value().is_none());
            assert!(window.try_find("raster-opacity").is_none());
            assert_eq!(window.find("raster-band").disabled(), Some(true));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let event = events.read(cx).last().unwrap();
            assert!(event.source().is_none());
            assert!(event.vector_source().is_none());
            assert!(!event.should_fit());
        });
    }

    #[gpui_kit::test]
    fn scrolling_changes_at_most_one_band_per_event(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bands.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 5).unwrap();
        let (panel, handle) = open_test_panel(cx, 760.0);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            panel.update(cx, |panel, cx| panel.prompt(cx));
        })
        .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![path]));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            for (delta, expected_band) in [
                (ScrollDelta::Lines(point(0.0, -3.0)), 2),
                (ScrollDelta::Lines(point(0.0, -6.0)), 3),
                (ScrollDelta::Lines(point(0.0, 3.0)), 2),
                (ScrollDelta::Pixels(point(px(0.0), px(-240.0))), 3),
                (ScrollDelta::Pixels(point(px(0.0), px(240.0))), 2),
            ] {
                window.scroll("raster-band", delta, cx);
                assert_eq!(panel.read(cx).active().unwrap().metadata().band_nr, Some(expected_band));
                assert_eq!(panel.read(cx).bands.read(cx).selected_value().copied(), panel.read(cx).selected);
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn prompt_cancel_and_open_then_range_validation_and_clear_work_through_the_ui(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let (panel, handle) = open_test_panel(cx, 760.0);
        verify_picker_cancel_and_open(&panel, handle, path, cx);
        let original_band = cx.update(|cx| panel.read(cx).selected);
        verify_band_navigation(&panel, handle, original_band, cx);
        verify_raster_range_precision(&panel, handle, cx);
        verify_raster_range_validation_and_repeated_commit(&panel, handle, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(window.try_find("fit-raster").is_none());
            assert!(window.try_find("clear-rasters").is_none());
            panel.update(cx, |panel, cx| panel.clear(window, cx));
            window.render_frame(cx);
            assert!(panel.read(cx).sources.is_empty());
            assert!(panel.read(cx).selected.is_none());
            assert!(window.try_find("raster-min").is_none());
        })
        .unwrap();
    }

    fn verify_picker_cancel_and_open(panel: &Entity<RasterPanel>, handle: PanelWindow, path: PathBuf, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("fit-raster").is_none());
            assert!(window.try_find("clear-rasters").is_none());
            assert!(panel.read(cx).selected.is_none());
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            assert!(panel.read(cx).selected.is_none(), "An empty selector must ignore scroll");
            panel.update(cx, |panel, cx| panel.prompt(cx));
        })
        .unwrap();
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|options| {
            assert!(options.files && options.multiple && !options.directories);
            None
        });
        cx.run_until_parked();
        assert!(!cx.update(|cx| panel.read(cx).loading));

        cx.update_window(handle.into(), |_, _, cx| {
            panel.update(cx, |panel, cx| panel.prompt(cx));
        })
        .unwrap();
        cx.simulate_path_prompt_response(|_| Some(vec![path]));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| panel.read(cx).sources.len()), 2);
        cx.update(|cx| {
            let panel = panel.read(cx);
            assert_eq!(panel.selected, Some(SelectionId::Raster(panel.sources[0].metadata().id)));
            assert_eq!(
                panel.active().unwrap().metadata().band_nr,
                Some(1),
                "Opening a multiband raster must select its first band"
            );
        });
    }

    fn verify_band_navigation(panel: &Entity<RasterPanel>, handle: PanelWindow, original_band: Option<SelectionId>, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_eq!(panel.read(cx).selected, original_band, "Scrolling up at the first band must not wrap");
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            let second = panel.read(cx).selected;
            assert_ne!(second, original_band, "Scrolling down must select the next band");
            assert_eq!(panel.read(cx).bands.read(cx).selected_value().copied(), second);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            assert_eq!(panel.read(cx).selected, second, "Scrolling down at the last band must not wrap");
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_eq!(panel.read(cx).selected, original_band);
            for _ in 0..3 {
                window.scroll("raster-band", ScrollDelta::Pixels(point(px(0.0), px(-10.0))), cx);
                assert_eq!(panel.read(cx).selected, original_band, "Small trackpad deltas must accumulate");
            }
            window.scroll("raster-band", ScrollDelta::Pixels(point(px(0.0), px(-10.0))), cx);
            assert_eq!(panel.read(cx).selected, second);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("raster-band", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_ne!(cx.update(|cx| panel.read(cx).selected), original_band);
    }

    fn verify_raster_range_precision(panel: &Entity<RasterPanel>, handle: PanelWindow, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            for (id, value) in [("raster-min", "-1.23"), ("raster-max", "200.45")] {
                window.render_frame(cx);
                window.click(id, cx);
                window.press("secondary-a", cx);
                window.input(value, cx);
            }
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(panel.read(cx).active().unwrap().range(), (-1.23, 200.45));
        });
        for id in ["raster-min", "raster-max"] {
            cx.update_window(handle.into(), |_, window, cx| {
                window.click(id, cx);
                window.press("end", cx);
                window.input("6", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update(|cx| {
                let panel = panel.read(cx);
                assert_eq!(panel.min.read(cx).value().as_str(), "-1.23");
                assert_eq!(panel.max.read(cx).value().as_str(), "200.45");
                assert_eq!(panel.active().unwrap().range(), (-1.23, 200.45));
            });
        }
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("reset-raster-range", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let panel = panel.read(cx);
            for input in [&panel.min, &panel.max] {
                assert_eq!(input.read(cx).value().split_once('.').unwrap().1.len(), 2);
            }
        });
    }

    fn verify_raster_range_validation_and_repeated_commit(panel: &Entity<RasterPanel>, handle: PanelWindow, cx: &mut TestAppContext) {
        let original_id = cx.update(|cx| panel.read(cx).active().unwrap().id());
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("apply-raster-range").is_none());
            window.click("raster-max", cx);
            window.press("secondary-a", cx);
            window.input("0", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(panel.read(cx).error.is_some());
            assert_eq!(panel.read(cx).active().unwrap().id(), original_id);
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.press("secondary-a", cx);
            window.input("200", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(panel.read(cx).error.is_none());
            assert_eq!(panel.read(cx).active().unwrap().range().1, 200.0);
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.click("raster-min", cx);
            window.press("secondary-a", cx);
            window.input("0", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let applied_id = cx.update(|cx| {
            assert!(panel.read(cx).error.is_none());
            assert_ne!(panel.read(cx).active().unwrap().id(), original_id);
            assert_eq!(panel.read(cx).active().unwrap().range().0, 0.0);
            panel.read(cx).active().unwrap().id()
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| panel.read(cx).active().unwrap().id()), applied_id);
    }
}
