use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Instant;

use maplibre_native_ffi::{LogicalExtent, MapHandle, MapOptions, NativeFuture, RuntimeEvent, RuntimeEventHandler, RuntimeEventType, RuntimeOptions};

use super::*;

const TIMEOUT: Duration = Duration::from_secs(10);
const EMPTY_STYLE: &str = r#"{"version":8,"sources":{},"layers":[]}"#;

fn wait<T>(future: NativeFuture<T>) -> Result<T> {
    future
        .wait(TIMEOUT)
        .map_err(|_| anyhow::anyhow!("Native raster test command timed out"))?
        .map_err(Into::into)
}

struct NativeFixture {
    map: MapHandle,
    _runtime: RuntimeHandle,
    events: mpsc::Receiver<Vec<RuntimeEvent>>,
}

impl NativeFixture {
    fn new(provider: ResourceProvider) -> Result<Self> {
        let runtime = RuntimeHandle::create(&RuntimeOptions::default())?;
        let (sender, events) = mpsc::channel();
        wait(runtime.set_event_handler(RuntimeEventHandler::new(move |batch| {
            let _ = sender.send(batch);
        }))?)?;
        wait(runtime.set_resource_provider(provider)?)?;
        let (map, created) = runtime.create_map(&MapOptions::new(LogicalExtent::new(256, 256, 1.0)))?;
        wait(created)?;
        Ok(Self {
            map,
            _runtime: runtime,
            events,
        })
    }

    fn request(&self, url: &str) -> Result<()> {
        wait(self.map.set_style_url(url)?)?;
        Ok(())
    }

    fn event(&self, wanted: RuntimeEventType) -> Result<RuntimeEvent> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let batch = self.events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            if let Some(event) = batch.into_iter().find(|event| event.r#type == wanted) {
                return Ok(event);
            }
        }
    }
}

fn receive_job(receiver: &async_channel::Receiver<TileJob>) -> Result<TileJob> {
    let deadline = Instant::now() + TIMEOUT;
    while receiver.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(receiver.try_recv()?)
}

fn queued_tiles(capacity: usize) -> (RasterTiles, async_channel::Receiver<TileJob>) {
    let (jobs, receiver) = async_channel::bounded(capacity);
    (RasterTiles { sources: Arc::default(), jobs }, receiver)
}

#[test]
fn image_response_borrows_original_bytes_and_expires_in_one_hour() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = super::tests::open_red_image_mbtiles(&directory.path().join("image.mbtiles"))?;
    let tiles = RasterTiles::init()?;
    tiles.register(&source)?;
    let url = format!("raster://{}/0/0/0@1.png", source.metadata().id);
    let bytes = tile_response(&tiles.sources, &url)?.context("Missing image MBTile")?;
    let before = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_millis() as i64;
    let response = tile_resource_response(Some(&bytes));
    let after = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_millis() as i64;
    assert_eq!(response.status, ResourceResponseStatus::Ok);
    assert_eq!(response.bytes.as_ptr(), bytes.as_ptr());
    assert!((before + 3_600_000..=after + 3_600_000).contains(&response.expires_unix_ms.context("Missing expiry")?));
    Ok(())
}

#[test]
fn missing_tile_response_is_no_content_without_cache_metadata() {
    let response = tile_resource_response(None);
    assert_eq!(response.status, ResourceResponseStatus::NoContent);
    assert_eq!(response.error_reason, ResourceErrorReason::None);
    assert!(response.bytes.is_empty());
    assert!(response.expires_unix_ms.is_none());
}

#[test]
fn runtime_provider_routes_custom_urls_to_owned_requests() -> Result<()> {
    let (tiles, receiver) = queued_tiles(1);
    let fixture = NativeFixture::new(tiles.resource_provider())?;
    let url = "raster://123/0/0/0@1.png";
    fixture.request(url)?;
    let job = receive_job(&receiver)?;
    assert_eq!(job.url, url);
    assert!(!tile_request_cancelled(&job.request));
    complete_tile_request(job.request, &tile_resource_response(Some(EMPTY_STYLE.as_bytes())));
    fixture.event(RuntimeEventType::MapStyleLoaded)?;
    Ok(())
}

fn cancel_request(map: &MapHandle, request: &ResourceRequestHandle) -> Result<()> {
    wait(map.set_style_json(EMPTY_STYLE)?)?;
    // Command commitment can precede cancellation on MapLibre's worker.
    let deadline = Instant::now() + TIMEOUT;
    while !request.is_cancelled()? && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    ensure!(request.is_cancelled()?, "Native request was not cancelled after replacing the style");
    Ok(())
}

#[test]
fn replacing_a_style_cancels_a_queued_native_request() -> Result<()> {
    let (tiles, receiver) = queued_tiles(1);
    let fixture = NativeFixture::new(tiles.resource_provider())?;
    fixture.request("raster://123/0/0/0@1.png")?;
    let job = receive_job(&receiver)?;
    cancel_request(&fixture.map, &job.request)?;
    assert!(tile_request_cancelled(&job.request));
    assert!(
        job.request.complete(&tile_resource_response(None)).is_err(),
        "Cancelled handles must not deliver responses"
    );
    Ok(())
}

fn numeric_source(path: &Path) -> Result<Arc<RasterSource>> {
    super::test_support::write_raster(path, 3857, 1)?;
    Ok(RasterSource::open(path)?.remove(0))
}

#[test]
fn installed_workers_report_numeric_image_requests_to_the_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = numeric_source(&directory.path().join("numeric.tif"))?;
    let tiles = RasterTiles::init()?;
    tiles.register(&source)?;
    let fixture = NativeFixture::new(tiles.resource_provider())?;
    fixture.request(&format!("raster://{}/0/0/0@1.png", source.metadata().id))?;
    let error = fixture.event(RuntimeEventType::MapLoadingFailed)?;
    assert!(error.message.contains("Numeric rasters are rendered by the custom layer"), "{}", error.message);
    assert!(source.errors().try_recv()?.contains("Couldn’t render raster tiles"));
    Ok(())
}

#[test]
fn workers_skip_cancelled_queued_tiles_without_reading_the_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = numeric_source(&directory.path().join("numeric.tif"))?;
    let (tiles, receiver) = queued_tiles(1);
    tiles.register(&source)?;
    let fixture = NativeFixture::new(tiles.resource_provider())?;
    fixture.request(&format!("raster://{}/0/0/0@1.png", source.metadata().id))?;
    let job = receive_job(&receiver)?;
    cancel_request(&fixture.map, &job.request)?;
    tiles.jobs.try_send(job)?;
    tiles.jobs.close();
    run_tile_worker(receiver, tiles.sources);
    assert!(
        source.errors().is_empty(),
        "Cancelled queued jobs must not reach the numeric image rejection path"
    );
    Ok(())
}

#[test]
fn provider_replacement_does_not_consume_outstanding_requests() -> Result<()> {
    let (tiles, receiver) = queued_tiles(1);
    let fixture = NativeFixture::new(tiles.resource_provider())?;
    fixture.request("raster://123/0/0/0@1.png")?;
    let job = receive_job(&receiver)?;
    wait(fixture._runtime.clear_resource_provider()?)?;
    assert!(!tile_request_cancelled(&job.request));
    job.request
        .complete(&tile_resource_response(Some(EMPTY_STYLE.as_bytes())))
        .map_err(|(_, error)| error)?;
    fixture.event(RuntimeEventType::MapStyleLoaded)?;
    Ok(())
}

#[test]
fn full_queue_reports_provider_error_without_blocking_native_commands() -> Result<()> {
    let (tiles, receiver) = queued_tiles(1);
    let first = NativeFixture::new(tiles.resource_provider())?;
    first.request("raster://123/0/0/0@1.png")?;
    let deadline = Instant::now() + TIMEOUT;
    while receiver.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    ensure!(!receiver.is_empty(), "First request never reached the queue");
    let second = NativeFixture::new(tiles.resource_provider())?;
    second.request("raster://456/0/0/0@1.png")?;
    let error = second.event(RuntimeEventType::MapLoadingFailed)?;
    assert!(error.message.contains("Raster tile queue is full or stopped"), "{}", error.message);
    Ok(())
}

#[test]
fn closed_queue_reports_provider_error_without_blocking_native_commands() -> Result<()> {
    let (tiles, receiver) = queued_tiles(1);
    receiver.close();
    let fixture = NativeFixture::new(tiles.resource_provider())?;
    fixture.request("raster://123/0/0/0@1.png")?;
    let error = fixture.event(RuntimeEventType::MapLoadingFailed)?;
    assert!(error.message.contains("Raster tile queue is full or stopped"), "{}", error.message);
    Ok(())
}

#[test]
fn installed_workers_report_invalid_raster_urls_on_each_runtime() -> Result<()> {
    for _ in 0..2 {
        let fixture = NativeFixture::new(RasterTiles::init()?.resource_provider())?;
        RasterTiles::install(&fixture._runtime)?;
        fixture.request("raster://invalid.png")?;
        let error = fixture.event(RuntimeEventType::MapLoadingFailed)?;
        assert!(error.message.contains("Invalid raster tile path"), "{}", error.message);
    }
    Ok(())
}

fn serve_style(listener: TcpListener) -> Result<()> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(TIMEOUT))?;
                let mut request = [0; 4096];
                ensure!(stream.read(&mut request)? > 0, "Native network loader sent an empty request");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{EMPTY_STYLE}",
                    EMPTY_STYLE.len()
                )?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[test]
fn raster_routes_leave_http_resources_to_the_native_network_loader() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/style.json", listener.local_addr()?);
    let server = std::thread::spawn(move || serve_style(listener));
    let (tiles, receiver) = queued_tiles(1);
    let fixture = NativeFixture::new(tiles.resource_provider())?;
    fixture.request(&url)?;
    fixture.event(RuntimeEventType::MapStyleLoaded)?;
    server.join().unwrap()?;
    assert!(receiver.is_empty(), "HTTP basemap resources must not reach the raster queue");
    Ok(())
}
