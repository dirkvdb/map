use std::{
    num::NonZeroU32,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use super::{MapCamera, mercator_y};
use crate::{
    map_renderer::{BasemapStyle, MapRenderer, MapRequest, MapSource, Viewport},
    raster_playback::PlaybackRequest,
    raster_source::RasterSource,
};
use anyhow::{Result, ensure};

fn quadrant_source(path: &Path) -> Result<Arc<RasterSource>> {
    quadrant_sources(path, 1)?.remove(0).restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))
}

fn quadrant_sources(path: &Path, bands: usize) -> Result<Vec<Arc<RasterSource>>> {
    let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f32, _>(path, 512, 512, bands)?;
    let half_world = geo::constants::EARTH_CIRCUMFERENCE_M / 2.0;
    dataset.set_geo_transform(&[-half_world, half_world / 256.0, 0.0, half_world, 0.0, -half_world / 256.0])?;
    dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857)?)?;
    let values = (0..512 * 512)
        .map(|ix| 50.0 + 50.0 * (ix % 512 / 256) as f32 + 100.0 * (ix / 512 / 256) as f32)
        .collect();
    let mut buffer = gdal::raster::Buffer::new((512, 512), values);
    for band in 1..=bands {
        dataset.rasterband(band)?.write((0, 0), (512, 512), &mut buffer)?;
    }
    dataset.close()?;
    RasterSource::open(path)
}

fn isolated_bar_source(path: &Path) -> Result<Arc<RasterSource>> {
    // Reopen after replacing the quadrants so no source can cache the old pixels.
    drop(quadrant_source(path)?);
    let dataset = gdal::Dataset::open_ex(
        path,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
            ..Default::default()
        },
    )?;
    let mut band = dataset.rasterband(1)?;
    band.fill(-9999.0, None)?;
    band.set_no_data_value(Some(-9999.0))?;
    band.write((252, 252), (8, 8), &mut gdal::raster::Buffer::new((8, 8), vec![1.0_f32; 64]))?;
    dataset.close()?;
    RasterSource::open(path)?.remove(0).restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 1.0))
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

fn assert_half_opacity(images: &[Arc<gpui_kit::RenderImage>], scale: u32) {
    let bytes: Vec<_> = images.iter().map(|image| image.as_bytes(0).unwrap()).collect();
    let expected_len = (800 * 600 * scale * scale * 4) as usize;
    assert!(bytes.iter().all(|bytes| bytes.len() == expected_len));
    let mut foreground = 0;
    for (pixel, ((background, full), half)) in bytes[0]
        .as_chunks::<4>()
        .0
        .iter()
        .zip(bytes[1].as_chunks::<4>().0)
        .zip(bytes[2].as_chunks::<4>().0)
        .enumerate()
    {
        foreground += usize::from(background[..3].iter().zip(full).any(|(&b, &f)| b.abs_diff(f) > 16));
        for channel in 0..4 {
            let expected = (u16::from(background[channel]) + u16::from(full[channel])) as f32 / 2.0;
            assert!(
                (f32::from(half[channel]) - expected).abs() <= 2.0,
                "DPI {scale}, pixel ({}, {}), channel {channel}: B={background:?}, F={full:?}, H={half:?}, expected {expected}",
                pixel % (800 * scale) as usize,
                pixel / (800 * scale) as usize,
            );
        }
    }
    assert!(
        foreground > 10_000 * (scale * scale) as usize,
        "Insufficient visible foreground at DPI {scale}: {foreground} pixels"
    );
}

fn assert_native_extrusion_opacity(renderer: &MapRenderer, render: impl Fn(MapRequest, f32) -> Result<()>) -> Result<()> {
    for scale in [1, 2] {
        // Center the pitched, rotated view on the intersection of four canonical tiles.
        let camera = MapCamera {
            x: 0.5,
            y: 0.5,
            zoom: 1.8,
            pitch: 60.0,
            bearing: 37.0,
            viewport_height: 600.0,
        };
        let viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: scale as f32,
        };
        let mut images = Vec::new();
        for opacity in [0.0, 1.0, 0.5] {
            render(MapRequest { viewport, camera }, opacity)?;
            images.push(settled_image(renderer)?);
        }
        assert_half_opacity(&images, scale);
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible device; offline raster and local style only"]
fn native_multitile_extrusion_applies_global_opacity_once_with_pitch_rotation_and_hidpi() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = quadrant_source(&directory.path().join("opacity.tif"))?.with_extrusion(true, 1.0)?;
    let style = r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##;
    let renderer = MapRenderer::with_test_style(style)?;
    assert_native_extrusion_opacity(&renderer, |request, opacity| {
        renderer.render_raster(request, Some(source.clone()), opacity, true, BasemapStyle::Dark, false)
    })
}

#[test]
#[ignore = "requires a wgpu-compatible device; offline raster and local style only"]
fn native_multiband_multitile_extrusion_applies_global_opacity_once_with_pitch_rotation_and_hidpi() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let bands = quadrant_sources(&directory.path().join("playback-opacity.tif"), 2)?
        .iter()
        .map(|source| source.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0))?.with_extrusion(true, 1.0))
        .collect::<Result<Vec<_>>>()?;
    let anchor = bands[0].clone();
    // Paused playback keeps the same accepted frame for all three opacity samples.
    let playback = PlaybackRequest::new(bands, 1.0, 1, false, 1.0, true);
    let style = r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##;
    let renderer = MapRenderer::with_test_style(style)?;
    assert_native_extrusion_opacity(&renderer, |request, opacity| {
        renderer.render_layer(
            request,
            Some(MapSource::Multiband(anchor.clone(), playback.clone())),
            opacity,
            true,
            BasemapStyle::Dark,
            false,
        )
    })
}

fn central_bar_bounds(image: &gpui_kit::RenderImage, scale: u32) -> Option<[u32; 4]> {
    let bytes = image.as_bytes(0).unwrap();
    assert_eq!(bytes.len(), (800 * 512 * scale * scale * 4) as usize);
    let mut bounds = [430 * scale, 300 * scale, 0, 0];
    // Exclude wrapped world copies, but include the entire central top and walls.
    for y in 200 * scale..300 * scale {
        for x in 370 * scale..430 * scale {
            let offset = ((y * 800 * scale + x) * 4) as usize;
            if bytes[offset..offset + 3].iter().all(|channel| *channel > 64) {
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x);
                bounds[3] = bounds[3].max(y);
            }
        }
    }
    (bounds[0] <= bounds[2]).then_some(bounds)
}

fn assert_global_bar_extent(renderer: &MapRenderer, source: &Arc<RasterSource>, pitch: f64, bearing: f64, scale: u32) -> Result<()> {
    let camera = MapCamera {
        x: 0.5,
        y: 0.5,
        zoom: 0.0,
        pitch,
        bearing,
        viewport_height: 512.0,
    };
    // A 512-logical-pixel height lets native zoom constraints fit the world at zoom 0.
    let viewport = Viewport {
        width: NonZeroU32::new(800).unwrap(),
        height: NonZeroU32::new(512).unwrap(),
        scale: scale as f32,
    };
    renderer.render_raster(MapRequest { viewport, camera }, Some(source.clone()), 1.0, true, BasemapStyle::Dark, false)?;
    let image = settled_image(renderer)?;
    let bounds = central_bar_bounds(&image, scale);
    ensure!(bounds.is_some(), "Global bar disappeared: {camera:?}, DPI {scale}");
    let [left, top, right, bottom] = bounds.unwrap();
    let width = f64::from(right - left + 1) / f64::from(scale);
    let height = f64::from(bottom - top + 1) / f64::from(scale);
    eprintln!("Global bar pitch={pitch}, bearing={bearing}, DPI={scale}: {width} x {height} logical pixels, bounds={bounds:?}");
    // Foreshortening is legitimate; an eye-latitude scale error instead produces huge walls or clips the bar away.
    ensure!(
        width < 35.0 && height < 35.0,
        "Global bar is oversized: {camera:?}, DPI {scale}, {width} x {height}, bounds={bounds:?}"
    );
    ensure!(
        left > 370 * scale && right < 430 * scale - 1 && top > 200 * scale && bottom < 300 * scale - 1,
        "Global bar touches the measurement ROI: {camera:?}, DPI {scale}, bounds={bounds:?}"
    );
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible device; offline raster and local style only"]
fn native_global_extrusion_height_stays_bounded_across_pitch_bearing_and_hidpi() -> Result<()> {
    let directory = tempfile::tempdir()?;
    // Height 1 means ten nominal raster-pixel widths, not the subpixel culling-test height.
    let source = isolated_bar_source(&directory.path().join("global-bar.tif"))?.with_extrusion(true, 1.0)?;
    let style = r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##;
    let renderer = MapRenderer::with_test_style(style)?;
    for (pitch, scale) in [(0.0, 1), (30.0, 1), (60.0, 1), (60.0, 2)] {
        for bearing in [0.0, 37.0, 90.0, 180.0, 270.0] {
            assert_global_bar_extent(&renderer, &source, pitch, bearing, scale)?;
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible device; offline raster and local style only"]
fn native_extrusion_culling_preserves_tops_with_pitch_rotation_and_hidpi() -> Result<()> {
    let directory = tempfile::tempdir()?;
    // Keep wall coverage negligible for this exact pixel/lookup comparison,
    // while still exercising indexed extrusion, winding, depth and culling.
    // The global-height regression above tests visibly raised bars at height 1.
    let source = quadrant_source(&directory.path().join("extruded.tif"))?.with_extrusion(true, f32::EPSILON)?;
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
