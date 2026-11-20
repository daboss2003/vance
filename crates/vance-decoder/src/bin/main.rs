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
//!   request at a time. Each arrives in a `DecoderEnvelope` whose released
//!   slots go back to the pool before the request is handled. A `Frame`
//!   reply's pixels are in a shared-memory slot, or follow the reply as a
//!   raw blob.
//! - During streaming, a background **decode thread** owns the FFmpeg
//!   `Input` / `Video` decoder and pushes decoded frames into a bounded
//!   channel. The main thread `recv()`s from that channel when a
//!   `NextFrame` request arrives.
//! - `StopStream` (or `Close` / `Shutdown` during a stream) signals the
//!   decode thread, joins it, and reclaims the `OpenContext` for further
//!   single-shot `Seek` / `DecodeAt` operations.
//!
//! ## Shared-memory frames (ADR 0006)
//!
//! After `AttachShm`, frames are packed straight into slots of the parent's
//! region. A slot stays unpublished while it waits in the stream channel and
//! is published only when the main thread builds the `Frame` reply, so the
//! channel's drain on stop hands unsent slots back to the pool. The main
//! thread never waits for a slot (it is also the thread that takes released
//! slots back): with none free, its frame goes inline. The decode thread
//! waits for one until its stream is stopped.

mod alloc;
mod media;
mod pack;

use alloc::Allocator;
use media::{DecodedItem, OpenContext, Stream};
use pack::PackedFrame;
use std::io::Write;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use vance_decoder::protocol::{
    DecoderEnvelope, DecoderRequest, DecoderResponse, OpenError, StreamInfo, PROTOCOL_VERSION,
};
use vance_ipc::Reply;

fn main() -> vance_ipc::Result<()> {
    vance_ffmpeg_sys::init_once();
    let mut state = State::default();
    let mut served_any = false;
    let served =
        vance_ipc::serve_with_blobs::<DecoderEnvelope, DecoderResponse, _>(|envelope, _blobs| {
            served_any = true;
            state.handle(envelope)
        });
    if let Err(vance_ipc::Error::Decode(e)) = &served {
        eprintln!(
            "[decoder] unreadable request (this sidecar speaks protocol v{PROTOCOL_VERSION}): {e}"
        );
        if !served_any {
            answer_unreadable_handshake();
        }
    }
    served
}

/// Reply to an unreadable first request with this sidecar's `Hello`.
///
/// Intentional: answering a request that was never understood. It is all
/// but certainly the `Hello` of a client on another protocol version, which
/// waits for a `Hello` back (every version keeps it as the first response
/// variant, with the same fields). Its handshake then fails as a version
/// mismatch instead of a dead sidecar. The sidecar exits right after.
fn answer_unreadable_handshake() {
    let hello = DecoderResponse::Hello {
        protocol_version: PROTOCOL_VERSION,
        ffmpeg_version: ffmpeg_version_string(),
    };
    let mut out = std::io::stdout().lock();
    if vance_ipc::write_frame(&mut out, &hello).is_ok() {
        let _ = out.flush();
    }
}

/// Log that `stale` released slots were ignored: another region's epoch,
/// already released, or an older generation. The decoder's tests parse
/// this line to count stale releases.
fn log_stale_releases(stale: u32) {
    let line = format!("[decoder] ignored {stale} stale shared-memory slot releases\n");
    // One write, so the line stays whole next to FFmpeg's own stderr output.
    let _ = std::io::stderr().write_all(line.as_bytes());
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

/// A `Frame` reply, with the pixels as its blob when they go inline. A
/// slot is published here, and belongs to the parent from now on.
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
    /// Take the envelope's released slots back, then handle its request.
    fn handle(&mut self, envelope: DecoderEnvelope) -> Reply<DecoderResponse> {
        let DecoderEnvelope { releases, request } = envelope;
        if !releases.is_empty() {
            let stale = self.alloc.release(&releases);
            if stale > 0 {
                log_stale_releases(stale);
            }
        }
        self.handle_request(request)
    }

    fn handle_request(&mut self, req: DecoderRequest) -> Reply<DecoderResponse> {
        match req {
            DecoderRequest::Hello { protocol_version } => {
                // The client compares the versions and gives up on a mismatch.
                if protocol_version != PROTOCOL_VERSION {
                    eprintln!(
                        "[decoder] protocol mismatch: client v{protocol_version}, sidecar v{PROTOCOL_VERSION}"
                    );
                }
                DecoderResponse::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    ffmpeg_version: ffmpeg_version_string(),
                }
                .into()
            }
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
                // Returns every slot still queued for the old stream first.
                self.stop_stream_if_any();
                match self.alloc.attach(&desc) {
                    Ok(()) => {
                        eprintln!(
                            "[decoder] shared memory attached: epoch {}, {} slots of {} bytes",
                            desc.epoch, desc.slot_count, desc.slot_size
                        );
                        DecoderResponse::ShmAttached { epoch: desc.epoch }
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        eprintln!(
                            "[decoder] shared memory unavailable, frames go inline: {reason}"
                        );
                        DecoderResponse::ShmUnavailable { reason }
                    }
                }
                .into()
            }
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
