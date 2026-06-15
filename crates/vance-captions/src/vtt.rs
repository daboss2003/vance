//! WebVTT (`.vtt`) import / export.
//!
//! WebVTT differs from SRT mainly in the `WEBVTT` header, the `.`
//! millisecond separator, optional cue identifiers + cue settings, and
//! `NOTE` / `STYLE` blocks. Parsing reuses the tolerant SRT block scanner
//! (which keys off the `-->` line, accepts `.` separators, ignores cue
//! settings after the end time, and skips non-timestamp lines), so header
//! / NOTE / identifier lines are naturally dropped.

use crate::srt;
use crate::{format_timestamp, sanitize_cue_text, CaptionError, Cue};

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
        out.push_str(&sanitize_cue_text(&c.text));
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

/// Extract a leading voice tag — `<v Speaker>`, classed `<v.loud Speaker>`,
/// with or without a closing `</v>` — into `speaker`, leaving clean text.
fn split_voice_tag(mut cue: Cue) -> Cue {
    let t = cue.text.trim_start();
    let Some(rest) = t.strip_prefix("<v") else {
        return cue;
    };
    // The char after `<v` must begin a tag (classes `.`, the name space, or
    // an immediate `>`); otherwise it's not a voice tag (e.g. `<video>`).
    if !rest.starts_with(['.', ' ', '\t', '>']) {
        return cue;
    }
    let Some((inner, body)) = rest.split_once('>') else {
        return cue;
    };
    // `inner` is like `.loud Bob` or ` Bob`: the speaker is whatever follows
    // the first whitespace (classes, if any, precede it).
    let name = inner
        .split_once(char::is_whitespace)
        .map(|(_, n)| n)
        .unwrap_or(inner)
        .trim();
    cue.speaker = name.to_string();
    // Strip a trailing `</v>` from the body so it doesn't leak into text.
    cue.text = body
        .trim_start()
        .trim_end()
        .trim_end_matches("</v>")
        .trim_end()
        .to_string();
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

    #[test]
    fn voice_tag_handles_classes_and_closing_tag() {
        // Review fix: classed voice tag + closing </v>.
        let src = "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\n<v.loud Bob>Hey</v>\n";
        let cues = from_vtt(src).unwrap();
        assert_eq!(cues[0].speaker, "Bob", "classed tag → speaker extracted");
        assert_eq!(cues[0].text, "Hey", "closing </v> stripped");
    }
}
