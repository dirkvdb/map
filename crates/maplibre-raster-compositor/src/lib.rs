//! Upload completed wgpu readback and composite inside a native custom-layer pass.
//! See the crate README for projection, pixel packing, and lifecycle constraints.
#![deny(missing_docs)]

mod native;
mod surface;
pub use maplibre_custom_layer::{FrameParameters, GraphicsContext};
use std::{ffi::c_void, marker::PhantomData, ptr::NonNull};
pub use surface::{NativeSurface, Surface};

/// Compositor validation, allocation, backend, or GPU failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Surface dimensions or packing are invalid.
    #[error("invalid surface: {0}")]
    InvalidSurface(&'static str),
    /// Native allocation failed.
    #[error("native compositor allocation failed")]
    Allocation,
    /// Native backend failure with a copied diagnostic.
    #[error("native compositor: {0}")]
    Native(String),
}
/// Result of compositor operations.
pub type Result<T> = std::result::Result<T, Error>;

/// CPU-owned renderer with native resources allocated lazily in `prepare`.
///
/// Calls are serialized and callback-scoped. `Drop` does not touch the driver;
/// call `deinitialize` in the native lifecycle callback for GPU retirement.
#[derive(Default)]
pub struct NativeCompositor {
    handle: Option<NonNull<c_void>>,
    not_sync: PhantomData<std::cell::Cell<()>>,
}
// Native callbacks serialize access and may transfer the host between threads.
// No native object is touched except through exclusive, unsafe callback methods.
unsafe impl Send for NativeCompositor {}

impl NativeCompositor {
    /// Construct a CPU-only compositor; no active native context is required.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate and copy packed color/depth into immutable native GPU resources.
    ///
    /// # Safety
    /// Call only inside this host's `pre_render`, outside the active native pass,
    /// with live, unmodified graphics handles from that callback. Complete wgpu
    /// readback first. Do not reuse this object on another device without cleanup.
    pub unsafe fn prepare(&mut self, parameters: &FrameParameters, surface: Surface) -> Result<()> {
        surface.validate()?;
        let handle = self.handle()?;
        let view = native::SurfaceView::from(&surface);
        // SAFETY: caller guarantees callback handles; validated vectors live until return.
        native::checked(|error, size| unsafe { native::map_compositor_prepare(handle.as_ptr(), parameters, &view, error, size) })
    }

    /// Draw the last prepared surface in the borrowed active native pass.
    ///
    /// # Safety
    /// Call only inside this host's `render`, using this callback's fresh native
    /// handles. The surface/projection must correspond to the current frame.
    /// The compositor neither ends nor submits native command buffers.
    pub unsafe fn render(&mut self, parameters: &FrameParameters) -> Result<()> {
        let handle = self.handle.ok_or_else(|| Error::Native("render before prepare".into()))?;
        // SAFETY: caller guarantees callback-scoped native pass and device.
        native::checked(|error, size| unsafe { native::map_compositor_render(handle.as_ptr(), parameters, error, size) })
    }

    /// Abandon handles after native context loss without touching the GPU driver.
    pub fn context_lost(&mut self) {
        if let Some(handle) = self.handle {
            // SAFETY: this is a live exclusive CPU owner; native abandonment makes no GPU calls.
            unsafe { native::map_compositor_context_lost(handle.as_ptr()) };
        }
    }

    /// Schedule GPU retirement in the native lifecycle callback; idempotent.
    ///
    /// # Safety
    /// Call only in this host's `deinitialize` callback, before context destruction.
    /// Vulkan uses the bridge's active native fence-backed retirement scope.
    pub unsafe fn deinitialize(&mut self) -> Result<()> {
        let Some(handle) = self.handle else {
            return Ok(());
        };
        // SAFETY: caller guarantees the live native retirement scope.
        native::checked(|error, size| unsafe { native::map_compositor_deinitialize(handle.as_ptr(), error, size) })
    }

    fn handle(&mut self) -> Result<NonNull<c_void>> {
        if let Some(handle) = self.handle {
            return Ok(handle);
        }
        // SAFETY: constructs CPU ownership only, without borrowed graphics objects.
        let handle = NonNull::new(unsafe { native::map_compositor_new() }).ok_or(Error::Allocation)?;
        self.handle = Some(handle);
        Ok(handle)
    }
}
impl Drop for NativeCompositor {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: unique ownership; native delete abandons handles without GPU calls.
            unsafe { native::map_compositor_delete(handle.as_ptr()) };
        }
    }
}
