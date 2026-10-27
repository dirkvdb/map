use super::*;

use gdal::vector::{Geometry, LayerOptions, OGRwkbGeometryType, geometry_type_set_modifier};

fn features(geometries: &[Value]) -> Vec<Value> {
    geometries
        .iter()
        .map(|geometry| json!({"type": "Feature", "geometry": geometry, "properties": {"value": 2}}))
        .collect()
}

#[test]
fn materialized_points_only_accepts_points_multipoints_and_their_combination() {
    let point = json!({"type": "Point", "coordinates": [4, 51]});
    let point_z = json!({"type": "Point", "coordinates": [4, 51, 7]});
    let multipoint = json!({"type": "MultiPoint", "coordinates": [[4, 51], [5, 52]]});
    let multipoint_z = json!({"type": "MultiPoint", "coordinates": [[4, 51, 7], [5, 52, 8]]});
    for geometries in [
        vec![point.clone()],
        vec![point_z.clone()],
        vec![multipoint.clone()],
        vec![multipoint_z.clone()],
        vec![point, point_z, multipoint, multipoint_z],
    ] {
        assert!(geojson_points_only(&features(&geometries)), "geometries={geometries:?}");
    }
}

#[test]
fn materialized_points_only_rejects_nonpoints_unknown_and_collections() {
    let point = json!({"type": "Point", "coordinates": [4, 51]});
    for geometry in [
        json!({"type": "LineString", "coordinates": [[4, 51], [5, 52]]}),
        json!({"type": "Polygon", "coordinates": [[[4, 51], [5, 51], [5, 52], [4, 51]]]}),
        json!({"type": "MultiLineString", "coordinates": [[[4, 51], [5, 52]]]}),
        json!({"type": "MultiPolygon", "coordinates": [[[[4, 51], [5, 51], [5, 52], [4, 51]]]]}),
        json!({"type": "GeometryCollection", "geometries": [point]}),
        json!({"type": "Unknown", "coordinates": [4, 51]}),
        Value::Null,
    ] {
        assert!(!geojson_points_only(&features(std::slice::from_ref(&geometry))), "geometry={geometry}");
        assert!(
            !geojson_points_only(&features(&[point.clone(), geometry.clone()])),
            "mixed point and geometry={geometry}"
        );
    }
}

#[test]
fn materialized_points_only_rejects_empty_and_malformed_geometries() {
    assert!(!geojson_points_only(&[]));
    for geometry in [
        json!({}),
        json!({"type": "Point"}),
        json!({"type": "Point", "coordinates": null}),
        json!({"type": "Point", "coordinates": []}),
        json!({"type": "Point", "coordinates": [4]}),
        json!({"type": "Point", "coordinates": [4, "51"]}),
        json!({"type": "Point", "coordinates": [4, null]}),
        json!({"type": "Point", "coordinates": [4, true]}),
        json!({"type": "Point", "coordinates": [4, 51, "height"]}),
        json!({"type": "Point", "coordinates": [[4, 51]]}),
        json!({"type": "MultiPoint"}),
        json!({"type": "MultiPoint", "coordinates": []}),
        json!({"type": "MultiPoint", "coordinates": [4, 51]}),
        json!({"type": "MultiPoint", "coordinates": [[4, 51], []]}),
        json!({"type": "MultiPoint", "coordinates": [[4, 51], [5]]}),
        json!({"type": "MultiPoint", "coordinates": [[4, 51], [5, null]]}),
    ] {
        assert!(!geojson_points_only(&features(std::slice::from_ref(&geometry))), "geometry={geometry}");
    }
    assert!(!geojson_points_only(&[json!({"type": "Feature", "properties": {}})]));
}

#[test]
fn open_retains_materialized_geometry_classification() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let point = json!({"type": "Point", "coordinates": [4, 51]});
    let multipoint = json!({"type": "MultiPoint", "coordinates": [[4, 51], [5, 52]]});
    let line = json!({"type": "LineString", "coordinates": [[4, 51], [5, 52]]});
    let polygon = json!({"type": "Polygon", "coordinates": [[[4, 51], [5, 51], [5, 52], [4, 51]]]});
    for (name, geometries, expected) in [
        ("point", vec![point.clone()], true),
        ("multipoint", vec![multipoint.clone()], true),
        ("point_and_multipoint", vec![point.clone(), multipoint], true),
        ("line", vec![line.clone()], false),
        ("polygon", vec![polygon.clone()], false),
        ("mixed_line", vec![point.clone(), line], false),
        ("mixed_polygon", vec![point.clone(), polygon], false),
        ("null", vec![Value::Null], false),
        ("point_and_null", vec![point, Value::Null], false),
        ("empty", vec![], false),
    ] {
        let path = directory.path().join(format!("{name}.geojson"));
        let data = json!({"type": "FeatureCollection", "features": features(&geometries)});
        std::fs::write(&path, serde_json::to_vec(&data)?)?;
        let source = VectorSource::open(&path)?.context("Expected materialized vector source")?.remove(0);
        std::fs::remove_file(path)?;
        assert_eq!(source.points_only(), expected, "dataset={name}");
    }
    Ok(())
}

#[test]
fn tiled_schema_points_only_flattens_z_m_and_rejects_unknown_or_nonpoints_without_features() -> Result<()> {
    for (base, expected) in [
        (OGRwkbGeometryType::wkbPoint, true),
        (OGRwkbGeometryType::wkbMultiPoint, true),
        (OGRwkbGeometryType::wkbUnknown, false),
        (OGRwkbGeometryType::wkbNone, false),
        (OGRwkbGeometryType::wkbLineString, false),
        (OGRwkbGeometryType::wkbMultiLineString, false),
        (OGRwkbGeometryType::wkbPolygon, false),
        (OGRwkbGeometryType::wkbMultiPolygon, false),
        (OGRwkbGeometryType::wkbGeometryCollection, false),
    ] {
        for (z, m) in [(false, false), (true, false), (false, true), (true, true)] {
            let ty = geometry_type_set_modifier(base, z, m);
            let mut dataset = gdal::DriverManager::get_driver_by_name("Memory")?.create_vector_only("")?;
            let layer = dataset.create_layer(LayerOptions { ty, ..Default::default() })?;
            assert_eq!(schema_points_only(layer.defn()), expected, "geometry type={ty}, Z={z}, M={m}");
        }
    }
    Ok(())
}

#[test]
fn tiled_schema_points_only_accepts_legacy_25d_point_types() -> Result<()> {
    for ty in [OGRwkbGeometryType::wkbPoint25D, OGRwkbGeometryType::wkbMultiPoint25D] {
        let mut dataset = gdal::DriverManager::get_driver_by_name("Memory")?.create_vector_only("")?;
        let layer = dataset.create_layer(LayerOptions { ty, ..Default::default() })?;
        assert!(schema_points_only(layer.defn()), "geometry type={ty}");
    }
    Ok(())
}

#[test]
fn tiled_schema_points_only_keeps_unknown_false_for_point_and_mixed_contents() -> Result<()> {
    let mut dataset = gdal::DriverManager::get_driver_by_name("Memory")?.create_vector_only("")?;
    let mut layer = dataset.create_layer(LayerOptions {
        ty: OGRwkbGeometryType::wkbUnknown,
        ..Default::default()
    })?;
    layer.create_feature(Geometry::from_wkt("POINT (4 51)")?)?;
    assert!(!schema_points_only(layer.defn()));
    layer.create_feature(Geometry::from_wkt("LINESTRING (4 51, 5 52)")?)?;
    assert!(!schema_points_only(layer.defn()));
    Ok(())
}

#[test]
fn points_only_is_retained_by_clone_property_selection_and_restyling() -> Result<()> {
    for points_only in [true, false] {
        for data in [
            VectorData::GeoJson(Arc::new("{\"type\":\"FeatureCollection\",\"features\":[]}".to_owned())),
            VectorData::Tiled {
                url: "pmtiles:///metadata-only.pmtiles".to_owned(),
            },
        ] {
            let source = VectorSource::new(
                "retained".to_owned(),
                [0.0; 4],
                data,
                vec![
                    NumericProperty {
                        name: "a".to_owned(),
                        range: (0.0, 1.0),
                    },
                    NumericProperty {
                        name: "b".to_owned(),
                        range: (2.0, 3.0),
                    },
                ],
                points_only,
            );
            let cloned = source.clone();
            let selected = source.with_property("b")?;
            let styled = selected.restyled(ColorMapPreset::Rainbow, (-1.0, 10.0))?;
            for snapshot in [&source, &cloned, &*selected, &*styled] {
                assert_eq!(snapshot.points_only(), points_only);
            }
        }
    }
    Ok(())
}
