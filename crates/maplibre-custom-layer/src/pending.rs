use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::{Error, LayerErrors, Result, sys};

/// Successful insertion's map generations, copied from the raw sys completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    /// Snapshot generation after the command ran.
    pub generation: u64,
    /// First render update generation containing the insertion.
    pub render_update_generation: u64,
}

/// An accepted insertion, not a promise that the queued command will succeed.
///
/// Does not borrow the map or require an async runtime. Dropping it only drops
/// the receiver: native still executes/releases the work. Never wait on a
/// native callback/runtime thread that must advance the command being awaited.
pub struct Pending {
    receiver: Receiver<Result<Completion>>,
    errors: LayerErrors,
}

impl Pending {
    /// Clones the lifecycle error monitor, which outlives this pending operation.
    pub fn errors(&self) -> LayerErrors {
        self.errors.clone()
    }

    /// Waits for the actual terminal insertion result, consuming this receiver.
    pub fn wait(self) -> Result<Completion> {
        self.receiver.recv().map_err(|_| Error::CompletionLost)?
    }

    /// Waits for at most `timeout`; on Timeout, this operation can be waited again.
    /// A delivered terminal result is consumed, so do not wait again afterwards.
    pub fn wait_timeout(&self, timeout: Duration) -> Result<Completion> {
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Error::CompletionLost),
        }
    }
}

pub(crate) struct CompletionOwner(Sender<Result<Completion>>);

pub(crate) fn prepare(errors: LayerErrors) -> (Pending, sys::mln_completion) {
    let (sender, receiver) = mpsc::channel();
    let owner = Box::new(CompletionOwner(sender));
    let descriptor = sys::mln_completion {
        size: std::mem::size_of::<sys::mln_completion>() as u32,
        callback: Some(completed),
        user_data: Box::into_raw(owner).cast(),
        release_user_data: Some(release),
        queue: sys::mln_delivery_queue(0),
    };
    (Pending { receiver, errors }, descriptor)
}

unsafe extern "C" fn completed(data: *mut c_void, result: *const sys::mln_completion_result) {
    // SAFETY: native borrows both records through this callback, then releases data.
    let owner = unsafe { &*data.cast::<CompletionOwner>() };
    let result = crate::callbacks::catch_host("completion", || {
        // SAFETY: native's result and diagnostic are borrowed until callback return.
        unsafe { copy_result(result) }
    });
    let _ = owner.0.send(result);
}

unsafe fn copy_result(result: *const sys::mln_completion_result) -> Result<Completion> {
    // SAFETY: completion contract supplies a full result record or null on broken delivery.
    let result = unsafe { result.as_ref() }.ok_or(Error::CompletionLost)?;
    if result.size < std::mem::size_of::<sys::mln_completion_result>() as u32 {
        return Err(Error::CompletionLost);
    }
    if result.status != 0 || result.disposition != 0 {
        return Err(Error::Native {
            status: result.status,
            disposition: Some(result.disposition),
            // SAFETY: diagnostic is borrowed for this callback; copy it now.
            diagnostic: unsafe { copy_view(result.diagnostic) },
        });
    }
    Ok(Completion {
        generation: result.generation,
        render_update_generation: result.render_update_generation,
    })
}

pub(crate) unsafe fn copy_view(view: sys::mln_buffer_view) -> String {
    if view.data.is_null() || view.size == 0 {
        return String::new();
    }
    // SAFETY: caller guarantees native's view is readable for size bytes.
    let bytes = unsafe { std::slice::from_raw_parts(view.data.cast::<u8>(), view.size) };
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) unsafe extern "C" fn release(data: *mut c_void) {
    // SAFETY: called exactly once after completion or manually after rejection.
    drop(unsafe { Box::from_raw(data.cast::<CompletionOwner>()) });
}
