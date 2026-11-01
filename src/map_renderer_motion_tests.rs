//! Continuous input exercises request admission, not just stationary rendering.

use super::*;

fn camera_request(dx: f64) -> RenderRequest {
    let mut camera = MapCamera::default();
    camera.pan(dx, 0.0);
    RenderRequest {
        map: MapRequest {
            viewport: Viewport {
                width: NonZeroU32::new(32).unwrap(),
                height: NonZeroU32::new(24).unwrap(),
                scale: 1.0,
            },
            camera,
        },
        raster: None,
        playback: None,
        vector: None,
        vector_color: String::new(),
        opacity: 1.0,
        reduce_motion: true,
        basemap: BasemapStyle::Dark,
        tile_boundaries: false,
        enqueued_at: None,
    }
}

#[test]
fn pending_draw_defers_camera_admission_and_keeps_only_latest_input() -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    let mut inbox = RenderInbox { receiver, pending: None };
    sender.send(camera_request(1.0))?;
    assert!(inbox.receive(Some(Duration::ZERO), true)?.is_none());
    sender.send(camera_request(2.0))?;
    assert!(inbox.receive(Some(Duration::ZERO), true)?.is_none());
    assert_eq!(
        inbox.receive(None, false)?.map(|request| request.map.camera),
        Some(camera_request(2.0).map.camera)
    );
    assert!(!inbox.has_pending());
    Ok(())
}

#[test]
fn polling_without_new_input_preserves_the_pending_camera() -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    let mut inbox = RenderInbox { receiver, pending: None };
    sender.send(camera_request(3.0))?;
    inbox.receive(Some(Duration::ZERO), true)?;
    inbox.receive(Some(Duration::ZERO), true)?;
    assert_eq!(
        inbox.receive(None, false)?.map(|request| request.map.camera),
        Some(camera_request(3.0).map.camera)
    );
    Ok(())
}

#[test]
fn input_arriving_during_readback_is_coalesced_before_priming_an_old_frame() -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    let mut inbox = RenderInbox { receiver, pending: None };
    sender.send(camera_request(1.0))?;
    let mut latest = camera_request(4.0);
    latest.basemap = BasemapStyle::Empty;
    sender.send(latest)?;
    inbox.coalesce();
    assert!(inbox.has_pending());
    let admitted = inbox.receive(None, false)?.context("Latest camera must be admitted after handoff")?;
    assert_eq!((admitted.map.camera, admitted.basemap), (camera_request(4.0).map.camera, BasemapStyle::Empty));
    Ok(())
}

#[test]
#[ignore = "release-mode continuous camera-input regression; local data, native and wgpu devices"]
fn continuous_camera_input_keeps_delivering_frames() -> Result<()> {
    let (_directory, bands) = crate::raster_source::test_support::cog_fixture(1)?;
    let source = &bands[0];
    let mut counts = Vec::new();
    for size in [512, 1024] {
        let renderer = MapRenderer::with_test_style(tests::DARK_STYLE)?;
        let request = MapRequest {
            viewport: Viewport {
                width: NonZeroU32::new(size).unwrap(),
                height: NonZeroU32::new(size).unwrap(),
                scale: 1.0,
            },
            camera: MapCamera::fit_bounds(source.metadata().bounds, f64::from(size), f64::from(size)),
        };
        renderer.render_raster(request, Some(source.clone()), 1.0, true, BasemapStyle::Dark, false)?;
        let frames = renderer.frames();
        let initial = settled_image(&frames)?;
        let (inputs, delivered, elapsed) = drive_camera(&renderer, request, source, initial)?;
        eprintln!(
            "{size}x{size} continuous camera input: {inputs} inputs, {delivered} images, {:.1} fps",
            delivered as f64 / elapsed.as_secs_f64()
        );
        counts.push(delivered);
        settled_image(&frames)?;
    }
    assert!(
        counts.iter().all(|count| *count > 0),
        "Frames must be delivered while input continues, not only after release: {counts:?}"
    );
    Ok(())
}

fn settled_image(frames: &async_channel::Receiver<MapFrame>) -> Result<Arc<RenderImage>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        ensure!(Instant::now() < deadline, "Camera worker did not settle");
        if let Ok(frame) = frames.try_recv() {
            let image = frame.image?;
            if frame.settled {
                return Ok(image);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn drive_camera(
    renderer: &MapRenderer,
    mut request: MapRequest,
    source: &Arc<RasterSource>,
    mut previous: Arc<RenderImage>,
) -> Result<(usize, usize, Duration)> {
    let frames = renderer.frames();
    let started = Instant::now();
    let mut inputs = 0;
    let mut delivered = 0;
    while started.elapsed() < Duration::from_secs(2) {
        request.camera.pan(0.5, 0.25);
        renderer.render_raster(request, Some(source.clone()), 1.0, true, BasemapStyle::Dark, false)?;
        inputs += 1;
        delivered += drain_images(&frames, &mut previous)?;
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok((inputs, delivered, started.elapsed()))
}

fn drain_images(frames: &async_channel::Receiver<MapFrame>, previous: &mut Arc<RenderImage>) -> Result<usize> {
    let mut delivered = 0;
    while let Ok(frame) = frames.try_recv() {
        let image = frame.image?;
        delivered += usize::from(!Arc::ptr_eq(previous, &image));
        *previous = image;
    }
    Ok(delivered)
}
