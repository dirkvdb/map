//! Native API contract checks that do not require a graphics device.

use super::*;

fn with_validation_map(check: impl FnOnce(&MapHandle) -> Result<()>) -> Result<()> {
    let runtime = RuntimeHandle::create(&RuntimeOptions::default())?;
    let (map, created) = runtime.create_map(&MapOptions::new(LogicalExtent::new(32, 32, 1.0)))?;
    wait_native(created)?;
    let result = check(&map);
    wait_native(map.release().map_err(|(_, error)| error)?)?;
    wait_native(runtime.release().map_err(|(_, error)| error)?)?;
    result
}

pub(crate) fn validate_style(json: &str) -> Result<()> {
    with_validation_map(|map| {
        wait_native(map.set_style_json(json)?)?;
        Ok(())
    })
}

pub(crate) fn validate_layer(layer: &serde_json::Value) -> Result<()> {
    with_validation_map(|map| {
        wait_native(
            map.set_style_json(r#"{"version":8,"sources":{"local-vector":{"type":"geojson","data":{"type":"FeatureCollection","features":[]}}},"layers":[]}"#)?,
        )?;
        wait_native(map.add_style_layer_json(&layer.to_string(), "")?)?;
        Ok(())
    })
}

#[test]
fn viewport_extent_keeps_logical_pixels_and_separate_scale() {
    let viewport = Viewport {
        width: NonZeroU32::new(800).unwrap(),
        height: NonZeroU32::new(600).unwrap(),
        scale: 2.0,
    };
    assert_eq!(viewport.native_extent(), LogicalExtent::new(800, 600, 2.0));
}

#[test]
fn native_camera_roundtrip_preserves_center_zoom_pitch_and_bearing() -> Result<()> {
    with_validation_map(|map| {
        let mut camera = MapCamera::default();
        camera.tilt(-80.0);
        camera.rotate(74.0);
        let update = camera.update();
        let expected = update.camera;
        wait_native(map.update_camera(&update)?)?;
        let actual = wait_native(map.get_camera()?)?.camera;
        let center = expected.center.unwrap();
        assert!((actual.center.latitude - center.latitude).abs() < 1e-10);
        assert!((actual.center.longitude - center.longitude).abs() < 1e-10);
        assert_eq!(actual.zoom, expected.zoom.unwrap());
        assert_eq!(actual.pitch, expected.pitch.unwrap());
        assert_eq!(actual.bearing, expected.bearing.unwrap());
        Ok(())
    })
}

#[test]
fn native_readback_removes_row_padding_and_converts_rgba_to_gpui_bgra() -> Result<()> {
    let image = TextureReadbackResult {
        info: maplibre_native_ffi::TextureImageInfo {
            width: 1,
            height: 2,
            stride: 8,
            byte_length: 16,
        },
        data: vec![10, 20, 30, 40, 99, 99, 99, 99, 50, 60, 70, 80, 99, 99, 99, 99],
    };
    let pixels = native_pixels(&image)?;
    assert_eq!(pixels.dimensions(), (1, 2));
    assert_eq!(pixels.into_raw(), vec![30, 20, 10, 40, 70, 60, 50, 80]);
    Ok(())
}

#[test]
fn native_readback_rejects_truncated_rows_and_invalid_stride() {
    let mut image = TextureReadbackResult {
        info: maplibre_native_ffi::TextureImageInfo {
            width: 2,
            height: 2,
            stride: 8,
            byte_length: 16,
        },
        data: vec![0; 15],
    };
    assert!(native_pixels(&image).is_err());
    image.data.resize(16, 0);
    image.info.stride = 7;
    assert!(native_pixels(&image).is_err());
}

#[test]
fn poisoned_raster_mutex_recovery_preserves_owned_state() {
    let value = Mutex::new(42);
    let _ = std::panic::catch_unwind(|| {
        let _guard = value.lock().unwrap();
        panic!("exercise poisoned state recovery");
    });
    assert_eq!(*lock(&value), 42);
}
