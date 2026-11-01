use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use crate::{Error, FrameParameters, GraphicsContext, LayerErrors, NativeCustomLayer, Pending};
use crate::{Result, lock, pending, sys};

#[repr(C)]
pub(crate) struct Callbacks {
    abi_version: u32,
    size: u32,
    pub(crate) user_data: *mut c_void,
    initialize: unsafe extern "C" fn(*mut c_void, *const GraphicsContext),
    pre_render: unsafe extern "C" fn(*mut c_void, *const FrameParameters),
    render: unsafe extern "C" fn(*mut c_void, *const FrameParameters),
    context_lost: unsafe extern "C" fn(*mut c_void),
    deinitialize: unsafe extern "C" fn(*mut c_void),
    release_user_data: unsafe extern "C" fn(*mut c_void),
}

struct State {
    host: Option<Box<dyn NativeCustomLayer>>,
    failed: bool,
}

struct UserData {
    state: Mutex<State>,
    errors: LayerErrors,
}

type Submit = unsafe extern "C" fn(
    sys::mln_map,
    sys::mln_buffer_view,
    *const Callbacks,
    sys::mln_buffer_view,
    *const sys::mln_completion,
    *mut sys::mln_diagnostic,
) -> sys::mln_status;

unsafe extern "C" {
    fn mln_custom_layer_add(
        map: sys::mln_map,
        id: sys::mln_buffer_view,
        callbacks: *const Callbacks,
        before: sys::mln_buffer_view,
        completion: *const sys::mln_completion,
        diagnostic: *mut sys::mln_diagnostic,
    ) -> sys::mln_status;
}

pub(crate) fn submit(map: sys::mln_map, id: &str, host: impl NativeCustomLayer, before: Option<&str>) -> Result<Pending> {
    submit_with(map, id, host, before, mln_custom_layer_add)
}

fn submit_with(map: sys::mln_map, id: &str, host: impl NativeCustomLayer, before: Option<&str>, submit: Submit) -> Result<Pending> {
    let errors = LayerErrors::default();
    let callbacks = descriptor(host, errors.clone());
    let (pending, completion) = pending::prepare(errors);
    let mut diagnostic = sys::mln_diagnostic {
        size: std::mem::size_of::<sys::mln_diagnostic>() as u32,
        message: [0; 4096],
    };
    // SAFETY: native copies views/descriptors during the call. Both userdata
    // allocations transfer only on OK; acceptance may complete before return.
    let status = unsafe { submit(map, view(id), &callbacks, optional_view(before), &completion, &mut diagnostic) };
    if status == 0 {
        return Ok(pending);
    }
    // SAFETY: non-OK invokes neither callback/release and transfers no ownership.
    unsafe {
        release(callbacks.user_data);
        pending::release(completion.user_data);
    }
    let bytes: Vec<u8> = diagnostic.message.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    Err(Error::Native {
        status,
        disposition: None,
        diagnostic: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

fn view(value: &str) -> sys::mln_buffer_view {
    sys::mln_buffer_view {
        data: value.as_ptr().cast(),
        size: value.len(),
    }
}

fn optional_view(value: Option<&str>) -> sys::mln_buffer_view {
    value.map_or(
        sys::mln_buffer_view {
            data: std::ptr::null(),
            size: 0,
        },
        view,
    )
}

fn descriptor(host: impl NativeCustomLayer, errors: LayerErrors) -> Callbacks {
    let data = Box::new(UserData {
        state: Mutex::new(State {
            host: Some(Box::new(host)),
            failed: false,
        }),
        errors,
    });
    Callbacks {
        abi_version: crate::ABI_VERSION,
        size: std::mem::size_of::<Callbacks>() as u32,
        user_data: Box::into_raw(data).cast(),
        initialize,
        pre_render,
        render,
        context_lost,
        deinitialize,
        release_user_data: release,
    }
}

fn invoke(data: *mut c_void, phase: &'static str, cleanup: bool, call: impl FnOnce(&mut dyn NativeCustomLayer) -> Result<()>) {
    // SAFETY: native retains userdata until all callbacks finish and final release.
    let data = unsafe { &*data.cast::<UserData>() };
    let mut state = lock(&data.state);
    if state.failed && !cleanup {
        return;
    }
    let Some(host) = state.host.as_mut() else { return };
    let result = catch_host(phase, || call(host.as_mut()));
    if let Err(error) = result {
        state.failed = true;
        data.errors.record(error);
    }
}

pub(crate) fn catch_host<T>(phase: &'static str, call: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(payload) => {
            // A custom panic payload's destructor can itself panic. Leaking only
            // that exceptional payload prevents a second unwind across the ABI.
            std::mem::forget(payload);
            Err(Error::CallbackPanic(phase))
        }
    }
}

unsafe extern "C" fn initialize(data: *mut c_void, graphics: *const GraphicsContext) {
    invoke(data, "initialize", false, |host| {
        // SAFETY: callback parameters are borrowed for this invocation.
        host.initialize(unsafe { &*graphics })
    });
}

unsafe extern "C" fn pre_render(data: *mut c_void, parameters: *const FrameParameters) {
    invoke(data, "pre_render", false, |host| {
        // SAFETY: callback parameters are borrowed for this invocation.
        host.pre_render(unsafe { &*parameters })
    });
}

unsafe extern "C" fn render(data: *mut c_void, parameters: *const FrameParameters) {
    invoke(data, "render", false, |host| {
        // SAFETY: callback parameters are borrowed for this invocation.
        host.render(unsafe { &*parameters })
    });
}

unsafe extern "C" fn context_lost(data: *mut c_void) {
    invoke(data, "context_lost", true, |host| host.context_lost());
}

unsafe extern "C" fn deinitialize(data: *mut c_void) {
    invoke(data, "deinitialize", true, |host| host.deinitialize());
}

unsafe extern "C" fn release(data: *mut c_void) {
    // SAFETY: only native's final release (or synchronous rejection) owns this box.
    let data = unsafe { Box::from_raw(data.cast::<UserData>()) };
    let host = lock(&data.state).host.take();
    if let Err(error) = catch_host("drop", || {
        drop(host);
        Ok(())
    }) {
        data.errors.record(error);
    }
}

#[cfg(test)]
#[path = "callback_tests.rs"]
mod tests;
