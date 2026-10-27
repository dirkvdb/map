use super::*;

fn image() -> Arc<RenderImage> {
    Arc::new(RenderImage::new(vec![image::Frame::new(RgbaImage::new(1, 1))]))
}

#[test]
fn first_readback_does_not_establish_readiness() {
    let mut progress = FrameProgress::default();
    let first = image();
    let (published, settled) = progress.update(Some(first.clone()), false).unwrap();
    assert!(Arc::ptr_eq(&published, &first));
    assert!(!settled);
    assert!(progress.update(None, false).is_none());
}

#[test]
fn idle_without_a_texture_republishes_only_the_latest_image_once() {
    let mut progress = FrameProgress::default();
    let first = image();
    let latest = image();
    progress.update(Some(first.clone()), false);
    progress.update(Some(latest.clone()), false);
    assert_eq!(Arc::strong_count(&first), 1, "Do not retain obsolete images");

    let (published, settled) = progress.update(None, true).unwrap();
    assert!(settled);
    assert!(Arc::ptr_eq(&published, &latest), "Completion must share pixels, not copy or read back again");
    assert!(progress.update(None, true).is_none(), "Idle polling must not repeatedly publish completion");
}

#[test]
fn an_already_idle_readback_is_published_settled_without_a_duplicate_completion() {
    let mut progress = FrameProgress::default();
    let (_, settled) = progress.update(Some(image()), true).unwrap();
    assert!(settled);
    assert!(progress.update(None, true).is_none());
}

#[test]
fn request_reset_drops_the_old_image_and_requires_a_current_request_readback() {
    let mut progress = FrameProgress::default();
    let old = image();
    progress.update(Some(old.clone()), true);
    progress.reset();
    assert_eq!(Arc::strong_count(&old), 1);
    assert!(!progress.settled);
    assert!(progress.update(None, true).is_none(), "An old idle callback cannot settle a new request");

    let current = image();
    let (_, settled) = progress.update(Some(current.clone()), false).unwrap();
    assert!(!settled);
    let (published, settled) = progress.update(None, true).unwrap();
    assert!(settled);
    assert!(Arc::ptr_eq(&published, &current));
}

#[test]
fn native_activity_after_idle_requires_another_idle_transition() {
    let mut progress = FrameProgress::default();
    progress.update(Some(image()), true);
    assert!(progress.update(None, false).is_none());
    assert!(progress.update(None, true).unwrap().1);
    assert!(progress.update(None, true).is_none());
}

fn receive_frame(frames: &async_channel::Receiver<MapFrame>, deadline: Instant) -> Result<MapFrame> {
    loop {
        ensure!(Instant::now() < deadline, "Local renderer readiness frame timed out");
        match frames.try_recv() {
            Ok(frame) => return Ok(frame),
            Err(async_channel::TryRecvError::Closed) => anyhow::bail!("Local renderer stopped before delivering readiness"),
            Err(async_channel::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; controlled local geometry and style, no network"]
fn local_worker_keeps_background_only_vector_frames_unsettled_and_delivers_settled_geometry() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("readiness.geojson");
    // Enough native GeoJSON indexing/tile work to expose the early background
    // frame, with bounded local geometry entirely inside the fitted viewport.
    let features: Vec<_> = (0..62_500)
        .map(|index| {
            let x = f64::from(index % 250) / 25.0 - 5.0;
            let y = f64::from(index / 250) / 25.0 - 5.0;
            serde_json::json!({
                "type": "Feature", "properties": {},
                "geometry": { "type": "Point", "coordinates": [x, y] }
            })
        })
        .collect();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"type": "FeatureCollection", "features": features}))?,
    )?;
    let source = VectorSource::open(&path)?.context("Expected local vector data")?.remove(0);
    let viewport = Viewport {
        width: NonZeroU32::new(256).unwrap(),
        height: NonZeroU32::new(256).unwrap(),
        scale: 1.0,
    };
    let request = MapRequest {
        viewport,
        camera: MapCamera::fit_bounds(source.bounds(), 256.0, 256.0),
    };
    let renderer = MapRenderer::with_test_style(tests::DARK_STYLE)?;
    let frames = renderer.frames();
    renderer.render(request)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let background = loop {
        let frame = receive_frame(&frames, deadline)?;
        let image = frame.image?;
        if frame.settled {
            break image;
        }
    };
    assert!(background.as_bytes(0).unwrap().chunks_exact(4).all(|pixel| pixel == [32, 32, 32, 255]));

    renderer.render_layer(
        request,
        Some(MapSource::Vector(source.clone(), "#3b82f6".into())),
        1.0,
        false,
        BasemapStyle::Dark,
        false,
    )?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_background_only = false;
    loop {
        let frame = receive_frame(&frames, deadline)?;
        if frame.vector_layer_id != Some(source.id()) {
            continue;
        }
        let image = frame.image?;
        let background_only = image.as_bytes(0) == background.as_bytes(0);
        if background_only {
            saw_background_only = true;
            assert!(!frame.settled, "A source-tagged background-only readback must not complete loading");
        }
        if frame.settled {
            assert!(saw_background_only, "Fixture must exercise an early background-only frame");
            assert!(!background_only, "The settled frame must contain actual geometry pixels, not just a source ID");
            // Coalescing may hide the last unsettled image, and native may render
            // a new settled image. Shared-Arc idle-only completion is unit-tested.
            break;
        }
    }

    // Continued idle polling should neither repaint nor send repeated completion.
    std::thread::sleep(Duration::from_millis(80));
    assert!(frames.try_recv().is_err());
    drop(renderer);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !frames.is_closed() {
        ensure!(Instant::now() < deadline, "Local renderer did not stop");
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device; local style, no network"]
fn settled_native_map_ignores_expired_deadline_but_pending_work_still_times_out() -> Result<()> {
    let viewport = Viewport {
        width: NonZeroU32::new(32).unwrap(),
        height: NonZeroU32::new(24).unwrap(),
        scale: 1.0,
    };
    let mut map = NativeMap::with_style(viewport, Some(tests::DARK_STYLE))?;
    map.render(MapRequest {
        viewport,
        camera: MapCamera::default(),
    })?;
    assert!(map.is_settled());
    // Test deadline handling after real native settlement, not the number of
    // callback/render turns needed to get there. Freeze callbacks so tick cannot
    // race the explicit pending-work flags below.
    map.renderer.map_observer().set_did_become_idle_callback(|| {});
    map.renderer.set_render_requested_callback(|| {});
    let deadline = Instant::now();
    map.deadline = deadline;
    assert!(map.poll_frame()?.is_none());
    assert!(map.is_settled());
    assert_eq!(map.deadline, deadline, "Idle polling must not extend the deadline");

    map.idle.set(false);
    assert_eq!(
        map.poll_frame().err().context("Stalled work must time out")?.to_string(),
        "Map rendering timed out"
    );
    assert_eq!(map.deadline, deadline, "Stalled work must not extend the deadline");
    // Native idle must not exempt a still-pending repaint from the deadline.
    map.idle.set(true);
    map.needs_frame.set(true);
    assert_eq!(
        map.poll_frame().err().context("Pending repaint must time out")?.to_string(),
        "Map rendering timed out"
    );
    assert!(map.needs_frame.get(), "Expired work must fail before rendering");
    assert_eq!(map.deadline, deadline, "Pending repaint must not extend the deadline");
    Ok(())
}
