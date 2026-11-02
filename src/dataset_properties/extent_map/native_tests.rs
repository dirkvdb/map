//! Offline native-pixel checks: Rust-only projection tests cannot detect native camera clamping.

use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use serde_json::json;

use super::*;
use crate::map_renderer::{BasemapStyle, MapRenderer};

type Landmark = (gpui_kit::Point<f64>, u8);

fn landmarks(extent: Bounds<f64>) -> [Landmark; 4] {
    [(0.2, 0.2, 64), (0.8, 0.2, 128), (0.2, 0.8, 192), (0.8, 0.8, 255)]
        .map(|(x, y, gray)| (point(extent.left() + extent.size.width * x, extent.top() + extent.size.height * y), gray))
}

fn landmark_style(landmarks: &[Landmark]) -> String {
    let features: Vec<_> = landmarks
        .iter()
        .map(|(position, gray)| {
            let longitude = (position.x * 360.0).rem_euclid(360.0) - 180.0;
            let latitude = (std::f64::consts::PI * (1.0 - 2.0 * position.y)).sinh().atan().to_degrees();
            json!({
                "type": "Feature", "properties": {"color": format!("#{gray:02x}{gray:02x}{gray:02x}")},
                "geometry": {"type": "Point", "coordinates": [longitude, latitude]}
            })
        })
        .collect();
    json!({
        "version": 8,
        "sources": {"landmarks": {"type": "geojson", "data": {"type": "FeatureCollection", "features": features}}},
        "layers": [
            {"id": "background", "type": "background", "paint": {"background-color": "#000000"}},
            {"id": "landmarks", "type": "circle", "source": "landmarks",
             "paint": {"circle-radius": 4, "circle-color": ["get", "color"]}}
        ]
    })
    .to_string()
}

fn settled_image(renderer: &MapRenderer) -> Result<Arc<RenderImage>> {
    let frames = renderer.frames();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        ensure!(Instant::now() < deadline, "Extent preview rendering timed out");
        if let Ok(frame) = frames.try_recv() {
            let image = frame.image?;
            if frame.settled {
                return Ok(image);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_landmark_pixels(preview: &ExtentMap, image: &RenderImage, landmarks: &[Landmark]) {
    let rendered = image.size(0);
    let width = i32::from(rendered.width) as usize;
    let height = i32::from(rendered.height) as usize;
    let bytes = image.as_bytes(0).unwrap();
    for &(position, gray) in landmarks {
        let relative = relative_region(Bounds::new(position, size(0.0, 0.0)), preview.viewport);
        let x = (f64::from(relative.left()) * width as f64).round() as usize;
        let y = (f64::from(relative.top()) * height as f64).round() as usize;
        let found = (y.saturating_sub(2)..=(y + 2).min(height - 1))
            .any(|y| (x.saturating_sub(2)..=(x + 2).min(width - 1)).any(|x| bytes[(y * width + x) * 4..][..3].iter().all(|value| value.abs_diff(gray) <= 2)));
        assert!(found, "Native landmark {position:?} ({gray}) must align with the overlay at ({x}, {y})");
    }
}

#[test]
#[ignore = "requires a wgpu-compatible device; uses only inline GeoJSON and an offline style"]
fn native_basemap_landmarks_align_with_global_polar_and_dateline_extent_overlays() -> Result<()> {
    for bounds in [
        [-180.0, -85.140412, 180.0, 85.051129],
        [-180.0, 70.0, 180.0, 90.0],
        [-180.0, -90.0, 180.0, -70.0],
        [170.0, -10.0, -170.0, 10.0],
    ] {
        let preview = ExtentMap::new(bounds);
        let extent = preview.extent.unwrap();
        let region = relative_region(extent, preview.viewport);
        assert!(region.size.width.max(region.size.height) <= 0.8 + 1e-6);
        assert!(region.left() >= 0.1 - 1e-6 && region.right() <= 0.9 + 1e-6);
        assert!(region.top() >= 0.1 - 1e-6 && region.bottom() <= 0.9 + 1e-6);
        let landmarks = landmarks(extent);
        for scale in [1.0, 2.0] {
            let renderer = MapRenderer::with_test_style(&landmark_style(&landmarks))?.with_unconstrained_camera();
            renderer.render_layer(preview.basemap_request(scale).unwrap(), None, 1.0, true, BasemapStyle::Dark, false)?;
            let image = settled_image(&renderer)?;
            assert_landmark_pixels(&preview, &image, &landmarks);
        }
    }
    Ok(())
}
