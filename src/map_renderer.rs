use std::cell::Cell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use gpui_kit::RenderImage;
#[cfg(test)]
use image::RgbaImage;
use maplibre_native::{AnyLayer, AnySource, Continuous, ImageRenderer, ImageRendererBuilder, RunLoopHandle, Size};

use crate::map_camera::MapCamera;
use crate::map_readback::GpuReadback;
use crate::raster_source::{RasterSource, RasterTiles};

pub const STYLE_URL: &str = "https://basemaps.cartocdn.com/gl/dark-matter-gl-style/style.json";

#[derive(Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub scale: f32,
}

#[derive(Clone, Copy, PartialEq)]
pub struct MapRequest {
    pub viewport: Viewport,
    pub camera: MapCamera,
}

pub struct MapFrame {
    pub viewport: Viewport,
    pub raster_id: Option<u64>,
    pub image: Result<Arc<RenderImage>>,
}

#[derive(Clone)]
struct RenderRequest {
    map: MapRequest,
    raster: Option<Arc<RasterSource>>,
    opacity: f32,
}

pub struct MapRenderer {
    sender: mpsc::Sender<RenderRequest>,
    frames: async_channel::Receiver<MapFrame>,
}

impl MapRenderer {
    pub fn new() -> Result<Self> {
        RasterTiles::init()?;
        let (sender, receiver) = mpsc::channel::<RenderRequest>();
        let (frame_sender, frames) = async_channel::bounded(1);
        let pending_frame = frames.clone();
        // The !Send renderer and its run loop remain on this dedicated thread.
        // Dropping the view closes the channel; the worker finishes its bounded
        // in-flight request, drops all native resources, and exits.
        std::thread::Builder::new()
            .name("map-renderer".into())
            .spawn(move || run_renderer(receiver, frame_sender, pending_frame))
            .context("Could not start the map rendering thread")?;
        Ok(Self { sender, frames })
    }

    pub fn render_raster(&self, map: MapRequest, raster: Option<Arc<RasterSource>>, opacity: f32) -> Result<()> {
        self.sender
            .send(RenderRequest { map, raster, opacity })
            .context("Map rendering thread stopped")?;
        Ok(())
    }

    #[cfg(test)]
    pub fn render(&self, request: MapRequest) -> Result<()> {
        self.render_raster(request, None, 1.0)
    }

    pub fn frames(&self) -> async_channel::Receiver<MapFrame> {
        self.frames.clone()
    }
}

fn run_renderer(requests: mpsc::Receiver<RenderRequest>, frames: async_channel::Sender<MapFrame>, pending_frame: async_channel::Receiver<MapFrame>) {
    let mut map = None;
    let mut current_request = None;
    let mut next_frame = Instant::now();

    loop {
        let timeout = map.as_ref().map(|_| next_frame.saturating_duration_since(Instant::now()));
        let Ok(request) = receive_latest_request(&requests, timeout) else {
            break;
        };
        let request_changed = request.is_some();
        if let Some(request) = request {
            current_request = Some(request);
        }
        let Some(current_request) = &current_request else { continue };

        let frame_started = Instant::now();
        let image = match poll_map_image(&mut map, current_request, request_changed, next_frame) {
            Ok(Some(image)) => Ok(image),
            Ok(None) => {
                // Poll native resource/fade callbacks without busy-waiting.
                if Instant::now() >= next_frame {
                    next_frame = Instant::now() + Duration::from_millis(16);
                }
                continue;
            }
            Err(error) => {
                map = None;
                Err(error)
            }
        };
        let frame = MapFrame {
            viewport: current_request.map.viewport,
            raster_id: current_request.raster.as_ref().map(|source| source.id()),
            image,
        };
        if let Err(async_channel::TrySendError::Full(frame)) = frames.try_send(frame) {
            // Keep one newest completed frame if GPUI is still busy.
            let _ = pending_frame.try_recv();
            let _ = frames.try_send(frame);
        }
        next_frame = frame_started + Duration::from_millis(16);
    }
}

fn receive_latest_request(requests: &mpsc::Receiver<RenderRequest>, timeout: Option<Duration>) -> Result<Option<RenderRequest>, mpsc::RecvTimeoutError> {
    let incoming = match timeout {
        Some(timeout) => requests.recv_timeout(timeout),
        None => requests.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
    };
    let mut request = match incoming {
        Ok(request) => Some(request),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(error) => return Err(error),
    };
    for latest in requests.try_iter() {
        request = Some(latest);
    }
    Ok(request)
}

fn poll_map_image(map: &mut Option<NativeMap>, request: &RenderRequest, mut request_changed: bool, next_frame: Instant) -> Result<Option<Arc<RenderImage>>> {
    if map.as_ref().is_none_or(|map| map.viewport.scale != request.map.viewport.scale) {
        *map = Some(NativeMap::new(request.map.viewport)?);
        request_changed = true;
    }
    let map = map.as_mut().context("Map renderer unavailable")?;
    if request_changed {
        map.set_raster(request.raster.clone(), request.opacity)?;
        map.apply_request(request.map);
    }
    if Instant::now() < next_frame {
        return Ok(None);
    }
    let Some(texture) = map.poll_frame()? else { return Ok(None) };
    let pixels = map.readback.read(&texture, &map.device, &map.queue)?;
    Ok(Some(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]))))
}

struct NativeMap {
    renderer: ImageRenderer<Continuous>,
    viewport: Viewport,
    device: wgpu::Device,
    queue: wgpu::Queue,
    idle: Rc<Cell<bool>>,
    needs_frame: Rc<Cell<bool>>,
    readback: GpuReadback,
    deadline: Instant,
    raster: Option<Arc<RasterSource>>,
    opacity: f32,
}

impl NativeMap {
    fn new(viewport: Viewport) -> Result<Self> {
        Self::with_style(viewport, None)
    }

    fn with_style(viewport: Viewport, json: Option<&str>) -> Result<Self> {
        RasterTiles::init()?;
        // The wgpu FFI backend requires a host-supplied device and queue. This
        // initialization can later accept GPUI's device instead of creating one.
        let (device, queue) = futures_lite::future::block_on(async {
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
                .request_adapter(&wgpu::RequestAdapterOptions::default())
                .await
                .context("Could not request wgpu adapter")?
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("MapLibre device"),
                    ..Default::default()
                })
                .await
                .context("Could not create the MapLibre wgpu device")
        })?;
        let mut renderer = ImageRendererBuilder::new()
            .with_size(viewport.width, viewport.height)
            .with_pixel_ratio(viewport.scale)
            .build_continuous_renderer();
        renderer.set_device_queue(device.clone(), queue.clone());
        let idle = Rc::new(Cell::new(false));
        renderer.map_observer().set_did_become_idle_callback({
            let idle = idle.clone();
            move || idle.set(true)
        });
        let needs_frame = Rc::new(Cell::new(true));
        renderer.set_render_requested_callback({
            let needs_frame = needs_frame.clone();
            move || needs_frame.set(true)
        });
        let run_loop = RunLoopHandle::current();
        let style = match json {
            Some(json) => renderer.load_style_from_json_str(json),
            None => renderer.load_style_from_url(&STYLE_URL.parse()?),
        };
        wait_until_ready(&run_loop, Instant::now() + Duration::from_secs(30), || style.is_ready())?;
        style.finish().context("Could not load the vector map style")?;
        Ok(Self {
            renderer,
            viewport,
            readback: GpuReadback::new(&device),
            device,
            queue,
            idle,
            needs_frame,
            deadline: Instant::now() + Duration::from_secs(30),
            raster: None,
            opacity: 1.0,
        })
    }

    fn set_raster(&mut self, raster: Option<Arc<RasterSource>>, opacity: f32) -> Result<()> {
        let changed = self.raster.as_ref().map(|source| source.id()) != raster.as_ref().map(|source| source.id());
        if !changed && self.opacity == opacity {
            return Ok(());
        }
        self.renderer.style().remove_layer("local-raster-layer");
        if changed {
            self.renderer.style().remove_source("local-raster");
            if let Some(source) = &raster {
                RasterTiles::init()?.register(source)?;
                let dpi = (self.viewport.scale.ceil() as u8).clamp(1, 4);
                self.renderer
                    .style()
                    .add_source(AnySource::from_json_value("local-raster", &source.source_json(dpi))?)?;
            }
        }
        if let Some(source) = &raster {
            self.renderer.style().add_layer(AnyLayer::from_json_value(&source.layer_json(opacity))?)?;
        }
        self.raster = raster;
        self.opacity = opacity;
        Ok(())
    }

    #[cfg(test)]
    fn render(&mut self, request: MapRequest) -> Result<RgbaImage> {
        let texture = self.render_texture(request)?;
        self.readback.read(&texture, &self.device, &self.queue)
    }

    #[cfg(test)]
    fn render_texture(&mut self, request: MapRequest) -> Result<wgpu::Texture> {
        self.apply_request(request);
        let mut latest = None;
        while !self.idle.get() {
            if let Some(texture) = self.poll_frame()? {
                latest = Some(texture)
            }
            if !self.idle.get() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        latest.context("MapLibre did not produce a wgpu texture")
    }

    fn apply_request(&mut self, request: MapRequest) {
        self.idle.set(false);
        if self.viewport != request.viewport {
            self.renderer.set_map_size(Size {
                width: request.viewport.width.get(),
                height: request.viewport.height.get(),
            });
            self.viewport = request.viewport;
        }
        self.renderer.update_camera(&request.camera.update());
        self.needs_frame.set(true);
        self.deadline = Instant::now() + Duration::from_secs(30);
    }

    fn poll_frame(&mut self) -> Result<Option<wgpu::Texture>> {
        ensure!(self.idle.get() || Instant::now() < self.deadline, "Map rendering timed out");
        RunLoopHandle::current().tick();
        if self.needs_frame.replace(false) {
            self.renderer.render_once();
            return self.renderer.take_texture().map(Some).context("MapLibre did not produce a wgpu texture");
        }
        Ok(None)
    }
}

fn wait_until_ready(run_loop: &RunLoopHandle, deadline: Instant, is_ready: impl Fn() -> bool) -> Result<()> {
    while !is_ready() {
        ensure!(Instant::now() < deadline, "Map loading timed out. Check your connection and retry.");
        run_loop.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn local_raster_tiles_render_restyle_and_clear_without_network() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 2.0,
        };
        let camera = MapCamera::fit_bounds(source.metadata().bounds, 512.0, 512.0);
        let request = MapRequest { viewport, camera };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        let background = map.render(request)?;
        map.set_raster(Some(source.clone()), 1.0)?;
        let rendered = map.render(request)?;
        assert_eq!(rendered.dimensions(), (1024, 1024));
        let changed_pixels = rendered.pixels().zip(background.pixels()).filter(|(a, b)| a != b).count();
        assert!(
            changed_pixels > 100_000,
            "Raster tiles must visibly cover their geographic footprint, got {changed_pixels} pixels"
        );
        let gray = source.restyled(inf::colormap::ColorMapPreset::Gray, source.range())?;
        map.set_raster(Some(gray.clone()), 1.0)?;
        let restyled = map.render(request)?;
        assert_ne!(rendered.as_raw(), restyled.as_raw(), "A palette change must replace cached raster tiles");
        map.set_raster(Some(gray), 0.0)?;
        assert_eq!(map.render(request)?.as_raw(), background.as_raw(), "Zero opacity must reveal the basemap");
        map.set_raster(None, 1.0)?;
        assert_eq!(
            map.render(request)?.as_raw(),
            background.as_raw(),
            "Clearing the raster must remove its source and layer"
        );
        Ok(())
    }

    #[test]
    #[ignore = "manual GPU performance measurement; requires internet access"]
    fn navigation_frame_timings() -> Result<()> {
        let _ = env_logger::builder().is_test(true).try_init();

        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 1.5,
        };
        let mut camera = MapCamera::default();
        camera.zoom_at(1.5, (0.0, 0.0));
        let mut map = NativeMap::new(viewport)?;
        map.render(MapRequest { viewport, camera })?;
        let mut rendering = Duration::ZERO;
        let mut readback = Duration::ZERO;
        for _ in 0..12 {
            camera.pan(10.0, 5.0);
            camera.zoom_at(0.01, (0.0, 0.0));
            let start = Instant::now();
            map.apply_request(MapRequest { viewport, camera });
            let texture = loop {
                if let Some(texture) = map.poll_frame()? {
                    break texture;
                }
            };
            let rendered = Instant::now();
            let image = map.readback.read(&texture, &map.device, &map.queue)?;
            rendering += rendered - start;
            readback += rendered.elapsed();
            assert_eq!(image.dimensions(), (1800, 1140));
        }
        log::info!(
            "Average navigation frame: first-frame rendering={:?}, readback/copy={:?}",
            rendering / 12,
            readback / 12
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and internet access"]
    fn streaming_worker_delivers_frames_and_accepts_new_viewports() -> Result<()> {
        let renderer = MapRenderer::new()?;
        let frames = renderer.frames();
        let mut viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        renderer.render(MapRequest {
            viewport,
            camera: MapCamera::default(),
        })?;
        let wait_for_frame = |viewport: Viewport| -> Result<Arc<RenderImage>> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                ensure!(Instant::now() < deadline, "Streaming map frame timed out");
                if let Ok(frame) = frames.try_recv() {
                    let image = frame.image?;
                    if frame.viewport == viewport {
                        let colors: std::collections::HashSet<_> = image.as_bytes(0).unwrap().chunks_exact(4).collect();
                        if colors.len() > 100 {
                            return Ok(image);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let first = wait_for_frame(viewport)?;
        assert_eq!(first.size(0).width.0, 800);
        viewport.width = NonZeroU32::new(1000).unwrap();
        let mut camera = MapCamera::default();
        camera.zoom_at(1.0, (0.0, 0.0));
        renderer.render(MapRequest { viewport, camera })?;
        let resized = wait_for_frame(viewport)?;
        assert_eq!(resized.size(0).width.0, 1000);
        drop(renderer);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !frames.is_closed() {
            ensure!(Instant::now() < deadline, "Map worker did not shut down");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device and internet access to the demo vector tiles"]
    fn vector_basemap_renders_geographic_content() -> Result<()> {
        let viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        let mut map = NativeMap::new(viewport)?;
        let image = map.render(MapRequest {
            viewport,
            camera: MapCamera::default(),
        })?;
        assert_eq!(image.dimensions(), (800, 600));
        let colors: std::collections::HashSet<_> = image.pixels().map(|pixel| pixel.0).collect();
        assert!(colors.len() > 100, "Expected vector features and labels, got {} colors", colors.len());
        assert!(image.pixels().all(|pixel| pixel[3] == 255));
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and internet access"]
    fn retained_renderer_updates_camera_and_viewport() -> Result<()> {
        let mut viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        let mut camera = MapCamera::default();
        let mut map = NativeMap::new(viewport)?;
        let before = map.render(MapRequest { viewport, camera })?;
        camera.pan(150.0, 70.0);
        camera.zoom_at(1.0, (120.0, -80.0));
        let after = map.render(MapRequest { viewport, camera })?;
        assert!(before.as_raw() != after.as_raw(), "Camera changes must change the rendered image");
        viewport.width = NonZeroU32::new(1000).unwrap();
        let resized = map.render(MapRequest { viewport, camera })?;
        assert_eq!(resized.dimensions(), (1000, 600));
        Ok(())
    }
}
