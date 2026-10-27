use super::*;

use gdal::spatial_ref::SpatialRef;
use gdal::vector::{Feature, FieldDefn, Geometry, LayerOptions, OGRFieldType};

fn write_projected_gpkg(path: &Path, names: &[&str]) -> Result<()> {
    let mut dataset = gdal::DriverManager::get_driver_by_name("GPKG")?.create_vector_only(path)?;
    let srs = SpatialRef::from_epsg(3857)?;
    for name in names {
        let layer = dataset.create_layer(LayerOptions {
            name,
            srs: Some(&srs),
            ..Default::default()
        })?;
        FieldDefn::new("ratio", OGRFieldType::OFTReal)?.add_to_layer(&layer)?;
        FieldDefn::new("code", OGRFieldType::OFTString)?.add_to_layer(&layer)?;
        // Intentionally much smaller than a single default GDAL Arrow batch.
        for index in 0..128 {
            let mut feature = Feature::new(layer.defn())?;
            feature.set_geometry(Geometry::from_wkt("POINT (445277.96317309426 6621293.722740169)")?)?;
            feature.set_field_double(0, index as f64 + 0.125)?;
            feature.set_field_string(1, "00123")?;
            feature.create(&layer)?;
        }
    }
    dataset.close()?;
    Ok(())
}

#[test]
fn progress_reports_real_conversion_and_stays_monotonic_across_layers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("projected.gpkg");
    write_projected_gpkg(&path, &["first", "second"])?;
    // Legacy progress deliberately honors caller configuration. Request the
    // per-feature path explicitly instead of assuming Arrow emits small batches.
    let _arrow_api = ScopedArrowApi::set(Some(c"NO"));
    let mut samples = Vec::new();
    let sources = VectorSource::open_with_progress(&path, &mut |fraction| samples.push(fraction))?.context("Expected vector layers")?;
    assert!(samples.iter().all(|fraction| fraction.is_finite() && (0.0..=1.0).contains(fraction)));
    assert!(samples.windows(2).all(|pair| pair[0] <= pair[1]), "Progress regressed: {samples:?}");
    assert!(
        samples.iter().any(|fraction| *fraction > 0.0 && *fraction < 0.5),
        "Missing first-layer progress: {samples:?}"
    );
    assert!(
        samples.iter().any(|fraction| *fraction > 0.5 && *fraction < 1.0),
        "Missing second-layer progress: {samples:?}"
    );
    assert_eq!(samples.last(), Some(&1.0));
    assert_eq!(sources.len(), 2);
    for source in sources {
        let [west, south, east, north] = source.bounds();
        assert!((west - 4.0).abs() < 1e-9 && (south - 51.0).abs() < 1e-9);
        assert_eq!([west, south], [east, north]);
        assert_eq!(source.numeric_properties()[0].name(), "ratio");
        assert_eq!(source.numeric_properties()[0].range(), (0.125, 127.125));
    }
    Ok(())
}

#[test]
fn load_progress_reports_intermediate_conversion_for_a_single_arrow_sized_layer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("single-batch.gpkg");
    write_projected_gpkg(&path, &["points"])?;
    let _arrow_api = ScopedArrowApi::set(Some(c"YES"));
    let mut events = Vec::new();
    VectorSource::open_with_load_progress(&path, &mut |event| {
        if matches!(event, VectorLoadProgress::Converting(fraction) if fraction > 0.0) || event == VectorLoadProgress::Preparing {
            assert_eq!(ScopedArrowApi::current().as_deref(), Some(c"NO"));
        }
        events.push(event);
    })?
    .context("Expected vector layer")?;

    assert_eq!(events.first(), Some(&VectorLoadProgress::Converting(0.0)));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, VectorLoadProgress::Converting(fraction) if *fraction > 0.0 && *fraction < 1.0)),
        "Missing per-feature conversion callbacks: {events:?}"
    );
    assert_eq!(
        &events[events.len() - 2..],
        &[VectorLoadProgress::Converting(1.0), VectorLoadProgress::Preparing]
    );
    assert_eq!(events.iter().filter(|event| **event == VectorLoadProgress::Preparing).count(), 1);
    assert_eq!(ScopedArrowApi::current().as_deref(), Some(c"YES"));
    Ok(())
}

#[test]
fn load_progress_prepares_each_layer_before_resuming_aggregate_conversion() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("layers.gpkg");
    write_projected_gpkg(&path, &["first", "second"])?;
    let mut events = Vec::new();
    let sources = VectorSource::open_with_load_progress(&path, &mut |event| events.push(event))?.context("Expected vector layers")?;
    let preparation_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(ix, event)| (*event == VectorLoadProgress::Preparing).then_some(ix))
        .collect();
    assert_eq!(sources.len(), 2);
    assert_eq!(preparation_positions.len(), 2, "Expected one preparation phase per layer: {events:?}");
    assert_eq!(events[preparation_positions[0] - 1], VectorLoadProgress::Converting(0.5));
    assert_eq!(events[preparation_positions[0] + 1], VectorLoadProgress::Converting(0.5));
    assert_eq!(events[preparation_positions[1] - 1], VectorLoadProgress::Converting(1.0));
    assert_eq!(events.last(), Some(&VectorLoadProgress::Preparing));
    for (start, end) in [(0, preparation_positions[0]), (preparation_positions[0] + 1, preparation_positions[1])] {
        let fractions: Vec<f64> = events[start..end]
            .iter()
            .map(|event| match event {
                VectorLoadProgress::Converting(fraction) => *fraction,
                VectorLoadProgress::Preparing => panic!("Unexpected preparation inside conversion"),
            })
            .collect();
        assert!(fractions.iter().all(|fraction| fraction.is_finite() && (0.0..=1.0).contains(fraction)));
        assert!(fractions.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(
            fractions
                .iter()
                .any(|fraction| *fraction > fractions[0] && *fraction < *fractions.last().unwrap())
        );
    }
    Ok(())
}

#[test]
fn preparing_is_reported_before_geojson_is_closed_and_finalized() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("preparation.gpkg");
    write_projected_gpkg(&path, &["points"])?;
    let dataset = gdalio::dataset::open_read_only(&path)?;
    let output = conversion_path();
    let mut complete_at_preparing = None;
    let (geojson, _) = translate_layer(
        &dataset,
        "points",
        &output,
        false,
        false,
        Some(ConversionProgress {
            callback: &mut |event| {
                if event == VectorLoadProgress::Preparing {
                    complete_at_preparing = Some(gdal::vsi::call_on_mem_file_bytes(&output, |bytes| {
                        serde_json::from_slice::<Value>(bytes).is_ok()
                    }));
                }
            },
            per_feature: true,
        }),
    )?;
    assert!(
        !complete_at_preparing.context("Missing preparation event")??,
        "Output was already finalized at Preparing"
    );
    assert_eq!(serde_json::from_str::<Value>(&geojson)?["features"].as_array().unwrap().len(), 128);
    assert!(gdal::vsi::call_on_mem_file_bytes(&output, |_| ()).is_err(), "Output buffer was not released");
    Ok(())
}

#[test]
fn load_progress_preserves_normal_output_properties_and_reprojected_crs() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("properties.gpkg");
    write_projected_gpkg(&path, &["points"])?;
    let _arrow_api = ScopedArrowApi::set(Some(c"YES"));
    let normal = VectorSource::open(&path)?.context("Expected normal vector source")?;
    let phased = VectorSource::open_with_load_progress(&path, &mut |_| {})?.context("Expected phased vector source")?;
    assert_eq!(normal.len(), phased.len());
    let normal = &normal[0];
    let phased = &phased[0];
    let normal_json: Value = serde_json::from_str(normal.geojson().unwrap())?;
    let phased_json: Value = serde_json::from_str(phased.geojson().unwrap())?;
    assert_eq!(normal_json, phased_json);
    assert_eq!(normal.name(), phased.name());
    assert_eq!(normal.bounds(), phased.bounds());
    assert_eq!(normal.points_only(), phased.points_only());
    assert_eq!(phased.numeric_properties()[0].name(), "ratio");
    assert_eq!(phased.numeric_properties()[0].range(), (0.125, 127.125));
    assert_eq!(phased_json["features"][0]["properties"], json!({"ratio": 0.125, "code": "00123"}));
    let [west, south, east, north] = phased.bounds();
    assert!((west - 4.0).abs() < 1e-9 && (south - 51.0).abs() < 1e-9);
    assert_eq!([west, south], [east, north]);
    Ok(())
}

#[test]
fn load_progress_restores_absent_empty_and_existing_thread_local_values() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("restore.gpkg");
    write_projected_gpkg(&path, &["points"])?;
    for previous in [None, Some(c""), Some(c"YES"), Some(c"NO"), Some(c"caller-value")] {
        let _restore = ScopedArrowApi::set(previous);
        assert_eq!(ScopedArrowApi::current().as_deref(), previous);
        VectorSource::open_with_load_progress(&path, &mut |_| {})?.context("Expected vector source")?;
        assert_eq!(ScopedArrowApi::current().as_deref(), previous);
    }
    Ok(())
}

#[test]
fn conversion_failure_restores_thread_local_values() -> Result<()> {
    let dataset = gdalio::dataset::create_in_memory()?;
    for previous in [None, Some(c""), Some(c"YES")] {
        let _restore = ScopedArrowApi::set(previous);
        let output = conversion_path();
        assert!(
            translate_layer(
                &dataset,
                "missing-layer",
                &output,
                false,
                false,
                Some(ConversionProgress {
                    callback: &mut |_| {},
                    per_feature: true,
                }),
            )
            .is_err()
        );
        assert_eq!(ScopedArrowApi::current().as_deref(), previous);
        assert!(gdal::vsi::call_on_mem_file_bytes(&output, |_| ()).is_err());
    }
    Ok(())
}

#[test]
fn callback_panics_restore_thread_local_values_without_unwinding_through_gdal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("callback-panic.gpkg");
    write_projected_gpkg(&path, &["points"])?;
    for previous in [None, Some(c""), Some(c"YES")] {
        let _restore = ScopedArrowApi::set(previous);
        for panic_on_preparing in [false, true] {
            let mut panics = 0;
            let error = VectorSource::open_with_load_progress(&path, &mut |event| {
                if if panic_on_preparing {
                    event == VectorLoadProgress::Preparing
                } else {
                    matches!(event, VectorLoadProgress::Converting(fraction) if fraction > 0.0)
                } {
                    panics += 1;
                    panic!("Callback panic during {event:?}");
                }
            })
            .expect_err("GDAL callback panics must become errors");
            assert!(format!("{error:#}").contains("progress callback panicked"));
            assert_eq!(panics, 1);
            assert_eq!(ScopedArrowApi::current().as_deref(), previous);
        }
    }
    Ok(())
}

#[test]
fn arrow_override_restores_thread_local_values_on_rust_unwind() {
    for previous in [None, Some(c""), Some(c"YES")] {
        let _restore = ScopedArrowApi::set(previous);
        let result = std::panic::catch_unwind(|| {
            let _conversion = ScopedArrowApi::set(Some(c"NO"));
            assert_eq!(ScopedArrowApi::current().as_deref(), Some(c"NO"));
            panic!("Rust unwind while the override is active");
        });
        assert!(result.is_err());
        assert_eq!(ScopedArrowApi::current().as_deref(), previous);
    }
}

#[test]
fn legacy_progress_keeps_the_callers_arrow_configuration() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy-arrow.gpkg");
    write_projected_gpkg(&path, &["points"])?;
    for previous in [None, Some(c""), Some(c"YES"), Some(c"NO")] {
        let _restore = ScopedArrowApi::set(previous);
        let mut samples = Vec::new();
        VectorSource::open_with_progress(&path, &mut |fraction| {
            assert_eq!(ScopedArrowApi::current().as_deref(), previous);
            samples.push(fraction);
        })?
        .context("Expected vector source")?;
        assert_eq!(samples.last(), Some(&1.0));
        assert_eq!(ScopedArrowApi::current().as_deref(), previous);
    }
    Ok(())
}

#[test]
fn tiled_sources_emit_no_load_progress_or_conversion_events() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("native.pmtiles");
    let metadata = serde_json::to_vec(&json!({"vector_layers": [{"id": "points", "fields": {}}]}))?;
    let mut header = [0_u8; 127];
    header[..8].copy_from_slice(b"PMTiles\x03");
    for (offset, value) in [
        (8, 127_u64),
        (16, 1),
        (24, 128),
        (32, metadata.len() as u64),
        (40, 128 + metadata.len() as u64),
        (56, 128 + metadata.len() as u64),
    ] {
        header[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    header[96..102].copy_from_slice(&[1, 1, 1, 1, 0, 14]);
    let mut bytes = header.to_vec();
    bytes.push(0); // Empty root directory; no tile reads are needed to open it.
    bytes.extend(metadata);
    std::fs::write(&path, bytes)?;
    let mut events = Vec::new();
    let sources = VectorSource::open_with_load_progress(&path, &mut |event| events.push(event))?.context("Expected native tiled source")?;
    assert_eq!(sources.len(), 1);
    assert!(sources[0].geojson().is_none());
    assert!(events.is_empty(), "Native tiled source emitted load progress: {events:?}");
    Ok(())
}

#[test]
fn load_progress_csv_preserves_typed_properties_and_missing_columns() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("typed.csv");
    std::fs::write(&path, "lon,lat,value,code\n4,51,1.25,00123\n5,52,,00007\n")?;
    std::fs::write(path.with_extension("csvt"), "Real,Real,Real,String\n")?;
    let mut events = Vec::new();
    let sources = VectorSource::open_with_load_progress(&path, &mut |event| events.push(event))?.context("Expected CSV points")?;
    let data: Value = serde_json::from_str(sources[0].geojson().context("Expected GeoJSON string")?)?;
    assert_eq!(
        data["features"][0]["properties"],
        json!({"lon": 4.0, "lat": 51.0, "value": 1.25, "code": "00123"})
    );
    assert_eq!(
        data["features"][1]["properties"],
        json!({"lon": 5.0, "lat": 52.0, "value": null, "code": "00007"})
    );
    assert_eq!(events.first(), Some(&VectorLoadProgress::Converting(0.0)));
    assert_eq!(events.last(), Some(&VectorLoadProgress::Preparing));
    Ok(())
}

#[test]
fn progress_enabled_csv_preserves_typed_properties_and_missing_columns() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("typed.csv");
    std::fs::write(&path, "lon,lat,value,code\n4,51,1.25,00123\n5,52,,00007\n")?;
    std::fs::write(path.with_extension("csvt"), "Real,Real,Real,String\n")?;
    let sources = VectorSource::open_with_progress(&path, &mut |_| {})?.context("Expected CSV points")?;
    let data: Value = serde_json::from_str(sources[0].geojson().context("Expected GeoJSON string")?)?;
    assert_eq!(
        data["features"][0]["properties"],
        json!({"lon": 4.0, "lat": 51.0, "value": 1.25, "code": "00123"})
    );
    assert_eq!(
        data["features"][1]["properties"],
        json!({"lon": 5.0, "lat": 52.0, "value": null, "code": "00007"})
    );
    Ok(())
}
