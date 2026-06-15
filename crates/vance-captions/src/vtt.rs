//! WebVTT (`.vtt`) import / export.
//!
//! WebVTT differs from SRT mainly in the `WEBVTT` header, the `.`
//! millisecond separator, optional cue identifiers + cue settings, and
//! `NOTE` / `STYLE` blocks. Parsing reuses the tolerant SRT block scanner
//! (which keys off the `-->` line, accepts `.` separators, ignores cue
//! settings after the end time, and skips non-timestamp lines), so header
//! / NOTE / identifier lines are naturally dropped.

use crate::srt;
use crate::{format_timestamp, CaptionError, Cue};

/// Serialize cues to a WebVTT document (header + `.` separators).
pub fn to_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for c in cues {
        out.push_str(&format_timestamp(c.start_ms, false));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(c.end_ms, false));
        out.push('\n');
        if !c.speaker.is_empty() {
            // VTT voice span — renders as the speaker in compliant players.
            out.push_str(&format!("<v {}>", c.speaker));
        }
        out.push_str(c.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

/// Parse a WebVTT document into cues (fresh ids). Tolerant of the header,
/// cue identifiers, cue settings, and `NOTE` / `STYLE` blocks.
pub fn from_vtt(text: &str) -> Result<Vec<Cue>, CaptionError> {
    // The SRT scanner is `-->`-anchored and `.`/`,`-agnostic, so it parses
    // VTT bodies directly. Strip a leading `<v Speaker>` voice tag into the
    // cue's speaker field afterward.
    let cues = srt::from_srt(text).map_err(|e| match e {
        CaptionError::BadTimestamp { line, text } => CaptionError::BadTimestamp { line, text },
        CaptionError::Malformed { line, msg, .. } => CaptionError::Malformed {
            format: "WebVTT",
            line,
            msg,
        },
    })?;
    Ok(cues.into_iter().map(split_voice_tag).collect())
}

/// Extract a leading `<v Speaker>text` voice tag into `speaker`.
fn split_voice_tag(mut cue: Cue) -> Cue {
    let t = cue.text.trim_start();
    if let Some(rest) = t.strip_prefix("<v ") {
        if let Some((name, body)) = rest.split_once('>') {
            cue.speaker = name.trim().to_string();
            cue.text = body.trim_start().to_string();
        }
    }
    cue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_webvtt_with_header_and_notes() {
        let src = "WEBVTT\n\nNOTE this is a comment\n\n\
                   00:00:01.000 --> 00:00:04.000\nHello\n\n\
                   cue-2\n00:00:05.000 --> 00:00:08.000 align:center\nWorld\n";
        let cues = from_vtt(src).unwrap();
        assert_eq!(cues.len(), 2, "NOTE + identifier + settings ignored");
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].text, "Hello");
        assert_eq!(cues[1].start_ms, 5000);
        assert_eq!(cues[1].end_ms, 8000);
        assert_eq!(cues[1].text, "World");
        let out = to_vtt(&cues);
        assert!(out.starts_with("WEBVTT"));
        assert!(out.contains("00:00:01.000 --> 00:00:04.000"));
    }

    #[test]
    fn voice_tag_extracts_speaker() {
        let src = "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\n<v Alice>Hi there\n";
        let cues = from_vtt(src).unwrap();
        assert_eq!(cues[0].speaker, "Alice");
        assert_eq!(cues[0].text, "Hi there");
    }
}
