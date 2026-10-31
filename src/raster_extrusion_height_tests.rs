use anyhow::Result;

use super::*;
use crate::raster_tiles::{extrusion_view, test_support::parameters, tests::coverage_source};

fn orientations() -> impl Iterator<Item = (f64, f64)> {
    [0.0, 30.0, 60.0]
        .into_iter()
        .flat_map(|pitch| [0.0, 37.0, 90.0, 180.0, 270.0].into_iter().map(move |bearing| (pitch, bearing)))
}

// Assemble the native W row from a unit forward vector and a known eye scale.
// The synthetic eye sits 1.5 viewport heights along the pitched camera ray;
// its unwrapped Mercator Y can lie well beyond the center's latitude range.
fn native_parameters(zoom: f64, latitude: f64, pitch: f64, bearing: f64) -> (WgpuLayerParameters, f64, f64) {
    let mut p = parameters(zoom);
    p.latitude = latitude;
    p.width = 800.0;
    p.height = 600.0;
    let world = 512.0 * zoom.exp2();
    let (sin_pitch, cos_pitch) = pitch.to_radians().sin_cos();
    let (sin_bearing, cos_bearing) = bearing.to_radians().sin_cos();
    let center_y = (1.0 - latitude.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0;
    let eye_y = center_y + 1.5 * p.height * sin_pitch * cos_bearing / world;
    let eye_scale = world / EARTH_CIRCUMFERENCE * (std::f64::consts::PI * (1.0 - 2.0 * eye_y)).cosh();
    let ground_scale = world / (EARTH_CIRCUMFERENCE * latitude.to_radians().cos());
    p.projection[3] = sin_pitch * sin_bearing;
    p.projection[7] = -sin_pitch * cos_bearing;
    p.projection[11] = -eye_scale * cos_pitch;
    (p, eye_scale, ground_scale)
}

fn assert_relative(actual: f64, expected: f64) {
    assert!((actual - expected).abs() <= expected.abs() * 1e-12, "actual={actual}, expected={expected}");
}

#[test]
fn native_perspective_recovers_eye_scale_for_zoom_latitude_pitch_and_bearing() {
    for zoom in [0.0, 1.8, 5.0] {
        for latitude in [0.0, -60.0, 60.0] {
            for (pitch, bearing) in orientations() {
                let (p, eye_scale, _) = native_parameters(zoom, latitude, pitch, bearing);
                assert_relative(pixels_per_meter(&p), eye_scale);
            }
        }
    }
}

#[test]
fn native_input_meters_produce_the_same_nominal_world_height_at_every_angle() {
    for zoom in [0.0, 1.8, 5.0] {
        for latitude in [0.0, -60.0, 60.0] {
            for (pitch, bearing) in orientations() {
                let (p, eye_scale, _) = native_parameters(zoom, latitude, pitch, bearing);
                let nominal_height = 20.0;
                let input_meters = nominal_height / pixels_per_meter(&p);
                assert_relative(input_meters * eye_scale, nominal_height);
            }
        }
    }
}

#[test]
fn global_pitched_camera_does_not_use_the_thousands_times_smaller_center_scale() {
    let (p, eye_scale, ground_scale) = native_parameters(0.0, 0.0, 60.0, 0.0);
    assert!((7000.0..7300.0).contains(&(eye_scale / ground_scale)));
    assert_relative(pixels_per_meter(&p), eye_scale);
    assert_relative(20.0 / pixels_per_meter(&p) * eye_scale, 20.0);
    assert!(20.0 / ground_scale * eye_scale > 140_000.0, "The former center-scale conversion would explode");
}

#[test]
fn top_down_native_perspective_uses_wz_instead_of_the_orthographic_fallback() {
    let (mut p, _, ground_scale) = native_parameters(1.8, 60.0, 0.0, 37.0);
    assert_relative(pixels_per_meter(&p), ground_scale);
    p.projection[11] *= 7.0;
    assert_relative(pixels_per_meter(&p), ground_scale * 7.0);
    p.projection[11] = -p.projection[11];
    assert_relative(pixels_per_meter(&p), ground_scale * 7.0);
}

#[test]
fn only_zero_spatial_w_uses_center_scale_for_orthographic_synthetic_projections() {
    for zoom in [0.0, 1.8, 5.0] {
        for latitude in [0.0, -60.0, 60.0] {
            let mut p = parameters(zoom);
            p.latitude = latitude;
            let expected = 512.0 * zoom.exp2() / (EARTH_CIRCUMFERENCE * latitude.to_radians().cos());
            assert_relative(pixels_per_meter(&p), expected);
            p.projection = [0.0; 16];
            assert_relative(pixels_per_meter(&p), expected);
            p.projection[3] = 0.1;
            assert_eq!(pixels_per_meter(&p), 0.0, "A missing perspective Z scale must not use the center fallback");
        }
    }
}

#[test]
fn perspective_scale_recovery_needs_no_viewport_fov_or_aspect() {
    let (mut p, expected, _) = native_parameters(0.0, -60.0, 60.0, 37.0);
    p.width = f64::NAN;
    p.height = f64::INFINITY;
    assert_relative(pixels_per_meter(&p), expected);
}

#[test]
fn malformed_perspective_w_rows_fail_closed_without_center_scale_fallback() {
    let mut p = parameters(0.0);
    for [wx, wy, wz] in [
        [0.1, 0.2, 0.0],
        [1.0, 0.0, -1.0],
        [0.9, 0.9, -1.0],
        [1e200, 0.0, -1.0],
        [0.0, 0.0, f64::INFINITY],
        [f64::NAN, 0.0, -1.0],
        [0.0, f64::NAN, -1.0],
        [0.0, 0.0, f64::NAN],
    ] {
        p.projection[3] = wx;
        p.projection[7] = wy;
        p.projection[11] = wz;
        assert_eq!(pixels_per_meter(&p), 0.0);
    }
    let (p, _, _) = native_parameters(0.0, 0.0, 61.0, 37.0);
    assert_eq!(pixels_per_meter(&p), 0.0, "The native camera contract ends at 60 degrees");
}

#[test]
fn nonfinite_matrix_entries_and_invalid_orthographic_parameters_return_zero_scale() {
    for index in 0..16 {
        let (mut p, _, _) = native_parameters(0.0, 0.0, 60.0, 37.0);
        p.projection[index] = f64::NAN;
        assert_eq!(pixels_per_meter(&p), 0.0);
    }
    for latitude in [f64::NAN, f64::INFINITY, 90.0, -90.0] {
        let mut p = parameters(0.0);
        p.latitude = latitude;
        assert_eq!(pixels_per_meter(&p), 0.0);
    }
    for zoom in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 2000.0, -2000.0] {
        assert_eq!(pixels_per_meter(&parameters(zoom)), 0.0);
    }
}

#[test]
fn source_selected_zoom_controls_maximum_world_height_at_every_camera_angle() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("height.tif"), [-0.05, -0.05, 0.05, 0.05])?;
    for zoom in [0.0, 1.8, 5.0] {
        for latitude in [0.0, -60.0, 60.0] {
            for (pitch, bearing) in orientations() {
                let (p, eye_scale, _) = native_parameters(zoom, latitude, pitch, bearing);
                let pixels = f64::from(source.metadata().tile_size.unwrap_or(256).max(1));
                let nominal = 512.0 * (zoom - f64::from(selected_zoom(&p, &source))).exp2() / pixels * 2.0 * f64::from(EXTRUSION_HEIGHT_FACTOR);
                let meters = maximum(&p, &source, 2.0);
                assert_relative(meters * eye_scale, nominal);
                assert!((meters as f32).is_finite());
            }
        }
    }
    Ok(())
}

#[test]
fn maximum_is_unchanged_when_actual_coverage_uses_a_coarser_grid() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("coverage.tif"), [-0.05, -0.05, 0.05, 0.05])?;
    let p = parameters(5.0);
    let sources = [(&*source, 1.0)];
    let meters = maximum(&p, &source, 1.0);
    let detailed = extrusion_view::visible_tiles(&p, &sources, 256);
    let coarse = extrusion_view::visible_tiles(&p, &sources, 1);
    assert!(!detailed.is_empty());
    assert!(!coarse.is_empty());
    assert!(detailed.iter().all(|tile| tile.z == selected_zoom(&p, &source)));
    assert!(coarse.iter().all(|tile| tile.z < selected_zoom(&p, &source)));
    assert_eq!(extrusion_view::maximum_height(&p, &sources), meters);
    assert_relative(meters * pixels_per_meter(&p), 10.0);
    assert_eq!(maximum(&p, &source, 1.0), meters);
    Ok(())
}

#[test]
fn disabled_nonfinite_and_unrepresentable_heights_return_finite_zero() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("invalid.tif"), [-0.05, -0.05, 0.05, 0.05])?;
    let p = parameters(0.0);
    for height in [-1.0, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX] {
        assert_eq!(maximum(&p, &source, height), 0.0);
    }
    for zoom in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 2000.0, -2000.0] {
        assert_eq!(maximum(&parameters(zoom), &source, 1.0), 0.0);
    }
    let mut p = parameters(0.0);
    p.projection[11] = -f64::MIN_POSITIVE;
    assert_eq!(maximum(&p, &source, 1.0), 0.0, "Division by a tiny native scale must not produce infinity");
    p.projection[11] = -1e10;
    assert!((maximum(&p, &source, f32::MAX) as f32).is_finite());
    Ok(())
}
