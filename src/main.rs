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
mod raster_projection;
mod raster_source;
mod raster_tiles;
mod recent_files;
mod recent_files_picker;
mod settings;
mod shortcuts;
mod vector_source;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use gdal::Metadata as _;
use gdal::vector::LayerAccess as _;
use gpui_kit::component::TitleBar;
use gpui_kit::{App, AppContext, Bounds, QuitMode, Window, WindowBounds, WindowOptions, px, size};

gpui_kit::actions!(app, [Quit]);

// env_logger matches target prefixes, so exclude third-party crates whose
// names start with an allowed crate name (notably geo_types, geojson and geos).
const DEFAULT_LOG_FILTER: &str = "off,map=info,geo=info,inf=warn,tiler=warn,maplibre_native=warn,geo_types=off,geojson=off,geos=off";

fn main() {
    logging_builder()
        .parse_env(env_logger::Env::default().default_filter_or(DEFAULT_LOG_FILTER))
        .init();

    if let Err(error) = raster_source::RasterTiles::init() {
        log::error!("Failed to initialize raster tiling: {error:#}");
        return;
    }

    let paths = command_line_paths(std::env::args_os().skip(1).map(PathBuf::from));

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        if let Err(error) = init_actions(cx).and_then(|()| map_view::init(cx)) {
            log::error!("Failed to initialize keyboard shortcuts: {error:#}");
            cx.quit();
            return;
        }
        appearance::init(cx);

        if let Err(error) = create_main_window(paths, cx) {
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

fn command_line_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if !path.is_dir() {
            files.push(path);
            continue;
        }
        match directory_source_paths(&path) {
            Ok(paths) => files.extend(paths),
            Err(error) => log::error!("Could not open source directory: {error:#}"),
        }
    }
    files
}

fn directory_source_paths(directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(directory).with_context(|| format!("Could not read {}", directory.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.with_context(|| format!("Could not read an entry in {}", directory.display()))?.path();
        if path.is_file() && is_source_dataset(&path) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn is_source_dataset(path: &Path) -> bool {
    // Probe metadata only: unsupported files and shapefile sidecars are not sources.
    let Ok(dataset) = gdal::Dataset::open_ex(path, gdal::DatasetOptions::default()) else {
        return false;
    };
    let driver = dataset.driver().short_name();
    // GDAL also opens .dbf and .shx as their sibling shapefile dataset.
    if driver == "ESRI Shapefile" && !path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("shp")) {
        return false;
    }
    dataset.raster_count() > 0
        || dataset.metadata_domain("SUBDATASETS").is_some()
        || driver == "CSV"
        || dataset.layers().any(|layer| layer.defn().geom_fields().next().is_some())
}

fn create_main_window(paths: Vec<PathBuf>, cx: &mut App) -> anyhow::Result<()> {
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
            view.open_paths(paths, cx);
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

    fn command_line_directories_add_supported_files_in_sorted_order_without_recursing() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let raster = directory.path().join("a-raster.TIF");
        crate::raster_source::test_support::write_raster(&raster, 4326, 1)?;
        let vector = directory.path().join("b-vector.geojson");
        std::fs::write(&vector, r#"{"type":"FeatureCollection","features":[]}"#)?;
        std::fs::write(directory.path().join("notes.txt"), "Not a dataset")?;
        std::fs::create_dir(directory.path().join("nested"))?;
        crate::raster_source::test_support::write_raster(&directory.path().join("nested/hidden.tif"), 4326, 1)?;
        std::fs::create_dir(directory.path().join("looks-like-a-file.tif"))?;

        assert_eq!(command_line_paths([directory.path().to_path_buf()]), [raster, vector]);
        Ok(())
    }

    #[test]
    fn command_line_paths_preserve_explicit_files_and_expand_multiple_directories_in_place() -> anyhow::Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let first_raster = first.path().join("first.tif");
        let second_raster = second.path().join("second.tif");
        for path in [&first_raster, &second_raster] {
            crate::raster_source::test_support::write_raster(path, 4326, 1)?;
        }
        let explicit = first.path().join("explicit.unknown");
        let missing = first.path().join("missing.tif");
        std::fs::write(&explicit, "Explicit inputs still go through the normal loader")?;
        let paths = command_line_paths([explicit.clone(), first.path().to_path_buf(), missing.clone(), second.path().to_path_buf()]);
        assert_eq!(paths, [explicit, first_raster, missing, second_raster]);
        Ok(())
    }

    #[test]
    fn command_line_directories_ignore_empty_and_unsupported_only_directories() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        assert!(command_line_paths([directory.path().to_path_buf()]).is_empty());
        std::fs::write(directory.path().join("notes.txt"), "Not a dataset")?;
        assert!(command_line_paths([directory.path().to_path_buf()]).is_empty());
        assert!(command_line_paths([]).is_empty());
        Ok(())
    }

    #[test]
    fn command_line_directories_detect_content_without_requiring_a_known_extension() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("raster.unusual");
        crate::raster_source::test_support::write_raster(&path, 4326, 1)?;
        assert_eq!(command_line_paths([directory.path().to_path_buf()]), [path]);
        Ok(())
    }

    #[test]
    fn command_line_directories_include_csv_sources_but_not_shapefile_sidecars() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let shapefile = directory.path().join("shape.shp");
        let mut dataset = gdal::DriverManager::get_driver_by_name("ESRI Shapefile")?.create_vector_only(&shapefile)?;
        dataset.create_layer(Default::default())?;
        drop(dataset);
        let csv = directory.path().join("points.csv");
        std::fs::write(&csv, "lat,lon,value\n51,4,12\n")?;
        assert_eq!(command_line_paths([directory.path().to_path_buf()]), [csv, shapefile]);
        Ok(())
    }

    #[test]
    fn directory_read_errors_include_the_requested_path() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        let error = directory_source_paths(&missing).unwrap_err();
        assert!(error.to_string().contains(&missing.display().to_string()));
    }

    #[test]
    fn console_logging_defaults_to_info_for_map_and_geo_and_warn_for_infra_and_maplibre() {
        let logger = logging_builder().parse_filters(DEFAULT_LOG_FILTER).build();
        for (target, max_level) in [
            ("map", Level::Info),
            ("map::map_renderer", Level::Info),
            ("map::vector_load", Level::Info),
            ("geo", Level::Info),
            ("geo::gdalinterop", Level::Info),
            ("inf", Level::Warn),
            ("tiler::tileio", Level::Warn),
            ("maplibre_native", Level::Warn),
            ("maplibre_native::bridge", Level::Warn),
        ] {
            for level in [Level::Error, Level::Warn, Level::Info, Level::Debug, Level::Trace] {
                let metadata = log::Metadata::builder().target(target).level(level).build();
                assert_eq!(logger.enabled(&metadata), level <= max_level, "Unexpected default for {target} at {level}");
            }
        }
    }

    #[test]
    fn console_logging_silences_other_crates_by_default_including_prefix_collisions() {
        let logger = logging_builder().parse_filters(DEFAULT_LOG_FILTER).build();
        for target in [
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
