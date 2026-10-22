use std::cell::{Cell, RefCell};
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
use crate::raster_layer::{RasterLayer, RasterLayerHost};
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
    pub raster_layer_id: Option<tiler::LayerId>,
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
            raster_layer_id: current_request.raster.as_ref().map(|source| source.metadata().id),
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
    camera: Option<MapCamera>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    idle: Rc<Cell<bool>>,
    needs_frame: Rc<Cell<bool>>,
    readback: GpuReadback,
    deadline: Instant,
    raster: Option<Arc<RasterSource>>,
    opacity: f32,
    numeric: Rc<RefCell<RasterLayer>>,
    #[cfg(test)]
    camera_updates: u64,
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
        let numeric = Rc::new(RefCell::new(RasterLayer::new(device.clone(), queue.clone())?));
        Ok(Self {
            renderer,
            viewport,
            camera: None,
            readback: GpuReadback::new(&device),
            device,
            queue,
            idle,
            needs_frame,
            deadline: Instant::now() + Duration::from_secs(30),
            raster: None,
            opacity: 1.0,
            numeric,
            #[cfg(test)]
            camera_updates: 0,
        })
    }

    fn set_raster(&mut self, raster: Option<Arc<RasterSource>>, opacity: f32) -> Result<()> {
        let style_changed = self.raster.as_ref().map(|source| source.id()) != raster.as_ref().map(|source| source.id());
        if !style_changed && self.opacity == opacity {
            return Ok(());
        }
        let changed = self.raster.as_ref().map(|source| source.metadata().id) != raster.as_ref().map(|source| source.metadata().id);
        let dpi = (self.viewport.scale.ceil() as u8).clamp(1, 4);
        let numeric_source = raster.as_ref().filter(|source| source.is_numeric()).cloned();
        if let Some(source) = raster.as_ref().filter(|source| !source.is_numeric()) {
            RasterTiles::init()?.register(source)?;
        }
        if changed {
            self.renderer.style().remove_layer("local-raster-layer");
            self.renderer.style().remove_source("local-raster");
            if let Some(source) = &raster {
                if source.is_numeric() {
                    self.renderer
                        .style()
                        .add_wgpu_custom_layer("local-raster-layer", RasterLayerHost::new(self.numeric.clone()), None)?;
                } else {
                    self.renderer
                        .style()
                        .add_source(AnySource::from_json_value("local-raster", &source.source_json(dpi))?)?;
                    self.renderer.style().add_layer(AnyLayer::from_json_value(&source.layer_json(opacity))?)?;
                }
            }
        } else if let Some(source) = raster.as_ref().filter(|source| !source.is_numeric()) {
            self.renderer.style().remove_layer("local-raster-layer");
            self.renderer.style().add_layer(AnyLayer::from_json_value(&source.layer_json(opacity))?)?;
        }
        self.numeric.borrow_mut().set_source(numeric_source, opacity, dpi);
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
        while self.needs_frame.get() || !self.idle.get() || self.numeric.borrow().is_loading() {
            if let Some(texture) = self.poll_frame()? {
                latest = Some(texture)
            }
            if !self.idle.get() || self.numeric.borrow().is_loading() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        latest.context("MapLibre did not produce a wgpu texture")
    }

    fn apply_request(&mut self, request: MapRequest) {
        if self.viewport != request.viewport || self.camera != Some(request.camera) {
            self.idle.set(false);
            if self.viewport != request.viewport {
                self.renderer.set_map_size(Size {
                    width: request.viewport.width.get(),
                    height: request.viewport.height.get(),
                });
                self.viewport = request.viewport;
            }
            self.renderer.update_camera(&request.camera.update());
            self.camera = Some(request.camera);
            #[cfg(test)]
            {
                self.camera_updates += 1;
            }
        }
        // Shader-only changes repaint the retained scene without invalidating
        // the native camera, tile layout, or label placement.
        self.needs_frame.set(true);
        self.deadline = Instant::now() + Duration::from_secs(30);
    }

    fn poll_frame(&mut self) -> Result<Option<wgpu::Texture>> {
        ensure!(
            (self.idle.get() && !self.numeric.borrow().is_loading()) || Instant::now() < self.deadline,
            "Map rendering timed out"
        );
        RunLoopHandle::current().tick();
        if self.numeric.borrow_mut().poll_tiles() {
            self.needs_frame.set(true);
        }
        if self.needs_frame.replace(false) {
            self.idle.set(false);
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
        // Check both the geographic footprint and the fixture's nodata strip.
        for x in 0..rendered.width() {
            let longitude = camera.coordinate_at((f64::from(x) / 2.0 - 256.0, 0.0)).longitude;
            if !(-8.0..=10.5).contains(&longitude) {
                assert_eq!(
                    rendered.get_pixel(x, 512),
                    background.get_pixel(x, 512),
                    "Nodata/outside at {longitude} must be transparent"
                );
            } else if longitude > -6.0 && longitude < 9.0 {
                assert_ne!(rendered.get_pixel(x, 512), background.get_pixel(x, 512), "Data at {longitude} must be visible");
            }
        }
        let gray = source.restyled(inf::colormap::ColorMapPreset::Gray, source.range())?;
        let (requests, uploads) = map.numeric.borrow().tile_counts();
        let camera_updates = map.camera_updates;
        assert!(requests > 0 && uploads > 0, "The custom layer must load and upload numeric tiles");
        // The following restyles must work even if the data is no longer readable.
        std::fs::rename(&path, path.with_extension("cached.tif"))?;
        map.set_raster(Some(gray.clone()), 1.0)?;
        let restyled = map.render(request)?;
        assert_ne!(rendered.as_raw(), restyled.as_raw(), "A palette change must recolor cached numeric tiles");
        let range = gray.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 200.0))?;
        map.set_raster(Some(range), 1.0)?;
        let rescaled = map.render(request)?;
        assert_ne!(restyled.as_raw(), rescaled.as_raw(), "A range change must update the shader mapping");
        assert_eq!(
            map.numeric.borrow().tile_counts(),
            (requests, uploads),
            "Restyling must not request or upload tile data"
        );
        assert_eq!(map.camera_updates, camera_updates, "Restyling must not reset the native camera");
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
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn custom_raster_respects_native_layer_order_and_band_changes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bands.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2)?;
        let sources = RasterSource::open(&path)?;
        let viewport = Viewport {
            width: NonZeroU32::new(256).unwrap(),
            height: NonZeroU32::new(256).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(sources[0].metadata().bounds, 256.0, 256.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        let first = sources[0].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 160.0))?;
        let second = sources[1].restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 160.0))?;
        map.set_raster(Some(first.clone()), 1.0)?;
        let first_image = map.render(request)?;
        let requests = map.numeric.borrow().tile_counts().0;
        map.set_raster(Some(second), 1.0)?;
        let second_image = map.render(request)?;
        assert_ne!(first_image.as_raw(), second_image.as_raw(), "A different band must load different values");
        assert!(map.numeric.borrow().tile_counts().0 > requests, "Band changes need new raw tiles");

        // A native opaque layer above the custom layer must cover it even when
        // MapLibre draws opaque geometry before its translucent/custom pass.
        map.renderer.style().add_source(AnySource::from_json_str(
            "cover",
            r##"{"type":"geojson","data":{"type":"Polygon","coordinates":[[[-20,-20],[20,-20],[20,20],[-20,20],[-20,-20]]]}}"##,
        )?)?;
        map.renderer.style().add_layer(AnyLayer::from_json_str(
            r##"{"id":"cover","type":"fill","source":"cover","paint":{"fill-color":"#00ff00","fill-antialias":false}}"##,
        )?)?;
        let covered = map.render(request)?;
        assert_eq!(*covered.get_pixel(128, 128), image::Rgba([0, 255, 0, 255]));

        // The same host API supports explicit insertion before a native layer.
        map.set_raster(None, 1.0)?;
        map.numeric.borrow_mut().set_source(Some(first), 1.0, 1);
        map.renderer
            .style()
            .add_wgpu_custom_layer("below-cover", RasterLayerHost::new(map.numeric.clone()), Some("cover"))?;
        let below = map.render(request)?;
        assert_eq!(*below.get_pixel(128, 128), image::Rgba([0, 255, 0, 255]));
        map.renderer.style().remove_layer("cover");
        map.renderer.style().add_layer(AnyLayer::from_json_str(
            r##"{"id":"cover","type":"fill","source":"cover","paint":{"fill-color":"#00ff00","fill-opacity":0.5,"fill-antialias":false}}"##,
        )?)?;
        let blended = map.render(request)?;
        let raster_pixel = first_image.get_pixel(128, 128);
        let expected = [raster_pixel[0] / 2, 128 + raster_pixel[1] / 2, raster_pixel[2] / 2, 255];
        for (actual, expected) in blended.get_pixel(128, 128).0.into_iter().zip(expected) {
            assert!(
                actual.abs_diff(expected) <= 1,
                "Native translucent layers must still draw correctly after the custom callback"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device; uses only local raster data"]
    fn byte_values_render_opaque_including_173_and_174() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("byte-values.tif");
        crate::raster_source::test_support::write_byte_raster(&path)?;
        let source = RasterSource::open(&path)?
            .remove(0)
            .restyled(inf::colormap::ColorMapPreset::Terrain, (0.0, 249.0))?;
        let viewport = Viewport {
            width: NonZeroU32::new(512).unwrap(),
            height: NonZeroU32::new(512).unwrap(),
            scale: 1.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.metadata().bounds, 512.0, 512.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(
                r##"{"version":8,"sources":{"water":{"type":"geojson","data":{"type":"Polygon","coordinates":[[[60,-20],[70,-20],[70,20],[60,20],[60,-20]]]}}},"layers":[{"id":"background","type":"background","paint":{"background-color":"#ff00ff"}},{"id":"water","type":"fill","source":"water","paint":{"fill-color":"#102030","fill-antialias":false}}]}"##,
            ),
        )?;
        map.set_raster(Some(source.clone()), 1.0)?;
        let image = map.render(request)?;
        for value in 0..255 {
            let color = source.legend().color_for_value(value, None);
            let expected = image::Rgba([color.b, color.g, color.r, 255]);
            assert_eq!(
                *image.get_pixel(value * 2 + 1, 256),
                expected,
                "Byte {value} must be opaque and match the legend"
            );
        }
        assert_eq!(
            *image.get_pixel(511, 256),
            image::Rgba([255, 0, 255, 255]),
            "Only nodata should show the background"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and vector basemap access; uses the landuse fixture"]
    fn landuse_water_values_render_opaque_over_the_basemap() -> Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("infra-rs/tests/data/landusebyte.tif");
        let source = RasterSource::open(&path)?
            .remove(0)
            .restyled(inf::colormap::ColorMapPreset::Terrain, (0.0, 249.0))?;
        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 2.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds([4.20, 51.10, 4.45, 51.23], 1200.0, 760.0),
        };
        let mut map = NativeMap::new(viewport)?;
        let background = map.render(request)?;
        map.set_raster(Some(source.clone()), 1.0)?;
        let rendered = map.render(request)?;
        let mut tiles = std::collections::HashMap::new();
        let mut checked = 0;
        for y in (0..rendered.height()).step_by(8) {
            for x in (0..rendered.width()).step_by(8) {
                let coordinate = request.camera.coordinate_at((f64::from(x) / 2.0 - 600.0, f64::from(y) / 2.0 - 380.0));
                let tile = geo::Tile::for_coordinate(coordinate, source.metadata().max_zoom);
                if let std::collections::hash_map::Entry::Vacant(entry) = tiles.entry(tile) {
                    entry.insert(source.numeric_tile(tile, 2)?.context("Missing landuse tile")?);
                }
                let data = &tiles[&tile];
                let grid = geo::GeoReference::from_tile(&tile, data.width as usize, 1);
                let cell = grid.point_to_cell(geo::crs::lat_lon_to_web_mercator(coordinate));
                let value = data.values[(cell.row as u32 * data.width + cell.col as u32) as usize];
                if value == 173.0 || value == 174.0 {
                    checked += 1;
                    assert_ne!(
                        rendered.get_pixel(x, y),
                        background.get_pixel(x, y),
                        "Landuse {value} at {coordinate:?} must not reveal the basemap"
                    );
                }
            }
        }
        assert!(checked > 100, "The test must exercise the reported water values, checked {checked}");
        Ok(())
    }

    #[test]
    #[ignore = "manual GPU performance measurement; uses only local raster data"]
    fn raster_style_frame_timings() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 2.0,
        };
        let request = MapRequest {
            viewport,
            camera: MapCamera::fit_bounds(source.metadata().bounds, 1200.0, 760.0),
        };
        let mut map = NativeMap::with_style(
            viewport,
            Some(r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#202020"}}]}"##),
        )?;
        map.set_raster(Some(source.clone()), 1.0)?;
        map.render(request)?;
        let counts = map.numeric.borrow().tile_counts();
        let camera_updates = map.camera_updates;
        let mut styling = Duration::ZERO;
        let mut rendering = Duration::ZERO;
        let mut readback = Duration::ZERO;
        for ix in 0..30 {
            let start = Instant::now();
            let source = source.restyled(source.palette(), (ix as f32, 160.0))?;
            map.set_raster(Some(source), 1.0)?;
            let styled = Instant::now();
            // Follow the production path: publish the first available frame,
            // rather than waiting for all native fades to become idle.
            map.apply_request(request);
            let texture = map.poll_frame()?.context("Range update did not produce a frame")?;
            let rendered = Instant::now();
            let _image = map.readback.read(&texture, &map.device, &map.queue)?;
            styling += styled - start;
            rendering += rendered - styled;
            readback += rendered.elapsed();
        }
        eprintln!(
            "Range update at 2400×1520: style {:?}, render {:?}, GPU wait/readback {:?} per frame",
            styling / 30,
            rendering / 30,
            readback / 30
        );
        assert_eq!(map.numeric.borrow().tile_counts(), counts);
        assert_eq!(map.camera_updates, camera_updates);
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
