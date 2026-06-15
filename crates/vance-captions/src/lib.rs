//! `vance-captions` — captions / subtitles for Vance.
//!
//! A [`Cue`] is one timed line of text (`[start_ms, end_ms)` + body). A
//! sequence carries an ordered list of cues (kept sorted by start). This
//! crate owns the data model, the SRT + WebVTT codecs (import / export),
//! and pure cue operations (shift / split / merge / sort). Auto-
//! transcription (whisper) lives in the app layer and feeds cues in via
//! these types.

#![warn(missing_docs)]

pub mod ops;
pub mod srt;
pub mod vtt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A stable cue id.
pub type CueId = Uuid;

/// One subtitle/caption: text shown over `[start_ms, end_ms)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    /// Stable id (for editing / selection).
    pub id: CueId,
    /// Inclusive start, milliseconds from sequence zero.
    pub start_ms: i64,
    /// Exclusive end, milliseconds.
    pub end_ms: i64,
    /// Cue body. May contain `\n` for a forced two-line cue.
    pub text: String,
    /// Optional speaker label (e.g. from diarization); empty = none.
    #[serde(default)]
    pub speaker: String,
}

impl Cue {
    /// A new cue with a fresh id. `end_ms` is clamped to `>= start_ms`.
    pub fn new(start_ms: i64, end_ms: i64, text: impl Into<String>) -> Self {
        Cue {
            id: Uuid::new_v4(),
            start_ms: start_ms.max(0),
            end_ms: end_ms.max(start_ms),
            text: text.into(),
            speaker: String::new(),
        }
    }

    /// Duration in milliseconds (never negative).
    pub fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }

    /// Whether `t_ms` falls within `[start_ms, end_ms)`.
    pub fn contains(&self, t_ms: i64) -> bool {
        t_ms >= self.start_ms && t_ms < self.end_ms
    }
}

/// Errors from caption codecs.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CaptionError {
    /// A timestamp couldn't be parsed.
    #[error("bad timestamp at line {line}: {text:?}")]
    BadTimestamp {
        /// 1-based source line.
        line: usize,
        /// The offending text.
        text: String,
    },
    /// Structurally invalid document.
    #[error("malformed {format} at line {line}: {msg}")]
    Malformed {
        /// "SRT" / "WebVTT".
        format: &'static str,
        /// 1-based source line.
        line: usize,
        /// What went wrong.
        msg: String,
    },
}

/// Format a millisecond timestamp as `HH:MM:SS,mmm` (SRT) when `comma` is
/// true, else `HH:MM:SS.mmm` (WebVTT). Negative inputs clamp to zero.
pub fn format_timestamp(ms: i64, comma: bool) -> String {
    let ms = ms.max(0);
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let milli = ms % 1000;
    let sep = if comma { ',' } else { '.' };
    format!("{h:02}:{m:02}:{s:02}{sep}{milli:03}")
}

/// Parse an `HH:MM:SS[,.]mmm` (or `MM:SS.mmm`) timestamp to milliseconds.
/// Accepts either `,` or `.` as the millisecond separator.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let (hms, milli) = if let Some((a, b)) = s.split_once([',', '.']) {
        (a, b)
    } else {
        (s, "0")
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        [m, s] => (0, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        _ => return None,
    };
    // Pad/truncate the fractional part to exactly 3 digits.
    let mut frac = milli.trim().to_string();
    frac.truncate(3);
    while frac.len() < 3 {
        frac.push('0');
    }
    let milli: i64 = frac.parse().ok()?;
    if m >= 60 || sec >= 60 {
        return None;
    }
    Some(((h * 3600 + m * 60 + sec) * 1000) + milli)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trips_srt_and_vtt() {
        let ms = 3_661_234; // 01:01:01.234
        assert_eq!(format_timestamp(ms, true), "01:01:01,234");
        assert_eq!(format_timestamp(ms, false), "01:01:01.234");
        assert_eq!(parse_timestamp("01:01:01,234"), Some(ms));
        assert_eq!(parse_timestamp("01:01:01.234"), Some(ms));
    }

    #[test]
    fn parse_accepts_mm_ss_and_pads_fraction() {
        assert_eq!(parse_timestamp("00:05.5"), Some(5500));
        assert_eq!(parse_timestamp("1:02:03"), Some(3_723_000));
        assert_eq!(parse_timestamp("bad"), None);
        assert_eq!(parse_timestamp("00:70:00,000"), None, "minutes >= 60 rejected");
    }

    #[test]
    fn cue_clamps_and_measures() {
        let c = Cue::new(1000, 500, "hi"); // end < start → clamped
        assert_eq!(c.end_ms, 1000);
        assert_eq!(c.duration_ms(), 0);
        let c = Cue::new(1000, 3000, "hi");
        assert!(c.contains(2000));
        assert!(!c.contains(3000));
    }
}
