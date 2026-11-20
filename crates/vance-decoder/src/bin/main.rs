//! `vance-decoder` sidecar binary.
//!
//! Wraps FFmpeg behind the protocol in `vance_decoder::protocol`. Software
//! decode, plus VideoToolbox hardware decode on macOS for the codecs it
//! handles.
//!
//! - `main` (this file): the request loop and its handlers.
//! - `media`: open / seek / decode-at and the background stream decode loop.
//! - `pack`: pixel conversion and packing a decoded frame into a buffer.
//! - `alloc`: where those buffers come from.
//!
//! ## Threading
//!
//! - The main thread runs `vance_ipc::serve_with_blobs()` and processes one
//!   request at a time. A `Frame` reply's pixels follow it as a raw blob.
//! - During streaming, a background **decode thread** owns the FFmpeg
//!   `Input` / `Video` decoder and pushes decoded frames into a bounded
//!   channel. The main thread `recv()`s from that channel when a
//!   `NextFrame` request arrives.
//! - `StopStream` (or `Close` / `Shutdown` during a stream) signals the
//!   decode thread, joins it, and reclaims the `OpenContext` for further
//!   single-shot `Seek` / `DecodeAt` operations.

mod alloc;
mod media;
mod pack;

use alloc::Allocator;
use media::{DecodedItem, OpenContext, Stream};
use pack::PackedFrame;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use vance_decoder::protocol::{
    DecoderRequest, DecoderResponse, OpenError, StreamInfo, PROTOCOL_VERSION,
};
use vance_ipc::Reply;

fn main() -> vance_ipc::Result<()> {
    vance_ffmpeg_sys::init_once();
    let mut state = State::default();
    vance_ipc::serve_with_blobs::<DecoderRequest, DecoderResponse, _>(|req, _blobs| {
        state.handle(req)
    })
}

#[derive(Default)]
struct State {
    /// Owned exclusively by the main thread while no stream is active.
    /// Moved INTO the decode thread for the duration of a stream, then
    /// reclaimed via the join handle when the stream stops.
    open: Option<OpenContext>,
    /// Set whenever a stream is in progress.
    streaming: Option<Stream>,
    /// Where packed frames are written.
    alloc: Allocator,
}

/// A `Frame` reply, with the pixels as its blob when they go inline.
fn frame_reply(frame: PackedFrame) -> Reply<DecoderResponse> {
    let (payload, blob) = frame.buf.into_payload();
    Reply {
        msg: DecoderResponse::Frame {
            meta: frame.meta,
            payload,
        },
        blob,
    }
}

impl State {
    fn handle(&mut self, req: DecoderRequest) -> Reply<DecoderResponse> {
        match req {
            DecoderRequest::Hello {
                protocol_version: _,
            } => DecoderResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                ffmpeg_version: ffmpeg_version_string(),
            }
            .into(),
            DecoderRequest::Open { path } => {
                self.stop_stream_if_any();
                match self.open(&path) {
                    Ok(info) => DecoderResponse::Opened(info),
                    Err(e) => DecoderResponse::OpenFailed(e),
                }
                .into()
            }
            DecoderRequest::Seek { target_pts } => {
                self.stop_stream_if_any();
                match self.seek(target_pts) {
                    Ok(pts) => DecoderResponse::Seeked { pts },
                    Err(e) => DecoderResponse::DecodeFailed { reason: e },
                }
                .into()
            }
            DecoderRequest::DecodeAt { target_pts } => {
                self.stop_stream_if_any();
                match self.decode_at(target_pts) {
                    Ok(f) => frame_reply(f),
                    Err(e) => DecoderResponse::DecodeFailed { reason: e }.into(),
                }
            }
            DecoderRequest::StartStream {
                from_pts,
                buffer_frames,
            } => {
                self.stop_stream_if_any();
                match self.start_stream(from_pts, buffer_frames) {
                    Ok(first_pts) => DecoderResponse::Streaming { first_pts },
                    Err(e) => DecoderResponse::DecodeFailed { reason: e },
                }
                .into()
            }
            DecoderRequest::NextFrame => self.next_frame(),
            DecoderRequest::StopStream => {
                self.stop_stream_if_any();
                DecoderResponse::Stopped.into()
            }
            DecoderRequest::Close => {
                self.stop_stream_if_any();
                self.open = None;
                DecoderResponse::Closed.into()
            }
            DecoderRequest::Shutdown => {
                self.stop_stream_if_any();
                self.open = None;
                DecoderResponse::Goodbye.into()
            }
            DecoderRequest::AttachShm(desc) => {
                self.stop_stream_if_any();
                match self.alloc.attach(&desc) {
                    Ok(()) => DecoderResponse::ShmAttached { epoch: desc.epoch },
                    Err(e) => DecoderResponse::ShmUnavailable {
                        reason: e.to_string(),
                    },
                }
                .into()
            }
            DecoderRequest::Release { slots } => DecoderResponse::Released {
                stale: self.alloc.release(&slots),
            }
            .into(),
        }
    }

    // ---- one-shot path (Phase 1 retained) ---------------------------------

    fn open(&mut self, path: &str) -> Result<StreamInfo, OpenError> {
        let ctx = media::open(path)?;
        let info = ctx.info.clone();
        self.open = Some(ctx);
        Ok(info)
    }

    fn seek(&mut self, target_pts: i64) -> Result<i64, String> {
        let ctx = self.open.as_mut().ok_or("no file open")?;
        media::seek(ctx, target_pts)?;
        Ok(target_pts)
    }

    fn decode_at(&mut self, target_pts: i64) -> Result<PackedFrame, String> {
        let ctx = self.open.as_mut().ok_or("no file open")?;
        media::decode_at(ctx, target_pts, &self.alloc)
    }

    // ---- streaming path (Phase 2.1) ---------------------------------------

    fn start_stream(&mut self, from_pts: i64, buffer_frames: u32) -> Result<i64, String> {
        // Take ownership of the open context and move it into the decode thread.
        let ctx = self.open.take().ok_or("no file open")?;
        let stream = media::start_stream(ctx, from_pts, buffer_frames, self.alloc.clone())?;
        self.streaming = Some(stream);
        // We return the requested `from_pts` as the first PTS upper-bound;
        // the actual first frame's PTS will arrive on the channel and the
        // caller will see it via `NextFrame`.
        Ok(from_pts)
    }

    fn next_frame(&mut self) -> Reply<DecoderResponse> {
        let Some(s) = self.streaming.as_ref() else {
            return DecoderResponse::DecodeFailed {
                reason: "no stream in progress".into(),
            }
            .into();
        };
        // Long-ish timeout so wedged decoders surface as errors rather than
        // hanging the parent forever. Real playback should always have a
        // frame within ~1s; if not, something is wrong.
        match s.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(DecodedItem::Frame(f))) => frame_reply(f),
            Ok(Some(DecodedItem::Failure(reason))) => {
                self.stop_stream_if_any();
                DecoderResponse::DecodeFailed { reason }.into()
            }
            Ok(None) => {
                self.stop_stream_if_any();
                DecoderResponse::EndOfStream.into()
            }
            Err(RecvTimeoutError::Timeout) => DecoderResponse::DecodeFailed {
                reason: "decode thread starved (5s timeout)".into(),
            }
            .into(),
            Err(RecvTimeoutError::Disconnected) => {
                self.stop_stream_if_any();
                DecoderResponse::DecodeFailed {
                    reason: "decode thread exited".into(),
                }
                .into()
            }
        }
    }

    fn stop_stream_if_any(&mut self) {
        if let Some(s) = self.streaming.take() {
            if let Some(ctx) = s.stop() {
                self.open = Some(ctx);
            }
        }
    }
}

fn ffmpeg_version_string() -> String {
    use std::ffi::CStr;
    unsafe {
        let p = vance_ffmpeg_sys::ffmpeg::sys::av_version_info();
        if p.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}
