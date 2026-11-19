//! A `Sequence` is the editable timeline. It owns ordered tracks plus
//! sequence-level markers, and carries its own frame rate (which need
//! not match any source media's rate).

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use vance_core::{FrameRate, RationalTime};

use crate::id::SequenceId;
use crate::marker::Marker;
use crate::track::Track;

/// A timeline.
///
/// `tracks` holds video tracks first, then audio tracks. The order
/// within each kind matters for compositing (video) and channel
/// assignment (audio). `Arc<[Arc<Track>]>` lets edit operations share
/// untouched tracks across versions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    /// Stable ID.
    pub id: SequenceId,
    /// Display name (e.g. "Sequence 01").
    pub name: String,
    /// The sequence's master frame rate. Source media at other rates is
    /// resampled at render time.
    pub frame_rate: FrameRate,
    /// Tracks, in render order. Convention: video tracks first (V1, V2,
    /// ...), then audio (A1, A2, ...). Edit operations preserve this
    /// grouping.
    pub tracks: Vec<Arc<Track>>,
    /// Sequence-level markers (chapters, notes, range brackets).
    pub markers: Vec<Marker>,
    /// Phase 7.4: master bus gain in decibels. 0.0 = unity. Applied
    /// uniformly to every audio source after track / clip gain
    /// composition. `#[serde(default)]` so pre-7.4 bundles deserialize
    /// at unity. The mixer snapshot precomputes the linear form into
    /// `MixerSnapshot.master_gain_linear` at build time.
    #[serde(default)]
    pub master_gain_db: f32,
    /// GOAT-4: mixer submix buses (Fairlight-class console routing). Tracks
    /// and buses route their main output to a bus or the master, and tap
    /// aux sends to buses. `#[serde(default)]` so older bundles open with
    /// no buses (every track straight to master, the Phase-7 behaviour).
    #[serde(default)]
    pub buses: Vec<crate::bus::AudioBus>,
    /// GOAT-6: clip connections (FCPX-style) — `connected` clips follow
    /// their `anchor` under magnetic moves/ripples. `#[serde(default)]` so
    /// older bundles open with none.
    #[serde(default)]
    pub connections: Vec<crate::connection::Connection>,
    /// GOAT-6: magnetic-timeline mode. When `true`, the editor's default
    /// edits ripple across all tracks and keep connected clips locked to
    /// their anchors. `#[serde(default)]` = off (classic per-track model).
    #[serde(default)]
    pub magnetic: bool,
    /// GOAT-7: subtitle / caption cues (sorted by start), in sequence
    /// milliseconds. Exported as a sidecar .srt/.vtt and/or burned in.
    /// `#[serde(default)]` so older bundles open with none.
    #[serde(default)]
    pub captions: Vec<vance_captions::Cue>,
    /// GOAT-gated (collaboration): review comments anchored to timeline
    /// positions (sorted by position), with author / timestamp / resolve /
    /// replies. Travel in the project bundle. `#[serde(default)]` so older
    /// bundles open with none.
    #[serde(default)]
    pub review_comments: Vec<crate::review::ReviewComment>,
}

impl Sequence {
    /// New empty sequence at the given frame rate.
    pub fn new(name: impl Into<String>, frame_rate: FrameRate) -> Self {
        Self {
            id: SequenceId::new(),
            name: name.into(),
            frame_rate,
            tracks: Vec::new(),
            markers: Vec::new(),
            master_gain_db: 0.0,
            buses: Vec::new(),
            connections: Vec::new(),
            magnetic: false,
            captions: Vec::new(),
            review_comments: Vec::new(),
        }
    }

    /// Total duration of the sequence — the largest `track.duration()`
    /// over all tracks, or `ZERO` if empty.
    pub fn duration(&self) -> RationalTime {
        self.tracks
            .iter()
            .map(|t| t.duration())
            .max()
            .unwrap_or(RationalTime::ZERO)
    }

    /// Total clip count across all tracks. O(tracks).
    pub fn clip_count(&self) -> usize {
        self.tracks.iter().map(|t| t.clips.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use crate::id::{MediaItemId, TrackId};
    use crate::track::{Track, TrackKind};

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    #[test]
    fn empty_sequence_zero_duration() {
        let s = Sequence::new("Seq 01", FrameRate::FPS_24);
        assert_eq!(s.duration(), RationalTime::ZERO);
        assert_eq!(s.clip_count(), 0);
    }

    #[test]
    fn connections_and_magnetic_round_trip_through_ron() {
        // GOAT-6 — the new Sequence fields must survive the .vance bundle's
        // RON path (and old bundles open via serde-default).
        let mut s = Sequence::new("Seq 01", FrameRate::FPS_24);
        s.magnetic = true;
        s.connections.push(crate::connection::Connection::new(
            crate::id::ClipId::new(),
            crate::id::ClipId::new(),
        ));
        // GOAT-7: captions ride the same RON path.
        s.captions
            .push(vance_captions::Cue::new(1000, 3000, "hello"));
        let ron = ron::ser::to_string_pretty(&s, ron::ser::PrettyConfig::default()).unwrap();
        let back: Sequence = ron::from_str(&ron).unwrap();
        assert_eq!(
            s, back,
            "connections + magnetic + captions survive RON round-trip"
        );
        assert!(back.magnetic);
        assert_eq!(back.connections.len(), 1);
        assert_eq!(back.captions.len(), 1);
        assert_eq!(back.captions[0].text, "hello");
    }

    #[test]
    fn duration_is_longest_track() {
        let media = MediaItemId::new();
        let c_short = Arc::new(Clip::new(media, "s", t(0), t(0), t(3)).unwrap());
        let c_long = Arc::new(Clip::new(media, "l", t(0), t(0), t(7)).unwrap());
        let v1 = Arc::new(Track {
            id: TrackId::new(),
            kind: TrackKind::Video,
            name: "V1".into(),
            enabled: true,
            locked: false,
            clips: vec![c_short],
            transitions: Vec::new(),
            mute: false,
            solo: false,
            gain_db: 0.0,
            gain_keyframes: Vec::new(),
            pan: 0.0,
            audio_effects: Vec::new(),
            output: crate::bus::BusOutput::Master,
            sends: Vec::new(),
        });
        let v2 = Arc::new(Track {
            id: TrackId::new(),
            kind: TrackKind::Video,
            name: "V2".into(),
            enabled: true,
            locked: false,
            clips: vec![c_long],
            transitions: Vec::new(),
            mute: false,
            solo: false,
            gain_db: 0.0,
            gain_keyframes: Vec::new(),
            pan: 0.0,
            audio_effects: Vec::new(),
            output: crate::bus::BusOutput::Master,
            sends: Vec::new(),
        });
        let seq = Sequence {
            id: SequenceId::new(),
            name: "Seq".into(),
            frame_rate: FrameRate::FPS_24,
            tracks: vec![v1, v2],
            markers: Vec::new(),
            master_gain_db: 0.0,
            buses: Vec::new(),
            connections: Vec::new(),
            magnetic: false,
            captions: Vec::new(),
            review_comments: Vec::new(),
        };
        assert_eq!(seq.duration(), t(7));
        assert_eq!(seq.clip_count(), 2);
    }
}
