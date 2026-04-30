//! Media library entries.
//!
//! `MediaItem` describes a source asset on disk. Clips reference a
//! `MediaItem` by ID; the clip carries the in/out range, the media item
//! carries everything intrinsic to the file (duration, frame rate, hash
//! for relink). Media items live on the `Project`, not the `Sequence`,
//! so the same source can be reused across many sequences.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use vance_core::{FrameRate, RationalTime};

use crate::id::MediaItemId;

/// What kind of asset this media item represents. Drives which tracks
/// can host clips that reference it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum MediaKind {
    /// Video-only or video+audio container; usable on video tracks.
    Video,
    /// Audio-only file; usable on audio tracks.
    Audio,
    /// Still image; usable on video tracks (treated as a held frame).
    Image,
}

/// A 32-byte content hash used for relink-by-content. We only require it
/// be filled in when the project is saved; in-memory items can leave it
/// `None` until the indexer computes it.
pub type ContentHash = [u8; 32];

/// One source asset registered in the project's media library.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MediaItem {
    /// Stable ID; survives save/load.
    pub id: MediaItemId,
    /// Path on disk. Stored as written by the user; the bundle
    /// serializer is responsible for any relativization.
    pub path: PathBuf,
    /// Human-readable name shown in the browser. Defaults to the file
    /// stem when not explicitly set.
    pub display_name: String,
    /// What kind of asset this is.
    pub kind: MediaKind,
    /// Total duration of the source. Required.
    pub duration: RationalTime,
    /// Source frame rate, when applicable (None for pure audio).
    pub frame_rate: Option<FrameRate>,
    /// BLAKE3 / SHA256 of the file contents, when computed. Used for
    /// relink-by-content if the file moves.
    pub content_hash: Option<ContentHash>,
}

impl MediaItem {
    /// Construct a new media item with a fresh ID.
    pub fn new(
        path: PathBuf,
        display_name: impl Into<String>,
        kind: MediaKind,
        duration: RationalTime,
    ) -> Self {
        Self {
            id: MediaItemId::new(),
            path,
            display_name: display_name.into(),
            kind,
            duration,
            frame_rate: None,
            content_hash: None,
        }
    }

    /// Builder-style: attach a frame rate (typically for video/image media).
    pub fn with_frame_rate(mut self, fr: FrameRate) -> Self {
        self.frame_rate = Some(fr);
        self
    }

    /// Builder-style: attach a precomputed content hash.
    pub fn with_content_hash(mut self, hash: ContentHash) -> Self {
        self.content_hash = Some(hash);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_video_item() {
        let m = MediaItem::new(
            PathBuf::from("/tmp/clip.mov"),
            "Take 1",
            MediaKind::Video,
            RationalTime::new(10, 1).unwrap(),
        )
        .with_frame_rate(FrameRate::FPS_24);
        assert_eq!(m.display_name, "Take 1");
        assert_eq!(m.kind, MediaKind::Video);
        assert_eq!(m.frame_rate, Some(FrameRate::FPS_24));
        assert!(m.content_hash.is_none());
    }
}
