//! `vance-viewport` — the native overlay surface that renders the video
//! viewport into the Tauri webview's content area.
//!
//! Per ADR 0005 (the Phase −1 spike result):
//!
//! - Chrome (Tauri webview) reports a placeholder `<div id="viewport">`
//!   bounds via IPC on every layout change.
//! - This crate owns a native overlay (NSView + CAMetalLayer on macOS;
//!   HWND child on Windows; X11 child / Wayland subsurface on Linux) that
//!   we position to match those bounds and render into via wgpu.
//! - The native overlay sits *visually above* the HTML chrome — chrome
//!   inside the viewport rectangle is hidden by it. Click events still
//!   route to the webview.
//!
//! ## Milestone 1.4 status
//!
//! - macOS: implemented and verified by the spike.
//! - Windows: stubbed `unimplemented!()` until Phase 0 Week 1 task 1
//!   re-runs the spike on a Windows machine.
//! - Linux X11/Wayland: same — stubbed pending verification.
//!
//! ## Lifetime
//!
//! ```ignore
//! // 1. Create on the main thread, after the Tauri window exists.
//! let viewport = Viewport::install(host_window_handle)?;
//!
//! // 2. Whenever the chrome reports new bounds:
//! viewport.set_bounds(ViewportBounds { x, y, w, h });
//!
//! // 3. Get a wgpu surface + matching device for rendering.
//! let surface = viewport.surface();
//! let renderer = vance_render::Renderer::new(...);  // sharing the device is a 1.5 task
//!
//! // 4. Each frame:
//! //    upload to renderer; renderer.render_to(viewport.next_view());
//!
//! // 5. Drop disposes the overlay cleanly.
//! ```

#![warn(missing_docs)]

mod bounds;
mod error;

pub use bounds::ViewportBounds;
pub use error::{Result, ViewportError};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Viewport;

#[cfg(not(target_os = "macos"))]
mod fallback;
#[cfg(not(target_os = "macos"))]
pub use fallback::Viewport;

/// An opaque, platform-specific handle to the host window we'll parent into.
///
/// On macOS this wraps an `NSWindow*`; on Windows an `HWND`; on Linux a GTK
/// window pointer. Use the `from_*_ptr` constructors at the call site.
#[derive(Debug, Clone, Copy)]
pub struct HostWindowHandle(
    // Intentional: only the macOS viewport reads the handle today; the
    // Linux/Windows fallback stub ignores it until the real viewports land
    // (Windows/Linux milestone), so it's dead code there for now.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] HostWindowHandleInner,
);

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // see HostWindowHandle
enum HostWindowHandleInner {
    #[cfg(target_os = "macos")]
    AppKit { ns_window: *mut std::ffi::c_void },
    #[cfg(target_os = "windows")]
    Win32 { hwnd: *mut std::ffi::c_void },
    #[cfg(all(unix, not(target_os = "macos")))]
    Gtk { gtk_window: *mut std::ffi::c_void },
    #[doc(hidden)]
    _Phantom,
}

impl HostWindowHandle {
    /// Wrap an `NSWindow*` from Tauri's `WebviewWindow::ns_window()`.
    #[cfg(target_os = "macos")]
    pub fn from_ns_window(ns_window: *mut std::ffi::c_void) -> Self {
        Self(HostWindowHandleInner::AppKit { ns_window })
    }

    /// Wrap an `HWND` from Tauri's `WebviewWindow::hwnd()`.
    #[cfg(target_os = "windows")]
    pub fn from_hwnd(hwnd: *mut std::ffi::c_void) -> Self {
        Self(HostWindowHandleInner::Win32 { hwnd })
    }

    /// Wrap a GTK window pointer from Tauri's `WebviewWindow::gtk_window()`.
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn from_gtk_window(gtk_window: *mut std::ffi::c_void) -> Self {
        Self(HostWindowHandleInner::Gtk { gtk_window })
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn as_ns_window(&self) -> Option<*mut std::ffi::c_void> {
        match self.0 {
            HostWindowHandleInner::AppKit { ns_window } => Some(ns_window),
            _ => None,
        }
    }
}

// Marker impls — the host handle is morally Send/Sync (it's a pointer to a
// long-lived OS object whose API we drive only on the main thread). The
// internal pointer never crosses thread boundaries inside this crate.
unsafe impl Send for HostWindowHandle {}
unsafe impl Sync for HostWindowHandle {}
