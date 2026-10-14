mod map_camera;
mod map_readback;
mod map_renderer;
mod map_view;

use gpui_kit::{AppContext, Bounds, WindowBounds, WindowOptions, px, size};

fn main() {
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        map_view::init(cx);

        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        if let Err(error) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1200.), px(760.)), cx))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                window.set_window_title("Map");
                cx.new(map_view::MapView::new)
            },
        ) {
            eprintln!("Failed to open window: {error:#}");
            cx.quit();
            return;
        }

        cx.activate(true);
    });
}
