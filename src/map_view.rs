use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::{ActiveTheme, button::Button};
use gpui_kit::{
    App, Bounds, Context, CursorStyle, DispatchPhase, FocusHandle, InteractiveElement, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Styled, StyledImage, Task,
    TestSupportExt, Window, canvas, div, img,
};

use crate::map_camera::MapCamera;
use crate::map_renderer::{MapFrame, MapRenderer, MapRequest, Viewport};

gpui_kit::actions!(map, [ZoomIn, ZoomOut, PanLeft, PanRight, PanUp, PanDown]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("+", ZoomIn, Some("MapNavigation")),
        KeyBinding::new("=", ZoomIn, Some("MapNavigation")),
        KeyBinding::new("-", ZoomOut, Some("MapNavigation")),
        KeyBinding::new("left", PanLeft, Some("MapNavigation")),
        KeyBinding::new("right", PanRight, Some("MapNavigation")),
        KeyBinding::new("up", PanUp, Some("MapNavigation")),
        KeyBinding::new("down", PanDown, Some("MapNavigation")),
    ]);
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

pub struct MapView {
    viewport: Option<Viewport>,
    bounds: Option<Bounds<Pixels>>,
    camera: MapCamera,
    requested: Option<MapRequest>,
    renderer: Option<MapRenderer>,
    drag: Option<MapDrag>,
    coast: Option<MapCoast>,
    coast_frame_pending: bool,
    focus: FocusHandle,
    image: Option<Arc<RenderImage>>,
    error: Option<SharedString>,
    _frames_task: Option<Task<()>>,
}

impl MapView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let renderer = MapRenderer::new().expect("Failed to create the map renderer");

        let task = Self::watch_frames(&renderer, cx);
        cx.on_release_in(window, |view, window, cx| {
            if let Some(image) = view.image.take() {
                cx.drop_image(image, Some(window));
            }
        })
        .detach();
        Self {
            viewport: None,
            bounds: None,
            camera: MapCamera::default(),
            requested: None,
            renderer: Some(renderer),
            drag: None,
            coast: None,
            coast_frame_pending: false,
            focus: cx.focus_handle().tab_stop(true),
            image: None,
            error: None,
            _frames_task: Some(task),
        }
    }

    fn watch_frames(renderer: &MapRenderer, cx: &mut Context<Self>) -> Task<()> {
        let frames = renderer.frames();
        cx.spawn(async move |this, cx| {
            while let Ok(frame) = frames.recv().await {
                if this.update_in(cx, |view, window, cx| view.apply_frame(frame, window, cx)).is_err() {
                    break;
                }
            }
        })
    }

    fn apply_frame(&mut self, frame: MapFrame, window: &mut Window, cx: &mut Context<Self>) {
        if self.viewport != Some(frame.viewport) {
            return;
        }
        match frame.image {
            Ok(image) => {
                if let Some(previous) = self.image.replace(image) {
                    // Dropping the Arc does not evict GPUI's uploaded texture.
                    cx.drop_image(previous, Some(window));
                }
                self.error = None;
            }
            Err(error) => {
                log::error!("Map rendering failed: {error:#}");
                self.error = Some(format!("Couldn’t load the map. {error}").into());
            }
        }
        cx.notify();
    }

    fn request_render(&mut self, cx: &mut Context<Self>) {
        let Some(viewport) = self.viewport else {
            return;
        };
        let request = MapRequest { viewport, camera: self.camera };
        if self.requested == Some(request) {
            return;
        }
        self.requested = Some(request);
        let Some(renderer) = &self.renderer else {
            return;
        };
        self.error = None;
        if let Err(error) = renderer.render(request) {
            self.error = Some(format!("Couldn’t load the map. {error}").into());
        }
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let renderer = MapRenderer::new().expect("Failed to create the map renderer");
        self._frames_task = Some(Self::watch_frames(&renderer, cx));
        self.renderer = Some(renderer);
        self.requested = None;
        self.request_render(cx);
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
}

impl Render for MapView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let previous_bounds = self.bounds;
        let previous_viewport = self.viewport;
        let drag_move = cx.listener(|view, event: &MouseMoveEvent, window, cx| view.drag_map(event, window, cx));
        let drag_end = cx.listener(|view, event: &MouseUpEvent, window, cx| {
            if event.button == MouseButton::Left {
                view.end_drag(window, cx)
            }
        });
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
            .cursor(if self.drag.is_some() {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    window.focus(&view.focus, cx);
                    view.coast = None;
                    view.drag = Some(MapDrag::new(event.position, Instant::now()));
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|view, event: &ScrollWheelEvent, _, cx| {
                let Some(bounds) = view.bounds else { return };
                let offset = event.position - bounds.center();
                let delta = match event.delta {
                    ScrollDelta::Lines(delta) => f64::from(delta.y) * 0.25,
                    ScrollDelta::Pixels(delta) => f64::from(f32::from(delta.y)) / 240.0,
                };
                view.zoom(delta, (f64::from(f32::from(offset.x)), f64::from(f32::from(offset.y))), cx);
                cx.stop_propagation();
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
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
                                let _ = view.update(cx, |view, cx| {
                                    view.bounds = Some(bounds);
                                    view.viewport = Some(viewport);
                                    view.request_render(cx);
                                });
                            });
                        }
                    },
                    move |_, _, window, _| {
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
                    },
                )
                .absolute()
                .size_full(),
            );

        if let Some(image) = &self.image {
            surface = surface.child(img(image.clone()).absolute().size_full().object_fit(ObjectFit::Fill));
        }
        if let Some(error) = &self.error {
            surface = surface.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .p_4()
                    .bg(cx.theme().background)
                    .child(error.clone())
                    .child(Button::new("retry-map").label("Retry").on_click(cx.listener(|view, _, _, cx| view.retry(cx)))),
            );
        } else if self.image.is_none() {
            surface = surface.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child("Loading vector map…"),
            );
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(surface)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, TestAppContext, point, px, size};

    use super::*;

    #[gpui_kit::test]
    fn replacing_map_frames_evicts_obsolete_texture_cache_entries(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut view = None;
        let handle = cx.open_window(size(px(800.0), px(600.0)), |window, cx| {
            let map = cx.new(|cx| {
                let mut map = MapView::new(window, cx);
                map.renderer = None;
                map._frames_task = None;
                map
            });
            view = Some(map.clone());
            gpui_kit::base::Root::new(map, window, cx)
        });
        let view = view.unwrap();
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
            window.replace_root(cx, |window, cx| {
                let mut map = MapView::new(window, cx);
                map.renderer = None;
                map._frames_task = None;
                map
            });
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
            init(cx);
        });
        let mut view = None;
        let handle = cx.open_window(size(px(800.0), px(600.0)), |window, cx| {
            let map = cx.new(|cx| {
                let mut map = MapView::new(window, cx);
                // Exercise actual input dispatch without a GPU or network worker.
                map.renderer = None;
                map._frames_task = None;
                map
            });
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
        })
        .unwrap();
    }
}
