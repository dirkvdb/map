use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    App, AppContext, InputEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, TestAppContext, Window, point, px, size,
};

use super::MapView;

fn with_map(cx: &mut TestAppContext, test: impl FnOnce(&gpui_kit::Entity<MapView>, &mut Window, &mut App)) {
    cx.update(gpui_kit::init);
    let mut view = None;
    let handle = cx.open_window(size(px(1000.), px(800.)), |window, cx| {
        let map = cx.new(|cx| MapView::with_renderer(None, window, cx));
        view = Some(map.clone());
        gpui_kit::base::Root::new(map, window, cx)
    });
    let view = view.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("camera.tif");
    crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
    cx.update(|cx| view.update(cx, |map, cx| map.open_paths(vec![path], cx)));
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        test(&view, window, cx);
    })
    .unwrap();
}

fn press(window: &mut Window, position: Point<Pixels>, button: MouseButton, cx: &mut App) {
    window.dispatch_event(
        MouseDownEvent {
            position,
            button,
            click_count: 1,
            ..Default::default()
        }
        .to_platform_input(),
        cx,
    );
    window.render_frame(cx);
}

fn move_pointer(window: &mut Window, position: Point<Pixels>, pressed_button: Option<MouseButton>, cx: &mut App) {
    window.dispatch_event(
        MouseMoveEvent {
            position,
            pressed_button,
            ..Default::default()
        }
        .to_platform_input(),
        cx,
    );
    window.render_frame(cx);
}

fn release(window: &mut Window, position: Point<Pixels>, button: MouseButton, cx: &mut App) {
    window.dispatch_event(
        MouseUpEvent {
            position,
            button,
            click_count: 1,
            ..Default::default()
        }
        .to_platform_input(),
        cx,
    );
    window.render_frame(cx);
}

#[gpui_kit::test]
fn right_drag_changes_bearing_and_pitch_without_panning_or_coasting(cx: &mut TestAppContext) {
    with_map(cx, |view, window, cx| {
        let start = window.find("map-surface").bounds().center();
        let before = view.read(cx).camera;
        press(window, start, MouseButton::Right, cx);
        move_pointer(window, start + point(px(80.), px(-40.)), Some(MouseButton::Right), cx);
        let mut expected = before;
        expected.rotate(80.);
        expected.tilt(-40.);
        assert_ne!(expected, before);
        assert_eq!(view.read(cx).camera, expected);
        release(window, start, MouseButton::Right, cx);
        assert!(view.read(cx).drag.is_none());
        assert!(view.read(cx).coast.is_none());
        move_pointer(window, start, None, cx);
        assert_eq!(view.read(cx).camera, expected);
    });
}

#[gpui_kit::test]
fn horizontal_right_drag_rotates_and_left_release_does_not_end_gesture(cx: &mut TestAppContext) {
    with_map(cx, |view, window, cx| {
        let start = window.find("map-surface").bounds().center();
        let before = view.read(cx).camera;
        press(window, start, MouseButton::Right, cx);
        move_pointer(window, start + point(px(40.), px(0.)), Some(MouseButton::Right), cx);
        let mut expected = before;
        expected.rotate(40.);
        assert_ne!(expected, before);
        assert_eq!(view.read(cx).camera, expected);
        release(window, start, MouseButton::Left, cx);
        assert!(view.read(cx).drag.is_some());
        move_pointer(window, start + point(px(40.), px(-20.)), Some(MouseButton::Right), cx);
        expected.tilt(-20.);
        assert_eq!(view.read(cx).camera, expected);
    });
}

#[gpui_kit::test]
fn right_release_outside_map_and_missing_button_cancel_tilt(cx: &mut TestAppContext) {
    with_map(cx, |view, window, cx| {
        let start = window.find("map-surface").bounds().center();
        press(window, start, MouseButton::Right, cx);
        release(window, point(px(-10.), px(-10.)), MouseButton::Right, cx);
        assert!(view.read(cx).drag.is_none());
        let before = view.read(cx).camera;
        move_pointer(window, start + point(px(0.), px(-40.)), Some(MouseButton::Right), cx);
        assert_eq!(view.read(cx).camera, before, "Motion alone cannot start a gesture");
        press(window, start, MouseButton::Right, cx);
        move_pointer(window, start + point(px(0.), px(-20.)), None, cx);
        assert!(view.read(cx).drag.is_none());
        assert!(view.read(cx).coast.is_none());
        assert_eq!(view.read(cx).camera, before);
    });
}

#[gpui_kit::test]
fn controls_do_not_start_right_drag_and_release_over_controls_ends_it(cx: &mut TestAppContext) {
    with_map(cx, |view, window, cx| {
        let control = window.find("raster-controls").bounds().center();
        let start = window.find("map-surface").bounds().center();
        let before = view.read(cx).camera;
        press(window, control, MouseButton::Right, cx);
        move_pointer(window, start, Some(MouseButton::Right), cx);
        assert!(view.read(cx).drag.is_none());
        assert_eq!(view.read(cx).camera, before);
        release(window, start, MouseButton::Right, cx);
        press(window, start, MouseButton::Right, cx);
        release(window, control, MouseButton::Right, cx);
        assert!(view.read(cx).drag.is_none());
    });
}

#[gpui_kit::test]
fn pitched_rotated_off_center_drag_and_scroll_keep_the_ground_anchor(cx: &mut TestAppContext) {
    with_map(cx, |view, window, cx| {
        let center = window.find("map-surface").bounds().center();
        press(window, center, MouseButton::Right, cx);
        move_pointer(window, center + point(px(74.), px(-120.)), Some(MouseButton::Right), cx);
        release(window, center, MouseButton::Right, cx);
        let from = (-70.0, 60.0);
        let to = (-30.0, 90.0);
        let anchor = view.read(cx).camera.coordinate_at(from);
        press(window, center + point(px(from.0 as f32), px(from.1 as f32)), MouseButton::Left, cx);
        let target = center + point(px(to.0 as f32), px(to.1 as f32));
        move_pointer(window, target, Some(MouseButton::Left), cx);
        assert_same_coordinate(view.read(cx).camera.coordinate_at(to), anchor);
        window.dispatch_event(
            gpui_kit::ScrollEvent {
                position: target,
                delta: ScrollDelta::Lines(point(0.0, 2.0)),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        assert_same_coordinate(view.read(cx).camera.coordinate_at(to), anchor);
    });
}

fn assert_same_coordinate(actual: geo::Coordinate, expected: geo::Coordinate) {
    assert!((actual.latitude - expected.latitude).abs() < 1e-8, "{actual:?} != {expected:?}");
    assert!((actual.longitude - expected.longitude).abs() < 1e-8, "{actual:?} != {expected:?}");
}

#[gpui_kit::test]
fn left_pan_and_scroll_preserve_bearing_and_pitch(cx: &mut TestAppContext) {
    with_map(cx, |view, window, cx| {
        let start = window.find("map-surface").bounds().center();
        press(window, start, MouseButton::Right, cx);
        move_pointer(window, start + point(px(60.), px(-40.)), Some(MouseButton::Right), cx);
        release(window, start, MouseButton::Right, cx);
        let mut expected = view.read(cx).camera;
        press(window, start, MouseButton::Left, cx);
        move_pointer(window, start + point(px(20.), px(0.)), Some(MouseButton::Left), cx);
        expected.pan(20., 0.);
        assert_eq!(view.read(cx).camera, expected);
        window.scroll("map-surface", ScrollDelta::Lines(point(0., 1.)), cx);
        expected.zoom_at(0.25, (0., 0.));
        assert_eq!(view.read(cx).camera, expected);
    });
}
