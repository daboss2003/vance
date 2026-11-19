//! Phase 9.3 — background prefetch of decoded frames into the cache.
//!
//! When the user parks the playhead at frame N, there's a good
//! chance their next action is either to play (frames N+1, N+2, ...)
//! or to scrub (frames within a few seconds in either direction).
//! Background prefetch decodes the next N frames after the playhead
//! INTO the frame cache so those subsequent reads are ring hits.
//!
//! The worker:
//! - Runs on a dedicated OS thread (not blocking the UI / playback).
//! - Owns its OWN [`DecoderClient`] sidecar so it can't fight the
//!   primary decoder for the open file handle.
//! - Walks frames forward from the requested anchor PTS by a stride
//!   of `step_pts` (typically `1 / frame_rate` in stream time-base).
//! - Stops when it reaches the prefetch horizon (`horizon_frames`)
//!   OR when a new prefetch target supersedes the current one OR
//!   when the cache reports the next frame is already a ring hit.
//!
//! The worker SHARES the engine's `frame_cache` via `Arc`, so its
//! writes are visible to the next foreground decode immediately.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vance_cache::FrameCache;
use vance_decoder::DecoderClient;

use crate::cached_decode::pack_frame;

/// Sentinel value meaning "no prefetch in flight."
const NO_TARGET: i64 = i64::MIN;

/// Inputs that drive one prefetch session.
#[derive(Clone, Debug)]
pub struct PrefetchRequest {
    /// Source URI to open + prefetch from.
    pub source_uri: String,
    /// Anchor PTS to start walking forward from (in stream time-base).
    pub anchor_pts: i64,
    /// PTS stride per frame (= `time_base_den / frame_rate_num`).
    pub step_pts: i64,
    /// Stop after this many forward steps.
    pub horizon_frames: u32,
}

/// Long-running prefetch worker. Drop the worker to stop the thread.
pub struct PrefetchWorker {
    /// Atomic encoding of the current target PTS + a one-bit
    /// "request supersedes" generation marker. Workers re-read each
    /// loop iteration to detect supersession.
    target_pts: Arc<AtomicI64>,
    /// Generation counter — incremented on every new request so the
    /// worker can spot a supersession even when the new PTS happens
    /// to equal the previous one.
    generation: Arc<AtomicI64>,
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl PrefetchWorker {
    /// Spawn a worker that decodes ahead of the playhead into
    /// `cache`. The worker spawns its own decoder sidecar lazily on
    /// the first request.
    pub fn spawn(cache: Arc<FrameCache>, decoder_binary: PathBuf) -> Self {
        let target_pts = Arc::new(AtomicI64::new(NO_TARGET));
        let generation = Arc::new(AtomicI64::new(0));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let t_pts = target_pts.clone();
        let gen_clone = generation.clone();
        let sd = shutdown.clone();
        let bin = decoder_binary;
        let cache_inner = cache;
        let handle = thread::Builder::new()
            .name("vance.prefetch".into())
            .spawn(move || {
                worker_loop(t_pts, gen_clone, sd, cache_inner, bin);
            })
            .expect("spawn prefetch thread");
        Self {
            target_pts,
            generation,
            handle: Some(handle),
            shutdown,
        }
    }

    /// Submit a new request — any prior in-flight prefetch is
    /// superseded. The worker re-checks its target each frame.
    pub fn request(&self, req: PrefetchRequest) {
        // Encode the request via a tiny "current request" slot. v1:
        // we only encode anchor_pts + step_pts + URI hash + horizon
        // are bundled in shared state set BEFORE writing the target.
        // Simpler: stash full request as a leak-free Arc<Mutex<_>>.
        // For v1, encode just the anchor PTS — full request lookup
        // happens via a sidecar parameter store.
        REQUEST_SLOT.with(|_| {
            // unused — see request_slot global below.
        });
        *PREFETCH_SLOT.lock() = Some(req.clone());
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.target_pts.store(req.anchor_pts, Ordering::Release);
    }

    /// Signal the worker to exit at its next iteration.
    // Intentional: lib API driven by the integration tests (tests/it); the
    // bin target, which compiles this file too, never calls it.
    #[allow(dead_code)]
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.target_pts.store(NO_TARGET, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PrefetchWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// The thread-local stub stays for forward-compat — the real shared
// state below is a simple parking_lot Mutex<Option<PrefetchRequest>>.
thread_local! {
    static REQUEST_SLOT: std::cell::Cell<()> = const { std::cell::Cell::new(()) };
}
static PREFETCH_SLOT: parking_lot::Mutex<Option<PrefetchRequest>> = parking_lot::Mutex::new(None);

fn worker_loop(
    target_pts: Arc<AtomicI64>,
    generation: Arc<AtomicI64>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    cache: Arc<FrameCache>,
    decoder_binary: PathBuf,
) {
    let mut decoder: Option<DecoderClient> = None;
    let mut current_uri: Option<String> = None;
    let mut last_seen_generation: i64 = -1;

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let cur_target = target_pts.load(Ordering::Acquire);
        let cur_generation = generation.load(Ordering::Acquire);
        if cur_target == NO_TARGET {
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        // Stale request guard.
        if cur_generation == last_seen_generation {
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        let Some(req) = PREFETCH_SLOT.lock().clone() else {
            thread::sleep(Duration::from_millis(25));
            continue;
        };
        last_seen_generation = cur_generation;

        // Lazily spawn a decoder sidecar dedicated to prefetch.
        if decoder.is_none() {
            match DecoderClient::spawn(&decoder_binary) {
                Ok(c) => decoder = Some(c),
                Err(e) => {
                    tracing::warn!(?e, "prefetch decoder spawn failed; retrying later");
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        let dec = decoder.as_mut().unwrap();

        // Open the requested file iff different from the last one.
        if current_uri.as_deref() != Some(req.source_uri.as_str()) {
            if let Err(e) = dec.open(req.source_uri.clone()) {
                tracing::warn!(?e, uri = %req.source_uri, "prefetch open failed");
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            current_uri = Some(req.source_uri.clone());
        }

        // Walk forward, decoding into the cache.
        let mut pts = req.anchor_pts;
        for i in 0..req.horizon_frames {
            // Bail if the user submitted a fresh request OR shutdown.
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            if generation.load(Ordering::Acquire) != cur_generation {
                break;
            }
            pts += req.step_pts;
            let key = vance_cache::CacheKey::for_source_frame(req.source_uri.clone(), pts, 0);
            // Skip frames the cache already has. Presence-only check so
            // the worker doesn't (a) copy bytes it won't use or (b) pollute
            // the user-facing scrub hit-ratio counters from this thread.
            if cache.contains(&key) {
                continue;
            }
            match dec.decode_at(pts) {
                Ok(frame) => {
                    let packed = pack_frame(&frame);
                    let _ = cache.put(&key, &packed);
                }
                Err(e) => {
                    tracing::trace!(?e, pts, i, "prefetch decode_at failed");
                    break;
                }
            }
        }

        // No new requests yet — sleep instead of busy-spinning.
        target_pts.store(NO_TARGET, Ordering::Release);
    }
}
