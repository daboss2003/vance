//! Markers — colored points on the timeline carrying notes, in/out flags,
//! or section anchors. Sequence-level markers are stored on `Sequence`;
//! clip-level markers (added in 3.2 with edit ops) are clip-relative.

use serde::{Deserialize, Serialize};
use vance_core::RationalTime;

use crate::id::MarkerId;

/// What kind of action this marker represents. Mirrors common NLE
/// conventions (cue, in/out brackets, chapters).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum MarkerKind {
    /// A plain note or cue.
    Cue,
    /// Range start (paired with `RangeEnd` to mark in/out).
    RangeStart,
    /// Range end (paired with `RangeStart`).
    RangeEnd,
    /// Chapter boundary (export hint).
    Chapter,
}

/// One of a small set of editor-conventional marker colors. Stored as a
/// named variant rather than RGB so the renderer can pick a theme-aware
/// shade on each platform.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum MarkerColor {
    /// Default neutral marker.
    Default,
    /// Red.
    Red,
    /// Orange.
    Orange,
    /// Yellow.
    Yellow,
    /// Green.
    Green,
    /// Blue.
    Blue,
    /// Purple.
    Purple,
}

/// A single marker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// Stable ID.
    pub id: MarkerId,
    /// Position along whatever timeline this marker belongs to (sequence
    /// time for sequence markers, clip-source time for clip markers).
    pub time: RationalTime,
    /// User-supplied label.
    pub name: String,
    /// Kind of marker (cue, chapter, range bracket, ...).
    pub kind: MarkerKind,
    /// Display color.
    pub color: MarkerColor,
}

impl Marker {
    /// Create a plain cue marker with a fresh ID and the default color.
    pub fn cue(time: RationalTime, name: impl Into<String>) -> Self {
        Self {
            id: MarkerId::new(),
            time,
            name: name.into(),
            kind: MarkerKind::Cue,
            color: MarkerColor::Default,
        }
    }

    /// Builder-style: set the marker's color.
    pub fn with_color(mut self, color: MarkerColor) -> Self {
        self.color = color;
        self
    }

    /// Builder-style: set the marker's kind.
    pub fn with_kind(mut self, kind: MarkerKind) -> Self {
        self.kind = kind;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cue_constructs_with_defaults() {
        let m = Marker::cue(RationalTime::new(1, 1).unwrap(), "intro");
        assert_eq!(m.kind, MarkerKind::Cue);
        assert_eq!(m.color, MarkerColor::Default);
        assert_eq!(m.name, "intro");
    }
}
