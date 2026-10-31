//! Manual selections must use color crossfades, not playback's value interpolation.

use super::*;
use crate::raster_source::NumericTile;
use crate::raster_tiles::test_support::{device, paint, parameters};
use std::time::Duration;

fn command(bands: &[std::sync::Arc<RasterSource>], position: f64, revision: u64) -> PlaybackRequest {
    tests::request(bands, position, revision).with_style(PlaybackStyle::default().with_palette(ColorMapPreset::Jet).with_range((0.0, 255.0)).unwrap())
}

fn image(layer: &mut MultibandLayer) -> Result<image::RgbaImage> {
    let (device, queue) = (layer.device.clone(), layer.queue.clone());
    paint(&device, &queue, |pass| layer.render(pass, &parameters(0.0)))
}

fn seed(layer: &mut MultibandLayer, bands: &[std::sync::Arc<RasterSource>], start: usize) -> Result<()> {
    for tile in visible_tiles(&parameters(0.0), &bands[0]) {
        let size = layer.tile_size;
        let data = window_range(start, layer.window_size, bands.len())
            .map(|band| {
                Some(NumericTile {
                    width: size,
                    height: size,
                    values: vec![if band == 0 { 0.0 } else { 255.0 }; size as usize * size as usize],
                })
            })
            .collect();
        let texture = tiles::upload_window(&layer.device, &layer.queue, data, size)?;
        let key = WindowKey { tile: tile.canonical(), start };
        layer.cache_bytes += texture.as_ref().map_or(0, |texture| texture.bytes);
        layer.cache.insert(key, CachedWindow { texture, used: layer.frame });
    }
    Ok(())
}

fn fade_image(layer: &mut MultibandLayer, elapsed: Duration) -> Result<image::RgbaImage> {
    let now = Instant::now();
    layer.band_fade.as_mut().unwrap().frame(now, true);
    layer.display_frame(now + elapsed / 2);
    let frame = layer.display_frame(now + elapsed);
    let request = layer.request.clone().unwrap();
    let visible = visible_tiles(&parameters(0.0), &request.bands()[0]);
    let (device, queue) = (layer.device.clone(), layer.queue.clone());
    paint(&device, &queue, |pass| layer.draw_frame(pass, &parameters(0.0), &request, &visible, frame))
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_dataset_switch_does_not_fade_from_outgoing_band_indices() -> Result<()> {
    let (_outgoing_directory, outgoing) = tiles::cog_fixture(43)?;
    let (_incoming_directory, incoming) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    for position in [42.0, 42.5] {
        let mut layer = MultibandLayer::new(device.clone(), queue.clone())?;
        layer.set_playback(command(&outgoing, position, 1), 1.0);
        seed(&mut layer, &outgoing, 40)?;
        seed(&mut layer, &outgoing, 0)?;
        image(&mut layer)?;
        assert!(!layer.clock.is_buffering());
        layer.set_playback(command(&incoming, 1.0, 2), 1.0);
        assert!(layer.band_fade.is_none(), "Dataset switches must discard outgoing band indices");
        tests::load_until_idle(&mut layer)?;
        assert_eq!(layer.playback_frame().unwrap().position(), 1.0);
        assert!(
            incoming[0].errors().try_recv().is_err(),
            "The new dataset must load without invalid band requests"
        );
        layer.set_playback(command(&incoming, 2.0, 3), 1.0);
        assert!(layer.band_fade.is_some(), "Selections within the new dataset must still crossfade");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_manual_band_selection_keeps_outgoing_pixels_then_crossfades_across_windows() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(43)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    layer.set_playback(command(&bands, 0.0, 1), 1.0);
    seed(&mut layer, &bands, 0)?;
    seed(&mut layer, &bands, 20)?;
    let outgoing = image(&mut layer)?;
    layer.set_playback(command(&bands, 42.0, 2), 1.0);
    assert!(image(&mut layer)? == outgoing, "Missing incoming tiles must not blank the raster");
    assert!(layer.is_loading() && layer.poll_tiles());
    seed(&mut layer, &bands, 40)?;
    let middle = fade_image(&mut layer, Duration::from_millis(90))?;
    assert!(layer.band_fade.is_some(), "A cached selection must still animate");
    // Expire the fade deterministically without relying on GPU/wall-clock speed.
    layer.band_fade = Some(BandFade::new(0, 42));
    let incoming = fade_image(&mut layer, Duration::from_secs(1))?;
    let (a, b, c) = (outgoing.get_pixel(32, 32), middle.get_pixel(32, 32), incoming.get_pixel(32, 32));
    assert_ne!(a, c);
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert!(
        b[1] <= a[1].max(c[1]) + 1,
        "Crossfading blue/red must not pass through Jet's green midpoint: {b:?}"
    );
    assert_eq!(b[3], 255, "Opaque bands must remain opaque throughout the fade");
    assert!(layer.band_fade.is_none());
    assert_eq!(layer.playback_frame().unwrap().position(), 42.0);
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_manual_band_selections_coalesce_without_replacing_an_active_blend() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    layer.set_playback(command(&bands, 0.0, 1), 1.0);
    seed(&mut layer, &bands, 0)?;
    image(&mut layer)?;
    layer.set_playback(command(&bands, 1.0, 2), 1.0);
    let now = Instant::now();
    layer.band_fade.as_mut().unwrap().frame(now, true);
    layer.set_playback(command(&bands, 2.0, 3), 1.0);
    let frame = layer.display_frame(now + Duration::from_millis(90));
    assert_eq!((frame.current, frame.next), (0, 1));
    layer.display_frame(now + Duration::from_secs(1));
    let frame = layer.display_frame(now + Duration::from_secs(1));
    assert_eq!((frame.current, frame.next, frame.fraction), (1, 2, 0.0));
    layer.set_reduce_motion(true);
    assert!(layer.band_fade.is_none());
    layer.set_playback(command(&bands, 0.0, 4), 1.0);
    assert!(layer.band_fade.is_none(), "Reduced motion must skip automatic fades");
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_uncached_band_selection_waits_for_loading_and_survives_a_first_frame_upload_stall() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(43)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    layer.set_playback(command(&bands, 0.0, 1), 1.0);
    seed(&mut layer, &bands, 0)?;
    image(&mut layer)?;
    layer.set_playback(command(&bands, 42.0, 2), 1.0);
    let now = Instant::now();
    let loaded = now + Duration::from_secs(10);
    assert_eq!(layer.display_frame(now).fraction, 0.0);
    assert_eq!(layer.display_frame(loaded).fraction, 0.0);
    seed(&mut layer, &bands, 40)?;
    assert_eq!(layer.display_frame(loaded).fraction, 0.0);
    // Cache readiness precedes GPU submission/readback. A large newly uploaded
    // window can stall this first frame for longer than the entire fade.
    assert_eq!(layer.display_frame(loaded + Duration::from_secs(2)).fraction, 0.5);
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device; deterministic local tiles, no network"]
fn gpu_timeline_seek_from_a_fractional_playback_frame_keeps_the_outgoing_raster() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(43)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    layer.set_playback(command(&bands, 0.25, 1), 1.0);
    seed(&mut layer, &bands, 0)?;
    let outgoing = image(&mut layer)?;
    layer.set_playback(command(&bands, 42.0, 2), 1.0);
    assert!(
        image(&mut layer)? == outgoing,
        "A timeline click must retain the actual fractional playback frame while loading"
    );
    assert!(layer.band_fade.is_some(), "A timeline click from playback must fade, not snap");
    seed(&mut layer, &bands, 40)?;
    let middle = fade_image(&mut layer, Duration::from_millis(90))?;
    let from = layer.band_fade.as_ref().unwrap().from;
    layer.band_fade = Some(BandFade::from_frame(from, 42));
    let incoming = fade_image(&mut layer, Duration::from_secs(1))?;
    assert!(
        middle != outgoing && middle != incoming,
        "The selected band must blend from the actual fractional frame"
    );
    let (a, b, c) = (outgoing.get_pixel(32, 32), middle.get_pixel(32, 32), incoming.get_pixel(32, 32));
    for channel in 0..4 {
        let expected = (u16::from(a[channel]) + u16::from(c[channel])).div_ceil(2) as u8;
        assert!(
            b[channel].abs_diff(expected) <= 1,
            "The midpoint must blend the outgoing temporal color, not an integer band"
        );
    }
    Ok(())
}
