//! SubRip (`.srt`) import / export.

use crate::{format_timestamp, parse_timestamp, CaptionError, Cue};

/// Serialize cues to SubRip text. Cues are emitted in the given order
/// (caller sorts); indices are 1-based and renumbered.
pub fn to_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&(i + 1).to_string());
        out.push('\n');
        out.push_str(&format_timestamp(c.start_ms, true));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(c.end_ms, true));
        out.push('\n');
        if !c.speaker.is_empty() {
            out.push_str(&format!("[{}] ", c.speaker));
        }
        out.push_str(c.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

/// Parse SubRip text into cues (fresh ids). Tolerant of missing index
/// lines, `\r\n`, and extra blank lines; the `-->` timestamp line is the
/// anchor for each block.
pub fn from_srt(text: &str) -> Result<Vec<Cue>, CaptionError> {
    let mut cues = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        // Find a timestamp line; skip indices / blanks before it.
        if !line.contains("-->") {
            i += 1;
            continue;
        }
        let (start, end) = parse_arrow(line, i + 1)?;
        // Collect text lines until a blank line or EOF.
        let mut body: Vec<String> = Vec::new();
        i += 1;
        while i < lines.len() && !lines[i].trim().is_empty() {
            // A bare integer immediately followed by a timestamp line is the
            // NEXT cue's index — stop before it.
            if lines[i].trim().parse::<u32>().is_ok()
                && i + 1 < lines.len()
                && lines[i + 1].contains("-->")
            {
                break;
            }
            body.push(lines[i].trim_end().to_string());
            i += 1;
        }
        let text = body.join("\n");
        cues.push(Cue::new(start, end, text));
    }
    Ok(cues)
}

/// Parse a `START --> END` line to `(start_ms, end_ms)`.
fn parse_arrow(line: &str, line_no: usize) -> Result<(i64, i64), CaptionError> {
    let (a, b) = line.split_once("-->").ok_or(CaptionError::Malformed {
        format: "SRT",
        line: line_no,
        msg: "missing -->".into(),
    })?;
    // WebVTT cue settings (after the end time) are ignored for SRT-style
    // parsing: take the first whitespace token of `b`.
    let end_tok = b.split_whitespace().next().unwrap_or("");
    let start = parse_timestamp(a).ok_or_else(|| CaptionError::BadTimestamp {
        line: line_no,
        text: a.trim().into(),
    })?;
    let end = parse_timestamp(end_tok).ok_or_else(|| CaptionError::BadTimestamp {
        line: line_no,
        text: end_tok.into(),
    })?;
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_basic_srt() {
        let src = "1\n00:00:01,000 --> 00:00:04,000\nHello world\n\n\
                   2\n00:00:05,500 --> 00:00:08,000\nLine one\nLine two\n\n";
        let cues = from_srt(src).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 4000);
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].start_ms, 5500);
        assert_eq!(cues[1].text, "Line one\nLine two");
        // Re-serialize → re-parse is stable.
        let out = to_srt(&cues);
        let again = from_srt(&out).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again[1].text, "Line one\nLine two");
    }

    #[test]
    fn tolerates_crlf_and_missing_indices() {
        let src = "00:00:00,000 --> 00:00:02,000\r\nNo index here\r\n";
        let cues = from_srt(src).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "No index here");
    }

    #[test]
    fn bad_timestamp_errors() {
        let src = "1\n00:00:01,000 --> notatime\nx\n";
        assert!(from_srt(src).is_err());
    }
}
