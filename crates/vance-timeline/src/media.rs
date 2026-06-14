//! Media library entries.
//!
//! `MediaItem` describes a source asset on disk. Clips reference a
//! `MediaItem` by ID; the clip carries the in/out range, the media item
//! carries everything intrinsic to the file (duration, frame rate, hash
//! for relink). Media items live on the `Project`, not the `Sequence`,
//! so the same source can be reused across many sequences.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use vance_color::ColorSpace;
use vance_core::{FrameRate, RationalTime};

use crate::id::MediaItemId;
use crate::title::TitleParams;

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
    /// Synthetic title — parameter-driven, no file backing. Its
    /// `path` is empty; the engine reads `title` for rasterization.
    Title,
    /// GOAT-5 — synthetic Motion Graphics Template instance. No file
    /// backing; the engine reads `mograph` and rasterizes it via
    /// `vance-mograph` at the clip-relative time (animated).
    MoGraph,
}

/// A 32-byte content hash used for relink-by-content. We only require it
/// be filled in when the project is saved; in-memory items can leave it
/// `None` until the indexer computes it.
pub type ContentHash = [u8; 32];

/// One source asset registered in the project's media library.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaItem {
    /// Stable ID; survives save/load.
    pub id: MediaItemId,
    /// Path on disk. Stored as written by the user; the bundle
    /// serializer is responsible for any relativization. Empty for
    /// synthetic media (titles).
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
    /// Parameters for synthetic titles. Set when `kind == Title`,
    /// `None` otherwise. `#[serde(default)]` so older projects round-
    /// trip without this field.
    #[serde(default)]
    pub title: Option<TitleParams>,
    /// GOAT-5 — the placed Motion Graphics Template instance. Set when
    /// `kind == MoGraph`, `None` otherwise. `#[serde(default)]` so older
    /// projects round-trip without this field.
    #[serde(default)]
    pub mograph: Option<vance_mograph::MographInstance>,
    /// What color space the decoded samples live in (Phase 6.2). On
    /// import we leave this `None`; the engine then falls back to
    /// `ColorSpace::Rec709Limited` — the existing pre-color-pipeline
    /// behavior. The UI lets the user override per item once they
    /// know better (camera raw, P3 capture, etc.).
    #[serde(default)]
    pub input_color_space: Option<ColorSpace>,
    /// Whether the source has an audio stream the peaks pipeline can
    /// extract (Phase 7.2). For `MediaKind::Audio` this is always
    /// `true` (forced by the bundle-load sweep in
    /// `vance_project::bundle::open` for legacy bundles); for `Video`
    /// it is set by `Engine::import_media_file`'s audio probe; for
    /// `Image` / `Title` it stays `false`. `#[serde(default)]` so
    /// older `.vance/` bundles deserialize with `false` and get
    /// backfilled by `peaks_backfill_video_audio` (in vance-app's
    /// Tauri command surface) on first project-open.
    #[serde(default)]
    pub has_audio_track: bool,
    /// Phase 9.5 — when set, points at a transcoded low-bandwidth
    /// proxy of this media. The engine reads frames from this path
    /// instead of `path` whenever the project's "use proxy" toggle
    /// is on. `None` = no proxy generated yet.
    #[serde(default)]
    pub proxy_path: Option<PathBuf>,
    /// GOAT-3 — user-defined "custom column" metadata (Avid-grade bins):
    /// arbitrary `key → value` strings (Scene, Take, Rating, Camera,
    /// Reviewed, …) that the browser shows as columns and smart bins
    /// query over. `#[serde(default)]` so older projects round-trip.
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
}

impl MediaItem {
    /// Construct a new media item with a fresh ID.
    pub fn new(
        path: PathBuf,
        display_name: impl Into<String>,
        kind: MediaKind,
        duration: RationalTime,
    ) -> Self {
        // Audio-kind items always carry an audio track; everything
        // else starts `false` until the import probe (or backfill)
        // confirms otherwise.
        let has_audio_track = matches!(kind, MediaKind::Audio);
        Self {
            id: MediaItemId::new(),
            path,
            display_name: display_name.into(),
            kind,
            duration,
            frame_rate: None,
            content_hash: None,
            title: None,
            mograph: None,
            input_color_space: None,
            has_audio_track,
            proxy_path: None,
            metadata: std::collections::BTreeMap::new(),
        }
    }

    /// Construct a synthetic title media item. The library entry has
    /// no file backing — the engine renders frames from `params`
    /// directly. Title media defaults to 24 fps for chain timing.
    pub fn new_title(
        display_name: impl Into<String>,
        duration: RationalTime,
        params: TitleParams,
    ) -> Self {
        Self {
            id: MediaItemId::new(),
            path: PathBuf::new(),
            display_name: display_name.into(),
            kind: MediaKind::Title,
            duration,
            frame_rate: Some(vance_core::FrameRate::FPS_24),
            content_hash: None,
            title: Some(params),
            mograph: None,
            // Titles are rasterized in sRGB-ish space (see
            // `vance-render::title`). Tag accordingly so they round-
            // trip through the pipeline correctly when the working
            // space differs.
            input_color_space: Some(ColorSpace::Srgb),
            has_audio_track: false,
            proxy_path: None,
            metadata: std::collections::BTreeMap::new(),
        }
    }

    /// GOAT-5 — construct a synthetic Motion Graphics Template item. Like
    /// a title, it has no file backing; the engine rasterizes `instance`
    /// at the clip-relative time via `vance-mograph`. Authored in sRGB.
    pub fn new_mograph(
        display_name: impl Into<String>,
        duration: RationalTime,
        instance: vance_mograph::MographInstance,
    ) -> Self {
        Self {
            id: MediaItemId::new(),
            path: PathBuf::new(),
            display_name: display_name.into(),
            kind: MediaKind::MoGraph,
            duration,
            frame_rate: Some(vance_core::FrameRate::FPS_24),
            content_hash: None,
            title: None,
            mograph: Some(instance),
            input_color_space: Some(ColorSpace::Srgb),
            has_audio_track: false,
            proxy_path: None,
            metadata: std::collections::BTreeMap::new(),
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

    #[test]
    fn mograph_media_round_trips_through_ron() {
        // GOAT-5 — a placed MoGraph instance (template + control values)
        // embeds Prop/ControlValue enums; it MUST survive the RON path the
        // .vance bundle uses, or a project with a title becomes unopenable
        // (the EffectValue-class data-loss bug we already fixed once).
        let mut inst = vance_mograph::MographInstance::new(vance_mograph::builtins::lower_third());
        inst.set("title", vance_mograph::ControlValue::Text("Ada Lovelace".into()));
        inst.set("accent", vance_mograph::ControlValue::Color([0.1, 0.4, 0.9, 1.0]));
        let item = MediaItem::new_mograph("Lower Third", RationalTime::new(6, 1).unwrap(), inst);
        assert_eq!(item.kind, MediaKind::MoGraph);

        let ron = ron::ser::to_string_pretty(&item, ron::ser::PrettyConfig::default())
            .expect("serialize mograph media to RON");
        let back: MediaItem = ron::from_str(&ron).expect("parse mograph media from RON");
        assert_eq!(item, back, "mograph media must survive RON round-trip");
        // The control override specifically must persist.
        let mg = back.mograph.expect("mograph present");
        assert_eq!(
            mg.value("title"),
            Some(vance_mograph::ControlValue::Text("Ada Lovelace".into()))
        );
    }
}
