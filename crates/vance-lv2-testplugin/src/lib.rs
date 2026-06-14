//! Test fixture: a minimal real LV2 plugin — a fixed 0.5x (-6 dB) mono
//! gain. Used to verify `vance-audio`'s LV2 host (and the sidecar LV2
//! backend) end-to-end without a third-party LV2 plugin installed.
//!
//! Exports the standard `lv2_descriptor` C symbol; a host (lilv) loads
//! it via a `.lv2` bundle (`manifest.ttl` + `vance-test.ttl`, written by
//! [`staging::stage_bundle`]). Three ports: 0 = audio input, 1 = audio
//! output, 2 = an (ignored) atom-sequence input — present so the host's
//! atom/CV port wiring is exercised by a plugin that declares one.

use lv2_sys::{LV2_Descriptor, LV2_Feature, LV2_Handle};
use std::os::raw::{c_char, c_void};
use std::ptr;

pub mod staging;

/// Plugin URI the host looks up.
pub const PLUGIN_URI: &str = "urn:vance:test:gain";
const GAIN: f32 = 0.5; // -6.02 dB

/// Per-instance state: the connected audio port pointers.
struct State {
    input: *const f32,
    output: *mut f32,
}

unsafe extern "C" fn instantiate(
    _descriptor: *const LV2_Descriptor,
    _sample_rate: f64,
    _bundle_path: *const c_char,
    _features: *const *const LV2_Feature,
) -> LV2_Handle {
    let state = Box::new(State {
        input: ptr::null(),
        output: ptr::null_mut(),
    });
    Box::into_raw(state) as LV2_Handle
}

unsafe extern "C" fn connect_port(instance: LV2_Handle, port: u32, data: *mut c_void) {
    let state = &mut *(instance as *mut State);
    match port {
        0 => state.input = data as *const f32,
        1 => state.output = data as *mut f32,
        // Port 2 is the atom-sequence input; this fixture ignores it (it
        // exists only so the host's atom-port wiring is exercised).
        _ => {}
    }
}

unsafe extern "C" fn run(instance: LV2_Handle, n_samples: u32) {
    let state = &*(instance as *const State);
    if state.input.is_null() || state.output.is_null() {
        return;
    }
    for i in 0..n_samples as usize {
        *state.output.add(i) = *state.input.add(i) * GAIN;
    }
}

unsafe extern "C" fn cleanup(instance: LV2_Handle) {
    drop(Box::from_raw(instance as *mut State));
}

unsafe extern "C" fn extension_data(_uri: *const c_char) -> *const c_void {
    ptr::null()
}

/// The plugin descriptor. Raw pointers aren't `Sync`, so wrap it.
struct Descriptor(LV2_Descriptor);
// SAFETY: the descriptor is immutable and only ever read; the URI points
// at a static NUL-terminated string with `'static` lifetime.
unsafe impl Sync for Descriptor {}

static DESCRIPTOR: Descriptor = Descriptor(LV2_Descriptor {
    // c"..." is a 'static NUL-terminated CStr literal.
    URI: c"urn:vance:test:gain".as_ptr(),
    instantiate: Some(instantiate),
    connect_port: Some(connect_port),
    activate: None,
    run: Some(run),
    deactivate: None,
    cleanup: Some(cleanup),
    extension_data: Some(extension_data),
});

/// The standard LV2 entry point: return the descriptor for `index`.
///
/// # Safety
/// Called by the LV2 host with a plugin index; we expose exactly one.
#[no_mangle]
pub unsafe extern "C" fn lv2_descriptor(index: u32) -> *const LV2_Descriptor {
    if index == 0 {
        &DESCRIPTOR.0
    } else {
        ptr::null()
    }
}
