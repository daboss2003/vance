//! Persistent low-latency audio output for scrubbing.
//!
//! Unlike playback (a forward stream of mixed audio), scrubbing wants to
//! hear a short *grain* of audio at the exact playhead position, swapped
//! out as the playhead moves. [`ScrubOutput`] owns one cpal output stream
//! for the life of a scrub session and plays whatever grain was last
//! pushed — replacing (not appending) on each push, so the audio always
//! tracks the latest position with no growing latency.
//!
//! The stream is `!Send`, so it lives on a dedicated owner thread; the
//! handle returned here shares only a small mutex-guarded grain buffer
//! and a stop flag. The cpal callback `try_lock`s that buffer (a real
//! lock would risk priority inversion): on contention it emits one block
//! of silence, which is inaudible during a grain swap.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

/// Errors starting the scrub output.
#[derive(Debug, thiserror::Error)]
pub enum ScrubError {
    /// No default output device.
    #[error("no audio output device")]
    NoDevice,
    /// The device isn't stereo (the grain mixer produces interleaved stereo).
    #[error("output device is not stereo ({0} channels)")]
    NotStereo(usize),
    /// cpal failed to build / start the stream.
    #[error("scrub stream: {0}")]
    Stream(String),
    /// The owner thread couldn't be spawned or died during setup.
    #[error("scrub thread: {0}")]
    Thread(String),
}

/// The currently-playing grain + read cursor.
struct Grain {
    samples: Vec<f32>,
    pos: usize,
}

/// A live scrub audio output. Drop tears down the stream + owner thread.
pub struct ScrubOutput {
    grain: Arc<Mutex<Grain>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Device sample rate — render grains at this rate, interleaved stereo.
    sample_rate: u32,
}

impl ScrubOutput {
    /// Open the default stereo output and start an (initially silent)
    /// scrub stream. Returns once the stream is playing (or errors).
    pub fn start() -> Result<Self, ScrubError> {
        let grain = Arc::new(Mutex::new(Grain {
            samples: Vec::new(),
            pos: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<u32, ScrubError>>();

        let grain_thread = grain.clone();
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("vance-audio.scrub".into())
            .spawn(move || scrub_owner_loop(grain_thread, stop_thread, tx))
            .map_err(|e| ScrubError::Thread(e.to_string()))?;

        match rx.recv() {
            Ok(Ok(sample_rate)) => Ok(Self {
                grain,
                stop,
                handle: Some(handle),
                sample_rate,
            }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Release);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(ScrubError::Thread("owner thread exited during setup".into()))
            }
        }
    }

    /// Device output sample rate; render grains at this rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Replace the playing grain with `samples` (interleaved stereo at
    /// [`Self::sample_rate`]). The old grain is dropped — the output
    /// always tracks the latest position.
    pub fn push_grain(&self, samples: Vec<f32>) {
        let mut g = self.grain.lock();
        g.samples = samples;
        g.pos = 0;
    }

    /// Stop emitting audio (the stream stays open for the next grain).
    pub fn silence(&self) {
        let mut g = self.grain.lock();
        g.samples.clear();
        g.pos = 0;
    }
}

impl Drop for ScrubOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn scrub_owner_loop(
    grain: Arc<Mutex<Grain>>,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Result<u32, ScrubError>>,
) {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        let _ = tx.send(Err(ScrubError::NoDevice));
        return;
    };
    let config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Err(ScrubError::Stream(e.to_string())));
            return;
        }
    };
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    if channels != 2 {
        let _ = tx.send(Err(ScrubError::NotStereo(channels)));
        return;
    }

    let cb_grain = grain.clone();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config.into(),
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // try_lock keeps the real-time callback lock-free on
                // contention (a grain swap) — emit silence that block.
                match cb_grain.try_lock() {
                    Some(mut g) => {
                        let avail = g.samples.len().saturating_sub(g.pos);
                        let n = avail.min(out.len());
                        out[..n].copy_from_slice(&g.samples[g.pos..g.pos + n]);
                        for s in &mut out[n..] {
                            *s = 0.0;
                        }
                        g.pos += n;
                    }
                    None => out.iter_mut().for_each(|s| *s = 0.0),
                }
            },
            |err| tracing::error!(?err, "scrub cpal stream error"),
            None,
        ),
        other => {
            let _ = tx.send(Err(ScrubError::Stream(format!(
                "unsupported sample format {other:?} (expected f32)"
            ))));
            return;
        }
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Err(ScrubError::Stream(e.to_string())));
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = tx.send(Err(ScrubError::Stream(e.to_string())));
        return;
    }
    // Stream is live; hand the sample rate back and keep it alive until
    // the handle is dropped.
    let _ = tx.send(Ok(sample_rate));
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
}
