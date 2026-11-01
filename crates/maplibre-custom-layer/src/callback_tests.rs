use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Probe {
    events: Arc<Mutex<Vec<&'static str>>>,
    drops: Arc<AtomicUsize>,
    panic_phase: Option<&'static str>,
    error_phase: Option<&'static str>,
}

impl Probe {
    fn new() -> Self {
        Self {
            events: Arc::default(),
            drops: Arc::default(),
            panic_phase: None,
            error_phase: None,
        }
    }

    fn event(&self, phase: &'static str) -> Result<()> {
        lock(&self.events).push(phase);
        assert_ne!(self.panic_phase, Some(phase), "test host panic");
        if self.error_phase == Some(phase) {
            return Err(Error::Host(phase.into()));
        }
        Ok(())
    }
}

impl NativeCustomLayer for Probe {
    fn initialize(&mut self, _: &GraphicsContext) -> Result<()> {
        self.event("initialize")
    }
    fn pre_render(&mut self, _: &FrameParameters) -> Result<()> {
        self.event("pre_render")
    }
    fn render(&mut self, _: &FrameParameters) -> Result<()> {
        self.event("render")
    }
    fn context_lost(&mut self) -> Result<()> {
        self.event("context_lost")
    }
    fn deinitialize(&mut self) -> Result<()> {
        self.event("deinitialize")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        let _ = self.event("drop");
    }
}

unsafe extern "C" fn rejected(
    _: sys::mln_map,
    _: sys::mln_buffer_view,
    _: *const Callbacks,
    _: sys::mln_buffer_view,
    _: *const sys::mln_completion,
    _: *mut sys::mln_diagnostic,
) -> i32 {
    -2
}

unsafe fn deliver(completion: *const sys::mln_completion, status: i32, disposition: u32) {
    let diagnostic = String::from("queued command failed");
    let result = sys::mln_completion_result {
        size: std::mem::size_of::<sys::mln_completion_result>() as u32,
        status,
        disposition,
        value_size: 0,
        generation: 17,
        render_update_generation: 18,
        diagnostic: view(&diagnostic),
        value: std::ptr::null(),
        value_count: 0,
    };
    // SAFETY: test mimics native's one-shot callback/release protocol.
    let completion = unsafe { &*completion };
    unsafe {
        completion.callback.unwrap()(completion.user_data, &result);
        completion.release_user_data.unwrap()(completion.user_data);
    }
}

unsafe extern "C" fn queued_failure(
    _: sys::mln_map,
    _: sys::mln_buffer_view,
    callbacks: *const Callbacks,
    _: sys::mln_buffer_view,
    completion: *const sys::mln_completion,
    _: *mut sys::mln_diagnostic,
) -> i32 {
    // SAFETY: accepted work may finish before submission returns.
    unsafe {
        deliver(completion, -10, 1);
        release((*callbacks).user_data);
    }
    0
}

unsafe extern "C" fn committed(
    _: sys::mln_map,
    _: sys::mln_buffer_view,
    callbacks: *const Callbacks,
    _: sys::mln_buffer_view,
    completion: *const sys::mln_completion,
    _: *mut sys::mln_diagnostic,
) -> i32 {
    // SAFETY: emulate a completed insertion then native's final host release.
    unsafe {
        deliver(completion, 0, 0);
        release((*callbacks).user_data);
    }
    0
}

#[test]
fn callback_descriptor_advertises_raster_frame_abi_version_two() {
    let callbacks = descriptor(Probe::new(), LayerErrors::default());
    assert_eq!(callbacks.abi_version, 2);
    // SAFETY: local descriptor owns its userdata and releases it exactly once.
    unsafe { release(callbacks.user_data) };
}

#[test]
fn synchronous_rejection_drops_host_exactly_once() {
    let probe = Probe::new();
    let drops = probe.drops.clone();
    let result = submit_with(sys::mln_map(0), "layer", probe, None, rejected);
    assert!(matches!(
        result,
        Err(Error::Native {
            status: -2,
            disposition: None,
            ..
        })
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn queued_failure_is_not_fake_success_and_copies_diagnostic() {
    let pending = submit_with(sys::mln_map(1), "layer", Probe::new(), None, queued_failure).unwrap();
    assert_eq!(
        pending.wait(),
        Err(Error::Native {
            status: -10,
            disposition: Some(1),
            diagnostic: "queued command failed".into(),
        })
    );
}

#[test]
fn queued_failure_drops_uninitialized_host_exactly_once() {
    let probe = Probe::new();
    let drops = probe.drops.clone();
    let pending = submit_with(sys::mln_map(1), "layer", probe, None, queued_failure).unwrap();
    drop(pending);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn successful_completion_reports_real_generations() {
    let pending = submit_with(sys::mln_map(1), "layer", Probe::new(), None, committed).unwrap();
    assert_eq!(
        pending.wait(),
        Ok(pending::Completion {
            generation: 17,
            render_update_generation: 18
        })
    );
}

#[test]
fn cancellation_is_an_error_even_if_status_is_ok() {
    let (pending, completion) = pending::prepare(LayerErrors::default());
    // SAFETY: local test owns the completion descriptor.
    unsafe { deliver(&completion, 0, 2) };
    assert!(matches!(pending.wait(), Err(Error::Native { disposition: Some(2), .. })));
}

#[test]
fn completion_can_arrive_from_another_thread() {
    let (pending, completion) = pending::prepare(LayerErrors::default());
    let callback = completion.callback;
    let release = completion.release_user_data;
    let data = completion.user_data as usize;
    let worker = std::thread::spawn(move || {
        let completion = sys::mln_completion {
            size: std::mem::size_of::<sys::mln_completion>() as u32,
            callback,
            release_user_data: release,
            user_data: data as *mut c_void,
            queue: sys::mln_delivery_queue(0),
        };
        // SAFETY: the sender allocation is Send and this worker is its sole owner.
        unsafe { deliver(&completion, -6, 2) };
    });
    let result = pending.wait_timeout(Duration::from_secs(5));
    worker.join().unwrap();
    assert!(matches!(
        result,
        Err(Error::Native {
            status: -6,
            disposition: Some(2),
            ..
        })
    ));
}

#[test]
fn completion_release_without_result_reports_disconnect() {
    let (pending, completion) = pending::prepare(LayerErrors::default());
    // SAFETY: no ownership transfer occurred; this test owns its allocation.
    unsafe { pending::release(completion.user_data) };
    assert_eq!(pending.wait(), Err(Error::CompletionLost));
}

#[test]
fn timeout_does_not_cancel_or_consume_future_completion() {
    let (pending, completion) = pending::prepare(LayerErrors::default());
    assert_eq!(pending.wait_timeout(Duration::ZERO), Err(Error::Timeout));
    // SAFETY: local test owns the completion descriptor.
    unsafe { deliver(&completion, 0, 0) };
    assert!(pending.wait().is_ok());
}

#[test]
fn dropped_receiver_does_not_prevent_native_release() {
    let (pending, completion) = pending::prepare(LayerErrors::default());
    drop(pending);
    // SAFETY: native still owns and can complete/release after receiver drop.
    unsafe { deliver(&completion, 0, 0) };
}

#[test]
fn panic_stops_work_but_does_not_skip_native_cleanup() {
    let mut probe = Probe::new();
    probe.panic_phase = Some("initialize");
    let events = probe.events.clone();
    let errors = LayerErrors::default();
    let callbacks = descriptor(probe, errors.clone());
    let graphics = GraphicsContext::default();
    // SAFETY: descriptor remains owned locally; callbacks are called before release.
    unsafe {
        initialize(callbacks.user_data, &graphics);
        initialize(callbacks.user_data, &graphics);
        context_lost(callbacks.user_data);
        deinitialize(callbacks.user_data);
        release(callbacks.user_data);
    }
    assert_eq!(errors.first_error(), Some(Error::CallbackPanic("initialize")));
    assert_eq!(*lock(&events), ["initialize", "context_lost", "deinitialize", "drop"]);
}

#[test]
fn render_panic_is_contained_and_later_frame_work_is_suppressed() {
    let mut probe = Probe::new();
    probe.panic_phase = Some("render");
    let events = probe.events.clone();
    let errors = LayerErrors::default();
    let callbacks = descriptor(probe, errors.clone());
    // SAFETY: all fields are floats, integers, nullable raw pointers or nullable fn pointers.
    let frame: FrameParameters = unsafe { std::mem::zeroed() };
    // SAFETY: local descriptor owns its userdata; release is final.
    unsafe {
        pre_render(callbacks.user_data, &frame);
        render(callbacks.user_data, &frame);
        pre_render(callbacks.user_data, &frame);
        render(callbacks.user_data, &frame);
        deinitialize(callbacks.user_data);
        release(callbacks.user_data);
    }
    assert_eq!(errors.first_error(), Some(Error::CallbackPanic("render")));
    assert_eq!(*lock(&events), ["pre_render", "render", "deinitialize", "drop"]);
}

#[test]
fn context_loss_does_not_synthesize_deinitialize() {
    let probe = Probe::new();
    let events = probe.events.clone();
    let callbacks = descriptor(probe, LayerErrors::default());
    // SAFETY: mirrors the native context-loss/final-release path.
    unsafe {
        context_lost(callbacks.user_data);
        release(callbacks.user_data);
    }
    assert_eq!(*lock(&events), ["context_lost", "drop"]);
}

#[test]
fn host_error_is_observable_after_final_release() {
    let mut probe = Probe::new();
    probe.error_phase = Some("initialize");
    let errors = LayerErrors::default();
    let callbacks = descriptor(probe, errors.clone());
    // SAFETY: local descriptor owns its userdata through final release.
    unsafe {
        initialize(callbacks.user_data, &GraphicsContext::default());
        release(callbacks.user_data);
    }
    assert_eq!(errors.first_error(), Some(Error::Host("initialize".into())));
}

#[test]
fn cleanup_without_initialization_does_not_synthesize_extra_cleanup() {
    let probe = Probe::new();
    let events = probe.events.clone();
    let callbacks = descriptor(probe, LayerErrors::default());
    // SAFETY: mimics native cleanup without prior initialize.
    unsafe {
        deinitialize(callbacks.user_data);
        release(callbacks.user_data);
    }
    assert_eq!(*lock(&events), ["deinitialize", "drop"]);
}

#[test]
fn final_drop_panic_is_contained_and_reported() {
    let mut probe = Probe::new();
    probe.panic_phase = Some("drop");
    let errors = LayerErrors::default();
    let callbacks = descriptor(probe, errors.clone());
    // SAFETY: final owner invokes release exactly once.
    unsafe { release(callbacks.user_data) };
    assert_eq!(errors.first_error(), Some(Error::CallbackPanic("drop")));
}

#[test]
fn panic_payload_with_panicking_drop_cannot_unwind_across_abi() {
    struct BadPayload;
    impl Drop for BadPayload {
        fn drop(&mut self) {
            panic!("payload drop");
        }
    }
    let result: Result<()> = catch_host("render", || std::panic::panic_any(BadPayload));
    assert_eq!(result, Err(Error::CallbackPanic("render")));
}
