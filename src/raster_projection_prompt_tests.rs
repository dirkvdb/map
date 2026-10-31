use std::path::PathBuf;

use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, TestAppContext, px, size};

use super::*;
use crate::raster_panel::{DatasetRequest, load_paths};
use crate::raster_source::{MissingProjection, RasterSource};

struct Harness {
    _directory: tempfile::TempDir,
    paths: Vec<PathBuf>,
    panel: Entity<RasterPanel>,
    window: AnyWindowHandle,
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = ["first.tif", "second.tif"].map(|name| directory.path().join(name)).to_vec();
        for path in &paths {
            crate::raster_source::test_support::write_raster(path, 0, 2).unwrap();
        }
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut panel = None;
        let handle = cx.open_window(size(px(800.), px(600.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut panel = RasterPanel::new(window, cx);
                panel.recent_files = crate::recent_files::RecentFiles::load_from(Some(directory.path().join("recent-files.json")));
                panel
            });
            let focus = view.read(cx).focus.clone();
            focus.focus(window, cx);
            panel = Some(view.clone());
            Root::new(view, window, cx)
        });
        Self {
            _directory: directory,
            paths,
            panel: panel.unwrap(),
            window: handle.into(),
        }
    }

    fn ui(&self, cx: &mut TestAppContext, interact: impl FnOnce(&mut Window, &mut App)) {
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            interact(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn load(&self, paths: Vec<PathBuf>, cx: &mut TestAppContext) {
        self.ui(cx, |_, cx| self.panel.update(cx, |panel, cx| panel.open_paths(paths, cx)));
    }

    fn assert_prompt(&self, index: usize, cx: &mut TestAppContext) {
        self.ui(cx, |window, cx| {
            assert!(window.find("dialog").visible());
            assert_eq!(window.find("projection-file").label(), self.paths[index].to_str());
            assert_eq!(window.find("projection-epsg").focused(), Some(true));
            assert!(self.panel.read(cx).is_loading());
            assert!(self.panel.read(cx).render_load_progress().is_none());
        });
    }
}

#[test]
fn epsg_input_accepts_codes_and_authority_prefixes() {
    for value in ["4326", " 4326 ", "EPSG:4326", "epsg: 4326"] {
        assert_eq!(parse_epsg(value), Ok(4326));
    }
}

#[test]
fn epsg_input_rejects_missing_invalid_unknown_and_nonspatial_codes() {
    for value in ["", "EPSG:", "0", "-1", "3.5", "WGS84", "999999", "5703", "4978"] {
        assert!(parse_epsg(value).is_err(), "Accepted {value:?}");
    }
}

#[test]
fn missing_projection_is_a_prompt_request_not_a_load_error_or_history_entry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raster.tif");
    crate::raster_source::test_support::write_raster(&path, 0, 2).unwrap();
    let (progress, _) = async_channel::bounded(1);
    let loaded = load_paths(vec![path.clone().into()], progress);
    assert_eq!(loaded.missing_projections, [path]);
    assert!(loaded.errors.is_empty());
    assert!(loaded.opened_paths.is_empty());
    assert!(loaded.sources.is_empty());
}

#[test]
fn supplied_projection_keeps_original_bytes_bands_and_lazy_tile_reads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raster.tif");
    crate::raster_source::test_support::write_raster(&path, 0, 2).unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = RasterSource::open(&path).err().unwrap();
    assert!(error.is::<MissingProjection>());
    let sources = RasterSource::open_with_epsg(&path, Some(4326)).unwrap();
    assert_eq!(sources.len(), 2);
    assert!(crate::raster_playback::PlaybackRequest::for_dataset(&sources).is_some());
    let source = sources[1].restyled(inf::colormap::ColorMapPreset::Gray, (0., 200.)).unwrap();
    drop(sources);
    assert_eq!(
        gdal::Dataset::open(&source.metadata().path)
            .unwrap()
            .spatial_ref()
            .unwrap()
            .auth_code()
            .unwrap(),
        4326
    );
    assert_eq!(source.metadata().bounds, [-10., -10., 10., 10.]);
    let tile = geo::Tile::for_coordinate(geo::Coordinate::latlon(0., 0.), source.metadata().max_zoom);
    assert!(source.numeric_tile(tile, 1).unwrap().unwrap().values.iter().any(|value| value.is_finite()));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(gdal::Dataset::open(&path).unwrap().spatial_ref().is_err());
}

#[test]
fn supplied_projection_never_overrides_an_existing_crs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raster.tif");
    crate::raster_source::test_support::write_raster(&path, 3857, 1).unwrap();
    let sources = RasterSource::open_with_epsg(&path, Some(4326)).unwrap();
    assert_eq!(sources[0].metadata().epsg, Some(geo::crs::epsg::WGS84_WEB_MERCATOR));
    assert_eq!(sources[0].metadata().path, path);
}

#[test]
fn netcdf_variables_without_projection_use_the_supplied_epsg() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raster.nc");
    crate::raster_source::test_support::write_multivariable_netcdf(&path, 0, 2).unwrap();
    assert!(RasterSource::open(&path).err().unwrap().is::<MissingProjection>());
    let sources = RasterSource::open_with_epsg(&path, Some(4326)).unwrap();
    assert_eq!(sources.len(), 2);
    for source in sources {
        assert_eq!(
            gdal::Dataset::open(&source.metadata().path)
                .unwrap()
                .spatial_ref()
                .unwrap()
                .auth_code()
                .unwrap(),
            4326
        );
    }
}

#[gpui_kit::test]
fn entering_epsg_opens_the_raster_and_reopening_uses_history_without_a_prompt(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.load(vec![harness.paths[0].clone()], cx);
    harness.assert_prompt(0, cx);
    harness.ui(cx, |window, cx| {
        assert!(harness.panel.read(cx).recent_files.paths().is_empty());
        window.input("EPSG:4326", cx);
        window.press("enter", cx);
    });
    // The Kit defers focus restoration after confirming a dialog.
    cx.executor().advance_clock(*gpui_kit::component::dialog::ANIMATION_DURATION);
    cx.run_until_parked();
    harness.ui(cx, |window, cx| {
        assert!(window.try_find("dialog").is_none());
        let panel = harness.panel.read(cx);
        assert!(!panel.is_loading());
        assert_eq!(panel.recent_files.epsg(&harness.paths[0]), Some(4326));
        assert_eq!(panel.sources.len(), 2);
        assert!(panel.load_error().is_none());
        assert!(panel.focus.is_focused(window));
    });
    harness.ui(cx, |window, cx| {
        harness.panel.update(cx, |panel, cx| {
            panel.clear(window, cx);
            panel.recent_files = crate::recent_files::RecentFiles::load_from(Some(harness._directory.path().join("recent-files.json")));
        });
    });
    harness.load(vec![harness.paths[0].clone()], cx);
    harness.ui(cx, |window, cx| {
        assert!(window.try_find("dialog").is_none());
        let panel = harness.panel.read(cx);
        assert!(!panel.is_loading());
        assert!(panel.load_error().is_none());
        assert_eq!(panel.sources.len(), 2, "Reopening replaces rather than duplicates the dataset");
    });
}

#[gpui_kit::test]
fn invalid_epsg_stays_in_the_dialog_and_clicking_open_retries(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.load(vec![harness.paths[0].clone()], cx);
    harness.ui(cx, |window, cx| {
        window.input("999999", cx);
        window.click("projection-open", cx);
    });
    harness.ui(cx, |window, cx| {
        assert!(window.find("projection-error").visible());
        assert_eq!(window.find("projection-epsg").value(), Some("999999"));
        assert!(harness.panel.read(cx).recent_files.paths().is_empty());
        window.press("secondary-a", cx);
        window.input("4326", cx);
    });
    harness.ui(cx, |window, cx| window.click("projection-open", cx));
    harness.ui(cx, |window, cx| {
        assert!(window.try_find("dialog").is_none());
        assert_eq!(harness.panel.read(cx).sources.len(), 2);
    });
}

#[gpui_kit::test]
fn escape_skips_one_file_then_cancel_finishes_the_batch_without_remembering(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.load(harness.paths.clone(), cx);
    harness.assert_prompt(0, cx);
    harness.ui(cx, |window, cx| {
        window.input("4326", cx);
        window.press("escape", cx);
    });
    harness.assert_prompt(1, cx);
    harness.ui(cx, |window, cx| window.click("projection-cancel", cx));
    harness.ui(cx, |window, cx| {
        assert!(window.try_find("dialog").is_none());
        let panel = harness.panel.read(cx);
        assert!(!panel.is_loading());
        assert!(panel.recent_files.paths().is_empty());
        assert!(panel.sources.is_empty());
        assert!(panel.focus.is_focused(window));
    });
}

#[gpui_kit::test]
fn each_file_in_a_batch_remembers_its_own_code_and_other_load_errors_are_preserved(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    let mut paths = harness.paths.clone();
    paths.push(harness.paths[0].with_file_name("missing.tif"));
    harness.load(paths, cx);
    for (index, code) in ["4326", "3857"].into_iter().enumerate() {
        harness.assert_prompt(index, cx);
        harness.ui(cx, |window, cx| {
            window.input(code, cx);
            window.press("enter", cx);
        });
    }
    harness.ui(cx, |window, cx| {
        assert!(window.try_find("dialog").is_none());
        let panel = harness.panel.read(cx);
        assert!(!panel.is_loading());
        assert_eq!(panel.sources.len(), 4);
        assert_eq!(panel.recent_files.epsg(&harness.paths[0]), Some(4326));
        assert_eq!(panel.recent_files.epsg(&harness.paths[1]), Some(3857));
        assert!(panel.load_error().unwrap().contains("missing.tif"));
    });
}

#[gpui_kit::test]
fn clearing_during_a_prompt_discards_the_remaining_batch(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.load(harness.paths.clone(), cx);
    harness.assert_prompt(0, cx);
    harness.ui(cx, |window, cx| harness.panel.update(cx, |panel, cx| panel.clear(window, cx)));
    harness.ui(cx, |window, cx| {
        assert!(window.try_find("dialog").is_none());
        assert!(!harness.panel.read(cx).is_loading());
        assert!(harness.panel.read(cx).projection_prompts.paths.is_empty());
    });
}

#[test]
fn failed_open_does_not_add_the_supplied_epsg_to_history() {
    let (progress, _) = async_channel::bounded(1);
    let loaded = load_paths(
        vec![DatasetRequest {
            path: PathBuf::from("/nonexistent/raster.tif"),
            epsg: Some(4326),
        }],
        progress,
    );
    assert_eq!(loaded.errors.len(), 1);
    assert!(loaded.opened_paths.is_empty());
}
