//! Offline native GPU regressions: native GeoJSON checks projection, flat rasters check bar coverage.
//!
//! Run explicitly with `cargo test globe_tests -- --ignored --test-threads=1`.
//! No styles, glyphs, sprites, terrain, or tile URLs are fetched by these tests.

use gdal::raster::Buffer;
use inf::colormap::ColorMapPreset;

use super::*;

const BROAD_BOUNDS: [f64; 4] = [-55.0, -35.0, 55.0, 45.0];
const EDGE_TOLERANCE: u32 = 8;

struct Fixture {
    _directory: tempfile::TempDir,
    bands: Vec<Arc<RasterSource>>,
    bounds: [f64; 4],
}

impl Fixture {
    fn new(bounds: [f64; 4], band_count: usize) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("globe.tif");
        write_fixture(&path, bounds, band_count)?;
        let bands = RasterSource::open(&path)?
            .iter()
            .map(|band| band.restyled(ColorMapPreset::Gray, (0.0, 255.0)))
            .collect::<Result<Vec<_>>>()?;
        ensure!(bands.len() == band_count, "Fixture must expose every numeric band");
        ensure!(bands.iter().all(|band| band.is_numeric()), "Must exercise custom numeric rendering");
        Ok(Self {
            _directory: directory,
            bands,
            bounds,
        })
    }

    fn maps(&self, request: MapRequest) -> Result<(NativeMap, NativeMap)> {
        let numeric = NativeMap::with_style(request.viewport, Some(tests::DARK_STYLE))?;
        let reference = NativeMap::with_style(request.viewport, Some(&reference_style(self.bounds)))?;
        Ok((numeric, reference))
    }

    fn gray(&self, value: f64) -> u8 {
        self.bands[0].legend().color_for_value(value, None).r
    }
}

fn write_fixture(path: &std::path::Path, bounds: [f64; 4], band_count: usize) -> Result<()> {
    crate::raster_source::test_support::write_raster(path, 4326, band_count)?;
    let mut dataset = gdal::Dataset::open_ex(
        path,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
            ..Default::default()
        },
    )?;
    let [west, south, east, north] = bounds;
    dataset.set_geo_transform(&[west, (east - west) / 64.0, 0.0, north, 0.0, (south - north) / 64.0])?;
    for band in 1..=band_count {
        dataset.rasterband(band)?.fill(128.0 + 96.0 * (band - 1) as f64, None)?;
    }
    dataset.close()?;
    Ok(())
}

fn request(longitude: f64, latitude: f64, zoom: f64, pitch: f64, bearing: f64) -> MapRequest {
    let viewport = Viewport {
        width: NonZeroU32::new(512).unwrap(),
        height: NonZeroU32::new(512).unwrap(),
        scale: 1.0,
    };
    // Point bounds give a known zoom of 18 and an exact geographic center.
    let mut camera = MapCamera::fit_bounds([longitude, latitude, longitude, latitude], 512.0, 512.0);
    camera.zoom_at(zoom - 18.0, (0.0, 0.0));
    camera.tilt(-2.0 * pitch);
    camera.rotate(2.0 * bearing);
    MapRequest { viewport, camera }
}

fn reference_style(bounds: [f64; 4]) -> String {
    let [west, south, east, north] = bounds;
    // Small native quads approximate curved parallels without relying on one
    // enormous polygon's triangulation. Shared edges have antialiasing disabled.
    let mut polygons = Vec::new();
    for row in 0..16 {
        for column in 0..16 {
            let x = west + (east - west) * f64::from(column) / 16.0;
            let y = south + (north - south) * f64::from(row) / 16.0;
            let right = west + (east - west) * f64::from(column + 1) / 16.0;
            let top = south + (north - south) * f64::from(row + 1) / 16.0;
            polygons.push(vec![vec![[x, y], [right, y], [right, top], [x, top], [x, y]]]);
        }
    }
    serde_json::json!({
        "version": 8,
        "sources": {"footprint": {"type": "geojson", "tolerance": 0, "data": {
            "type": "Feature", "properties": {},
            "geometry": {"type": "MultiPolygon", "coordinates": polygons}
        }}},
        "layers": [
            {"id": "background", "type": "background", "paint": {"background-color": "#202020"}},
            {"id": "footprint", "type": "fill", "source": "footprint",
                "paint": {"fill-color": "#ffffff", "fill-antialias": false}}
        ]
    })
    .to_string()
}

fn foreground(pixel: &image::Rgba<u8>) -> bool {
    pixel.0[..3].iter().all(|channel| *channel > 80)
}

fn uniform_neighborhood(image: &RgbaImage, x: u32, y: u32, inside: bool) -> bool {
    (y - EDGE_TOLERANCE..=y + EDGE_TOLERANCE)
        .all(|row| (x - EDGE_TOLERANCE..=x + EDGE_TOLERANCE).all(|column| foreground(image.get_pixel(column, row)) == inside))
}

struct Samples {
    inside: Vec<(u32, u32)>,
    outside: Vec<(u32, u32)>,
    reference: RgbaImage,
}

impl Samples {
    fn from_reference(image: &RgbaImage) -> Self {
        let mut samples = Self {
            inside: Vec::new(),
            outside: Vec::new(),
            reference: image.clone(),
        };
        let coordinates = (EDGE_TOLERANCE..image.height() - EDGE_TOLERANCE)
            .step_by(7)
            .flat_map(|y| (EDGE_TOLERANCE..image.width() - EDGE_TOLERANCE).step_by(7).map(move |x| (x, y)));
        for (x, y) in coordinates {
            let inside = foreground(image.get_pixel(x, y));
            if !uniform_neighborhood(image, x, y, inside) {
                continue;
            }
            if inside {
                samples.inside.push((x, y));
            } else {
                samples.outside.push((x, y));
            }
        }
        assert!(samples.inside.len() > 80, "Native geometry must have substantial visible interior");
        assert!(samples.outside.len() > 80, "Native geometry must leave substantial visible exterior");
        samples
    }

    fn assert_aligned(&self, image: &RgbaImage, gray: u8, context: &str) {
        assert_sample_colors(image, &self.inside, gray, context);
        let mismatches = self
            .outside
            .iter()
            .filter(|&&(x, y)| {
                let expected = self.reference.get_pixel(x, y).0;
                image
                    .get_pixel(x, y)
                    .0
                    .into_iter()
                    .zip(expected)
                    .any(|(actual, expected)| actual.abs_diff(expected) > 4)
            })
            .count();
        assert!(
            mismatches * 100 <= self.outside.len(),
            "{context}: {mismatches} exterior samples differ from native background"
        );
    }
}

fn assert_sample_colors(image: &RgbaImage, samples: &[(u32, u32)], gray: u8, context: &str) {
    let mismatches: Vec<_> = samples
        .iter()
        .filter(|&&(x, y)| image.get_pixel(x, y).0[..3].iter().any(|channel| channel.abs_diff(gray) > 4))
        .collect();
    // Allow at most 1% residual tessellation/raster-resampling disagreement,
    // independently for interiors and exteriors, outside the eight-pixel band.
    assert!(
        mismatches.len() * 100 <= samples.len(),
        "{context}: {} / {} samples differ from gray {gray}; first coordinates: {:?}",
        mismatches.len(),
        samples.len(),
        mismatches
            .iter()
            .take(8)
            .map(|&&(x, y)| ((x, y), image.get_pixel(x, y).0))
            .collect::<Vec<_>>()
            .as_slice()
    );
}

fn assert_empty(image: &RgbaImage, context: &str) {
    let lit = image.pixels().filter(|pixel| foreground(pixel)).count();
    assert_eq!(lit, 0, "{context}: far hemisphere leaked {lit} foreground pixels");
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF and GeoJSON, no network"]
fn numeric_globe_matches_native_geometry_after_camera_rotation() -> Result<()> {
    let fixture = Fixture::new(BROAD_BOUNDS, 1)?;
    let (mut map, mut reference) = fixture.maps(request(0.0, 10.0, 1.0, 0.0, 0.0))?;
    map.set_globe(true);
    reference.set_globe(true);
    for view in [request(25.0, 20.0, 1.0, 25.0, 37.0), request(-30.0, 5.0, 1.3, 40.0, 123.0)] {
        let native = reference.render(view)?;
        let actual = map.render_raster(view, Some(fixture.bands[0].clone()), 1.0)?;
        Samples::from_reference(&native).assert_aligned(&actual, fixture.gray(128.0), &format!("{:?}", view.camera));
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF and GeoJSON, no network"]
fn numeric_globe_hides_far_hemisphere_and_recovers_when_camera_returns() -> Result<()> {
    let fixture = Fixture::new(BROAD_BOUNDS, 1)?;
    let near = request(0.0, 5.0, 1.0, 0.0, 37.0);
    let far = request(180.0, 0.0, 1.0, 0.0, 37.0);
    let (mut map, mut reference) = fixture.maps(near)?;
    map.set_globe(true);
    reference.set_globe(true);
    let samples = Samples::from_reference(&reference.render(near)?);
    let visible = map.render_raster(near, Some(fixture.bands[0].clone()), 1.0)?;
    samples.assert_aligned(&visible, fixture.gray(128.0), "near hemisphere before orbit");
    assert_empty(&reference.render(far)?, "native geometry");
    assert_empty(&map.render(far)?, "custom numeric surface");
    samples.assert_aligned(&map.render(near)?, fixture.gray(128.0), "near hemisphere after orbit");
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF and GeoJSON, no network"]
fn numeric_projection_toggle_roundtrip_preserves_native_alignment_and_camera() -> Result<()> {
    let fixture = Fixture::new(BROAD_BOUNDS, 1)?;
    let view = request(20.0, 15.0, 1.0, 30.0, 37.0);
    let (mut map, mut reference) = fixture.maps(view)?;
    map.set_raster(Some(fixture.bands[0].clone()), 1.0)?;
    let mercator = map.render(view)?;
    let mut first_globe = None;
    let updates = map.camera_updates;
    for globe in [false, true, false, true] {
        map.set_globe(globe);
        reference.set_globe(globe);
        let actual = map.render(view)?;
        Samples::from_reference(&reference.render(view)?).assert_aligned(&actual, fixture.gray(128.0), &format!("globe={globe}"));
        if !globe {
            assert_eq!(actual, mercator, "Toggling off must restore the numeric Mercator image");
        } else if let Some(first) = &first_globe {
            assert_eq!(&actual, first, "Second globe visit must not retain stale Mercator uniforms");
        } else {
            assert_ne!(actual, mercator, "Fixture must distinguish the two projections");
            first_globe = Some(actual);
        }
    }
    assert_eq!(map.camera_updates, updates, "Projection toggles must not move the native camera");
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF and GeoJSON, no network"]
fn numeric_globe_mercator_blend_at_zoom_11_5_matches_native_geometry() -> Result<()> {
    let fixture = Fixture::new([29.97, 59.985, 30.03, 60.015], 1)?;
    let view = request(30.0, 60.0, 11.5, 45.0, 37.0);
    let (mut map, mut reference) = fixture.maps(view)?;
    map.set_raster(Some(fixture.bands[0].clone()), 1.0)?;
    for globe in [false, true, false] {
        map.set_globe(globe);
        reference.set_globe(globe);
        let actual = map.render(view)?;
        Samples::from_reference(&reference.render(view)?).assert_aligned(&actual, fixture.gray(128.0), &format!("zoom=11.5 globe={globe}"));
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF and GeoJSON, no network"]
fn numeric_globe_blend_keeps_the_nearest_dateline_world_copy() -> Result<()> {
    let fixture = Fixture::new([-180.0, 59.985, -179.94, 60.015], 1)?;
    let view = request(179.99, 60.0, 11.5, 25.0, 0.0);
    let (mut map, mut reference) = fixture.maps(view)?;
    map.set_globe(true);
    reference.set_globe(true);
    let actual = map.render_raster(view, Some(fixture.bands[0].clone()), 1.0)?;
    Samples::from_reference(&reference.render(view)?).assert_aligned(&actual, fixture.gray(128.0), "dateline blend");
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF and GeoJSON, no network"]
fn numeric_globe_half_opacity_has_no_overlapping_tile_seams() -> Result<()> {
    let fixture = Fixture::new(BROAD_BOUNDS, 1)?;
    let view = request(0.0, 10.0, 1.0, 25.0, 37.0);
    let (mut map, mut reference) = fixture.maps(view)?;
    map.set_globe(true);
    reference.set_globe(true);
    let actual = map.render_raster(view, Some(fixture.bands[0].clone()), 0.5)?;
    let blended_gray = (f32::from(fixture.gray(128.0)) * 0.5 + 16.0).round() as u8;
    Samples::from_reference(&reference.render(view)?).assert_aligned(&actual, blended_gray, "half opacity");
    Ok(())
}

fn playback_image(map: &mut NativeMap, fixture: &Fixture, view: MapRequest, position: f64, revision: u64) -> Result<RgbaImage> {
    map.set_raster(Some(fixture.bands[0].clone()), 1.0)?;
    let style = crate::raster_playback::PlaybackStyle::default().with_range((0.0, 255.0))?;
    let playback = PlaybackRequest::new(fixture.bands.clone(), position, revision, false, 1.0, true).with_style(style);
    map.numeric.borrow_mut().set_playback(Some(playback), 1.0)?;
    let image = map.render(view)?;
    let frame = map.numeric.borrow().playback_frame().context("Multiband path must publish a playback frame")?;
    assert!(!frame.is_buffering(), "Pixel assertions require resident playback tiles");
    assert_eq!(frame.position(), position);
    Ok(image)
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local multiband GeoTIFF and GeoJSON, no network"]
fn multiband_globe_interpolation_stays_aligned_after_orbit_and_projection_toggle() -> Result<()> {
    let fixture = Fixture::new(BROAD_BOUNDS, 2)?;
    let near = request(25.0, 15.0, 1.0, 25.0, 37.0);
    let rotated = request(-25.0, 10.0, 1.3, 35.0, 123.0);
    let (mut map, mut reference) = fixture.maps(near)?;
    for (revision, position, view, globe) in [(1, 0.0, near, true), (2, 0.5, rotated, true), (3, 1.0, rotated, false), (4, 0.5, near, true)] {
        map.set_globe(globe);
        reference.set_globe(globe);
        let actual = playback_image(&mut map, &fixture, view, position, revision)?;
        let gray = fixture.gray(128.0 + 96.0 * position);
        Samples::from_reference(&reference.render(view)?).assert_aligned(&actual, gray, &format!("playback={position} globe={globe}"));
    }
    let far = request(180.0, 0.0, 1.0, 0.0, 37.0);
    assert_empty(&reference.render(far)?, "native geometry during playback");
    assert_empty(&map.render(far)?, "custom multiband surface");
    Ok(())
}

fn isolated_bar_fixture() -> Result<Fixture> {
    let mut fixture = Fixture::new(BROAD_BOUNDS, 1)?;
    let path = fixture._directory.path().join("globe.tif");
    // Do not open a tile source until after replacing the constant-valued pixels.
    fixture.bands.clear();
    let dataset = gdal::Dataset::open_ex(
        &path,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
            ..Default::default()
        },
    )?;
    let mut band = dataset.rasterband(1)?;
    band.fill(-9999.0, None)?;
    band.write((44, 28), (4, 4), &mut Buffer::new((4, 4), vec![224.0_f32; 16]))?;
    dataset.close()?;
    fixture.bands = RasterSource::open(&path)?
        .iter()
        .map(|source| source.restyled(ColorMapPreset::Gray, (0.0, 255.0)))
        .collect::<Result<Vec<_>>>()?;
    Ok(fixture)
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF only, no network"]
fn numeric_globe_radial_extrusion_is_visible_after_camera_motion_and_hides_far_side() -> Result<()> {
    let fixture = isolated_bar_fixture()?;
    let mut map = NativeMap::with_style(request(0.0, 5.0, 1.0, 0.0, 0.0).viewport, Some(tests::DARK_STYLE))?;
    map.set_globe(true);
    map.numeric.borrow_mut().set_reduce_motion(true);
    let flat = fixture.bands[0].clone();
    let raised = flat.with_extrusion(true, 1.0)?;
    for view in [request(0.0, 5.0, 1.3, 20.0, 37.0), request(10.0, 15.0, 1.5, 35.0, 123.0)] {
        let ground = map.render_raster(view, Some(flat.clone()), 1.0)?;
        let volume = map.render_raster(view, Some(raised.clone()), 1.0)?;
        assert_visible_extrusion(&ground, &volume);
    }
    assert_empty(&map.render(request(180.0, 0.0, 1.0, 0.0, 37.0))?, "far-side extruded bars");
    Ok(())
}

fn ground_fixture(band_count: usize, normalized_height: f32) -> Result<Fixture> {
    let mut fixture = Fixture::new([52.0, 22.0, 55.0, 25.0], band_count)?;
    // Playback interpolates the raw constants 128 and 224 before normalization.
    let value = if band_count > 1 { 176.0 } else { 128.0 };
    let minimum = value - 128.0 * normalized_height;
    fixture.bands = fixture
        .bands
        .iter()
        .map(|source| {
            source
                .restyled(ColorMapPreset::Gray, (minimum, minimum + 128.0))?
                .with_color_map_direction(inf::colormap::ColorMapDirection::Reversed)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(fixture)
}

fn ground_image(map: &mut NativeMap, fixture: &Fixture, view: MapRequest, extruded: bool, opacity: f32) -> Result<RgbaImage> {
    let source = fixture.bands[0].with_extrusion(extruded, 1.0)?;
    map.set_raster(Some(source), opacity)?;
    if fixture.bands.len() > 1 {
        // Override the automatic dataset range so the interpolated constant
        // really has the requested zero/low-positive normalized height.
        let style = crate::raster_playback::PlaybackStyle::default()
            .with_range(fixture.bands[0].range())?
            .with_extrusion(extruded, 1.0);
        let playback = PlaybackRequest::new(fixture.bands.clone(), 0.5, 1, false, 1.0, true).with_style(style);
        map.numeric.borrow_mut().set_playback(Some(playback), opacity)?;
    }
    let image = map.render(view)?;
    if fixture.bands.len() > 1 {
        let frame = map.numeric.borrow().playback_frame().context("Ground coverage requires a playback frame")?;
        assert!(!frame.is_buffering(), "Both constant bands must be resident before checking coverage");
        assert_eq!(frame.position(), 0.5);
    }
    Ok(image)
}

fn flat_ground_interior(image: &RgbaImage, x: u32, y: u32, gray: u8) -> bool {
    // Erode only the flat reference, never the tested image: an isolated hole
    // must not exclude itself or its neighbors from the coverage assertion.
    let radius = EDGE_TOLERANCE;
    [(x, y), (x - radius, y), (x + radius, y), (x, y - radius), (x, y + radius)]
        .into_iter()
        .all(|(x, y)| image.get_pixel(x, y).0[..3].iter().all(|channel| channel.abs_diff(gray) <= 1))
}

fn ground_coverage_failure(flat: &RgbaImage, volume: &RgbaImage, opacity: f32, normalized_height: f32) -> Option<String> {
    assert_eq!(flat.dimensions(), volume.dimensions());
    let expected = (255.0 * opacity + 32.0 * (1.0 - opacity)).round() as u8;
    let gray = flat.get_pixel(flat.width() / 2, flat.height() / 2)[0];
    assert!(
        gray.abs_diff(expected) <= 4,
        "Flat constant raster's center must be near white: {gray} vs {expected}"
    );
    // Positive bars have real walls, whose minimum illumination is 0.32.
    // Accept that shading, but never a background-colored interior hole.
    let illumination = if normalized_height > 0.0 { 0.32 } else { 1.0 };
    let background = 32.0 * (1.0 - opacity);
    let minimum = ((f32::from(gray) - background) * illumination + background).round() as u8;
    let samples: Vec<_> = (EDGE_TOLERANCE..flat.height() - EDGE_TOLERANCE)
        .flat_map(|y| (EDGE_TOLERANCE..flat.width() - EDGE_TOLERANCE).map(move |x| (x, y)))
        .filter(|&(x, y)| flat_ground_interior(flat, x, y, gray))
        .collect();
    assert!(
        samples.len() > 10_000,
        "Flat constant raster must have substantial visible interior: {} samples, expected gray={gray}, center={:?}",
        samples.len(),
        flat.get_pixel(flat.width() / 2, flat.height() / 2).0
    );
    let missing: Vec<_> = samples
        .iter()
        .copied()
        .filter(|&(x, y)| volume.get_pixel(x, y).0[..3].iter().any(|channel| *channel < minimum.saturating_sub(4)))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "missing={}/{} interior pixels; first coordinates/colors: {:?}",
        missing.len(),
        samples.len(),
        missing.iter().take(3).map(|&(x, y)| ((x, y), volume.get_pixel(x, y).0)).collect::<Vec<_>>()
    ))
}

fn assert_globe_ground_coverage(band_count: usize, normalized_height: f32) -> Result<()> {
    let fixture = ground_fixture(band_count, normalized_height)?;
    let mut map = NativeMap::with_style(request(53.5, 23.5, 8.0, 0.0, 0.0).viewport, Some(tests::DARK_STYLE))?;
    map.set_globe(true);
    map.numeric.borrow_mut().set_reduce_motion(true);
    let mut failures = Vec::new();
    let views = [8.0, 9.0, 10.0, 11.0, 11.5, 12.0].into_iter().flat_map(|zoom| {
        [
            (53.5, 23.5, 0.0, 0.0),
            (53.5, 23.5, 55.0, 37.0),
            (53.501, 23.499, 55.25, 37.5),
            (53.499, 23.501, 54.75, 36.5),
        ]
        .map(|(longitude, latitude, pitch, bearing)| request(longitude, latitude, zoom, pitch, bearing))
    });
    for view in views {
        // Globe volumes use private depth at every opacity, while flat rasters
        // retain the native path. Include both near-opaque and ordinary alpha.
        for opacity in [1.0, 0.999, 0.5] {
            let flat = ground_image(&mut map, &fixture, view, false, opacity)?;
            let volume = ground_image(&mut map, &fixture, view, true, opacity)?;
            if let Some(failure) = ground_coverage_failure(&flat, &volume, opacity, normalized_height) {
                failures.push(format!(
                    "bands={band_count} height={normalized_height} opacity={opacity} camera={:?}: {failure}",
                    view.camera
                ));
            }
        }
    }
    assert!(failures.is_empty(), "Globe ground coverage:\n{}", failures.join("\n"));
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF only, no network"]
fn numeric_globe_zero_height_bars_preserve_surface_coverage() -> Result<()> {
    assert_globe_ground_coverage(1, 0.0)
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local GeoTIFF only, no network"]
fn numeric_globe_low_positive_bars_preserve_surface_coverage() -> Result<()> {
    assert_globe_ground_coverage(1, 1.0 / 1024.0)
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local multiband GeoTIFF only, no network"]
fn multiband_globe_zero_height_bars_preserve_surface_coverage() -> Result<()> {
    assert_globe_ground_coverage(2, 0.0)
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local multiband GeoTIFF only, no network"]
fn multiband_globe_low_positive_bars_preserve_surface_coverage() -> Result<()> {
    assert_globe_ground_coverage(2, 1.0 / 1024.0)
}

fn assert_visible_extrusion(ground: &RgbaImage, volume: &RgbaImage) {
    let ground_pixels = ground.pixels().filter(|pixel| foreground(pixel)).count();
    let volume_pixels = volume.pixels().filter(|pixel| foreground(pixel)).count();
    let new_pixels = volume
        .enumerate_pixels()
        .filter(|(x, y, pixel)| foreground(pixel) && !foreground(ground.get_pixel(*x, *y)))
        .count();
    assert!(ground_pixels > 10, "The isolated bar's ground footprint must be visible");
    assert!(volume_pixels > 10, "Radially extruded geometry must remain visible");
    assert!(
        new_pixels > 10,
        "Extrusion must extend beyond the flat footprint, not merely recolor it: {new_pixels}"
    );
}
