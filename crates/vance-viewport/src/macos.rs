//! macOS viewport: a borderless **child NSWindow** parented to the host
//! window via `addChildWindow:ordered:NSWindowAbove`, hosting a layer-backed
//! NSView + CAMetalLayer.
//!
//! ## Why a child window
//!
//! The previous attempt added a CAMetalLayer as a sublayer of the host
//! window's content view (or the WKWebView's layer). Modern macOS WKWebView
//! uses out-of-process compositing; sublayers added locally do not
//! reliably surface above the WKWebView's content. A borderless child
//! NSWindow sidesteps the entire composition problem — it's a separate OS
//! window that the window server composites on top of the parent. This is
//! the same pattern Final Cut Pro, DaVinci Resolve, etc. use for their
//! native preview surfaces.
//!
//! ## Trade-offs (acceptable for v1)
//!
//! - The child window stays anchored to the parent (`addChildWindow`
//!   handles move / minimize / close together).
//! - `setIgnoresMouseEvents:YES` so all clicks pass through to the
//!   webview chrome below.
//! - When the parent loses focus, the child window stays visible (this
//!   matches every other NLE's behavior).
//! - Drag-resize of the parent triggers per-frame `set_bounds` updates
//!   from the chrome's ResizeObserver; the child window snaps to match.

use crate::{HostWindowHandle, Result, ViewportBounds, ViewportError};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::CAMetalLayer;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::Arc;

/// macOS viewport: owns a child NSWindow + layer-hosting NSView +
/// CAMetalLayer + the wgpu Surface bound to it.
pub struct Viewport {
    inner: Arc<Inner>,
    bounds: Mutex<ViewportBounds>,
    surface: wgpu::Surface<'static>,
    backing_scale: f64,
}

struct Inner {
    /// The parent host window — used to compute child window screen coords.
    parent_window: Retained<NSWindow>,
    /// Borderless child window we create.
    child_window: Retained<NSWindow>,
    /// Content view of the child window; layer-hosting our CAMetalLayer.
    content_view: Retained<NSView>,
    /// The metal layer wgpu renders into.
    metal_layer: Retained<CAMetalLayer>,
}

impl Viewport {
    /// Install the child window + create a wgpu surface bound to its
    /// CAMetalLayer. Must be called on the main thread.
    pub fn install(host: HostWindowHandle, wgpu_instance: &wgpu::Instance) -> Result<Self> {
        let mtm = MainThreadMarker::new().ok_or(ViewportError::NotMainThread)?;
        let ns_window_ptr = host.as_ns_window().ok_or(ViewportError::WrongHandleKind {
            expected: "AppKit NSWindow",
        })?;

        // SAFETY: caller-supplied NSWindow*.
        let parent_ref: &NSWindow = unsafe { &*(ns_window_ptr as *const NSWindow) };
        let parent_window: Retained<NSWindow> =
            unsafe { Retained::retain(parent_ref as *const NSWindow as *mut NSWindow) }
                .ok_or(ViewportError::NoContentView)?;
        let backing_scale = parent_window.backingScaleFactor();

        // ---- Build a layer-hosting NSView that will become the child
        //      window's content view. setLayer: must be called BEFORE
        //      setWantsLayer:true to get layer-hosting (vs layer-backed)
        //      mode where our CAMetalLayer is the actual backing layer.
        let init_frame = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 1.0,
                height: 1.0,
            },
        };
        let content_view: Retained<NSView> = NSView::initWithFrame(NSView::alloc(mtm), init_frame);

        let metal_layer = CAMetalLayer::new();
        unsafe {
            let layer_ptr: *const CAMetalLayer = Retained::as_ptr(&metal_layer);
            let layer_obj: &AnyObject = &*(layer_ptr as *const AnyObject);
            let _: () = msg_send![&*content_view, setLayer: layer_obj];
            let _: () = msg_send![&*metal_layer, setContentsScale: backing_scale];
            metal_layer.setAnchorPoint(NSPoint { x: 0.0, y: 0.0 });
            // Solid black background until the first wgpu render arrives,
            // so the layer doesn't flash whatever default the OS gives it.
            let black: *mut c_void = create_cg_color_rgba(0.0, 0.0, 0.0, 1.0);
            let _: () = msg_send![layer_obj, setBackgroundColor: black];
            cf_release(black);
        }
        content_view.setWantsLayer(true);

        // ---- Build the borderless child NSWindow.
        let style = NSWindowStyleMask::Borderless;
        let initial_screen_frame = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 1.0,
                height: 1.0,
            },
        };
        let child_window: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                initial_screen_frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        child_window.setContentView(Some(&content_view));

        // Transparent so any wgpu alpha respects the chrome behind it
        // (we render fully opaque, but be defensive).
        unsafe {
            let win_ptr: *const NSWindow = Retained::as_ptr(&child_window);
            let win_obj: &AnyObject = &*(win_ptr as *const AnyObject);
            let _: () = msg_send![win_obj, setOpaque: false];
            let clear: *mut c_void = create_cg_color_rgba(0.0, 0.0, 0.0, 0.0);
            // setBackgroundColor on NSWindow takes an NSColor not CGColor;
            // we use NSColor's clearColor singleton instead.
            cf_release(clear);
            let nscolor_class = objc2::runtime::AnyClass::get(c"NSColor").expect("NSColor missing");
            let clear_color: *mut AnyObject = msg_send![nscolor_class, clearColor];
            let _: () = msg_send![win_obj, setBackgroundColor: clear_color];
            let _: () = msg_send![win_obj, setHasShadow: false];
            let _: () = msg_send![win_obj, setIgnoresMouseEvents: true];
        }

        // ---- Attach to parent. NSWindowAbove (= 1) makes the child
        // composite above the parent. The parent will move/close the
        // child along with itself.
        const NS_WINDOW_ABOVE: i64 = 1;
        unsafe {
            let parent_obj: &AnyObject = &*(Retained::as_ptr(&parent_window) as *const AnyObject);
            let child_obj: &AnyObject = &*(Retained::as_ptr(&child_window) as *const AnyObject);
            let _: () = msg_send![parent_obj,
                addChildWindow: child_obj,
                ordered: NS_WINDOW_ABOVE
            ];
        }

        // ---- Create wgpu surface bound to the CAMetalLayer.
        let layer_raw = Retained::as_ptr(&metal_layer) as *mut c_void;
        let surface = unsafe {
            wgpu_instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer_raw))
                .map_err(|e| ViewportError::Surface(e.to_string()))?
        };

        let inner = Arc::new(Inner {
            parent_window,
            child_window,
            content_view,
            metal_layer,
        });

        tracing::info!("viewport: child window installed");

        Ok(Self {
            inner,
            bounds: Mutex::new(ViewportBounds::default()),
            surface,
            backing_scale,
        })
    }

    /// The wgpu surface backing the child window's CAMetalLayer.
    pub fn surface(&self) -> &wgpu::Surface<'static> {
        &self.surface
    }

    /// HiDPI scale factor of the parent window when this viewport was installed.
    pub fn backing_scale(&self) -> f64 {
        self.backing_scale
    }

    /// Update the child window's screen frame to cover the chrome's
    /// reported viewport rectangle.
    pub fn set_bounds(&self, bounds: ViewportBounds) {
        tracing::debug!(
            x = bounds.x,
            y = bounds.y,
            w = bounds.w,
            h = bounds.h,
            "viewport set_bounds"
        );
        *self.bounds.lock() = bounds;

        if !bounds.is_empty() {
            // CAMetalLayer.drawableSize is documented thread-safe.
            let pixel_size = NSSize {
                width: bounds.w as f64,
                height: bounds.h as f64,
            };
            // Re-sync contentsScale with the current screen — same
            // multi-monitor reason as backingScaleFactor below.
            let current_scale = self.inner.parent_window.backingScaleFactor();
            unsafe {
                let layer_ptr: *const CAMetalLayer = Retained::as_ptr(&self.inner.metal_layer);
                let layer_obj: &AnyObject = &*(layer_ptr as *const AnyObject);
                let _: () = msg_send![layer_obj, setDrawableSize: pixel_size];
                let _: () = msg_send![layer_obj, setContentsScale: current_scale];
            }
        }

        // Re-read the parent window's current backing scale every call.
        // The chrome reports bounds in *physical* pixels using the
        // current `window.devicePixelRatio`; if the user drags the
        // window across screens with different DPRs, that ratio
        // changes — and so must our divisor. The cached value captured
        // at install time is only correct if the window never leaves
        // its starting screen.
        let inner = self.inner.clone();
        let scale = inner.parent_window.backingScaleFactor();
        let work = move || {
            let screen_frame = compute_child_screen_frame(&inner.parent_window, &bounds, scale);
            unsafe {
                let win_ptr: *const NSWindow = Retained::as_ptr(&inner.child_window);
                let win_obj: &AnyObject = &*(win_ptr as *const AnyObject);
                let _: () = msg_send![win_obj, setFrame: screen_frame, display: true];
            }
        };
        if MainThreadMarker::new().is_some() {
            work();
        } else {
            unsafe { dispatch_async_on_main(work) };
        }
    }

    /// Most recent bounds the chrome has reported.
    pub fn current_bounds(&self) -> ViewportBounds {
        *self.bounds.lock()
    }
}

impl Drop for Viewport {
    fn drop(&mut self) {
        let inner = self.inner.clone();
        unsafe {
            dispatch_async_on_main(move || {
                let parent_obj: &AnyObject =
                    &*(Retained::as_ptr(&inner.parent_window) as *const AnyObject);
                let child_obj: &AnyObject =
                    &*(Retained::as_ptr(&inner.child_window) as *const AnyObject);
                let _: () = msg_send![parent_obj, removeChildWindow: child_obj];
                let _: () = msg_send![child_obj, orderOut: std::ptr::null_mut::<AnyObject>()];
                let _ = &inner.content_view;
                let _ = &inner.metal_layer;
            });
        }
    }
}

/// Compute the child window's screen frame from the chrome's reported
/// viewport rectangle.
///
/// Chrome reports `bounds` in **physical pixels with origin at the
/// top-left of the parent window's *content area*** (i.e. the
/// `<body>`-relative coordinates that `getBoundingClientRect` returns).
/// macOS screen coordinates are **points with origin at the bottom-left
/// of the primary display**, and `parent.frame()` includes the title
/// bar — so we have to subtract the title-bar height from the parent's
/// frame height to get the content-area height that the chrome's `y`
/// values are measured against.
fn compute_child_screen_frame(parent: &NSWindow, bounds: &ViewportBounds, scale: f64) -> NSRect {
    // Parent's frame on screen (in points, bottom-left origin).
    let parent_frame: NSRect = parent.frame();
    // `contentLayoutRect` is the rect *inside* the parent window where
    // chrome content is laid out — it excludes the title bar/toolbar
    // even with `NSWindowStyleMask::FullSizeContentView`, which is
    // exactly the area `getBoundingClientRect` measures against. Using
    // it instead of `contentView.frame` keeps the math correct under
    // Tauri's various title-bar styles.
    //
    // `contentLayoutRect` returns a rect in *window* coordinates
    // (bottom-left origin). Its top-left corresponds to chrome y=0.
    let layout_rect: NSRect = unsafe { msg_send![parent, contentLayoutRect] };
    let layout_top_in_window = layout_rect.origin.y + layout_rect.size.height;

    // Convert chrome-reported pixels → points.
    let x_pt = bounds.x as f64 / scale;
    let y_pt_top = bounds.y as f64 / scale;
    let w_pt = bounds.w as f64 / scale;
    let h_pt = bounds.h as f64 / scale;

    let x = parent_frame.origin.x + layout_rect.origin.x + x_pt;
    let y = parent_frame.origin.y + layout_top_in_window - y_pt_top - h_pt;

    tracing::trace!(
        parent_origin_x = parent_frame.origin.x,
        parent_origin_y = parent_frame.origin.y,
        parent_w = parent_frame.size.width,
        parent_h = parent_frame.size.height,
        layout_origin_x = layout_rect.origin.x,
        layout_origin_y = layout_rect.origin.y,
        layout_w = layout_rect.size.width,
        layout_h = layout_rect.size.height,
        layout_top_in_window,
        scale,
        bounds_x = bounds.x,
        bounds_y = bounds.y,
        bounds_w = bounds.w,
        bounds_h = bounds.h,
        out_x = x,
        out_y = y,
        out_w = w_pt,
        out_h = h_pt,
        "compute_child_screen_frame"
    );

    NSRect {
        origin: NSPoint { x, y },
        size: NSSize {
            width: w_pt,
            height: h_pt,
        },
    }
}

// ---- CoreGraphics CGColor helpers (for diagnostic backgroundColor) ---------

extern "C" {
    fn CGColorCreateGenericRGB(r: f64, g: f64, b: f64, a: f64) -> *mut c_void;
    fn CFRelease(cf: *mut c_void);
}

fn create_cg_color_rgba(r: f64, g: f64, b: f64, a: f64) -> *mut c_void {
    unsafe { CGColorCreateGenericRGB(r, g, b, a) }
}

fn cf_release(cf: *mut c_void) {
    unsafe { CFRelease(cf) }
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

// `Inner` holds Retained<NSWindow>/Retained<NSView>/Retained<CAMetalLayer>
// which are not Send by default. We promise Send/Sync because all
// mutations of those AppKit objects happen on the main queue.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}
