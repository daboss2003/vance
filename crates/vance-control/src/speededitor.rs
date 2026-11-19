//! Blackmagic Speed Editor HID input-report decoding.
//!
//! Decodes the two input reports the Speed Editor emits: the jog report
//! (id `0x03` — a mode byte + signed LE 32-bit accumulator) and the keys
//! report (id `0x04` — up to 6 little-endian `u16` keycodes currently
//! held). The report format is pure + tested here; the device I/O
//! (`hidapi`) AND the Speed Editor's required authentication handshake
//! (a keep-alive challenge/response, without which the device stays
//! silent) are gated on the physical device and live in the app layer.
//!
//! The keys report lists CURRENTLY-held keys, so the caller diffs
//! successive reports to derive press/release edges.

/// A decoded Speed Editor report.
#[derive(Clone, Debug, PartialEq)]
pub enum SpeedEditorReport {
    /// Jog wheel accumulator (signed). The caller tracks deltas.
    Jog {
        /// Jog mode byte (0 = relative, others = shuttle modes).
        mode: u8,
        /// Signed jog value.
        value: i32,
    },
    /// The set of keycodes currently held (empty = all released).
    Keys(Vec<u16>),
    /// A report id we don't decode.
    Other(u8),
}

/// Decode one raw HID input report (INCLUDING the leading report id byte).
pub fn decode(report: &[u8]) -> Option<SpeedEditorReport> {
    let id = *report.first()?;
    match id {
        0x03 => {
            // [id, mode, jog_le_i32 (4 bytes), ...]
            if report.len() < 6 {
                return None;
            }
            let mode = report[1];
            let value = i32::from_le_bytes([report[2], report[3], report[4], report[5]]);
            Some(SpeedEditorReport::Jog { mode, value })
        }
        0x04 => {
            // [id, key0_le_u16, key1_le_u16, ...] — zero terminates.
            let mut keys = Vec::new();
            let mut i = 1;
            while i + 1 < report.len() {
                let k = u16::from_le_bytes([report[i], report[i + 1]]);
                if k != 0 {
                    keys.push(k);
                }
                i += 2;
            }
            Some(SpeedEditorReport::Keys(keys))
        }
        other => Some(SpeedEditorReport::Other(other)),
    }
}

/// Diff two held-key sets into (newly_pressed, newly_released) keycodes.
/// The app calls this on successive [`SpeedEditorReport::Keys`] to turn
/// held-state reports into press/release edges.
pub fn key_edges(prev: &[u16], now: &[u16]) -> (Vec<u16>, Vec<u16>) {
    let pressed = now.iter().copied().filter(|k| !prev.contains(k)).collect();
    let released = prev.iter().copied().filter(|k| !now.contains(k)).collect();
    (pressed, released)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_jog_report() {
        // id 3, mode 0, jog = -5 (LE i32).
        let mut r = vec![0x03u8, 0x00];
        r.extend_from_slice(&(-5i32).to_le_bytes());
        assert_eq!(
            decode(&r),
            Some(SpeedEditorReport::Jog { mode: 0, value: -5 })
        );
    }

    #[test]
    fn decodes_keys_report_and_edges() {
        // id 4, keys 0x0001 and 0x0034, then zero.
        let r = [0x04u8, 0x01, 0x00, 0x34, 0x00, 0x00, 0x00];
        assert_eq!(
            decode(&r),
            Some(SpeedEditorReport::Keys(vec![0x0001, 0x0034]))
        );
        // Edges: from {1} to {1, 0x34} → pressed 0x34; to {} → released both.
        let (p, _) = key_edges(&[0x0001], &[0x0001, 0x0034]);
        assert_eq!(p, vec![0x0034]);
        let (_, rel) = key_edges(&[0x0001, 0x0034], &[]);
        assert_eq!(rel, vec![0x0001, 0x0034]);
    }

    #[test]
    fn short_and_unknown_reports() {
        assert!(decode(&[]).is_none());
        assert_eq!(decode(&[0x03, 0x00]), None, "jog too short");
        assert_eq!(decode(&[0x09]), Some(SpeedEditorReport::Other(0x09)));
    }
}
