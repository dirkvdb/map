use std::num::NonZeroU32;
use std::sync::Arc;

use gpui_kit::component::{ActiveTheme, button::Button};
use gpui_kit::{
    App, Bounds, Context, CursorStyle, DispatchPhase, FocusHandle, InteractiveElement, IntoElement, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta,
    ScrollWheelEvent, SharedString, Styled, StyledImage, Task, TestSupportExt, Window, canvas, div, img,
};

use crate::map_camera::MapCamera;
use crate::map_renderer::{MapRenderer, MapRequest, Viewport};

gpui_kit::actions!(map, [ZoomIn, ZoomOut, PanLeft, PanRight, PanUp, PanDown]);

pub(crate) fn init(cx: &mut App) {
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

pub(crate) struct MapView {
    viewport: Option<Viewport>,
    bounds: Option<Bounds<Pixels>>,
    camera: MapCamera,
    requested: Option<MapRequest>,
    renderer: Option<MapRenderer>,
    drag_position: Option<Point<Pixels>>,
    focus: FocusHandle,
    image: Option<Arc<RenderImage>>,
    error: Option<SharedString>,
    _frames_task: Option<Task<()>>,
}

impl MapView {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let (renderer, error) = match MapRenderer::new() {
            Ok(renderer) => (Some(renderer), None),
            Err(error) => (None, Some(format!("Couldn’t start the map. {error}").into())),
        };
        let task = renderer.as_ref().map(|renderer| Self::watch_frames(renderer, cx));
        Self {
            viewport: None,
            bounds: None,
            camera: MapCamera::default(),
            requested: None,
            renderer,
            drag_position: None,
            focus: cx.focus_handle().tab_stop(true),
            image: None,
            error,
            _frames_task: task,
        }
    }

    fn watch_frames(renderer: &MapRenderer, cx: &mut Context<Self>) -> Task<()> {
        let frames = renderer.frames();
        cx.spawn(async move |this, cx| {
            while let Ok(frame) = frames.recv().await {
                if this
                    .update(cx, |view, cx| {
                        if view.viewport != Some(frame.viewport) {
                            return;
                        }
                        match frame.image {
                            Ok(image) => {
                                view.image = Some(image);
                                view.error = None;
                            }
                            Err(error) => {
                                log::error!("Map rendering failed: {error:#}");
                                view.error = Some(format!("Couldn’t load the map. {error}").into());
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn request_render(&mut self, cx: &mut Context<Self>) {
        let Some(viewport) = self.viewport else {
            return;
        };
        let request = MapRequest {
            viewport,
            camera: self.camera,
        };
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
        match MapRenderer::new() {
            Ok(renderer) => {
                self._frames_task = Some(Self::watch_frames(&renderer, cx));
                self.renderer = Some(renderer);
            }
            Err(error) => {
                self.error = Some(format!("Couldn’t start the map. {error}").into());
                cx.notify();
                return;
            }
        }
        self.requested = None;
        self.request_render(cx);
    }

    fn pan(&mut self, dx: f64, dy: f64, cx: &mut Context<Self>) {
        self.camera.pan(dx, dy);
        self.request_render(cx);
    }

    fn zoom(&mut self, delta: f64, offset: (f64, f64), cx: &mut Context<Self>) {
        self.camera.zoom_at(delta, offset);
        self.request_render(cx);
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag_position.take().is_some() {
            cx.notify();
        }
    }

    fn drag_map(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            self.end_drag(cx);
            return;
        }
        if let Some(previous) = self.drag_position.replace(event.position) {
            let delta = event.position - previous;
            self.pan(f64::from(f32::from(delta.x)), f64::from(f32::from(delta.y)), cx);
        } else {
            // A drag begun outside this map must not start map navigation.
            self.drag_position = None;
        }
    }
}

impl Render for MapView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let previous_bounds = self.bounds;
        let previous_viewport = self.viewport;
        let drag_move = cx.listener(|view, event: &MouseMoveEvent, _, cx| view.drag_map(event, cx));
        let drag_end = cx.listener(|view, event: &MouseUpEvent, _, cx| {
            if event.button == MouseButton::Left {
                view.end_drag(cx)
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
            .cursor(if self.drag_position.is_some() {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    window.focus(&view.focus, cx);
                    view.drag_position = Some(event.position);
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
                    .child(
                        Button::new("retry-map")
                            .label("Retry")
                            .on_click(cx.listener(|view, _, _, cx| view.retry(cx))),
                    ),
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
    fn mouse_navigation_updates_the_camera_and_ends_drag_outside_map(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx);
        });
        let mut view = None;
        let handle = cx.open_window(size(px(800.0), px(600.0)), |window, cx| {
            let map = cx.new(|cx| {
                let mut map = MapView::new(cx);
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
            assert!(view.read(cx).drag_position.is_none());

            let after_pan = view.read(cx).camera;
            // End a drag on the attribution footer, outside the map hitbox.
            window.drag(from, point(from.x, bounds.bottom() + px(5.0)), cx);
            assert_ne!(view.read(cx).camera, after_pan);
            assert!(view.read(cx).drag_position.is_none());

            let after_release = view.read(cx).camera;
            window.hover("map-surface", cx);
            assert_eq!(view.read(cx).camera, after_release);
            window.right_click("map-surface", cx);
            assert_eq!(view.read(cx).camera, after_release);
            window.press("-", cx);
            assert_ne!(view.read(cx).camera, after_release);
        })
        .unwrap();
    }
}
