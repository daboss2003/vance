//! GOAT-3 — media query + smart bins (Avid-grade bin parity).
//!
//! A [`MediaQuery`] is a small boolean filter over a [`MediaItem`]'s
//! intrinsic fields (name, kind, duration, …) and its user-defined
//! "custom column" [`metadata`](MediaItem::metadata). A [`SmartBin`] pairs
//! a name with a query and auto-populates from the project's media library
//! — the "saved search" bins Avid/Premiere/Resolve editors live in.

use serde::{Deserialize, Serialize};

use crate::id::MediaItemId;
use crate::media::{MediaItem, MediaKind};

/// A field of a [`MediaItem`] a query can test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaField {
    /// `display_name`.
    Name,
    /// Kind as a lowercase string: `video` / `audio` / `image` / `title`.
    Kind,
    /// Source duration in seconds (numeric).
    DurationSeconds,
    /// Whether the source has an audio track (`true`/`false`).
    HasAudio,
    /// Whether a proxy has been generated (`true`/`false`).
    HasProxy,
    /// On-disk path (string).
    Path,
    /// A user-defined custom column, by key.
    Custom(String),
}

/// Comparison operator. String ops compare case-insensitively; numeric
/// ops parse both sides as `f64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryOp {
    /// Equal (string, case-insensitive).
    Eq,
    /// Not equal.
    Ne,
    /// Substring match.
    Contains,
    /// Prefix match.
    StartsWith,
    /// Suffix match.
    EndsWith,
    /// Numeric greater-than.
    Gt,
    /// Numeric less-than.
    Lt,
    /// Numeric greater-or-equal.
    Ge,
    /// Numeric less-or-equal.
    Le,
    /// Field is present + non-empty (the comparison `value` is ignored).
    Exists,
}

/// One leaf condition: `field op value`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    /// The field to test.
    pub field: MediaField,
    /// The operator.
    pub op: QueryOp,
    /// The right-hand operand (ignored for [`QueryOp::Exists`]).
    pub value: String,
}

/// A boolean filter tree over media items.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaQuery {
    /// Matches every item (the "all media" bin).
    All,
    /// A single leaf condition.
    Cond(Condition),
    /// Conjunction — all sub-queries must match (empty = matches all).
    And(Vec<MediaQuery>),
    /// Disjunction — any sub-query matches (empty = matches none).
    Or(Vec<MediaQuery>),
    /// Negation.
    Not(Box<MediaQuery>),
}

impl MediaQuery {
    /// Does `item` satisfy this query?
    pub fn matches(&self, item: &MediaItem) -> bool {
        match self {
            MediaQuery::All => true,
            MediaQuery::Cond(c) => eval_condition(c, item),
            MediaQuery::And(qs) => qs.iter().all(|q| q.matches(item)),
            MediaQuery::Or(qs) => qs.iter().any(|q| q.matches(item)),
            MediaQuery::Not(q) => !q.matches(item),
        }
    }
}

/// A saved search that auto-populates from the media library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartBin {
    /// Stable id.
    pub id: MediaItemId,
    /// Display name.
    pub name: String,
    /// The query that defines membership.
    pub query: MediaQuery,
}

impl SmartBin {
    /// New smart bin with a fresh id.
    pub fn new(name: impl Into<String>, query: MediaQuery) -> Self {
        Self {
            id: MediaItemId::new(),
            name: name.into(),
            query,
        }
    }

    /// The ids of every item in `library` that matches this bin's query,
    /// in input order.
    pub fn filter<'a>(
        &self,
        library: impl IntoIterator<Item = &'a MediaItem>,
    ) -> Vec<MediaItemId> {
        library
            .into_iter()
            .filter(|m| self.query.matches(m))
            .map(|m| m.id)
            .collect()
    }
}

/// Extract a field's string form (lowercased for case-insensitive
/// compares). `None` when the field is absent (e.g. a missing custom key).
fn field_string(field: &MediaField, item: &MediaItem) -> Option<String> {
    let s = match field {
        MediaField::Name => item.display_name.clone(),
        MediaField::Kind => kind_str(item.kind).to_string(),
        MediaField::DurationSeconds => item.duration.0.as_f64().to_string(),
        MediaField::HasAudio => item.has_audio_track.to_string(),
        MediaField::HasProxy => item.proxy_path.is_some().to_string(),
        MediaField::Path => item.path.to_string_lossy().into_owned(),
        MediaField::Custom(key) => item.metadata.get(key)?.clone(),
    };
    Some(s.to_lowercase())
}

/// Numeric form of a field, where it makes sense.
fn field_number(field: &MediaField, item: &MediaItem) -> Option<f64> {
    match field {
        MediaField::DurationSeconds => Some(item.duration.0.as_f64()),
        MediaField::Custom(key) => item.metadata.get(key)?.trim().parse::<f64>().ok(),
        // Other fields parse their string form if numeric (e.g. a rating
        // stored as a custom column already handled above).
        _ => field_string(field, item)?.trim().parse::<f64>().ok(),
    }
}

fn eval_condition(c: &Condition, item: &MediaItem) -> bool {
    if c.op == QueryOp::Exists {
        return field_string(&c.field, item)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
    }
    match c.op {
        QueryOp::Gt | QueryOp::Lt | QueryOp::Ge | QueryOp::Le => {
            let (Some(lhs), Ok(rhs)) = (field_number(&c.field, item), c.value.trim().parse::<f64>())
            else {
                return false;
            };
            match c.op {
                QueryOp::Gt => lhs > rhs,
                QueryOp::Lt => lhs < rhs,
                QueryOp::Ge => lhs >= rhs,
                QueryOp::Le => lhs <= rhs,
                _ => unreachable!(),
            }
        }
        _ => {
            let Some(lhs) = field_string(&c.field, item) else {
                // Missing field: only `Ne` is satisfied (it's != anything).
                return c.op == QueryOp::Ne;
            };
            let rhs = c.value.to_lowercase();
            match c.op {
                QueryOp::Eq => lhs == rhs,
                QueryOp::Ne => lhs != rhs,
                QueryOp::Contains => lhs.contains(&rhs),
                QueryOp::StartsWith => lhs.starts_with(&rhs),
                QueryOp::EndsWith => lhs.ends_with(&rhs),
                _ => unreachable!(),
            }
        }
    }
}

fn kind_str(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Image => "image",
        MediaKind::Title => "title",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use vance_core::RationalTime;

    fn item(name: &str, kind: MediaKind, secs: i64) -> MediaItem {
        MediaItem::new(
            PathBuf::from(format!("/m/{name}.mov")),
            name,
            kind,
            RationalTime::new(secs, 1).unwrap(),
        )
    }

    #[test]
    fn cond_name_contains_case_insensitive() {
        let m = item("Interview_A", MediaKind::Video, 10);
        let q = MediaQuery::Cond(Condition {
            field: MediaField::Name,
            op: QueryOp::Contains,
            value: "interview".into(),
        });
        assert!(q.matches(&m));
    }

    #[test]
    fn cond_kind_and_duration() {
        let v = item("a", MediaKind::Video, 30);
        let a = item("b", MediaKind::Audio, 5);
        let long_video = MediaQuery::And(vec![
            MediaQuery::Cond(Condition {
                field: MediaField::Kind,
                op: QueryOp::Eq,
                value: "video".into(),
            }),
            MediaQuery::Cond(Condition {
                field: MediaField::DurationSeconds,
                op: QueryOp::Gt,
                value: "20".into(),
            }),
        ]);
        assert!(long_video.matches(&v));
        assert!(!long_video.matches(&a));
    }

    #[test]
    fn custom_column_query_and_numeric_rating() {
        let mut m = item("take3", MediaKind::Video, 12);
        m.metadata.insert("Scene".into(), "12B".into());
        m.metadata.insert("Rating".into(), "4".into());

        let scene = MediaQuery::Cond(Condition {
            field: MediaField::Custom("Scene".into()),
            op: QueryOp::Eq,
            value: "12b".into(),
        });
        assert!(scene.matches(&m));

        let rating_ge_3 = MediaQuery::Cond(Condition {
            field: MediaField::Custom("Rating".into()),
            op: QueryOp::Ge,
            value: "3".into(),
        });
        assert!(rating_ge_3.matches(&m));

        // Missing custom field: Exists false; Eq false; Ne true.
        let no_camera = MediaQuery::Cond(Condition {
            field: MediaField::Custom("Camera".into()),
            op: QueryOp::Exists,
            value: String::new(),
        });
        assert!(!no_camera.matches(&m));
    }

    #[test]
    fn not_and_or_compose() {
        let m = item("b_roll", MediaKind::Video, 8);
        let q = MediaQuery::And(vec![
            MediaQuery::Not(Box::new(MediaQuery::Cond(Condition {
                field: MediaField::Kind,
                op: QueryOp::Eq,
                value: "audio".into(),
            }))),
            MediaQuery::Or(vec![
                MediaQuery::Cond(Condition {
                    field: MediaField::Name,
                    op: QueryOp::StartsWith,
                    value: "b_".into(),
                }),
                MediaQuery::Cond(Condition {
                    field: MediaField::Name,
                    op: QueryOp::Contains,
                    value: "hero".into(),
                }),
            ]),
        ]);
        assert!(q.matches(&m));
    }

    #[test]
    fn smart_bin_filters_library() {
        let mut a = item("scene12_take1", MediaKind::Video, 10);
        a.metadata.insert("Rating".into(), "5".into());
        let mut b = item("scene12_take2", MediaKind::Video, 10);
        b.metadata.insert("Rating".into(), "2".into());
        let c = item("music", MediaKind::Audio, 120);

        let selects = SmartBin::new(
            "Selects",
            MediaQuery::Cond(Condition {
                field: MediaField::Custom("Rating".into()),
                op: QueryOp::Ge,
                value: "4".into(),
            }),
        );
        let lib = [&a, &b, &c];
        let hits = selects.filter(lib);
        assert_eq!(hits, vec![a.id]);
    }

    #[test]
    fn query_round_trips_through_serde() {
        let q = MediaQuery::And(vec![
            MediaQuery::Cond(Condition {
                field: MediaField::Custom("Scene".into()),
                op: QueryOp::Eq,
                value: "12B".into(),
            }),
            MediaQuery::Not(Box::new(MediaQuery::Cond(Condition {
                field: MediaField::HasProxy,
                op: QueryOp::Eq,
                value: "true".into(),
            }))),
        ]);
        let s = serde_json::to_string(&q).unwrap();
        let back: MediaQuery = serde_json::from_str(&s).unwrap();
        assert_eq!(q, back);
    }
}
