//! Headless UI integration tests for the panel -> view -> playback feedback seam.

use gdal::Metadata as _;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, InputEvent, TestAppContext, point, px, size};

use super::*;

struct Harness {
    _directory: tempfile::TempDir,
    view: Entity<MapView>,
    window: AnyWindowHandle,
    events: Entity<usize>,
}

impl Harness {
    fn new(cx: &mut TestAppContext, datasets: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = (0..datasets).map(|ix| write_cog(directory.path(), ix)).collect();
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx).unwrap();
            cx.set_reduce_motion(true);
        });
        let mut view = None;
        let window = cx.open_window(size(px(1000.0), px(1100.0)), |window, cx| {
            let map = cx.new(|cx| MapView::with_renderer(None, window, cx));
            view = Some(map.clone());
            Root::new(map, window, cx)
        });
        let view = view.unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |view, cx| view.open_paths(paths, cx));
        })
        .unwrap();
        cx.run_until_parked();
        let panel = cx.update(|cx| view.read(cx).raster_panel.clone());
        let events = cx.new(|cx: &mut Context<usize>| {
            cx.subscribe(&panel, |events, _, _, _| *events += 1).detach();
            0
        });
        let harness = Self {
            _directory: directory,
            view,
            window: window.into(),
            events,
        };
        assert_eq!(harness.command(cx).bands().len(), 5, "Fixture must retain every native COG band");
        harness
    }

    fn command(&self, cx: &mut TestAppContext) -> PlaybackRequest {
        cx.update(|cx| {
            self.view
                .read(cx)
                .raster_panel
                .read(cx)
                .playback_request()
                .expect("Expected a native multiband COG command")
        })
    }

    fn click(&self, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn frame(&self, feedback: Option<PlaybackFrame>, cx: &mut TestAppContext) -> MapFrame {
        cx.update(|cx| {
            let view = self.view.read(cx);
            MapFrame {
                viewport: view.viewport.unwrap(),
                basemap: basemap_style(cx),
                tile_boundaries: view.tile_boundaries,
                raster_layer_id: view.raster.as_ref().map(|source| source.metadata().id),
                vector_layer_id: None,
                settled: true,
                playback: feedback,
                image: Ok(image()),
            }
        })
    }

    fn apply(&self, frame: MapFrame, cx: &mut TestAppContext) {
        cx.update_window(self.window, |_, window, cx| {
            self.view.update(cx, |view, cx| view.apply_frame(frame, window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
}

fn write_cog(directory: &std::path::Path, ix: usize) -> PathBuf {
    let input = directory.join(format!("input-{ix}.tif"));
    let output = directory.join(format!("playback-{ix}.tif"));
    crate::raster_source::test_support::write_raster(&input, 3857, 5).unwrap();
    let dataset = gdal::Dataset::open(input).unwrap();
    for band in 1..=5 {
        let mut raster_band = dataset.rasterband(band).unwrap();
        raster_band.set_description(&format!("Frame {band}")).unwrap();
        raster_band.set_scale(band as f64 * 0.5).unwrap();
        raster_band.set_offset(band as f64 * -3.0).unwrap();
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
    dataset
        .create_copy(&gdal::DriverManager::get_driver_by_name("COG").unwrap(), &output, &options)
        .unwrap()
        .close()
        .unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&output).unwrap();
    file.set_len(file.metadata().unwrap().len().max(16 * 1024)).unwrap();
    assert!(RasterSource::open(&output).unwrap()[0].metadata().source_format == tiler::LayerSourceType::CloudOptimizedGeoTiff);
    output
}

fn image() -> Arc<RenderImage> {
    Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(8, 8))]))
}

fn requested_key(view: &MapView) -> RenderKey {
    let key = view.requested.as_ref().unwrap();
    RenderKey {
        map: key.map,
        raster_id: key.raster_id,
        playback_revision: key.playback_revision,
        playback_style: key.playback_style,
        vector_id: key.vector_id,
        vector_style_id: key.vector_style_id,
        vector_color: key.vector_color.clone(),
        opacity: key.opacity,
        reduce_motion: key.reduce_motion,
        basemap: key.basemap,
        tile_boundaries: key.tile_boundaries,
    }
}

fn feedback(command: &PlaybackRequest, position: f64, buffering: bool) -> PlaybackFrame {
    PlaybackFrame::new(command.dataset_id().unwrap(), command.revision(), position, buffering)
}

#[gpui_kit::test]
fn accepted_feedback_syncs_panel_band_slider_and_status_without_a_command_loop(cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    harness.click("toggle-raster-playback", cx);
    let command = harness.command(cx);
    assert!(command.is_playing());
    let key = cx.update(|cx| requested_key(harness.view.read(cx)));
    let event_count = cx.update(|cx| *harness.events.read(cx));
    for (position, buffering, label) in [(1.25, false, "Band Frame 2"), (2.75, true, "Band Frame 3")] {
        let accepted = feedback(&command, position, buffering);
        let frame = harness.frame(Some(accepted), cx);
        let pixels = frame.image.as_ref().unwrap().clone();
        harness.apply(frame, cx);
        cx.update_window(harness.window, |_, window, cx| {
            window.render_frame(cx);
            let view = harness.view.read(cx);
            assert_eq!(view.playback_frame, Some(accepted));
            assert!(Arc::ptr_eq(view.image.as_ref().unwrap(), &pixels));
            assert!(view.requested.as_ref() == Some(&key), "Feedback must not invalidate the render request key");
            assert_eq!(*harness.events.read(cx), event_count, "Owner feedback cannot emit a new RasterEvent");
            assert_eq!(window.find("raster-band-label").label(), Some(label));
            assert_eq!(
                window.find("raster-playback-status").label(),
                Some(if buffering { "Buffering" } else { "Playing" })
            );
        })
        .unwrap();
        let synced = harness.command(cx);
        assert_eq!(synced.position(), position);
        assert_eq!(synced.revision(), command.revision());
        assert!(synced.is_playing());
    }
}

#[gpui_kit::test]
fn pause_and_seek_revisions_reject_stale_images_feedback_and_errors(cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    harness.click("toggle-raster-playback", cx);
    let playing = harness.command(cx);
    let accepted = feedback(&playing, 1.5, false);
    let frame = harness.frame(Some(accepted), cx);
    harness.apply(frame, cx);
    let mut late = harness.frame(Some(feedback(&playing, 3.5, false)), cx);
    late.image = Err(anyhow::anyhow!("obsolete playback failure"));
    harness.click("toggle-raster-playback", cx);
    let paused = harness.command(cx);
    assert!(!paused.is_playing());
    assert_eq!(paused.position(), 1.5);
    assert!(paused.revision() > playing.revision());
    harness.apply(late, cx);
    cx.update(|cx| {
        let view = harness.view.read(cx);
        assert!(view.error.is_none());
        assert!(view.playback_frame.is_none());
    });
    let current = feedback(&paused, 1.5, false);
    let frame = harness.frame(Some(current), cx);
    harness.apply(frame, cx);
    let stale = harness.frame(Some(current), cx);
    harness.click("last-raster-band", cx);
    let seek = harness.command(cx);
    assert_eq!(seek.position(), 4.0);
    assert!(seek.revision() > paused.revision());
    harness.apply(stale, cx);
    let missing_identity = harness.frame(None, cx);
    harness.apply(missing_identity, cx);
    assert_eq!(harness.command(cx).position(), 4.0);
    cx.update(|cx| assert!(harness.view.read(cx).playback_frame.is_none()));
}

#[gpui_kit::test]
fn matching_playback_error_identity_reaches_the_existing_map_error_banner(cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    let command = harness.command(cx);
    let mut frame = harness.frame(Some(feedback(&command, 0.0, true)), cx);
    frame.image = Err(anyhow::anyhow!("Map rendering timed out"));
    harness.apply(frame, cx);
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("map-error-banner").visible());
        assert!(harness.view.read(cx).error.as_ref().unwrap().contains("Map rendering timed out"));
        assert_eq!(window.find("raster-playback-status").label(), Some("Buffering"));
    })
    .unwrap();
}

fn assert_source_switch_and_clear(playing: bool, cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 2);
    if playing {
        harness.click("toggle-raster-playback", cx);
    }
    let original = harness.command(cx);
    let old_frame = harness.frame(Some(feedback(&original, 2.5, false)), cx);
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.scroll("raster-band", ScrollDelta::Lines(point(0.0, 1.0)), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let switched = harness.command(cx);
    assert_ne!(switched.dataset_id(), original.dataset_id());
    assert!(!switched.is_playing());
    harness.apply(old_frame, cx);
    assert_eq!(harness.command(cx).position(), 0.0);
    let late = harness.frame(Some(feedback(&switched, 1.5, false)), cx);
    cx.update_window(harness.window, |_, window, cx| window.dispatch_action(Box::new(ClearRasters), cx))
        .unwrap();
    cx.run_until_parked();
    harness.apply(late, cx);
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        let view = harness.view.read(cx);
        assert!(view.raster.is_none() && view.playback.is_none() && view.playback_frame.is_none());
        assert!(view.raster_panel.read(cx).playback_request().is_none());
        assert!(window.try_find("raster-playback-controls").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn switching_and_clearing_paused_datasets_rejects_late_playback_frames(cx: &mut TestAppContext) {
    assert_source_switch_and_clear(false, cx);
}

#[gpui_kit::test]
fn switching_and_clearing_playing_datasets_rejects_late_playback_frames(cx: &mut TestAppContext) {
    assert_source_switch_and_clear(true, cx);
}

#[gpui_kit::test]
fn restyling_playback_updates_request_style_without_refitting_camera_or_seeking(cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    harness.click("toggle-raster-playback", cx);
    let command = harness.command(cx);
    let frame = harness.frame(Some(feedback(&command, 1.25, false)), cx);
    harness.apply(frame, cx);
    let original_camera = cx.update(|cx| harness.view.read(cx).camera);
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click_at("map-surface", point(px(20.0), px(20.0)), cx);
        window.press("right", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let camera = cx.update(|cx| harness.view.read(cx).camera);
    assert_ne!(
        camera, original_camera,
        "The regression must preserve a user-panned camera, not merely refit the same extent"
    );
    let style = command.style();
    harness.click("raster-palette-reverse", cx);
    let styled = harness.command(cx);
    assert_eq!(styled.revision(), command.revision());
    assert_eq!(styled.position(), 1.25);
    assert!(styled.is_playing());
    assert!(styled.style() != style);
    assert!(styled.shares_bands(&command));
    cx.update(|cx| {
        let view = harness.view.read(cx);
        assert_eq!(view.camera, camera);
        let requested = view.requested.as_ref().unwrap();
        assert_eq!(requested.map.camera, camera);
        assert_eq!(requested.playback_revision, Some(command.revision()));
        assert_eq!(requested.raster_id, Some(styled.bands()[0].id()));
        assert!(requested.playback_style == Some(styled.style()));
        assert_eq!(view.playback_frame, Some(feedback(&command, 1.25, false)));
        assert!(Arc::ptr_eq(view.raster.as_ref().unwrap(), &command.bands()[0]));
    });
    let frame = harness.frame(Some(feedback(&styled, 2.5, false)), cx);
    harness.apply(frame, cx);
    let advanced = harness.command(cx);
    assert!(
        advanced
            .bands()
            .iter()
            .all(|band| advanced.style().color_map_direction_for(band) == inf::colormap::ColorMapDirection::Reversed)
    );
    assert!(advanced.is_playing());
    assert_eq!(advanced.position(), 2.5);
    assert_eq!(advanced.revision(), command.revision());
    cx.update(|cx| assert!(Arc::ptr_eq(harness.view.read(cx).raster.as_ref().unwrap(), &command.bands()[0])));
}

#[gpui_kit::test]
fn repeated_range_updates_keep_playing_feedback_position_and_raw_band_allocation(cx: &mut TestAppContext) {
    assert_range_updates_preserve_playback(true, cx);
}

#[gpui_kit::test]
fn repeated_range_updates_keep_paused_feedback_position_and_raw_band_allocation(cx: &mut TestAppContext) {
    assert_range_updates_preserve_playback(false, cx);
}

fn assert_range_updates_preserve_playback(playing: bool, cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    if playing {
        harness.click("toggle-raster-playback", cx);
    }
    let original = harness.command(cx);
    let accepted = feedback(&original, 2.25, true);
    let frame = harness.frame(Some(accepted), cx);
    harness.apply(frame, cx);
    cx.update_window(harness.window, |_, window, cx| {
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
    })
    .unwrap();
    cx.run_until_parked();
    // GPUI's first move begins the native drag; subsequent moves change the value.
    move_range_thumb(&harness, 0.05, cx);
    let primed = harness.command(cx);
    let mut previous_range = primed.style().range_for(&primed.bands()[2]);
    let mut previous = cx.update(|cx| requested_key(harness.view.read(cx)));
    for update in 1..=32 {
        move_range_thumb(&harness, 0.1 + update as f32 * 0.01, cx);
        let command = harness.command(cx);
        let range = command.style().range_for(&command.bands()[2]);
        assert_ne!(range, previous_range, "Drag move {update} must change the effective range");
        previous_range = range;
        assert!(command.shares_bands(&original));
        assert_eq!(command.position(), accepted.position());
        assert_eq!(command.revision(), original.revision());
        assert_eq!(command.is_playing(), playing);
        assert!(command.bands().iter().zip(original.bands()).all(|(band, raw)| Arc::ptr_eq(band, raw)));
        cx.update(|cx| {
            let view = harness.view.read(cx);
            let key = requested_key(view);
            assert!(key != previous, "Each effective range edit must invalidate the render key");
            assert!(key.map == previous.map);
            assert_eq!(key.raster_id, previous.raster_id);
            assert!(key.playback_style == Some(command.style()));
            assert_eq!(view.playback_frame, Some(accepted));
            assert!(Arc::ptr_eq(view.raster.as_ref().unwrap(), &original.bands()[0]));
            previous = key;
        });
        assert!(command.bands().iter().all(|band| command.style().range_for(band) == range));
    }
}

#[gpui_kit::test]
fn style_events_preserve_navigation_but_a_seek_cancels_it(cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("raster-min", cx);
        window.press("secondary-a", cx);
        harness.view.update(cx, |view, _| {
            view.drag = Some(MapDrag::new(point(px(20.0), px(20.0)), Instant::now()));
            view.coast = Some(MapCoast {
                velocity: (100.0, 0.0),
                sampled_at: Instant::now(),
            });
        });
        window.input("40", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let view = harness.view.read(cx);
        assert!(view.drag.is_some());
        assert!(view.coast.is_some());
        assert_eq!(
            view.playback.as_ref().unwrap().style().range_for(&view.playback.as_ref().unwrap().bands()[0]).0,
            40.0
        );
    });
    harness.click("next-raster-band", cx);
    cx.update(|cx| {
        let view = harness.view.read(cx);
        assert!(view.drag.is_none());
        assert!(view.coast.is_none());
    });
}

#[gpui_kit::test]
fn playback_band_refreshes_keep_the_hover_value_visible_until_sampling_finishes(cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    harness.click("toggle-raster-playback", cx);
    let command = harness.command(cx);
    cx.update_window(harness.window, |_, window, cx| window.hover("map-surface", cx)).unwrap();
    cx.run_until_parked();
    let first = hover_value(&harness, cx);
    for (position, expected) in [(1.25, 78.0), (3.25, 196.0), (0.25, first)] {
        // Neighbour prefetches evict earlier bands' tiles, so even looping back
        // keeps the displayed value until the first band is sampled again.
        let immediate = hover_value(&harness, cx);
        let frame = harness.frame(Some(feedback(&command, position, false)), cx);
        cx.update_window(harness.window, |_, window, cx| {
            harness.view.update(cx, |view, cx| view.apply_frame(frame, window, cx));
            window.render_frame(cx);
            let text = window.find("pixel-info").label().unwrap().to_owned();
            assert_eq!(
                text.split_whitespace().nth(4).and_then(|value| value.parse::<f64>().ok()),
                Some(immediate),
                "Band refreshes must not flash a coordinates-only readout: {text}"
            );
        })
        .unwrap();
        cx.run_until_parked();
        let value = hover_value(&harness, cx);
        assert!((value - expected).abs() <= 2.0, "Expected the new band's scaled value {expected}, got {value}");
    }
}

#[gpui_kit::test]
fn hover_uses_shared_unscaling_and_the_accepted_band_without_restyling_sources(cx: &mut TestAppContext) {
    let harness = Harness::new(cx, 1);
    let original = harness.command(cx);
    let frame = harness.frame(Some(feedback(&original, 1.25, false)), cx);
    harness.apply(frame, cx);
    cx.update_window(harness.window, |_, window, cx| window.hover("map-surface", cx)).unwrap();
    cx.run_until_parked();
    let physical = hover_value(&harness, cx);
    assert!((physical - 78.0).abs() <= 1.0, "Expected band 2's scaled center sample, got {physical}");
    harness.click("raster-unscaling", cx);
    let raw_command = harness.command(cx);
    assert!(raw_command.shares_bands(&original));
    assert!(!raw_command.style().unscaling_for(&raw_command.bands()[1]));
    assert!(raw_command.bands()[1].unscaling_enabled(), "Raw band sources must remain immutable");
    cx.update_window(harness.window, |_, window, cx| window.hover("map-surface", cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(hover_value(&harness, cx), physical + 6.0);
    // Pointer activation preserves focus, so Tab to the checkbox before toggling at this same pointer position.
    cx.update_window(harness.window, |_, window, cx| {
        for _ in 0..20 {
            if window.find("raster-unscaling").focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find("raster-unscaling").focused(), Some(true));
        assert_eq!(window.find("raster-unscaling").checked(), Some(false));
        window.press("space", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(hover_value(&harness, cx), physical);
    let restored = harness.command(cx);
    assert!(restored.style().unscaling_for(&restored.bands()[1]));
    let frame = harness.frame(Some(feedback(&restored, 3.25, false)), cx);
    harness.apply(frame, cx);
    let later = hover_value(&harness, cx);
    assert!((later - 196.0).abs() <= 2.0, "Expected band 4's own scale and offset, got {later}");
    cx.update(|cx| assert!(Arc::ptr_eq(harness.view.read(cx).raster.as_ref().unwrap(), &original.bands()[0])));
}

fn hover_value(harness: &Harness, cx: &mut TestAppContext) -> f64 {
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        window.find("pixel-info").label().unwrap().split_whitespace().nth(4).unwrap().parse().unwrap()
    })
    .unwrap()
}

fn move_range_thumb(harness: &Harness, fraction: f32, cx: &mut TestAppContext) {
    cx.update_window(harness.window, |_, window, cx| {
        window.render_frame(cx);
        let range = window.find("raster-range-slider").bounds();
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
    })
    .unwrap();
    cx.run_until_parked();
}
