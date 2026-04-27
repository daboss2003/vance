//! macOS viewport: an overlay `NSView` parented to the host window's
//! content view, backed by a `CAMetalLayer` we hand to wgpu.
//!
//! Productionized from the Phase −1 spike (`vance-spike-parenting/src-tauri/src/viewport.rs`).
//! Differences from the spike:
//! - The overlay NSView and CAMetalLayer are owned by the [`Viewport`]
//!   struct (no `mem::forget`); `Drop` removes the overlay and releases
//!   the layer.
//! - Bounds updates always dispatch onto the main queue.
//! - We expose the wgpu `Surface` so [`vance_render::Renderer`] can render
//!   into it directly.

use crate::{HostWindowHandle, Result, ViewportBounds, ViewportError};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::CAMetalLayer;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::Arc;

/// macOS viewport: owns an overlay NSView + CAMetalLayer attached to the
/// Tauri webview's content view, plus a wgpu Surface bound to that layer.
pub struct Viewport {
    inner: Arc<Inner>,
    bounds: Mutex<ViewportBounds>,
    surface: wgpu::Surface<'static>,
    backing_scale: f64,
}

struct Inner {
    overlay: Retained<NSView>,
    metal_layer: Retained<CAMetalLayer>,
    content_view: Retained<NSView>,
}

impl Viewport {
    /// Install the overlay and create a wgpu surface bound to its CAMetalLayer.
    ///
    /// Must be called on the main thread (AppKit requirement). The wgpu
    /// instance you pass in must have the `METAL` backend enabled.
    pub fn install(host: HostWindowHandle, wgpu_instance: &wgpu::Instance) -> Result<Self> {
        let _mtm = MainThreadMarker::new().ok_or(ViewportError::NotMainThread)?;
        let ns_window_ptr = host.as_ns_window().ok_or(ViewportError::WrongHandleKind {
            expected: "AppKit NSWindow",
        })?;

        // SAFETY: caller-supplied NSWindow*; Tauri returns a valid one from
        // `WebviewWindow::ns_window()`. The pointer outlives this call.
        let ns_window: &NSWindow = unsafe { &*(ns_window_ptr as *const NSWindow) };
        let content_view: Retained<NSView> = ns_window
            .contentView()
            .ok_or(ViewportError::NoContentView)?;
        let backing_scale = ns_window.backingScaleFactor();

        // Allocate the overlay NSView. Initial frame is zero-sized; a
        // `set_bounds` call from the chrome will resize it on the first
        // layout-change event.
        let mtm_for_alloc = MainThreadMarker::new().ok_or(ViewportError::NotMainThread)?;
        let init_frame = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 1.0,
                height: 1.0,
            },
        };
        let overlay: Retained<NSView> =
            NSView::initWithFrame(NSView::alloc(mtm_for_alloc), init_frame);
        overlay.setWantsLayer(true);
        // We manage our own frame; no AppKit autoresize.
        overlay.setAutoresizingMask(NSAutoresizingMaskOptions::ViewNotSizable);

        // Build the CAMetalLayer and attach it as the overlay's backing layer.
        let metal_layer = CAMetalLayer::new();
        unsafe {
            let layer_ptr: *const CAMetalLayer = Retained::as_ptr(&metal_layer);
            let layer_obj: &AnyObject = &*(layer_ptr as *const AnyObject);
            let _: () = msg_send![&*overlay, setLayer: layer_obj];
            let _: () = msg_send![&*metal_layer, setContentsScale: backing_scale];
            metal_layer.setAnchorPoint(NSPoint { x: 0.0, y: 0.0 });
        }

        // Add overlay as the topmost subview of the content view (above the
        // WKWebView). Per ADR 0005, this is what gives us "native overlay
        // sits above HTML chrome."
        const NS_WINDOW_ABOVE: i64 = 1;
        unsafe {
            let cv_ptr: *const NSView = Retained::as_ptr(&content_view);
            let ov_ptr: *const NSView = Retained::as_ptr(&overlay);
            let cv_obj: &AnyObject = &*(cv_ptr as *const AnyObject);
            let ov_obj: &AnyObject = &*(ov_ptr as *const AnyObject);
            let nil_view: *mut NSView = std::ptr::null_mut();
            let _: () = msg_send![cv_obj,
                addSubview: ov_obj,
                positioned: NS_WINDOW_ABOVE,
                relativeTo: nil_view
            ];
        }

        // Create a wgpu surface bound to the CAMetalLayer.
        // SAFETY: the layer is owned by `Inner` (held in an Arc) and outlives
        // every clone of this Viewport, so the raw pointer remains valid for
        // the surface's lifetime.
        let layer_raw = Retained::as_ptr(&metal_layer) as *mut c_void;
        let surface = unsafe {
            wgpu_instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer_raw))
                .map_err(|e| ViewportError::Surface(e.to_string()))?
        };

        let inner = Arc::new(Inner {
            overlay,
            metal_layer,
            content_view,
        });

        Ok(Self {
            inner,
            bounds: Mutex::new(ViewportBounds::default()),
            surface,
            backing_scale,
        })
    }

    /// The wgpu surface backing the overlay layer. Hand this to
    /// [`vance_render::Renderer`] (or any wgpu code that needs a surface).
    pub fn surface(&self) -> &wgpu::Surface<'static> {
        &self.surface
    }

    /// HiDPI scale factor of the host window when this viewport was installed.
    /// Used by the renderer to size its target.
    pub fn backing_scale(&self) -> f64 {
        self.backing_scale
    }

    /// Update the overlay's position/size. Schedules an update on the main
    /// queue (CALayer mutations must happen on the main thread). Returns
    /// immediately; the chrome doesn't need to wait for the geometry to apply.
    pub fn set_bounds(&self, bounds: ViewportBounds) {
        *self.bounds.lock() = bounds;
        let inner = self.inner.clone();
        let scale = self.backing_scale;
        unsafe {
            dispatch_async_on_main(move || {
                let frame = bounds_to_view_frame(&inner.content_view, &bounds, scale);
                let ov_ptr: *const NSView = Retained::as_ptr(&inner.overlay);
                let ov_obj: &AnyObject = &*(ov_ptr as *const AnyObject);
                let _: () = msg_send![ov_obj, setFrame: frame];
                // Match the layer's drawableSize to the overlay's pixel size
                // so wgpu's swapchain reconfigure delivers the right resolution.
                let pixel_size = NSSize {
                    width: bounds.w as f64,
                    height: bounds.h as f64,
                };
                let layer_ptr: *const CAMetalLayer = Retained::as_ptr(&inner.metal_layer);
                let layer_obj: &AnyObject = &*(layer_ptr as *const AnyObject);
                let _: () = msg_send![layer_obj, setDrawableSize: pixel_size];
            });
        }
    }

    /// Latest bounds the chrome has reported.
    pub fn current_bounds(&self) -> ViewportBounds {
        *self.bounds.lock()
    }
}

impl Drop for Viewport {
    fn drop(&mut self) {
        // Remove the overlay from the host window. Schedule on the main
        // thread; the Arc keeps `inner` alive across the dispatch.
        let inner = self.inner.clone();
        unsafe {
            dispatch_async_on_main(move || {
                let ov_ptr: *const NSView = Retained::as_ptr(&inner.overlay);
                let ov_obj: &AnyObject = &*(ov_ptr as *const AnyObject);
                let _: () = msg_send![ov_obj, removeFromSuperview];
            });
        }
    }
}

fn bounds_to_view_frame(content_view: &NSView, b: &ViewportBounds, scale: f64) -> NSRect {
    // HTML reports physical pixels with origin top-left of the host window's
    // client area. AppKit defaults to bottom-left origin; flip Y in points.
    let view_size_pts = content_view.bounds().size;
    let view_h_pts = view_size_pts.height;
    let x_pt = b.x as f64 / scale;
    let w_pt = b.w as f64 / scale;
    let h_pt = b.h as f64 / scale;
    let y_pt_top = b.y as f64 / scale;
    let y_pt = view_h_pts - y_pt_top - h_pt;
    NSRect {
        origin: NSPoint { x: x_pt, y: y_pt },
        size: NSSize {
            width: w_pt,
            height: h_pt,
        },
    }
}

// ---- libdispatch main-queue dispatch ---------------------------------------

type DispatchBlock = Box<dyn FnOnce() + Send + 'static>;

extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *mut c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
}

extern "C" fn trampoline(ctx: *mut c_void) {
    let b: Box<DispatchBlock> = unsafe { Box::from_raw(ctx as *mut DispatchBlock) };
    (b)();
}

unsafe fn dispatch_async_on_main<F: FnOnce() + Send + 'static>(f: F) {
    let boxed: Box<DispatchBlock> = Box::new(Box::new(f));
    let ctx = Box::into_raw(boxed) as *mut c_void;
    let main_q = &_dispatch_main_q as *const c_void as *mut c_void;
    dispatch_async_f(main_q, ctx, trampoline);
}

// `Inner` holds Retained<NSView>/Retained<CAMetalLayer> which are not Send by
// default. We promise Send/Sync because (a) all mutations of the wrapped
// AppKit objects happen on the main queue via libdispatch, and (b) the
// inner Arc is only ever read for cloning + main-thread closures.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}
