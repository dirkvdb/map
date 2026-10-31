use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{TestAppContext, px, size};

use super::*;

/// Gates completion so UI tests exercise the production updater without timing
/// their assertions against a particular machine's GDAL throughput.
pub(crate) struct ControlledLoad {
    progress: async_channel::Sender<LoadProgress>,
    completion: async_channel::Sender<LoadedDatasets>,
}

impl ControlledLoad {
    pub(crate) fn start(panel: &Entity<RasterPanel>, cx: &mut App) -> Self {
        let (progress, receiver) = async_channel::bounded(1);
        let (completion, completed) = async_channel::bounded(1);
        let work = cx.spawn(async move |_| completed.recv().await.unwrap());
        panel.update(cx, |panel, cx| panel.start_load(work, receiver, cx));
        Self { progress, completion }
    }

    pub(crate) fn report_conversion(&self, fraction: f64) {
        self.progress.force_send(LoadProgress::Converting(fraction)).unwrap();
    }

    pub(crate) fn report_preparing(&self) {
        self.progress.force_send(LoadProgress::Preparing).unwrap();
    }

    pub(crate) fn report_opening(&self) {
        self.progress.force_send(LoadProgress::Opening).unwrap();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.progress.is_closed()
    }

    pub(crate) fn finish_vector(&self, source: Arc<VectorSource>) {
        assert!(
            self.completion
                .try_send(LoadedDatasets {
                    selected: Some(SelectionId::Vector(source.id())),
                    vectors: vec![source],
                    ..Default::default()
                })
                .is_ok()
        );
    }

    pub(crate) fn finish(&self, error: Option<&str>) {
        assert!(
            self.completion
                .try_send(LoadedDatasets {
                    errors: error.into_iter().map(str::to_owned).collect(),
                    ..Default::default()
                })
                .is_ok()
        );
    }
}

fn open_panel(cx: &mut TestAppContext) -> (Entity<RasterPanel>, gpui_kit::AnyWindowHandle) {
    cx.update(gpui_kit::init);
    let mut panel = None;
    let handle = cx.open_window(size(px(800.0), px(600.0)), |window, cx| {
        let view = cx.new(|cx| RasterPanel::new(window, cx));
        panel = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    cx.run_until_parked();
    (panel.unwrap(), handle.into())
}

#[gpui_kit::test]
fn progress_starts_opening_immediately_but_stays_absent_while_the_picker_is_open(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    cx.update(|cx| {
        assert!(!panel.read(cx).is_loading());
        assert!(panel.read(cx).load_progress.is_none());
        panel.update(cx, |panel, cx| panel.prompt(cx));
        assert!(panel.read(cx).is_loading());
        assert!(panel.read(cx).load_progress.is_none());
        assert!(panel.read(cx).render_load_progress().is_none());
        assert!(panel.read(cx).load_status().is_none());
    });
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    assert!(!cx.update(|cx| panel.read(cx).is_loading()));

    let load = cx.update(|cx| {
        let load = ControlledLoad::start(&panel, cx);
        let panel = panel.read(cx);
        assert!(panel.is_loading());
        assert_eq!(
            panel.load_progress,
            Some(LoadProgress::Opening),
            "Opening cannot wait for the worker's first callback"
        );
        assert_eq!(panel.load_status().as_deref(), Some("Opening datasets"));
        assert!(panel.render_load_progress().is_some());
        load
    });
    load.finish(None);
    cx.run_until_parked();
}

#[gpui_kit::test]
fn progress_coalesces_to_the_latest_sample_and_throttles_notifications(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    cx.run_until_parked();
    let notifications = Rc::new(Cell::new(0));
    let _observation = cx.update(|cx| {
        let notifications = notifications.clone();
        cx.observe(&panel, move |_, _| notifications.set(notifications.get() + 1))
    });

    for step in 0..=10_000 {
        load.report_conversion(step as f64 / 20_000.0);
    }
    assert_eq!(load.progress.len(), 1, "GDAL must never enqueue an unbounded update backlog");
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Converting(0.5)));
    assert_eq!(notifications.get(), 1);

    for step in 5_001..=9_000 {
        load.report_conversion(step as f64 / 10_000.0);
    }
    cx.run_until_parked();
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL - Duration::from_millis(1));
    cx.run_until_parked();
    assert_eq!(notifications.get(), 1, "Progress cannot notify before the throttle deadline");
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Converting(0.9)));
    assert_eq!(notifications.get(), 2);

    load.finish(None);
    cx.run_until_parked();
}

#[gpui_kit::test]
fn progress_completion_and_errors_reset_the_indicator_and_close_delivery(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    for error in [None, Some("Reprojection failed")] {
        let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
        load.report_conversion(0.4);
        cx.run_until_parked();
        cx.update(|cx| assert!(panel.read(cx).render_load_progress().is_some()));

        // Completion must win even while the progress updater is sleeping.
        load.finish(error);
        cx.run_until_parked();
        cx.update(|cx| {
            let panel = panel.read(cx);
            assert!(!panel.is_loading());
            assert!(panel.render_load_progress().is_none());
            assert!(panel.load_task.is_none());
            assert_eq!(panel.load_error.as_deref(), error);
        });
        assert!(load.progress.force_send(LoadProgress::Converting(0.95)).is_err());
        assert!(load.is_closed());
        cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| panel.read(cx).load_progress), None);
    }
}

#[gpui_kit::test]
fn progress_clear_cancels_the_load_and_rejects_late_updates_from_an_old_generation(cx: &mut TestAppContext) {
    let (panel, window) = open_panel(cx);
    let old_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    old_load.report_conversion(0.3);
    cx.run_until_parked();
    let old_generation = cx.update(|cx| panel.read(cx).load_generation);
    cx.update_window(window, |_, window, cx| panel.update(cx, |panel, cx| panel.clear(window, cx)))
        .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let panel = panel.read(cx);
        assert!(!panel.is_loading());
        assert!(panel.load_progress.is_none());
        assert!(panel.load_task.is_none());
    });
    assert!(old_load.progress.force_send(LoadProgress::Converting(0.99)).is_err());
    assert!(old_load.completion.is_closed(), "Clear must cancel completion as well as the progress updater");
    assert!(
        old_load.completion.try_send(LoadedDatasets::default()).is_err(),
        "A cleared load cannot finish late"
    );

    let new_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    new_load.report_conversion(0.2);
    cx.run_until_parked();
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            for progress in [
                LoadProgress::Opening,
                LoadProgress::Converting(1.0),
                LoadProgress::Preparing,
                LoadProgress::Rendering(0),
            ] {
                panel.update_load_progress(old_generation, progress, cx);
            }
        });
        assert_eq!(panel.read(cx).load_progress, Some(LoadProgress::Converting(0.2)));
    });
    new_load.finish(None);
    cx.run_until_parked();
}

#[gpui_kit::test]
fn progress_ignores_invalid_and_regressing_conversion_fractions(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            let generation = panel.load_generation;
            for fraction in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                panel.update_load_progress(generation, LoadProgress::Converting(fraction), cx);
                assert_eq!(panel.load_progress, Some(LoadProgress::Opening));
            }
            panel.update_load_progress(generation, LoadProgress::Converting(-1.0), cx);
            assert_eq!(panel.load_progress, Some(LoadProgress::Converting(0.0)));
            panel.update_load_progress(generation, LoadProgress::Converting(0.6), cx);
            for fraction in [0.2, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                panel.update_load_progress(generation, LoadProgress::Converting(fraction), cx);
                assert_eq!(panel.load_progress, Some(LoadProgress::Converting(0.6)));
            }
            panel.update_load_progress(generation, LoadProgress::Converting(2.0), cx);
            assert_eq!(panel.load_progress, Some(LoadProgress::Converting(1.0)));
            panel.update_load_progress(generation, LoadProgress::Rendering(0), cx);
            assert_eq!(panel.load_progress, Some(LoadProgress::Converting(1.0)), "Only completion may begin rendering");
        });
    });
    load.finish(None);
    cx.run_until_parked();
}

fn vector_fixture() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("points.geojson");
    std::fs::write(
        &path,
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"value":1},"geometry":{"type":"Point","coordinates":[4,51]}}]}"#,
    )
    .unwrap();
    (directory, path)
}

fn open_vector() -> Arc<VectorSource> {
    let (_directory, path) = vector_fixture();
    VectorSource::open(&path).unwrap().unwrap().remove(0)
}

#[gpui_kit::test]
fn progress_phase_changes_allow_conversion_to_resume_after_preparing_and_opening(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    load.report_conversion(0.6);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Converting(0.6)));

    load.report_preparing();
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Preparing));

    load.report_conversion(0.7);
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Converting(0.7)));

    load.report_opening();
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Opening));

    load.report_conversion(0.1);
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Converting(0.1)));
    load.finish(None);
    cx.run_until_parked();
}

#[gpui_kit::test]
fn progress_preparing_remains_visible_after_conversion_reaches_100_percent(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    load.report_conversion(1.0);
    cx.run_until_parked();
    cx.update(|cx| {
        let panel = panel.read(cx);
        assert!(panel.is_loading(), "Conversion completion is not load completion");
        assert_eq!(panel.load_progress, Some(LoadProgress::Converting(1.0)));
        assert_eq!(panel.load_status().as_deref(), Some("Converting vectors · 100%"));
    });

    load.report_preparing();
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    cx.update(|cx| {
        let panel = panel.read(cx);
        assert!(panel.is_loading());
        assert_eq!(panel.load_progress, Some(LoadProgress::Preparing));
        assert_eq!(panel.load_status().as_deref(), Some("Preparing vector data"));
        assert!(!panel.load_status().unwrap().contains('%'), "Preparation has no measured overall percentage");
        assert!(panel.render_load_progress().is_some());
    });
    load.finish(None);
    cx.run_until_parked();
}

#[gpui_kit::test]
fn progress_vector_completion_waits_for_the_exact_rendered_source(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    let source = open_vector();
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    load.report_preparing();
    cx.run_until_parked();
    cx.update(|cx| {
        panel.update(cx, |panel, cx| panel.complete_vector_render(source.id(), cx));
        assert_eq!(
            panel.read(cx).load_progress,
            Some(LoadProgress::Preparing),
            "Frames cannot finish background preparation"
        );
    });

    load.finish_vector(source.clone());
    cx.run_until_parked();
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    assert!(load.is_closed());
    cx.update(|cx| {
        let panel = panel.read(cx);
        assert!(!panel.is_loading(), "The loader is finished even though rendering is pending");
        assert!(panel.load_task.is_none());
        assert_eq!(panel.selected, Some(SelectionId::Vector(source.id())));
        assert_eq!(panel.vectors.len(), 1);
        assert!(Arc::ptr_eq(panel.active_vector().unwrap(), &source));
        assert_eq!(panel.load_progress, Some(LoadProgress::Rendering(source.id())));
        assert_eq!(panel.load_status().as_deref(), Some("Rendering vectors"));
        assert!(panel.render_load_progress().is_some());
        assert!(panel.load_error.is_none());
    });
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            panel.complete_vector_render(source.id().wrapping_add(1), cx);
            panel.update_load_progress(panel.load_generation, LoadProgress::Preparing, cx);
            assert_eq!(panel.load_progress, Some(LoadProgress::Rendering(source.id())));
            panel.complete_vector_render(source.id(), cx);
            assert!(panel.load_progress.is_none());
            assert!(panel.render_load_progress().is_none());
            assert!(panel.load_status().is_none());
            panel.complete_vector_render(source.id(), cx);
            assert!(panel.load_progress.is_none(), "Duplicate frame completions cannot revive progress");
        });
    });
}

#[gpui_kit::test]
fn progress_new_load_rejects_a_queued_stale_vector_completion(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    let stale_source = open_vector();
    let old_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    old_load.report_conversion(0.4);
    cx.run_until_parked();
    let old_generation = cx.update(|cx| panel.read(cx).load_generation);

    // Queue a result, then replace the retained task before the executor can
    // install it. A ready old result is just as stale as a late callback.
    old_load.finish_vector(stale_source.clone());
    let new_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    cx.run_until_parked();
    assert!(old_load.is_closed());
    assert!(old_load.completion.is_closed());
    assert!(old_load.completion.try_send(LoadedDatasets::default()).is_err());
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            assert_ne!(panel.load_generation, old_generation);
            panel.update_load_progress(old_generation, LoadProgress::Preparing, cx);
            panel.complete_vector_render(stale_source.id(), cx);
            assert!(panel.is_loading());
            assert_eq!(panel.load_progress, Some(LoadProgress::Opening));
            assert!(panel.vectors.is_empty(), "The old result must not install its vector");
            assert!(panel.selected.is_none());
            assert!(panel.load_error.is_none());
        });
    });

    new_load.finish(Some("Current load failed"));
    cx.run_until_parked();
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            panel.update_load_progress(old_generation, LoadProgress::Converting(1.0), cx);
            panel.complete_vector_render(stale_source.id(), cx);
            assert!(!panel.is_loading());
            assert!(panel.load_progress.is_none());
            assert!(panel.vectors.is_empty());
            assert_eq!(panel.load_error.as_deref(), Some("Current load failed"));
        });
    });
}

#[gpui_kit::test]
fn progress_new_load_replaces_pending_rendering_and_rejects_the_old_frame(cx: &mut TestAppContext) {
    let (panel, _) = open_panel(cx);
    let old_source = open_vector();
    let new_source = open_vector();
    let old_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    old_load.finish_vector(old_source.clone());
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Rendering(old_source.id())));

    let new_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            panel.complete_vector_render(old_source.id(), cx);
            assert_eq!(panel.load_progress, Some(LoadProgress::Opening));
        });
    });
    new_load.finish_vector(new_source.clone());
    cx.run_until_parked();
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            panel.complete_vector_render(old_source.id(), cx);
            assert_eq!(panel.load_progress, Some(LoadProgress::Rendering(new_source.id())));
            assert_eq!(panel.selected, Some(SelectionId::Vector(new_source.id())));
            panel.complete_vector_render(new_source.id(), cx);
            assert!(panel.load_progress.is_none());
        });
    });
}

#[gpui_kit::test]
fn progress_clear_invalidates_pending_rendering_and_late_frame_completion(cx: &mut TestAppContext) {
    let (panel, window) = open_panel(cx);
    let source = open_vector();
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    load.finish_vector(source.clone());
    cx.run_until_parked();
    let generation = cx.update(|cx| panel.read(cx).load_generation);
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Rendering(source.id())));

    cx.update_window(window, |_, window, cx| panel.update(cx, |panel, cx| panel.clear(window, cx)))
        .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            assert_ne!(panel.load_generation, generation);
            panel.complete_vector_render(source.id(), cx);
            panel.update_load_progress(generation, LoadProgress::Preparing, cx);
            assert!(!panel.is_loading());
            assert!(panel.load_progress.is_none());
            assert!(panel.render_load_progress().is_none());
            assert!(panel.vectors.is_empty());
            assert!(panel.selected.is_none());
            assert!(panel.load_task.is_none());
            assert!(panel.load_error.is_none());
        });
    });
}

#[gpui_kit::test]
fn progress_selecting_another_source_clears_pending_rendering(cx: &mut TestAppContext) {
    let (panel, window) = open_panel(cx);
    let first = open_vector();
    let second = open_vector();
    let first_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    first_load.finish_vector(first.clone());
    cx.run_until_parked();
    let second_load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    second_load.finish_vector(second.clone());
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.select(Some(SelectionId::Vector(second.id())), false, window, cx);
            assert_eq!(
                panel.load_progress,
                Some(LoadProgress::Rendering(second.id())),
                "Selecting the pending source keeps its progress"
            );
            panel.select(Some(SelectionId::Vector(first.id())), false, window, cx);
            assert_eq!(panel.selected, Some(SelectionId::Vector(first.id())));
            assert!(panel.load_progress.is_none());
            assert!(panel.render_load_progress().is_none());
            panel.complete_vector_render(second.id(), cx);
            panel.select(Some(SelectionId::Vector(second.id())), false, window, cx);
            assert!(
                panel.load_progress.is_none(),
                "Returning to a source cannot revive an obsolete rendering handoff"
            );
        });
    })
    .unwrap();
}

#[test]
fn progress_loader_aggregates_real_vector_callbacks_with_raster_paths_and_errors_without_blocking() {
    let (directory, vector) = vector_fixture();
    let raster = directory.path().join("raster.tif");
    crate::raster_source::test_support::write_raster(&raster, 4326, 2).unwrap();
    let (progress, receiver) = async_channel::bounded(1);
    // Deliberately never drain during loading: progress delivery cannot block
    // either a GDAL callback or processing the remaining paths.
    let loaded = load_paths(vec![directory.path().join("missing.tif").into(), vector.into(), raster.into()], progress);
    assert_eq!(loaded.errors.len(), 1);
    assert_eq!(loaded.vectors.len(), 1);
    assert_eq!(loaded.sources.len(), 2);
    assert_eq!(loaded.selected, Some(SelectionId::Raster(loaded.sources[0].metadata().id)));
    assert_eq!(
        receiver.try_recv().unwrap(),
        LoadProgress::Opening,
        "The last raster opens without vector conversion; it must not synthesize overall 100%"
    );
    assert!(receiver.is_closed());
}

#[test]
fn progress_loader_finishes_vector_paths_in_preparing_without_fake_overall_completion() {
    let (directory, vector) = vector_fixture();
    let raster = directory.path().join("raster.tif");
    crate::raster_source::test_support::write_raster(&raster, 4326, 2).unwrap();
    let (progress, receiver) = async_channel::bounded(1);
    let loaded = load_paths(vec![directory.path().join("missing.tif").into(), raster.into(), vector.into()], progress);
    assert_eq!(loaded.errors.len(), 1);
    assert_eq!(loaded.sources.len(), 2);
    assert_eq!(loaded.vectors.len(), 1);
    assert_eq!(loaded.selected, Some(SelectionId::Vector(loaded.vectors[0].id())));
    assert_eq!(
        receiver.try_recv().unwrap(),
        LoadProgress::Preparing,
        "Vector preparation must outlive the last conversion callback"
    );
    assert!(receiver.is_closed());
}

#[gpui_kit::test]
fn progress_delivery_closes_when_the_panel_is_dropped(cx: &mut TestAppContext) {
    let (panel, window) = open_panel(cx);
    let load = cx.update(|cx| ControlledLoad::start(&panel, cx));
    load.report_conversion(0.25);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| panel.read(cx).load_progress), Some(LoadProgress::Converting(0.25)));

    let released = panel.downgrade();
    cx.update_window(window, |_, window, _| window.remove_window()).unwrap();
    drop(panel);
    // GPUI destroys released entities during an app effect cycle. Draining
    // executor tasks alone does not drop the panel's retained load task.
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(released.upgrade().is_none(), "The closed window must release its panel");
    assert!(load.progress.force_send(LoadProgress::Converting(0.5)).is_err());
    assert!(load.completion.is_closed(), "Dropping the panel must also cancel the pending load");
    cx.executor().advance_clock(LOAD_PROGRESS_INTERVAL);
    cx.run_until_parked();
    assert!(load.is_closed(), "The canceled updater cannot resume after its throttle deadline");
}
