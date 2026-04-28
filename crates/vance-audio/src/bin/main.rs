//! `vance-audio-decoder` sidecar binary.
//!
//! Decodes audio from a media file via Symphonia and ships interleaved
//! stereo f32 PCM chunks over IPC. Input audio of any channel layout is
//! mixed/duplicated to stereo. Sample rate is reported in `AudioStreamInfo`
//! but **not** resampled by the sidecar — that's the parent's
//! responsibility (Phase 2.5).

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphErr;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::sample::SampleFormat;
use symphonia::core::units::Time;

use vance_audio::protocol::{
    AudioRequest, AudioResponse, AudioStreamInfo, PcmChunk, PROTOCOL_VERSION,
};

fn main() -> vance_ipc::Result<()> {
    let mut state = State::default();
    vance_ipc::serve::<AudioRequest, AudioResponse, _>(|req| state.handle(req))
}

#[derive(Default)]
struct State {
    open: Option<OpenFile>,
    streaming: Option<StreamingState>,
}

struct OpenFile {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    #[allow(dead_code)]
    info: AudioStreamInfo,
}

struct StreamingState {
    rx: Receiver<Option<DecodedItem>>,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<OpenFile, String>>,
}

enum DecodedItem {
    Chunk(PcmChunk),
    Failure(String),
}

impl State {
    fn handle(&mut self, req: AudioRequest) -> AudioResponse {
        match req {
            AudioRequest::Hello {
                protocol_version: _,
            } => AudioResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                decoder: format!("symphonia {}", env!("CARGO_PKG_VERSION")),
            },
            AudioRequest::Open { path } => {
                self.stop_stream_if_any();
                match open_file(&path) {
                    Ok((file, info)) => {
                        self.open = Some(file);
                        AudioResponse::Opened(info)
                    }
                    Err(e) => AudioResponse::OpenFailed(e),
                }
            }
            AudioRequest::StartStream {
                from_pts,
                chunk_frames,
            } => {
                self.stop_stream_if_any();
                match self.start_stream(from_pts, chunk_frames) {
                    Ok(first_pts) => AudioResponse::Streaming { first_pts },
                    Err(e) => AudioResponse::DecodeFailed { reason: e },
                }
            }
            AudioRequest::NextChunk => self.next_chunk(),
            AudioRequest::StopStream => {
                self.stop_stream_if_any();
                AudioResponse::Stopped
            }
            AudioRequest::Close => {
                self.stop_stream_if_any();
                self.open = None;
                AudioResponse::Closed
            }
            AudioRequest::Shutdown => {
                self.stop_stream_if_any();
                self.open = None;
                AudioResponse::Goodbye
            }
        }
    }

    fn start_stream(&mut self, from_pts: i64, chunk_frames: u32) -> Result<i64, String> {
        let mut file = self.open.take().ok_or("no file open")?;
        // Seek the demuxer/decoder to `from_pts`.
        if let Err(e) = file.reader.seek(
            SeekMode::Coarse,
            SeekTo::TimeStamp {
                ts: from_pts.max(0) as u64,
                track_id: file.track_id,
            },
        ) {
            self.open = Some(file);
            return Err(format!("seek: {e}"));
        }
        file.decoder.reset();

        let chunk_frames = chunk_frames.clamp(64, 8192) as usize;
        let (tx, rx) = sync_channel::<Option<DecodedItem>>(8);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = thread::Builder::new()
            .name("vance-audio.stream".into())
            .spawn(move || stream_loop(file, tx, stop_clone, from_pts, chunk_frames))
            .map_err(|e| format!("spawn audio decode thread: {e}"))?;

        self.streaming = Some(StreamingState { rx, stop, handle });
        Ok(from_pts)
    }

    fn next_chunk(&mut self) -> AudioResponse {
        let Some(s) = self.streaming.as_ref() else {
            return AudioResponse::DecodeFailed {
                reason: "no stream in progress".into(),
            };
        };
        match s.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(DecodedItem::Chunk(c))) => AudioResponse::Chunk(c),
            Ok(Some(DecodedItem::Failure(reason))) => {
                self.stop_stream_if_any();
                AudioResponse::DecodeFailed { reason }
            }
            Ok(None) => {
                self.stop_stream_if_any();
                AudioResponse::EndOfStream
            }
            Err(RecvTimeoutError::Timeout) => AudioResponse::DecodeFailed {
                reason: "audio decode thread starved (5s timeout)".into(),
            },
            Err(RecvTimeoutError::Disconnected) => {
                self.stop_stream_if_any();
                AudioResponse::DecodeFailed {
                    reason: "audio decode thread exited".into(),
                }
            }
        }
    }

    fn stop_stream_if_any(&mut self) {
        if let Some(s) = self.streaming.take() {
            s.stop.store(true, Ordering::Release);
            while s.rx.try_recv().is_ok() {}
            match s.handle.join() {
                Ok(Ok(file)) => self.open = Some(file),
                Ok(Err(e)) => eprintln!("[audio] decode thread error: {e}"),
                Err(_) => eprintln!("[audio] decode thread panicked"),
            }
        }
    }
}

fn open_file(path: &str) -> Result<(OpenFile, AudioStreamInfo), String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(format!("not found: {path}"));
    }
    let file = File::open(p).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe: {e}"))?;

    let reader = probed.format;
    let track = reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or("no decodable audio track")?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let codec_name = format!("{:?}", codec_params.codec).to_lowercase();
    let sample_rate = codec_params.sample_rate.unwrap_or(0);
    if sample_rate == 0 {
        return Err("audio track has unknown sample rate".into());
    }
    let time_base = codec_params
        .time_base
        .map(|tb| (tb.numer as i64, tb.denom as i64))
        .unwrap_or((1, sample_rate as i64));
    let duration_pts = codec_params.n_frames.map(|n| n as i64);

    let decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| format!("make decoder: {e}"))?;

    let info = AudioStreamInfo {
        sample_rate,
        channels: 2, // we always output stereo
        time_base,
        codec_name,
        duration_pts,
    };

    eprintln!(
        "[audio] open ok: {} {}Hz, dur={:?}",
        info.codec_name, sample_rate, duration_pts
    );

    Ok((
        OpenFile {
            reader,
            decoder,
            track_id,
            info: info.clone(),
        },
        info,
    ))
}

/// Background decode loop. Reads packets, decodes to PCM, mixes to stereo
/// f32, accumulates into `chunk_frames`-sized chunks, ships them.
fn stream_loop(
    mut file: OpenFile,
    tx: SyncSender<Option<DecodedItem>>,
    stop: Arc<AtomicBool>,
    from_pts: i64,
    chunk_frames: usize,
) -> Result<OpenFile, String> {
    let mut accum: Vec<f32> = Vec::with_capacity(chunk_frames * 2);
    let mut accum_pts: Option<i64> = None;
    let track_id = file.track_id;

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let packet = match file.reader.next_packet() {
            Ok(p) => p,
            Err(SymphErr::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Flush remaining and EOS.
                if !accum.is_empty() {
                    let _ = tx.send(Some(DecodedItem::Chunk(PcmChunk {
                        pts: accum_pts.unwrap_or(0),
                        samples: std::mem::take(&mut accum),
                    })));
                }
                let _ = tx.send(None);
                break;
            }
            Err(e) => {
                let _ = tx.send(Some(DecodedItem::Failure(format!("next_packet: {e}"))));
                return Ok(file);
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let pts = packet.ts() as i64;
        if pts < from_pts {
            continue;
        }
        let decoded = match file.decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphErr::DecodeError(_)) => continue, // recoverable; skip
            Err(e) => {
                let _ = tx.send(Some(DecodedItem::Failure(format!("decode: {e}"))));
                return Ok(file);
            }
        };

        // Mix to interleaved stereo f32 and append to accum.
        accum_pts.get_or_insert(pts);
        append_as_stereo_f32(decoded, &mut accum);

        // Flush full chunks.
        let stride = chunk_frames * 2; // stereo
        while accum.len() >= stride {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let mut out = Vec::with_capacity(stride);
            out.extend(accum.drain(..stride));
            let chunk = PcmChunk {
                pts: accum_pts.take().unwrap_or(pts),
                samples: out,
            };
            // Reset PTS marker for the remaining accum.
            accum_pts = Some(pts);
            if tx.send(Some(DecodedItem::Chunk(chunk))).is_err() {
                return Ok(file);
            }
        }
    }
    Ok(file)
}

/// Convert a Symphonia decoded buffer to interleaved stereo f32 and append
/// to `out`. Mono → duplicated to L+R; multichannel → first two channels.
fn append_as_stereo_f32(buf: AudioBufferRef<'_>, out: &mut Vec<f32>) {
    macro_rules! push_sample_typed {
        ($buf:expr, $convert:expr) => {{
            let frames = $buf.frames();
            let channels = $buf.spec().channels.count();
            for f in 0..frames {
                let l = $convert($buf.chan(0)[f]);
                let r = if channels >= 2 {
                    $convert($buf.chan(1)[f])
                } else {
                    l
                };
                out.push(l);
                out.push(r);
            }
        }};
    }
    match buf {
        AudioBufferRef::F32(b) => push_sample_typed!(b, |s: f32| s),
        AudioBufferRef::F64(b) => push_sample_typed!(b, |s: f64| s as f32),
        AudioBufferRef::S16(b) => push_sample_typed!(b, |s: i16| s as f32 / 32768.0),
        AudioBufferRef::S32(b) => push_sample_typed!(b, |s: i32| s as f32 / 2_147_483_648.0),
        AudioBufferRef::U8(b) => push_sample_typed!(b, |s: u8| (s as f32 - 128.0) / 128.0),
        AudioBufferRef::U16(b) => push_sample_typed!(b, |s: u16| (s as f32 - 32768.0) / 32768.0),
        AudioBufferRef::U32(b) => {
            push_sample_typed!(b, |s: u32| (s as f64 - 2_147_483_648.0) as f32
                / 2_147_483_648.0)
        }
        AudioBufferRef::S24(b) => push_sample_typed!(b, |s: symphonia::core::sample::i24| {
            s.inner() as f32 / 8_388_608.0
        }),
        AudioBufferRef::U24(b) => push_sample_typed!(b, |s: symphonia::core::sample::u24| {
            (s.inner() as f32 - 8_388_608.0) / 8_388_608.0
        }),
        AudioBufferRef::S8(b) => push_sample_typed!(b, |s: i8| s as f32 / 128.0),
    }
    let _ = SampleFormat::F32; // keep symphonia imports honest
    let _ = Time::default();
}
