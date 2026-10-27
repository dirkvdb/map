//! Local, opt-in measurement, not a timing-threshold regression test.
//! Run in release mode with MAP_TEST_VECTOR pointing at a spatial vector file
//! and RUST_LOG=map::vector_load=info. No network basemap or GPUI window is used.

use super::*;

// Read only the TLS slot, not the effective value inherited from GDAL globals
// or the environment. Two fallbacks distinguish absent from present-but-empty.
fn thread_local_gdal_option(key: &str) -> Result<Option<String>> {
    let value = gdal::config::get_thread_local_config_option(key, "")?;
    let alternate = gdal::config::get_thread_local_config_option(key, "<absent>")?;
    Ok((value == alternate).then_some(value))
}

struct ThreadLocalGdalOption {
    key: &'static str,
    previous: Option<String>,
    // Restoration must happen on the same OS thread, even during unwinding.
    _same_thread: std::marker::PhantomData<Rc<()>>,
}

impl ThreadLocalGdalOption {
    fn set(key: &'static str, value: Option<&str>) -> Result<Self> {
        let previous = thread_local_gdal_option(key)?;
        match value {
            Some(value) => gdal::config::set_thread_local_config_option(key, value)?,
            None => gdal::config::clear_thread_local_config_option(key)?,
        }
        Ok(Self {
            key,
            previous,
            _same_thread: std::marker::PhantomData,
        })
    }
}

impl Drop for ThreadLocalGdalOption {
    fn drop(&mut self) {
        let restored = match &self.previous {
            Some(value) => gdal::config::set_thread_local_config_option(self.key, value),
            None => gdal::config::clear_thread_local_config_option(self.key),
        };
        if let Err(error) = restored {
            log::error!(target: "map::vector_load", "Could not restore thread-local GDAL option {}: {error}", self.key);
        }
    }
}

#[test]
fn thread_local_gdal_option_guard_restores_absent_empty_and_existing_values_on_unwind() -> Result<()> {
    const KEY: &str = "OGR2OGR_USE_ARROW_API";
    let original = thread_local_gdal_option(KEY)?;
    for previous in [None, Some(""), Some("YES")] {
        let outer = ThreadLocalGdalOption::set(KEY, previous)?;
        {
            let _inner = ThreadLocalGdalOption::set(KEY, Some("NO"))?;
            assert_eq!(thread_local_gdal_option(KEY)?.as_deref(), Some("NO"));
        }
        assert_eq!(thread_local_gdal_option(KEY)?.as_deref(), previous);
        let panicked = std::panic::catch_unwind(|| {
            let _inner = ThreadLocalGdalOption::set(KEY, Some("NO")).unwrap();
            panic!("exercise thread-local restoration during unwinding");
        });
        assert!(panicked.is_err());
        assert_eq!(thread_local_gdal_option(KEY)?.as_deref(), previous);
        drop(outer);
        assert_eq!(thread_local_gdal_option(KEY)?, original);
    }
    Ok(())
}

/// Loader-only comparison. CPL_DEBUG=OGR2OGR prints "Using WriteArrowBatch()"
/// when GDAL actually selects Arrow; callback timing alone is not that proof.
#[test]
#[ignore = "manual local GDAL progress comparison; requires MAP_TEST_VECTOR; no GPU or network"]
fn local_vector_arrow_progress_comparison() -> Result<()> {
    use gdal::vector::{LayerAccess as _, LayerCaps};

    const KEY: &str = "OGR2OGR_USE_ARROW_API";
    let _ = env_logger::builder().is_test(true).format_timestamp_millis().try_init();
    let path = std::path::PathBuf::from(std::env::var_os("MAP_TEST_VECTOR").context("Set MAP_TEST_VECTOR to the local vector file to measure")?);
    let original = thread_local_gdal_option(KEY)?;
    let original_debug = thread_local_gdal_option("CPL_DEBUG")?;
    let debug = ThreadLocalGdalOption::set("CPL_DEBUG", Some("OGR2OGR"))?;
    log::info!(target: "map::vector_load", "Arrow progress experiment: GDAL={}, path={path:?}, file_bytes={}, original_tls={original:?}",
        gdal::version::VersionInfo::release_name(), std::fs::metadata(&path)?.len());
    {
        let dataset = geo::vector::gdalio::dataset::open_read_only(&path)?;
        for layer in dataset.layers() {
            log::info!(target: "map::vector_load", "layer={:?} feature_count={:?} fast_arrow_stream={}",
                layer.name(), layer.try_feature_count(), layer.has_capability(LayerCaps::OLCFastGetArrowStream));
        }
    }

    let mut baseline: Option<Vec<Arc<VectorSource>>> = None;
    // Reverse the order on the warm repeat to reduce a simple cache/order bias.
    for (case, value) in [("normal-1", None), ("no-arrow-1", Some("NO")), ("no-arrow-2", Some("NO")), ("normal-2", None)] {
        let override_option = ThreadLocalGdalOption::set(KEY, value)?;
        log::info!(target: "map::vector_load", "case={case} effective_OGR2OGR_USE_ARROW_API={:?} (unset means GDAL auto-selection)",
            gdal::config::get_config_option(KEY, "<unset>")?);
        let started = Instant::now();
        let mut callbacks = Vec::with_capacity(10_000);
        // Record in memory, not a log per feature: otherwise logging itself
        // would distort the feature-at-a-time path's elapsed time.
        let sources =
            VectorSource::open_with_progress(&path, &mut |fraction| callbacks.push((fraction, started.elapsed())))?.context("Expected a vector dataset")?;
        let elapsed = started.elapsed();
        drop(override_option);
        assert_eq!(thread_local_gdal_option(KEY)?, original, "case={case}: TLS must be restored after every load");

        let mut milestones = Vec::new();
        let mut last_bucket = None;
        let mut previous_time = Duration::ZERO;
        let mut longest_callback_gap = Duration::ZERO;
        for &(fraction, at) in &callbacks {
            longest_callback_gap = longest_callback_gap.max(at - previous_time);
            previous_time = at;
            let bucket = (fraction * 10.0).floor() as u32;
            if last_bucket != Some(bucket) {
                milestones.push((fraction, at));
                last_bucket = Some(bucket);
            }
        }
        let zeros = callbacks.iter().filter(|(fraction, _)| *fraction == 0.0).count();
        let intermediate = callbacks.iter().filter(|(fraction, _)| *fraction > 0.0 && *fraction < 1.0).count();
        let complete = callbacks.iter().filter(|(fraction, _)| *fraction == 1.0).count();
        log::info!(target: "map::vector_load",
            "case={case} progress summary: source_load={elapsed:?}, callbacks={}, zero={zeros}, intermediate={intermediate}, complete={complete}, first={:?}, last={:?}, longest_gap_including_start={longest_callback_gap:?}, decile_milestones={milestones:?}, layers={}, geojson_bytes={}",
            callbacks.len(), callbacks.first(), callbacks.last(), sources.len(), sources.iter().filter_map(|source| source.geojson()).map(str::len).sum::<usize>());
        ensure!(
            callbacks.last().is_some_and(|(fraction, _)| *fraction == 1.0),
            "case={case}: expected completion callback"
        );
        ensure!(
            callbacks.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "case={case}: progress must be monotonic"
        );
        if let Some(baseline) = &baseline {
            ensure!(sources.len() == baseline.len(), "case={case}: layer count changed");
            for (source, expected) in sources.iter().zip(baseline) {
                // Compare without re-parsing or serializing the 100 MB output.
                // Exact equality also checks preservation of geometry/properties.
                ensure!(
                    source.name() == expected.name() && source.geojson() == expected.geojson(),
                    "case={case}: converted GeoJSON changed"
                );
            }
            log::info!(target: "map::vector_load", "case={case} output is byte-identical to normal-1");
        } else {
            baseline = Some(sources);
        }
    }
    drop(debug);
    assert_eq!(thread_local_gdal_option("CPL_DEBUG")?, original_debug);
    assert_eq!(thread_local_gdal_option(KEY)?, original);
    Ok(())
}

#[test]
#[ignore = "manual local GPU timing; requires MAP_TEST_VECTOR and a wgpu-compatible graphics device"]
fn local_vector_load_frame_timings() -> Result<()> {
    use gdal::vector::LayerAccess as _;

    let _ = env_logger::builder().is_test(true).format_timestamp_millis().try_init();
    let path = std::path::PathBuf::from(std::env::var_os("MAP_TEST_VECTOR").context("Set MAP_TEST_VECTOR to the local vector file to measure")?);
    let viewport = Viewport {
        width: NonZeroU32::new(1200).unwrap(),
        height: NonZeroU32::new(760).unwrap(),
        scale: 1.0,
    };
    let warmup_started = Instant::now();
    let mut map = NativeMap::with_style(viewport, Some(tests::DARK_STYLE))?;
    map.render(MapRequest {
        viewport,
        camera: MapCamera::default(),
    })?;
    log::info!(target: "map::vector_load",
        "local GPU timing: file={:?}, file_bytes={}, viewport=1200x760 scale=1, renderer/background/readback warmup={:?} (excluded)",
        path, std::fs::metadata(&path)?.len(), warmup_started.elapsed());

    // Reopen on the second pass as well: this reports a genuinely warm load,
    // not merely attaching the already-converted GeoJSON again.
    for case in ["warm-renderer", "warm-repeat"] {
        map.set_vector(None, 1.0, "#3b82f6")?;
        let load_started = Instant::now();
        let mut conversion_complete = None;
        let sources = VectorSource::open_with_progress(&path, &mut |fraction| {
            if fraction >= 1.0 && conversion_complete.is_none() {
                conversion_complete = Some(load_started.elapsed());
            }
        })?
        .context("Expected a vector dataset")?;
        let source_load = load_started.elapsed();
        let source = sources.first().context("Expected at least one vector layer")?.clone();
        log::info!(target: "map::vector_load",
            "case={case} source={} VectorSource::open_with_progress complete: elapsed={source_load:?}, first_progress_100pct={conversion_complete:?}, layers={}, selected_layer={:?}, geojson_bytes={:?}",
            source.id(), sources.len(), source.name(), source.geojson().map(str::len));

        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.bounds(), 1200.0, 760.0),
        };
        // Same camera, viewport, and local style make pixel differences evidence
        // of vector data, unlike MapFrame's requested source ID. This extra
        // harness-only background render is outside the installation timer.
        let background_started = Instant::now();
        let background = map.render(request)?;
        let background_preparation = background_started.elapsed();
        let installation_started = Instant::now();
        map.set_vector(Some(source.clone()), 1.0, "#3b82f6")?;
        let installation = installation_started.elapsed();
        map.apply_request(request);
        let installed = Instant::now();
        // Keep this manual test bounded even if native work stops making progress.
        let deadline = installed + Duration::from_secs(90);
        map.deadline = deadline;
        let mut first_frame = None;
        let mut first_data_frame = None;
        let mut latest = None;
        let mut frames = 0;
        let mut comparison_time = Duration::ZERO;
        loop {
            ensure!(Instant::now() < deadline, "case={case}: vector did not settle within 90 seconds");
            let render_started = Instant::now();
            if let Some(texture) = map.poll_frame()? {
                let rendering = render_started.elapsed();
                let readback_started = Instant::now();
                let pixels = map.read_frame(&texture)?;
                let readback = readback_started.elapsed();
                let available = installed.elapsed();
                frames += 1;
                first_frame.get_or_insert(available);
                let comparison_started = Instant::now();
                let differs = pixels.as_raw() != background.as_raw();
                comparison_time += comparison_started.elapsed();
                if differs && first_data_frame.is_none() {
                    first_data_frame = Some(available);
                    log::info!(target: "map::vector_load",
                        "case={case} source={} first actual data-bearing readback: after_install={available:?}, including_install={:?}, since_source_open={:?}, frame={frames}, native_render={rendering:?}, readback={readback:?} (pixels differ from same-camera local background)",
                        source.id(), installation_started.elapsed(), load_started.elapsed());
                }
                latest = Some(pixels);
            }
            if map.idle.get() && !map.needs_frame.get() && !map.numeric.borrow().is_loading() {
                break;
            }
            // Match the worker's approximate cadence, rather than busy-polling.
            std::thread::sleep(Duration::from_millis(16));
        }
        let settled = installed.elapsed();
        let latest = latest.context("Native renderer produced no frames")?;
        let changed_pixels = latest
            .pixels()
            .zip(background.pixels())
            .filter(|(actual, background)| actual != background)
            .count();
        ensure!(
            first_data_frame.is_some() && changed_pixels > 0,
            "case={case}: native idle must not be mistaken for vector display; no vector pixels differed from the background"
        );
        let dataset = geo::vector::gdalio::dataset::open_read_only(&path)?;
        let feature_count = dataset.layer_by_name(source.name())?.try_feature_count();
        log::info!(target: "map::vector_load",
            "case={case} source={} local timing summary: source_load={source_load:?}, progress_100pct={conversion_complete:?}, harness_background_preparation={background_preparation:?}, installation={installation:?}, first_readback_after_install={first_frame:?}, first_data_after_install={first_data_frame:?}, native_idle_after_install={settled:?}, install_through_idle={:?}, frames={frames}, settled_changed_pixels={changed_pixels}, feature_count={feature_count:?}, pixel_comparisons={comparison_time:?}",
            source.id(), installation + settled);
    }
    Ok(())
}
