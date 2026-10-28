use gdal::Metadata;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, point, px, size};

use super::*;

type PanelWindow = gpui_kit::WindowHandle<gpui_kit::base::Root>;

struct PanelHarness(Entity<RasterPanel>);

impl Render for PanelHarness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selector = self.0.update(cx, |panel, cx| panel.render_layer_selector(window, cx));
        div().flex().flex_col().size_full().child(selector).child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(div().absolute().top_0().right_0().bottom_0().w_80().child(self.0.clone())),
        )
    }
}

fn open_panel(cx: &mut TestAppContext) -> (Entity<RasterPanel>, PanelWindow) {
    cx.update(gpui_kit::init);
    let mut panel = None;
    let handle = cx.open_window(size(px(900.0), px(900.0)), |window, cx| {
        let view = cx.new(|cx| RasterPanel::new(window, cx));
        panel = Some(view.clone());
        let harness = cx.new(|_| PanelHarness(view));
        gpui_kit::base::Root::new(harness, window, cx)
    });
    (panel.unwrap(), handle)
}

fn cog_paths(directory: &tempfile::TempDir, counts: &[usize]) -> Vec<PathBuf> {
    counts
        .iter()
        .enumerate()
        .map(|(ix, &bands)| {
            let input = directory.path().join(format!("input-{ix}.tif"));
            let output = directory.path().join(format!("playback-{ix}.tif"));
            crate::raster_source::test_support::write_raster(&input, 3857, bands).unwrap();
            write_aligned_cog(&input, &output).unwrap();
            output
        })
        .collect()
}

fn write_aligned_cog(input: &std::path::Path, output: &std::path::Path) -> anyhow::Result<()> {
    let input = gdal::Dataset::open(input)?;
    for nr in 1..=input.raster_count() {
        input.rasterband(nr)?.set_description(&format!("Frame {nr}"))?;
    }
    let options = gdal::raster::RasterCreationOptions::from_iter([
        "TILING_SCHEME=GoogleMapsCompatible",
        "ZOOM_LEVEL=3",
        "BLOCKSIZE=256",
        "OVERVIEWS=NONE",
        "COMPRESS=DEFLATE",
        "INTERLEAVE=BAND",
        "ADD_ALPHA=NO",
    ]);
    input.create_copy(&gdal::DriverManager::get_driver_by_name("COG")?, output, &options)?.close()?;
    pad_cog_header(output)
}

fn pad_cog_header(path: &std::path::Path) -> anyhow::Result<()> {
    // The native COG probe reads a full 16-KiB header even for tiny fixtures.
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(file.metadata()?.len().max(16 * 1024))?;
    Ok(())
}

fn load(panel: &Entity<RasterPanel>, handle: PanelWindow, paths: Vec<PathBuf>, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, _, cx| panel.update(cx, |panel, cx| panel.open_paths(paths, cx)))
        .unwrap();
    cx.run_until_parked();
}

fn record_commands(panel: &Entity<RasterPanel>, cx: &mut TestAppContext) -> Entity<Vec<Option<PlaybackRequest>>> {
    cx.new(|cx: &mut Context<Vec<Option<PlaybackRequest>>>| {
        cx.subscribe(panel, |events, _, event, _| events.push(event.playback())).detach();
        Vec::new()
    })
}

fn request(panel: &Entity<RasterPanel>, cx: &mut TestAppContext) -> PlaybackRequest {
    let request = cx.update(|cx| panel.read(cx).playback_request().expect("A multiband COG must provide a playback command"));
    assert!(request.interpolation(), "Panel requests must always enable interpolation");
    request
}

fn assert_raw_bands_unchanged(original: &PlaybackRequest, current: &PlaybackRequest) {
    assert!(current.shares_bands(original), "Runtime edits must retain the exact immutable band array");
    assert_eq!(current.bands().len(), original.bands().len());
    for (before, after) in original.bands().iter().zip(current.bands()) {
        assert!(Arc::ptr_eq(before, after), "Runtime styling cannot replace a raw source");
        assert_eq!(before.id(), after.id());
        assert_eq!(before.metadata().id, after.metadata().id);
    }
}

fn click(handle: PanelWindow, id: &'static str, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn apply_frame(panel: &Entity<RasterPanel>, handle: PanelWindow, frame: PlaybackFrame, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        panel.update(cx, |panel, cx| panel.apply_playback_frame(frame, window, cx));
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn focus_control(handle: PanelWindow, id: impl Into<gpui_kit::ElementId>, cx: &mut TestAppContext) {
    let id = id.into();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Headless windows and clicks on disabled endpoints can leave no
        // focused control. Establish a neutral anchor, then use real Tab input.
        if window.focused(cx).is_none() {
            window.click("raster-min", cx);
        }
        assert!(window.focused(cx).is_some(), "Tab traversal needs an initial focused control");
    })
    .unwrap();
    cx.run_until_parked();
    for _ in 0..64 {
        let focused = cx
            .update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                if window.find(id.clone()).focused() == Some(true) {
                    return true;
                }
                window.press("tab", cx);

                false
            })
            .unwrap();
        cx.run_until_parked();
        if focused {
            return;
        }
    }
    panic!("{id:?} must be reachable by Tab");
}

fn press(handle: PanelWindow, key: &str, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| window.press(key, cx)).unwrap();
    cx.run_until_parked();
}

fn assert_playback_visibility(panel: &Entity<RasterPanel>, handle: PanelWindow, expected: bool, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for id in [
            "raster-playback-controls",
            "toggle-raster-playback",
            "first-raster-band",
            "last-raster-band",
            "raster-playback-speed",
            "raster-playback-status",
        ] {
            assert_eq!(window.try_find(id).is_some(), expected, "{id}");
        }
        assert!(window.try_find("raster-playback-interpolation").is_none());
        assert_eq!(panel.read(cx).playback_request().is_some(), expected);
    })
    .unwrap();
}

#[test]
fn aligned_playback_fixture_opens_as_native_cog_with_all_band_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    let path = cog_paths(&dir, &[3]).remove(0);
    let dataset = gdal::Dataset::open(&path).unwrap();
    assert_eq!(dataset.metadata_item("LAYOUT", "IMAGE_STRUCTURE").as_deref(), Some("COG"));
    assert_eq!(dataset.metadata_item("INTERLEAVE", "IMAGE_STRUCTURE").as_deref(), Some("BAND"));
    let bands = RasterSource::open(&path).unwrap();
    assert_eq!(bands.len(), 3);
    for (ix, source) in bands.iter().enumerate() {
        assert_eq!(source.metadata().source_format, tiler::LayerSourceType::CloudOptimizedGeoTiff);
        assert_eq!(source.metadata().band_nr, Some(ix + 1));
        assert_eq!(source.metadata().description, format!("Frame {}", ix + 1));
    }
}

#[gpui_kit::test]
fn playback_controls_are_available_only_for_multiband_cogs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let cogs = cog_paths(&dir, &[1, 5]);
    let plain = dir.path().join("plain.tif");
    crate::raster_source::test_support::write_raster(&plain, 4326, 5).unwrap();
    let netcdf = dir.path().join("time.nc");
    crate::raster_source::test_support::write_netcdf(&netcdf, true, false).unwrap();
    let vector = dir.path().join("points.geojson");
    std::fs::write(
        &vector,
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#,
    )
    .unwrap();
    let (panel, handle) = open_panel(cx);
    assert_playback_visibility(&panel, handle, false, cx);
    for path in [plain, netcdf, vector, cogs[0].clone()] {
        load(&panel, handle, vec![path], cx);
        assert_playback_visibility(&panel, handle, false, cx);
    }
    load(&panel, handle, vec![cogs[1].clone()], cx);
    assert_playback_visibility(&panel, handle, true, cx);
    let initial = request(&panel, cx);
    assert!(!initial.is_playing());
    assert_eq!(initial.position(), 0.0);
    assert_eq!(initial.bands_per_second(), 3.0);
    assert!(initial.interpolation());
    assert_eq!(initial.bands().len(), 5);
    assert!(
        initial
            .bands()
            .iter()
            .all(|source| source.metadata().source_format == tiler::LayerSourceType::CloudOptimizedGeoTiff)
    );
}

#[gpui_kit::test]
fn play_and_pause_support_keyboard_and_pause_at_the_last_fractional_frame(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let paths = cog_paths(&dir, &[5]);
    let (panel, handle) = open_panel(cx);
    let events = record_commands(&panel, cx);
    load(&panel, handle, paths, cx);
    let initial = request(&panel, cx);
    focus_control(handle, "toggle-raster-playback", cx);
    press(handle, "enter", cx);
    let playing = request(&panel, cx);
    assert!(playing.is_playing());
    assert!(playing.interpolation());
    assert!(playing.revision() > initial.revision());
    apply_frame(
        &panel,
        handle,
        PlaybackFrame::new(playing.dataset_id().unwrap(), playing.revision(), 1.375, false),
        cx,
    );
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("toggle-raster-playback").label(), Some("Pause"));
        assert_eq!(window.find("raster-playback-status").label(), Some("Playing"));
    })
    .unwrap();
    // Time advances only in the renderer; the panel has no independent timer.
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(request(&panel, cx).position(), 1.375);
    press(handle, "space", cx);
    let paused = request(&panel, cx);
    assert!(!paused.is_playing());
    assert!(paused.interpolation());
    assert_eq!(paused.position(), 1.375);
    assert!(paused.revision() > playing.revision());
    cx.update(|cx| {
        let events = events.read(cx);
        assert_eq!(events.len(), 3, "Load, play and pause emit once each; feedback emits nothing");
        assert!(!events.last().unwrap().as_ref().unwrap().is_playing());
    });
    apply_frame(
        &panel,
        handle,
        PlaybackFrame::new(playing.dataset_id().unwrap(), playing.revision(), 3.0, true),
        cx,
    );
    assert_eq!(request(&panel, cx).position(), 1.375);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("toggle-raster-playback").label(), Some("Play"));
        assert_eq!(window.find("raster-playback-status").label(), Some("Paused"));
    })
    .unwrap();
}

fn assert_speed_selection(handle: PanelWindow, selected: u8, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let first = window.find(("raster-playback-speed", 1_usize)).bounds();
        for speed in [1_u8, 2, 3, 5] {
            let button = window.find(("raster-playback-speed", usize::from(speed)));
            assert_eq!(button.checked(), Some(speed == selected), "Exactly the current preset must be highlighted");
            assert_eq!(button.label(), Some(format!("{speed} bands per second").as_str()));
            assert_eq!(button.bounds().top(), first.top(), "Presets must form a horizontal group");
            assert_eq!(button.bounds().size.height, first.size.height, "Preset segments must share the group height");
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn speed_presets_highlight_selection_and_update_live_with_keyboard_access(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    let events = record_commands(&panel, cx);
    load(&panel, handle, cog_paths(&dir, &[5]), cx);
    assert_speed_selection(handle, 3, cx);
    click(handle, "toggle-raster-playback", cx);
    let initial = request(&panel, cx);
    apply_frame(
        &panel,
        handle,
        PlaybackFrame::new(initial.dataset_id().unwrap(), initial.revision(), 0.625, false),
        cx,
    );
    for speed in [1_u8, 2, 3, 5] {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click(("raster-playback-speed", usize::from(speed)), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(request(&panel, cx).bands_per_second(), f64::from(speed));
        assert_speed_selection(handle, speed, cx);
    }
    focus_control(handle, ("raster-playback-speed", 3_usize), cx);
    press(handle, "enter", cx);
    assert_speed_selection(handle, 3, cx);
    press(handle, "tab", cx);
    press(handle, "space", cx);
    assert_speed_selection(handle, 5, cx);
    let updated = request(&panel, cx);
    let count = cx.update(|cx| events.read(cx).len());
    press(handle, "space", cx);
    cx.update(|cx| assert_eq!(events.read(cx).len(), count, "Re-selecting a preset must not issue a command"));
    assert!(updated.interpolation());
    assert!(updated.is_playing());
    assert_eq!(updated.position(), 0.625);
    assert!(updated.revision() > initial.revision());
    assert_eq!(updated.bands_per_second(), 5.0);
}

#[gpui_kit::test]
fn first_last_navigation_and_slider_scrubbing_pause_and_revise_playback(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, cog_paths(&dir, &[5]), cx);
    let initial = request(&panel, cx);
    click(handle, "first-raster-band", cx);
    assert_eq!(request(&panel, cx).revision(), initial.revision());
    click(handle, "toggle-raster-playback", cx);
    let playing = request(&panel, cx);
    apply_frame(
        &panel,
        handle,
        PlaybackFrame::new(playing.dataset_id().unwrap(), playing.revision(), 0.25, false),
        cx,
    );
    click(handle, "first-raster-band", cx);
    assert_eq!(
        request(&panel, cx).position(),
        0.0,
        "Seeking within the selected band must discard its fraction"
    );
    assert!(!request(&panel, cx).is_playing());
    assert!(request(&panel, cx).revision() > playing.revision());
    click(handle, "toggle-raster-playback", cx);
    click(handle, "last-raster-band", cx);
    assert_eq!(request(&panel, cx).position(), 4.0);
    assert!(!request(&panel, cx).is_playing());
    let end = request(&panel, cx);
    click(handle, "last-raster-band", cx);
    assert_eq!(request(&panel, cx).revision(), end.revision());
    focus_control(handle, "first-raster-band", cx);
    press(handle, "enter", cx);
    assert_eq!(request(&panel, cx).position(), 0.0);
    click(handle, "toggle-raster-playback", cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let slider = window.find("raster-band-slider").bounds();
        window.click_at("raster-band-slider", point(slider.size.width * 0.5, slider.size.height * 0.5), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(request(&panel, cx).position(), 2.0);
    assert!(!request(&panel, cx).is_playing());
    click(handle, "toggle-raster-playback", cx);
    click(handle, "previous-raster-band", cx);
    assert_eq!(request(&panel, cx).position(), 1.0);
    assert!(!request(&panel, cx).is_playing());
    click(handle, "next-raster-band", cx);
    assert_eq!(request(&panel, cx).position(), 2.0);
    assert!(request(&panel, cx).revision() > end.revision());
}

#[gpui_kit::test]
fn renderer_feedback_syncs_band_name_slider_and_style_without_emitting_or_seeking(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    let events = record_commands(&panel, cx);
    load(&panel, handle, cog_paths(&dir, &[5]), cx);
    cx.update(|cx| {
        panel.update(cx, |panel, _| {
            let original = panel.playback_request().unwrap();
            let mut bands = original.bands().to_vec();
            bands[1] = bands[1].restyled(ColorMapPreset::Gray, (10.0, 20.0)).unwrap();
            panel.sources[1] = bands[1].clone();
            // Fixture-only replacement: refresh the immutable dataset command too.
            panel.raster_datasets[0].playback = Some(PlaybackRequest::new(
                bands,
                original.position(),
                original.revision(),
                original.is_playing(),
                original.bands_per_second(),
                original.interpolation(),
            ));
        })
    });
    click(handle, "toggle-raster-playback", cx);
    let command = request(&panel, cx);
    let dataset = command.dataset_id().unwrap();
    apply_frame(&panel, handle, PlaybackFrame::new(dataset, command.revision(), 1.25, true), cx);
    let accepted = request(&panel, cx);
    assert_eq!(accepted.revision(), command.revision());
    assert_eq!(accepted.position(), 1.25);
    assert_eq!(accepted.style().palette_for(&accepted.bands()[1]), ColorMapPreset::Gray);
    assert_eq!(accepted.style().range_for(&accepted.bands()[1]), (10.0, 20.0));
    assert_raw_bands_unchanged(&command, &accepted);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let state = panel.read(cx);
        assert_eq!(state.active().unwrap().metadata().band_nr, Some(2));
        assert_eq!(state.band_slider.read(cx).value().end(), 2.25);
        assert_eq!(state.palette.read(cx).selected_value(), Some(&"Gray"));
        assert_eq!(state.min.read(cx).value().as_str(), "10.00");
        assert_eq!(state.max.read(cx).value().as_str(), "20.00");
        assert_eq!(window.find("raster-band-label").label(), Some("Band Frame 2"));
        assert_eq!(window.find("raster-playback-status").label(), Some("Buffering"));
    })
    .unwrap();
    apply_frame(&panel, handle, PlaybackFrame::new(dataset, command.revision(), 1.75, false), cx);
    assert_eq!(request(&panel, cx).position(), 1.75);
    cx.update(|cx| assert_eq!(events.read(cx).len(), 2));
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("raster-playback-status").label(), Some("Playing"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn final_band_fractional_feedback_wraps_to_first_and_pauses_without_losing_its_fraction(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    let events = record_commands(&panel, cx);
    load(&panel, handle, cog_paths(&dir, &[3]), cx);
    click(handle, "toggle-raster-playback", cx);
    let command = request(&panel, cx);
    let dataset = command.dataset_id().unwrap();
    apply_frame(&panel, handle, PlaybackFrame::new(dataset, command.revision(), 2.5, true), cx);
    let final_band = request(&panel, cx);
    assert_eq!(final_band.position(), 2.5);
    assert_eq!(final_band.revision(), command.revision());
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let state = panel.read(cx);
        assert_eq!(state.active().unwrap().metadata().band_nr, Some(3));
        assert_eq!(state.band_slider.read(cx).value().end(), 3.0);
        assert_eq!(window.find("raster-band-label").label(), Some("Band Frame 3"));
        assert_eq!(window.find("raster-playback-status").label(), Some("Buffering"));
    })
    .unwrap();
    apply_frame(&panel, handle, PlaybackFrame::new(dataset, command.revision(), 0.25, false), cx);
    assert_eq!(request(&panel, cx).position(), 0.25);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(panel.read(cx).band_slider.read(cx).value().end(), 1.25);
        assert_eq!(window.find("raster-band-label").label(), Some("Band Frame 1"));
    })
    .unwrap();
    apply_frame(&panel, handle, PlaybackFrame::new(dataset, command.revision(), 2.75, false), cx);
    cx.update(|cx| assert_eq!(events.read(cx).len(), 2, "Feedback must not emit commands, including across wraparound"));
    click(handle, "toggle-raster-playback", cx);
    let paused = request(&panel, cx);
    assert!(!paused.is_playing());
    assert_eq!(paused.position(), 2.75);
    assert!(paused.revision() > command.revision());
}

#[gpui_kit::test]
fn color_map_edits_survive_playback_band_changes_and_looping(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, cog_paths(&dir, &[3]), cx);
    click(handle, "toggle-raster-playback", cx);
    let command = request(&panel, cx);
    click(handle, "raster-palette-reverse", cx);
    click(handle, "raster-clamp-colors", cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
        window.click("raster-max", cx);
        window.press("secondary-a", cx);
        window.input("2000", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let styled = request(&panel, cx);
    let style = styled.style();
    let first = &styled.bands()[0];
    for position in [1.25, 2.75, 0.25] {
        apply_frame(
            &panel,
            handle,
            PlaybackFrame::new(command.dataset_id().unwrap(), command.revision(), position, false),
            cx,
        );
        let current = request(&panel, cx);
        let band = &current.bands()[position.floor() as usize];
        assert!(
            current.style().palette_for(band) == style.palette_for(first),
            "Playback must retain the layer palette"
        );
        assert!(
            current.style().color_map_direction_for(band) == style.color_map_direction_for(first),
            "Playback must retain palette reversal"
        );
        assert_eq!(
            current.style().range_for(band),
            style.range_for(first),
            "Playback must retain the edited value range"
        );
        assert_eq!(
            current.style().clamp_colors_for(band),
            style.clamp_colors_for(first),
            "Playback must retain color clamping"
        );
        assert_raw_bands_unchanged(&command, &current);
        assert_eq!(current.revision(), command.revision(), "Styling must not seek");
        assert!(current.is_playing(), "Styling must not pause playback");
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                panel.read(cx).max.read(cx).value().as_str(),
                "2000",
                "Playback must preserve focused input text"
            );
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn playback_frames_preserve_an_in_progress_range_edit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, cog_paths(&dir, &[3]), cx);
    click(handle, "toggle-raster-playback", cx);
    let command = request(&panel, cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("raster-min", cx);
        window.press("secondary-a", cx);
        window.input("-", cx);
    })
    .unwrap();
    cx.run_until_parked();
    apply_frame(
        &panel,
        handle,
        PlaybackFrame::new(command.dataset_id().unwrap(), command.revision(), 1.25, false),
        cx,
    );
    cx.update_window(handle.into(), |_, window, cx| {
        assert_eq!(
            panel.read(cx).min.read(cx).value().as_str(),
            "-",
            "Frame feedback cannot replace unfinished input"
        );
        window.input("10", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let edited = request(&panel, cx);
    assert!(edited.bands().iter().all(|band| edited.style().range_for(band).0 == -10.0));
    assert_raw_bands_unchanged(&command, &edited);
    assert!(edited.is_playing());
    assert_eq!(edited.revision(), command.revision());
}

#[gpui_kit::test]
fn style_edits_while_playing_refresh_runtime_style_without_replacing_raw_bands_or_seeking(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    let events = record_commands(&panel, cx);
    load(&panel, handle, cog_paths(&dir, &[3]), cx);
    click(handle, "toggle-raster-playback", cx);
    let command = request(&panel, cx);
    let dataset = command.dataset_id().unwrap();
    apply_frame(&panel, handle, PlaybackFrame::new(dataset, command.revision(), 1.25, false), cx);
    click(handle, "raster-palette-reverse", cx);
    let reversed = request(&panel, cx);
    assert!(reversed.style().color_map_direction_for(&reversed.bands()[1]) == ColorMapDirection::Reversed);
    assert_raw_bands_unchanged(&command, &reversed);
    click(handle, "raster-clamp-colors", cx);
    let unclamped = request(&panel, cx);
    assert!(!unclamped.style().clamp_colors_for(&unclamped.bands()[1]));
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.scroll("raster-palette", ScrollDelta::Lines(point(0.0, -1.0)), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let recolored = request(&panel, cx);
    assert_ne!(
        recolored.style().palette_for(&recolored.bands()[1]),
        command.style().palette_for(&command.bands()[1])
    );
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("raster-max", cx);
        window.press("secondary-a", cx);
        window.input("2000", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let styled = request(&panel, cx);
    assert_eq!(styled.style().range_for(&styled.bands()[1]).1, 2000.0);
    cx.update(|cx| {
        let events = events.read(cx);
        assert!(events.len() >= 6, "Load, play, reverse, clamp, palette and range must emit snapshots");
        for event in events.iter().skip(2) {
            let snapshot = event.as_ref().unwrap();
            assert_eq!(snapshot.revision(), command.revision(), "Style edits must not seek the renderer clock");
            assert_eq!(snapshot.dataset_id(), Some(dataset));
            assert_eq!(snapshot.position(), 1.25);
            assert!(snapshot.is_playing());
            assert_raw_bands_unchanged(&command, snapshot);
            assert!(
                snapshot
                    .bands()
                    .iter()
                    .all(|band| snapshot.style().color_map_direction_for(band) == ColorMapDirection::Reversed)
            );
        }
        let emitted = events.last().unwrap().as_ref().unwrap();
        assert!(Arc::ptr_eq(&emitted.bands()[1], &styled.bands()[1]));
    });
    // The renderer can continue reporting the same command revision after a style edit.
    apply_frame(&panel, handle, PlaybackFrame::new(dataset, command.revision(), 1.75, false), cx);
    assert_eq!(request(&panel, cx).position(), 1.75);
    assert_eq!(request(&panel, cx).revision(), command.revision());
}

#[gpui_kit::test]
fn animated_layer_style_edits_do_not_change_other_datasets(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, cog_paths(&dir, &[3, 3]), cx);
    let command = request(&panel, cx);
    let ids: Vec<_> = command.bands().iter().map(|band| band.metadata().id).collect();
    let other_bands = cx.update(|cx| {
        panel
            .read(cx)
            .sources
            .iter()
            .filter(|band| !ids.contains(&band.metadata().id))
            .cloned()
            .collect::<Vec<_>>()
    });
    let other_commands = cx.update(|cx| {
        panel
            .read(cx)
            .raster_datasets
            .iter()
            .filter_map(|dataset| dataset.playback.clone())
            .filter(|other| other.dataset_id() != command.dataset_id())
            .collect::<Vec<_>>()
    });
    click(handle, "raster-palette-reverse", cx);
    click(handle, "raster-clamp-colors", cx);
    click(handle, "last-raster-band", cx);
    let styled = request(&panel, cx);
    assert!(!styled.is_playing());
    assert!(
        styled
            .bands()
            .iter()
            .all(|band| styled.style().color_map_direction_for(band) == ColorMapDirection::Reversed && !styled.style().clamp_colors_for(band))
    );
    assert_raw_bands_unchanged(&command, &styled);
    cx.update(|cx| {
        let state = panel.read(cx);
        for original in &other_bands {
            let current = state.sources.iter().find(|band| band.metadata().id == original.metadata().id).unwrap();
            assert!(Arc::ptr_eq(current, original), "Styling cannot leak into another COG layer");
        }
        for original in &other_commands {
            let current = state
                .raster_datasets
                .iter()
                .filter_map(|dataset| dataset.playback.as_ref())
                .find(|current| current.dataset_id() == original.dataset_id())
                .unwrap();
            assert_raw_bands_unchanged(original, current);
            assert!(current.style() == original.style(), "Runtime overrides cannot leak into another dataset");
        }
    });
}

fn scaled_cog_path(directory: &tempfile::TempDir) -> PathBuf {
    let input = directory.path().join("scaled-input.tif");
    let output = directory.path().join("scaled-playback.tif");
    crate::raster_source::test_support::write_raster(&input, 3857, 3).unwrap();
    let dataset = gdal::Dataset::open_ex(
        &input,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
            ..Default::default()
        },
    )
    .unwrap();
    for nr in 1..=3 {
        let mut band = dataset.rasterband(nr).unwrap();
        band.set_scale(nr as f64 * 0.5).unwrap();
        band.set_offset(nr as f64 * 10.0).unwrap();
    }
    dataset.close().unwrap();
    write_aligned_cog(&input, &output).unwrap();
    output
}

#[gpui_kit::test]
fn value_units_update_all_animation_frames_without_replacing_band_scales(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, vec![scaled_cog_path(&dir)], cx);
    click(handle, "toggle-raster-playback", cx);
    let original = request(&panel, cx);
    click(handle, "raster-unscaling", cx);
    let raw = request(&panel, cx);
    let initial_range = original.style().range_for(&original.bands()[0]);
    let initial_scale = original.style().scale_for(&original.bands()[0]);
    assert_eq!(
        raw.style().range_for(&raw.bands()[0]),
        (
            ((f64::from(initial_range.0) - initial_scale.offset) / initial_scale.scale) as f32,
            ((f64::from(initial_range.1) - initial_scale.offset) / initial_scale.scale) as f32,
        )
    );
    assert!(raw.bands().iter().all(|band| {
        !raw.style().unscaling_for(band)
            && raw.style().scale_for(band) == geo::RasterScale::default()
            && raw.style().range_for(band) == raw.style().range_for(&raw.bands()[0])
    }));
    assert_raw_bands_unchanged(&original, &raw);
    apply_frame(
        &panel,
        handle,
        PlaybackFrame::new(original.dataset_id().unwrap(), original.revision(), 2.25, false),
        cx,
    );
    click(handle, "raster-unscaling", cx);
    let physical = request(&panel, cx);
    let raw_range = raw.style().range_for(&raw.bands()[2]);
    assert_eq!(
        physical.style().range_for(&physical.bands()[2]),
        (raw_range.0 * 1.5 + 30.0, raw_range.1 * 1.5 + 30.0)
    );
    for (ix, band) in physical.bands().iter().enumerate() {
        assert!(physical.style().unscaling_for(band));
        assert_eq!(physical.style().scale_for(band).scale, (ix + 1) as f64 * 0.5);
        assert_eq!(physical.style().scale_for(band).offset, (ix + 1) as f64 * 10.0);
        assert_eq!(
            physical.style().range_for(band),
            physical.style().range_for(&physical.bands()[0]),
            "The display range belongs to the animated layer"
        );
        assert_eq!(band.metadata().scale, original.bands()[ix].metadata().scale);
    }
    assert_raw_bands_unchanged(&original, &physical);
    assert!(physical.is_playing());
    assert_eq!(physical.revision(), original.revision());
    assert_eq!(physical.position(), 2.25);
}

fn edit_maximum(handle: PanelWindow, maximum: f32, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("raster-max", cx);
        window.press("secondary-a", cx);
        window.input(&maximum.to_string(), cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn drag_range_maximum(handle: PanelWindow, fraction: f32, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let bounds = window.find("raster-range-slider").bounds();
        // Each preceding text edit expands the domain, putting the upper thumb at its end.
        let start = point(bounds.right() - px(8.0), bounds.center().y);
        let end = point(bounds.left() + bounds.size.width * fraction, bounds.center().y);
        window.drag(start, end, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn assert_range_edit_resources(panel: &Entity<RasterPanel>, original: &PlaybackRequest, colors: &Arc<[inf::Color]>, cx: &mut TestAppContext) {
    let current = request(panel, cx);
    assert_raw_bands_unchanged(original, &current);
    assert_eq!(current.dataset_id(), original.dataset_id());
    assert_eq!(current.position(), original.position());
    assert_eq!(current.revision(), original.revision(), "Range edits cannot seek");
    assert!(current.is_playing(), "Range edits cannot pause");
    assert_eq!(current.bands_per_second(), original.bands_per_second());
    assert!(
        current
            .bands()
            .iter()
            .all(|band| current.style().range_for(band) == current.style().range_for(&current.bands()[0]))
    );
    cx.update(|cx| {
        let state = panel.read(cx);
        assert!(Arc::ptr_eq(colors, &state.legend_colors), "Range-only edits must reuse palette samples");
        assert!(state.tile_errors_task.is_some());
        assert!(state.error.is_none());
        for source in original.bands() {
            let index = state.source_indices[&source.metadata().id];
            assert!(Arc::ptr_eq(source, &state.sources[index]));
        }
    });
}

#[gpui_kit::test]
fn a_hundred_range_input_and_slider_edits_share_many_band_sources_palette_and_error_channel(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    let events = record_commands(&panel, cx);
    load(&panel, handle, cog_paths(&dir, &[32]), cx);
    click(handle, "toggle-raster-playback", cx);
    let playing = request(&panel, cx);
    apply_frame(
        &panel,
        handle,
        PlaybackFrame::new(playing.dataset_id().unwrap(), playing.revision(), 0.25, false),
        cx,
    );
    let original = request(&panel, cx);
    assert_eq!(original.bands().len(), 32);
    let (colors, source_indices, dataset_indices) = cx.update(|cx| {
        let state = panel.read(cx);
        (state.legend_colors.clone(), state.source_indices.clone(), state.dataset_indices.clone())
    });
    let mut previous = original.style().range_for(&original.bands()[0]);
    for update in 0..100 {
        if update % 2 == 0 {
            edit_maximum(handle, 2000.0 + update as f32, cx);
        } else {
            drag_range_maximum(handle, 0.6, cx);
        }
        let current = request(&panel, cx);
        let range = current.style().range_for(&current.bands()[0]);
        assert_ne!(range, previous, "UI update {update} must actually change the shader range");
        previous = range;
        assert_range_edit_resources(&panel, &original, &colors, cx);
        cx.update(|cx| {
            let state = panel.read(cx);
            assert_eq!(state.source_indices, source_indices);
            assert_eq!(state.dataset_indices, dataset_indices);
        });
    }
    cx.update(|cx| {
        let commands = events.read(cx);
        assert!(commands.len() >= 102, "Every actual range edit must emit a runtime command");
        for command in commands.iter().skip(2).map(|command| command.as_ref().unwrap()) {
            assert_raw_bands_unchanged(&original, command);
            assert_eq!(command.revision(), original.revision());
            assert!(command.is_playing());
        }
        assert!(panel.read(cx).load_error.is_none());
    });
    original.bands()[0].report_tile_error(&anyhow::anyhow!("retained-range-edit-error-channel"));
    cx.run_until_parked();
    cx.update(|cx| {
        assert!(
            panel.read(cx).load_error.as_ref().unwrap().contains("retained-range-edit-error-channel"),
            "The tile error watcher must still receive errors from the original source"
        );
    });
}

#[gpui_kit::test]
fn static_style_changes_watch_the_replacement_sources_error_channel(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, cog_paths(&dir, &[1]), cx);
    let original = cx.update(|cx| panel.read(cx).active().unwrap().clone());
    edit_maximum(handle, 2000.0, cx);
    let styled = cx.update(|cx| panel.read(cx).active().unwrap().clone());
    assert_ne!(styled.id(), original.id());
    assert_eq!(styled.metadata().id, original.metadata().id);
    styled.report_tile_error(&anyhow::anyhow!("static-restyle-channel"));
    cx.run_until_parked();
    cx.update(|cx| assert!(panel.read(cx).load_error.as_ref().unwrap().contains("static-restyle-channel")));
}

#[gpui_kit::test]
fn invalid_or_stale_feedback_cannot_change_the_visible_selection(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, cog_paths(&dir, &[5]), cx);
    let command = request(&panel, cx);
    let dataset = command.dataset_id().unwrap();
    for frame in [
        PlaybackFrame::new(command.bands()[1].metadata().id, command.revision(), 2.0, true),
        PlaybackFrame::new(dataset, command.revision().wrapping_add(1), 2.0, true),
        PlaybackFrame::new(dataset, command.revision(), f64::NAN, true),
        PlaybackFrame::new(dataset, command.revision(), -1.0, true),
        PlaybackFrame::new(dataset, command.revision(), 5.0, true),
    ] {
        apply_frame(&panel, handle, frame, cx);
        assert_eq!(request(&panel, cx).position(), 0.0);
    }
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("raster-band-label").label(), Some("Band Frame 1"));
        assert_eq!(window.find("raster-playback-status").label(), Some("Paused"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn switching_datasets_pauses_and_clear_cancels_playback_and_late_feedback(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (panel, handle) = open_panel(cx);
    let events = record_commands(&panel, cx);
    load(&panel, handle, cog_paths(&dir, &[5, 3]), cx);
    click(handle, "toggle-raster-playback", cx);
    let previous = request(&panel, cx);
    let late = PlaybackFrame::new(previous.dataset_id().unwrap(), previous.revision(), 1.5, true);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let switched = request(&panel, cx);
    assert_ne!(switched.dataset_id(), previous.dataset_id());
    assert!(switched.revision() > previous.revision());
    assert!(!switched.is_playing());
    apply_frame(&panel, handle, late, cx);
    assert_eq!(request(&panel, cx).position(), 0.0);
    click(handle, "toggle-raster-playback", cx);
    cx.update_window(handle.into(), |_, window, cx| panel.update(cx, |panel, cx| panel.clear(window, cx)))
        .unwrap();
    cx.run_until_parked();
    apply_frame(&panel, handle, late, cx);
    assert_playback_visibility(&panel, handle, false, cx);
    cx.update(|cx| assert!(events.read(cx).last().unwrap().is_none()));
}

#[gpui_kit::test]
fn reinstalling_the_same_path_discards_old_identity_and_playback_state(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let paths = cog_paths(&dir, &[5]);
    let (panel, handle) = open_panel(cx);
    load(&panel, handle, paths.clone(), cx);
    click(handle, "toggle-raster-playback", cx);
    let original = request(&panel, cx);
    let late = PlaybackFrame::new(original.dataset_id().unwrap(), original.revision(), 2.5, true);
    apply_frame(&panel, handle, late, cx);
    load(&panel, handle, paths, cx);
    let reopened = request(&panel, cx);
    assert_ne!(reopened.dataset_id(), original.dataset_id());
    assert!(reopened.revision() > original.revision());
    assert!(!reopened.is_playing());
    assert_eq!(reopened.position(), 0.0);
    apply_frame(&panel, handle, late, cx);
    assert_eq!(request(&panel, cx).position(), 0.0);
    cx.update(|cx| assert_eq!(panel.read(cx).sources.len(), 5));
}
