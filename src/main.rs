#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod appearance;
mod background;
mod bundled_themes;
mod config;
mod dataset_properties;
mod feature_info;
mod log_output;
mod map_camera;
mod map_readback;
mod map_renderer;
mod map_view;
mod multiband_layer;
mod pixel_info;
mod raster_composite;
mod raster_extrusion_animation;
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

// env_logger matches target prefixes, so exclude third-party crates whose
// names start with an allowed crate name (notably maplibre_native and geojson).
const DEFAULT_LOG_FILTER: &str = "off,map=warn,geo=warn,inf=warn,tiler=warn,maplibre_native=off,geo_types=off,geojson=off,geos=off";

fn main() {
    logging_builder()
        .parse_env(env_logger::Env::default().default_filter_or(DEFAULT_LOG_FILTER))
        .init();

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

fn logging_builder() -> env_logger::Builder {
    let mut builder = env_logger::Builder::new();
    builder
        .format_timestamp_millis()
        .write_style(env_logger::WriteStyle::Never)
        .target(env_logger::Target::Pipe(Box::new(log_output::LogWriter)));
    builder
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
    use log::{Level, Log as _};

    use super::*;

    #[test]
    fn console_logging_defaults_to_map_and_infra_warnings_and_errors() {
        let logger = logging_builder().parse_filters(DEFAULT_LOG_FILTER).build();
        for target in [
            "map",
            "map::map_renderer",
            "map::vector_load",
            "geo",
            "geo::gdalinterop",
            "inf",
            "tiler::tileio",
        ] {
            for (level, enabled) in [
                (Level::Error, true),
                (Level::Warn, true),
                (Level::Info, false),
                (Level::Debug, false),
                (Level::Trace, false),
            ] {
                let metadata = log::Metadata::builder().target(target).level(level).build();
                assert_eq!(logger.enabled(&metadata), enabled, "Unexpected default for {target} at {level}");
            }
        }
    }

    #[test]
    fn console_logging_silences_other_crates_by_default_including_prefix_collisions() {
        let logger = logging_builder().parse_filters(DEFAULT_LOG_FILTER).build();
        for target in [
            "maplibre_native::bridge",
            "geojson",
            "geo_types",
            "geos",
            "geos_sys",
            "wgpu_core",
            "gpui",
            "image",
            "reqwest",
            "raster_tile",
            "sqlite",
        ] {
            let metadata = log::Metadata::builder().target(target).level(Level::Error).build();
            assert!(!logger.enabled(&metadata), "Other crates must be off by default: {target}");
        }
    }

    #[test]
    fn console_logging_off_override_disables_map_and_infra_too() {
        let logger = logging_builder().parse_filters("off").build();
        for target in ["map", "geo", "inf", "raster_tile", "sqlite", "tiler", "maplibre_native"] {
            let metadata = log::Metadata::builder().target(target).level(Level::Error).build();
            assert!(!logger.enabled(&metadata), "The off override must disable {target}");
        }
    }

    #[test]
    fn console_logging_accepts_module_filters_and_can_be_disabled() {
        let logger = logging_builder().parse_filters("off,map=debug,maplibre_native=trace").build();
        for (target, level, enabled) in [
            ("map::map_renderer", Level::Debug, true),
            ("map::map_renderer", Level::Trace, false),
            ("maplibre_native::bridge", Level::Trace, true),
            ("wgpu_core", Level::Error, false),
        ] {
            let metadata = log::Metadata::builder().target(target).level(level).build();
            assert_eq!(logger.enabled(&metadata), enabled, "Unexpected filter for {target} at {level}");
        }
    }

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
