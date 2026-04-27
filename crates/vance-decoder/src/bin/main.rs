//! `vance-decoder` sidecar binary.
//!
//! Wraps FFmpeg behind the protocol in `vance_decoder::protocol`. Software
//! decode only in Milestone 1.2; hardware decode (VideoToolbox / NVDEC /
//! VAAPI / D3D11VA) lands in Milestone 1.6.
//!
//! Lives in this crate so the parent and child share the wire types
//! verbatim (no manual schema sync).

use std::path::Path;
use vance_decoder::protocol::{
    DecoderRequest, DecoderResponse, FrameData, OpenError, PixelFormat, StreamInfo,
    PROTOCOL_VERSION,
};
use vance_ffmpeg_sys::ffmpeg;

fn main() -> vance_ipc::Result<()> {
    vance_ffmpeg_sys::init_once();
    let mut state = State::default();
    vance_ipc::serve::<DecoderRequest, DecoderResponse, _>(|req| state.handle(req))
}

#[derive(Default)]
struct State {
    open: Option<OpenContext>,
}

struct OpenContext {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    info: StreamInfo,
}

impl State {
    fn handle(&mut self, req: DecoderRequest) -> DecoderResponse {
        match req {
            DecoderRequest::Hello {
                protocol_version: _,
            } => DecoderResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                ffmpeg_version: ffmpeg_version_string(),
            },
            DecoderRequest::Open { path } => match self.open(&path) {
                Ok(info) => DecoderResponse::Opened(info),
                Err(e) => DecoderResponse::OpenFailed(e),
            },
            DecoderRequest::Seek { target_pts } => match self.seek(target_pts) {
                Ok(pts) => DecoderResponse::Seeked { pts },
                Err(e) => DecoderResponse::DecodeFailed { reason: e },
            },
            DecoderRequest::DecodeAt { target_pts } => match self.decode_at(target_pts) {
                Ok(f) => DecoderResponse::Frame(f),
                Err(e) => DecoderResponse::DecodeFailed { reason: e },
            },
            DecoderRequest::Close => {
                self.open = None;
                DecoderResponse::Closed
            }
            DecoderRequest::Shutdown => {
                self.open = None;
                // Returning `Goodbye` lets the parent see a clean reply
                // before stdin EOF closes our serve loop.
                DecoderResponse::Goodbye
            }
        }
    }

    fn open(&mut self, path: &str) -> Result<StreamInfo, OpenError> {
        let p = Path::new(path);
        if !p.exists() {
            return Err(OpenError::NotFound(path.to_string()));
        }

        let input = ffmpeg::format::input(p).map_err(|e| OpenError::FfmpegError(e.to_string()))?;

        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or(OpenError::NoVideoStream)?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let avg_rate = stream.avg_frame_rate();
        let duration = stream.duration();
        let codec_id = stream.parameters().id();
        let codec_name = format!("{codec_id:?}").to_lowercase();

        let codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| OpenError::FfmpegError(e.to_string()))?;
        let decoder = codec_ctx
            .decoder()
            .video()
            .map_err(|e| OpenError::UnsupportedCodec(format!("{codec_name}: {e}")))?;

        let pixel_format =
            map_pixel_format(decoder.format()).map_err(OpenError::UnsupportedPixelFormat)?;

        let info = StreamInfo {
            width: decoder.width(),
            height: decoder.height(),
            pixel_format,
            frame_rate: (avg_rate.numerator() as i64, avg_rate.denominator() as i64),
            time_base: (time_base.numerator() as i64, time_base.denominator() as i64),
            duration_pts: if duration > 0 { Some(duration) } else { None },
            codec_name,
            stream_index: stream_index as u32,
        };

        self.open = Some(OpenContext {
            input,
            decoder,
            stream_index,
            info: info.clone(),
        });
        Ok(info)
    }

    fn seek(&mut self, target_pts: i64) -> Result<i64, String> {
        let ctx = self.open.as_mut().ok_or("no file open")?;
        // ffmpeg-the-third's `seek` calls `avformat_seek_file` with stream
        // index = -1, which means the timestamp is in AV_TIME_BASE units
        // (microseconds), not the stream's time base. Convert here.
        let (tb_num, tb_den) = ctx.info.time_base;
        let target_us = if tb_den != 0 {
            // pts * tb_num / tb_den == seconds; * 1_000_000 == microseconds.
            // Order multiplications carefully to avoid i64 overflow on long files.
            target_pts.saturating_mul(tb_num).saturating_mul(1_000_000) / tb_den
        } else {
            target_pts
        };
        // Range needs to *include* the target to be a valid seek window.
        // Seek backward to the nearest keyframe at or before `target_us`.
        ctx.input
            .seek(target_us, i64::MIN..=target_us)
            .map_err(|e| format!("seek: {e}"))?;
        ctx.decoder.flush();
        Ok(target_pts)
    }

    fn decode_at(&mut self, target_pts: i64) -> Result<FrameData, String> {
        // Always seek to the requested PTS before decoding so successive
        // calls don't accumulate state. Optimization (skip seek if already
        // close) lands in Milestone 1.5 with the cache.
        self.seek(target_pts)?;

        let ctx = self.open.as_mut().ok_or("no file open")?;
        let mut decoded = ffmpeg::frame::Video::empty();

        // Drain packets from the chosen stream into the decoder until it
        // emits a frame at or after `target_pts`.
        let stream_index = ctx.stream_index;
        // ffmpeg's `Input::packets()` borrows the input mutably, so we have
        // to be careful — collect packets ourselves.
        loop {
            let mut got_packet = false;
            // Read one packet at a time so we can break/continue cleanly.
            for item in ctx.input.packets() {
                let (stream, packet) = item.map_err(|e| format!("read packet: {e}"))?;
                if stream.index() != stream_index {
                    continue;
                }
                got_packet = true;
                ctx.decoder
                    .send_packet(&packet)
                    .map_err(|e| format!("send_packet: {e}"))?;
                // Pull as many frames as the decoder will emit for this packet.
                while ctx.decoder.receive_frame(&mut decoded).is_ok() {
                    let pts = decoded.pts().unwrap_or(0);
                    if pts >= target_pts {
                        return frame_to_payload(&decoded, ctx.info.pixel_format);
                    }
                }
                break; // process more packets in next loop iteration
            }
            if !got_packet {
                // EOF: drain the decoder.
                ctx.decoder
                    .send_eof()
                    .map_err(|e| format!("send_eof: {e}"))?;
                while ctx.decoder.receive_frame(&mut decoded).is_ok() {
                    let pts = decoded.pts().unwrap_or(0);
                    if pts >= target_pts {
                        return frame_to_payload(&decoded, ctx.info.pixel_format);
                    }
                }
                return Err(format!("no frame at or after pts {target_pts}"));
            }
        }
    }
}

fn ffmpeg_version_string() -> String {
    // av_version_info() is available via the sys crate.
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

fn map_pixel_format(pf: ffmpeg::format::Pixel) -> Result<PixelFormat, String> {
    use ffmpeg::format::Pixel as P;
    match pf {
        P::YUV420P | P::YUVJ420P => Ok(PixelFormat::Yuv420P),
        P::NV12 => Ok(PixelFormat::Nv12),
        other => Err(format!("{other:?}")),
    }
}

fn frame_to_payload(
    frame: &ffmpeg::frame::Video,
    expected_pf: PixelFormat,
) -> Result<FrameData, String> {
    let pts = frame.pts().unwrap_or(0);
    let width = frame.width();
    let height = frame.height();

    // Re-validate format in case the decoder switched mid-stream.
    let actual = map_pixel_format(frame.format()).map_err(|s| format!("pixel format: {s}"))?;
    if actual != expected_pf {
        return Err(format!(
            "pixel format changed mid-stream: expected {expected_pf:?}, got {actual:?}"
        ));
    }

    // Pack planes back-to-back, recording strides.
    // YUV420P: planes Y (h rows, stride[0]), U (h/2 rows, stride[1]), V (h/2 rows, stride[2]).
    // NV12:    planes Y (h rows, stride[0]), UV interleaved (h/2 rows, stride[1]).
    let plane_count = match expected_pf {
        PixelFormat::Yuv420P => 3,
        PixelFormat::Nv12 => 2,
    };

    let mut strides = Vec::with_capacity(plane_count);
    let mut bytes = Vec::new();
    for plane in 0..plane_count {
        let stride = frame.stride(plane) as u32;
        strides.push(stride);
        let plane_height = if plane == 0 {
            height
        } else {
            height.div_ceil(2)
        };
        let plane_width = if plane == 0 || expected_pf == PixelFormat::Nv12 && plane == 1 {
            // Y plane: width pixels, 1 byte each. UV plane in NV12: width pixels (interleaved).
            width
        } else {
            // U or V plane in YUV420P: half width.
            width.div_ceil(2)
        };
        let data = frame.data(plane);
        for row in 0..plane_height {
            let start = (row as usize) * (stride as usize);
            let end = start + plane_width as usize;
            bytes.extend_from_slice(&data[start..end]);
        }
        // After packing, the strides we report are now `plane_width` (no padding).
        strides[plane] = plane_width;
    }

    Ok(FrameData {
        pts,
        width,
        height,
        pixel_format: expected_pf,
        strides,
        bytes,
    })
}
