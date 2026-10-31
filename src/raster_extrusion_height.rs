//! Convert nominal raster-pixel heights to the meter-valued Z input of MapLibre's projection.

use geo::constants::LATITUDE_MAX;
use maplibre_native::WgpuLayerParameters;

use super::selected_zoom;
use crate::raster_source::{EXTRUSION_HEIGHT_FACTOR, RasterSource};

const EARTH_CIRCUMFERENCE: f64 = std::f64::consts::TAU * 6_378_137.0;
const PITCH_EPSILON: f64 = 1e-12;

/// World pixels per input meter of the native, column-major projection.
///
/// Native perspective W has a unit-length forward XY component and a Z component
/// of `-pixels_per_meter * cos(pitch)`. Recovering that scale accounts for the
/// camera-eye latitude, which can differ greatly from `parameters.latitude`.
/// This assumes the application's native pitch range of 0–60 degrees and an
/// unrescaled native matrix; no viewport, FOV, aspect, or DPI factor is involved.
///
/// Only an exactly zero spatial W row uses the synthetic orthographic fallback:
/// world size divided by the Mercator circumference at the camera-center latitude.
/// Non-finite matrices, unsupported pitch, and invalid perspective scales return
/// zero, never a potentially enormous center-latitude perspective fallback.
pub fn pixels_per_meter(parameters: &WgpuLayerParameters) -> f64 {
    let m = &parameters.projection;
    if !m.iter().all(|value| value.is_finite()) {
        return 0.0;
    }
    if m[3] == 0.0 && m[7] == 0.0 && m[11] == 0.0 {
        return orthographic_scale(parameters);
    }
    let cos_pitch = (1.0 - m[3] * m[3] - m[7] * m[7]).sqrt();
    if !cos_pitch.is_finite() || cos_pitch < 0.5 - PITCH_EPSILON {
        return 0.0;
    }
    positive_finite(m[11].abs() / cos_pitch)
}

/// Maximum extrusion height in projection-input meters, shared by CPU and GPU.
///
/// The nominal pixel width is fixed by the source-selected zoom, before coverage
/// budgeting or cached-ancestor selection. Rendered tile zoom, texture grid size,
/// DPI, geometry LOD, and averaging must not change this height. The shader still
/// normalizes values and multiplies them by this maximum.
///
/// Disabled/non-finite styles, invalid parameters/scales, and results that cannot
/// be represented by a finite f32 GPU uniform return zero.
pub fn maximum(parameters: &WgpuLayerParameters, source: &RasterSource, height: f32) -> f64 {
    if !height.is_finite() || height <= 0.0 || !parameters.zoom.is_finite() || !parameters.latitude.is_finite() || parameters.latitude.abs() > LATITUDE_MAX {
        return 0.0;
    }
    let scale = pixels_per_meter(parameters);
    if scale <= 0.0 {
        return 0.0;
    }
    let pixels = f64::from(source.metadata().tile_size.unwrap_or(256).max(1));
    let pixel_width = 512.0 * (parameters.zoom - f64::from(selected_zoom(parameters, source))).exp2() / pixels;
    let meters = positive_finite(pixel_width * f64::from(height) * f64::from(EXTRUSION_HEIGHT_FACTOR) / scale);
    if meters <= f64::from(f32::MAX) { meters } else { 0.0 }
}

fn orthographic_scale(parameters: &WgpuLayerParameters) -> f64 {
    if !parameters.latitude.is_finite() || parameters.latitude.abs() > LATITUDE_MAX {
        return 0.0;
    }
    let circumference = EARTH_CIRCUMFERENCE * parameters.latitude.to_radians().cos();
    positive_finite(512.0 * parameters.zoom.exp2() / circumference)
}

fn positive_finite(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 { value } else { 0.0 }
}

#[cfg(test)]
#[path = "raster_extrusion_height_tests.rs"]
mod tests;
