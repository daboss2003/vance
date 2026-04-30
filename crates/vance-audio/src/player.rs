//! Parent-side audio output player.
//!
//! Owns the cpal output stream and a lock-free producer/consumer ring.
//! A background **puller thread** issues `next_chunk` calls against the
//! audio sidecar and pushes samples into the ring; the cpal callback
//! pulls samples out at the device's pace.
//!
//! Phase 2.3 scope:
//! - cpal default output device, f32 stereo.
//! - **No resampling.** Sidecar must produce samples at a rate the device
//!   accepts (most macOS devices accept 48000; if the file rate differs,
//!   playback will sound off-pitch). Real resampling lands in 2.5 with
//!   the A/V sync work.
//! - Underrun fills with silence and logs a warning.
//! - `current_pts()` returns the most recent chunk-leading PTS the puller
//!   thread has consumed — used by 2.5 as the audio clock for A/V sync.

use crate::client::{AudioClient, AudioClientError};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use thiserror::Error;

/// Audio player errors.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum AudioPlayerError {
    /// No default audio output device.
    #[error("no default audio output device")]
    NoDevice,

    /// cpal couldn't build the output stream.
    #[error("cpal build_output_stream: {0}")]
    Build(String),

    /// cpal couldn't start the stream.
    #[error("cpal play: {0}")]
    Play(String),

    /// Sidecar/IPC error.
    #[error("audio client: {0}")]
    Client(#[from] AudioClientError),

    /// Couldn't spawn the puller thread.
    #[error("spawn puller thread: {0}")]
    Spawn(String),
}

/// Parent-side audio playback. Owns the cpal stream + the puller thread.
///
/// Drop stops the stream and joins the puller thread.
pub struct AudioPlayer {
    /// Holds the cpal `Stream` so it stays alive. Also where we tell
    /// the puller thread to exit on Drop.
    inner: Mutex<Option<Inner>>,
    /// Audio clock: most recent chunk-leading PTS the puller has emitted
    /// into the ring. Used by 2.5 for A/V sync.
    current_pts: Arc<AtomicI64>,
    /// Sample rate the cpal output stream is configured at.
    device_sample_rate: u32,
    /// Audio stream's time base as `(num, den)`. Required to interpret
    /// `current_pts` as seconds for A/V sync.
    stream_time_base: (i64, i64),
}

struct Inner {
    _stream: cpal::Stream,
    stop: Arc<AtomicBool>,
    puller: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Spawn the audio sidecar, open `path`, and start a cpal output stream
    /// fed by a puller thread. Returns once the stream is playing.
    ///
    /// `from_seconds` is the playhead position in absolute file seconds
    /// (i.e. the same units the video side uses, modulo time bases). The
    /// player converts to the audio stream's PTS internally.
    pub fn start(
        sidecar_binary: impl AsRef<Path>,
        media_path: impl Into<String>,
        from_seconds: f64,
    ) -> Result<Self, AudioPlayerError> {
        // 1. Pick the default output device + a stereo f32 config.
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioPlayerError::NoDevice)?;
        let config = device
            .default_output_config()
            .map_err(|e| AudioPlayerError::Build(e.to_string()))?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        tracing::info!(sample_rate, channels, "audio: opened cpal output device");

        // 2. Spawn the audio sidecar and open the file.
        let mut client = AudioClient::spawn(sidecar_binary)?;
        let info = client.open(media_path)?;
        tracing::info!(
            file_rate = info.sample_rate,
            file_channels = info.channels,
            "audio: opened media file"
        );
        // Convert the requested seek (in file seconds) to the audio stream's
        // PTS units. PTS = sec * tb_den / tb_num.
        let (atb_n, atb_d) = info.time_base;
        let from_pts = if atb_n > 0 {
            (from_seconds.max(0.0) * (atb_d as f64) / (atb_n as f64)).round() as i64
        } else {
            0
        };
        tracing::info!(from_seconds, from_pts, "audio: seeking to start");
        let _ = client.start_stream(from_pts, 1024)?;

        // 3. Build a lock-free ring sized to ~250 ms at the device rate.
        let ring_capacity = (sample_rate as usize) * channels / 4;
        let rb = HeapRb::<f32>::new(ring_capacity);
        let (mut producer, mut consumer) = rb.split();

        // 4. Build the cpal output stream. The callback pulls from the
        // consumer; on underrun, fills with silence.
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_output_stream(
                    &config.into(),
                    move |out: &mut [f32], _info| {
                        let pulled = consumer.pop_slice(out);
                        for s in &mut out[pulled..] {
                            *s = 0.0;
                        }
                        if pulled < out.len() {
                            tracing::trace!(requested = out.len(), got = pulled, "audio underrun");
                        }
                    },
                    |err| tracing::error!(?err, "cpal stream error"),
                    None,
                )
                .map_err(|e| AudioPlayerError::Build(e.to_string()))?,
            other => {
                return Err(AudioPlayerError::Build(format!(
                    "unsupported sample format {other:?} (expected f32)"
                )));
            }
        };

        stream
            .play()
            .map_err(|e| AudioPlayerError::Play(e.to_string()))?;

        // 5. Start the puller thread.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let current_pts = Arc::new(AtomicI64::new(from_pts));
        let current_pts_clone = current_pts.clone();

        let puller = thread::Builder::new()
            .name("vance-audio.puller".into())
            .spawn(move || {
                puller_loop(client, &mut producer, stop_clone, current_pts_clone);
            })
            .map_err(|e| AudioPlayerError::Spawn(e.to_string()))?;

        Ok(Self {
            inner: Mutex::new(Some(Inner {
                _stream: stream,
                stop,
                puller: Some(puller),
            })),
            current_pts,
            device_sample_rate: sample_rate,
            stream_time_base: info.time_base,
        })
    }

    /// Audio stream's time base as `(num, den)`. PTS values reported by
    /// `current_pts` are in this unit; seconds = pts * num / den.
    pub fn stream_time_base(&self) -> (i64, i64) {
        self.stream_time_base
    }

    /// Most recently consumed chunk's leading PTS. Used as the audio
    /// clock by the A/V sync work in Phase 2.5.
    pub fn current_pts(&self) -> i64 {
        self.current_pts.load(Ordering::Acquire)
    }

    /// Shared atomic handle to the audio clock, for callers that want to
    /// read it from another thread without going through `&self`.
    pub fn current_pts_handle(&self) -> Arc<AtomicI64> {
        self.current_pts.clone()
    }

    /// Sample rate of the cpal output device.
    pub fn device_sample_rate(&self) -> u32 {
        self.device_sample_rate
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.lock().take() {
            inner.stop.store(true, Ordering::Release);
            if let Some(handle) = inner.puller.take() {
                let _ = handle.join();
            }
            // _stream drops here, stopping cpal output.
        }
    }
}

fn puller_loop(
    mut client: AudioClient,
    producer: &mut ringbuf::HeapProd<f32>,
    stop: Arc<AtomicBool>,
    current_pts: Arc<AtomicI64>,
) {
    while !stop.load(Ordering::Acquire) {
        // Backpressure: if the ring is more than half-full, sleep briefly
        // so the cpal callback can drain.
        if producer.vacant_len() < producer.capacity().get() / 2 {
            thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        match client.next_chunk() {
            Ok(Some(chunk)) => {
                current_pts.store(chunk.pts, Ordering::Release);
                let pushed = producer.push_slice(&chunk.samples);
                if pushed < chunk.samples.len() {
                    tracing::warn!(
                        chunk_len = chunk.samples.len(),
                        pushed,
                        "audio: ring full, dropped tail of chunk"
                    );
                }
            }
            Ok(None) => {
                tracing::info!("audio: end of stream");
                break;
            }
            Err(e) => {
                tracing::error!(?e, "audio: puller error, exiting");
                break;
            }
        }
    }
    let _ = client.stop_stream();
    let _ = client.shutdown();
}
