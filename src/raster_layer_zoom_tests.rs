use super::*;
use crate::raster_tiles::test_support::{assert_half_opacity, device, paint, pan_right, parameters};

fn grid(z: i32) -> Vec<VisibleTile> {
    vec![VisibleTile { z, x: 0, y: 0 }, VisibleTile { z, x: -1, y: 0 }]
}

#[test]
fn zoom_coverage_keeps_the_displayed_grid_until_replacements_are_ready_in_both_directions() {
    for (old, new) in [(2, 3), (3, 2), (2, 5), (5, 1)] {
        let mut displayed = grid(old);
        assert_eq!(retained_zoom_tiles(&mut displayed, grid(new), false), grid(old));
        assert_eq!(retained_zoom_tiles(&mut displayed, grid(new), true), grid(new));
        assert_eq!(displayed, grid(new));
    }
}

#[test]
fn zoom_coverage_retargets_without_discarding_the_last_displayed_grid() {
    let mut displayed = grid(3);
    for target in [4, 5, 2, 1] {
        assert_eq!(retained_zoom_tiles(&mut displayed, grid(target), false), grid(3));
    }
    assert_eq!(retained_zoom_tiles(&mut displayed, grid(3), false), grid(3));
}

#[test]
fn zoom_coverage_allows_initial_loading_panning_and_empty_coverage() {
    let mut displayed = Vec::new();
    assert_eq!(retained_zoom_tiles(&mut displayed, grid(2), false), grid(2));
    let shifted: Vec<_> = grid(2).iter().map(|tile| VisibleTile { x: tile.x + 4, ..*tile }).collect();
    assert_eq!(retained_zoom_tiles(&mut displayed, shifted.clone(), false), shifted);
    assert!(retained_zoom_tiles(&mut displayed, Vec::new(), false).is_empty());
    assert!(displayed.is_empty());
}

fn source() -> Result<(tempfile::TempDir, Arc<RasterSource>)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("zoom.tif");
    crate::raster_source::test_support::write_raster(&path, 4326, 1)?;
    let source = RasterSource::open(&path)?.remove(0).restyled(ColorMapPreset::Gray, (0.0, 255.0))?;
    Ok((directory, source))
}

fn seed(layer: &mut RasterLayer, tiles: &[VisibleTile], value: Option<f32>) {
    for tile in tiles {
        let texture = value.map(|value| {
            upload_tile(
                &layer.device,
                &layer.queue,
                NumericTile {
                    width: 2,
                    height: 2,
                    values: vec![value; 4],
                },
            )
        });
        if let Some(old) = layer.cache.insert(tile.canonical(), CachedTile { texture, used: layer.frame }) {
            layer.cache_bytes -= old.texture.map_or(0, |texture| texture.bytes);
        }
        layer.cache_bytes += layer.cache[&tile.canonical()].texture.as_ref().map_or(0, |texture| texture.bytes);
    }
}

fn image(layer: &mut RasterLayer, zoom: f64) -> Result<image::RgbaImage> {
    let (device, queue) = (layer.device.clone(), layer.queue.clone());
    paint(&device, &queue, |pass| layer.render(pass, &parameters(zoom)))
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_retains_tiles_through_partial_loads_without_double_opacity() -> Result<()> {
    let (_directory, source) = source()?;
    let (device, queue) = device()?;
    for (old_zoom, new_zoom) in [(0.0, 1.0), (1.0, 0.0), (0.0, 2.0)] {
        let mut layer = RasterLayer::new(device.clone(), queue.clone())?;
        layer.set_source(Some(source.clone()), 0.5, 1);
        let old = visible_tiles(&parameters(old_zoom), &source);
        let target = visible_tiles(&parameters(new_zoom), &source);
        assert_ne!(old[0].z, target[0].z);
        seed(&mut layer, &old, Some(80.0));
        let before = image(&mut layer, old_zoom)?;
        assert_half_opacity(&before);
        assert_eq!(image(&mut layer, new_zoom)?, before, "The first frame must retain covering tiles");
        seed(&mut layer, &target[..1], Some(160.0));
        assert_eq!(image(&mut layer, new_zoom)?, before, "Partial replacement must not expose holes or overlap");
        seed(&mut layer, &target, Some(160.0));
        let after = image(&mut layer, new_zoom)?;
        assert_half_opacity(&after);
        assert_ne!(after, before, "The ready zoom grid must replace the retained tiles");
        assert_eq!(layer.displayed, target);
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_empty_replacements_remove_old_data_and_clear_releases_retention() -> Result<()> {
    let (_directory, source) = source()?;
    let (device, queue) = device()?;
    let mut layer = RasterLayer::new(device, queue)?;
    layer.set_source(Some(source.clone()), 0.5, 1);
    seed(&mut layer, &visible_tiles(&parameters(1.0), &source), Some(80.0));
    assert_half_opacity(&image(&mut layer, 1.0)?);
    assert_half_opacity(&image(&mut layer, 0.0)?);
    seed(&mut layer, &visible_tiles(&parameters(0.0), &source), None);
    assert!(image(&mut layer, 0.0)?.pixels().all(|pixel| pixel[3] == 0));
    layer.set_source(None, 1.0, 1);
    assert!(layer.displayed.is_empty() && layer.draws.is_empty() && layer.cache.is_empty());
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_reprojects_retained_tiles_while_panning() -> Result<()> {
    let (_directory, source) = source()?;
    let (device, queue) = device()?;
    let mut layer = RasterLayer::new(device.clone(), queue.clone())?;
    layer.set_source(Some(source.clone()), 0.5, 1);
    let (west, east): (Vec<_>, Vec<_>) = visible_tiles(&parameters(1.0), &source)
        .into_iter()
        .partition(|tile| tile.x < 1 << (tile.z - 1));
    seed(&mut layer, &west, Some(80.0));
    seed(&mut layer, &east, None);
    assert_eq!(image(&mut layer, 1.0)?.get_pixel(40, 32)[3], 0);
    let mut panned = parameters(0.0);
    pan_right(&mut panned, 16.0);
    let image = paint(&device, &queue, |pass| layer.render(pass, &panned))?;
    assert!(
        image.get_pixel(40, 32)[3].abs_diff(128) <= 1,
        "Covering tiles must follow the new camera, not freeze on screen"
    );
    assert_eq!(image.get_pixel(56, 32)[3], 0);
    Ok(())
}
