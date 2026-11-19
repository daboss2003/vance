//! Hardware-accelerated decode setup.
//!
//! Phase 2.2 ships VideoToolbox on macOS. NVDEC / VAAPI / D3D11VA land in
//! Phase 2.x as Win/Linux become testable.
//!
//! Setting `VANCE_FORCE_SW=1` in the environment forces the software path
//! (used by tests and for debugging HW-vs-SW behavior).
//!
//! ## How it works (macOS / VideoToolbox)
//!
//! 1. We create an `AVBufferRef` wrapping an `AVHWDeviceContext` of type
//!    `AV_HWDEVICE_TYPE_VIDEOTOOLBOX`.
//! 2. Before opening the decoder, we attach `hw_device_ctx` to the codec
//!    context. FFmpeg's default `get_format` then picks
//!    `AV_PIX_FMT_VIDEOTOOLBOX` whenever the codec offers it.
//! 3. After `receive_frame`, frames come back in `AV_PIX_FMT_VIDEOTOOLBOX`
//!    (an opaque GPU surface). We call `av_hwframe_transfer_data` to copy
//!    each frame down to system memory; the transferred frame is in NV12.
//! 4. `vance-render` already handles NV12 (split UV at upload), so the
//!    rest of the pipeline doesn't change.

use std::ffi::c_void;
use vance_ffmpeg_sys::ffmpeg;

/// Holds the lifetime of the hardware device context attached to a decoder.
/// Drops the underlying `AVBufferRef` on `Drop`.
pub struct HwDeviceCtx {
    raw: *mut ffmpeg::sys::AVBufferRef,
}

impl HwDeviceCtx {
    /// Try to create a VideoToolbox device context. Returns `None` on
    /// platforms without VT or if creation fails.
    #[cfg(target_os = "macos")]
    pub fn try_videotoolbox() -> Option<Self> {
        if std::env::var("VANCE_FORCE_SW").is_ok() {
            eprintln!("[hwaccel] VANCE_FORCE_SW set; forcing software decode");
            return None;
        }
        let mut raw: *mut ffmpeg::sys::AVBufferRef = std::ptr::null_mut();
        let ret = unsafe {
            ffmpeg::sys::av_hwdevice_ctx_create(
                &mut raw,
                ffmpeg::sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ret < 0 || raw.is_null() {
            eprintln!("[hwaccel] av_hwdevice_ctx_create(VIDEOTOOLBOX) failed: {ret}");
            return None;
        }
        Some(Self { raw })
    }

    /// VideoToolbox is macOS-only: always `None` here, so callers fall back
    /// to software decode.
    #[cfg(not(target_os = "macos"))]
    pub fn try_videotoolbox() -> Option<Self> {
        None
    }

    /// Attach this device context to a codec context (ownership is shared
    /// via `av_buffer_ref`). Must be called BEFORE the decoder is opened.
    ///
    /// # Safety
    /// `codec_ctx_ptr` must point to a valid, mutable `AVCodecContext`
    /// whose decoder has not yet been opened, and must outlive this call.
    pub unsafe fn attach_to(&self, codec_ctx_ptr: *mut c_void) {
        let codec_ctx = codec_ctx_ptr as *mut ffmpeg::sys::AVCodecContext;
        unsafe {
            (*codec_ctx).hw_device_ctx = ffmpeg::sys::av_buffer_ref(self.raw);
        }
    }
}

impl Drop for HwDeviceCtx {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                ffmpeg::sys::av_buffer_unref(&mut self.raw);
            }
        }
    }
}

// AVBufferRef is internally reference-counted; the pointer is opaque to us
// after construction. We only ever drop it from the same thread it was
// created on (the main thread before stream start, or the streaming
// decode thread that owns the OpenContext).
unsafe impl Send for HwDeviceCtx {}

/// True if the given AVPixelFormat (cast as i32) is a hardware-surface
/// format that needs `av_hwframe_transfer_data` before we can read pixels.
pub fn is_hardware_pixel_format(pf: i32) -> bool {
    pf == ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX as i32
}

/// Transfer a hardware frame to system memory. Returns NV12 (the format
/// VideoToolbox produces).
///
/// # Safety
/// `hw_frame_ptr` must point to a valid `AVFrame` containing a HW surface;
/// `sw_frame_ptr` must point to a valid, mutable `AVFrame` to receive the
/// transferred pixels.
pub unsafe fn transfer_to_sw(
    hw_frame_ptr: *const c_void,
    sw_frame_ptr: *mut c_void,
) -> Result<(), String> {
    let ret = unsafe {
        ffmpeg::sys::av_hwframe_transfer_data(
            sw_frame_ptr as *mut ffmpeg::sys::AVFrame,
            hw_frame_ptr as *const ffmpeg::sys::AVFrame,
            0,
        )
    };
    if ret < 0 {
        return Err(format!("av_hwframe_transfer_data failed: {ret}"));
    }
    Ok(())
}
