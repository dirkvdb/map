use super::*;

fn open_csv(contents: &str) -> Result<Vec<Arc<VectorSource>>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("locations.csv");
    std::fs::write(&path, contents)?;
    VectorSource::open(&path)?.context("Expected CSV vector data")
}

#[test]
fn open_csv_creates_points_and_preserves_all_columns() -> Result<()> {
    let sources =
        open_csv("name,lat,lon,code,notes,value\n\"café, 東京\",51,4,00123,\"first line\nsecond line\",1.25\nSouth,-20,-10,00007,\"a \"\"quote\"\"\",\n")?;
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].name(), "locations");
    assert_eq!(sources[0].bounds(), [-10.0, -20.0, 4.0, 51.0]);
    let exported = sources[0].source_json();
    let features = exported["data"]["features"].as_array().unwrap();
    assert_eq!(features.len(), 2);
    assert_eq!(features[0]["geometry"], json!({"type": "Point", "coordinates": [4.0, 51.0]}));
    assert_eq!(
        features[0]["properties"],
        json!({
            "name": "café, 東京", "lat": 51.0, "lon": 4.0, "code": "00123",
            "notes": "first line\nsecond line", "value": "1.25"
        })
    );
    assert_eq!(features[1]["properties"]["notes"], "a \"quote\"");
    assert_eq!(features[1]["properties"]["value"], "");
    Ok(())
}

#[test]
fn open_csv_detects_coordinate_aliases_case_insensitively() -> Result<()> {
    for latitude in ["lat", "latitude", "LAT", "Latitude"] {
        for longitude in ["lon", "long", "longitude", "LON", "Longitude"] {
            let sources = open_csv(&format!("{longitude},{latitude},name\n4,51,test\n"))?;
            assert_eq!(sources[0].bounds(), [4.0, 51.0, 4.0, 51.0], "Headers: {longitude}, {latitude}");
        }
    }
    Ok(())
}

#[test]
fn open_csv_detects_semicolon_and_tab_delimiters() -> Result<()> {
    for delimiter in [';', '\t'] {
        let sources = open_csv(&format!("name{delimiter}latitude{delimiter}longitude\npoint{delimiter}51{delimiter}4\n"))?;
        assert_eq!(sources[0].bounds(), [4.0, 51.0, 4.0, 51.0]);
    }
    Ok(())
}

#[test]
fn open_csv_reports_missing_coordinate_headers() -> Result<()> {
    for contents in ["name,value\npoint,1\n", "name,lat\npoint,51\n", "name,lon\npoint,4\n"] {
        let error = open_csv(contents).unwrap_err();
        assert!(error.to_string().contains("requires latitude"), "{error:#}");
    }
    Ok(())
}

#[test]
fn open_csv_rejects_invalid_coordinates_instead_of_losing_records() {
    for (lon, lat) in [
        ("", "51"),
        ("4", ""),
        ("invalid", "51"),
        ("4oops", "51"),
        ("4", "NaN"),
        ("inf", "51"),
        ("181", "51"),
        ("4", "91"),
    ] {
        let error = open_csv(&format!("name,lon,lat\nvalid,4,51\ninvalid,{lon},{lat}\n")).unwrap_err();
        assert!(format!("{error:#}").contains("coordinates"), "{error:#}");
    }
}

#[test]
fn open_csv_rejects_conflicting_geometry_columns() {
    let error = open_csv("WKT,lon,lat\nPOINT (10 20),4,51\n").unwrap_err();
    assert!(error.to_string().contains("conflicting geometry"), "{error:#}");
}

#[test]
fn open_csv_reprojects_a_declared_crs_and_keeps_source_coordinate_properties() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("projected.csv");
    std::fs::write(&path, "lon,lat,name\n445277.96317309426,6621293.722740169,café\n")?;
    let srs = gdal::spatial_ref::SpatialRef::from_epsg(3857)?;
    std::fs::write(directory.path().join("projected.prj"), srs.to_wkt()?)?;
    let sources = VectorSource::open(&path)?.context("Expected projected CSV")?;
    let feature = &sources[0].source_json()["data"]["features"][0];
    assert!((feature["geometry"]["coordinates"][0].as_f64().unwrap() - 4.0).abs() < 1e-9);
    assert!((feature["geometry"]["coordinates"][1].as_f64().unwrap() - 51.0).abs() < 1e-9);
    assert_eq!(
        feature["properties"],
        json!({"lon": 445277.96317309426, "lat": 6621293.722740169, "name": "café"})
    );
    Ok(())
}

#[test]
fn open_csv_rejects_conflicting_csvt_coordinate_columns() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("conflicting.csv");
    std::fs::write(&path, "x,y,lon,lat\n10,20,4,51\n")?;
    std::fs::write(directory.path().join("conflicting.csvt"), "CoordX,CoordY,Real,Real\n")?;
    let error = VectorSource::open(&path).unwrap_err();
    assert!(error.to_string().contains("conflicting geometry"), "{error:#}");
    Ok(())
}

#[test]
fn open_csv_rejects_ambiguous_coordinate_headers() {
    let error = open_csv("lon,longitude,lat\n4,10,51\n").unwrap_err();
    assert!(error.to_string().contains("ambiguous"), "{error:#}");
}

#[test]
fn open_csv_rejects_invalid_declared_crs() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("invalid.csv");
    std::fs::write(&path, "lon,lat,name\n4,51,test\n")?;
    std::fs::write(directory.path().join("invalid.prj"), "not a valid CRS")?;
    let error = VectorSource::open(&path).unwrap_err();
    assert!(format!("{error:#}").contains("coordinate reference system"), "{error:#}");
    Ok(())
}

#[test]
fn open_csv_rejects_lossy_declared_attribute_conversions() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("invalid.csv");
    std::fs::write(&path, "lon,lat,count\n4,51,not a number\n")?;
    std::fs::write(directory.path().join("invalid.csvt"), "Real,Real,Integer64\n")?;
    assert!(VectorSource::open(&path).is_err(), "Invalid typed properties must not become null silently");
    Ok(())
}

#[test]
fn open_csv_with_only_headers_has_empty_finite_bounds() -> Result<()> {
    let sources = open_csv("name,longitude,latitude\n")?;
    assert_eq!(sources[0].bounds(), [0.0; 4]);
    assert_eq!(sources[0].source_json()["data"]["features"], json!([]));
    Ok(())
}

#[test]
fn open_csv_honors_declared_field_types_and_nulls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("typed.csv");
    std::fs::write(&path, "lon,lat,count,value,name\n4,51,9007199254740993,1.25,café\n5,52,,,other\n")?;
    std::fs::write(directory.path().join("typed.csvt"), "Real,Real,Integer64,Real,String\n")?;
    let sources = VectorSource::open(&path)?.context("Expected typed CSV vector data")?;
    let exported = sources[0].source_json();
    assert_eq!(
        exported["data"]["features"][0]["properties"],
        json!({
            "lon": 4.0, "lat": 51.0, "count": 9007199254740993_i64, "value": 1.25, "name": "café"
        })
    );
    let properties = exported["data"]["features"][1]["properties"].as_object().unwrap();
    assert_eq!(properties.get("count"), Some(&Value::Null));
    assert_eq!(properties.get("value"), Some(&Value::Null));
    Ok(())
}
