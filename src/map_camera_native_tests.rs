use std::{
    num::NonZeroU32,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use super::{MapCamera, mercator_y};
use crate::{
    map_renderer::{BasemapStyle, MapRenderer, MapRequest, Viewport},
    raster_source::RasterSource,
};
use anyhow::{Result, ensure};

fn quadrant_source(path: &Path) -> Result<Arc<RasterSource>> {
    let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f32, _>(path, 512, 512, 1)?;
    let half_world = geo::constants::EARTH_CIRCUMFERENCE_M / 2.0;
    dataset.set_geo_transform(&[-half_world, half_world / 256.0, 0.0, half_world, 0.0, -half_world / 256.0])?;
    dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857)?)?;
    let values = (0..512 * 512)
        .map(|ix| 50.0 + 50.0 * (ix % 512 / 256) as f32 + 100.0 * (ix / 512 / 256) as f32)
        .collect();
    dataset
        .rasterband(1)?
        .write((0, 0), (512, 512), &mut gdal::raster::Buffer::new((512, 512), values))?;
    dataset.close()?;
    RasterSource::open(path)?.remove(0).restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))
}

fn settled_image(renderer: &MapRenderer) -> Result<Arc<gpui_kit::RenderImage>> {
    let frames = renderer.frames();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        ensure!(Instant::now() < deadline, "Native camera test timed out");
        if let Ok(frame) = frames.try_recv() {
            let image = frame.image?;
            if frame.settled {
                return Ok(image);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_quadrants(image: &gpui_kit::RenderImage, camera: MapCamera, source: &RasterSource, scale: u32) {
    let bytes = image.as_bytes(0).unwrap();
    let mut counts = [0; 4];
    for y in (20..600 * scale - 20).step_by(39) {
        for x in (20..800 * scale - 20).step_by(47) {
            let coordinate = camera.coordinate_at(((f64::from(x) + 0.5) / f64::from(scale) - 400.0, (f64::from(y) + 0.5) / f64::from(scale) - 300.0));
            let mx = (coordinate.longitude + 180.0) / 360.0;
            let my = mercator_y(coordinate.latitude);
            if my < 0.02 || my > 0.98 || (mx - 0.5).abs() < 0.02 || (my - 0.5).abs() < 0.02 {
                continue;
            }
            let quadrant = usize::from(mx >= 0.5) + 2 * usize::from(my >= 0.5);
            let expected = source.legend().color_for_value(50.0 + 50.0 * quadrant as f32, None).r;
            let actual = bytes[((y * 800 * scale + x) * 4) as usize];
            assert!(
                actual.abs_diff(expected) <= 2,
                "{camera:?} at ({x}, {y}), {coordinate:?}: {actual} != {expected}"
            );
            counts[quadrant] += 1;
        }
    }
    assert!(counts.into_iter().all(|count| count > 3), "Must exercise all four quadrants: {counts:?}");
}

#[test]
#[ignore = "requires a wgpu-compatible device; offline raster and local style only"]
fn native_pitched_rotated_pixels_agree_with_lookup_and_cover_far_ground() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = quadrant_source(&directory.path().join("quadrants.tif"))?;
    let style = r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##;
    let renderer = MapRenderer::with_test_style(style)?;
    for (pitch, bearing, scale) in [(0.0, 90.0, 1), (60.0, 37.0, 1), (60.0, 270.0, 2)] {
        let camera = MapCamera {
            x: 0.5,
            y: 0.5,
            zoom: 1.8,
            pitch,
            bearing,
            viewport_height: 600.0,
        };
        let viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: scale as f32,
        };
        renderer.render_raster(MapRequest { viewport, camera }, Some(source.clone()), 1.0, true, BasemapStyle::Dark, false)?;
        let image = settled_image(&renderer)?;
        assert_quadrants(&image, camera, &source, scale);
    }
    Ok(())
}
