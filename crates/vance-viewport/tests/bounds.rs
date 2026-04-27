//! Pure-Rust unit tests. End-to-end viewport tests need a live AppKit run
//! loop and live in `vance-app` once the integration glue lands (Milestone 1.5).

use vance_viewport::ViewportBounds;

#[test]
fn empty_when_zero_dimension() {
    assert!(ViewportBounds::default().is_empty());
    assert!(ViewportBounds::new(10, 20, 0, 100).is_empty());
    assert!(ViewportBounds::new(10, 20, 100, 0).is_empty());
    assert!(!ViewportBounds::new(0, 0, 1, 1).is_empty());
}

#[test]
fn equality() {
    assert_eq!(
        ViewportBounds::new(10, 20, 30, 40),
        ViewportBounds::new(10, 20, 30, 40)
    );
    assert_ne!(
        ViewportBounds::new(10, 20, 30, 40),
        ViewportBounds::new(11, 20, 30, 40)
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn fallback_install_returns_unimplemented() {
    use vance_viewport::{HostWindowHandle, Viewport, ViewportError};

    // Construct a placeholder host handle just to exercise the API.
    // (On non-macOS the only reachable handle constructors are HWND / GTK,
    // and on a host where neither is available we just rely on the err path.)
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

    // Use whichever ctor exists for this build.
    #[cfg(target_os = "windows")]
    let host = HostWindowHandle::from_hwnd(std::ptr::null_mut());
    #[cfg(all(unix, not(target_os = "macos")))]
    let host = HostWindowHandle::from_gtk_window(std::ptr::null_mut());

    #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
    {
        let err = Viewport::install(host, &instance).unwrap_err();
        assert!(matches!(err, ViewportError::Unimplemented));
    }
}
