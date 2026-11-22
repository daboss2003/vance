//! Timeline-driven compositing playback (ADR 0007): one loop over the
//! sequence with per-clip streaming feeds and an audio-master clock. There
//! is no clip chaining: the mixer runs straight through cuts and gaps play
//! as black.
//!
//! The loop walks the sequence's frame grid. Frame `k` (at `k / fps`) is
//! drawn once the clock reaches it; a frame whose successor is already due
//! (it came due while the previous one was drawn, or the clock jumped while
//! waiting for it) is skipped and counted as dropped, so the picture never
//! falls behind the sound. The clock is the mixer's
//! output position (`from + master_frame / sample_rate`), or the wall clock
//! when there is no audio.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use vance_core::RationalTime;
use vance_render::Compositor;
use vance_timeline::{scene_plan_at, transition_window, Project, Sequence, TrackId, TrackKind};
use vance_viewport::{Viewport, ViewportBounds};

use crate::compose::{
    compose_frame, load_plan_luts, output_spec_for, plan_requests_with, ComposeCtx,
    ComposeResources, FileSnapshot, FrameProvider, FrameRequest, Prefetched, Purpose, ViewerState,
};
use crate::decoder_pool::{DecoderPool, SidecarRegistry};
use crate::engine::{acquire_surface_texture, AudioSync, EngineError, RenderState};
use crate::layer_feed::StreamProvider;

/// How far ahead of the playhead clips get their streams started, so a
/// stream is running by its cut (a sidecar spawn takes tens of ms).
pub const LOOKAHEAD_SECONDS: f64 = 1.0;

/// How long `SequencePlayback::stop` waits for the session's thread before
/// killing the sidecars it may be blocked on.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long `stop` then waits before abandoning the thread.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Timeline seconds → `RationalTime` at microsecond precision. A frame time
/// `k / fps` then lands within half a microsecond of its exact value, so the
/// providers' nearest-PTS rounding picks frame `k`. (Millisecond rounding,
/// used for clip placement, can push a frame time past the frame it names.)
pub fn timeline_rt(seconds: f64) -> RationalTime {
    let micros = if seconds.is_finite() {
        (seconds.max(0.0) * 1e6).round() as i64
    } else {
        0
    };
    RationalTime::new(micros, 1_000_000).unwrap_or(RationalTime::ZERO)
}

/// The sequence's frame rate, or 24 fps when it is unusable.
pub fn sequence_fps(seq: &Sequence) -> f64 {
    let fps = seq.frame_rate.0.as_f64();
    if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        24.0
    }
}

/// Frame requests for the clips and transition windows that begin within
/// `(t, t + LOOKAHEAD_SECONDS]`, at their first needed source time: what to
/// hand [`crate::compose::FrameProvider::prepare`].
pub fn upcoming_requests(
    project: &Project,
    seq: &Sequence,
    t: f64,
    purpose: Purpose,
    track_filter: Option<TrackId>,
) -> Vec<FrameRequest> {
    upcoming_requests_with(project, seq, t, purpose, track_filter, None)
}

/// [`upcoming_requests`], checking proxies against `files` when given.
fn upcoming_requests_with(
    project: &Project,
    seq: &Sequence,
    t: f64,
    purpose: Purpose,
    track_filter: Option<TrackId>,
    files: Option<&FileSnapshot>,
) -> Vec<FrameRequest> {
    let horizon = t + LOOKAHEAD_SECONDS;
    let mut starts: Vec<f64> = Vec::new();
    for track in seq
        .tracks
        .iter()
        .filter(|tr| tr.kind == TrackKind::Video && tr.enabled)
    {
        if track_filter.is_some_and(|f| f != track.id) {
            continue;
        }
        let clips = track.clips.iter().filter(|c| c.enabled);
        starts.extend(clips.map(|c| c.timeline_in.0.as_f64()));
        // The window as rendered (an over-long saved dissolve is capped);
        // an orphaned transition renders nothing.
        starts.extend(
            track
                .transitions
                .iter()
                .filter_map(|tr| transition_window(track, tr))
                .map(|(start, _)| start.0.as_f64()),
        );
    }
    starts.retain(|&s| s > t && s <= horizon);
    starts.sort_by(f64::total_cmp);
    starts.dedup();
    // Plan half a frame inside each start (a cut sits exactly on the
    // boundary), then pull the requests back to the start itself plus a
    // millisecond, so the stream opens at or before the first frame asked of
    // it instead of restarting there. Every request's source time moves 1:1
    // with timeline time.
    let nudge = 0.5 / sequence_fps(seq);
    let mut out: Vec<FrameRequest> = Vec::new();
    for s in starts {
        let plan = scene_plan_at(seq, timeline_rt(s + nudge));
        for mut r in plan_requests_with(project, seq, &plan, purpose, track_filter, files) {
            if out.iter().any(|o| o.clip_id == r.clip_id) {
                continue;
            }
            r.source_seconds -= nudge + 0.001;
            out.push(r);
        }
    }
    out
}

/// Compose into the Record viewport's surface and present it: `compose`
/// records the frame (`compose_frame`), then the output pass runs into the
/// surface texture.
pub(crate) fn present_composed(
    rs: &mut RenderState,
    viewport: &Viewport,
    bounds: ViewportBounds,
    compose: impl FnOnce(&mut Compositor) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let surface = viewport.surface();
    if rs.surface_config.is_none() && !bounds.is_empty() {
        if let Some(cfg) = rs
            .renderer
            .recommended_surface_config(surface, bounds.w, bounds.h)
        {
            rs.renderer.configure_surface(surface, &cfg);
            rs.surface_config = Some(cfg);
        }
    }
    let format = rs
        .surface_config
        .as_ref()
        .map(|c| c.format)
        .ok_or_else(|| EngineError::SurfaceAcquire("viewport bounds not yet reported".into()))?;
    compose(rs.renderer.compositor())?;
    let frame = acquire_surface_texture(surface, &mut rs.surface_config)?;
    let view = frame
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    rs.renderer.compositor().finish_to_view(&view, format)?;
    frame.present();
    Ok(())
}

/// Where a playback session draws.
pub enum PlaybackTarget {
    /// The Record viewport's surface.
    Viewport(Arc<Viewport>),
    /// An offscreen texture (headless sessions: tests and measurements).
    // Intentional: the app always plays to the viewport; tests build this.
    #[allow(dead_code)]
    Offscreen(wgpu::Texture),
}

/// Everything a session needs; moved onto its thread.
pub struct PlaybackInputs {
    /// Snapshot of the project (media library, colour settings).
    pub project: Project,
    /// Snapshot of the sequence to play.
    pub seq: Sequence,
    /// Timeline time of the first frame; on the frame grid.
    pub from_seconds: f64,
    /// The mixer's clock, or `None` for the wall clock.
    pub audio: Option<AudioSync>,
    /// Renderer shared with the engine.
    pub render_state: Arc<Mutex<RenderState>>,
    /// Where frames go.
    pub target: PlaybackTarget,
    /// Realtime feeds over the preview decoder pool.
    pub provider: StreamProvider,
    /// Compose resources (add-ons, LUTs, synthetic media caches).
    pub res: ComposeResources,
}

/// What a finished session hands back to the engine.
pub struct PlaybackReturn {
    /// The preview pool, with every feed's decoder back in it.
    pub pool: DecoderPool,
    /// The compose resources.
    pub res: ComposeResources,
    /// Timeline time of the last frame drawn (the sequence end after a
    /// natural end).
    pub playhead: f64,
    /// Frames drawn.
    pub presented: u64,
    /// Frames skipped because they came due while another was drawn.
    pub dropped: u64,
    /// Proxy requests (see [`SequencePlayback::take_proxy_reports`]) not
    /// taken before the session ended, including its last frames'.
    pub proxy_requests: Vec<String>,
    /// Broken proxies (see [`SequencePlayback::take_proxy_reports`]) not
    /// taken before the session ended.
    pub broken_proxies: Vec<String>,
}

/// What a session's pool reported about proxies since the last take. Both
/// lists live under one lock and are taken together, so a broken proxy is
/// never separated from its original's request.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProxyReports {
    /// Originals that need a proxy (see [`DecoderPool::take_proxy_requests`]).
    pub requests: Vec<String>,
    /// Proxy files that wouldn't open (see
    /// [`DecoderPool::take_broken_proxies`]).
    pub broken_proxies: Vec<String>,
}

impl ProxyReports {
    /// Move the pool's new reports in.
    fn collect(&mut self, pool: &mut DecoderPool) {
        self.requests.extend(pool.take_proxy_requests());
        self.broken_proxies.extend(pool.take_broken_proxies());
    }
}

/// A running compositing playback session.
pub struct SequencePlayback {
    stop: Arc<AtomicBool>,
    playhead_micros: Arc<AtomicI64>,
    reports: Arc<Mutex<ProxyReports>>,
    /// The session pool's busy sidecars.
    sidecars: SidecarRegistry,
    /// What `stop` hands back if the thread has to be abandoned.
    spare: Option<(DecoderPool, ComposeResources)>,
    /// Frames drawn so far (tests step a session frame by frame).
    #[cfg(test)]
    presented: Arc<AtomicU64>,
    handle: JoinHandle<PlaybackReturn>,
}

impl SequencePlayback {
    /// Start the session's thread.
    pub fn spawn(mut inputs: PlaybackInputs) -> Result<Self, EngineError> {
        let sidecars = inputs.provider.pool_mut().sidecars();
        let spare = Some((inputs.provider.pool_mut().fresh(), inputs.res.fresh()));
        let stop = Arc::new(AtomicBool::new(false));
        let playhead_micros = Arc::new(AtomicI64::new(micros(inputs.from_seconds)));
        let reports = Arc::new(Mutex::new(ProxyReports::default()));
        let presented = Arc::new(AtomicU64::new(0));
        let shared = Shared {
            stop: stop.clone(),
            playhead_micros: playhead_micros.clone(),
            reports: reports.clone(),
            presented: presented.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("vance-engine.comp-playback".into())
            .spawn(move || run(inputs, &shared))
            .map_err(|e| EngineError::PlaybackThread(format!("spawn: {e}")))?;
        #[cfg(not(test))]
        drop(presented);
        Ok(Self {
            stop,
            playhead_micros,
            reports,
            sidecars,
            spare,
            #[cfg(test)]
            presented,
            handle,
        })
    }

    /// Frames drawn so far.
    #[cfg(test)]
    fn frames_presented(&self) -> u64 {
        self.presented.load(Ordering::Acquire)
    }

    /// Timeline time of the frame on screen.
    pub fn playhead_seconds(&self) -> f64 {
        self.playhead_micros.load(Ordering::Acquire) as f64 / 1e6
    }

    /// The thread has exited (end of sequence, or a panic).
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// The proxy requests and broken proxies reported since the last take,
    /// together (see [`ProxyReports`]).
    pub fn take_proxy_reports(&self) -> ProxyReports {
        std::mem::take(&mut *self.reports.lock())
    }

    /// Stop the session and wait for its thread.
    /// The requests and broken proxies the session reported but nobody took
    /// yet come back in the [`PlaybackReturn`]: they are drained AFTER the
    /// join, so the last frames' can't be lost.
    ///
    /// It always returns within about [`STOP_GRACE`] + [`KILL_GRACE`]. A
    /// thread still running after `STOP_GRACE` is blocked; the session's busy
    /// sidecars are killed, which ends a wait on one (an open or a read on a
    /// hung network share). A thread STILL running after `KILL_GRACE` is
    /// stuck in a call no kill reaches (its own stat or read on a hung
    /// mount): it is abandoned, and a fresh pool and resources come back
    /// instead of its own (logged as an error; see the `Intentional:` note).
    pub fn stop(self) -> Result<PlaybackReturn, EngineError> {
        let SequencePlayback {
            stop,
            playhead_micros,
            reports,
            sidecars,
            mut spare,
            handle,
            ..
        } = self;
        stop.store(true, Ordering::Release);
        if !finishes_within(&handle, STOP_GRACE) {
            let killed = sidecars.kill_busy();
            tracing::warn!(
                killed,
                "playback didn't stop in time; killed its busy decoders"
            );
            if !finishes_within(&handle, KILL_GRACE) {
                // Intentional: abandoned, not joined: a stat or read on a
                // hung mount never returns, and stopping must. The thread
                // keeps its pool (its idle sidecars, and any busy one, which
                // dies when its decoder drops) and its title cache until the
                // call returns; it then sees the stop flag and ends, and its
                // results are dropped. Its registry stays stopping, so any
                // sidecar it starts meanwhile is killed at once.
                tracing::error!(
                    "playback thread stuck outside its decoders (a file on a hung \
                     network share?); abandoned it, continuing with a fresh decoder \
                     pool and caches"
                );
                drop(handle);
                let (pool, res) = spare.take().expect("taken once, here");
                let reports = std::mem::take(&mut *reports.lock());
                return Ok(PlaybackReturn {
                    pool,
                    res,
                    playhead: playhead_micros.load(Ordering::Acquire) as f64 / 1e6,
                    presented: 0,
                    dropped: 0,
                    proxy_requests: reports.requests,
                    broken_proxies: reports.broken_proxies,
                });
            }
        }
        let joined = handle.join();
        sidecars.resume();
        let mut done = joined
            .map_err(|_| EngineError::PlaybackThread("compositing playback panicked".into()))?;
        let reports = std::mem::take(&mut *reports.lock());
        done.proxy_requests = reports.requests;
        done.broken_proxies = reports.broken_proxies;
        Ok(done)
    }
}

/// Whether `handle`'s thread finishes within `wait`.
fn finishes_within<T>(handle: &JoinHandle<T>, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}

fn micros(seconds: f64) -> i64 {
    (seconds * 1e6).round() as i64
}

/// The session's side of the handles in [`SequencePlayback`].
struct Shared {
    stop: Arc<AtomicBool>,
    playhead_micros: Arc<AtomicI64>,
    reports: Arc<Mutex<ProxyReports>>,
    /// Frames drawn so far.
    presented: Arc<AtomicU64>,
}

/// Timeline-seconds clock: the mixer's output position, else the wall
/// clock, both starting at `from`.
struct Clock {
    audio: Option<AudioSync>,
    from: f64,
    started: Instant,
}

impl Clock {
    fn now(&self) -> f64 {
        match &self.audio {
            Some(a) => {
                let (num, den) = a.time_base;
                let pts = a.current_pts.load(Ordering::Acquire);
                let elapsed = if den != 0 {
                    pts as f64 * num as f64 / den as f64
                } else {
                    0.0
                };
                self.from + elapsed
            }
            None => self.from + self.started.elapsed().as_secs_f64(),
        }
    }

    /// Sleep until the clock reaches `t`; `false` if stopped first.
    fn wait_until(&self, t: f64, stop: &AtomicBool) -> bool {
        loop {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            let ahead = t - self.now();
            if ahead <= 0.0005 {
                return true;
            }
            std::thread::sleep(Duration::from_secs_f64(ahead.min(0.005)));
        }
    }
}

fn run(inputs: PlaybackInputs, shared: &Shared) -> PlaybackReturn {
    let PlaybackInputs {
        project,
        seq,
        from_seconds,
        audio,
        render_state,
        target,
        mut provider,
        mut res,
    } = inputs;
    let fps = sequence_fps(&seq);
    let end = seq.duration().0.as_f64();
    let clock = Clock {
        audio,
        from: from_seconds,
        started: Instant::now(),
    };
    let mut k = (from_seconds * fps).round() as i64;
    let mut playhead = from_seconds;
    let mut ended = false;
    let mut warned = false;
    let (mut presented, mut dropped) = (0u64, 0u64);
    tracing::info!(from_seconds, fps, end, "compositing playback started");
    // Checked once, here on the session thread, not per frame.
    let files = Arc::new(FileSnapshot::of(&project));
    res.set_file_snapshot(Some(files.clone()));
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let due = (clock.now() * fps).floor() as i64;
        if due > k {
            dropped += (due - k) as u64;
            k = due;
        }
        let t = k as f64 / fps;
        if t >= end {
            ended = true;
            break;
        }
        if !clock.wait_until(t, &shared.stop) {
            break;
        }
        // The clock jumped past this frame while we waited: it's stale now,
        // and the top of the loop drops it.
        if (clock.now() * fps).floor() as i64 > k {
            continue;
        }
        provider.prepare(&upcoming_requests_with(
            &project,
            &seq,
            t,
            Purpose::Preview,
            None,
            Some(&files),
        ));
        let frame = FrameInputs {
            project: &project,
            seq: &seq,
            t,
        };
        match draw(&render_state, &target, &frame, &mut provider, &mut res) {
            Ok(()) => presented += 1,
            Err(e) if !warned => {
                warned = true;
                tracing::warn!(error = %e, t, "playback: frame not drawn (later failures not logged)");
            }
            Err(_) => {}
        }
        playhead = t;
        shared.playhead_micros.store(micros(t), Ordering::Release);
        // After the playhead: whoever sees the count sees this frame's time.
        shared.presented.store(presented, Ordering::Release);
        let pool = provider.pool_mut();
        shared.reports.lock().collect(pool);
        // Realtime feeds hold a failed layer's last frame; the pool already
        // logged the reason.
        let _ = pool.take_error();
        k += 1;
    }
    if ended {
        playhead = end;
        shared.playhead_micros.store(micros(end), Ordering::Release);
    }
    let mut pool = provider.into_pool();
    shared.reports.lock().collect(&mut pool);
    // The engine's own (scrub) use stats files as before.
    res.set_file_snapshot(None);
    PlaybackReturn {
        pool,
        res,
        playhead,
        presented,
        dropped,
        // Filled by `SequencePlayback::stop` from the shared lists.
        proxy_requests: Vec::new(),
        broken_proxies: Vec::new(),
    }
}

struct FrameInputs<'a> {
    project: &'a Project,
    seq: &'a Sequence,
    t: f64,
}

/// Compose the frame at `f.t` and send it to `target`. Frames are fetched
/// before the render state is locked: a feed can wait up to a second for
/// its first frame, and the UI's renderer calls must not wait with it.
fn draw(
    render_state: &Mutex<RenderState>,
    target: &PlaybackTarget,
    f: &FrameInputs<'_>,
    provider: &mut dyn FrameProvider,
    res: &mut ComposeResources,
) -> Result<(), EngineError> {
    let plan = scene_plan_at(f.seq, timeline_rt(f.t));
    let out = output_spec_for(f.project, f.seq, Purpose::Preview);
    let files = res.file_snapshot();
    let reqs = plan_requests_with(f.project, f.seq, &plan, Purpose::Preview, None, files);
    // Dropped when this returns, after the frame is presented: the frames
    // are shares of the feeds' leases.
    let mut frames = Prefetched::fetch(provider, reqs);
    // Read before the render lock too: a LUT on a hung share must not
    // freeze the renderer.
    load_plan_luts(f.seq, &plan, &res.luts);
    let mut ctx = ComposeCtx {
        project: f.project,
        seq: f.seq,
        purpose: Purpose::Preview,
        // Intentional: viewer solos (grade / keyer mattes) are a paused-
        // workflow tool, as before ADR 0007; playback shows the picture.
        viewer: ViewerState::default(),
        track_filter: None,
        realtime: true,
        res,
    };
    let mut rs = render_state.lock();
    match target {
        PlaybackTarget::Viewport(viewport) => {
            let bounds = viewport.current_bounds();
            present_composed(&mut rs, viewport, bounds, |c| {
                compose_frame(c, &plan, &mut frames, &mut ctx, &out).map(|_| ())
            })
        }
        PlaybackTarget::Offscreen(texture) => {
            let c = rs.renderer.compositor();
            compose_frame(c, &plan, &mut frames, &mut ctx, &out)?;
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            c.finish_to_view(&view, texture.format())?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder_pool::MemoryBudget;
    use crate::layer_feed::FeedPolicy;
    use vance_core::FrameRate;
    use vance_timeline::{ops, Clip, MediaItem, MediaKind, TitleParams, TransitionKind};

    fn rt(s: f64) -> RationalTime {
        RationalTime::new((s * 1000.0).round() as i64, 1000).unwrap()
    }

    #[test]
    fn frame_times_round_to_the_microsecond() {
        assert_eq!(
            timeline_rt(1.0 / 24.0),
            RationalTime::new(41_667, 1_000_000).unwrap()
        );
        assert_eq!(timeline_rt(-3.0), RationalTime::ZERO);
        assert_eq!(timeline_rt(f64::NAN), RationalTime::ZERO);
        // 1/24 s at a 1/12288 time base is PTS 512 exactly after rounding.
        let pts = crate::decoder_pool::source_seconds_to_pts(
            timeline_rt(1.0 / 24.0).0.as_f64(),
            (1, 12_288),
        );
        assert_eq!(pts, 512);
    }

    #[test]
    fn upcoming_requests_name_clips_and_transition_sides_ahead() {
        let mut project = Project::new("p");
        let media: Vec<MediaItem> = (0..3)
            .map(|i| {
                MediaItem::new(
                    format!("m{i}.mp4").into(),
                    format!("m{i}"),
                    MediaKind::Video,
                    rt(10.0),
                )
            })
            .collect();
        for m in &media {
            project.media.insert(m.id, Arc::new(m.clone()));
        }
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let a = Clip::new(media[0].id, "a", rt(0.0), rt(0.0), rt(2.0)).unwrap();
        let b = Clip::new(media[1].id, "b", rt(2.0), rt(1.0), rt(5.0)).unwrap();
        let c = Clip::new(media[2].id, "c", rt(6.0), rt(0.0), rt(2.0)).unwrap();
        let (a_id, b_id, c_id) = (a.id, b.id, c.id);
        for clip in [a, b, c] {
            seq = ops::insert_clip(&seq, v1, clip).unwrap();
        }
        let (seq, _) =
            ops::add_transition_at_cut(&seq, v1, a_id, TransitionKind::Dissolve, rt(0.5)).unwrap();

        // At 1.0 the dissolve window [1.75, 2.25] is ahead: both sides.
        let reqs = upcoming_requests(&project, &seq, 1.0, Purpose::Preview, None);
        let ids: Vec<_> = reqs.iter().map(|r| r.clip_id).collect();
        assert_eq!(ids, vec![a_id, b_id]);
        // b's first needed source time: source_in 1.0 − 0.25 pre-roll, less
        // the 1 ms guard (to the planner's microsecond).
        assert!((reqs[1].source_seconds - (0.75 - 0.001)).abs() < 1e-6);
        // Nothing starts within a second of 3.0; c (at 6.0) is past it.
        assert!(upcoming_requests(&project, &seq, 3.0, Purpose::Preview, None).is_empty());
        let reqs = upcoming_requests(&project, &seq, 5.5, Purpose::Preview, None);
        assert_eq!(
            reqs.iter().map(|r| r.clip_id).collect::<Vec<_>>(),
            vec![c_id]
        );
        assert!((reqs[0].source_seconds + 0.001).abs() < 1e-6);
        // A filtered-out track contributes nothing.
        let other = TrackId::new();
        assert!(upcoming_requests(&project, &seq, 5.5, Purpose::Preview, Some(other)).is_empty());

        // An over-long dissolve saved by an older build (6 s across a 2 s
        // clip) renders capped to [1, 3], so its sides are due by 1.0, not
        // by its stored start (2 − 3, clamped to 0).
        let mut long = seq.clone();
        let track = Arc::make_mut(&mut long.tracks[0]);
        Arc::make_mut(&mut track.transitions[0]).duration = rt(6.0);
        assert_eq!(
            long.tracks[0].transitions[0].window_in(),
            RationalTime::ZERO
        );
        let reqs = upcoming_requests(&project, &long, 0.5, Purpose::Preview, None);
        assert_eq!(
            reqs.iter().map(|r| r.clip_id).collect::<Vec<_>>(),
            vec![a_id, b_id],
            "prefetched from the capped window"
        );
        // An orphaned transition (its clips gone) prefetches nothing.
        let mut orphan = seq.clone();
        let track = Arc::make_mut(&mut orphan.tracks[0]);
        track.clips.retain(|c| c.id == c_id);
        assert!(upcoming_requests(&project, &orphan, 1.0, Purpose::Preview, None).is_empty());
    }

    fn headless_target(renderer: &vance_render::Renderer) -> wgpu::Texture {
        renderer.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("test.playback-target"),
            size: wgpu::Extent3d {
                width: 64,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    }

    /// Checks, from inside `frames`, whether the render state is free; then
    /// takes `wait` (a slow decode).
    struct LockProbe {
        render_state: Arc<Mutex<RenderState>>,
        wait: Duration,
        calls: usize,
        while_unlocked: usize,
    }

    impl FrameProvider for LockProbe {
        fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<vance_decoder::FrameData>> {
            self.calls += 1;
            if self.render_state.try_lock().is_some() {
                self.while_unlocked += 1;
            }
            std::thread::sleep(self.wait);
            vec![None; reqs.len()]
        }
    }

    #[test]
    fn frames_are_fetched_before_the_render_state_is_locked() {
        let renderer = match vance_render::Renderer::new(vance_render::RendererConfig::default()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("no GPU ({e}) — skipping");
                return;
            }
        };
        let target = PlaybackTarget::Offscreen(headless_target(&renderer));
        let render_state = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        let plate = MediaItem::new("plate.mp4".into(), "plate", MediaKind::Video, rt(5.0));
        let mut project = Project::new("p");
        project.media.insert(plate.id, Arc::new(plate.clone()));
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let clip = Clip::new(plate.id, "plate", rt(0.0), rt(0.0), rt(5.0)).unwrap();
        seq = ops::insert_clip(&seq, v1, clip).unwrap();
        seq.frame_size = Some((64, 32));
        let mut res = ComposeResources::new(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let wait = Duration::from_millis(60);
        let mut probe = LockProbe {
            render_state: render_state.clone(),
            wait,
            calls: 0,
            while_unlocked: 0,
        };
        let frame = FrameInputs {
            project: &project,
            seq: &seq,
            t: 1.0,
        };
        for _ in 0..3 {
            draw(&render_state, &target, &frame, &mut probe, &mut res).expect("draw");
        }
        assert_eq!(probe.calls, 3, "one fetch per frame");
        assert_eq!(
            probe.while_unlocked, 3,
            "the fetch never holds the render state"
        );

        // Meanwhile a UI call keeps asking for the renderer: how long does
        // it wait? (Reported, not asserted: it is timing.)
        let done = Arc::new(AtomicBool::new(false));
        let ui = {
            let (rs, done) = (render_state.clone(), done.clone());
            std::thread::spawn(move || {
                let mut longest = Duration::ZERO;
                while !done.load(Ordering::Acquire) {
                    let asked = Instant::now();
                    drop(rs.lock());
                    longest = longest.max(asked.elapsed());
                    std::thread::sleep(Duration::from_millis(1));
                }
                longest
            })
        };
        for _ in 0..3 {
            draw(&render_state, &target, &frame, &mut probe, &mut res).expect("draw");
        }
        done.store(true, Ordering::Release);
        let longest = ui.join().unwrap();
        eprintln!(
            "longest UI wait for the renderer over 3 draws with {wait:?} decodes: {longest:?}"
        );
    }

    /// A finished session of a 0.25 s clip of the fixture whose only proxy
    /// is junk, over a 1-byte memory budget: the pool falls back to the
    /// original and reports both the request and the broken proxy.
    /// `None` without a sidecar, a GPU or shared memory.
    fn over_budget_session(junk_proxy: &std::path::Path) -> Option<SequencePlayback> {
        let bin = crate::decoder_pool::tests::decoder_bin()?;
        let renderer = vance_render::Renderer::new(vance_render::RendererConfig::default())
            .map_err(|e| eprintln!("no GPU ({e}) — skipping"))
            .ok()?;
        let target = PlaybackTarget::Offscreen(headless_target(&renderer));
        let render_state = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        let path = crate::decoder_pool::tests::fixture();
        let plate = MediaItem::new(path.clone(), "plate", MediaKind::Video, rt(5.0));
        let mut project = Project::new("p");
        project.media.insert(plate.id, Arc::new(plate.clone()));
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let clip = Clip::new(plate.id, "plate", rt(0.0), rt(0.0), rt(0.25)).unwrap();
        seq = ops::insert_clip(&seq, v1, clip).unwrap();
        seq.frame_size = Some((64, 32));
        let mut pool = DecoderPool::new(bin, 2, MemoryBudget { cap_bytes: 1 });
        pool.set_proxy_paths(std::collections::HashMap::from([(
            path.to_string_lossy().into_owned(),
            junk_proxy.to_string_lossy().into_owned(),
        )]));
        let session = SequencePlayback::spawn(PlaybackInputs {
            project,
            seq,
            from_seconds: 0.0,
            audio: None,
            render_state,
            target,
            provider: StreamProvider::new(pool, FeedPolicy::Realtime),
            res: ComposeResources::new(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
            ),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !session.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(session.is_finished());
        Some(session)
    }

    #[cfg(unix)]
    #[test]
    fn a_stuck_open_does_not_block_stop() {
        use crate::decoder_pool::tests::{decoder_bin, fixture, read_pid, stuck_open_decoder};
        let Some(real) = decoder_bin() else { return };
        let renderer = match vance_render::Renderer::new(vance_render::RendererConfig::default()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("no GPU ({e}) — skipping");
                return;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let (bin, pid_file, feeder_file) = stuck_open_decoder(dir.path(), &real);
        let target = PlaybackTarget::Offscreen(headless_target(&renderer));
        let render_state = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        let plate = MediaItem::new(fixture(), "plate", MediaKind::Video, rt(5.0));
        let mut project = Project::new("p");
        project.media.insert(plate.id, Arc::new(plate.clone()));
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let clip = Clip::new(plate.id, "plate", rt(0.0), rt(0.0), rt(5.0)).unwrap();
        seq = ops::insert_clip(&seq, v1, clip).unwrap();
        seq.frame_size = Some((64, 32));
        let pool = DecoderPool::new(
            bin,
            2,
            MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let session = SequencePlayback::spawn(PlaybackInputs {
            project,
            seq,
            from_seconds: 0.0,
            audio: None,
            render_state,
            target,
            provider: StreamProvider::new(pool, FeedPolicy::Realtime),
            res: ComposeResources::new(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
            ),
        })
        .unwrap();
        // The first frame's open is stuck in the sidecar.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !feeder_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(session.stop());
        });
        let stopped = rx.recv_timeout(Duration::from_secs(20));
        let took = started.elapsed();
        for file in [&pid_file, &feeder_file] {
            if file.exists() {
                // SAFETY: kills the test's own helper processes.
                unsafe { libc::kill(read_pid(file), libc::SIGKILL) };
            }
        }
        eprintln!("stop with a stuck open took {took:?}");
        let done = stopped.expect("stop hung on the stuck open").expect("stop");
        // A sanity bound only: by design stop takes STOP_GRACE (+ the kill).
        assert!(
            took < STOP_GRACE + KILL_GRACE + Duration::from_secs(3),
            "{took:?}"
        );
        // The session's own pool came back (not an abandoned session's
        // fresh one), without the killed decoder.
        assert!(done.pool.spawn_count() >= 1, "the session's pool");
        assert!(
            done.pool.idle_transport_stats().is_empty(),
            "no dead decoder kept"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_decoder_stuck_starting_up_does_not_block_stop() {
        use crate::decoder_pool::tests::{fixture, read_pid, silent_decoder};
        let renderer = match vance_render::Renderer::new(vance_render::RendererConfig::default()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("no GPU ({e}) — skipping");
                return;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let (bin, pid_file) = silent_decoder(dir.path());
        let target = PlaybackTarget::Offscreen(headless_target(&renderer));
        let render_state = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        let plate = MediaItem::new(fixture(), "plate", MediaKind::Video, rt(5.0));
        let mut project = Project::new("p");
        project.media.insert(plate.id, Arc::new(plate.clone()));
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let clip = Clip::new(plate.id, "plate", rt(0.0), rt(0.0), rt(5.0)).unwrap();
        seq = ops::insert_clip(&seq, v1, clip).unwrap();
        seq.frame_size = Some((64, 32));
        let pool = DecoderPool::new(
            bin,
            2,
            MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let session = SequencePlayback::spawn(PlaybackInputs {
            project,
            seq,
            from_seconds: 0.0,
            audio: None,
            render_state,
            target,
            provider: StreamProvider::new(pool, FeedPolicy::Realtime),
            res: ComposeResources::new(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
            ),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pid_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(session.stop());
        });
        let stopped = rx.recv_timeout(Duration::from_secs(20));
        let took = started.elapsed();
        if pid_file.exists() {
            // SAFETY: kills the test's own helper process (if still there).
            unsafe { libc::kill(read_pid(&pid_file), libc::SIGKILL) };
        }
        eprintln!("stop with a decoder stuck starting up took {took:?}");
        let done = stopped.expect("stop hung on the handshake").expect("stop");
        // The kill freed the thread: its own pool and frames came back, not
        // an abandoned session's fresh ones.
        assert!(took < STOP_GRACE + KILL_GRACE, "{took:?}");
        assert!(done.presented >= 1, "the session ran on after the kill");
    }

    #[cfg(unix)]
    #[test]
    fn a_session_stuck_outside_its_decoders_is_abandoned() {
        let renderer = match vance_render::Renderer::new(vance_render::RendererConfig::default()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("no GPU ({e}) — skipping");
                return;
            }
        };
        // A LUT on a "hung share": reading a fifo blocks until a writer
        // comes, in the session's own read, which no sidecar kill reaches.
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("hung.cube");
        let made = vance_ipc::process::status(std::process::Command::new("mkfifo").arg(&fifo));
        assert!(made.is_ok_and(|s| s.success()), "mkfifo");
        let card = MediaItem::new_title(
            "Card",
            rt(5.0),
            TitleParams {
                width: 64,
                height: 32,
                ..TitleParams::default()
            },
        );
        let mut project = Project::new("p");
        project.media.insert(card.id, Arc::new(card.clone()));
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let mut clip = Clip::new(card.id, "card", rt(0.0), rt(0.0), rt(5.0)).unwrap();
        let mut lut = vance_timeline::Effect::new("lut");
        lut.params.insert(
            "path".into(),
            vance_timeline::EffectValue::String(fifo.to_string_lossy().into()),
        );
        clip.effects.push(lut);
        seq = ops::insert_clip(&seq, v1, clip).unwrap();
        seq.frame_size = Some((64, 32));
        let addons: Arc<Mutex<crate::addons::AddonHost>> = Default::default();
        let res = ComposeResources::new(
            addons.clone(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let target = PlaybackTarget::Offscreen(headless_target(&renderer));
        let render_state = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        let ui_render_state = render_state.clone();
        let session = SequencePlayback::spawn(PlaybackInputs {
            project,
            seq,
            from_seconds: 0.0,
            audio: None,
            render_state,
            target,
            provider: StreamProvider::new(
                DecoderPool::new(
                    std::path::PathBuf::new(),
                    2,
                    MemoryBudget {
                        cap_bytes: u64::MAX,
                    },
                ),
                FeedPolicy::Realtime,
            ),
            res,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(session.stop());
        });
        let stopped = rx.recv_timeout(Duration::from_secs(20));
        let took = started.elapsed();
        // The LUT is read before the render lock: the stuck thread doesn't
        // hold the renderer the UI needs.
        let renderer_free = ui_render_state.try_lock().is_some();
        // Let the stuck read finish (EOF), so the abandoned thread ends.
        drop(std::fs::OpenOptions::new().write(true).open(&fifo));
        eprintln!("stop with the session stuck in a read took {took:?}");
        let done = stopped
            .expect("stop hung on the session's own read")
            .expect("stop");
        // A sanity bound only: by design stop takes STOP_GRACE + KILL_GRACE.
        assert!(
            took < STOP_GRACE + KILL_GRACE + Duration::from_secs(3),
            "{took:?}"
        );
        assert!(renderer_free, "the stuck read holds the render lock");
        // Fresh resources and pool, still sharing the engine's add-on host.
        assert!(Arc::ptr_eq(&done.res.addons, &addons));
        assert_eq!((done.presented, done.pool.spawn_count()), (0, 0));
    }

    #[test]
    fn stop_returns_the_proxy_reports_of_the_last_frames() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.proxy.mov");
        std::fs::write(&junk, b"not a video").unwrap();
        let Some(session) = over_budget_session(&junk) else {
            return;
        };
        let original = crate::decoder_pool::tests::fixture()
            .to_string_lossy()
            .into_owned();
        let junk = junk.to_string_lossy().into_owned();
        // Nothing was taken while it ran: it all comes back from `stop`.
        let done = session.stop().unwrap();
        if done.pool.shm_bytes() == 0 {
            eprintln!("shared memory off (VANCE_IPC_SHM=0?) — skipping");
            return;
        }
        assert_eq!(done.proxy_requests, [original]);
        assert_eq!(done.broken_proxies, [junk]);
    }

    #[test]
    fn proxy_reports_are_taken_together() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.proxy.mov");
        std::fs::write(&junk, b"not a video").unwrap();
        let Some(session) = over_budget_session(&junk) else {
            return;
        };
        let reports = session.take_proxy_reports();
        let done = session.stop().unwrap();
        if done.pool.shm_bytes() == 0 {
            eprintln!("shared memory off (VANCE_IPC_SHM=0?) — skipping");
            return;
        }
        let original = crate::decoder_pool::tests::fixture()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            reports,
            ProxyReports {
                requests: vec![original],
                broken_proxies: vec![junk.to_string_lossy().into_owned()],
            },
            "the broken proxy and its original's request in one take"
        );
        assert!(done.proxy_requests.is_empty() && done.broken_proxies.is_empty());
    }

    #[test]
    fn a_headless_session_plays_every_frame_to_the_end() {
        let renderer = match vance_render::Renderer::new(vance_render::RendererConfig::default()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("no GPU ({e}) — skipping");
                return;
            }
        };
        let texture = renderer.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("test.playback-target"),
            size: wgpu::Extent3d {
                width: 64,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let render_state = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        let card = MediaItem::new_title(
            "Card",
            rt(0.5),
            TitleParams {
                width: 64,
                height: 32,
                ..TitleParams::default()
            },
        );
        let mut project = Project::new("p");
        project.media.insert(card.id, Arc::new(card.clone()));
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let clip = Clip::new(card.id, "card", rt(0.0), rt(0.0), rt(0.5)).unwrap();
        seq = ops::insert_clip(&seq, v1, clip).unwrap();
        seq.frame_size = Some((64, 32));
        let pool = DecoderPool::new(
            std::path::PathBuf::new(),
            2,
            MemoryBudget {
                cap_bytes: u64::MAX,
            },
        );
        let res = ComposeResources::new(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        // The session's clock is the audio position, which this test sets:
        // a frame is due only when the test says so, so a slow runner can't
        // make it drop frames (no wall clock involved). In frames: pts k is
        // timeline time k / 24.
        let clock = Arc::new(AtomicI64::new(0));
        let session = SequencePlayback::spawn(PlaybackInputs {
            project,
            seq,
            from_seconds: 0.0,
            audio: Some(AudioSync {
                current_pts: clock.clone(),
                time_base: (1, 24),
            }),
            render_state,
            target: PlaybackTarget::Offscreen(texture),
            provider: StreamProvider::new(pool, FeedPolicy::Realtime),
            res,
        })
        .unwrap();
        // 0.5 s at 24 fps is frames 0..=11. Step through them: frame k is
        // due once the clock reads k; wait for it to be drawn, check it is
        // the one on screen, then move the clock on. (After frame 11 the
        // playhead moves straight to the end, so that frame is known by the
        // count alone.)
        let mut shown = Vec::new();
        for k in 0..12u64 {
            clock.store(k as i64, Ordering::Release);
            let deadline = Instant::now() + Duration::from_secs(30);
            while session.frames_presented() < k + 1 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(session.frames_presented(), k + 1, "frame {k} drawn");
            if k < 11 {
                shown.push((session.playhead_seconds() * 24.0).round() as u64);
            }
        }
        assert_eq!(shown, (0..11).collect::<Vec<_>>(), "every frame, in order");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !session.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            session.is_finished(),
            "the session ends at the sequence end"
        );
        let done = session.stop().unwrap();
        assert_eq!(done.playhead, 0.5, "the playhead rests at the end");
        assert_eq!((done.presented, done.dropped), (12, 0));
    }

    #[test]
    fn a_session_behind_the_clock_drops_frames_but_accounts_for_all() {
        let renderer = match vance_render::Renderer::new(vance_render::RendererConfig::default()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("no GPU ({e}) — skipping");
                return;
            }
        };
        let target = PlaybackTarget::Offscreen(headless_target(&renderer));
        let render_state = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        let card = MediaItem::new_title(
            "Card",
            rt(0.5),
            TitleParams {
                width: 64,
                height: 32,
                ..TitleParams::default()
            },
        );
        let mut project = Project::new("p");
        project.media.insert(card.id, Arc::new(card.clone()));
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let v1 = seq.tracks[0].id;
        let clip = Clip::new(card.id, "card", rt(0.0), rt(0.0), rt(0.5)).unwrap();
        seq = ops::insert_clip(&seq, v1, clip).unwrap();
        seq.frame_size = Some((64, 32));
        let clock = Arc::new(AtomicI64::new(0));
        let session = SequencePlayback::spawn(PlaybackInputs {
            project,
            seq,
            from_seconds: 0.0,
            audio: Some(AudioSync {
                current_pts: clock.clone(),
                time_base: (1, 24),
            }),
            render_state,
            target,
            provider: StreamProvider::new(
                DecoderPool::new(
                    std::path::PathBuf::new(),
                    2,
                    MemoryBudget {
                        cap_bytes: u64::MAX,
                    },
                ),
                FeedPolicy::Realtime,
            ),
            res: ComposeResources::new(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
            ),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while session.frames_presented() < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        // The clock jumps 5 frames ahead of the picture (frame 0): frames
        // 1..=4 are stale, so they are dropped, not drawn late, whether the
        // loop was already waiting for frame 1 or not.
        clock.store(5, Ordering::Release);
        while session.frames_presented() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.frames_presented(), 2);
        assert_eq!(
            (session.playhead_seconds() * 24.0).round(),
            5.0,
            "frame 5 next"
        );
        clock.store(12, Ordering::Release);
        while !session.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let done = session.stop().unwrap();
        assert_eq!(done.playhead, 0.5);
        assert_eq!(
            done.presented + done.dropped,
            12,
            "every frame drawn or dropped"
        );
        assert_eq!(
            done.dropped,
            4 + 6,
            "1..=4, then 6..=11 when the clock reached the end"
        );
    }
}
