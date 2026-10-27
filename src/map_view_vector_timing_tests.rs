//! Opt-in end-to-end file-drop measurement. The test drives the real view,
//! loader, worker, and GPU readback, but uses a local background-only basemap.
//! Headless GPUI frame application/drawing is not physical monitor presentation;
//! its deterministic executor also does not model production thread scheduling.

use gpui_kit::component::{Root, Theme, ThemeMode};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{FileDropEvent, InputEvent as _, TestAppContext, px, size};

use super::*;

const BACKGROUND_STYLE: &str = r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##;

#[gpui_kit::test]
#[ignore = "manual local GPU/UI timing; requires MAP_TEST_VECTOR and a graphics device; run in release mode"]
fn local_vector_drop_to_display_timings(cx: &mut TestAppContext) {
    let _ = env_logger::builder().is_test(true).format_timestamp_millis().try_init();
    let path = PathBuf::from(std::env::var_os("MAP_TEST_VECTOR").expect("Set MAP_TEST_VECTOR to the local spatial vector file"));
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        cx.set_reduce_motion(true);
    });
    let renderer = MapRenderer::with_test_style(BACKGROUND_STYLE).unwrap();
    let frames = renderer.frames();
    let mut view = None;
    let handle = cx.open_window(size(px(1200.0), px(760.0)), |window, cx| {
        let map = cx.new(|cx| {
            let mut map = MapView::new(window, cx);
            // GPUI's deterministic scheduler rejects foreign-thread wakeups.
            // Poll the real worker channel on the test thread, then use the
            // production apply_frame path; the app's async frame wakeup itself
            // is not part of this headless measurement.
            map._frames_task = None;
            map._feature_task = None;
            map.renderer = Some(renderer);
            map
        });
        view = Some(map.clone());
        Root::new(map, window, cx)
    });
    let view = view.unwrap();
    let warmup = Instant::now();
    loop {
        assert!(warmup.elapsed() < Duration::from_secs(30), "Local background failed to initialize");
        cx.run_until_parked();
        let ready = cx
            .update_window(handle.into(), |_, window, cx| {
                while let Ok(frame) = frames.try_recv() {
                    view.update(cx, |map, cx| map.apply_frame(frame, window, cx));
                }
                window.render_frame(cx);
                assert!(view.read(cx).error.is_none(), "{:?}", view.read(cx).error);
                view.read(cx).image.is_some()
            })
            .unwrap();
        if ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    log::info!(target: "map::vector_load", "stage=ui-timing-warmup elapsed={:?} (excluded from drop timing)", warmup.elapsed());

    let background = cx.update(|cx| view.read(cx).image.clone().unwrap());
    let dropped = Instant::now();
    cx.update_window(handle.into(), |_, window, cx| {
        let position = window.find("map-surface").bounds().center();
        window.dispatch_event(
            FileDropEvent::Entered {
                position,
                paths: ExternalPaths([path.clone()].into_iter().collect()),
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(FileDropEvent::Submit { position }.to_platform_input(), cx);
        assert!(view.read(cx).raster_panel.read(cx).is_loading(), "Drop must invoke the production loader");
    })
    .unwrap();
    let mut loader_finished = None;
    let mut first_requested_frame = None;
    let mut first_data_frame = None;
    loop {
        assert!(dropped.elapsed() < Duration::from_secs(90), "Dropped vector did not display within 90 seconds");
        cx.run_until_parked();
        let displayed = cx
            .update_window(handle.into(), |_, window, cx| {
                while let Ok(frame) = frames.try_recv() {
                    view.update(cx, |map, cx| map.apply_frame(frame, window, cx));
                }
                window.render_frame(cx);
                let map = view.read(cx);
                assert!(map.error.is_none(), "{:?}", map.error);
                assert!(map.raster_panel.read(cx).load_error().is_none(), "{:?}", map.raster_panel.read(cx).load_error());
                if map.vector.is_some() && !map.raster_panel.read(cx).is_loading() && loader_finished.is_none() {
                    loader_finished = Some(dropped.elapsed());
                }
                if map.traced_frame_vector.is_some() && first_requested_frame.is_none() {
                    first_requested_frame = Some(dropped.elapsed());
                }
                // This background is independent of camera position. No tiles,
                // labels, or other geometry can create a pixel difference.
                let changed_pixels = map
                    .image
                    .as_ref()
                    .and_then(|image| image.as_bytes(0))
                    .map(|pixels| {
                                            let baseline = background.as_bytes(0).unwrap();
                                            assert_eq!(pixels.len(), baseline.len(), "Timing comparison requires the same viewport");
                                            pixels.chunks_exact(4).zip(baseline.chunks_exact(4)).filter(|(pixel, baseline)| pixel != baseline).count()
                                        })
                    .unwrap_or(0);
                if map.vector.is_some() && changed_pixels > 0 && first_data_frame.is_none() {
                    first_data_frame = Some(dropped.elapsed());
                    log::info!(target: "map::vector_load", "path={path:?} stage=ui-first-data-bearing-frame-drawn since_drop={:?} loader_finished={loader_finished:?} first_requested_frame={first_requested_frame:?} changed_pixels={changed_pixels} (headless GPUI draw, not physical presentation)", dropped.elapsed());
                }
                if first_data_frame.is_some() && map.raster_panel.read(cx).render_load_progress().is_none() {
                    assert!(changed_pixels > 0, "Completion must retain a data-bearing frame");
                    log::info!(target: "map::vector_load", "path={path:?} stage=ui-render-progress-complete since_drop={:?} first_data_frame={first_data_frame:?}", dropped.elapsed());
                    true
                } else {
                    if map.vector.is_some() {
                        assert!(map.raster_panel.read(cx).render_load_progress().is_some(), "Loading cannot finish on a background-only frame");
                    }
                    false
                }
            })
            .unwrap();
        if displayed {
            break;
        }
        cx.executor().advance_clock(Duration::from_millis(16));
        std::thread::sleep(Duration::from_millis(5));
    }
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |map, cx| {
            map.raster_panel.update(cx, |panel, cx| panel.clear(window, cx));
            map._frames_task = None;
            map._feature_task = None;
            map.renderer = None;
        });
    })
    .unwrap();
}
