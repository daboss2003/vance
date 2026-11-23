//! The decoded-frame providers behind [`compose_frame`] (ADR 0007).
//!
//! - [`ScrubFetcher`]: scrub frames per layer on workers; a slow layer holds
//!   its last frame. `ScrubProvider` (tests only, feature `test-support`)
//!   waits for every layer instead.
//! - [`StreamProvider`]: per-clip streaming feeds — `Realtime` (playback)
//!   may drop or hold frames, `Blocking` (export) never drops.
//!
//! All of them pick frames by the same rule (see
//! [`crate::compose::FrameProvider`]) and round with
//! [`crate::decoder_pool::source_seconds_to_pts`]: the first frame with
//! `pts >= target`, else (every frame is before the target) the last frame.
//!
//! ## Feeds
//!
//! A feed is one clip's stream: a pooled decoder streaming from the first
//! requested target, run by a worker thread so a slow decoder never blocks
//! the caller. The worker keeps [`QUEUE_FRAMES`] frame ready on top of the
//! sidecar's own decode-ahead; with the frame the feed holds, that is the
//! client's default two leases, so frames don't spill to copies. A request
//! for a target before the held frame's range, or more than a second past
//! it, restarts the stream there. A feed no request or `prepare` has named
//! for [`RETIRE_AFTER`] `frames` calls goes back to the pool.
//!
//! Frames are handed out as zero-copy shares ([`FrameData::share`]) of the
//! held frame. A share keeps its lease until every owner drops it, so the
//! caller drops each frame once it is drawn (before the next `frames`
//! call); the feed then never holds more than its two leases.
//!
//! A feed that couldn't start is retried on its next request (the pool
//! caches open failures until the file changes, so that is cheap). One
//! whose stream failed is retried on its next request (`Blocking`), or
//! after [`RETRY_FAILED_AFTER`] (`Realtime`, which shows the failed feed's
//! last frame meanwhile and until the retry has one): a decode error
//! restarts the same worker and sidecar, a dead sidecar gets a new feed.
//!
//! [`compose_frame`]: crate::compose::compose_frame

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use vance_cache::FrameCache;
use vance_decoder::client::ClientError;
use vance_decoder::{DecoderClient, FrameData, SidecarKill, StreamInfo};
use vance_timeline::ClipId;

use crate::cached_decode::cached_decode_at;
use crate::compose::{FrameProvider, FrameRequest};
use crate::decoder_pool::{
    duplicate_error, is_fatal, source_seconds_to_pts, DecoderPool, OpenAttempt, PooledDecoder,
    Reservation,
};
use crate::engine::EngineError;

/// Frames a feed's worker keeps decoded ahead of the caller.
const QUEUE_FRAMES: usize = 1;
/// The sidecar's decode-ahead window (`start_stream`'s `buffer_frames`),
/// as in the single-clip playback path. Its slot ring bounds it further.
const STREAM_BUFFER_FRAMES: u32 = 8;
/// `Realtime`: how long one `frames` call waits, in total, for frames that
/// are late before holding the last ones.
const REALTIME_GRACE: Duration = Duration::from_millis(4);
/// `Realtime`: how long one `frames` call waits for a feed's first frame
/// (there is nothing to hold yet).
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(1);
/// `Blocking`: a frame not ready after this means the decoder is stuck.
const BLOCKING_WAIT: Duration = Duration::from_secs(30);
/// `frames` calls a feed may go unnamed before it is retired.
const RETIRE_AFTER: u64 = 2;
/// A forward jump past this restarts a stream instead of decoding through.
const JUMP_SECONDS: f64 = 1.0;
/// `Realtime`: how long a failed feed holds its last frame before it is
/// retried (respawning a sidecar is a hitch; a file that keeps crashing
/// it shouldn't cost one every frame).
const RETRY_FAILED_AFTER: Duration = Duration::from_secs(1);
/// `into_pool`: how long stopped workers get to finish their current
/// request before their sidecars are killed.
const STOP_WAIT: Duration = Duration::from_millis(500);
/// `into_pool`: how long a worker whose sidecar was killed gets to notice.
const KILL_WAIT: Duration = Duration::from_millis(500);

/// Where to stream from to find the last frame when every frame is before
/// `target`: a second before the end, further back, then the start.
fn tail_starts(target: i64, info: &StreamInfo) -> Vec<i64> {
    let second = source_seconds_to_pts(1.0, info.time_base).max(1);
    let end = info
        .duration_pts
        .filter(|&d| d > 0)
        .map_or(target, |d| d.min(target));
    let mut starts: Vec<i64> = [1, 4, 16, 64]
        .iter()
        .map(|&n| end.saturating_sub(second.saturating_mul(n)))
        .chain([i64::MIN])
        .collect();
    starts.dedup();
    starts
}

/// The file's last frame, streamed from the tail. `None` if it has none.
fn last_frame(
    client: &mut DecoderClient,
    info: &StreamInfo,
    target: i64,
) -> Result<Option<FrameData>, ClientError> {
    for from in tail_starts(target, info) {
        client.start_stream(from, STREAM_BUFFER_FRAMES)?;
        let mut last = None;
        // Each assignment ends the previous lease: at most two are held.
        while let Some(f) = client.next_frame()? {
            last = Some(f);
        }
        if last.is_some() {
            return Ok(last);
        }
    }
    Ok(None)
}

/// Scrub / single-frame provider: pooled `decode_at` + the frame cache,
/// waiting for every layer. The app scrubs through [`ScrubFetcher`]; this is
/// for tests (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
// Intentional: main.rs compiles this module into the bin as well; in a
// test build the bin gets `test-support` too (feature unification) but never
// uses this test-only API. The app build has neither the item nor the allow.
#[cfg_attr(feature = "test-support", allow(dead_code))]
pub struct ScrubProvider<'a> {
    /// Decoders to borrow from.
    pub pool: &'a mut DecoderPool,
    /// Shared frame cache.
    pub cache: Arc<FrameCache>,
}

/// One file's requests in a scrub `frames` call, decoded on one decoder.
#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(feature = "test-support", allow(dead_code))]
struct ScrubJob {
    dec: PooledDecoder,
    /// Indexes into the requests.
    idx: Vec<usize>,
    /// The file's last-frame PTS, known or learned here.
    last_pts: Option<i64>,
    results: Vec<(usize, Result<FrameData, ClientError>)>,
    /// The sidecar failed: drop it instead of returning it to the pool.
    fatal: bool,
}

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(feature = "test-support", allow(dead_code))]
impl ScrubJob {
    fn run(&mut self, reqs: &[FrameRequest], cache: &FrameCache) {
        let mut idx = std::mem::take(&mut self.idx).into_iter();
        for i in idx.by_ref() {
            let target = source_seconds_to_pts(reqs[i].source_seconds, self.dec.info.time_base);
            let result = self.frame_at(target, cache);
            self.fatal = result.as_ref().err().is_some_and(is_fatal);
            self.results.push((i, result));
            if self.fatal {
                break;
            }
        }
        // The sidecar died: the file's other requests fail with it.
        let dead = match self.results.last() {
            Some((_, Err(e))) if self.fatal => Some(duplicate_error(e)),
            _ => None,
        };
        if let Some(e) = dead {
            for i in idx {
                self.results.push((i, Err(duplicate_error(&e))));
            }
        }
    }

    fn frame_at(&mut self, target: i64, cache: &FrameCache) -> Result<FrameData, ClientError> {
        let dec = &mut self.dec;
        scrub_frame_at(
            &mut dec.client,
            &dec.info,
            &dec.path,
            &mut self.last_pts,
            target,
            cache,
        )
    }
}

/// The scrub frame for `target` of the file `path` open in `client`: the
/// first frame at or after it, else (past the end) the last frame, whose
/// PTS is kept in `last_pts`.
fn scrub_frame_at(
    client: &mut DecoderClient,
    info: &StreamInfo,
    path: &str,
    last_pts: &mut Option<i64>,
    target: i64,
    cache: &FrameCache,
) -> Result<FrameData, ClientError> {
    if let Some(last) = last_pts.filter(|&last| target > last) {
        return cached_decode_at(client, cache, path, last).map(|(f, _)| f);
    }
    match cached_decode_at(client, cache, path, target) {
        Ok((f, _)) => Ok(f),
        // `decode_at` finds no frame at or after a target past the last
        // frame; the rule then gives the last frame.
        Err(ClientError::Decode(reason)) => match last_frame(client, info, target)? {
            Some(f) if f.pts < target => {
                *last_pts = Some(f.pts);
                Ok(f)
            }
            _ => Err(ClientError::Decode(reason)),
        },
        Err(e) => Err(e),
    }
}

#[cfg(any(test, feature = "test-support"))]
impl FrameProvider for ScrubProvider<'_> {
    fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
        self.pool.clear_failures();
        let mut out: Vec<Option<FrameData>> =
            std::iter::repeat_with(|| None).take(reqs.len()).collect();
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, r) in reqs.iter().enumerate() {
            if r.path.is_empty() {
                continue;
            }
            match groups.iter_mut().find(|(p, _)| *p == r.path) {
                Some((_, idx)) => idx.push(i),
                None => groups.push((&r.path, vec![i])),
            }
        }
        let active = groups.len();
        // Every layer's decoder is out at once, so the budget sees the
        // whole frame's regions.
        let mut jobs = Vec::with_capacity(groups.len());
        for (path, idx) in groups {
            match self.pool.take(path, active) {
                Ok(dec) => jobs.push(ScrubJob {
                    last_pts: self.pool.last_frame_pts(&dec.path),
                    dec,
                    idx,
                    results: Vec::new(),
                    fatal: false,
                }),
                Err(e) => self.pool.record_error(path, e),
            }
        }
        let cache = &*self.cache;
        // Separate sidecars decode at the same time: one thread per file.
        match jobs.as_mut_slice() {
            [] => {}
            [job] => job.run(reqs, cache),
            jobs => std::thread::scope(|s| {
                for job in jobs.iter_mut() {
                    s.spawn(move || job.run(reqs, cache));
                }
            }),
        }
        for job in jobs {
            if let Some(pts) = job.last_pts {
                self.pool.note_last_frame_pts(&job.dec.path, pts);
            }
            for (i, result) in job.results {
                match result {
                    Ok(f) => out[i] = Some(f),
                    // Under the requested path (not a proxy swapped in), so
                    // `failure` finds it.
                    Err(e) => self.pool.record_error(&reqs[i].path, e.into()),
                }
            }
            if job.fatal {
                drop(job.dec);
            } else {
                self.pool.put(job.dec);
            }
        }
        out
    }

    fn failure(&self, req: &FrameRequest) -> Option<String> {
        self.pool.failure(&req.path).map(str::to_owned)
    }
}

// Intentional: `SCRUB_HANG_LIMIT` is lib API (tests); the bin doesn't name it.
#[allow(unused_imports)]
pub use scrub_fetch::{ScrubFetcher, ScrubFrames, SCRUB_HANG_LIMIT};

/// Per-layer scrub fetching ([`ScrubFetcher`]).
mod scrub_fetch {
    use super::*;

    /// How long a scrub layer may go without answering before its sidecar is
    /// killed (a hung network share, a decoder loop).
    pub const SCRUB_HANG_LIMIT: Duration = Duration::from_secs(30);
    /// `fetch` calls a layer may go unnamed before its held picture is dropped.
    const SCRUB_FORGET_AFTER: u64 = 8;

    /// Scrub frames decoded on worker threads, per layer, so one slow file
    /// doesn't hold the whole frame (or the engine).
    ///
    /// [`ScrubFetcher::fetch`] waits a bounded time for each file's decode.
    /// A layer whose file is still decoding (or opening) keeps showing the
    /// last frame it had ("held") and is reported pending; the decode goes on
    /// in the background, `on_landed` is called when it finishes, and the next
    /// `fetch` shows it. One job runs per file at a time; a job that doesn't
    /// answer for [`SCRUB_HANG_LIMIT`] has its sidecar killed. Opens go through
    /// [`DecoderPool::reserve`] / [`DecoderPool::adopt`], so the pool's budget,
    /// proxies and kill registry apply as for `take`.
    pub struct ScrubFetcher {
        on_landed: Option<Arc<dyn Fn() + Send + Sync>>,
        /// In-flight jobs by (requested) path.
        jobs: HashMap<String, FetchJob>,
        tx: std::sync::mpsc::Sender<JobDone>,
        rx: std::sync::mpsc::Receiver<JobDone>,
        /// Jobs [`Self::reap_hung`] found finished, landed by the next fetch.
        finished: Vec<JobDone>,
        /// Each layer's last frame, and the request it answers.
        held: HashMap<ClipId, Held>,
        generation: u64,
        pub(super) hang_limit: Duration,
    }

    struct Held {
        req: FrameRequest,
        frame: FrameData,
        touched: u64,
    }

    struct FetchJob {
        started: Instant,
        kill: Arc<Mutex<Option<SidecarKill>>>,
        killed: bool,
    }

    /// What a scrub job sends back.
    struct JobDone {
        path: String,
        decoder: JobDecoder,
        /// Every request it was given.
        reqs: Vec<FrameRequest>,
        /// What each decoded to (empty when the open failed).
        results: Vec<(FrameRequest, Result<FrameData, ClientError>)>,
        last_pts: Option<i64>,
        /// The sidecar failed: drop the decoder instead of keeping it.
        fatal: bool,
    }

    enum JobDecoder {
        /// Was idle in the pool: put it back.
        Pooled(PooledDecoder),
        /// Opened by the job: the pool adopts it.
        Opened(OpenAttempt),
    }

    impl ScrubFetcher {
        /// A fetcher calling `on_landed` (from a worker thread) whenever a
        /// layer's decode finishes: repaint then, and the next `fetch` shows
        /// it. It may also fire for a decode the `fetch` itself waited for.
        pub fn new(on_landed: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
            let (tx, rx) = std::sync::mpsc::channel();
            Self {
                on_landed,
                jobs: HashMap::new(),
                tx,
                rx,
                finished: Vec::new(),
                held: HashMap::new(),
                generation: 0,
                hang_limit: SCRUB_HANG_LIMIT,
            }
        }

        /// Whether a layer's decode is still running.
        pub fn busy(&self) -> bool {
            !self.jobs.is_empty()
        }

        /// The frames for `reqs`: each file's decode gets up to `wait`; a layer
        /// whose decode is still running keeps its previous frame (if it had
        /// one) and is pending in the result.
        pub fn fetch(
            &mut self,
            pool: &mut DecoderPool,
            cache: &Arc<FrameCache>,
            reqs: Vec<FrameRequest>,
            wait: Duration,
        ) -> ScrubFrames {
            self.generation += 1;
            pool.clear_failures();
            let deadline = Instant::now() + wait;
            let mut failures: HashMap<ClipId, String> = HashMap::new();
            // What landed since the last call.
            for done in std::mem::take(&mut self.finished) {
                self.land(pool, done, &mut failures);
            }
            while let Ok(done) = self.rx.try_recv() {
                self.land(pool, done, &mut failures);
            }
            // Each file's requests not answered by a held frame: one job per
            // file, unless one is already running for it.
            let mut groups: Vec<(&str, Vec<&FrameRequest>)> = Vec::new();
            for r in reqs.iter().filter(|r| !r.path.is_empty()) {
                if self.held.get(&r.clip_id).is_some_and(|h| h.req == *r) {
                    continue;
                }
                match groups.iter_mut().find(|(p, _)| *p == r.path) {
                    Some((_, rs)) => rs.push(r),
                    None => groups.push((&r.path, vec![r])),
                }
            }
            let active = reqs
                .iter()
                .map(|r| r.path.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len();
            for (path, rs) in groups {
                if self.jobs.contains_key(path) {
                    continue;
                }
                let rs: Vec<FrameRequest> = rs.into_iter().cloned().collect();
                self.start(pool, cache, path, rs, active);
            }
            // Wait (bounded) for this frame's decodes.
            while !self.jobs.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.rx.recv_timeout(left) {
                    Ok(done) => self.land(pool, done, &mut failures),
                    Err(_) => break,
                }
            }
            self.kill_hung();
            let generation = self.generation;
            let mut out = ScrubFrames {
                reqs: Vec::with_capacity(reqs.len()),
                frames: Vec::with_capacity(reqs.len()),
                failures: Vec::with_capacity(reqs.len()),
                pending: Vec::with_capacity(reqs.len()),
            };
            for r in reqs {
                let held = self.held.get_mut(&r.clip_id);
                let exact = held.as_ref().is_some_and(|h| h.req == r);
                let frame = held.map(|h| {
                    h.touched = generation;
                    h.frame.share()
                });
                out.pending.push(!exact && self.jobs.contains_key(&r.path));
                out.failures.push(failures.get(&r.clip_id).cloned());
                out.frames.push(frame);
                out.reqs.push(r);
            }
            self.held
                .retain(|_, h| generation - h.touched < SCRUB_FORGET_AFTER);
            out
        }

        /// Start a job decoding `reqs` of `path`.
        fn start(
            &mut self,
            pool: &mut DecoderPool,
            cache: &Arc<FrameCache>,
            path: &str,
            reqs: Vec<FrameRequest>,
            active: usize,
        ) {
            let last_pts = pool.last_frame_pts(path);
            let (decoder, kill) = match pool.reserve(path, active) {
                Reservation::Ready(dec) => {
                    let kill = Arc::new(Mutex::new(Some(dec.client.kill_handle())));
                    (JobStart::Pooled(dec), kill)
                }
                Reservation::Pending(open) => {
                    let kill = open.kill_slot();
                    (JobStart::Open(open), kill)
                }
            };
            let (tx, cache, on_landed) =
                (self.tx.clone(), Arc::clone(cache), self.on_landed.clone());
            let job_path = path.to_owned();
            let spawned = std::thread::Builder::new()
                .name("vance-scrub-layer".into())
                .spawn(move || {
                    let done = run_fetch_job(job_path, decoder, reqs, &cache, last_pts);
                    let _ = tx.send(done);
                    if let Some(cb) = on_landed {
                        cb();
                    }
                });
            match spawned {
                Ok(_) => {
                    self.jobs.insert(
                        path.to_owned(),
                        FetchJob {
                            started: Instant::now(),
                            kill,
                            killed: false,
                        },
                    );
                }
                Err(e) => pool.record_error(
                    path,
                    EngineError::PlaybackThread(format!("scrub layer thread: {e}")),
                ),
            }
        }

        /// Take in a finished job: its decoder back to the pool, its frames as
        /// the layers' new pictures, its errors as their failures.
        fn land(
            &mut self,
            pool: &mut DecoderPool,
            done: JobDone,
            failures: &mut HashMap<ClipId, String>,
        ) {
            self.jobs.remove(&done.path);
            if let Some(pts) = done.last_pts {
                pool.note_last_frame_pts(&done.path, pts);
            }
            let decoder = match done.decoder {
                JobDecoder::Pooled(d) => Ok(d),
                JobDecoder::Opened(attempt) => pool.adopt(attempt),
            };
            match decoder {
                Ok(d) if done.fatal => drop(d),
                Ok(d) => pool.put(d),
                Err(e) => {
                    let msg = e.to_string();
                    pool.record_error(&done.path, e);
                    for r in &done.reqs {
                        failures.insert(r.clip_id, msg.clone());
                        self.held.remove(&r.clip_id);
                    }
                }
            }
            for (r, result) in done.results {
                match result {
                    Ok(frame) => {
                        failures.remove(&r.clip_id);
                        let touched = self.generation;
                        self.held.insert(
                            r.clip_id,
                            Held {
                                req: r,
                                frame,
                                touched,
                            },
                        );
                    }
                    Err(e) => {
                        failures.insert(r.clip_id, e.to_string());
                        self.held.remove(&r.clip_id);
                        pool.record_error(&r.path, e.into());
                    }
                }
            }
        }

        /// Kill the sidecars of decodes that haven't answered for
        /// [`SCRUB_HANG_LIMIT`], without fetching: a timer's call, so a stuck
        /// decoder doesn't linger once the user stops scrubbing. Cheap and
        /// non-blocking (a kill, when due, is a signal).
        pub fn reap_hung(&mut self) {
            // Finished ones first, so a decoder that already answered is
            // never killed; the next fetch lands them.
            while let Ok(done) = self.rx.try_recv() {
                self.jobs.remove(&done.path);
                self.finished.push(done);
            }
            self.kill_hung();
        }

        /// Kill the sidecars of jobs that haven't answered for the hang limit.
        fn kill_hung(&mut self) {
            for (path, job) in &mut self.jobs {
                if !job.killed && job.started.elapsed() >= self.hang_limit {
                    if let Some(kill) = job.kill.lock().as_ref() {
                        tracing::warn!(%path, "scrub layer hasn't answered; killing its decoder");
                        kill.kill();
                        job.killed = true;
                    }
                }
            }
        }
    }

    impl Drop for ScrubFetcher {
        // Intentional: in-flight jobs aren't joined; their sidecars are killed so
        // none is left behind on a hung share. A job that finishes anyway drops
        // its decoder (nobody receives it).
        fn drop(&mut self) {
            for job in self.jobs.values() {
                if let Some(kill) = job.kill.lock().as_ref() {
                    kill.kill();
                }
            }
        }
    }

    // Intentional: unboxed, as `Reservation` (moved into the job at once).
    #[allow(clippy::large_enum_variant)]
    enum JobStart {
        Pooled(PooledDecoder),
        Open(crate::decoder_pool::PendingOpen),
    }

    /// A scrub job: open the decoder if need be, then decode `reqs`.
    fn run_fetch_job(
        path: String,
        start: JobStart,
        reqs: Vec<FrameRequest>,
        cache: &FrameCache,
        mut last_pts: Option<i64>,
    ) -> JobDone {
        #[cfg(test)]
        super::tests::scrub_gate(&path);
        let mut decoder = match start {
            JobStart::Pooled(d) => JobDecoder::Pooled(d),
            JobStart::Open(open) => JobDecoder::Opened(open.open()),
        };
        let target = match &mut decoder {
            JobDecoder::Pooled(d) => Some((&mut d.client, &d.info, d.path.as_str())),
            JobDecoder::Opened(a) => a.decoder_mut(),
        };
        let mut results = Vec::with_capacity(reqs.len());
        let mut fatal = false;
        if let Some((client, info, file)) = target {
            let (file, info) = (file.to_owned(), info.clone());
            let mut failed: Option<ClientError> = None;
            for r in reqs.iter().cloned() {
                if let Some(e) = &failed {
                    // The sidecar died: the file's other requests fail with it.
                    results.push((r, Err(duplicate_error(e))));
                    continue;
                }
                let t = source_seconds_to_pts(r.source_seconds, info.time_base);
                let result = scrub_frame_at(client, &info, &file, &mut last_pts, t, cache);
                if let Err(e) = &result {
                    if is_fatal(e) {
                        fatal = true;
                        failed = Some(duplicate_error(e));
                    }
                }
                results.push((r, result));
            }
        }
        // (The open failed otherwise: `land` reports it for `reqs`.)
        JobDone {
            path,
            decoder,
            reqs,
            results,
            last_pts,
            fatal,
        }
    }

    /// What [`ScrubFetcher::fetch`] got for one frame: a [`FrameProvider`] for
    /// `compose_frame` (like [`crate::compose::Prefetched`]), plus which layers
    /// are still decoding.
    pub struct ScrubFrames {
        reqs: Vec<FrameRequest>,
        frames: Vec<Option<FrameData>>,
        failures: Vec<Option<String>>,
        pending: Vec<bool>,
    }

    impl ScrubFrames {
        /// Whether any layer is still decoding (showing its previous frame, or
        /// nothing yet): repaint when the fetcher's `on_landed` fires.
        pub fn pending(&self) -> bool {
            self.pending.iter().any(|&p| p)
        }

        /// Whether `req`'s layer is still decoding (not an error: don't report
        /// it as one).
        pub fn is_pending(&self, req: &FrameRequest) -> bool {
            self.reqs
                .iter()
                .zip(&self.pending)
                .any(|(r, &p)| p && r == req)
        }
    }

    impl FrameProvider for ScrubFrames {
        fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
            reqs.iter()
                .map(|r| {
                    let i = self
                        .reqs
                        .iter()
                        .zip(&self.frames)
                        .position(|(have, f)| have == r && f.is_some())?;
                    self.frames[i].take()
                })
                .collect()
        }

        fn failure(&self, req: &FrameRequest) -> Option<String> {
            let i = self.reqs.iter().position(|have| have == req)?;
            self.failures[i].clone()
        }
    }
}

/// How a [`StreamProvider`] behaves when decoding falls behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedPolicy {
    /// Playback: drop or hold frames to keep up with the clock.
    Realtime,
    /// Export: wait for every frame.
    Blocking,
}

/// Streaming provider for playback and export.
///
/// Call [`FrameProvider::prepare`] every frame with the clips that start
/// within about a second, so their streams are ready at the cut (it may
/// spawn a sidecar, tens of ms). Call [`StreamProvider::into_pool`] when
/// done to keep the decoders; dropping the provider shuts them down.
pub struct StreamProvider {
    pool: DecoderPool,
    policy: FeedPolicy,
    feeds: HashMap<ClipId, Feed>,
    /// Workers told to stop, joined once finished.
    retiring: Vec<Worker>,
    /// `frames` calls so far.
    generation: u64,
}

/// A feed's worker thread, and a way to kill the sidecar it talks to.
struct Worker {
    handle: JoinHandle<(PooledDecoder, bool)>,
    kill: SidecarKill,
}

impl StreamProvider {
    /// A provider drawing decoders from `pool`.
    pub fn new(pool: DecoderPool, policy: FeedPolicy) -> Self {
        Self {
            pool,
            policy,
            feeds: HashMap::new(),
            retiring: Vec::new(),
            generation: 0,
        }
    }

    /// Stop every feed and hand the pool back, with the decoders whose
    /// workers finish within [`STOP_WAIT`].
    pub fn into_pool(self) -> DecoderPool {
        let StreamProvider {
            mut pool,
            feeds,
            mut retiring,
            ..
        } = self;
        for (_, mut feed) in feeds {
            retiring.extend(feed.stop());
        }
        join_within(&mut retiring, &mut pool, STOP_WAIT);
        if !retiring.is_empty() {
            // A worker still busy is blocked on its sidecar (a stalled
            // network file, an FFmpeg loop: the client has no read timeout),
            // and stopping playback or an export must not hang with it:
            // killing the sidecar makes the blocked request return.
            tracing::warn!(
                workers = retiring.len(),
                "layer feeds still busy after stop; killing their sidecars"
            );
            for w in &retiring {
                w.kill.kill();
            }
            join_within(&mut retiring, &mut pool, KILL_WAIT);
        }
        // Intentional: anything left is detached, not joined. A worker
        // stuck past a kill (the OS hasn't delivered it yet) finishes on its
        // own and drops its decoder there.
        pool
    }

    /// The pool, for its budget and error reporting while feeds run
    /// ([`DecoderPool::set_proxy_paths`], [`DecoderPool::take_proxy_requests`],
    /// [`DecoderPool::take_error`]).
    pub fn pool_mut(&mut self) -> &mut DecoderPool {
        &mut self.pool
    }

    /// Join finished retired workers, returning healthy decoders.
    fn reap(&mut self) {
        join_finished(&mut self.retiring, &mut self.pool);
    }

    fn retire(&mut self, mut feed: Feed) {
        self.retiring.extend(feed.stop());
    }

    /// Make sure `r`'s clip has a live feed on `r.path`, and mark it used.
    fn ensure_feed(&mut self, r: &FrameRequest, active: usize) {
        let (now, policy) = (Instant::now(), self.policy);
        let mut replace = false;
        if let Some(f) = self.feeds.get_mut(&r.clip_id) {
            if f.path != r.path {
                // The engine switched this clip's file (to a proxy, say).
                replace = true;
            } else {
                match f.stream.as_mut() {
                    // Never started: retry. A file still missing or broken
                    // costs no sidecar (the pool caches open failures).
                    None => replace = true,
                    Some(s) if s.failed => {
                        let since = *f.failed_since.get_or_insert(now);
                        let due = policy == FeedPolicy::Blocking
                            || now.duration_since(since) >= RETRY_FAILED_AFTER;
                        if due && s.worker_healthy() {
                            // A decode error: the worker and its sidecar are
                            // fine, so the same stream starts again.
                            let from = source_seconds_to_pts(r.source_seconds, s.info.time_base);
                            s.retry(from);
                            f.failed_since = None;
                        } else {
                            replace = due;
                        }
                    }
                    Some(_) => {}
                }
            }
        }
        let mut fallback = None;
        if replace {
            if let Some(mut old) = self.feeds.remove(&r.clip_id) {
                if policy == FeedPolicy::Realtime && old.path == r.path {
                    fallback = old
                        .stream
                        .as_mut()
                        .and_then(Stream::take_shown)
                        .or_else(|| old.fallback.take());
                }
                self.retire(old);
            }
        }
        if !self.feeds.contains_key(&r.clip_id) {
            let mut stream = match self.pool.take(&r.path, active) {
                Ok(dec) => {
                    let from = source_seconds_to_pts(r.source_seconds, dec.info.time_base);
                    match Stream::start(dec, from) {
                        Ok(stream) => Some(stream),
                        Err(e) => {
                            self.pool.record_error(&r.path, e);
                            None
                        }
                    }
                }
                Err(e) => {
                    self.pool.record_error(&r.path, e);
                    None
                }
            };
            let fallback = match stream.as_mut() {
                Some(s) => {
                    s.fallback = fallback;
                    None
                }
                None => fallback,
            };
            let feed = Feed {
                path: r.path.clone(),
                touched: 0,
                stream,
                failed_since: None,
                fallback,
            };
            self.feeds.insert(r.clip_id, feed);
        }
        if let Some(feed) = self.feeds.get_mut(&r.clip_id) {
            feed.touched = self.generation;
        }
    }
}

impl FrameProvider for StreamProvider {
    fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
        self.generation += 1;
        self.reap();
        self.pool.clear_failures();
        let now = Instant::now();
        let deadlines = Deadlines {
            grace: now + REALTIME_GRACE,
            first_frame: now + FIRST_FRAME_WAIT,
            blocking: now + BLOCKING_WAIT,
        };
        let active = reqs.len().max(self.feeds.len());
        let mut out = Vec::with_capacity(reqs.len());
        for r in reqs {
            if r.path.is_empty() {
                out.push(None);
                continue;
            }
            self.ensure_feed(r, active);
            let Some(feed) = self.feeds.get_mut(&r.clip_id) else {
                out.push(None);
                continue;
            };
            let Some(stream) = feed.stream.as_mut() else {
                // `Realtime` keeps showing a failed feed's last frame while
                // its replacement can't start.
                out.push(feed.fallback.as_mut().map(FrameData::share));
                continue;
            };
            let target = source_seconds_to_pts(r.source_seconds, stream.info.time_base);
            out.push(stream.frame_for(target, self.policy, &deadlines));
            if let Some(e) = stream.error.take() {
                self.pool.record_error(&feed.path, e.into());
            }
        }
        let generation = self.generation;
        let stale: Vec<ClipId> = self
            .feeds
            .iter()
            .filter(|(_, f)| generation - f.touched >= RETIRE_AFTER)
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            if let Some(feed) = self.feeds.remove(&id) {
                self.retire(feed);
            }
        }
        out
    }

    fn prepare(&mut self, upcoming: &[FrameRequest]) {
        self.reap();
        let new = upcoming
            .iter()
            .filter(|r| !self.feeds.contains_key(&r.clip_id))
            .count();
        let active = self.feeds.len() + new;
        for r in upcoming.iter().filter(|r| !r.path.is_empty()) {
            self.ensure_feed(r, active);
        }
    }

    fn failure(&self, req: &FrameRequest) -> Option<String> {
        self.pool.failure(&req.path).map(str::to_owned)
    }
}

/// Join the finished workers in `retiring`, putting healthy decoders back
/// in `pool`.
fn join_finished(retiring: &mut Vec<Worker>, pool: &mut DecoderPool) {
    let mut i = 0;
    while i < retiring.len() {
        if !retiring[i].handle.is_finished() {
            i += 1;
            continue;
        }
        // Unhealthy decoders and panicked workers' are dropped.
        if let Ok((dec, true)) = retiring.swap_remove(i).handle.join() {
            pool.put(dec);
        }
    }
}

/// [`join_finished`] until `retiring` is empty or `wait` has passed.
fn join_within(retiring: &mut Vec<Worker>, pool: &mut DecoderPool, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        join_finished(retiring, pool);
        if retiring.is_empty() || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// When one `frames` call stops waiting.
struct Deadlines {
    grace: Instant,
    first_frame: Instant,
    blocking: Instant,
}

/// One clip's feed.
struct Feed {
    /// The requested path (the stream may be on its proxy).
    path: String,
    /// `generation` of the last `frames` / `prepare` naming this clip.
    touched: u64,
    /// `None`: the decoder or worker couldn't start (already reported).
    stream: Option<Stream>,
    /// When `ensure_feed` first saw the stream failed.
    failed_since: Option<Instant>,
    /// `Realtime`, with no stream: the last frame of the failed feed this
    /// one replaced.
    fallback: Option<FrameData>,
}

impl Feed {
    /// Tell the worker to stop; its handle, to join for the decoder.
    fn stop(&mut self) -> Option<Worker> {
        let stream = self.stream.as_mut()?;
        stream.shared.state.lock().stop = true;
        stream.shared.wake.notify_all();
        stream.worker.take()
    }
}

impl Drop for Feed {
    // Intentional: no join here. The worker finishes its current request,
    // then drops the decoder (shutting the sidecar down) on its own thread,
    // so dropping a provider never waits on a decode.
    fn drop(&mut self) {
        self.stop();
    }
}

/// State a feed shares with its worker.
struct Shared {
    state: Mutex<FeedState>,
    /// Signalled on every change: frames queued or taken, restarts, stops.
    wake: Condvar,
}

struct FeedState {
    /// Bumped by every (re)start; frames and ends of older streams are
    /// dropped.
    epoch: u64,
    /// Where the current epoch's stream starts.
    from_pts: i64,
    queue: VecDeque<FrameData>,
    /// How the current epoch's stream ended.
    end: Option<StreamEnd>,
    stop: bool,
    /// The worker has exited (stopped, sidecar failed, or panicked).
    worker_gone: bool,
}

enum StreamEnd {
    Eos,
    Failed(ClientError),
}

/// The consumer side of a running feed.
struct Stream {
    /// The opened file's info (the proxy's when the pool swapped).
    info: StreamInfo,
    shared: Arc<Shared>,
    worker: Option<Worker>,
    /// The frame last taken from the stream.
    held: Option<FrameData>,
    /// Lowest target `held` (or, with none yet, the stream's next frame)
    /// answers: `held` is the first frame with `pts >= valid_from`.
    valid_from: i64,
    /// The stream ended: `held` is the file's last frame.
    eof: bool,
    /// A stream from here ended with no frame: every frame is before it.
    no_frames_from: Option<i64>,
    /// The stream failed (until a restart): `failure` says why.
    failed: bool,
    failure: Option<ClientError>,
    /// A failure not yet reported to the pool.
    error: Option<ClientError>,
    /// `Realtime`: the last frame of the failed feed this one replaced,
    /// shown until this stream has a frame of its own.
    fallback: Option<FrameData>,
}

impl Stream {
    fn start(dec: PooledDecoder, from: i64) -> Result<Self, EngineError> {
        let info = dec.info.clone();
        let kill = dec.client.kill_handle();
        let shared = Arc::new(Shared {
            state: Mutex::new(FeedState {
                epoch: 0,
                from_pts: from,
                queue: VecDeque::new(),
                end: None,
                stop: false,
                worker_gone: false,
            }),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        // On a failed spawn the closure, decoder and all, is dropped.
        let handle = std::thread::Builder::new()
            .name("vance-layer-feed".into())
            .spawn(move || run_worker(dec, &worker_shared))
            .map_err(|e| EngineError::PlaybackThread(format!("layer feed thread: {e}")))?;
        let worker = Worker { handle, kill };
        Ok(Self {
            info,
            shared,
            worker: Some(worker),
            held: None,
            valid_from: from,
            eof: false,
            no_frames_from: None,
            failed: false,
            failure: None,
            error: None,
            fallback: None,
        })
    }

    /// A zero-copy share of the frame to show when there is nothing newer:
    /// the held frame, else the replaced feed's.
    fn shown(&mut self) -> Option<FrameData> {
        self.held
            .as_mut()
            .or(self.fallback.as_mut())
            .map(FrameData::share)
    }

    /// Move out the frame [`Self::shown`] would share.
    fn take_shown(&mut self) -> Option<FrameData> {
        self.held.take().or_else(|| self.fallback.take())
    }

    /// The failure was a decode error and the worker is still running, so
    /// the stream can simply start again.
    fn worker_healthy(&self) -> bool {
        !self.failure.as_ref().is_some_and(is_fatal) && !self.shared.state.lock().worker_gone
    }

    /// Start the failed stream again from `from`, showing what it showed
    /// until the new stream has a frame.
    fn retry(&mut self, from: i64) {
        self.fallback = self.take_shown();
        self.restart(from);
    }

    /// The stream failed with `e`: report it now (and, `Blocking`, every
    /// time a frame is asked of it).
    fn fail(&mut self, e: ClientError) {
        self.failed = true;
        self.error = Some(duplicate_error(&e));
        self.failure = Some(e);
    }

    /// Point the worker at a new stream from `from`.
    fn restart(&mut self, from: i64) {
        {
            let mut st = self.shared.state.lock();
            st.epoch += 1;
            st.from_pts = from;
            st.queue.clear();
            st.end = None;
        }
        self.shared.wake.notify_all();
        self.held = None;
        self.valid_from = from;
        self.eof = false;
        self.failed = false;
        self.failure = None;
    }

    /// The frame for `target` under `policy`.
    fn frame_for(
        &mut self,
        target: i64,
        policy: FeedPolicy,
        deadlines: &Deadlines,
    ) -> Option<FrameData> {
        let jump = source_seconds_to_pts(JUMP_SECONDS, self.info.time_base).max(1);
        let head = self.held.as_ref().map_or(self.valid_from, |h| h.pts);
        // Past the end the answer is the last frame, which a running tail
        // search is already looking for: a jump must not restart it.
        let past_end = self.no_frames_from.is_some_and(|x| target >= x);
        if target < self.valid_from
            || (!self.eof && !past_end && target > head.saturating_add(jump))
        {
            self.restart(target);
        }
        loop {
            let answers = self
                .held
                .as_ref()
                .is_some_and(|h| h.pts >= target || self.eof);
            if answers {
                return self.held.as_mut().map(FrameData::share);
            }
            match &self.held {
                None if self.eof => {
                    // Nothing at or after `valid_from`: look further back
                    // for the last frame.
                    self.no_frames_from = Some(
                        self.no_frames_from
                            .map_or(self.valid_from, |x| x.min(self.valid_from)),
                    );
                    let from = tail_starts(target, &self.info)
                        .into_iter()
                        .find(|&s| s < self.valid_from)?;
                    self.restart(from);
                    continue;
                }
                _ => {}
            }
            if self.failed {
                // Intentional: playback freezes a failed layer on its last
                // frame; export gets nothing, and the error every time.
                return match policy {
                    FeedPolicy::Realtime => self.shown(),
                    FeedPolicy::Blocking => {
                        if self.error.is_none() {
                            self.error = self.failure.as_ref().map(duplicate_error);
                        }
                        None
                    }
                };
            }
            let mut st = self.shared.state.lock();
            if let Some(frame) = st.queue.pop_front() {
                self.fallback = None;
                if let Some(prev) = self.held.replace(frame) {
                    self.valid_from = prev.pts.saturating_add(1);
                }
                // The previous frame's lease ended above, before the worker
                // can see the queue's room and fetch another.
                drop(st);
                self.shared.wake.notify_all();
                continue;
            }
            match st.end.take() {
                Some(StreamEnd::Eos) => {
                    self.eof = true;
                    continue;
                }
                Some(StreamEnd::Failed(e)) => {
                    drop(st);
                    self.fail(e);
                    continue;
                }
                None if st.worker_gone => {
                    drop(st);
                    self.fail(ClientError::Ipc(vance_ipc::Error::SidecarExited(
                        "the layer's decode worker stopped".into(),
                    )));
                    continue;
                }
                None => {}
            }
            let has_picture = self.held.is_some() || self.fallback.is_some();
            let until = match (policy, has_picture) {
                (FeedPolicy::Blocking, _) => deadlines.blocking,
                (FeedPolicy::Realtime, true) => deadlines.grace,
                (FeedPolicy::Realtime, false) => deadlines.first_frame,
            };
            if Instant::now() >= until {
                return match policy {
                    // Starved: hold the last frame (or skip, with none yet).
                    FeedPolicy::Realtime => {
                        drop(st);
                        self.shown()
                    }
                    FeedPolicy::Blocking => {
                        drop(st);
                        self.fail(ClientError::Decode(format!(
                            "no frame for pts {target} within {BLOCKING_WAIT:?}"
                        )));
                        None
                    }
                };
            }
            self.shared.wake.wait_until(&mut st, until);
        }
    }
}

enum Job {
    Start(u64, i64),
    Next(u64),
    Stop,
}

/// A feed's worker: runs the decoder's stream for the feed's current
/// epoch, keeping up to [`QUEUE_FRAMES`] frames queued. Returns the decoder
/// and whether its sidecar is still usable.
fn run_worker(mut dec: PooledDecoder, shared: &Shared) -> (PooledDecoder, bool) {
    struct Gone<'a>(&'a Shared);
    impl Drop for Gone<'_> {
        fn drop(&mut self) {
            self.0.state.lock().worker_gone = true;
            self.0.wake.notify_all();
        }
    }
    let _gone = Gone(shared);
    // The epoch last started, and the one whose stream is still running.
    let mut started: Option<u64> = None;
    let mut running: Option<u64> = None;
    let mut healthy = true;
    while healthy {
        let job = {
            let mut st = shared.state.lock();
            loop {
                if st.stop {
                    break Job::Stop;
                }
                if started != Some(st.epoch) {
                    break Job::Start(st.epoch, st.from_pts);
                }
                if running == Some(st.epoch) && st.queue.len() < QUEUE_FRAMES {
                    break Job::Next(st.epoch);
                }
                shared.wake.wait(&mut st);
            }
        };
        let (epoch, result) = match job {
            Job::Stop => break,
            Job::Start(epoch, from) => {
                started = Some(epoch);
                // A start also stops the sidecar's previous stream.
                match dec.client.start_stream(from, STREAM_BUFFER_FRAMES) {
                    Ok(_) => {
                        running = Some(epoch);
                        continue;
                    }
                    Err(e) => (epoch, Err(e)),
                }
            }
            Job::Next(epoch) => (epoch, dec.client.next_frame()),
        };
        let mut st = shared.state.lock();
        let current = st.epoch == epoch;
        match result {
            Ok(Some(frame)) if current => st.queue.push_back(frame),
            // A frame of a stream the feed has moved on from.
            Ok(Some(_)) => {}
            Ok(None) => {
                running = None;
                if current {
                    st.end = Some(StreamEnd::Eos);
                }
            }
            Err(e) => {
                running = None;
                healthy = !is_fatal(&e);
                if current || !healthy {
                    // A dead sidecar ends the current stream too, whichever
                    // stream it died on.
                    st.end = Some(StreamEnd::Failed(e));
                } else {
                    tracing::warn!(error = %e, "layer feed: a superseded stream failed");
                }
            }
        }
        drop(st);
        shared.wake.notify_all();
    }
    if healthy && running.is_some() {
        // A sidecar that dies here (killed while hung, say) mustn't go back
        // to the pool as healthy.
        if let Err(e) = dec.client.stop_stream() {
            healthy = !is_fatal(&e);
        }
    }
    (dec, healthy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(time_base: (i64, i64), duration_pts: Option<i64>) -> StreamInfo {
        StreamInfo {
            width: 16,
            height: 16,
            pixel_format: vance_decoder::PixelFormat::Yuv420P,
            frame_rate: (30, 1),
            time_base,
            duration_pts,
            codec_name: "test".into(),
            stream_index: 0,
            color_transfer: Default::default(),
            color_range: Default::default(),
            color_matrix: Default::default(),
            sample_aspect: (1, 1),
        }
    }

    #[test]
    fn tail_search_starts_a_second_before_the_end_then_backs_off() {
        let i = info((1, 1000), Some(5000));
        assert_eq!(
            tail_starts(9000, &i),
            [4000, 1000, -11_000, -59_000, i64::MIN]
        );
        // A target before the reported end anchors on the target.
        assert_eq!(tail_starts(3000, &i)[0], 2000);
        // Unknown duration: the target.
        assert_eq!(tail_starts(3000, &info((1, 1000), None))[0], 2000);
        // Saturation collapses into one start-of-file attempt.
        assert_eq!(tail_starts(i64::MIN + 5, &i), [i64::MIN]);
    }

    #[test]
    fn providers_move_to_the_playback_thread() {
        fn send<T: Send>() {}
        send::<StreamProvider>();
    }

    /// A stream with no worker: the test plays the worker's part through
    /// the shared state.
    fn bare_stream() -> Stream {
        Stream {
            info: info((1, 1000), Some(5000)),
            shared: Arc::new(Shared {
                state: Mutex::new(FeedState {
                    epoch: 0,
                    from_pts: 0,
                    queue: VecDeque::new(),
                    end: None,
                    stop: false,
                    worker_gone: false,
                }),
                wake: Condvar::new(),
            }),
            worker: None,
            held: None,
            valid_from: 0,
            eof: false,
            no_frames_from: None,
            failed: false,
            failure: None,
            error: None,
            fallback: None,
        }
    }

    fn deadlines() -> Deadlines {
        let now = Instant::now();
        Deadlines {
            grace: now,
            first_frame: now,
            blocking: now + Duration::from_secs(5),
        }
    }

    fn tiny_frame(pts: i64) -> FrameData {
        FrameData {
            pts,
            width: 2,
            height: 2,
            pixel_format: vance_decoder::PixelFormat::Yuv420P,
            strides: vec![2, 1, 1],
            bytes: vec![0u8; 6].into(),
            color: Default::default(),
        }
    }

    #[test]
    fn a_dead_worker_is_an_error_not_silence() {
        for policy in [FeedPolicy::Blocking, FeedPolicy::Realtime] {
            let mut s = bare_stream();
            // The worker died (a panic, or its sidecar on an old stream)
            // without saying why.
            s.shared.state.lock().worker_gone = true;
            assert!(s.frame_for(0, policy, &deadlines()).is_none());
            let e = s.error.take().expect("an error for the pool");
            assert!(is_fatal(&e), "{e}");
            assert!(s.failed);
        }
    }

    #[test]
    fn a_failed_blocking_feed_reports_every_time() {
        let mut s = bare_stream();
        s.shared.state.lock().end = Some(StreamEnd::Failed(ClientError::Decode("bad".into())));
        for _ in 0..3 {
            assert!(s.frame_for(0, FeedPolicy::Blocking, &deadlines()).is_none());
            let e = s.error.take().expect("reported again");
            assert!(e.to_string().contains("bad"), "{e}");
        }
    }

    #[test]
    fn a_failed_realtime_feed_holds_its_frame_or_the_replaced_feeds() {
        let mut s = bare_stream();
        s.fallback = Some(tiny_frame(7));
        s.shared.state.lock().worker_gone = true;
        let shown = s.frame_for(0, FeedPolicy::Realtime, &deadlines());
        assert_eq!(shown.map(|f| f.pts), Some(7), "the replaced feed's frame");
        assert!(s.error.take().is_some(), "reported once");
        assert!(s.frame_for(0, FeedPolicy::Realtime, &deadlines()).is_some());
        assert!(s.error.take().is_none(), "playback reports a failure once");
    }

    #[test]
    fn a_realtime_feed_that_cant_restart_keeps_showing_its_last_frame() {
        let pool = DecoderPool::new(
            "unused".into(),
            6,
            crate::decoder_pool::MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let mut prov = StreamProvider::new(pool, FeedPolicy::Realtime);
        let clip = ClipId::new();
        let path = "/no/such/vance-gone.mp4";
        // Its sidecar died, so the retry needs a new decoder.
        let mut failed = bare_stream();
        failed.held = Some(tiny_frame(3));
        failed.failed = true;
        failed.failure = Some(ClientError::Ipc(vance_ipc::Error::UnexpectedEof));
        failed.shared.state.lock().worker_gone = true;
        prov.feeds.insert(
            clip,
            Feed {
                path: path.into(),
                touched: 0,
                stream: Some(failed),
                failed_since: Some(Instant::now() - RETRY_FAILED_AFTER),
                fallback: None,
            },
        );
        let r = request(clip, path, 0.1);
        for _ in 0..2 {
            // Replaced (the retry is due), the file is gone: still frozen.
            let f = prov.frames(std::slice::from_ref(&r)).remove(0);
            assert_eq!(f.map(|f| f.pts), Some(3));
            assert!(prov.failure(&r).is_some_and(|m| m.contains("NotFound")));
        }
        let mut blocking = StreamProvider::new(prov.into_pool(), FeedPolicy::Blocking);
        assert!(blocking.frames(std::slice::from_ref(&r))[0].is_none());
    }

    /// A provider with one feed on `path` whose stream is `stream`.
    fn provider_with(
        policy: FeedPolicy,
        clip: ClipId,
        path: &str,
        stream: Stream,
        failed_since: Option<Instant>,
    ) -> StreamProvider {
        let pool = DecoderPool::new(
            "unused".into(),
            6,
            crate::decoder_pool::MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let mut prov = StreamProvider::new(pool, policy);
        prov.feeds.insert(
            clip,
            Feed {
                path: path.into(),
                touched: 0,
                stream: Some(stream),
                failed_since,
                fallback: None,
            },
        );
        prov
    }

    #[test]
    fn a_decode_error_restarts_the_healthy_worker_not_a_new_sidecar() {
        let (clip, path) = (ClipId::new(), "/no/such/vance-decode-error.mp4");
        let mut s = bare_stream();
        s.held = Some(tiny_frame(3));
        s.failed = true;
        s.failure = Some(ClientError::Decode("bad frame".into()));
        let shared = Arc::clone(&s.shared);
        let due = Some(Instant::now() - RETRY_FAILED_AFTER);
        let mut prov = provider_with(FeedPolicy::Realtime, clip, path, s, due);
        let r = request(clip, path, 0.1);
        let f = prov.frames(std::slice::from_ref(&r)).remove(0);
        assert_eq!(f.map(|f| f.pts), Some(3), "frozen while it restarts");
        assert!(prov.failure(&r).is_none(), "no new decoder was asked for");
        let feed = &prov.feeds[&clip];
        let same = feed.stream.as_ref().expect("the same feed");
        assert!(Arc::ptr_eq(&same.shared, &shared), "the same worker");
        let st = shared.state.lock();
        assert_eq!((st.epoch, st.from_pts), (1, 100), "restarted at the target");
    }

    #[test]
    fn a_failed_realtime_feed_is_not_replaced_before_the_retry_interval() {
        let (clip, path) = (ClipId::new(), "/no/such/vance-crashed.mp4");
        let mut s = bare_stream();
        s.held = Some(tiny_frame(5));
        s.failed = true;
        s.failure = Some(ClientError::Ipc(vance_ipc::Error::UnexpectedEof));
        s.shared.state.lock().worker_gone = true;
        let shared = Arc::clone(&s.shared);
        let mut prov = provider_with(FeedPolicy::Realtime, clip, path, s, None);
        let r = request(clip, path, 0.1);
        for _ in 0..3 {
            let f = prov.frames(std::slice::from_ref(&r)).remove(0);
            assert_eq!(f.map(|f| f.pts), Some(5), "frozen on its last frame");
            assert!(prov.failure(&r).is_none(), "no respawn within the second");
        }
        let feed = &prov.feeds[&clip];
        assert!(feed.failed_since.is_some());
        let same = feed.stream.as_ref().expect("the same feed");
        assert!(Arc::ptr_eq(&same.shared, &shared));
    }

    #[test]
    fn frames_are_shares_of_the_held_frame() {
        let mut s = bare_stream();
        s.shared.state.lock().queue.push_back(tiny_frame(0));
        let f = s.frame_for(0, FeedPolicy::Blocking, &deadlines()).unwrap();
        assert!(s
            .held
            .as_ref()
            .is_some_and(|h| h.bytes.as_ptr() == f.bytes.as_ptr()));
        assert_eq!(f.pts, 0);
    }

    fn request(clip_id: ClipId, path: &str, t: f64) -> FrameRequest {
        FrameRequest {
            clip_id,
            media: vance_timeline::MediaItemId::new(),
            path: path.to_owned(),
            source_seconds: t,
        }
    }

    fn real_pool() -> Option<DecoderPool> {
        let bin = crate::decoder_pool::tests::decoder_bin()?;
        Some(DecoderPool::new(
            bin,
            6,
            crate::decoder_pool::MemoryBudget {
                cap_bytes: u64::MAX,
            },
        ))
    }

    #[test]
    fn a_feed_that_failed_to_open_is_retried_once_the_file_appears() {
        if real_pool().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late.mp4");
        let path = path.to_string_lossy().into_owned();
        for policy in [FeedPolicy::Blocking, FeedPolicy::Realtime] {
            let _ = std::fs::remove_file(&path);
            let pool = real_pool().expect("checked above");
            let mut prov = StreamProvider::new(pool, policy);
            let r = request(ClipId::new(), &path, 0.5);
            assert!(prov.frames(std::slice::from_ref(&r))[0].is_none());
            assert!(prov.failure(&r).is_some_and(|m| m.contains("NotFound")));
            assert!(prov.pool_mut().take_error().is_some());
            std::fs::copy(crate::decoder_pool::tests::fixture(), &path).unwrap();
            // A `Realtime` call waits at most a second, and that second also
            // covers the respawn and open, so a slow machine (a Windows debug
            // build) may need more than one call. What this pins is that the
            // feed is retried at all; it used to stay dead.
            let deadline = Instant::now() + Duration::from_secs(20);
            let f = loop {
                let f = prov.frames(std::slice::from_ref(&r)).remove(0);
                if f.is_some() || Instant::now() >= deadline {
                    break f;
                }
            };
            assert!(f.is_some(), "{policy:?}: the retry reopened the file");
            assert!(prov.failure(&r).is_none());
            drop(f);
            drop(prov.into_pool());
        }
    }

    #[test]
    fn a_killed_sidecar_is_an_error_then_the_feed_reopens() {
        let Some(mut pool) = real_pool() else { return };
        let path = crate::decoder_pool::tests::fixture()
            .to_string_lossy()
            .into_owned();
        // An idle decoder whose sidecar is dead: the feed's worker fails on
        // its first request.
        let mut dec = pool.take(&path, 1).expect("take");
        dec.client.kill().expect("kill");
        pool.put(dec);
        let mut prov = StreamProvider::new(pool, FeedPolicy::Blocking);
        let r = request(ClipId::new(), &path, 0.5);
        assert!(prov.frames(std::slice::from_ref(&r))[0].is_none());
        let e = prov.pool_mut().take_error().expect("the death is an error");
        assert!(
            matches!(e, EngineError::DecoderClient(ref c) if is_fatal(c)),
            "{e}"
        );
        let f = prov.frames(std::slice::from_ref(&r))[0].take();
        assert!(
            f.is_some(),
            "the failed feed was replaced with a fresh decoder"
        );
        drop(f);
        drop(prov.into_pool());
    }

    #[test]
    fn shared_frames_keep_the_lease_budget_over_many_frames() {
        let Some(pool) = real_pool() else { return };
        let path = crate::decoder_pool::tests::fixture()
            .to_string_lossy()
            .into_owned();
        let mut prov = StreamProvider::new(pool, FeedPolicy::Blocking);
        let clip = ClipId::new();
        let started = Instant::now();
        let mut shared = 0;
        let frames = 90;
        for k in 0..frames {
            let r = request(clip, &path, k as f64 / 30.0);
            let mut out = crate::compose::Prefetched::fetch(&mut prov, vec![r.clone()]);
            let f = out
                .frames(std::slice::from_ref(&r))
                .remove(0)
                .expect("frame");
            shared += usize::from(f.bytes.is_shared());
            // Drawn, then dropped (with `out`) before the next frame, as
            // compose's callers do.
        }
        assert!(started.elapsed() < Duration::from_secs(60), "no deadlock");
        let pool = prov.into_pool();
        let stats = pool.idle_transport_stats();
        assert_eq!(stats.len(), 1);
        if stats[0].shm_bytes == 0 {
            eprintln!("shared memory off (VANCE_IPC_SHM=0?) — lease checks skipped");
            return;
        }
        assert_eq!(
            shared, frames,
            "every frame handed out is a zero-copy share"
        );
        assert_eq!(
            stats[0].spilled, 0,
            "two leases (held + queued) were enough"
        );
    }

    /// Wait up to `secs` for `done`.
    fn wait_for(secs: u64, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[cfg(unix)]
    fn signal(pid: i32, sig: i32) {
        // SAFETY: sends a signal to the test's own sidecar; no memory involved.
        unsafe { libc::kill(pid, sig) };
    }

    #[cfg(unix)]
    #[test]
    fn stopping_does_not_wait_for_a_hung_sidecar() {
        use crate::decoder_pool::tests::{decoder_bin, fixture, pid_recording_decoder, read_pid};
        let Some(real) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let (bin, pid_file) = pid_recording_decoder(dir.path(), &real);
        let pool = DecoderPool::new(
            bin,
            6,
            crate::decoder_pool::MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let mut prov = StreamProvider::new(pool, FeedPolicy::Blocking);
        let path = fixture().to_string_lossy().into_owned();
        let r = request(ClipId::new(), &path, 0.5);
        assert!(prov.frames(std::slice::from_ref(&r))[0].is_some());
        let pid = read_pid(&pid_file);
        // The sidecar hangs mid-stream (a stalled network file, a decoder
        // loop): its worker blocks in a request that never returns.
        signal(pid, libc::SIGSTOP);
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(prov.into_pool());
        });
        let returned = rx.recv_timeout(Duration::from_secs(5));
        let took = started.elapsed();
        // The stuck worker's sidecar was killed (and reaped), not leaked.
        let gone = wait_for(2, || !alive(pid));
        eprintln!("into_pool with a hung sidecar took {took:?}");
        signal(pid, libc::SIGKILL);
        assert!(returned.is_ok(), "into_pool hung on the stuck worker");
        assert!(took < Duration::from_secs(3), "{took:?}");
        assert!(gone, "the hung sidecar is still running after into_pool");
    }

    /// Whether process `pid` exists (a zombie counts: it isn't reaped).
    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn a_fatal_error_on_a_superseded_stream_ends_the_current_one() {
        use crate::decoder_pool::tests::{decoder_bin, fixture, pid_recording_decoder, read_pid};
        let Some(real) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let (bin, pid_file) = pid_recording_decoder(dir.path(), &real);
        let mut pool = DecoderPool::new(
            bin,
            6,
            crate::decoder_pool::MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let path = fixture().to_string_lossy().into_owned();
        let dec = pool.take(&path, 1).expect("take");
        let mut s = Stream::start(dec, 0).expect("start");
        assert!(wait_for(10, || s.shared.state.lock().queue.len() == QUEUE_FRAMES));
        let pid = read_pid(&pid_file);
        signal(pid, libc::SIGSTOP);
        // Room in the queue: the worker asks the stopped sidecar for the
        // next frame of stream 0 and blocks there.
        s.shared.state.lock().queue.clear();
        s.shared.wake.notify_all();
        std::thread::sleep(Duration::from_millis(200));
        // The feed moves on (a seek) while that request is stuck …
        s.restart(1000);
        // … and the sidecar dies under it.
        signal(pid, libc::SIGKILL);
        assert!(wait_for(10, || s.shared.state.lock().worker_gone));
        let st = s.shared.state.lock();
        assert!(
            matches!(&st.end, Some(StreamEnd::Failed(e)) if is_fatal(e)),
            "the dead sidecar must end the current stream, not vanish with stream 0"
        );
    }

    /// Paths whose scrub job waits (a slow file) until released.
    static GATED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static UNGATED: Condvar = Condvar::new();

    pub(super) fn scrub_gate(path: &str) {
        let mut gated = GATED.lock();
        while gated.iter().any(|g| g == path) {
            UNGATED.wait(&mut gated);
        }
    }

    fn gate(path: &str) {
        GATED.lock().push(path.to_owned());
    }

    fn ungate(path: &str) {
        GATED.lock().retain(|g| g != path);
        UNGATED.notify_all();
    }

    /// The frames `fetch` got for `reqs`, with their pending flags.
    fn frames_of(got: &mut ScrubFrames, reqs: &[FrameRequest]) -> Vec<(Option<i64>, bool)> {
        let pending: Vec<bool> = reqs.iter().map(|r| got.is_pending(r)).collect();
        got.frames(reqs)
            .into_iter()
            .zip(pending)
            .map(|(f, p)| (f.map(|f| f.pts), p))
            .collect()
    }

    #[test]
    fn a_slow_scrub_layer_holds_its_picture_while_a_fast_one_updates() {
        let Some(mut pool) = real_pool() else { return };
        let dir = tempfile::tempdir().unwrap();
        let files = crate::decoder_pool::tests::copies(dir.path(), 2);
        let (fast, slow) = (&files[0], &files[1]);
        let landed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&landed);
        let on_landed: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let mut fetcher = ScrubFetcher::new(Some(on_landed));
        let cache =
            Arc::new(FrameCache::new(dir.path().join("cache"), 1 << 24, 1 << 26).expect("cache"));
        let (a, b) = (ClipId::new(), ClipId::new());
        let at = |t: f64| vec![request(a, fast, t), request(b, slow, t)];
        // Both decode within the wait.
        let first = at(0.5);
        let mut got = fetcher.fetch(&mut pool, &cache, first.clone(), Duration::from_secs(20));
        assert!(!got.pending());
        let shown = frames_of(&mut got, &first);
        let (Some(a0), Some(b0)) = (shown[0].0, shown[1].0) else {
            panic!("both decoded: {shown:?}")
        };
        drop(got);
        // The slow file stalls: the fast layer updates at once, the slow
        // one keeps its old picture and is pending.
        gate(slow);
        // A safety net: without the fix, `fetch` would wait for the gate.
        ungate_later(slow, Duration::from_secs(3));
        let second = at(2.0);
        let started = Instant::now();
        let mut got = fetcher.fetch(
            &mut pool,
            &cache,
            second.clone(),
            Duration::from_millis(300),
        );
        let took = started.elapsed();
        let shown = frames_of(&mut got, &second);
        drop(got);
        assert!(
            took < Duration::from_secs(2),
            "one slow file held the frame: {took:?}"
        );
        let (a1, pending_a) = shown[0];
        assert!(
            a1.is_some_and(|p| p > a0) && !pending_a,
            "the fast layer updated"
        );
        assert_eq!(shown[1], (Some(b0), true), "the slow one holds its picture");
        // It lands: the next fetch shows it.
        let before = landed.load(std::sync::atomic::Ordering::SeqCst);
        ungate(slow);
        assert!(wait_for(20, || !fetcher_busy_after(&landed, before)));
        let mut got = fetcher.fetch(&mut pool, &cache, second.clone(), Duration::ZERO);
        let shown = frames_of(&mut got, &second);
        assert!(!got.pending());
        assert_eq!(shown[0], (a1, false), "the fast layer, unchanged");
        assert!(
            shown[1].0.is_some_and(|p| p > b0) && !shown[1].1,
            "the slow layer landed"
        );
    }

    /// Whether no layer landed since `before` was counted.
    fn fetcher_busy_after(landed: &std::sync::atomic::AtomicUsize, before: usize) -> bool {
        landed.load(std::sync::atomic::Ordering::SeqCst) <= before
    }

    /// Release `path`'s gate after `after`, in case a reverted fix blocks.
    fn ungate_later(path: &str, after: Duration) {
        let path = path.to_owned();
        std::thread::spawn(move || {
            std::thread::sleep(after);
            ungate(&path);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_scrub_decode_is_reaped_without_another_fetch() {
        use crate::decoder_pool::tests::{decoder_bin, fixture, pid_recording_decoder, read_pid};
        let Some(real) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let (bin, pid_file) = pid_recording_decoder(dir.path(), &real);
        let mut pool = DecoderPool::new(
            bin,
            6,
            crate::decoder_pool::MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let cache =
            Arc::new(FrameCache::new(dir.path().join("cache"), 1 << 24, 1 << 26).expect("cache"));
        let mut fetcher = ScrubFetcher::new(None);
        fetcher.hang_limit = Duration::from_millis(300);
        let path = fixture().to_string_lossy().into_owned();
        let clip = ClipId::new();
        let first = vec![request(clip, &path, 0.5)];
        drop(fetcher.fetch(&mut pool, &cache, first, Duration::from_secs(20)));
        let pid = read_pid(&pid_file);
        signal(pid, libc::SIGSTOP);
        let next = vec![request(clip, &path, 3.0)];
        assert!(fetcher
            .fetch(&mut pool, &cache, next.clone(), Duration::from_millis(50))
            .pending());
        // The user stops scrubbing: only the timer runs from here.
        let early = Instant::now();
        fetcher.reap_hung();
        assert!(fetcher.busy(), "not killed before the limit");
        assert!(
            early.elapsed() < Duration::from_millis(100),
            "reap_hung doesn't wait"
        );
        std::thread::sleep(Duration::from_millis(400));
        let ended = wait_for(10, || {
            fetcher.reap_hung();
            !fetcher.busy()
        });
        signal(pid, libc::SIGKILL);
        assert!(ended, "the stuck decode was never killed");
        // The next fetch takes in its error and drops the dead decoder.
        let got = fetcher.fetch(&mut pool, &cache, next.clone(), Duration::ZERO);
        assert!(got.failure(&next[0]).is_some(), "the layer reports it");
        assert!(wait_for(10, || !alive(pid)), "the dead decoder was reaped");
    }

    #[cfg(unix)]
    #[test]
    fn a_scrub_layer_that_never_answers_is_killed_after_the_limit() {
        use crate::decoder_pool::tests::{decoder_bin, fixture, pid_recording_decoder, read_pid};
        let Some(real) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let (bin, pid_file) = pid_recording_decoder(dir.path(), &real);
        let mut pool = DecoderPool::new(
            bin,
            6,
            crate::decoder_pool::MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let cache =
            Arc::new(FrameCache::new(dir.path().join("cache"), 1 << 24, 1 << 26).expect("cache"));
        let mut fetcher = ScrubFetcher::new(None);
        fetcher.hang_limit = Duration::from_millis(300);
        let path = fixture().to_string_lossy().into_owned();
        let clip = ClipId::new();
        let first = vec![request(clip, &path, 0.5)];
        let mut got = fetcher.fetch(&mut pool, &cache, first.clone(), Duration::from_secs(20));
        assert!(got.frames(&first)[0].is_some());
        drop(got);
        let pid = read_pid(&pid_file);
        signal(pid, libc::SIGSTOP);
        // Its next frame never comes: pending, then killed past the limit.
        let next = vec![request(clip, &path, 3.0)];
        let got = fetcher.fetch(&mut pool, &cache, next.clone(), Duration::from_millis(50));
        assert!(got.pending());
        std::thread::sleep(Duration::from_millis(400));
        drop(fetcher.fetch(&mut pool, &cache, next.clone(), Duration::from_millis(50)));
        // The kill makes its request fail: the job lands with an error, and
        // its (dead) decoder is dropped, not kept.
        let landed = wait_for(10, || {
            let got = fetcher.fetch(&mut pool, &cache, next.clone(), Duration::from_millis(50));
            got.failure(&next[0]).is_some()
        });
        let gone = wait_for(10, || !alive(pid));
        signal(pid, libc::SIGKILL);
        assert!(landed, "the killed layer reports an error");
        // Reaped, so not kept as an idle decoder either.
        assert!(gone, "the hung decoder is still there");
    }
}
