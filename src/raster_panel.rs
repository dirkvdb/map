use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, InputState, NumberInput};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme, Icon, IndexPath, Sizable};
use gpui_kit::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, PathPromptOptions,
    Render, ScrollDelta, ScrollEvent, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, Window, div, rgba,
};
use inf::colormap::ColorMapPreset;

use crate::raster_source::RasterSource;
use crate::vector_source::VectorSource;

gpui_kit::actions!(raster, [OpenDataset, FitRaster, ClearRasters, ToggleRasterControls]);

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
    bands: Entity<SelectState<Vec<BandChoice>>>,
    palette: Entity<SelectState<Vec<&'static str>>>,
    opacity: Entity<SliderState>,
    range_slider: Entity<SliderState>,
    range_domain: (f32, f32),
    min: Entity<InputState>,
    max: Entity<InputState>,
    error: Option<SharedString>,
    load_error: Option<SharedString>,
    loading: bool,
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
        let palette = cx.new(|cx| SelectState::new(vec!["Terrain", "Blues", "Gray", "Turbo", "Spectral"], Some(IndexPath::default()), window, cx));
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
            cx.subscribe_in(&opacity, window, |this, _, event: &SliderEvent, _, cx| {
                if let SliderEvent::Change(_) = event {
                    this.emit(false, cx);
                }
            }),
            cx.subscribe_in(&range_slider, window, |this, _, event: &SliderEvent, window, cx| {
                if let SliderEvent::Change(value) = event {
                    let (min, max) = this.range_domain;
                    let start = min + (max - min) * value.start() / 1000.0;
                    let end = min + (max - min) * value.end() / 1000.0;
                    this.min.update(cx, |input, cx| input.set_value(start.to_string(), window, cx));
                    this.max.update(cx, |input, cx| input.set_value(end.to_string(), window, cx));
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
            bands,
            palette,
            opacity,
            range_slider,
            range_domain: (0.0, 1.0),
            min,
            max,
            error: None,
            load_error: None,
            loading: false,
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
        self.selected = id;
        self.error = None;
        self.load_error = None;
        self.collapsed = false;
        if let Some(source) = self.active() {
            let range = source.range();
            let palette = match source.palette() {
                ColorMapPreset::Gray => "Gray",
                ColorMapPreset::Blues => "Blues",
                ColorMapPreset::Turbo => "Turbo",
                ColorMapPreset::Spectral => "Spectral",
                _ => "Terrain",
            };
            self.min.update(cx, |input, cx| input.set_value(range.0.to_string(), window, cx));
            self.max.update(cx, |input, cx| input.set_value(range.1.to_string(), window, cx));
            self.palette.update(cx, |state, cx| state.set_selected_value(&palette, window, cx));
        }
        self.sync_range_slider(window, cx);
        self.watch_tile_errors(cx);
        self.emit(fit, cx);
    }

    fn watch_tile_errors(&mut self, cx: &mut Context<Self>) {
        self.tile_errors_task = self.active().map(|source| {
            let errors = source.errors();
            cx.spawn(async move |this, cx| {
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
            })
        });
    }

    fn scroll_bands(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.choices();
        let Some(ix) = items.iter().position(|item| Some(item.id) == self.selected) else {
            return;
        };
        let delta = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            // Accumulate precise device pixels so small trackpad events don't
            // race through bands. This is an input threshold, not layout size.
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / 40.0,
        };
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        cx.stop_propagation();
        if self.band_scroll.signum() != delta.signum() {
            self.band_scroll = 0.0;
        }
        self.band_scroll += delta;
        // A wheel notch can report several lines; band navigation still moves
        // by only one index. Keep only the fractional trackpad remainder.
        let steps = self.band_scroll.trunc().clamp(-1.0, 1.0);
        self.band_scroll = self.band_scroll.fract();
        let next = (ix as f32 - steps).clamp(0.0, (items.len() - 1) as f32) as usize;
        if next != ix {
            let id = items[next].id;
            self.bands.update(cx, |state, cx| state.set_selected_value(&id, window, cx));
            self.select(Some(id), false, window, cx);
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
        cx.notify();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = paths.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.loading = false;
                match result {
                    Ok(Ok(Some(paths))) => this.open_paths(paths, cx),
                    Ok(Ok(None)) => cx.notify(),
                    error => {
                        this.load_error = Some(format!("Couldn’t open the file picker: {error:?}").into());
                        cx.notify();
                    }
                }
            });
        }));
    }

    pub fn open_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.loading || paths.is_empty() {
            return;
        }
        self.loading = true;
        self.error = None;
        self.load_error = None;
        cx.notify();
        let work = cx.background_spawn(async move {
            let mut sources = Vec::new();
            let mut vectors = Vec::new();
            let mut errors = Vec::new();
            let mut selected = None;
            for path in paths {
                match VectorSource::open(&path) {
                    Ok(Some(opened)) => {
                        selected = opened.first().map(|source| SelectionId::Vector(source.id()));
                        vectors.extend(opened);
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        errors.push(format!("{}: {error:#}", path.display()));
                        continue;
                    }
                }
                match RasterSource::open(&path) {
                    Ok(opened) => {
                        selected = opened.first().map(|source| SelectionId::Raster(source.metadata().id));
                        sources.extend(opened);
                    }
                    Err(error) => errors.push(format!("{}: {error:#}", path.display())),
                }
            }
            (sources, vectors, errors, selected)
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let (sources, vectors, errors, selected) = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                this.sources.extend(sources);
                this.vectors.extend(vectors);
                let items = this.choices();
                this.bands.update(cx, |state, cx| state.set_items(items, window, cx));
                if let Some(id) = selected {
                    this.bands.update(cx, |state, cx| state.set_selected_value(&id, window, cx));
                    this.select(Some(id), true, window, cx);
                }
                this.load_error = (!errors.is_empty()).then(|| errors.join("\n").into());
                cx.notify();
            });
        }));
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

    fn sync_range_slider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.active() else { return };
        let (Ok(min), Ok(max)) = (self.min.read(cx).value().parse::<f32>(), self.max.read(cx).value().parse::<f32>()) else {
            return;
        };
        if !min.is_finite() || !max.is_finite() || min >= max {
            return;
        }
        let (data_min, data_max) = source.data_range();
        self.range_domain = (data_min.min(min), data_max.max(max));
        let (start, end) = self.range_domain;
        let span = end - start;
        if !span.is_finite() || span <= 0.0 {
            return;
        }
        self.range_slider.update(cx, |slider, cx| {
            slider.set_value((min - start) / span * 1000.0..(max - start) / span * 1000.0, window, cx)
        });
    }

    fn reset_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.active() else { return };
        let (min, max) = source.data_range();
        self.min.update(cx, |input, cx| input.set_value(min.to_string(), window, cx));
        self.max.update(cx, |input, cx| input.set_value(max.to_string(), window, cx));
        self.apply_style(window, cx);
    }

    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_task = None;
        self.tile_errors_task = None;
        self.loading = false;
        self.sources.clear();
        self.vectors.clear();
        self.band_scroll = 0.0;
        self.selected = None;
        self.error = None;
        self.load_error = None;
        self.bands.update(cx, |state, cx| {
            state.set_items(Vec::new(), window, cx);
            state.set_selected_index(None, window, cx);
        });
        self.emit(false, cx);
    }

    fn apply_style(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.active() else { return };
        let result = (|| -> anyhow::Result<Arc<RasterSource>> {
            let min = self.min.read(cx).value().trim().parse::<f32>()?;
            let max = self.max.read(cx).value().trim().parse::<f32>()?;
            let palette = match self.palette.read(cx).selected_value().copied() {
                Some("Gray") => ColorMapPreset::Gray,
                Some("Blues") => ColorMapPreset::Blues,
                Some("Turbo") => ColorMapPreset::Turbo,
                Some("Spectral") => ColorMapPreset::Spectral,
                _ => ColorMapPreset::Terrain,
            };
            // Synchronization and repeated commits must not invalidate tiles
            // when the applied values are already current.
            if source.palette() == palette && source.range() == (min, max) {
                Ok(source.clone())
            } else {
                source.restyled(palette, (min, max))
            }
        })();
        match result {
            Ok(source) => {
                if let Some(current) = self
                    .sources
                    .iter_mut()
                    .find(|current| Some(SelectionId::Raster(current.metadata().id)) == self.selected)
                {
                    *current = source;
                }
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
    pub fn render_layer_selector(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        div()
            .id("raster-band-scroll")
            .w_64()
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
                    .placeholder("Open a geospatial dataset")
                    .disabled(self.sources.is_empty() && self.vectors.is_empty()),
            )
    }
}

impl Render for RasterPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            if self.active().is_some() {
                layers = layers
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(field_label("Color map", cx))
                            .child(Select::new(&self.palette).id("raster-palette").accessibility_label("Color map")),
                    )
                    .child(self.render_legend(cx));
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

        let mut panel = div().flex().flex_col().gap_3().size_full().child(layers);
        if !self.collapsed && self.active().is_some() {
            panel = panel.child(self.render_value_range(cx));
        }
        div()
            .id("raster-controls")
            .test_support()
            .track_focus(&self.focus)
            .occlude()
            .size_full()
            .min_h_0()
            .child(panel.overflow_y_scrollbar().id("raster-controls-scroll"))
    }
}

impl RasterPanel {
    fn render_legend(&self, cx: &App) -> impl IntoElement {
        let mut legend = div().id("raster-legend").test_support().flex().flex_col().gap_1();
        if let Some(source) = self.active() {
            let (min, max) = source.range();
            let swatches = (0..256).map(|ix| {
                let color = source.legend().color_for_value(min + (max - min) * ix as f32 / 255.0, None);
                // These colors encode raster data, not application chrome.
                div().flex_1().h_3().bg(rgba(u32::from_be_bytes([color.r, color.g, color.b, color.a])))
            });
            legend = legend
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .children((0..5).map(|ix| format_value(min + (max - min) * ix as f32 / 4.0))),
                )
                .child(
                    div()
                        .flex()
                        .w_full()
                        .rounded(cx.theme().radius_tokens().sm)
                        .overflow_hidden()
                        .children(swatches),
                );
        }
        legend
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
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(field_label("Minimum", cx))
                            .child(div().id("raster-min").test_support().child(NumberInput::new(&self.min).small())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(field_label("Maximum", cx))
                            .child(div().id("raster-max").test_support().child(NumberInput::new(&self.max).small())),
                    ),
            )
    }
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

fn format_value(value: f32) -> String {
    if value != 0.0 && (value.abs() < 0.001 || value.abs() >= 100_000.0) {
        format!("{value:.2e}")
    } else {
        format!("{value:.3}").trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{TestAppContext, point, px, size};

    use super::*;

    struct PanelHarness(Entity<RasterPanel>);

    impl Render for PanelHarness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let selector = self.0.update(cx, |panel, cx| panel.render_layer_selector(cx));
            div().flex().flex_col().size_full().child(selector).child(self.0.clone())
        }
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
        let mut panel = None;
        let handle = cx.open_window(size(px(900.0), px(760.0)), |window, cx| {
            let view = cx.new(|cx| RasterPanel::new(window, cx));
            panel = Some(view.clone());
            let harness = cx.new(|_| PanelHarness(view));
            gpui_kit::base::Root::new(harness, window, cx)
        });
        let panel = panel.unwrap();
        let events = cx.update(|cx| {
            cx.new(|cx: &mut Context<Vec<RasterEvent>>| {
                cx.subscribe(&panel, |events, _, event, _| {
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
        });
        cx.update_window(handle.into(), |_, _, cx| {
            panel.update(cx, |panel, cx| panel.prompt(cx));
        })
        .unwrap();
        cx.simulate_path_prompt_response(|options| {
            assert_eq!(options.prompt.as_deref(), Some("Open datasets"));
            Some(vec![vector, raster])
        });
        cx.run_until_parked();
        cx.update(|cx| {
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
        });
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
            assert!(window.try_find("raster-palette").is_none());
            assert!(window.try_find("raster-legend").is_none());
            assert!(window.try_find("raster-range-slider").is_none());
            assert!(window.try_find("raster-min").is_none());
            assert!(window.try_find("raster-max").is_none());
            assert_eq!(window.find("toggle-raster-controls").label(), Some("Hide layer controls"));
            window.click("toggle-raster-controls", cx);
            assert!(window.try_find("raster-opacity").is_none());
            window.click("toggle-raster-controls", cx);
            let opacity = window.find("raster-opacity").bounds();
            window.click_at("raster-opacity", point(opacity.size.width * 0.5, opacity.size.height * 0.5), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let event = events.read(cx).last().unwrap();
            assert!(event.source().is_none());
            assert_eq!(event.vector_source().unwrap().id(), panel.read(cx).active_vector().unwrap().id());
            assert!((event.opacity() - 0.5).abs() < 0.05);
            assert!(!event.should_fit());
            assert!(events.read(cx).iter().all(|event| event.source().is_none() || event.vector_source().is_none()));
        });
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
        let mut panel = None;
        let handle = cx.open_window(size(px(900.0), px(760.0)), |window, cx| {
            let view = cx.new(|cx| RasterPanel::new(window, cx));
            panel = Some(view.clone());
            let harness = cx.new(|_| PanelHarness(view));
            gpui_kit::base::Root::new(harness, window, cx)
        });
        let panel = panel.unwrap();
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
        let mut panel = None;
        let handle = cx.open_window(size(px(900.0), px(760.0)), |window, cx| {
            let view = cx.new(|cx| RasterPanel::new(window, cx));
            panel = Some(view.clone());
            let harness = cx.new(|_| PanelHarness(view));
            gpui_kit::base::Root::new(harness, window, cx)
        });
        let panel = panel.unwrap();
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

        let original_band = cx.update(|cx| panel.read(cx).selected);
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
}
