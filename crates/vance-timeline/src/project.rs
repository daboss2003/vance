//! The root `Project` owns the media library and all sequences. It is
//! the unit of save/load handled by `vance-project` (Phase 3.4).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

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
}

impl Project {
    /// New empty project with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: ProjectId::new(),
            name: name.into(),
            media: BTreeMap::new(),
            sequences: Vec::new(),
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
}
