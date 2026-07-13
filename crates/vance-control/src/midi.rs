//! MIDI message → [`ControlEvent`] decoding.
//!
//! Channel Voice messages only (the useful control-surface subset):
//! Note On/Off → buttons (id = note number), Control Change → faders
//! (id = CC number, value = data/127). Channel is ignored. The `midir`
//! binding in the app feeds 3-byte messages here.

use crate::ControlEvent;

/// Decode a MIDI channel-voice message into a control event, or `None`
/// for messages we don't map (program change, aftertouch, sysex, …).
pub fn decode(bytes: &[u8]) -> Option<ControlEvent> {
    if bytes.len() < 3 {
        return None;
    }
    let status = bytes[0] & 0xF0;
    let d1 = bytes[1] & 0x7F;
    let d2 = bytes[2] & 0x7F;
    match status {
        // Note On — velocity 0 is the conventional "note off".
        0x90 => Some(ControlEvent::Button {
            id: d1 as u32,
            pressed: d2 > 0,
        }),
        // Note Off.
        0x80 => Some(ControlEvent::Button {
            id: d1 as u32,
            pressed: false,
        }),
        // Control Change → fader.
        0xB0 => Some(ControlEvent::Fader {
            id: d1 as u32,
            value: d2 as f32 / 127.0,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_off_and_cc() {
        // Note On ch1 note 60 vel 100 → button 60 pressed.
        assert_eq!(
            decode(&[0x90, 60, 100]),
            Some(ControlEvent::Button { id: 60, pressed: true })
        );
        // Note On vel 0 = note off.
        assert_eq!(
            decode(&[0x90, 60, 0]),
            Some(ControlEvent::Button { id: 60, pressed: false })
        );
        // Note Off.
        assert_eq!(
            decode(&[0x80, 60, 0]),
            Some(ControlEvent::Button { id: 60, pressed: false })
        );
        // CC 7 (volume) value 127 → fader 7 = 1.0.
        match decode(&[0xB2, 7, 127]) {
            Some(ControlEvent::Fader { id, value }) => {
                assert_eq!(id, 7);
                assert!((value - 1.0).abs() < 1e-4);
            }
            other => panic!("expected fader, got {other:?}"),
        }
        // Program change → ignored.
        assert_eq!(decode(&[0xC0, 1, 0]), None);
        assert_eq!(decode(&[0x90, 60]), None, "too short");
    }
}
