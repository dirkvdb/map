use super::*;
use crate::raster_source::NumericTile;
use crate::raster_tiles::test_support::{assert_half_opacity, device, paint, pan_right, parameters};

fn command(bands: &[std::sync::Arc<RasterSource>], revision: u64) -> PlaybackRequest {
    PlaybackRequest::new(bands.to_vec(), 0.25, revision, false, 10.0, true)
        .with_style(PlaybackStyle::default().with_palette(ColorMapPreset::Gray).with_range((0.0, 255.0)).unwrap())
}

fn image(layer: &mut MultibandLayer, zoom: f64) -> Result<image::RgbaImage> {
    let (device, queue) = (layer.device.clone(), layer.queue.clone());
    paint(&device, &queue, |pass| layer.render(pass, &parameters(zoom)))
}

fn seed(layer: &mut MultibandLayer, visible: &[VisibleTile], value: Option<f32>) -> Result<()> {
    let count = layer.request.as_ref().unwrap().bands().len().min(layer.window_size);
    for tile in visible {
        let size = layer.tile_size;
        let data = (0..count)
            .map(|_| {
                value.map(|value| NumericTile {
                    width: size,
                    height: size,
                    values: vec![value; size as usize * size as usize],
                })
            })
            .collect();
        let texture = tiles::upload_window(&layer.device, &layer.queue, data, size)?;
        let key = WindowKey {
            tile: tile.canonical(),
            start: 0,
        };
        if let Some(old) = layer.cache.insert(key, CachedWindow { texture, used: layer.frame }) {
            layer.cache_bytes -= old.bytes();
        }
        layer.cache_bytes += layer.cache[&key].bytes();
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_retains_the_accepted_multiband_frame_until_all_tiles_are_ready() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    for (old_zoom, new_zoom) in [(0.0, 1.0), (1.0, 0.0), (0.0, 2.0)] {
        let mut layer = MultibandLayer::new(device.clone(), queue.clone())?;
        layer.set_playback(command(&bands, 1), 0.5);
        let old = visible_tiles(&parameters(old_zoom), &bands[0]);
        let target = visible_tiles(&parameters(new_zoom), &bands[0]);
        assert_ne!(old[0].z, target[0].z);
        seed(&mut layer, &old, Some(80.0))?;
        let before = image(&mut layer, old_zoom)?;
        assert_half_opacity(&before);
        assert_eq!(image(&mut layer, new_zoom)?, before);
        assert!(layer.retained.is_some() && layer.clock.is_buffering());
        seed(&mut layer, &target[..1], Some(160.0))?;
        assert_eq!(image(&mut layer, new_zoom)?, before, "Partial windows must not replace covering tiles");
        assert_eq!(layer.clock.position(), 0.25, "Navigation must not seek playback");
        seed(&mut layer, &target, Some(160.0))?;
        let after = image(&mut layer, new_zoom)?;
        assert_half_opacity(&after);
        assert_ne!(after, before);
        assert!(layer.retained.is_none() && !layer.clock.is_buffering());
        assert!(layer.cache_bytes <= MAX_GRID_BYTES);
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_retargets_and_returns_to_a_cached_grid_without_losing_the_displayed_frame() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    layer.set_playback(command(&bands, 1), 0.5);
    seed(&mut layer, &visible_tiles(&parameters(0.0), &bands[0]), Some(80.0))?;
    let before = image(&mut layer, 0.0)?;
    for zoom in [1.0, 2.0, 1.0] {
        assert_eq!(image(&mut layer, zoom)?, before);
        assert!(layer.retained.is_some());
    }
    assert_eq!(image(&mut layer, 0.0)?, before);
    assert!(layer.retained.is_none());
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_survives_window_resize_and_cache_eviction_and_uses_current_style() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device.clone(), queue.clone())?;
    let request = command(&bands, 1);
    layer.set_playback(request.clone(), 0.5);
    seed(&mut layer, &visible_tiles(&parameters(0.0), &bands[0]), Some(80.0))?;
    let before = image(&mut layer, 0.0)?;
    let expanded = (0..64)
        .map(|index| VisibleTile {
            z: 3,
            x: index % 8,
            y: (index / 8) as i32,
        })
        .collect();
    layer.update_coverage(expanded, Instant::now());
    assert_eq!(layer.window_size, 2);
    assert!(layer.cache.is_empty() && layer.retained.is_some());
    let retained = paint(&device, &queue, |pass| layer.draw_retained_frame(pass, &parameters(1.0), &request))?;
    assert_eq!(retained, before, "Retained bindings must own textures after their cache is cleared");
    layer.set_playback(request.clone(), 0.25);
    let styled = paint(&device, &queue, |pass| layer.draw_retained_frame(pass, &parameters(1.0), &request))?;
    assert!(styled.pixels().all(|pixel| pixel[3].abs_diff(64) <= 1));
    layer.clear_dataset();
    assert!(layer.retained.is_none() && layer.draws.is_empty() && layer.cache_bytes == 0);
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_empty_tiles_resolve_retention_without_leaving_old_data_visible() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    layer.set_playback(command(&bands, 1), 0.5);
    seed(&mut layer, &visible_tiles(&parameters(1.0), &bands[0]), Some(80.0))?;
    assert_half_opacity(&image(&mut layer, 1.0)?);
    assert_half_opacity(&image(&mut layer, 0.0)?);
    seed(&mut layer, &visible_tiles(&parameters(0.0), &bands[0]), None)?;
    assert!(image(&mut layer, 0.0)?.pixels().all(|pixel| pixel[3] == 0));
    assert!(layer.retained.is_none() && !layer.clock.is_buffering());
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_explicit_seeks_discard_the_retained_temporal_frame() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    layer.set_playback(command(&bands, 1), 0.5);
    seed(&mut layer, &visible_tiles(&parameters(0.0), &bands[0]), Some(80.0))?;
    image(&mut layer, 0.0)?;
    image(&mut layer, 1.0)?;
    assert!(layer.retained.is_some());
    layer.set_playback(command(&bands, 2), 0.5);
    assert!(layer.retained.is_none(), "An explicit seek must never show an obsolete accepted frame");
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_zoom_coverage_reprojects_the_retained_multiband_frame_while_panning() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device.clone(), queue.clone())?;
    layer.set_playback(command(&bands, 1), 0.5);
    let (west, east): (Vec<_>, Vec<_>) = visible_tiles(&parameters(1.0), &bands[0])
        .into_iter()
        .partition(|tile| tile.x < 1 << (tile.z - 1));
    seed(&mut layer, &west, Some(80.0))?;
    seed(&mut layer, &east, None)?;
    assert_eq!(image(&mut layer, 1.0)?.get_pixel(40, 32)[3], 0);
    let mut panned = parameters(0.0);
    pan_right(&mut panned, 16.0);
    let image = paint(&device, &queue, |pass| layer.render(pass, &panned))?;
    assert!(
        image.get_pixel(40, 32)[3].abs_diff(128) <= 1,
        "The accepted frame must move with the camera while buffering"
    );
    assert_eq!(image.get_pixel(56, 32)[3], 0);
    Ok(())
}
