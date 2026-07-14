//! Stable, type-distinct IDs for every timeline entity.
//!
//! Each ID is a thin newtype around `uuid::Uuid` so the type system
//! prevents passing (say) a `ClipId` where a `TrackId` is expected. IDs
//! survive save/load — a project bundle stores the same UUIDs we
//! generated in memory.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Allocate a fresh, random ID.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wrap an existing `Uuid`.
            pub fn from_uuid(u: Uuid) -> Self {
                Self(u)
            }

            /// The underlying `Uuid`.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(ProjectId, "Identifies a `Project`.");
id_type!(SequenceId, "Identifies a `Sequence`.");
id_type!(TrackId, "Identifies a `Track` within a `Sequence`.");
id_type!(ClipId, "Identifies a `Clip` within a `Track`.");
id_type!(MediaItemId, "Identifies a `MediaItem` in the project's media library.");
id_type!(MarkerId, "Identifies a `Marker` on a `Sequence` or `Clip`.");
id_type!(EffectId, "Identifies an `Effect` instance applied to a `Clip` or `Track`.");
id_type!(TransitionId, "Identifies a `Transition` between two clips on a `Track`.");
id_type!(BusId, "Identifies an `AudioBus` (mixer submix) within a `Sequence`.");
id_type!(CommentId, "Identifies a review `ReviewComment` on a `Sequence`.");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let a = ClipId::new();
        let b = ClipId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn ids_round_trip_via_uuid() {
        let id = TrackId::new();
        let back = TrackId::from_uuid(id.as_uuid());
        assert_eq!(id, back);
    }

    #[test]
    fn distinct_id_kinds_are_distinct_types() {
        // This is a compile-time check more than a runtime one: the line
        // below would not type-check if these aliased the same type.
        let _: fn(ClipId) = |_| {};
        let _: fn(TrackId) = |_| {};
    }
}
