use super::*;

fn features(properties: &[Value]) -> Vec<Value> {
    properties
        .iter()
        .map(|properties| json!({"type": "Feature", "properties": properties, "geometry": {"type": "Point", "coordinates": [4, 51]}}))
        .collect()
}

fn source(properties: &[Value]) -> VectorSource {
    let features = features(properties);
    let numeric_properties = geojson_numeric_properties(&features);
    let points_only = geojson_points_only(&features);
    VectorSource::new(
        "values".to_owned(),
        [4.0, 51.0, 4.0, 51.0],
        VectorData::GeoJson(Arc::new(json!({"type": "FeatureCollection", "features": features}).to_string())),
        numeric_properties,
        points_only,
    )
}

#[test]
fn metadata_discovers_integers_fractions_and_mixed_missing_values() {
    let source = source(&[
        json!({"integer": -7, "fraction": 0.125, "mixed": "12", "boolean": true, "string": "42", "null": null, "nested": {"n": 1}, "array": [1]}),
        json!({"integer": 9_000_000_000_i64, "fraction": 0.875, "mixed": -2.5, "boolean": false}),
        json!({"fraction": null, "mixed": 8}),
        json!({"mixed": true}),
        json!({"mixed": {"value": 50}}),
        json!({}),
        Value::Null,
    ]);
    assert_eq!(
        source
            .numeric_properties()
            .iter()
            .map(|property| (property.name(), property.range()))
            .collect::<Vec<_>>(),
        [("fraction", (0.125, 0.875)), ("integer", (-7.0, 9_000_000_000.0)), ("mixed", (-2.5, 8.0))]
    );
    assert_eq!(source.property(), Some("fraction"));
    assert_eq!(source.range(), Some((0.125, 0.875)));
    assert_eq!(source.data_range(), source.range());
    assert_eq!(source.palette(), ColorMapPreset::Terrain);
}

#[test]
fn constants_have_finite_increasing_ranges_including_float_extremes() {
    for value in [0.0, -0.0, 0.125, -42.0, f64::MIN_POSITIVE, f64::from_bits(1), -f64::MAX, f64::MAX] {
        let source = source(&[json!({"constant": value}), json!({"constant": value})]);
        let (min, max) = source.numeric_properties()[0].range();
        assert!(min.is_finite() && max.is_finite() && min < max, "value={value}, range={min}..{max}");
        assert!(min <= value && value <= max);
        assert_eq!(source.data_range(), Some((min, max)));
    }
}

#[test]
fn absent_numeric_values_keep_uniform_theme_coloring() -> Result<()> {
    for properties in [
        vec![],
        vec![json!({"boolean": true, "string": "1.5", "null": null, "nested": {"n": 1}, "array": [1]})],
    ] {
        let source = source(&properties);
        assert!(source.numeric_properties().is_empty());
        assert_eq!(source.property(), None);
        assert_eq!(source.range(), None);
        assert_eq!(source.data_range(), None);
        assert!(source.with_property("string").is_err());
        assert!(source.restyled(ColorMapPreset::Gray, (0.0, 1.0)).is_err());
        for layer in source.layers_json(0.25, "rgb(12,34,56)") {
            let kind = layer["type"].as_str().unwrap();
            assert_eq!(layer["paint"][format!("{kind}-color")], "rgb(12,34,56)");
            maplibre_native::AnyLayer::from_json_value(&layer)?;
        }
    }
    Ok(())
}

#[test]
fn open_computes_metadata_during_materialization_and_preserves_geojson() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("values.geojson");
    let features = features(&[
        json!({"count": 2, "fraction": 0.125, "boolean": true, "string": "0012"}),
        json!({"count": 14, "fraction": 0.875, "boolean": false}),
        json!({"count": null}),
    ]);
    std::fs::write(&path, serde_json::to_vec(&json!({"type": "FeatureCollection", "features": features}))?)?;
    let source = VectorSource::open(&path)?.context("Expected GeoJSON")?.remove(0);
    std::fs::remove_file(path)?;
    assert_eq!(
        source
            .numeric_properties()
            .iter()
            .map(|property| (property.name(), property.range()))
            .collect::<Vec<_>>(),
        [("count", (2.0, 14.0)), ("fraction", (0.125, 0.875))]
    );
    let selected = source.with_property("fraction")?;
    assert_eq!(selected.range(), Some((0.125, 0.875)));
    assert_eq!(selected.source_json(), source.source_json());
    assert_eq!(source.source_json()["data"]["features"][0]["properties"], features[0]["properties"]);
    Ok(())
}

#[test]
fn restyling_shares_data_and_metadata_without_changing_layer_identity() -> Result<()> {
    let source = source(&[json!({"a": 2, "b": -8}), json!({"a": 6, "b": -2})]);
    let styled = source.restyled(ColorMapPreset::Rainbow, (-1.0, 10.0))?;
    let selected = styled.with_property("b")?;
    for snapshot in [&*styled, &*selected] {
        assert_eq!(snapshot.id(), source.id());
        assert_eq!(snapshot.name(), source.name());
        assert_eq!(snapshot.bounds(), source.bounds());
        assert_eq!(snapshot.source_json(), source.source_json());
        assert!(Arc::ptr_eq(&snapshot.numeric_properties, &source.numeric_properties));
        match (&snapshot.data, &source.data) {
            (VectorData::GeoJson(actual), VectorData::GeoJson(original)) => assert!(Arc::ptr_eq(actual, original)),
            _ => panic!("Expected shared GeoJSON"),
        }
        assert_eq!(
            snapshot.layers_json(1.0, "red").iter().map(|layer| layer["id"].clone()).collect::<Vec<_>>(),
            source.layers_json(1.0, "red").iter().map(|layer| layer["id"].clone()).collect::<Vec<_>>()
        );
    }
    assert_ne!(styled.style_id(), source.style_id());
    assert_ne!(selected.style_id(), styled.style_id());
    assert_ne!(selected.style_id(), source.style_id());
    assert_eq!(source.property(), Some("a"));
    assert_eq!(source.range(), Some((2.0, 6.0)));
    assert_eq!(styled.property(), Some("a"));
    assert_eq!(styled.data_range(), Some((2.0, 6.0)));
    assert_eq!(selected.property(), Some("b"));
    assert_eq!(selected.palette(), ColorMapPreset::Rainbow);
    assert_eq!(selected.range(), Some((-8.0, -2.0)));
    assert_eq!(selected.data_range(), Some((-8.0, -2.0)));
    Ok(())
}

#[test]
fn noop_styling_retains_revision_and_reselecting_resets_edited_range() -> Result<()> {
    let source = source(&[json!({"value": 2}), json!({"value": 6})]);
    assert_eq!(source.with_property("value")?.style_id(), source.style_id());
    assert_eq!(source.restyled(source.palette(), source.range().unwrap())?.style_id(), source.style_id());
    let edited = source.restyled(ColorMapPreset::Gray, (0.0, 10.0))?;
    let reset = edited.with_property("value")?;
    assert_ne!(reset.style_id(), edited.style_id());
    assert_eq!(reset.range(), source.range());
    assert_eq!(reset.palette(), ColorMapPreset::Gray);
    let independent = source.restyled(ColorMapPreset::Gray, (0.0, 10.0))?;
    assert_ne!(independent.style_id(), edited.style_id());
    Ok(())
}

#[test]
fn styling_rejects_unknown_properties_and_invalid_ranges() {
    let source = source(&[json!({"value": 2, "label": "2"}), json!({"value": 6})]);
    for property in ["missing", "label", "VALUE", ""] {
        assert!(source.with_property(property).is_err(), "property={property:?}");
    }
    for range in [
        (0.0, 0.0),
        (2.0, 1.0),
        (f64::NAN, 1.0),
        (0.0, f64::NAN),
        (f64::NEG_INFINITY, 1.0),
        (0.0, f64::INFINITY),
    ] {
        assert!(source.restyled(ColorMapPreset::Gray, range).is_err(), "range={range:?}");
    }
    assert_eq!(source.range(), Some((2.0, 6.0)));
}

fn css(color: inf::Color) -> String {
    format!("rgb({},{},{})", color.r, color.g, color.b)
}

#[test]
fn every_geometry_uses_guarded_palette_stops_with_independent_opacity() -> Result<()> {
    let source = source(&[json!({"value": -0.5}), json!({"value": 0.75})]);
    for preset in [ColorMapPreset::Terrain, ColorMapPreset::Rainbow, ColorMapPreset::Gray, ColorMapPreset::Blues] {
        let source = source.restyled(preset, (-0.5, 0.75))?;
        let palette = ProcessedColorMap::<256>::create_for_preset(preset, ColorMapDirection::Regular);
        for layer in source.layers_json(0.375, "rgb(12,34,56)") {
            let kind = layer["type"].as_str().unwrap();
            let expression = &layer["paint"][format!("{kind}-color")];
            assert_eq!(expression[0], "case");
            assert_eq!(expression[1], json!(["==", ["typeof", ["get", "value"]], "number"]));
            assert_eq!(expression[3], "rgb(12,34,56)");
            let interpolation = expression[2].as_array().unwrap();
            assert_eq!(interpolation[0], "interpolate");
            assert_eq!(interpolation[1], json!(["linear"]));
            assert_eq!(interpolation.len(), 3 + 256 * 2);
            for ix in 0..256 {
                assert_eq!(interpolation[3 + ix * 2], json!(ix as f64 / 255.0));
                assert_eq!(interpolation[4 + ix * 2], css(palette.get_color_by_value(ix as u8)));
            }
            assert_eq!(layer["paint"][format!("{kind}-opacity")], 0.375);
            maplibre_native::AnyLayer::from_json_value(&layer)?;
        }
    }
    Ok(())
}

// Evaluate just the arithmetic emitted by production, independently of the
// MapLibre parser tests, to check overflow, clamping and endpoint semantics.
fn numeric_expression(expression: &Value, property: f64) -> f64 {
    if let Some(value) = expression.as_f64() {
        return value;
    }
    let args = expression.as_array().unwrap();
    let lhs = || numeric_expression(&args[1], property);
    let rhs = || numeric_expression(&args[2], property);
    match args[0].as_str().unwrap() {
        "get" => property,
        "number" => lhs(),
        "min" => lhs().min(rhs()),
        "max" => lhs().max(rhs()),
        "-" => lhs() - rhs(),
        "/" => lhs() / rhs(),
        operator => panic!("Unexpected arithmetic operator {operator}"),
    }
}

#[test]
fn normalized_interpolation_handles_tiny_adjacent_and_overflowing_ranges() -> Result<()> {
    let source = source(&[json!({"value": 0}), json!({"value": 1})]);
    for (min, max) in [
        (-0.5, 0.75),
        (0.0, f64::from_bits(1)),
        (1.0, 1.0 + f64::EPSILON),
        (1e300, f64::from_bits(1e300_f64.to_bits() + 1)),
        (-f64::MAX, f64::MAX),
        (-f64::MAX, 0.0),
        (0.0, f64::MAX),
    ] {
        let styled = source.restyled(ColorMapPreset::Rainbow, (min, max))?;
        let expression = styled.color_expression("red");
        let normalized = &expression[2][2];
        assert_eq!(numeric_expression(normalized, min), 0.0, "range={min}..{max}");
        assert_eq!(numeric_expression(normalized, max), 1.0, "range={min}..{max}");
        assert_eq!(numeric_expression(normalized, -f64::MAX), 0.0);
        assert_eq!(numeric_expression(normalized, f64::MAX), 1.0);
        for layer in styled.layers_json(1.0, "red") {
            maplibre_native::AnyLayer::from_json_value(&layer)?;
        }
    }
    Ok(())
}

#[test]
fn numeric_guard_excludes_missing_null_booleans_strings_and_nested_values() {
    let source = source(&[json!({"value": 0}), json!({"value": 1})]);
    let expression = source.color_expression("rgb(12,34,56)");
    assert_eq!(expression[1], json!(["==", ["typeof", ["get", "value"]], "number"]));
    assert_eq!(expression[3], "rgb(12,34,56)");
    assert_eq!(expression[2][2][1][1][2][2], json!(["number", ["get", "value"]]));
    for properties in [
        json!({}),
        json!({"value": null}),
        json!({"value": true}),
        json!({"value": "0.5"}),
        json!({"value": [1]}),
        json!({"value": {"n": 1}}),
    ] {
        assert!(!properties["value"].is_number());
    }
}

#[test]
fn opacity_is_clamped_without_altering_color_expression() {
    let source = source(&[json!({"value": 0}), json!({"value": 1})]);
    let expected = source.color_expression("red");
    for (opacity, expected_opacity) in [(-0.5, 0.0), (1.5, 1.0), (f32::NAN, 1.0), (f32::INFINITY, 1.0)] {
        for layer in source.layers_json(opacity, "red") {
            let kind = layer["type"].as_str().unwrap();
            assert_eq!(layer["paint"][format!("{kind}-opacity")], expected_opacity);
            assert_eq!(layer["paint"][format!("{kind}-color")], expected);
        }
    }
}

#[test]
fn schema_metadata_includes_only_scalar_numeric_types_without_features() -> Result<()> {
    use gdal::vector::{LayerOptions, OGRFieldType};

    let mut dataset = gdal::DriverManager::get_driver_by_name("Memory")?.create_vector_only("")?;
    let layer = dataset.create_layer(LayerOptions::default())?;
    layer.create_defn_fields(&[
        ("integer", OGRFieldType::OFTInteger),
        ("wide", OGRFieldType::OFTInteger64),
        ("fraction", OGRFieldType::OFTReal),
        ("string", OGRFieldType::OFTString),
        ("list", OGRFieldType::OFTRealList),
        ("date", OGRFieldType::OFTDate),
    ])?;
    assert_eq!(
        schema_numeric_properties(layer.defn())
            .iter()
            .map(|property| (property.name(), property.range()))
            .collect::<Vec<_>>(),
        [("integer", (0.0, 1.0)), ("wide", (0.0, 1.0)), ("fraction", (0.0, 1.0))]
    );
    Ok(())
}

#[test]
fn schema_metadata_excludes_gdal_boolean_subtypes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("boolean.geojson");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"type": "FeatureCollection", "features": features(&[json!({"active": true, "value": 2.5})])}))?,
    )?;
    let dataset = gdalio::dataset::open_read_only(&path)?;
    let layer = dataset.layer(0)?;
    assert_eq!(
        schema_numeric_properties(layer.defn()).iter().map(NumericProperty::name).collect::<Vec<_>>(),
        ["value"]
    );
    Ok(())
}

fn write_tiled_fixture(path: &Path, pmtiles: bool) -> Result<()> {
    let metadata = json!({"vector_layers": [{"id": "values", "fields": {"value": "Number", "label": "String"}}]});
    if pmtiles {
        let metadata = serde_json::to_vec(&metadata)?;
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
        bytes.push(0); // No tiles to scan: the numeric schema is metadata-only.
        bytes.extend(metadata);
        std::fs::write(path, bytes)?;
    } else {
        let connection = sqlite::Connection::new(path, sqlite::AccessMode::Create)?;
        connection.execute("CREATE TABLE metadata (name TEXT, value TEXT)")?;
        connection.execute("CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB)")?;
        let mut insert = connection.prepare_statement("INSERT INTO tiles VALUES (0,0,0,?1) RETURNING zoom_level")?;
        insert.bind_blob(1, &[0x1a, 0x0e, 0x0a, 0x06, b'v', b'a', b'l', b'u', b'e', b's', 0x28, 0x80, 0x20, 0x78, 0x02])?;
        ensure!(insert.next().is_some(), "Could not insert empty fixture tile");
        drop(insert);
        let metadata = metadata.to_string();
        for (name, value) in [
            ("format", "pbf"),
            ("json", metadata.as_str()),
            ("minzoom", "0"),
            ("maxzoom", "0"),
            ("bounds", "-4,49,6,53"),
        ] {
            let statement = connection.prepare_statement("INSERT INTO metadata VALUES (?, ?)")?;
            statement.bind_text(1, name)?;
            statement.bind_text(2, value)?;
            statement.step();
        }
    }
    Ok(())
}

#[test]
fn tiled_archives_use_schema_ranges_and_remain_native_after_styling() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for (extension, pmtiles) in [("pmtiles", true), ("mbtiles", false)] {
        let path = directory.path().join(format!("values.{extension}"));
        write_tiled_fixture(&path, pmtiles)?;
        let source = VectorSource::open(&path)?.context("Expected tiled vector source")?.remove(0);
        assert!(matches!(source.data, VectorData::Tiled { .. }));
        assert!(!source.points_only(), "Archive schema does not declare a point geometry type");
        assert!(source.numeric_properties().iter().any(|property| property.name() == "value"));
        assert!(
            source
                .numeric_properties()
                .iter()
                .all(|property| property.range() == (0.0, 1.0) && property.name() != "label")
        );
        // GDAL may also expose the native numeric `mvt_id` feature identifier.
        let source = source.with_property("value")?;
        assert!(!source.points_only());
        assert_eq!(source.property(), Some("value"));
        assert_eq!(source.range(), Some((0.0, 1.0)));
        let styled = source.restyled(ColorMapPreset::Rainbow, (-2.5, 42.5))?;
        assert_eq!(styled.data_range(), Some((0.0, 1.0)));
        assert_eq!(styled.source_json(), source.source_json());
        assert_eq!(styled.source_json()["type"], "vector");
        assert!(!styled.points_only());
        assert_eq!(styled.id(), source.id());
        assert_ne!(styled.style_id(), source.style_id());
        for layer in styled.layers_json(0.5, "red") {
            assert_eq!(layer["source-layer"], "values");
            maplibre_native::AnyLayer::from_json_value(&layer)?;
        }
    }
    Ok(())
}
