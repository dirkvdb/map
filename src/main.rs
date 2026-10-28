#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod appearance;
mod bundled_themes;
mod config;
mod feature_info;
mod map_camera;
mod map_readback;
mod map_renderer;
mod map_view;
mod multiband_layer;
mod pixel_info;
mod raster_layer;
mod raster_panel;
mod raster_playback;
mod raster_source;
mod raster_tiles;
mod recent_files;
mod recent_files_picker;
mod settings;
mod shortcuts;
mod vector_source;

use gpui_kit::component::TitleBar;
use gpui_kit::{App, AppContext, Bounds, QuitMode, Window, WindowBounds, WindowOptions, px, size};

gpui_kit::actions!(app, [Quit]);

fn main() {
    env_logger::init();

    if let Err(error) = raster_source::RasterTiles::init() {
        log::error!("Failed to initialize raster tiling: {error:#}");
        return;
    }

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        if let Err(error) = init_actions(cx).and_then(|()| map_view::init(cx)) {
            log::error!("Failed to initialize keyboard shortcuts: {error:#}");
            cx.quit();
            return;
        }
        appearance::init(cx);

        if let Err(error) = create_main_window(cx) {
            log::error!("Failed to open window: {error:#}");
            cx.quit();
            return;
        }

        cx.set_quit_mode(QuitMode::LastWindowClosed);
        cx.activate(true);
    });
}

fn init_actions(cx: &mut App) -> anyhow::Result<()> {
    cx.bind_keys([shortcuts::key_binding("secondary-q", Quit, None, cx)?]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    Ok(())
}

fn create_main_window(cx: &mut App) -> anyhow::Result<()> {
    let window_bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(window_bounds)),
        // Platform window constraints are physical geometry, not UI spacing.
        window_min_size: Some(size(px(640.), px(400.))),
        ..TitleBar::window_options()
    };

    gpui_kit::open_window(window_options, cx, |window, cx| {
        follow_system_appearance(window, cx);
        window.set_window_title("Map");
        cx.new(|cx| {
            let view = map_view::MapView::new(window, cx);
            view.open_paths(std::env::args_os().skip(1).map(std::path::PathBuf::from).collect(), cx);
            view
        })
    })?;

    Ok(())
}

fn follow_system_appearance(window: &mut Window, cx: &mut App) {
    appearance::apply(window, cx);
    window.observe_window_appearance(appearance::sync_system).detach();
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::component::{ActiveTheme, Theme, ThemeMode};

    use super::*;

    #[gpui_kit::test]
    fn controls_start_with_the_system_appearance(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            appearance::setup_test(None, cx);
            Theme::change(ThemeMode::Dark, None, cx);
        });
        cx.add_window(|window, cx| {
            follow_system_appearance(window, cx);
            assert_eq!(cx.theme().mode, ThemeMode::from(window.appearance()));
            gpui_kit::Empty
        });
    }
}
