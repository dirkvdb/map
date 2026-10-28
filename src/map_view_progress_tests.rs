use gpui_kit::component::{Root, ThemeMode};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, TestAppContext, px, size};

use super::*;
use crate::raster_panel::progress_tests::ControlledLoad;

fn open_map(cx: &mut TestAppContext) -> (Entity<MapView>, AnyWindowHandle) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        init(cx).unwrap();
        cx.set_reduce_motion(true);
    });
    let mut view = None;
    let handle = cx.open_window(size(px(900.0), px(600.0)), |window, cx| {
        let map = cx.new(|cx| MapView::with_renderer(None, window, cx));
        view = Some(map.clone());
        Root::new(map, window, cx)
    });
    cx.run_until_parked();
    (view.unwrap(), handle.into())
}

fn vector() -> Arc<VectorSource> {
    vector_named("progress.geojson")
}

fn vector_named(name: &str) -> Arc<VectorSource> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(name);
    std::fs::write(
        &path,
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[4,51]}}]}"#,
    )
    .unwrap();
    VectorSource::open(&path).unwrap().unwrap().remove(0)
}

fn deliver_frame(view: &Entity<MapView>, source: u64, settled: bool, image: anyhow::Result<Arc<RenderImage>>, window: &mut Window, cx: &mut App) {
    view.update(cx, |view, cx| {
        view.apply_frame(
            MapFrame {
                playback: None,
                viewport: view.viewport.unwrap(),
                basemap: basemap_style(cx),
                tile_boundaries: view.tile_boundaries,
                raster_layer_id: None,
                vector_layer_id: Some(source),
                settled,
                image,
            },
            window,
            cx,
        );
    });
    window.render_frame(cx);
}

#[gpui_kit::test]
fn progress_phases_remain_visible_until_a_matching_settled_frame_is_applied(cx: &mut TestAppContext) {
    let (view, window) = open_map(cx);
    let panel = cx.update(|cx| view.read(cx).raster_panel.clone());
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let progress = window.find("vector-load-progress");
        assert_eq!(progress.label(), Some("Opening datasets"));
        assert!(progress.value().is_none(), "Unknown work cannot report a percentage");
        assert_eq!(window.find("vector-load-status").label(), Some("Opening datasets"));
    })
    .unwrap();

    load.report_conversion(0.42);
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("vector-load-progress").label(), Some("Converting vectors · 42%"));
        assert_eq!(window.find("vector-load-status").label(), Some("Converting vectors · 42%"));
        assert!(window.find("vector-load-status").bounds().right() <= window.find("open-settings").bounds().left());
    })
    .unwrap();
    load.report_preparing();
    cx.executor().advance_clock(Duration::from_millis(60));
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            Theme::change(mode, Some(window), cx);
            window.render_frame(cx);
            let progress = window.find("vector-load-progress");
            assert_eq!(progress.label(), Some("Preparing vector data"));
            assert!(progress.value().is_none());
            assert_eq!(window.find("vector-load-status").label(), Some("Preparing vector data"));
            assert_eq!(window.find("map-surface").focused(), Some(true));
        }
    })
    .unwrap();

    let source = vector();
    load.finish_vector(source.clone());
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(!panel.read(cx).is_loading(), "The loader has finished, but rendering has not");
        assert_eq!(window.find("vector-load-progress").label(), Some("Rendering vectors"));
        assert!(window.find("vector-load-progress").value().is_none());
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(32, 32))]));
        deliver_frame(&view, source.id(), false, Ok(image.clone()), window, cx);
        assert!(
            window.try_find("vector-load-progress").is_some(),
            "A source-tagged frame may still be background-only"
        );
        assert!(Arc::ptr_eq(view.read(cx).image.as_ref().unwrap(), &image));
        deliver_frame(&view, source.id() + 1, true, Ok(image.clone()), window, cx);
        assert!(
            window.try_find("vector-load-progress").is_some(),
            "A stale settled source cannot complete this load"
        );
        deliver_frame(&view, source.id(), true, Ok(image.clone()), window, cx);
        assert!(window.try_find("vector-load-progress").is_none());
        assert!(window.try_find("vector-load-status").is_none());
        assert!(
            Arc::ptr_eq(view.read(cx).image.as_ref().unwrap(), &image),
            "Idle-only completion shares the existing image"
        );
        let camera = view.read(cx).camera;
        window.press("right", cx);
        assert_ne!(view.read(cx).camera, camera);
        assert_eq!(window.find("map-surface").focused(), Some(true));
    })
    .unwrap();
}

#[gpui_kit::test]
fn progress_status_shares_narrow_header_space_without_hiding_actions(cx: &mut TestAppContext) {
    let (view, window) = open_map(cx);
    let source = vector_named(&format!("{}.geojson", "regional_observations_".repeat(10)));
    let load = cx.update(|cx| ControlledLoad::start(&view.read(cx).raster_panel.clone(), cx));
    load.finish_vector(source);
    cx.run_until_parked();
    cx.simulate_window_resize(window, size(px(640.0), px(400.0)));
    for font_size in [18.0, 24.0, 32.0] {
        cx.update(|cx| Theme::update(cx, |theme| theme.font_size = px(font_size)));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let title = window.find("title-bar").bounds();
            let band = window.find("raster-band").bounds();
            let status = window.find("vector-load-status");
            let debug = window.find("toggle-tile-boundaries").bounds();
            let settings = window.find("open-settings").bounds();
            assert!(status.visible() && status.bounds().size.width > px(0.0));
            assert!(band.right() <= status.bounds().left());
            assert!(
                status.bounds().right() <= debug.left(),
                "Progress cannot overlap header actions at font size {font_size}"
            );
            assert!(debug.right() <= settings.left() && settings.right() <= title.right());
            let controls = window.find("window-controls").bounds();
            if controls.size.width > px(0.0) {
                assert!(settings.right() <= controls.left() && controls.right() <= title.right());
            }
            window.click("toggle-tile-boundaries", cx);
            assert!(window.find("vector-load-progress").visible());
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn progress_render_failure_ends_the_indicator_and_shows_the_existing_map_error(cx: &mut TestAppContext) {
    let (view, window) = open_map(cx);
    let source = vector();
    let load = cx.update(|cx| ControlledLoad::start(&view.read(cx).raster_panel.clone(), cx));
    load.finish_vector(source.clone());
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("vector-load-progress").is_some());
        deliver_frame(&view, source.id(), false, Err(anyhow::anyhow!("Map rendering timed out")), window, cx);
        assert!(window.try_find("vector-load-progress").is_none());
        assert!(window.try_find("vector-load-status").is_none());
        assert!(window.find("map-error-banner").visible());
        assert!(view.read(cx).error.as_ref().unwrap().contains("Map rendering timed out"));
    })
    .unwrap();
}
