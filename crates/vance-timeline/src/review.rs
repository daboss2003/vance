//! GOAT-gated (collaboration) — timeline review comments.
//!
//! A lightweight, FULLY-LOCAL review layer: comments anchored to a timeline
//! position, each with an author, timestamp, resolved state, and a reply
//! thread. Comments live on the [`crate::Sequence`] (like markers/captions),
//! so they travel inside the `.vance` project bundle — sharing the bundle IS
//! the review package. Real-time cloud sync / presence is a separate,
//! server-gated layer; the local model here is complete on its own.

use serde::{Deserialize, Serialize};

use crate::id::CommentId;

/// One reply in a comment thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReply {
    /// Who wrote it.
    pub author: String,
    /// The reply body.
    pub text: String,
    /// Creation time, Unix seconds (UTC).
    pub created_unix: i64,
}

/// A review note anchored to a timeline position, with an optional thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// Stable id.
    pub id: CommentId,
    /// Timeline position in SEQUENCE milliseconds the note refers to.
    pub timeline_ms: i64,
    /// Comment author.
    pub author: String,
    /// The note body.
    pub text: String,
    /// Creation time, Unix seconds (UTC).
    pub created_unix: i64,
    /// Whether the note has been addressed.
    #[serde(default)]
    pub resolved: bool,
    /// Threaded replies, oldest first.
    #[serde(default)]
    pub replies: Vec<ReviewReply>,
}

impl ReviewComment {
    /// A fresh unresolved comment with no replies.
    pub fn new(
        timeline_ms: i64,
        author: impl Into<String>,
        text: impl Into<String>,
        created_unix: i64,
    ) -> Self {
        ReviewComment {
            id: CommentId::new(),
            timeline_ms: timeline_ms.max(0),
            author: author.into(),
            text: text.into(),
            created_unix,
            resolved: false,
            replies: Vec::new(),
        }
    }
}

/// Insert `comment` into `comments`, keeping the list sorted by timeline
/// position (stable for equal positions). Returns the new comment's id.
pub fn insert_sorted(comments: &mut Vec<ReviewComment>, comment: ReviewComment) -> CommentId {
    let id = comment.id;
    let pos = comments
        .iter()
        .position(|c| c.timeline_ms > comment.timeline_ms)
        .unwrap_or(comments.len());
    comments.insert(pos, comment);
    id
}

/// Number of unresolved comments — the review "open notes" badge count.
pub fn open_count(comments: &[ReviewComment]) -> usize {
    comments.iter().filter(|c| !c.resolved).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_keeps_sorted_by_position() {
        let mut v = Vec::new();
        insert_sorted(&mut v, ReviewComment::new(3000, "a", "third", 100));
        insert_sorted(&mut v, ReviewComment::new(1000, "a", "first", 100));
        insert_sorted(&mut v, ReviewComment::new(2000, "a", "second", 100));
        let ms: Vec<i64> = v.iter().map(|c| c.timeline_ms).collect();
        assert_eq!(ms, vec![1000, 2000, 3000]);
    }

    #[test]
    fn open_count_ignores_resolved() {
        let mut v = vec![
            ReviewComment::new(0, "a", "x", 1),
            ReviewComment::new(1, "a", "y", 1),
        ];
        assert_eq!(open_count(&v), 2);
        v[0].resolved = true;
        assert_eq!(open_count(&v), 1);
    }

    #[test]
    fn negative_position_clamps_to_zero() {
        assert_eq!(ReviewComment::new(-500, "a", "x", 1).timeline_ms, 0);
    }

    #[test]
    fn round_trips_through_json() {
        let mut c = ReviewComment::new(1500, "sam", "tighten this cut", 1_700_000_000);
        c.replies.push(ReviewReply {
            author: "lee".into(),
            text: "agreed".into(),
            created_unix: 1_700_000_100,
        });
        let j = serde_json::to_string(&c).unwrap();
        let back: ReviewComment = serde_json::from_str(&j).unwrap();
        assert_eq!(c, back);
    }
}
