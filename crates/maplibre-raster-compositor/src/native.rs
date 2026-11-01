use crate::{Error, FrameParameters, Result, Surface};
use std::ffi::{CStr, c_char, c_int, c_void};

#[repr(C)]
pub(crate) struct SurfaceView {
    width: u32,
    height: u32,
    color: *const u8,
    depth: *const f32,
    extruded: bool,
}
impl From<&Surface> for SurfaceView {
    fn from(surface: &Surface) -> Self {
        Self {
            width: surface.width,
            height: surface.height,
            color: surface.color.as_ptr(),
            depth: surface.depth.as_ptr(),
            extruded: surface.extruded,
        }
    }
}
pub(crate) fn checked(call: impl FnOnce(*mut c_char, usize) -> c_int) -> Result<()> {
    let mut diagnostic = [0 as c_char; 1024];
    if call(diagnostic.as_mut_ptr(), diagnostic.len()) == 0 {
        return Ok(());
    }
    diagnostic[1023] = 0;
    // SAFETY: initialized array ends with NUL; native diagnostics are copied before returning.
    let message = unsafe { CStr::from_ptr(diagnostic.as_ptr()) }.to_string_lossy().into_owned();
    Err(Error::Native(message))
}
unsafe extern "C" {
    pub(crate) fn map_compositor_new() -> *mut c_void;
    pub(crate) fn map_compositor_prepare(
        handle: *mut c_void,
        parameters: *const FrameParameters,
        surface: *const SurfaceView,
        error: *mut c_char,
        size: usize,
    ) -> c_int;
    pub(crate) fn map_compositor_render(handle: *mut c_void, parameters: *const FrameParameters, error: *mut c_char, size: usize) -> c_int;
    pub(crate) fn map_compositor_deinitialize(handle: *mut c_void, error: *mut c_char, size: usize) -> c_int;
    pub(crate) fn map_compositor_context_lost(handle: *mut c_void);
    pub(crate) fn map_compositor_delete(handle: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_error_is_owned_and_truncated_safely() {
        let result = checked(|buffer, _| {
            // SAFETY: checked passes a writable buffer of 1024 bytes.
            unsafe { std::ptr::write_bytes(buffer, b'x', 1024) };
            1
        });
        assert_eq!(result, Err(Error::Native("x".repeat(1023))));
    }
    #[test]
    fn successful_status_does_not_read_diagnostic() {
        assert_eq!(checked(|_, _| 0), Ok(()));
    }
}
