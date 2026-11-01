use super::*;

#[test]
#[cfg(target_pointer_width = "64")]
fn graphics_and_frame_layout_match_64_bit_c_abi() {
    assert_eq!(std::mem::size_of::<GraphicsContext>(), 96);
    assert_eq!(std::mem::offset_of!(GraphicsContext, vk_get_device_proc_addr), 40);
    assert_eq!(std::mem::offset_of!(GraphicsContext, metal_device), 64);
    assert_eq!(std::mem::size_of::<FrameParameters>(), 576);
    assert_eq!(std::mem::offset_of!(FrameParameters, raster_projection_matrix), 320);
    assert_eq!(std::mem::offset_of!(FrameParameters, flat_depth_offset), 456);
    assert_eq!(std::mem::offset_of!(FrameParameters, pixel_ratio), 464);
    assert_eq!(std::mem::offset_of!(FrameParameters, depth_test), 472);
    assert_eq!(std::mem::offset_of!(FrameParameters, graphics), 480);
}

#[test]
fn ids_reject_empty_values() {
    assert_eq!(validate_id("", "id"), Err(Error::InvalidId("id")));
}

#[test]
fn ids_reject_embedded_nul() {
    assert_eq!(validate_id("a\0b", "before"), Err(Error::InvalidId("before")));
}

#[test]
fn ids_accept_utf8_names() {
    assert_eq!(validate_id("raster-世界", "id"), Ok(()));
}

#[test]
fn monitor_preserves_first_failure() {
    let errors = LayerErrors::default();
    errors.record(Error::Host("first".into()));
    errors.record(Error::CallbackPanic("deinitialize"));
    assert_eq!(errors.first_error(), Some(Error::Host("first".into())));
}

#[test]
fn monitor_recovers_poisoned_lock() {
    let errors = LayerErrors::default();
    let _ = std::panic::catch_unwind(|| {
        let _guard = errors.0.lock().unwrap();
        panic!("poison");
    });
    errors.record(Error::Host("still observable".into()));
    assert_eq!(errors.first_error(), Some(Error::Host("still observable".into())));
}
