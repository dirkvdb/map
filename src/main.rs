use gpui_kit::component::ActiveTheme;
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, Render, Styled, Window, WindowBounds, WindowOptions,
    div, px, size,
};

struct Map;

impl Render for Map {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(cx.theme().background)
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx| {
            gpui_kit::init(cx);

            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            if let Err(error) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1200.), px(760.)),
                        cx,
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    window.set_window_title("Map");
                    cx.new(|_| Map)
                },
            ) {
                eprintln!("Failed to open window: {error:#}");
                cx.quit();
                return;
            }

            cx.activate(true);
        });
}
