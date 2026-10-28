//! Worker/native integration regressions. GPU cases use local data and no style URLs.

use gdal::Metadata as _;
use inf::colormap::ColorMapPreset;

use super::*;
use crate::raster_playback::PlaybackStyle;

struct Fixture {
    _directory: tempfile::TempDir,
    bands: Vec<Arc<RasterSource>>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("input.tif");
        let output = directory.path().join("playback.tif");
        crate::raster_source::test_support::write_raster(&input, 3857, 43)?;
        write_cog(&input, &output)?;
        pad_header(&output)?;
        let bands = RasterSource::open(&output)?
            .iter()
            .map(|band| band.restyled(ColorMapPreset::Gray, (0.0, 600.0)))
            .collect::<Result<Vec<_>>>()?;
        ensure!(bands.len() == 43, "Fixture must expose every COG band");
        ensure!(
            bands[0].metadata().source_format == tiler::LayerSourceType::CloudOptimizedGeoTiff,
            "Fixture must use native COG reads"
        );
        Ok(Self { _directory: directory, bands })
    }

    fn command(&self, position: f64, revision: u64, playing: bool) -> PlaybackRequest {
        PlaybackRequest::new(self.bands.clone(), position, revision, playing, 2.0, true)
    }

    fn map_request(&self) -> MapRequest {
        let viewport = Viewport {
            width: NonZeroU32::new(96).unwrap(),
            height: NonZeroU32::new(96).unwrap(),
            scale: 1.0,
        };
        MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(self.bands[0].metadata().bounds, 96.0, 96.0),
        }
    }

    fn render_request(&self, command: PlaybackRequest) -> RenderRequest {
        RenderRequest {
            map: self.map_request(),
            raster: Some(self.bands[0].clone()),
            playback: Some(command),
            vector: None,
            vector_color: String::new(),
            opacity: 1.0,
            reduce_motion: false,
            basemap: BasemapStyle::Dark,
            tile_boundaries: false,
            enqueued_at: None,
        }
    }
}

fn write_cog(input: &std::path::Path, output: &std::path::Path) -> Result<()> {
    let dataset = gdal::Dataset::open(input)?;
    for band in 1..=43 {
        dataset.rasterband(band)?.set_description(&format!("Frame {band}"))?;
    }
    let options = gdal::raster::RasterCreationOptions::from_iter([
        "TILING_SCHEME=GoogleMapsCompatible",
        "ZOOM_LEVEL=3",
        "BLOCKSIZE=256",
        "OVERVIEWS=NONE",
        "COMPRESS=DEFLATE",
        "INTERLEAVE=BAND",
        "ADD_ALPHA=NO",
    ]);
    dataset
        .create_copy(&gdal::DriverManager::get_driver_by_name("COG")?, output, &options)?
        .close()?;
    Ok(())
}

fn pad_header(output: &std::path::Path) -> Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(output)?;
    file.set_len(file.metadata()?.len().max(16 * 1024))?;
    Ok(())
}

fn send(renderer: &MapRenderer, map: MapRequest, command: PlaybackRequest) -> Result<()> {
    let anchor = command.bands()[0].clone();
    renderer.render_layer(map, Some(MapSource::Multiband(anchor, command)), 1.0, false, BasemapStyle::Dark, false)
}

fn receive_matching(frames: &async_channel::Receiver<MapFrame>, matches: impl Fn(&MapFrame) -> bool) -> Result<MapFrame> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        ensure!(Instant::now() < deadline, "Local playback worker did not deliver the expected frame");
        match frames.try_recv() {
            Ok(frame) => {
                if let Err(error) = &frame.image {
                    anyhow::bail!("Local playback worker failed: {error:#}");
                }
                if matches(&frame) {
                    return Ok(frame);
                }
            }
            Err(async_channel::TryRecvError::Closed) => anyhow::bail!("Playback worker stopped unexpectedly"),
            Err(async_channel::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

fn ready_revision(frame: &MapFrame, revision: u64) -> bool {
    frame
        .playback
        .is_some_and(|playback| playback.revision() == revision && !playback.is_buffering())
}

fn settled_native(fixture: &Fixture) -> Result<(NativeMap, RenderRequest, RgbaImage)> {
    settled_native_request(fixture.render_request(fixture.command(0.0, 1, false)))
}

fn settled_native_request(request: RenderRequest) -> Result<(NativeMap, RenderRequest, RgbaImage)> {
    let mut map = NativeMap::with_style(request.map.viewport, Some(tests::DARK_STYLE))?;
    map.apply_render_request(&request)?;
    let image = settle(&mut map)?;
    Ok((map, request, image))
}

fn settle(map: &mut NativeMap) -> Result<RgbaImage> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut image = None;
    loop {
        ensure!(Instant::now() < deadline, "Local native playback did not settle");
        if let Some(texture) = map.poll_frame()? {
            image = Some(map.read_frame(&texture)?);
        }
        if map.is_settled() {
            return image.context("Settled playback must produce a readback");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn freeze_native_callbacks(map: &mut NativeMap) {
    map.renderer.map_observer().set_did_become_idle_callback(|| {});
    map.renderer.set_render_requested_callback(|| {});
    map.needs_frame.set(false);
}

#[test]
fn renderer_failure_fallback_preserves_command_identity_position_and_buffering() -> Result<()> {
    let fixture = Fixture::new()?;
    let request = fixture.render_request(fixture.command(25.5, 9, true));
    assert_eq!(
        request.playback_frame(None),
        Some(PlaybackFrame::new(fixture.bands[0].metadata().id, 9, 25.5, true))
    );
    let mut cleared = request;
    cleared.playback = None;
    assert!(cleared.playback_frame(None).is_none());
    Ok(())
}

#[test]
fn replacement_command_only_resumes_matching_dataset_and_revision() -> Result<()> {
    let fixture = Fixture::new()?;
    let command = fixture.command(0.0, 7, true);
    let dataset = command.dataset_id().unwrap();
    for feedback in [
        None,
        Some(PlaybackFrame::new(dataset, 6, 12.5, false)),
        Some(PlaybackFrame::new(fixture.bands[1].metadata().id, 7, 12.5, false)),
    ] {
        assert_eq!(command.resumed_from(feedback).position(), 0.0);
    }
    let resumed = command.resumed_from(Some(PlaybackFrame::new(dataset, 7, 12.5, false)));
    assert_eq!(resumed.position(), 12.5);
    assert_eq!(resumed.revision(), command.revision());
    assert!(resumed.is_playing());
    assert_eq!(resumed.bands_per_second(), command.bands_per_second());
    assert_eq!(resumed.interpolation(), command.interpolation());
    assert!(Arc::ptr_eq(&resumed.bands()[0], &command.bands()[0]));
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn worker_playback_advances_readbacks_pauses_stably_and_seeks_across_windows() -> Result<()> {
    let fixture = Fixture::new()?;
    let renderer = MapRenderer::with_test_style(tests::DARK_STYLE)?;
    let frames = renderer.frames();
    let map = fixture.map_request();
    let position = advance_worker(&renderer, &frames, &fixture)?;
    let paused_pixels = assert_paused_worker(&renderer, &frames, &fixture, position)?;

    send(&renderer, map, fixture.command(25.5, 3, false))?;
    let seek = receive_matching(&frames, |frame| ready_revision(frame, 3) && frame.settled)?;
    assert_eq!(seek.playback.unwrap().position(), 25.5);
    assert_ne!(seek.image?.as_bytes(0), paused_pixels.as_bytes(0));
    Ok(())
}

fn advance_worker(renderer: &MapRenderer, frames: &async_channel::Receiver<MapFrame>, fixture: &Fixture) -> Result<f64> {
    send(renderer, fixture.map_request(), fixture.command(0.0, 1, true))?;
    let initial = receive_matching(frames, |frame| ready_revision(frame, 1))?;
    let initial_position = initial.playback.unwrap().position();
    let advanced = receive_matching(frames, |frame| {
        ready_revision(frame, 1) && frame.playback.unwrap().position() >= initial_position + 0.75
    })?;
    let position = advanced.playback.unwrap().position();
    assert_ne!(
        initial.image?.as_bytes(0),
        advanced.image?.as_bytes(0),
        "Advancing time must change actual raster pixels"
    );
    Ok(position)
}

fn assert_paused_worker(renderer: &MapRenderer, frames: &async_channel::Receiver<MapFrame>, fixture: &Fixture, position: f64) -> Result<Arc<RenderImage>> {
    send(renderer, fixture.map_request(), fixture.command(position, 2, false))?;
    let paused = receive_matching(frames, |frame| ready_revision(frame, 2) && frame.settled)?;
    assert_eq!(paused.playback.unwrap().position(), position);
    let paused_pixels = paused.image?;
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(40));
        send(renderer, fixture.map_request(), fixture.command(position, 2, false))?;
        let frame = receive_matching(frames, |frame| ready_revision(frame, 2) && frame.settled)?;
        assert_eq!(frame.playback.unwrap().position(), position);
        assert_eq!(frame.image?.as_bytes(0), paused_pixels.as_bytes(0));
    }
    Ok(paused_pixels)
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn worker_dpi_recreation_resumes_the_accepted_position_not_the_original_command() -> Result<()> {
    let fixture = Fixture::new()?;
    let renderer = MapRenderer::with_test_style(tests::DARK_STYLE)?;
    let frames = renderer.frames();
    let original = fixture.command(0.0, 7, true);
    let mut map = fixture.map_request();
    send(&renderer, map, original.clone())?;
    let accepted = receive_matching(&frames, |frame| ready_revision(frame, 7) && frame.playback.unwrap().position() >= 1.5)?;
    let position = accepted.playback.unwrap().position();
    map.viewport.scale = 2.0;
    send(&renderer, map, original)?;
    let recreated = receive_matching(&frames, |frame| frame.viewport.scale == 2.0 && ready_revision(frame, 7))?;
    assert!(
        recreated.playback.unwrap().position() >= position,
        "DPI recreation must not rewind to the command's initial zero"
    );
    assert_eq!(recreated.image?.as_bytes(0).unwrap().len(), 192 * 192 * 4);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn worker_source_switches_and_clear_stop_both_paused_and_playing_feedback() -> Result<()> {
    let first = Fixture::new()?;
    let second = Fixture::new()?;
    let renderer = MapRenderer::with_test_style(tests::DARK_STYLE)?;
    let frames = renderer.frames();
    for (ix, playing) in [false, true].into_iter().enumerate() {
        let revision = 10 + ix as u64 * 2;
        send(&renderer, first.map_request(), first.command(0.0, revision, playing))?;
        receive_matching(&frames, |frame| ready_revision(frame, revision))?;
        assert_switched_worker(&renderer, &frames, &second, revision + 1)?;
        assert_static_worker(&renderer, &frames, &second)?;
        assert_cleared_worker(&renderer, &frames, second.map_request())?;
    }
    Ok(())
}

fn assert_switched_worker(renderer: &MapRenderer, frames: &async_channel::Receiver<MapFrame>, fixture: &Fixture, revision: u64) -> Result<()> {
    send(renderer, fixture.map_request(), fixture.command(2.5, revision, false))?;
    let switched = receive_matching(frames, |frame| ready_revision(frame, revision) && frame.settled)?;
    assert_eq!(switched.playback.unwrap().dataset_id(), fixture.bands[0].metadata().id);
    assert_eq!(switched.playback.unwrap().position(), 2.5);
    Ok(())
}

fn assert_static_worker(renderer: &MapRenderer, frames: &async_channel::Receiver<MapFrame>, fixture: &Fixture) -> Result<()> {
    let source = fixture.bands[2].clone();
    let id = source.metadata().id;
    renderer.render_layer(fixture.map_request(), Some(MapSource::Raster(source)), 1.0, false, BasemapStyle::Dark, false)?;
    receive_matching(frames, |frame| frame.raster_layer_id == Some(id) && frame.playback.is_none() && frame.settled)?;
    Ok(())
}

fn assert_cleared_worker(renderer: &MapRenderer, frames: &async_channel::Receiver<MapFrame>, map: MapRequest) -> Result<()> {
    renderer.render(map)?;
    receive_matching(frames, |frame| frame.raster_layer_id.is_none() && frame.playback.is_none() && frame.settled)?;
    std::thread::sleep(Duration::from_millis(80));
    while let Ok(late) = frames.try_recv() {
        assert!(
            late.raster_layer_id.is_none() && late.playback.is_none(),
            "Cleared playback cannot publish a late source frame"
        );
        late.image?;
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn paused_ready_playback_does_not_renew_an_expired_native_deadline() -> Result<()> {
    let fixture = Fixture::new()?;
    let (mut map, _, _) = settled_native(&fixture)?;
    freeze_native_callbacks(&mut map);
    map.idle.set(true);
    let expired = Instant::now() - Duration::from_secs(1);
    map.deadline = expired;
    assert!(map.poll_frame()?.is_none());
    assert_eq!(map.deadline, expired, "A paused, ready raster must not extend the native loading deadline");
    map.needs_frame.set(true);
    assert!(map.poll_frame().unwrap_err().to_string().contains("timed out"));
    assert_eq!(map.deadline, expired);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn playing_raster_does_not_exempt_stalled_native_work_from_timeout() -> Result<()> {
    let fixture = Fixture::new()?;
    let (mut map, mut request, _) = settled_native(&fixture)?;
    request.playback = Some(fixture.command(0.0, 2, true));
    map.apply_render_request(&request)?;
    assert!(!map.numeric.borrow().is_loading());
    freeze_native_callbacks(&mut map);
    map.idle.set(false);
    let expired = Instant::now() - Duration::from_secs(1);
    map.deadline = expired;
    assert!(map.poll_frame().unwrap_err().to_string().contains("timed out"));
    assert_eq!(map.deadline, expired);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn playing_buffered_raster_does_not_renew_deadline_even_when_native_is_idle() -> Result<()> {
    let fixture = Fixture::new()?;
    let (mut map, mut request, _) = settled_native(&fixture)?;
    request.playback = Some(fixture.command(42.5, 2, true));
    map.apply_render_request(&request)?;
    assert!(
        map.numeric.borrow().is_loading(),
        "The last partial window must not have been prefetched at band zero"
    );
    freeze_native_callbacks(&mut map);
    map.idle.set(true);
    let expired = Instant::now() - Duration::from_secs(1);
    map.deadline = expired;
    assert!(map.poll_frame().unwrap_err().to_string().contains("timed out"));
    assert_eq!(map.deadline, expired);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn ready_playing_raster_renews_deadline_without_changing_the_native_camera() -> Result<()> {
    let fixture = Fixture::new()?;
    let (mut map, mut request, _) = settled_native(&fixture)?;
    let camera_updates = map.camera_updates;
    request.playback = Some(fixture.command(0.0, 2, true));
    map.apply_render_request(&request)?;
    freeze_native_callbacks(&mut map);
    map.idle.set(true);
    map.deadline = Instant::now() - Duration::from_secs(1);
    assert!(map.poll_frame()?.is_some());
    assert!(map.deadline > Instant::now());
    assert_eq!(map.camera_updates, camera_updates);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn shared_band_range_drag_renders_latest_colors_without_changing_anchor_or_clock() -> Result<()> {
    let fixture = Fixture::new()?;
    let command = fixture.command(3.25, 7, false);
    let (mut map, mut request, before) = settled_native_request(fixture.render_request(command.clone()))?;
    let camera_updates = map.camera_updates;
    let anchor = map.raster.as_ref().unwrap().id();
    std::fs::remove_file(&fixture.bands[0].metadata().path)?;
    // Simulate a selected-band anchor from the UI; playback must ignore it.
    request.raster = Some(fixture.bands[3].clone());
    let mut after = before.clone();
    for maximum in 601..=700 {
        request.playback = Some(command.clone().with_style(PlaybackStyle::default().with_range((0.0, maximum as f32))?));
        map.apply_render_request(&request)?;
        after = settle(&mut map)?;
    }
    assert_ne!(before.as_raw(), after.as_raw());
    assert_eq!(map.raster.as_ref().unwrap().id(), anchor);
    assert_eq!(map.numeric.borrow().playback_frame().unwrap().position(), 3.25);
    assert_eq!(map.camera_updates, camera_updates);
    assert_final_range_matches_legacy(&fixture, &mut map, request, &after)
}

fn assert_final_range_matches_legacy(fixture: &Fixture, map: &mut NativeMap, mut request: RenderRequest, after: &RgbaImage) -> Result<()> {
    // The final pixels must exactly match a direct legacy source restyle.
    let styled = fixture
        .bands
        .iter()
        .map(|band| band.restyled(ColorMapPreset::Gray, (0.0, 700.0)))
        .collect::<Result<Vec<_>>>()?;
    request.playback = Some(PlaybackRequest::new(styled, 0.0, 7, false, 2.0, true));
    map.apply_render_request(&request)?;
    let expected = settle(map)?;
    assert_eq!(after.as_raw(), expected.as_raw());
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local COG and style, no network"]
fn same_revision_restyle_changes_pixels_without_rewinding_or_updating_camera() -> Result<()> {
    let fixture = Fixture::new()?;
    let request = fixture.render_request(fixture.command(3.25, 7, false));
    let (mut map, mut request, before) = settled_native_request(request)?;
    let updates = map.camera_updates;
    let position = map.numeric.borrow().playback_frame().unwrap().position();
    let styled = fixture
        .bands
        .iter()
        .map(|band| band.restyled(ColorMapPreset::Turbo, (0.0, 600.0)))
        .collect::<Result<Vec<_>>>()?;
    request.raster = Some(styled[0].clone());
    // A stale position in a same-revision style snapshot must not become a seek.
    request.playback = Some(PlaybackRequest::new(styled, 0.0, 7, false, 2.0, true));
    map.apply_render_request(&request)?;
    let after = settle(&mut map)?;
    assert_ne!(before.as_raw(), after.as_raw());
    assert_eq!(map.numeric.borrow().playback_frame().unwrap().position(), position);
    assert_eq!(map.camera_updates, updates);
    assert!(map.camera == Some(request.map.camera));
    Ok(())
}
