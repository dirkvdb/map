use super::*;

use gdal::DriverManager;
use gdal::spatial_ref::SpatialRef;
use gdal::vector::{Feature, FieldDefn, Geometry, LayerOptions, OGRFieldType, OGRwkbGeometryType};

struct ProjectedPoint {
    projected: [f64; 2],
    longitude_latitude: [f64; 2],
    name: &'static str,
    count: i64,
    ratio: f64,
}

// Fixed coordinates keep the expected result independent of GDAL's transform API.
const MERCATOR_POINTS: [ProjectedPoint; 2] = [
    ProjectedPoint {
        projected: [445277.96317309426, 6621293.722740169],
        longitude_latitude: [4.0, 51.0],
        name: "café 東京",
        count: 9_007_199_254_740_993,
        ratio: 1.25,
    },
    ProjectedPoint {
        projected: [-445277.96317309426, -6621293.722740169],
        longitude_latitude: [-4.0, -51.0],
        name: "München — rivière",
        count: -1_234_567_890_123,
        ratio: -2.5,
    },
];

const UTM_POINTS: [ProjectedPoint; 1] = [ProjectedPoint {
    projected: [500_000.0, 0.0],
    longitude_latitude: [3.0, 0.0],
    name: "別の層",
    count: 4_294_967_297,
    ratio: 0.125,
}];

fn write_projected_points(dataset: &mut Dataset, name: &str, epsg: u32, options: &[&str], points: &[ProjectedPoint]) -> Result<()> {
    let srs = SpatialRef::from_epsg(epsg)?;
    let layer = dataset.create_layer(LayerOptions {
        name,
        srs: Some(&srs),
        ty: OGRwkbGeometryType::wkbPoint,
        options: Some(options),
    })?;
    add_projected_point_fields(&layer)?;
    let defn = layer.defn();
    let fields = [
        defn.field_index("name")?,
        defn.field_index("count")?,
        defn.field_index("ratio")?,
        defn.field_index("missing")?,
    ];
    for point in points {
        write_projected_point(&layer, fields, point)?;
    }
    Ok(())
}

fn add_projected_point_fields(layer: &gdal::vector::Layer<'_>) -> Result<()> {
    for (name, field_type, width, precision) in [
        ("name", OGRFieldType::OFTString, 80, 0),
        // DBF widths above 18 can be reopened as real rather than integer64.
        ("count", OGRFieldType::OFTInteger64, 18, 0),
        ("ratio", OGRFieldType::OFTReal, 18, 6),
        ("missing", OGRFieldType::OFTReal, 18, 6),
    ] {
        let field = FieldDefn::new(name, field_type)?;
        field.set_width(width);
        field.set_precision(precision);
        field.add_to_layer(layer)?;
    }
    Ok(())
}

fn write_projected_point(layer: &gdal::vector::Layer<'_>, fields: [usize; 4], point: &ProjectedPoint) -> Result<()> {
    let mut feature = Feature::new(layer.defn())?;
    let [x, y] = point.projected;
    feature.set_geometry(Geometry::from_wkt(&format!("POINT ({x} {y})"))?)?;
    feature.set_field_string(fields[0], point.name)?;
    feature.set_field_integer64(fields[1], point.count)?;
    feature.set_field_double(fields[2], point.ratio)?;
    feature.set_field_null(fields[3])?;
    feature.create(layer)?;
    Ok(())
}

fn assert_coordinate(actual: f64, expected: f64, context: &str) {
    assert!((actual - expected).abs() < 1e-8, "{context}: expected {expected}, got {actual}");
}

fn assert_projected_points(source: &VectorSource, points: &[ProjectedPoint]) {
    let exported = source.source_json();
    assert_eq!(exported["type"], "geojson");
    assert_eq!(exported["data"]["type"], "FeatureCollection");
    let features = exported["data"]["features"].as_array().expect("Expected GeoJSON features");
    assert_eq!(features.len(), points.len(), "Feature count for {}", source.name());

    let mut expected_bounds = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for point in points {
        let feature = features
            .iter()
            .find(|feature| feature["properties"]["name"] == point.name)
            .unwrap_or_else(|| panic!("Missing feature {:?} in {}", point.name, source.name()));
        assert_eq!(feature["type"], "Feature");
        assert_eq!(feature["geometry"]["type"], "Point");
        let coordinates = feature["geometry"]["coordinates"].as_array().expect("Expected point coordinates");
        assert_eq!(coordinates.len(), 2);
        for (actual, expected) in coordinates.iter().zip(point.longitude_latitude) {
            assert_coordinate(actual.as_f64().expect("Expected numeric coordinate"), expected, point.name);
        }
        let properties = feature["properties"].as_object().expect("Expected feature properties");
        // Indexing a Value returns null for absent keys, which would hide field loss.
        assert_eq!(properties.get("missing"), Some(&Value::Null), "Null field for {:?}", point.name);
        assert_eq!(
            feature["properties"],
            json!({"name": point.name, "count": point.count, "ratio": point.ratio, "missing": null}),
            "Attributes for {:?}",
            point.name
        );
        assert_eq!(properties["count"].as_i64(), Some(point.count));
        let [longitude, latitude] = point.longitude_latitude;
        expected_bounds[0] = expected_bounds[0].min(longitude);
        expected_bounds[1] = expected_bounds[1].min(latitude);
        expected_bounds[2] = expected_bounds[2].max(longitude);
        expected_bounds[3] = expected_bounds[3].max(latitude);
    }
    for (actual, expected) in source.bounds().into_iter().zip(expected_bounds) {
        assert_coordinate(actual, expected, source.name());
    }
}

fn assert_projected_layers(sources: &[Arc<VectorSource>]) {
    assert_eq!(sources.len(), 2);
    for (name, points) in [("mercator", MERCATOR_POINTS.as_slice()), ("utm", UTM_POINTS.as_slice())] {
        let source = sources
            .iter()
            .find(|source| source.name() == name)
            .unwrap_or_else(|| panic!("Missing layer {name:?}"));
        assert_projected_points(source, points);
    }
    assert_ne!(sources[0].id(), sources[1].id());
}

#[test]
fn open_projected_geopackage_preserves_every_layer_and_typed_attributes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("projected.gpkg");
    let mut dataset = DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&path)?;
    write_projected_points(&mut dataset, "mercator", 3857, &[], &MERCATOR_POINTS)?;
    write_projected_points(&mut dataset, "utm", 32631, &[], &UTM_POINTS)?;
    dataset.flush_cache()?;
    drop(dataset);

    let sources = VectorSource::open(&path)?.context("Expected projected GeoPackage layers")?;
    assert_projected_layers(&sources);
    Ok(())
}

#[test]
fn open_projected_shapefile_reprojects_and_preserves_utf8_and_typed_attributes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mercator.shp");
    let mut dataset = DriverManager::get_driver_by_name("ESRI Shapefile")?.create_vector_only(&path)?;
    // A .cpg sidecar makes Unicode deterministic without changing global GDAL config.
    write_projected_points(&mut dataset, "mercator", 3857, &["ENCODING=UTF-8"], &MERCATOR_POINTS)?;
    dataset.flush_cache()?;
    drop(dataset);

    let sources = VectorSource::open(&path)?.context("Expected a projected shapefile")?;
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].name(), "mercator");
    assert_projected_points(&sources[0], &MERCATOR_POINTS);
    Ok(())
}

#[test]
fn open_shapefile_directory_preserves_every_layer_with_its_own_projection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shapefiles");
    let mut dataset = DriverManager::get_driver_by_name("ESRI Shapefile")?.create_vector_only(&path)?;
    write_projected_points(&mut dataset, "mercator", 3857, &["ENCODING=UTF-8"], &MERCATOR_POINTS)?;
    write_projected_points(&mut dataset, "utm", 32631, &["ENCODING=UTF-8"], &UTM_POINTS)?;
    dataset.flush_cache()?;
    drop(dataset);

    let sources = VectorSource::open(&path)?.context("Expected layers from a shapefile directory")?;
    assert_projected_layers(&sources);
    Ok(())
}
