use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{InputEvent, InputState, NumberInput};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectDelegate, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, IndexPath, Selectable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement, MouseButton,
    ParentElement, PathPromptOptions, Render, ScrollDelta, ScrollEvent, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt,
    WeakEntity, Window, div, rgba,
};
use inf::colormap::{ColorMapDirection, ColorMapPreset, ProcessedColorMap};

use crate::raster_playback::{PlaybackFrame, PlaybackRequest, PlaybackStyle};
use crate::raster_source::{DEFAULT_EXTRUSION_HEIGHT, RasterSource};
use crate::vector_source::{VectorLoadProgress, VectorSource};

gpui_kit::actions!(raster, [OpenDataset, FitRaster, ClearRasters, ToggleRasterControls]);

#[path = "raster_projection_prompt.rs"]
mod projection_prompt;

const PALETTES: [(&str, ColorMapPreset); 24] = [
    ("Blues", ColorMapPreset::Blues),
    ("BrBG", ColorMapPreset::BrBg),
    ("Cool", ColorMapPreset::Cool),
    ("Copper", ColorMapPreset::Copper),
    ("GistEarth", ColorMapPreset::GistEarth),
    ("Gray", ColorMapPreset::Gray),
    ("Greens", ColorMapPreset::Greens),
    ("Hot", ColorMapPreset::Hot),
    ("Jet", ColorMapPreset::Jet),
    ("Oranges", ColorMapPreset::Oranges),
    ("PiYG", ColorMapPreset::PiYg),
    ("PRGn", ColorMapPreset::PrGn),
    ("PuOr", ColorMapPreset::PuOr),
    ("Purples", ColorMapPreset::Purples),
    ("RdBu", ColorMapPreset::RdBu),
    ("RdGy", ColorMapPreset::RdGy),
    ("RdYlBu", ColorMapPreset::RdYlBu),
    ("RdYlGn", ColorMapPreset::RdYlGn),
    ("Reds", ColorMapPreset::Reds),
    ("Spectral", ColorMapPreset::Spectral),
    ("Terrain", ColorMapPreset::Terrain),
    ("Turbo", ColorMapPreset::Turbo),
    ("YlGnBu", ColorMapPreset::YlGnBu),
    ("YlOrRd", ColorMapPreset::YlOrRd),
];

#[derive(Clone)]
struct PaletteChoice {
    name: &'static str,
    preset: ColorMapPreset,
    direction: ColorMapDirection,
}

struct PaletteChoices(Vec<PaletteChoice>);

fn palette_choices(direction: ColorMapDirection) -> PaletteChoices {
    PaletteChoices(PALETTES.iter().map(|&(name, preset)| PaletteChoice { name, preset, direction }).collect())
}

impl SelectDelegate for PaletteChoices {
    type Item = PaletteChoice;

    fn items_count(&self, section: usize) -> usize {
        self.0.items_count(section)
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        self.0.item(ix)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SelectItem<Value = V>,
        V: PartialEq,
    {
        self.0.position(value)
    }

    fn render_item(&self, _: IndexPath, item: &PaletteChoice, _: bool, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        Some(item.render_preview(cx).into_any_element())
    }
}

impl SelectItem for PaletteChoice {
    type Value = &'static str;

    fn title(&self) -> SharedString {
        self.name.into()
    }

    fn value(&self) -> &Self::Value {
        &self.name
    }
}

impl PaletteChoice {
    fn render_preview(&self, cx: &App) -> impl IntoElement {
        let colors = ProcessedColorMap::<256>::create_for_preset(self.preset, self.direction);
        div()
            .id(SharedString::from(format!("palette-row-{}", self.name)))
            .test_support()
            .flex()
            .items_center()
            .w_full()
            .min_w_0()
            // Mirror the standard row's trailing checkmark lane on the leading side.
            .pl_4()
            .gap_1()
            .child(
                div()
                    .id(SharedString::from(format!("palette-name-{}", self.name)))
                    .test_support()
                    .w_20()
                    .flex_shrink_0()
                    .truncate()
                    .child(self.name),
            )
            .child(
                div()
                    .id(SharedString::from(format!("palette-preview-{}", self.name)))
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .h_6()
                    .rounded(cx.theme().radius_tokens().sm)
                    .overflow_hidden()
                    .child(color_strip((0..=255).map(|ix| colors.get_color_by_value(ix)))),
            )
    }
}

fn initial_palette(window: &mut Window, cx: &mut Context<SelectState<PaletteChoices>>) -> SelectState<PaletteChoices> {
    let items = palette_choices(ColorMapDirection::Regular);
    let selected = items.0.iter().position(|item| item.name == "Turbo").map(IndexPath::new);
    SelectState::new(items, selected, window, cx)
}

fn color_strip(colors: impl IntoIterator<Item = inf::Color>) -> impl IntoElement {
    div().flex().size_full().children(colors.into_iter().map(|color| {
        // These colors encode data, not application chrome.
        div().flex_1().h_full().bg(rgba(u32::from_be_bytes([color.r, color.g, color.b, color.a])))
    }))
}

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

struct RasterDataset {
    path: PathBuf,
    bands: Vec<tiler::LayerId>,
    selected: tiler::LayerId,
    playback: Option<PlaybackRequest>,
}

impl RasterDataset {
    fn id(&self) -> SelectionId {
        SelectionId::Raster(self.bands[0])
    }

    fn title(&self) -> SharedString {
        self.path.file_name().unwrap_or(self.path.as_os_str()).to_string_lossy().into_owned().into()
    }
}

#[derive(Default)]
struct LoadedDatasets {
    opened_paths: Vec<DatasetRequest>,
    missing_projections: Vec<PathBuf>,
    sources: Vec<Arc<RasterSource>>,
    raster_datasets: Vec<RasterDataset>,
    vectors: Vec<Arc<VectorSource>>,
    errors: Vec<String>,
    selected: Option<SelectionId>,
}

struct DatasetRequest {
    path: PathBuf,
    epsg: Option<u32>,
}

#[derive(Default)]
struct ProjectionPrompts {
    paths: VecDeque<PathBuf>,
    open: bool,
    errors: Vec<String>,
}

impl From<PathBuf> for DatasetRequest {
    fn from(path: PathBuf) -> Self {
        Self { path, epsg: None }
    }
}

pub struct RasterEvent {
    playback: Option<PlaybackRequest>,
    source: Option<Arc<RasterSource>>,
    vector_source: Option<Arc<VectorSource>>,
    opacity: f32,
    fit: bool,
}

impl RasterEvent {
    /// Playback configuration for eligible multiband rasters, including paused selections.
    pub fn playback(&self) -> Option<PlaybackRequest> {
        self.playback.clone()
    }

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

struct PlaybackState {
    position: f64,
    revision: u64,
    playing: bool,
    bands_per_second: f64,
    buffering: bool,
}

const PLAYBACK_SPEEDS: [u8; 4] = [1, 2, 3, 5];

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            position: 0.0,
            revision: 0,
            playing: false,
            bands_per_second: 3.0,
            buffering: false,
        }
    }
}

pub struct RasterPanel {
    recent_files: crate::recent_files::RecentFiles,
    sources: Vec<Arc<RasterSource>>,
    raster_datasets: Vec<RasterDataset>,
    source_indices: HashMap<tiler::LayerId, usize>,
    dataset_indices: HashMap<tiler::LayerId, usize>,
    legend_colors: Arc<[inf::Color]>,
    legend_palette: Option<(ColorMapPreset, ColorMapDirection)>,
    vectors: Vec<Arc<VectorSource>>,
    selected: Option<SelectionId>,
    band_scroll: f32,
    palette_scroll: f32,
    bands: Entity<SelectState<Vec<BandChoice>>>,
    palette: Entity<SelectState<PaletteChoices>>,
    property: Entity<SelectState<SearchableVec<SharedString>>>,
    opacity: Entity<SliderState>,
    band_slider: Entity<SliderState>,
    playback: PlaybackState,

    extrusion_height: Entity<SliderState>,
    range_slider: Entity<SliderState>,
    range_domain: (f64, f64),
    min: Entity<InputState>,
    max: Entity<InputState>,
    error: Option<SharedString>,
    load_error: Option<SharedString>,
    loading: bool,
    projection_prompts: ProjectionPrompts,
    load_progress: Option<LoadProgress>,
    load_generation: u64,
    load_started: Option<Instant>,
    collapsed: bool,
    focus: FocusHandle,
    load_task: Option<Task<()>>,
    tile_errors_task: Option<Task<()>>,
    tile_errors_source: Option<u64>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RasterEvent> for RasterPanel {}

impl RasterPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let bands = cx.new(|cx| SelectState::new(Vec::<BandChoice>::new(), None, window, cx));
        let palette = cx.new(|cx| initial_palette(window, cx));
        let (extrusion_height, extrusion_subscription) = Self::extrusion_control(window, cx);
        let property = cx.new(|cx| SelectState::new(SearchableVec::new(Vec::<SharedString>::new()), None, window, cx).searchable(true));
        let opacity = cx.new(|_| SliderState::new().min(0.0).max(100.0).step(1.0).default_value(100.0));
        let band_slider = cx.new(|_| SliderState::new().min(1.0).max(1.0).step(1.0).default_value(1.0));

        // A normalized domain keeps slider stepping useful for tiny or very
        // large data ranges without replacing retained component state.
        let range_slider = cx.new(|_| SliderState::new().min(0.0).max(1000.0).step(1.0).default_value(0.0..1000.0));
        let min = cx.new(|cx| InputState::new(window, cx).placeholder("Minimum"));
        let max = cx.new(|cx| InputState::new(window, cx).placeholder("Maximum"));
        let subscriptions = vec![
            extrusion_subscription,
            cx.subscribe_in(&bands, window, |this, _, event, window, cx| {
                let SelectEvent::Confirm(id) = event;
                this.select_layer(*id, window, cx);
            }),
            cx.subscribe_in(&palette, window, |this, _, _: &SelectEvent<PaletteChoices>, window, cx| {
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
            cx.subscribe_in(&band_slider, window, |this, _, event: &SliderEvent, window, cx| {
                if let SliderEvent::Change(value) = event {
                    this.select_band(value.end().round() as usize, window, cx);
                }
            }),
            cx.subscribe_in(&range_slider, window, |this, _, event: &SliderEvent, window, cx| {
                if let SliderEvent::Change(value) = event {
                    this.apply_slider_range((value.start(), value.end()), window, cx);
                }
            }),
            cx.subscribe_in(&min, window, Self::on_range_input),
            cx.subscribe_in(&max, window, Self::on_range_input),
        ];
        Self {
            recent_files: initial_recent_files(),
            sources: Vec::new(),
            raster_datasets: Vec::new(),
            source_indices: HashMap::new(),
            dataset_indices: HashMap::new(),
            legend_colors: Arc::from([]),
            legend_palette: None,
            vectors: Vec::new(),
            selected: None,
            band_scroll: 0.0,
            palette_scroll: 0.0,
            bands,
            palette,
            property,
            opacity,
            band_slider,
            playback: PlaybackState::default(),

            extrusion_height,
            range_slider,
            range_domain: (0.0, 1.0),
            min,
            max,
            error: None,
            load_error: None,
            loading: false,
            projection_prompts: ProjectionPrompts::default(),
            load_progress: None,
            load_generation: 0,
            load_started: None,
            collapsed: false,
            focus: cx.focus_handle(),
            load_task: None,
            tile_errors_task: None,
            tile_errors_source: None,
            _subscriptions: subscriptions,
        }
    }

    fn active(&self) -> Option<&Arc<RasterSource>> {
        let SelectionId::Raster(id) = self.selected? else { return None };
        self.sources.get(*self.source_indices.get(&id)?)
    }

    /// True when the active raster is a valid COG served by the optimized COG tile provider.
    fn active_is_cog(&self) -> bool {
        self.active()
            .is_some_and(|source| source.metadata().source_format == tiler::LayerSourceType::CloudOptimizedGeoTiff)
    }

    fn active_vector(&self) -> Option<&Arc<VectorSource>> {
        self.vectors.iter().find(|source| Some(SelectionId::Vector(source.id())) == self.selected)
    }

    pub(crate) fn properties(&self) -> crate::dataset_properties::DatasetProperties {
        use crate::dataset_properties::DatasetProperties;
        if let Some(source) = self.active() {
            let dataset = self.active_dataset();
            let path = dataset.map_or(source.metadata().path.as_path(), |dataset| dataset.path.as_path());
            let band_count = dataset.map_or(1, |dataset| dataset.bands.len());
            DatasetProperties::raster(source, path, band_count)
        } else if let Some(source) = self.active_vector() {
            DatasetProperties::vector(source)
        } else {
            DatasetProperties::default()
        }
    }

    fn choices(&self) -> Vec<BandChoice> {
        self.raster_datasets
            .iter()
            .map(|dataset| BandChoice {
                id: dataset.id(),
                title: dataset.title(),
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
            playback: self.playback_request(),
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
        let position = self
            .active_dataset()
            .and_then(|dataset| dataset.bands.iter().position(|&band| Some(SelectionId::Raster(band)) == id));
        self.reset_playback(position.unwrap_or(0) as f64);
        self.sync_band_controls(window, cx);
        self.palette_scroll = 0.0;
        self.error = None;
        self.load_error = None;
        self.collapsed = false;
        self.sync_color_controls(window, cx);
        self.watch_tile_errors(cx);
        self.emit(fit, cx);
    }

    fn active_dataset(&self) -> Option<&RasterDataset> {
        let SelectionId::Raster(id) = self.selected? else { return None };
        self.raster_datasets.get(*self.dataset_indices.get(&id)?)
    }

    fn selected_layer(&self) -> Option<SelectionId> {
        self.active_dataset().map(RasterDataset::id).or(self.selected)
    }

    fn select_layer(&mut self, id: Option<SelectionId>, window: &mut Window, cx: &mut Context<Self>) {
        let id = self
            .raster_datasets
            .iter()
            .find(|dataset| Some(dataset.id()) == id)
            .map(|dataset| SelectionId::Raster(dataset.selected))
            .or(id);
        self.select(id, false, window, cx);
    }

    fn select_band(&mut self, band: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dataset) = self.active_dataset() else { return };
        let Some(&id) = band.checked_sub(1).and_then(|ix| dataset.bands.get(ix)) else {
            return;
        };
        if self.selected == Some(SelectionId::Raster(id)) && self.playback_dataset().is_none() {
            return;
        }
        let collapsed = self.collapsed;
        self.select(Some(SelectionId::Raster(id)), false, window, cx);
        self.collapsed = collapsed;
    }

    fn sync_band_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(SelectionId::Raster(id)) = self.selected else { return };
        let Some(dataset) = self.dataset_indices.get(&id).and_then(|ix| self.raster_datasets.get_mut(*ix)) else {
            return;
        };
        dataset.selected = id;
        let selected = dataset.id();
        let count = dataset.bands.len() as f32;
        let band = dataset.bands.iter().position(|&band| band == id).unwrap_or(0) as f32 + 1.0;
        self.bands.update(cx, |state, cx| state.set_selected_value(&selected, window, cx));
        self.band_slider.update(cx, |slider, cx| {
            if slider.max_value() != count {
                *slider = SliderState::new().min(1.0).max(count).step(1.0).default_value(band);
            }
            slider.set_value(band, window, cx);
        });
    }

    fn install_rasters(&mut self, sources: Vec<Arc<RasterSource>>, datasets: Vec<RasterDataset>) {
        for dataset in datasets {
            let Some(ix) = self.raster_datasets.iter().position(|current| current.path == dataset.path) else {
                self.raster_datasets.push(dataset);
                continue;
            };
            let previous = &self.raster_datasets[ix];
            let replaces_selected = self
                .selected
                .is_some_and(|id| matches!(id, SelectionId::Raster(id) if previous.bands.contains(&id)));
            self.sources.retain(|source| !previous.bands.contains(&source.metadata().id));
            self.raster_datasets[ix] = dataset;
            if replaces_selected {
                self.selected = None;
                self.reset_playback(0.0);
            }
        }
        self.sources.extend(
            sources
                .into_iter()
                .filter(|source| self.raster_datasets.iter().any(|dataset| dataset.bands.contains(&source.metadata().id))),
        );
        self.source_indices = self.sources.iter().enumerate().map(|(ix, source)| (source.metadata().id, ix)).collect();
        self.dataset_indices = self
            .raster_datasets
            .iter()
            .enumerate()
            .flat_map(|(ix, dataset)| dataset.bands.iter().map(move |id| (*id, ix)))
            .collect();
    }

    fn watch_tile_errors(&mut self, cx: &mut Context<Self>) {
        let source = self
            .playback_dataset()
            .and_then(|dataset| dataset.playback.as_ref()?.bands().first())
            .or_else(|| self.active());
        let id = source.map(|source| source.id());
        if self.tile_errors_source == id {
            return;
        }
        let errors = source.map(|source| source.errors());
        self.tile_errors_source = id;
        self.tile_errors_task = errors.map(|errors| cx.spawn(async move |this, cx| receive_tile_errors(this, errors, cx).await));
    }

    fn scroll_bands(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.choices();
        let Some(ix) = items.iter().position(|item| Some(item.id) == self.selected_layer()) else {
            return;
        };
        let next = scroll_index(event, &mut self.band_scroll, ix, items.len(), cx);
        if next != ix {
            let id = items[next].id;
            self.bands.update(cx, |state, cx| state.set_selected_value(&id, window, cx));
            self.select_layer(Some(id), window, cx);
        }
    }

    fn scroll_palette(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.color_range().is_none() {
            return;
        }
        let Some(ix) = PALETTES.iter().position(|(name, _)| Some(name) == self.palette.read(cx).selected_value()) else {
            return;
        };
        let next = scroll_index(event, &mut self.palette_scroll, ix, PALETTES.len(), cx);
        if next != ix {
            self.palette.update(cx, |state, cx| state.set_selected_value(&PALETTES[next].0, window, cx));
            self.apply_style(window, cx);
        }
    }

    pub(crate) fn open_recent_files(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loading {
            crate::recent_files_picker::open(self.recent_files.paths(), cx.weak_entity(), window, cx);
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
        self.projection_prompts = ProjectionPrompts::default();
        let requests = paths
            .into_iter()
            .map(|path| DatasetRequest {
                epsg: self.recent_files.epsg(&path),
                path,
            })
            .collect();
        self.open_requests(requests, cx);
    }

    fn open_requests(&mut self, requests: Vec<DatasetRequest>, cx: &mut Context<Self>) {
        let enqueued = Instant::now();
        log::info!(target: "map::vector_load", "stage=open-request paths={:?}", requests.iter().map(|request| &request.path).collect::<Vec<_>>());
        let (progress_tx, progress_rx) = async_channel::bounded(1);
        let work = cx.background_spawn(async move {
            log::info!(target: "map::vector_load", "stage=loader-worker-start queue_delay={:?}", enqueued.elapsed());
            load_paths(requests, progress_tx)
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
        for request in &loaded.opened_paths {
            if let Err(error) = self.recent_files.record(&request.path, request.epsg) {
                log::warn!("Could not save recent files: {error:#}");
            }
        }
        self.install_rasters(loaded.sources, loaded.raster_datasets);
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
        self.projection_prompts.errors.extend(loaded.errors);
        self.load_error = (!self.projection_prompts.errors.is_empty()).then(|| self.projection_prompts.errors.join("\n").into());
        self.projection_prompts.paths.extend(loaded.missing_projections);
        self.prompt_next_projection(window, cx);
        log::info!(target: "map::vector_load", "load={generation} stage=ui-install-loaded-layers elapsed={:?} since_request={:?} awaiting_vector_frame={}", installed.elapsed(), started.elapsed(), self.load_progress.is_some());
        cx.notify();
    }

    fn prompt_next_projection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.projection_prompts.paths.front() else { return };
        self.loading = true;
        self.load_progress = None;
        self.projection_prompts.open = true;
        projection_prompt::open(path, self.load_generation, cx.weak_entity(), window, cx);
    }

    fn complete_projection_prompt(&mut self, generation: u64, epsg: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        if self.load_generation != generation || !self.projection_prompts.open {
            return;
        }
        self.projection_prompts.open = false;
        self.loading = false;
        let Some(path) = self.projection_prompts.paths.pop_front() else { return };
        if epsg.is_some() {
            self.open_requests(vec![DatasetRequest { path, epsg }], cx);
        } else {
            self.prompt_next_projection(window, cx);
            cx.notify();
        }
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
                let (min, max) = self.playback_style().range_for(source);
                (f64::from(min), f64::from(max))
            })
            .or_else(|| self.active_vector().and_then(|source| source.range()))
    }

    fn data_range(&self) -> Option<(f64, f64)> {
        if let Some(request) = self.playback_dataset().and_then(|dataset| dataset.playback.as_ref()) {
            return request.data_range().map(|(min, max)| (f64::from(min), f64::from(max)));
        }
        self.active()
            .map(|source| {
                let (min, max) = source.data_range();
                (f64::from(min), f64::from(max))
            })
            .or_else(|| self.active_vector().and_then(|source| source.data_range()))
    }

    fn color_palette(&self) -> Option<ColorMapPreset> {
        self.active()
            .map(|source| self.playback_style().palette_for(source))
            .or_else(|| self.active_vector().filter(|source| source.property().is_some()).map(|source| source.palette()))
    }

    fn color_map_direction(&self) -> ColorMapDirection {
        self.active()
            .map(|source| self.playback_style().color_map_direction_for(source))
            .or_else(|| self.active_vector().map(|source| source.color_map_direction()))
            .unwrap_or_default()
    }

    fn set_color_map_reversed(&mut self, reversed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let direction = if reversed { ColorMapDirection::Reversed } else { ColorMapDirection::Regular };
        if self.color_range().is_none() || self.color_map_direction() == direction {
            return;
        }
        let result = if self.playback_dataset().is_some() {
            self.set_playback_style(self.playback_style().with_color_map_direction(direction));
            Ok(())
        } else if let Some(source) = self.active() {
            source.with_color_map_direction(direction).map(|source| self.replace_raster(source))
        } else if let Some(source) = self.active_vector() {
            source.with_color_map_direction(direction).map(|source| self.replace_vector(source))
        } else {
            return;
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.sync_palette(window, cx);
                self.watch_tile_errors(cx);
                self.emit(false, cx);
            }
            Err(error) => {
                self.error = Some(format!("Could not reverse color map: {error}").into());
                cx.notify();
            }
        }
    }

    fn sync_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_legend_colors();
        let direction = self.color_map_direction();
        let preset = self.color_palette().unwrap_or(ColorMapPreset::Turbo);
        let palette = PALETTES.iter().find(|(_, value)| *value == preset).map_or("Turbo", |(name, _)| *name);
        self.palette.update(cx, |state, cx| {
            state.set_items(palette_choices(direction), window, cx);
            state.set_selected_value(&palette, window, cx);
        });
    }

    fn refresh_legend_colors(&mut self) {
        let Some(palette) = self.color_palette() else { return };
        let key = (palette, self.color_map_direction());
        if self.legend_palette == Some(key) {
            return;
        }
        self.legend_colors = if let Some(source) = self.active().filter(|_| self.playback_dataset().is_none()) {
            let range = source.range();
            (0..256)
                .map(|ix| {
                    source
                        .legend()
                        .color_for_value(range_value((f64::from(range.0), f64::from(range.1)), ix as f64 / 255.0) as f32, None)
                })
                .collect::<Vec<_>>()
                .into()
        } else {
            let colors = ProcessedColorMap::<256>::create_for_preset(key.0, key.1);
            (0..=255).map(|ix| colors.get_color_by_value(ix)).collect::<Vec<_>>().into()
        };
        self.legend_palette = Some(key);
    }

    fn apply_slider_range(&mut self, positions: (f32, f32), window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = slider_range(self.range_domain, positions, self.active().is_some()) else {
            self.sync_range_slider(window, cx);
            return;
        };
        self.set_range_inputs(range, window, cx);
        self.apply_style(window, cx);
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
        self.sync_palette(window, cx);
        self.sync_range_slider(window, cx);
        self.sync_extrusion_control(window, cx);
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

    fn playback_style(&self) -> PlaybackStyle {
        self.playback_dataset()
            .and_then(|dataset| dataset.playback.as_ref())
            .map_or(PlaybackStyle::default(), PlaybackRequest::style)
    }

    fn set_playback_style(&mut self, style: PlaybackStyle) {
        let Some(SelectionId::Raster(id)) = self.selected else { return };
        let Some(dataset) = self.dataset_indices.get(&id).and_then(|ix| self.raster_datasets.get_mut(*ix)) else {
            return;
        };
        if let Some(request) = &mut dataset.playback {
            *request = request.clone().with_style(style);
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
        let Some(source) = self.active().cloned() else { return };
        if !source.has_scale_metadata() || self.playback_style().unscaling_for(&source) == enabled {
            return;
        }
        let result = if self.playback_dataset().is_some() {
            self.playback_style_in_units(enabled).map(|style| {
                let range = style.range_for(&source);
                self.set_playback_style(style);
                range
            })
        } else {
            source.with_unscaling(enabled).map(|source| {
                let range = source.range();
                self.replace_raster(source);
                range
            })
        };
        match result {
            Ok(range) => {
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

    fn playback_style_in_units(&self, enabled: bool) -> anyhow::Result<PlaybackStyle> {
        let source = self.active().ok_or_else(|| anyhow::anyhow!("No raster selected"))?;
        let current = self.playback_style();
        let target = current.with_unscaling(enabled);
        let old_scale = current.scale_for(source);
        let scale = target.scale_for(source);
        let range = current.range_for(source);
        let convert = |value: f32| ((f64::from(value) - old_scale.offset) / old_scale.scale * scale.scale + scale.offset) as f32;
        let first = convert(range.0);
        let last = convert(range.1);
        if old_scale.scale != 0.0
            && let Ok(style) = target.with_range((first.min(last), first.max(last)))
        {
            return Ok(style);
        }
        target.with_range(source.data_range_with_scale(scale))
    }

    fn set_clamp_colors(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(source) = self.active() else { return };
        if self.playback_style().clamp_colors_for(source) == enabled {
            return;
        }
        let result = if self.playback_dataset().is_some() {
            self.set_playback_style(self.playback_style().with_clamp_colors(enabled));
            Ok(())
        } else {
            source.with_clamp_colors(enabled).map(|source| self.replace_raster(source))
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.watch_tile_errors(cx);
                self.emit(false, cx);
            }
            Err(error) => {
                self.error = Some(format!("Could not change color clamping: {error}").into());
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
        self.reset_playback(0.0);
        self.load_generation += 1;
        self.load_task = None;
        self.projection_prompts.paths.clear();
        if self.projection_prompts.open {
            self.projection_prompts.open = false;
            window.close_dialog(cx);
        }
        self.tile_errors_task = None;
        self.tile_errors_source = None;
        self.loading = false;
        self.load_progress = None;
        self.sources.clear();
        self.raster_datasets.clear();
        self.source_indices.clear();
        self.dataset_indices.clear();
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
        PALETTES
            .iter()
            .find(|(name, _)| Some(name) == self.palette.read(cx).selected_value())
            .map_or(ColorMapPreset::Turbo, |(_, preset)| *preset)
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
        let min = self.min.read(cx).value().trim().parse::<f32>()?;
        let max = self.max.read(cx).value().trim().parse::<f32>()?;
        if self.playback_dataset().is_some() {
            let style = self.playback_style().with_range((min, max))?.with_palette(palette);
            self.set_playback_style(style);
        } else if let Some(source) = self.active()
            && (source.palette() != palette || source.range() != (min, max))
        {
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
                self.refresh_legend_colors();
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
    fn playback_dataset(&self) -> Option<&RasterDataset> {
        self.active_dataset().filter(|dataset| dataset.playback.is_some())
    }

    /// Current playback command, with each band's retained style. The renderer owns the clock.
    /// Style snapshots refresh on every emit without changing the command revision.
    pub fn playback_request(&self) -> Option<PlaybackRequest> {
        let request = self.playback_dataset()?.playback.as_ref()?;
        Some(request.clone().with_controls(
            self.playback.position,
            self.playback.revision,
            self.playback.playing,
            self.playback.bands_per_second,
        ))
    }

    fn revise_playback(&mut self) {
        self.playback.revision = self.playback.revision.wrapping_add(1);
        self.playback.buffering = false;
    }

    fn reset_playback(&mut self, position: f64) {
        self.playback.position = position;
        self.playback.playing = false;
        self.revise_playback();
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if self.playback_dataset().is_none() {
            return;
        }
        self.playback.playing = !self.playback.playing;
        self.revise_playback();
        self.emit(false, cx);
    }

    fn set_playback_speed(&mut self, speed: u8, cx: &mut Context<Self>) {
        if self.playback_dataset().is_none() || !PLAYBACK_SPEEDS.contains(&speed) {
            return;
        }
        let speed = f64::from(speed);
        if self.playback.bands_per_second == speed {
            return;
        }
        self.playback.bands_per_second = speed;
        self.revise_playback();
        self.emit(false, cx);
    }

    fn sync_playback_color_controls(&mut self, previous: Option<&RasterSource>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.active() else { return };
        let style = self.playback_style();
        let range = style.range_for(source);
        let palette_changed = previous.is_none_or(|previous| {
            style.palette_for(previous) != style.palette_for(source) || style.color_map_direction_for(previous) != style.color_map_direction_for(source)
        });
        let range_changed = previous.is_none_or(|previous| style.range_for(previous) != range);
        let editing_range = [&self.min, &self.max].iter().any(|input| input.read(cx).focus_handle(cx).is_focused(window));
        if palette_changed {
            self.sync_palette(window, cx);
        }
        // Feedback must not replace unfinished text or rebuild an unchanged palette popup.
        if range_changed && !editing_range {
            self.set_range_inputs((f64::from(range.0), f64::from(range.1)), window, cx);
        }
        self.sync_range_slider(window, cx);
        self.sync_extrusion_control(window, cx);
    }

    /// Accept renderer feedback without emitting a new command or changing its revision.
    /// Frames from a replaced dataset or an earlier user command are ignored.
    pub fn apply_playback_frame(&mut self, frame: PlaybackFrame, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dataset) = self.playback_dataset() else { return };
        if dataset.bands.first().copied() != Some(frame.dataset_id()) || self.playback.revision != frame.revision() {
            return;
        }
        let position = frame.position();
        if !position.is_finite() || !(0.0..dataset.bands.len() as f64).contains(&position) {
            return;
        }
        let slider_position = (position + 1.0).min(dataset.bands.len() as f64) as f32;
        let selected = Some(SelectionId::Raster(dataset.bands[position.floor() as usize]));
        let changed_band = self.selected != selected;
        let previous_style = self.active().cloned();
        self.playback.position = position;
        self.playback.buffering = frame.is_buffering();
        if changed_band {
            self.selected = selected;
            self.sync_band_controls(window, cx);
            self.sync_playback_color_controls(previous_style.as_deref(), window, cx);
            self.watch_tile_errors(cx);
        }
        // The final-to-first blend has no tick beyond the last band. Retain its
        // fraction in playback state, but keep the slider inside its domain.
        // SliderState::set_value is an owner sync, not a SliderEvent::Change.
        self.band_slider.update(cx, |slider, cx| slider.set_value(slider_position, window, cx));
        cx.notify();
    }

    fn render_playback_controls(&self, cx: &Context<Self>) -> Option<impl IntoElement + use<>> {
        let dataset = self.playback_dataset()?;
        let count = dataset.bands.len();
        let status = if self.playback.buffering {
            "Buffering"
        } else if self.playback.playing {
            "Playing"
        } else {
            "Paused"
        };
        Some(
            div()
                .id("raster-playback-controls")
                .test_support()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("first-raster-band")
                                .small()
                                .ghost()
                                .icon(IconName::SkipBack)
                                .accessibility_label("First band")
                                .tooltip("First band")
                                .disabled(self.playback.position == 0.0)
                                .on_click(cx.listener(|this, _, window, cx| this.select_band(1, window, cx))),
                        )
                        .child(
                            Button::new("toggle-raster-playback")
                                .small()
                                .icon(if self.playback.playing { IconName::Pause } else { IconName::Play })
                                .accessibility_label(if self.playback.playing { "Pause" } else { "Play" })
                                .tooltip(if self.playback.playing { "Pause playback" } else { "Play bands" })
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_playback(cx))),
                        )
                        .child(
                            Button::new("last-raster-band")
                                .small()
                                .ghost()
                                .icon(IconName::SkipForward)
                                .accessibility_label("Last band")
                                .tooltip("Last band")
                                .disabled(self.playback.position == (count - 1) as f64)
                                .on_click(cx.listener(move |this, _, window, cx| this.select_band(count, window, cx))),
                        )
                        .child(
                            div()
                                .id("raster-playback-status")
                                .role(gpui_kit::Role::Status)
                                .test_support()
                                .aria_label(status)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(status),
                        ),
                )
                .child(self.render_playback_options(cx)),
        )
    }

    fn render_playback_options(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        div().flex().flex_col().gap_2().child(
            div().flex().items_center().gap_3().child(field_label("Speed (bands/sec)", cx)).child(
                div().id("raster-playback-speed").test_support().flex_1().min_w_0().child(
                    ButtonGroup::new("raster-playback-speed-presets")
                        .small()
                        .w_full()
                        .children(PLAYBACK_SPEEDS.into_iter().map(|speed| {
                            Button::new(("raster-playback-speed", usize::from(speed)))
                                .label(speed.to_string())
                                .accessibility_label(format!("{speed} bands per second"))
                                .selected(self.playback.bands_per_second == f64::from(speed))
                                .flex_1()
                                .on_click(cx.listener(move |this, _, _, cx| this.set_playback_speed(speed, cx)))
                        })),
                ),
            ),
        )
    }

    fn render_band_selector(&self, cx: &Context<Self>) -> Option<impl IntoElement + use<>> {
        let dataset = self.active_dataset().filter(|dataset| dataset.bands.len() > 1)?;
        let count = dataset.bands.len();
        let band = dataset.bands.iter().position(|&id| id == dataset.selected)? + 1;
        let metadata = self.active()?.metadata();
        let name = if metadata.source_format == tiler::LayerSourceType::Netcdf && metadata.description.is_empty() {
            metadata.name.as_str()
        } else {
            metadata.description.as_str()
        };
        let label = band_title(name, band, count);
        let tooltip_label = label.clone();
        Some(
            overlay_card("raster-band-controls", cx)
                .w_full()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(Icon::new(IconName::Layers).small())
                        .child(
                            div().id("raster-band-name").test_support().flex_1().min_w_0().child(
                                div()
                                    .id("raster-band-label")
                                    .test_support()
                                    .aria_label(label.clone())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when(label.chars().count() > 32, |title| title.text_sm())
                                    .when(label.chars().count() > 64, |title| title.text_xs())
                                    .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
                                    .truncate()
                                    .child(label),
                            ),
                        )
                        .children((!name.trim().is_empty()).then(|| {
                            div()
                                .id("raster-band-number")
                                .test_support()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(band.to_string())
                        })),
                )
                .child(self.render_band_navigation(band, count, cx))
                .child(render_band_ticks(count, cx))
                .children(self.render_playback_controls(cx)),
        )
    }

    fn render_band_navigation(&self, band: usize, count: usize, cx: &Context<Self>) -> impl IntoElement + use<> {
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                Button::new("previous-raster-band")
                    .small()
                    .ghost()
                    .icon(IconName::ChevronLeft)
                    .accessibility_label("Previous band")
                    .disabled(band == 1)
                    .on_click(cx.listener(move |this, _, window, cx| this.select_band(band - 1, window, cx))),
            )
            .child(
                div()
                    .id("raster-band-slider")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .child(Slider::new(&self.band_slider).accessibility_label("Band")),
            )
            .child(
                Button::new("next-raster-band")
                    .small()
                    .ghost()
                    .icon(IconName::ChevronRight)
                    .accessibility_label("Next band")
                    .disabled(band == count)
                    .on_click(cx.listener(move |this, _, window, cx| this.select_band(band + 1, window, cx))),
            )
    }

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
            .choices()
            .iter()
            .map(|choice| measure(&choice.title))
            .reduce(gpui_kit::Pixels::max)
            .unwrap_or_else(|| measure(PLACEHOLDER));
        // Match small Select padding, its rem-based caret + gap, and physical borders.
        let chrome_width = gpui_kit::component::Size::Small.input_px() * 2.0 + rem * 1.125 + gpui_kit::px(2.0);
        let viewport_width = window.viewport_size().width;
        let is_cog = self.active_is_cog();
        // Reserve the background selector's 24% width cap, a drag region,
        // Open, the COG tag, trailing actions, and native window controls.
        let reserved = rem * if is_cog { 17.0 } else { 14.0 };
        let max_width = (viewport_width * 0.5).min((viewport_width * 0.76 - reserved).max(gpui_kit::Pixels::ZERO));

        let selector = div()
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
            );

        div()
            .flex()
            .items_center()
            .gap_1p5()
            .min_w_0()
            .flex_shrink_0()
            .child(selector)
            .when(is_cog, |row| {
                row.child(
                    div()
                        .id("raster-cog-tag")
                        .role(gpui_kit::Role::Status)
                        .aria_label("Cloud Optimized GeoTIFF")
                        .test_support()
                        .flex_shrink_0()
                        .child(Tag::success().outline().xsmall().child("COG")),
                )
            })
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
                .child(Icon::new(IconName::Palette).small())
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
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div().flex().items_center().justify_between().gap_2().child(field_label("Color map", cx)).child(
                    Checkbox::new("raster-palette-reverse")
                        .small()
                        .label("Reverse")
                        .checked(self.color_map_direction() == ColorMapDirection::Reversed)
                        .tooltip("Reverse the color map")
                        .on_change(cx.listener(|this, reversed, window, cx| this.set_color_map_reversed(*reversed, window, cx))),
                ),
            )
            .child(
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
            panel = panel.child(self.render_value_range(cx)).children(self.render_visualization(cx));
        }
        panel = panel.children(self.render_band_selector(cx));
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
    fn extrusion_control(window: &mut Window, cx: &mut Context<Self>) -> (Entity<SliderState>, Subscription) {
        let height = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(crate::raster_source::MAX_EXTRUSION_HEIGHT)
                .step(0.2)
                .default_value(DEFAULT_EXTRUSION_HEIGHT)
        });
        let subscription = cx.subscribe_in(&height, window, |this, _, event: &SliderEvent, _, cx| {
            if let SliderEvent::Change(value) = event
                && this.extrusion().0
            {
                this.set_extrusion(true, value.end(), cx);
            }
        });
        (height, subscription)
    }

    fn extrusion(&self) -> (bool, f32) {
        self.active()
            .map_or((false, DEFAULT_EXTRUSION_HEIGHT), |source| self.playback_style().extrusion_for(source))
    }

    fn sync_extrusion_control(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (_, height) = self.extrusion();
        self.extrusion_height.update(cx, |slider, cx| slider.set_value(height, window, cx));
    }

    fn set_extrusion(&mut self, enabled: bool, height: f32, cx: &mut Context<Self>) {
        let Some(source) = self.active().filter(|source| source.is_numeric()) else {
            return;
        };
        if self.extrusion() == (enabled, height) {
            return;
        }
        let result = if self.playback_dataset().is_some() {
            self.set_playback_style(self.playback_style().with_extrusion(enabled, height));
            Ok(())
        } else {
            source.with_extrusion(enabled, height).map(|source| self.replace_raster(source))
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.watch_tile_errors(cx);
                self.emit(false, cx);
            }
            Err(error) => {
                self.error = Some(format!("Could not change pixel extrusion: {error}").into());
                cx.notify();
            }
        }
    }

    fn set_extrusion_averaging(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(source) = self.active().filter(|source| source.is_numeric()) else {
            return;
        };
        if self.playback_style().extrusion_averaging_for(source) == enabled {
            return;
        }
        let result = if self.playback_dataset().is_some() {
            self.set_playback_style(self.playback_style().with_extrusion_averaging(enabled));
            Ok(())
        } else {
            source.with_extrusion_averaging(enabled).map(|source| self.replace_raster(source))
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.watch_tile_errors(cx);
                self.emit(false, cx);
            }
            Err(error) => {
                self.error = Some(format!("Could not change extrusion averaging: {error}").into());
                cx.notify();
            }
        }
    }

    fn render_extrusion_averaging(&self, extrusion_enabled: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let averaged = self.active().is_some_and(|source| self.playback_style().extrusion_averaging_for(source));
        let description = "Reduce resolution";
        Button::new("raster-average-extrusion-pixels")
            .small()
            .ghost()
            .icon(IconName::Grid2x2)
            .selected(averaged)
            .toggled(averaged)
            .disabled(!extrusion_enabled)
            .tooltip(description)
            .accessibility_label(description)
            .on_click(cx.listener(move |this, _, _, cx| this.set_extrusion_averaging(!averaged, cx)))
    }

    fn render_visualization(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.active().filter(|source| source.is_numeric())?;
        let (enabled, height) = self.extrusion();
        Some(
            overlay_card("raster-visualization-card", cx)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(IconName::ChartNoAxesColumn).small())
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("Visualization")),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            Switch::new("raster-extrude-pixels")
                                .small()
                                .label("Extrude pixels")
                                .checked(enabled)
                                .on_change(cx.listener(|this, enabled, _, cx| {
                                    this.set_extrusion(*enabled, this.extrusion().1, cx);
                                })),
                        )
                        .child(self.render_extrusion_averaging(enabled, cx)),
                )
                .child(
                    div().flex().justify_between().child(field_label("Maximum height", cx)).child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{:.1} × pixel width", height * crate::raster_source::EXTRUSION_HEIGHT_FACTOR)),
                    ),
                )
                .child(
                    div().id("raster-extrusion-height").test_support().px_2().child(
                        Slider::new(&self.extrusion_height)
                            .disabled(!enabled)
                            .accessibility_label("Maximum extrusion height"),
                    ),
                ),
        )
    }

    fn render_legend(&self, cx: &App) -> impl IntoElement {
        let mut legend = div().id("raster-legend").test_support().flex().flex_col().gap_1();
        if let Some(((min, max), _)) = self.color_range().zip(self.color_palette()) {
            legend = legend
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .children((0..5).map(|ix| format_value(range_value((min, max), ix as f64 / 4.0)))),
                )
                .child(self.render_legend_swatches(cx));
        }
        legend
    }

    fn render_legend_swatches(&self, cx: &App) -> impl IntoElement {
        let swatches = self.legend_colors.iter().copied();
        div()
            .flex()
            .w_full()
            .h_3()
            .rounded(cx.theme().radius_tokens().sm)
            .overflow_hidden()
            .child(color_strip(swatches))
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
            .children(self.active().filter(|source| source.is_numeric()).map(|source| {
                Checkbox::new("raster-clamp-colors")
                    .small()
                    .label("Clamp colors")
                    .checked(self.playback_style().clamp_colors_for(source))
                    .tooltip("Use endpoint colors outside the range; when off, render those values as nodata")
                    .on_change(cx.listener(|this, enabled, _, cx| this.set_clamp_colors(*enabled, cx)))
            }))
            .children(self.active().filter(|source| source.has_scale_metadata()).map(|source| {
                Checkbox::new("raster-unscaling")
                    .small()
                    .label("Descale values")
                    .checked(self.playback_style().unscaling_for(source))
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

fn initial_recent_files() -> crate::recent_files::RecentFiles {
    // Headless tests must never read or overwrite the user's history.
    if cfg!(test) {
        crate::recent_files::RecentFiles::default()
    } else {
        crate::recent_files::RecentFiles::load()
    }
}

fn load_paths(paths: Vec<DatasetRequest>, progress: async_channel::Sender<LoadProgress>) -> LoadedDatasets {
    let mut loaded = LoadedDatasets::default();
    let path_count = paths.len() as f64;

    for (index, path) in paths.into_iter().enumerate() {
        load_path(path, index, path_count, &progress, &mut loaded);
    }
    for error in &loaded.errors {
        log::error!("Dataset loading failed: {error}");
    }
    loaded
}

fn load_path(request: DatasetRequest, index: usize, path_count: f64, progress: &async_channel::Sender<LoadProgress>, loaded: &mut LoadedDatasets) {
    let DatasetRequest { path, epsg } = request;
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
            loaded.opened_paths.push(path.clone().into());
        }
        Ok(None) => load_raster_path(&path, epsg, loaded),
        Err(error) => loaded.errors.push(format!("{}: {error:#}", path.display())),
    }
    log::info!(target: "map::vector_load", "path={path:?} stage=path-loaded elapsed={:?}", opened.elapsed());
}

fn load_raster_path(path: &std::path::Path, epsg: Option<u32>, loaded: &mut LoadedDatasets) {
    let result = match epsg {
        Some(code) => RasterSource::open_with_epsg(path, Some(code)),
        None => RasterSource::open(path),
    };
    match result {
        Ok(opened) => {
            if let Some(first) = opened.first() {
                loaded.raster_datasets.push(RasterDataset {
                    path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
                    bands: opened.iter().map(|source| source.metadata().id).collect(),
                    selected: first.metadata().id,
                    playback: PlaybackRequest::for_dataset(&opened),
                });
                loaded.selected = Some(SelectionId::Raster(first.metadata().id));
            }
            loaded.sources.extend(opened);
            loaded.opened_paths.push(DatasetRequest {
                path: path.to_path_buf(),
                epsg,
            });
        }
        Err(error) if error.is::<crate::raster_source::MissingProjection>() => loaded.missing_projections.push(path.to_path_buf()),
        Err(error) => loaded.errors.push(format!("{}: {error:#}", path.display())),
    }
}

pub(crate) fn scroll_index(event: &ScrollEvent, scroll: &mut f32, ix: usize, count: usize, cx: &mut App) -> usize {
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

fn band_title(name: &str, band: usize, count: usize) -> String {
    let name = name.trim();
    if name.is_empty() {
        format!("Band {band}/{count}")
    } else {
        format!("Band {name}")
    }
}

fn band_tick_values(count: usize) -> Vec<usize> {
    let intervals = (count - 1).min(12);
    (0..=intervals).map(|tick| 1 + tick * (count - 1) / intervals).collect()
}

fn render_band_ticks(count: usize, cx: &App) -> impl IntoElement + use<> {
    div()
        .id("raster-band-ticks")
        .test_support()
        .relative()
        // Match the small navigation buttons, row gaps, and slider's thumb inset.
        .mx_11()
        .mt_neg_3()
        .h_6()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .children(band_tick_values(count).into_iter().enumerate().map(|(tick, band)| {
            let major = tick % 2 == 0 || count <= 7 || band == count;
            div()
                .id(("raster-band-tick", band))
                .test_support()
                .aria_label(band.to_string())
                .absolute()
                .left(gpui_kit::relative((band - 1) as f32 / (count - 1) as f32))
                .ml_neg_4()
                .w_8()
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .border_l_1()
                        .border_color(cx.theme().muted_foreground)
                        .h(gpui_kit::rems(if major { 0.375 } else { 0.25 })),
                )
                .children(major.then(|| band.to_string()))
        }))
}

fn overlay_card(id: &'static str, cx: &App) -> impl IntoElement + ParentElement + Styled + InteractiveElement + use<> {
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

fn slider_range(domain: (f64, f64), positions: (f32, f32), raster: bool) -> Option<(f64, f64)> {
    let mut min = range_value(domain, f64::from(positions.0) / 1000.0);
    let mut max = range_value(domain, f64::from(positions.1) / 1000.0);
    if raster {
        // Validate the same precision used by the inputs and raster styling.
        min = f64::from(format!("{min:.2}").parse::<f32>().ok()?);
        max = f64::from(format!("{max:.2}").parse::<f32>().ok()?);
    }
    (min.is_finite() && max.is_finite() && min < max).then_some((min, max))
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
#[path = "raster_panel_playback_tests.rs"]
mod playback_tests;

#[cfg(test)]
#[path = "raster_panel_progress_tests.rs"]
pub(crate) mod progress_tests;

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, point, px, size};

    use super::*;

    #[gpui_kit::test]
    fn band_title_uses_band_metadata_in_the_rendered_selector(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let directory = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&directory, &[3]);
        std::fs::write(
            paths[0].with_extension("tif.aux.xml"),
            "<PAMDataset><PAMRasterBand band=\"1\"><Description>Near infrared</Description></PAMRasterBand></PAMDataset>",
        )
        .unwrap();
        let (panel, handle) = open_test_panel(cx, 760.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("raster-band-label").label(), Some("Band Near infrared"));
            assert!(window.find("raster-band-number").visible());
        })
        .unwrap();
        click_band_slider(&panel, handle, 0.5, 2, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("raster-band-label").label(), Some("Band 2/3"));
            assert!(window.try_find("raster-band-number").is_none());
        })
        .unwrap();
    }

    #[test]
    fn band_titles_prefer_metadata_and_fall_back_to_one_based_indices() {
        assert_eq!(band_title("Near infrared", 2, 5), "Band Near infrared");
        assert_eq!(band_title("  ", 2, 5), "Band 2/5");
        assert_eq!(band_title("", 1, 5), "Band 1/5");
    }

    #[test]
    fn band_ticks_include_endpoints_and_bound_large_scales() {
        assert_eq!(band_tick_values(2), [1, 2]);
        assert_eq!(band_tick_values(5), [1, 2, 3, 4, 5]);
        let ticks = band_tick_values(1000);
        assert_eq!(ticks.len(), 13);
        assert_eq!(ticks.first(), Some(&1));
        assert_eq!(ticks.last(), Some(&1000));
        assert!(ticks.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn palette_names_are_unique_and_alphabetically_sorted() {
        assert!(PALETTES.windows(2).all(|pair| pair[0].0.to_ascii_lowercase() < pair[1].0.to_ascii_lowercase()));
    }

    #[gpui_kit::test]
    fn every_palette_can_be_scrolled_applied_and_restored(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[1]);
        let (panel, handle) = open_test_panel(cx, 760.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).palette.read(cx).selected_value(), Some(&"Turbo"));
            for _ in 0..PALETTES.len() {
                window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            }
            for (name, preset) in PALETTES {
                assert_eq!(panel.read(cx).active().unwrap().palette(), preset);
                panel.update(cx, |panel, cx| {
                    panel.palette.update(cx, |state, cx| state.set_selected_index(None, window, cx));
                    panel.sync_color_controls(window, cx);
                });
                assert_eq!(panel.read(cx).palette.read(cx).selected_value(), Some(&name));
                window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn palette_previews_preserve_pointer_selection_keyboard_navigation_and_focus(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[1]);
        let (panel, handle) = open_test_panel(cx, 760.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("raster-palette").value(), Some("Turbo"));
            window.click("raster-palette", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let preview = window.find("palette-preview-Terrain");
            assert!(preview.visible());
            assert!(preview.bounds().size.width > px(0.0));
            assert!(preview.bounds().size.height > px(0.0));
            window.click("palette-preview-Terrain", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).active().unwrap().palette(), ColorMapPreset::Terrain);
            assert_eq!(window.find("raster-palette").value(), Some("Terrain"));
            assert_eq!(window.find("raster-palette").focused(), Some(true));
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).active().unwrap().palette(), ColorMapPreset::Turbo);
            assert_eq!(window.find("raster-palette").value(), Some("Turbo"));
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("palette-preview-Turbo").visible());
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("raster-palette").expanded(), Some(false));
            assert_eq!(window.find("raster-palette").focused(), Some(true));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn palette_preview_columns_align_and_outer_margins_match_at_different_font_sizes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[1]);
        let (panel, handle) = open_test_panel(cx, 900.0);
        load_test_paths(&panel, handle, paths, cx);
        for font_size in [16.0, 20.0] {
            cx.update(|cx| gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(font_size)));
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                window.click("raster-palette", cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                assert_palette_preview_alignment(window);
                window.press("escape", cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
    }

    fn assert_palette_preview_alignment(window: &Window) {
        let control = window.find("raster-palette").bounds();
        let reference = window.find("palette-preview-Spectral").bounds();
        let name_left = window.find("palette-name-Spectral").bounds().left();
        for name in ["RdYlBu", "RdYlGn", "Reds", "Spectral"] {
            let preview = window.find(SharedString::from(format!("palette-preview-{name}")));
            let label = window.find(SharedString::from(format!("palette-name-{name}")));
            assert!(preview.visible(), "{name}: preview={:?}, control={control:?}", preview.bounds());
            assert!(label.visible(), "{name}");
            assert_eq!(preview.bounds().left(), reference.left());
            assert_eq!(preview.bounds().right(), reference.right());
            assert_eq!(label.bounds().left(), name_left);
            let left_margin = label.bounds().left() - control.left();
            let right_margin = control.right() - preview.bounds().right();
            assert!(
                (left_margin - right_margin).abs() <= px(1.0),
                "{name}: left={left_margin:?}, right={right_margin:?}"
            );
        }
    }

    #[gpui_kit::test]
    fn reverse_color_map_preserves_raster_ranges_and_per_band_state_through_the_ui(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![per_band_style_test_path(&dir, 2)];
        let (panel, handle) = open_test_panel(cx, 1000.0);
        load_test_paths(&panel, handle, paths, cx);
        let events = record_events(&panel, cx);
        let original = cx.update(|cx| panel.read(cx).active().unwrap().clone());
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("raster-palette-reverse").label(), Some("Reverse"));
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(false));
            assert!(window.find("raster-palette-reverse").bounds().bottom() <= window.find("raster-palette").bounds().top());
            window.click("raster-palette-reverse", cx);
            let reversed = panel.read(cx).active().unwrap().clone();
            assert!(reversed.color_map_direction() == ColorMapDirection::Reversed);
            assert_eq!(reversed.metadata().id, original.metadata().id);
            assert_ne!(reversed.id(), original.id());
            assert_eq!(reversed.range(), original.range());
            assert_eq!(reversed.palette(), original.palette());
            assert_eq!(&reversed.palette_rgba()[..4], &original.palette_rgba()[1020..]);
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(true));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            toggle_reverse_checkbox_with_keyboard(window, cx);
            assert_eq!(panel.read(cx).active().unwrap().palette_rgba(), original.palette_rgba());
            window.press("space", cx);
            window.click("next-raster-band", cx);
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(false));
            assert!(panel.read(cx).color_map_direction() == ColorMapDirection::Regular);
            window.click("previous-raster-band", cx);
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(true));
            assert!(panel.read(cx).color_map_direction() == ColorMapDirection::Reversed);
            window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(panel.read(cx).color_map_direction() == ColorMapDirection::Reversed);
            assert_eq!(window.find("raster-palette").value(), Some("YlGnBu"));
            window.click("raster-palette", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("palette-preview-YlGnBu").visible());
            window.press("escape", cx);
            assert!(events.read(cx).iter().all(|event| !event.should_fit()));
            assert!(
                events
                    .read(cx)
                    .iter()
                    .any(|event| { event.source().is_some_and(|source| source.color_map_direction() == ColorMapDirection::Reversed) })
            );
        })
        .unwrap();
    }

    fn toggle_reverse_checkbox_with_keyboard(window: &mut Window, cx: &mut App) {
        window.click("raster-min", cx);
        for _ in 0..20 {
            if window.find("raster-palette-reverse").focused() == Some(true) {
                break;
            }

            window.press("tab", cx);
        }
        assert_eq!(window.find("raster-palette-reverse").focused(), Some(true));
        window.press("space", cx);
        assert_eq!(window.find("raster-palette-reverse").checked(), Some(false));
    }

    #[gpui_kit::test]
    fn reverse_color_map_preserves_numeric_vector_properties_and_per_layer_state_through_the_ui(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = numeric_vector_paths(&dir);
        let (panel, handle) = open_test_panel(cx, 900.0);
        load_test_paths(&panel, handle, paths, cx);
        let events = record_events(&panel, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let original = panel.read(cx).active_vector().unwrap().clone();
            window.click("raster-palette-reverse", cx);
            let reversed = panel.read(cx).active_vector().unwrap();
            assert!(reversed.color_map_direction() == ColorMapDirection::Reversed);
            assert_eq!(reversed.id(), original.id());
            assert_ne!(reversed.style_id(), original.style_id());
            assert_eq!(reversed.range(), original.range());
            assert_eq!(reversed.property(), original.property());
            assert_eq!(reversed.source_json(), original.source_json());
            window.click("vector-property", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).active_vector().unwrap().property(), Some("b_fraction"));
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(true));
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(false));
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert!(window.try_find("raster-palette-reverse").is_none());
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(false));
            for _ in 0..3 {
                window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            }
            assert!(panel.read(cx).color_map_direction() == ColorMapDirection::Reversed);
            assert_eq!(window.find("raster-palette-reverse").checked(), Some(true));
            assert_eq!(panel.read(cx).active_vector().unwrap().property(), Some("b_fraction"));
            toggle_reverse_checkbox_with_keyboard(window, cx);
            assert!(panel.read(cx).color_map_direction() == ColorMapDirection::Regular);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(events.read(cx).iter().all(|event| !event.should_fit()));
            assert!(events.read(cx).iter().any(|event| {
                event
                    .vector_source()
                    .is_some_and(|source| source.color_map_direction() == ColorMapDirection::Reversed)
            }));
        });
    }

    struct PanelHarness(Entity<RasterPanel>);

    impl Render for PanelHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let selector = self.0.update(cx, |panel, cx| panel.render_layer_selector(window, cx));

            div().flex().flex_col().size_full().child(selector).child(
                div()
                    .id("test-map-area")
                    .test_support()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(div().absolute().top_0().right_0().bottom_0().w_80().child(self.0.clone())),
            )
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
                    playback: event.playback(),
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

    /// 1-based position of the selected band within its file, independent of
    /// how the format numbers bands (multi-variable NetCDF grids are all band 1).
    fn selected_band(panel: &RasterPanel) -> Option<usize> {
        let dataset = panel.active_dataset()?;
        let position = dataset.bands.iter().position(|&id| panel.selected == Some(SelectionId::Raster(id)))?;
        Some(position + 1)
    }

    fn raster_test_paths(directory: &tempfile::TempDir, band_counts: &[usize]) -> Vec<PathBuf> {
        band_counts
            .iter()
            .enumerate()
            .map(|(index, &bands)| {
                let path = directory.path().join(format!("raster-{index}.tif"));
                crate::raster_source::test_support::write_raster(&path, 4326, bands).unwrap();
                path
            })
            .collect()
    }

    /// Multiband rasters from one file play back with a dataset-wide style.
    /// Multi-variable NetCDF files keep independent per-band styles.
    fn per_band_style_test_path(directory: &tempfile::TempDir, bands: usize) -> PathBuf {
        let path = directory.path().join("variables.nc");
        crate::raster_source::test_support::write_multivariable_netcdf(&path, 4326, bands).unwrap();
        path
    }

    fn load_test_paths(panel: &Entity<RasterPanel>, handle: PanelWindow, paths: Vec<PathBuf>, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, _, cx| panel.update(cx, |panel, cx| panel.open_paths(paths, cx)))
            .unwrap();
        cx.run_until_parked();
    }

    fn click_band_slider(panel: &Entity<RasterPanel>, handle: PanelWindow, position: f32, band: usize, cx: &mut TestAppContext) {
        let file = cx.update(|cx| panel.read(cx).bands.read(cx).selected_value().copied());
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let slider = window.find("raster-band-slider").bounds();
            window.click_at("raster-band-slider", point(slider.size.width * position, slider.size.height * 0.5), cx);
        })
        .unwrap();
        // Slider subscriptions run after the borrowed window update finishes.
        cx.run_until_parked();
        cx.update(|cx| assert_band_selection(panel, band, file, cx));
    }

    fn band_slider_snapshot(window: &mut Window) -> gpui_kit::test::ElementSnapshot {
        let mut sliders = window.within("raster-band-slider").find_all_by_role(gpui_kit::Role::Slider);
        assert_eq!(sliders.len(), 1, "The band selector must contain one native Slider");
        sliders.remove(0)
    }

    fn assert_band_selection(panel: &Entity<RasterPanel>, band: usize, file: Option<SelectionId>, cx: &App) {
        let panel = panel.read(cx);
        let source = panel.active().unwrap();
        assert_eq!(selected_band(panel), Some(band));
        assert_eq!(panel.selected, Some(SelectionId::Raster(source.metadata().id)));
        assert_eq!(
            panel.bands.read(cx).selected_value().copied(),
            file,
            "The title must retain the file's first-band identity"
        );
    }

    fn press_panel_key(handle: PanelWindow, key: &str, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press(key, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn focus_band_button(id: &'static str, handle: PanelWindow, cx: &mut TestAppContext) {
        for _ in 0..40 {
            let focused = cx
                .update_window(handle.into(), |_, window, cx| {
                    window.render_frame(cx);
                    assert_eq!(window.find(id).role(), Some(gpui_kit::Role::Button));
                    window.find(id).focused() == Some(true)
                })
                .unwrap();
            if focused {
                return;
            }
            press_panel_key(handle, "tab", cx);
        }
        panic!("Band navigation button {id} must be reachable with Tab");
    }

    fn assert_band_selector_absent(window: &Window) {
        for id in ["raster-band-slider", "raster-band-label", "previous-raster-band", "next-raster-band"] {
            assert!(window.try_find(id).is_none(), "{id} must be absent without an active multiband raster");
        }
    }

    #[gpui_kit::test]
    fn multiband_files_have_one_title_choice_with_canonical_first_band_ids(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[3, 5]);
        let (panel, handle) = open_test_panel(cx, 760.0);
        load_test_paths(&panel, handle, paths, cx);
        let files = cx.update(|cx| {
            let panel = panel.read(cx);
            let expected = [
                SelectionId::Raster(panel.sources[0].metadata().id),
                SelectionId::Raster(panel.sources[3].metadata().id),
            ];
            assert_eq!(panel.sources.len(), 8);
            assert_eq!(panel.choices().iter().map(|choice| choice.id).collect::<Vec<_>>(), expected);
            expected
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_band_selection(&panel, 1, Some(files[1]), cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_band_selection(&panel, 1, Some(files[0]), cx);
            window.click("raster-band", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_band_selection(&panel, 1, Some(files[1]), cx));
    }

    #[gpui_kit::test]
    fn reopening_a_raster_keeps_one_file_entry_and_only_its_current_bands(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let path = raster_test_paths(&dir, &[3]).remove(0);
        let (panel, handle) = open_test_panel(cx, 760.0);
        load_test_paths(&panel, handle, vec![path.clone(), path.clone()], cx);
        let original = cx.update(|cx| {
            let panel = panel.read(cx);
            assert_eq!(panel.choices().len(), 1);
            assert_eq!(panel.choices()[0].title.as_str(), "raster-0.tif");
            assert_eq!(panel.sources.len(), 3);
            panel.active().unwrap().metadata().id
        });
        load_test_paths(&panel, handle, vec![path], cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let panel = panel.read(cx);
            assert_eq!(panel.choices().len(), 1);
            assert_eq!(panel.sources.len(), 3);
            assert_ne!(panel.active().unwrap().metadata().id, original);
            assert_eq!(window.find("raster-band-label").label(), Some("Band 1/3"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn band_slider_below_layer_and_range_panes_changes_source_without_fitting(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[5]);
        let (panel, handle) = open_test_panel(cx, 1000.0);
        let events = record_events(&panel, cx);
        load_test_paths(&panel, handle, paths, cx);
        let original_source = cx.update(|cx| panel.read(cx).active().unwrap().id());
        let identity = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                assert_band_card_below_peer_panes(window);
                assert_eq!(window.find("raster-band-label").label(), Some("Band 1/5"));
                assert!(window.find("raster-band-ticks").visible());
                for band in 1_usize..=5 {
                    assert!(window.find(("raster-band-tick", band)).visible());
                }
                band_slider_snapshot(window).path().to_vec()
            })
            .unwrap();
        for (position, band) in [(0.5, 3), (0.95, 5), (0.05, 1)] {
            click_band_slider(&panel, handle, position, band, cx);
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(band_slider_snapshot(window).path(), identity);
                assert_eq!(window.find("raster-band-label").label(), Some(format!("Band {band}/5").as_str()));
            })
            .unwrap();
        }
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(panel.read(cx).active().unwrap().id(), original_source);
            window.click("toggle-raster-controls", cx);
            assert!(window.try_find("raster-range-card").is_none());
            assert_band_card_below_peer_panes(window);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let events = events.read(cx);
            assert_eq!(events.len(), 4, "Each changed band must emit once");
            assert!(events[0].should_fit());
            let sources: Vec<_> = events.iter().map(|event| event.source().unwrap()).collect();
            assert_eq!(
                sources.iter().map(|source| source.metadata().band_nr.unwrap()).collect::<Vec<_>>(),
                [1, 3, 5, 1]
            );
            assert_ne!(sources[0].id(), sources[1].id());
            assert_ne!(sources[1].id(), sources[2].id());
            assert!(events.iter().skip(1).all(|event| !event.should_fit() && event.vector_source().is_none()));
        });
    }

    fn assert_band_card_below_peer_panes(window: &Window) {
        let area = window.find("test-map-area").bounds();
        let card = window.find("raster-band-controls").bounds();
        let slider = window.find("raster-band-slider");
        let mut previous = window.find("raster-layers-card").bounds();
        for id in ["raster-range-card", "raster-visualization-card"] {
            if let Some(peer) = window.try_find(id) {
                assert!((peer.bounds().top() - previous.bottom() - window.rem_size() * 0.75).abs() <= px(1.0));
                previous = peer.bounds();
            }
        }
        assert!((card.top() - previous.bottom() - window.rem_size() * 0.75).abs() <= px(1.0));
        for id in ["raster-layers-card", "raster-range-card", "raster-visualization-card"] {
            if let Some(peer) = window.try_find(id) {
                assert!((card.left() - peer.bounds().left()).abs() <= px(1.0), "{id} must share the band's left edge");
                assert!((card.right() - peer.bounds().right()).abs() <= px(1.0), "{id} must share the band's right edge");
            }
        }
        assert!(slider.visible(), "Band navigation must remain visible with or without styling controls");
        assert!(card.left() >= area.left() && card.right() <= area.right());
        assert!(card.top() >= area.top() && card.bottom() <= area.bottom());
        assert!(slider.bounds().left() >= card.left() && slider.bounds().right() <= card.right());
        assert!(slider.bounds().top() >= card.top() && slider.bounds().bottom() <= card.bottom());
    }

    fn style_active_band(
        panel: &Entity<RasterPanel>,
        handle: PanelWindow,
        palette_steps: usize,
        range: (f32, f32),
        cx: &mut TestAppContext,
    ) -> Arc<RasterSource> {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            for _ in 0..palette_steps {
                window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            }
            for (id, value) in [("raster-min", range.0), ("raster-max", range.1)] {
                window.click(id, cx);
                window.press("secondary-a", cx);
                window.input(&value.to_string(), cx);
            }
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| panel.read(cx).active().unwrap().clone())
    }

    #[gpui_kit::test]
    fn band_slider_preserves_independent_palette_and_range_styles(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![per_band_style_test_path(&dir, 3)];
        let (panel, handle) = open_test_panel(cx, 1000.0);
        load_test_paths(&panel, handle, paths, cx);
        let first = style_active_band(&panel, handle, 1, (25.0, 45.0), cx);
        assert_eq!(first.palette(), ColorMapPreset::YlGnBu);
        assert_eq!(first.range(), (25.0, 45.0));
        click_band_slider(&panel, handle, 0.5, 2, cx);
        cx.update(|cx| {
            let source = panel.read(cx).active().unwrap();
            assert_eq!(selected_band(panel.read(cx)), Some(2));
            assert_eq!(source.palette(), ColorMapPreset::Turbo);
            assert_eq!(source.range(), source.data_range());
            assert_ne!(source.metadata().id, first.metadata().id);
        });
        let second = style_active_band(&panel, handle, 2, (35.0, 55.0), cx);
        assert_eq!(second.palette(), ColorMapPreset::YlOrRd);
        assert_eq!(second.range(), (35.0, 55.0));
        for (position, band, source, palette) in [(0.05, 1, &first, "YlGnBu"), (0.5, 2, &second, "YlOrRd")] {
            click_band_slider(&panel, handle, position, band, cx);
            cx.update(|cx| {
                assert_eq!(panel.read(cx).active().unwrap().id(), source.id());
                assert_eq!(panel.read(cx).min.read(cx).value().as_str(), format!("{:.2}", source.range().0));
                assert_eq!(panel.read(cx).max.read(cx).value().as_str(), format!("{:.2}", source.range().1));
                assert_eq!(panel.read(cx).palette.read(cx).selected_value().copied(), Some(palette));
            });
        }
    }

    #[gpui_kit::test]
    fn switching_raster_files_restores_each_files_selected_band(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[3, 5]);
        let (panel, handle) = open_test_panel(cx, 760.0);
        load_test_paths(&panel, handle, paths, cx);
        let files = cx.update(|cx| panel.read(cx).choices().iter().map(|choice| Some(choice.id)).collect::<Vec<_>>());
        let identity = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                band_slider_snapshot(window).path().to_vec()
            })
            .unwrap();
        click_band_slider(&panel, handle, 0.95, 5, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_band_selection(&panel, 5, files[1], cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_band_selection(&panel, 1, files[0], cx);
            assert_eq!(window.find("raster-band-label").label(), Some("Band 1/3"));
        })
        .unwrap();
        click_band_slider(&panel, handle, 0.5, 2, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_band_selection(&panel, 2, files[0], cx);
            window.click("raster-band", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_band_selection(&panel, 5, files[1], cx);
            assert_ne!(panel.read(cx).selected, files[1]);
            assert_eq!(window.find("raster-band-label").label(), Some("Band 5/5"));
            assert_eq!(band_slider_snapshot(window).path(), identity);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert_band_selection(&panel, 2, files[0], cx);
            assert_ne!(panel.read(cx).selected, files[0]);
            assert_eq!(window.find("raster-band-label").label(), Some("Band 2/3"));
            assert_eq!(band_slider_snapshot(window).path(), identity);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn band_selector_is_absent_for_single_band_vectors_and_cleared_datasets(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let mut paths = numeric_vector_paths(&dir);
        paths.extend(raster_test_paths(&dir, &[3]));
        let (panel, handle) = open_test_panel(cx, 760.0);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_band_selector_absent(window);
        })
        .unwrap();
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("raster-band-slider").visible());
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert!(panel.read(cx).active().is_some());
            assert_band_selector_absent(window);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            assert!(window.find("raster-band-slider").visible());
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            assert!(panel.read(cx).active_vector().is_some());
            assert_band_selector_absent(window);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -1.0)), cx);
            assert!(window.find("vector-property").visible());
            assert_band_selector_absent(window);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
            assert!(window.find("raster-band-slider").visible());
            panel.update(cx, |panel, cx| panel.clear(window, cx));
            window.render_frame(cx);
            assert_band_selector_absent(window);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn band_navigation_buttons_support_keyboard_activation_without_wrapping(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[3]);
        let (panel, handle) = open_test_panel(cx, 760.0);
        let events = record_events(&panel, cx);
        load_test_paths(&panel, handle, paths, cx);
        let file = cx.update(|cx| panel.read(cx).selected);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("previous-raster-band").disabled(), Some(true));
            window.click("previous-raster-band", cx);
            assert_band_selection(&panel, 1, file, cx);
            window.click("raster-min", cx);
            assert!(window.focused(cx).is_some(), "Tab navigation needs an initial focused control");
        })
        .unwrap();
        cx.run_until_parked();
        for (id, key, band) in [
            ("next-raster-band", "space", 2),
            ("previous-raster-band", "enter", 1),
            ("next-raster-band", "enter", 2),
            ("next-raster-band", "enter", 3),
        ] {
            focus_band_button(id, handle, cx);
            press_panel_key(handle, key, cx);
            cx.update(|cx| assert_band_selection(&panel, band, file, cx));
        }
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("next-raster-band").disabled(), Some(true));
            window.click("next-raster-band", cx);
            assert_band_selection(&panel, 3, file, cx);
            assert_eq!(window.find("raster-band-label").label(), Some("Band 3/3"));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let events = events.read(cx);
            assert_eq!(events.len(), 5, "Disabled endpoint buttons must not emit or wrap");
            assert!(events.iter().skip(1).all(|event| !event.should_fit()));
        });
    }

    #[test]
    fn slider_ranges_reject_equal_or_reversed_endpoints() {
        for position in [0.0, 500.0, 1000.0] {
            for raster in [false, true] {
                assert_eq!(slider_range((0.0, 25.56), (position, position), raster), None);
            }
        }
        assert_eq!(slider_range((0.0, 25.56), (600.0, 500.0), false), None);
    }

    #[test]
    fn slider_ranges_reject_endpoints_that_round_to_the_same_raster_value() {
        assert_eq!(slider_range((25.55, 25.56), (999.0, 1000.0), true), None);
        assert_eq!(slider_range((1e10, 1e10 + 1.0), (0.0, 1000.0), true), None);
    }

    #[test]
    fn slider_ranges_preserve_valid_raster_and_tiny_vector_ranges() {
        assert_eq!(slider_range((0.0, 100.0), (250.0, 750.0), true), Some((25.0, 75.0)));
        assert_eq!(slider_range((1e-12, 9e-12), (0.0, 1000.0), false), Some((1e-12, 9e-12)));
    }

    #[gpui_kit::test]
    fn collapsed_slider_ranges_preserve_inputs_style_and_slider_without_errors(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[1]);
        let (panel, handle) = open_test_panel(cx, 900.0);
        load_test_paths(&panel, handle, paths, cx);
        let events = record_events(&panel, cx);
        for position in [0.0, 500.0, 1000.0] {
            cx.update_window(handle.into(), |_, window, cx| {
                let (slider, range, inputs) = {
                    let panel = panel.read(cx);
                    (
                        panel.range_slider.clone(),
                        panel.color_range(),
                        (panel.min.read(cx).value(), panel.max.read(cx).value()),
                    )
                };
                slider.update(cx, |slider, cx| {
                    slider.set_value(position..position, window, cx);
                    cx.emit(SliderEvent::Change(slider.value()));
                });
                (range, inputs)
            })
            .map(|before| {
                cx.run_until_parked();
                cx.update(|cx| {
                    let panel = panel.read(cx);
                    assert_eq!(panel.color_range(), before.0);
                    assert_eq!((panel.min.read(cx).value(), panel.max.read(cx).value()), before.1);
                    assert!(panel.error.is_none());
                    let value = panel.range_slider.read(cx).value();
                    assert!(value.start() < value.end());
                    assert!(events.read(cx).is_empty());
                });
            })
            .unwrap();
        }
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
            assert_eq!(source.palette(), ColorMapPreset::YlGnBu);
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
            assert_eq!(panel.read(cx).active_vector().unwrap().palette(), ColorMapPreset::Turbo);
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
    fn extrusion_controls_preserve_per_band_style_and_support_keyboard(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![per_band_style_test_path(&dir, 2)];
        let (panel, handle) = open_test_panel(cx, 1200.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("raster-visualization-card").bounds().top() >= window.find("raster-range-card").bounds().bottom());
            assert_eq!(window.find("raster-extrude-pixels").checked(), Some(false));
            assert_eq!(
                panel.read(cx).extrusion_height.read(cx).value().end(),
                crate::raster_source::MAX_EXTRUSION_HEIGHT / 2.0
            );
            window.click("raster-extrusion-height", cx);
            assert_eq!(panel.read(cx).extrusion(), (false, DEFAULT_EXTRUSION_HEIGHT));
            window.click("raster-extrude-pixels", cx);
            assert_eq!(panel.read(cx).extrusion(), (true, DEFAULT_EXTRUSION_HEIGHT));
            let bounds = window.find("raster-extrusion-height").bounds();
            window.click_at("raster-extrusion-height", point(bounds.size.width * 0.75, bounds.size.height * 0.5), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let height = panel.read(cx).extrusion().1;
            assert!(height > DEFAULT_EXTRUSION_HEIGHT);
            window.click("next-raster-band", cx);
            assert_eq!(panel.read(cx).extrusion(), (false, DEFAULT_EXTRUSION_HEIGHT));
            window.click("previous-raster-band", cx);
            assert_eq!(panel.read(cx).extrusion(), (true, height));
            assert_eq!(panel.read(cx).extrusion_height.read(cx).value().end(), height);
            window.click("raster-extrude-pixels", cx);
            assert_eq!(panel.read(cx).extrusion(), (false, height));
            window.press("space", cx);
            assert_eq!(panel.read(cx).extrusion(), (true, height));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn extrusion_height_slider_can_exceed_the_previous_maximum(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![per_band_style_test_path(&dir, 2)];
        let (panel, handle) = open_test_panel(cx, 1200.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("raster-extrude-pixels", cx);
            let bounds = window.find("raster-extrusion-height").bounds();
            window.click_at("raster-extrusion-height", point(bounds.size.width * 0.9, bounds.size.height * 0.5), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let height = panel.read(cx).extrusion().1;
            assert!(height > 10.0 && height <= crate::raster_source::MAX_EXTRUSION_HEIGHT, "height={height}");
            assert_eq!(panel.read(cx).active().unwrap().extrusion_height(), height);
        });
    }

    #[gpui_kit::test]
    fn extrusion_averaging_is_controlled_disabled_and_retained_per_band(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![per_band_style_test_path(&dir, 2)];
        let (panel, handle) = open_test_panel(cx, 1200.0);
        load_test_paths(&panel, handle, paths, cx);
        let events = record_events(&panel, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let original = panel.read(cx).active().unwrap().clone();
            let control = window.find("raster-average-extrusion-pixels");
            assert_eq!(control.role(), Some(gpui_kit::Role::Button));
            assert_eq!(control.label(), Some("Reduce resolution"));
            assert!(!extrusion_averaging_pressed(window));
            assert_eq!(control.disabled(), Some(true));
            assert!(control.bounds().left() >= window.find("raster-extrude-pixels").bounds().right());
            assert!(control.bounds().bottom() <= window.find("raster-extrusion-height").bounds().top());
            window.click("raster-average-extrusion-pixels", cx);
            assert!(Arc::ptr_eq(panel.read(cx).active().unwrap(), &original));
            window.click("raster-extrude-pixels", cx);
            window.click("raster-average-extrusion-pixels", cx);
            assert!(extrusion_averaging_pressed(window));
            assert!(panel.read(cx).active().unwrap().is_extrusion_averaged());
            toggle_extrusion_averaging_with_keyboard(window, cx);
            assert!(!panel.read(cx).active().unwrap().is_extrusion_averaged());
            window.press("enter", cx);
            assert!(extrusion_averaging_pressed(window));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("next-raster-band", cx);
            assert!(!extrusion_averaging_pressed(window));
            assert_eq!(window.find("raster-average-extrusion-pixels").disabled(), Some(true));
            window.click("previous-raster-band", cx);
            assert!(extrusion_averaging_pressed(window));
            window.click("raster-extrude-pixels", cx);
            assert!(extrusion_averaging_pressed(window));
            assert_eq!(window.find("raster-average-extrusion-pixels").disabled(), Some(true));
            window.click("raster-average-extrusion-pixels", cx);
            assert!(panel.read(cx).active().unwrap().is_extrusion_averaged());
            window.click("raster-extrude-pixels", cx);
            assert!(extrusion_averaging_pressed(window));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let events = events.read(cx);
            assert!(events.iter().all(|event| !event.should_fit()));
            assert!(events.last().unwrap().source().unwrap().is_extrusion_averaged());
        });
        cx.update_window(handle.into(), |_, window, cx| {
            panel.update(cx, |panel, cx| panel.clear(window, cx));
            window.render_frame(cx);
            assert!(window.try_find("raster-average-extrusion-pixels").is_none());
            assert!(window.try_find("raster-extrusion-averaging-description").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn extrusion_averaging_icon_aligns_without_overflow_at_larger_font_sizes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[1]);
        let (panel, handle) = open_test_panel(cx, 1200.0);
        load_test_paths(&panel, handle, paths, cx);
        for font_size in [16.0, 20.0] {
            cx.update(|cx| gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(font_size)));
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let card = window.find("raster-visualization-card").bounds();
                let toggle = window.find("raster-extrude-pixels").bounds();
                let control = window.find("raster-average-extrusion-pixels").bounds();
                let height = window.find("raster-extrusion-height").bounds();
                assert!(window.find("raster-average-extrusion-pixels").visible());
                assert!(window.try_find("raster-extrusion-averaging-description").is_none());
                assert!(control.left() >= toggle.right());
                assert_eq!(control.center().y, toggle.center().y);
                assert!(height.top() >= control.bottom());
                assert!(height.bottom() <= card.bottom());
                assert_eq!(control.right(), card.right() - (toggle.left() - card.left()));
            })
            .unwrap();
        }
    }

    #[gpui_kit::test]
    fn extrusion_averaging_and_range_controls_remain_reachable_in_short_windows(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[1]);
        let (panel, handle) = open_test_panel(cx, 400.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update(|cx| gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(18.0)));
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let before = window.find("raster-visualization-card").bounds();
            window.scroll("raster-layers-card", ScrollDelta::Lines(point(0.0, -30.0)), cx);
            assert!(window.find("raster-visualization-card").bounds().top() < before.top());
            assert!(window.find("raster-average-extrusion-pixels").visible());
            assert!(window.find("raster-extrusion-height").visible());
            window.click("raster-extrude-pixels", cx);
            window.click("raster-average-extrusion-pixels", cx);
            assert!(panel.read(cx).active().unwrap().is_extrusion_averaged());
            window.scroll("raster-visualization-card", ScrollDelta::Lines(point(0.0, 5.0)), cx);
            assert!(window.find("raster-max").visible());
        })
        .unwrap();
    }

    fn extrusion_averaging_pressed(window: &Window) -> bool {
        // Kit exposes AccessKit's native toggle-button pressed state through checked().
        window.find("raster-average-extrusion-pixels").checked().expect("Native button pressed state")
    }

    fn toggle_extrusion_averaging_with_keyboard(window: &mut Window, cx: &mut App) {
        window.click("raster-min", cx);
        for _ in 0..30 {
            if window.find("raster-average-extrusion-pixels").focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find("raster-average-extrusion-pixels").focused(), Some(true));
        window.press("space", cx);
        assert!(!extrusion_averaging_pressed(window));
    }

    #[gpui_kit::test]
    fn extrusion_averaging_updates_shared_playback_without_seeking_or_rebuilding_bands(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[3]);
        let (panel, handle) = open_test_panel(cx, 1200.0);
        load_test_paths(&panel, handle, paths, cx);
        let events = record_events(&panel, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("raster-extrude-pixels", cx);
            window.click("toggle-raster-playback", cx);
            let playing = panel.read(cx).playback_request().unwrap();
            panel.update(cx, |panel, cx| {
                panel.apply_playback_frame(PlaybackFrame::new(playing.dataset_id().unwrap(), playing.revision(), 1.25, true), window, cx);
            });
            window.render_frame(cx);
            let before = panel.read(cx).playback_request().unwrap();
            assert!(before.is_playing());
            assert_eq!(before.style().extrusion_averaging(), None);
            window.click("raster-average-extrusion-pixels", cx);
            let averaged = panel.read(cx).playback_request().unwrap();
            assert_averaging_playback_controls_unchanged(&before, &averaged);
            assert_eq!(averaged.style().extrusion_averaging(), Some(true));
            assert!(averaged.bands().iter().all(|source| averaged.style().extrusion_averaging_for(source)));
            assert!(panel.read(cx).playback.buffering);
            window.click("raster-extrude-pixels", cx);
            assert!(extrusion_averaging_pressed(window));
            assert_eq!(window.find("raster-average-extrusion-pixels").disabled(), Some(true));
            window.click("raster-extrude-pixels", cx);
            window.click("raster-extrusion-height", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let before = panel.read(cx).playback_request().unwrap();
            assert!(before.style().extrusion().unwrap().1 > 1.0);
            assert!(extrusion_averaging_pressed(window));
            toggle_extrusion_averaging_with_keyboard(window, cx);
            let unaveraged = panel.read(cx).playback_request().unwrap();
            assert_averaging_playback_controls_unchanged(&before, &unaveraged);
            assert_eq!(unaveraged.style().extrusion_averaging(), Some(false));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let events = events.read(cx);
            assert!(events.iter().all(|event| !event.should_fit()));
            let last = events.last().unwrap().playback().unwrap();
            assert_eq!(last.style().extrusion_averaging(), Some(false));
            assert_eq!(last.position(), 1.25);
        });
    }

    fn assert_averaging_playback_controls_unchanged(before: &PlaybackRequest, after: &PlaybackRequest) {
        assert!(after.shares_bands(before));
        assert_eq!(after.revision(), before.revision());
        assert_eq!(after.position(), before.position());
        assert_eq!(after.is_playing(), before.is_playing());
        assert_eq!(after.bands_per_second(), before.bands_per_second());
        assert_eq!(after.interpolation(), before.interpolation());
        for (before, after) in before.bands().iter().zip(after.bands()) {
            assert!(Arc::ptr_eq(before, after));
            assert!(!after.is_extrusion_averaged(), "Shared overrides cannot restyle the raw bands");
        }
    }

    #[gpui_kit::test]
    fn extrusion_playback_style_is_shared_without_revising_playback(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[2]);
        let (panel, handle) = open_test_panel(cx, 1200.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let revision = panel.read(cx).playback.revision;
            window.click("raster-extrude-pixels", cx);
            assert_eq!(panel.read(cx).playback.revision, revision);
            assert_eq!(panel.read(cx).playback_style().extrusion(), Some((true, DEFAULT_EXTRUSION_HEIGHT)));
            window.click("next-raster-band", cx);
            assert_eq!(panel.read(cx).extrusion(), (true, DEFAULT_EXTRUSION_HEIGHT));
            panel.update(cx, |panel, cx| panel.clear(window, cx));
            window.render_frame(cx);
            assert!(window.try_find("raster-visualization-card").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn clamp_colors_checkbox_updates_style_and_retains_per_band_state_through_the_ui(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![per_band_style_test_path(&dir, 2)];
        let (panel, handle) = open_test_panel(cx, 900.0);
        load_test_paths(&panel, handle, paths, cx);
        let events = record_events(&panel, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let original = panel.read(cx).active().unwrap().clone();
            assert_eq!(window.within("raster-range-card").find("raster-clamp-colors").label(), Some("Clamp colors"));
            assert_eq!(window.find("raster-clamp-colors").checked(), Some(true));
            assert!(window.find("raster-clamp-colors").bounds().top() >= window.find("raster-max").bounds().bottom());
            window.click("raster-clamp-colors", cx);
            let unclamped = panel.read(cx).active().unwrap().clone();
            assert!(!unclamped.clamp_colors());
            assert_eq!(unclamped.metadata().id, original.metadata().id);
            assert_eq!(unclamped.range(), original.range());
            assert_eq!(unclamped.palette(), original.palette());
            assert_eq!(window.find("raster-clamp-colors").checked(), Some(false));
            window.click("next-raster-band", cx);
            assert_ne!(panel.read(cx).active().unwrap().metadata().id, original.metadata().id);
            assert!(panel.read(cx).active().unwrap().clamp_colors());
            assert_eq!(window.find("raster-clamp-colors").checked(), Some(true));
            window.click("previous-raster-band", cx);
            assert_eq!(panel.read(cx).active().unwrap().metadata().id, original.metadata().id);
            assert!(!panel.read(cx).active().unwrap().clamp_colors());
            assert_eq!(window.find("raster-clamp-colors").checked(), Some(false));
            window.click("reset-raster-range", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("raster-clamp-colors").checked(),
                Some(false),
                "Resetting the range must retain clamping mode"
            );
            window.click("raster-min", cx);
            for _ in 0..20 {
                if window.find("raster-clamp-colors").focused() == Some(true) {
                    break;
                }
                window.press("tab", cx);
            }
            assert_eq!(window.find("raster-clamp-colors").focused(), Some(true));
            window.press("space", cx);
            assert_eq!(window.find("raster-clamp-colors").checked(), Some(true));
            assert!(panel.read(cx).active().unwrap().clamp_colors());
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(events.read(cx).last().unwrap().source().unwrap().clamp_colors());
            assert!(events.read(cx).iter().all(|event| !event.fit));
            assert!(panel.read(cx).active().unwrap().clamp_colors());
        });
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
        assert_eq!(panel.choices().len(), 2);
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
        })
        .unwrap();
        click_band_slider(panel, handle, 0.95, 2, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -6.0)), cx);
            assert!(panel.read(cx).active().is_none());
            assert!(panel.read(cx).active_vector().is_some());
            assert_eq!(panel.read(cx).bands.read(cx).selected_value().copied(), panel.read(cx).selected);
            assert!(window.try_find("raster-band-slider").is_none());
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
            assert!(window.try_find("raster-band-slider").is_none());
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
    fn scrolling_changes_at_most_one_file_per_event(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let paths = raster_test_paths(&dir, &[2, 3, 5]);
        let (panel, handle) = open_test_panel(cx, 760.0);
        load_test_paths(&panel, handle, paths, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let files: Vec<_> = panel.read(cx).choices().iter().map(|choice| Some(choice.id)).collect();
            window.scroll("raster-band", ScrollDelta::Lines(point(0.0, -6.0)), cx);
            assert_eq!(panel.read(cx).selected, files[2], "The last file must not wrap");
            for (delta, expected_file) in [
                (ScrollDelta::Lines(point(0.0, 3.0)), 1),
                (ScrollDelta::Lines(point(0.0, 6.0)), 0),
                (ScrollDelta::Lines(point(0.0, 3.0)), 0),
                (ScrollDelta::Lines(point(0.0, -3.0)), 1),
                (ScrollDelta::Pixels(point(px(0.0), px(-240.0))), 2),
                (ScrollDelta::Pixels(point(px(0.0), px(240.0))), 1),
            ] {
                window.scroll("raster-band", delta, cx);
                assert_band_selection(&panel, 1, files[expected_file], cx);
            }
            for _ in 0..3 {
                window.scroll("raster-band", ScrollDelta::Pixels(point(px(0.0), px(-10.0))), cx);
                assert_eq!(panel.read(cx).selected, files[1], "Small trackpad deltas must accumulate");
            }
            window.scroll("raster-band", ScrollDelta::Pixels(point(px(0.0), px(-10.0))), cx);
            assert_band_selection(&panel, 1, files[2], cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn prompt_cancel_and_open_then_range_validation_and_clear_work_through_the_ui(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let path = per_band_style_test_path(&dir, 2);
        let (panel, handle) = open_test_panel(cx, 1000.0);
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
            assert_eq!(selected_band(panel), Some(1), "Opening a multiband raster must select its first band");
        });
    }

    fn verify_band_navigation(panel: &Entity<RasterPanel>, handle: PanelWindow, original_band: Option<SelectionId>, cx: &mut TestAppContext) {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            for delta in [1.0, -1.0] {
                window.scroll("raster-band", ScrollDelta::Lines(point(0.0, delta)), cx);
                assert_eq!(panel.read(cx).selected, original_band, "Title scrolling must not change bands within a file");
            }
            assert_eq!(window.find("previous-raster-band").disabled(), Some(true));
            window.click("previous-raster-band", cx);
            assert_band_selection(panel, 1, original_band, cx);
            window.click("next-raster-band", cx);
            assert_band_selection(panel, 2, original_band, cx);
            assert_ne!(panel.read(cx).selected, original_band);
            assert_eq!(window.find("next-raster-band").disabled(), Some(true));
            window.click("next-raster-band", cx);
            assert_band_selection(panel, 2, original_band, cx);
            window.click("previous-raster-band", cx);
            assert_band_selection(panel, 1, original_band, cx);
        })
        .unwrap();
        click_band_slider(panel, handle, 0.95, 2, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_band_selection(panel, 2, original_band, cx);
            window.click("raster-band", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_band_selection(panel, 2, original_band, cx));
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
