//! The root `Project` owns the media library and all sequences. It is
//! the unit of save/load handled by `vance-project` (Phase 3.4).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use vance_color::ColorSpace;

use crate::id::{MediaItemId, ProjectId, SequenceId};
use crate::media::MediaItem;
use crate::sequence::Sequence;

/// A whole project — media library + sequences.
///
/// The media library is keyed by `MediaItemId` so clip→media references
/// stay valid after rename/move. Sequences are stored in insertion order
/// so the UI can preserve the user's tab arrangement; `BTreeMap` would
/// reorder them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Stable ID.
    pub id: ProjectId,
    /// Human-readable project name.
    pub name: String,
    /// All registered media. Keyed by ID for O(log n) clip→media lookups.
    pub media: BTreeMap<MediaItemId, Arc<MediaItem>>,
    /// All sequences in insertion order.
    pub sequences: Vec<Arc<Sequence>>,
    /// Working color space — what color grading + render compositing
    /// happens in. v1 default is `LinearRec709` (matches the existing
    /// pre-Phase-6 path). `#[serde(default)]` so older projects open.
    #[serde(default = "default_working_space")]
    pub working_color_space: ColorSpace,
    /// Display LUT (Phase 6.5). Path to a .cube / .3dl applied *only*
    /// to the viewport for monitoring — export bypasses it.
    /// `None` for an unfiltered preview.
    #[serde(default)]
    pub view_lut_path: Option<std::path::PathBuf>,
    /// OpenColorIO display transform (Phase 6.1b). When set, the engine
    /// bakes a `src → dst` 3D LUT via OCIO and drives the SAME viewport
    /// display-LUT slot as [`Self::view_lut_path`] — so the OCIO
    /// transform takes precedence over a plain file LUT when both are
    /// present. `None` leaves the file LUT (or nothing) in charge.
    #[serde(default)]
    pub ocio_view: Option<OcioView>,
}

/// A persisted OpenColorIO display-transform spec (Phase 6.1b). Plain
/// data only — the actual conversion lives in the `vance-ocio` crate so
/// `vance-timeline` stays free of the native dependency.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OcioView {
    /// Path to the `.ocio` config, or `None` to use the `$OCIO` env config.
    #[serde(default)]
    pub config_path: Option<std::path::PathBuf>,
    /// Source colorspace / role name.
    pub src: String,
    /// Destination (display) colorspace / role name.
    pub dst: String,
    /// Baked cube side length. 33 is the usual quality/size sweet spot.
    #[serde(default = "default_ocio_size")]
    pub size: u32,
}

fn default_ocio_size() -> u32 {
    33
}

fn default_working_space() -> ColorSpace {
    ColorSpace::LinearRec709
}

impl Project {
    /// New empty project with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: ProjectId::new(),
            name: name.into(),
            media: BTreeMap::new(),
            sequences: Vec::new(),
            working_color_space: default_working_space(),
            view_lut_path: None,
            ocio_view: None,
        }
    }

    /// Look up a media item by ID, if registered.
    pub fn media_get(&self, id: MediaItemId) -> Option<&Arc<MediaItem>> {
        self.media.get(&id)
    }

    /// Find a sequence by ID. Linear over `sequences`; this is fine while
    /// projects have <~100 sequences. We can add a side-index later if a
    /// project genuinely grows past that.
    pub fn sequence(&self, id: SequenceId) -> Option<&Arc<Sequence>> {
        self.sequences.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{MediaItem, MediaKind};
    use std::path::PathBuf;
    use vance_core::{FrameRate, RationalTime};

    #[test]
    fn empty_project() {
        let p = Project::new("My Movie");
        assert_eq!(p.name, "My Movie");
        assert!(p.media.is_empty());
        assert!(p.sequences.is_empty());
    }

    #[test]
    fn register_media_and_lookup() {
        let mut p = Project::new("p");
        let m = MediaItem::new(
            PathBuf::from("/tmp/a.mov"),
            "a",
            MediaKind::Video,
            RationalTime::new(5, 1).unwrap(),
        );
        let id = m.id;
        p.media.insert(id, Arc::new(m));
        assert!(p.media_get(id).is_some());
        assert_eq!(p.media_get(id).unwrap().display_name, "a");
    }

    #[test]
    fn add_sequence_and_lookup() {
        let mut p = Project::new("p");
        let s = Sequence::new("Seq 01", FrameRate::FPS_24);
        let id = s.id;
        p.sequences.push(Arc::new(s));
        assert!(p.sequence(id).is_some());
        assert_eq!(p.sequence(id).unwrap().name, "Seq 01");
    }

    #[test]
    fn ocio_view_round_trips() {
        let mut p = Project::new("p");
        p.ocio_view = Some(OcioView {
            config_path: Some(PathBuf::from("/cfg/studio.ocio")),
            src: "ACEScg".into(),
            dst: "sRGB - Display".into(),
            size: 33,
        });
        let json = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ocio_view, p.ocio_view);
    }

    #[test]
    fn project_without_ocio_view_deserializes_to_none() {
        // Back-compat: a project serialized before 6.1b has no
        // `ocio_view` key. `#[serde(default)]` must fill in `None`.
        let p = Project::new("legacy");
        let mut value: serde_json::Value = serde_json::to_value(&p).unwrap();
        value.as_object_mut().unwrap().remove("ocio_view");
        let back: Project = serde_json::from_value(value).unwrap();
        assert!(back.ocio_view.is_none());
    }
}
