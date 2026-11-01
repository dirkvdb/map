//! An isolated bridge to the pinned MapLibre Native `CustomLayerHost` API.
//!
//! Build the official FFI with `cmake/inject.cmake` and the isolated raster
//! parameter core patch in `patches/`; missing patches fail native compilation.
//! An unmodified upstream artifact cannot serve this crate. See the crate README.
//! Callbacks must not reenter/wait on the map or graphics driver. `wait` belongs
//! on a thread that does not drive native callbacks/runtime progress.
#![deny(missing_docs)]

mod callbacks;
mod parameters;
mod pending;

pub use maplibre_native_ffi::{MapHandle, sys};
pub use parameters::*;
pub use pending::{Completion, Pending};

use std::sync::{Arc, Mutex};

/// C callback/frame ABI version; raster projection and depth fields require version 2.
pub const ABI_VERSION: u32 = 2;

/// Bridge submission, completion, or lifecycle failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A layer ID was empty or contained a NUL byte.
    #[error("{0} must be nonempty and contain no NUL bytes")]
    InvalidId(&'static str),
    /// Native rejection or terminal command failure (including cancellation).
    #[error("native status {status}, disposition {disposition:?}: {diagnostic}")]
    Native {
        /// Raw `mln_status`.
        status: i32,
        /// Raw terminal disposition, or None for synchronous rejection.
        disposition: Option<u32>,
        /// Owned native diagnostic, copied before the callback returns.
        diagnostic: String,
    },
    /// The native callback owner was released without delivering a result.
    #[error("native completion was released without a terminal result")]
    CompletionLost,
    /// A bounded wait elapsed; the pending operation is still live.
    #[error("timed out waiting for native completion")]
    Timeout,
    /// A host method reported a failure.
    #[error("custom layer host: {0}")]
    Host(String),
    /// A host method or destructor panicked; the panic never crosses C.
    #[error("custom layer panicked in {0}")]
    CallbackPanic(&'static str),
}

/// A bridge operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// A monitor independent of the insertion operation and native host lifetime.
///
/// Stores the first lifecycle error, including a final destructor panic, even
/// after `Pending::wait` consumes the operation. Clone before consuming Pending.
#[derive(Clone, Debug, Default)]
pub struct LayerErrors(Arc<Mutex<Option<Error>>>);

impl LayerErrors {
    /// Returns the first recorded lifecycle failure, without clearing it.
    pub fn first_error(&self) -> Option<Error> {
        lock(&self.0).clone()
    }

    pub(crate) fn record(&self, error: Error) {
        let mut first = lock(&self.0);
        if first.is_none() {
            *first = Some(error);
        }
    }
}

/// Native custom-layer implementation owned until native's final host release.
///
/// Calls are serialized, but may occur on different native threads; hence Send,
/// not Sync. Native decides initialization/context-loss/deinitialization order:
/// cleanup may occur without initialization, and context loss does not imply a
/// deinitialize call. Cleanup should tolerate partial initialization. `Drop`
/// releases CPU ownership only; it must not assume an active graphics context.
///
/// After any error/panic, initialize/pre-render/render are suppressed. Cleanup
/// callbacks still run. This cannot catch aborting panics or GPU/API misuse.
pub trait NativeCustomLayer: Send + 'static {
    /// Prepare backend resources. Metal has no device at this stage.
    fn initialize(&mut self, _graphics: &GraphicsContext) -> Result<()> {
        Ok(())
    }

    /// Prepare the frame before native layers render (outside the native pass).
    fn pre_render(&mut self, _parameters: &FrameParameters) -> Result<()> {
        Ok(())
    }

    /// Record rendering into the active native pass, using borrowed handles.
    fn render(&mut self, parameters: &FrameParameters) -> Result<()>;

    /// Abandon resources whose context was destroyed; do not use lost handles.
    fn context_lost(&mut self) -> Result<()> {
        Ok(())
    }

    /// Release graphics resources with native's context active, if available.
    fn deinitialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Enqueue a native custom layer, optionally before an existing style layer.
///
/// Uses the public `MapHandle::id` and `sys::mln_map` constructor. Native validates
/// that handle and holds its own command lease. No high-level private state is
/// accessed. IDs are copied before returning; the host is moved into native on
/// acceptance and dropped on rejection. Dropping Pending does not cancel work.
///
/// # Errors
/// Returns synchronous validation/submission failures here; queued failures
/// (duplicate ID, missing `before`, closing map, cancellation) arrive via Pending.
/// Successful insertion says nothing about later rendering; use its error monitor.
pub fn add_custom_layer(map: &MapHandle, id: &str, host: impl NativeCustomLayer, before: Option<&str>) -> Result<Pending> {
    validate_id(id, "id")?;
    if let Some(before) = before {
        validate_id(before, "before")?;
    }
    callbacks::submit(sys::mln_map(map.id()), id, host, before)
}

fn validate_id(value: &str, field: &'static str) -> Result<()> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return Err(Error::InvalidId(field));
    }
    Ok(())
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests;
