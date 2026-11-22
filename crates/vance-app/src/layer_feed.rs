//! The two decoded-frame providers behind [`compose_frame`] (ADR 0007).
//!
//! - [`ScrubProvider`]: pooled `decode_at` plus the frame cache, fanned
//!   out across layers.
//! - [`StreamProvider`]: per-clip streaming feeds — `Realtime` (playback)
//!   may drop or hold frames, `Blocking` (export) never drops.
//!
//! All of them pick frames by the same rule (see
//! [`crate::compose::FrameProvider`]) and round with
//! [`crate::decoder_pool::source_seconds_to_pts`].
//!
//! [`compose_frame`]: crate::compose::compose_frame

use std::sync::Arc;

use vance_cache::FrameCache;
use vance_decoder::FrameData;

use crate::compose::{FrameProvider, FrameRequest};
use crate::decoder_pool::DecoderPool;

/// Scrub / single-frame provider: pooled `decode_at` + the frame cache.
pub struct ScrubProvider<'a> {
    /// Decoders to borrow from.
    pub pool: &'a mut DecoderPool,
    /// Shared frame cache.
    pub cache: Arc<FrameCache>,
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
pub struct StreamProvider {
    pool: DecoderPool,
    // Intentional: read by the feeds the sources seam (S) adds.
    #[allow(dead_code)]
    policy: FeedPolicy,
}

impl StreamProvider {
    /// A provider drawing decoders from `pool`.
    pub fn new(pool: DecoderPool, policy: FeedPolicy) -> Self {
        Self { pool, policy }
    }

    /// Stop every feed and hand the pool back.
    pub fn into_pool(self) -> DecoderPool {
        self.pool
    }
}

// Intentional: both providers return no frames (every layer skipped) until
// the sources seam (S) lands the decode paths.
impl FrameProvider for ScrubProvider<'_> {
    fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
        reqs.iter().map(|_| None).collect()
    }
}

impl FrameProvider for StreamProvider {
    fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
        reqs.iter().map(|_| None).collect()
    }
}
