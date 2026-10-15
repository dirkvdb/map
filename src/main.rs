mod map_camera;
mod map_readback;
mod map_renderer;
mod map_view;

use gpui_kit::{App, AppContext, Bounds, QuitMode, WindowBounds, WindowOptions, px, size};

fn main() {
    env_logger::init();

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        map_view::init(cx);

        if let Err(error) = create_main_window(cx) {
            log::error!("Failed to open window: {error:#}");
            cx.quit();
            return;
        }

        cx.set_quit_mode(QuitMode::LastWindowClosed);
        cx.activate(true);
    });
}

fn create_main_window(cx: &mut App) -> anyhow::Result<()> {
    let window_bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(window_bounds)),
        ..Default::default()
    };

    gpui_kit::open_window(window_options, cx, |window, cx| {
        window.set_window_title("Map");
        cx.new(map_view::MapView::new)
    })?;

    Ok(())
}
